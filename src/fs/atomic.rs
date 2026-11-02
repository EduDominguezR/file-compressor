//! Temporales por operación, bloqueo de exclusión REAL y publicación segura del resultado.
//!
//! # Modelo de bloqueo
//! Cada operación crea, en el directorio de destino, un par de archivos:
//!
//! ```text
//!   <stem>.<ext>.<instancia>-<op>.megacompress.lock   ← bloqueado con File::try_lock (exclusivo)
//!   <stem>.<ext>.<instancia>-<op>.megacompress.tmp    ← datos de salida
//! ```
//! * `instancia` = 64 bits aleatorios por proceso; `op` = contador por operación. Dos operaciones
//!   (aunque sean del mismo proceso o de instancias distintas) nunca comparten nombre.
//! * El bloqueo es del SISTEMA OPERATIVO (`flock` en Unix, `LockFileEx` en Windows). El sistema lo
//!   libera solo si el proceso muere (cierre inesperado, kill, corte de luz tras reinicio). Por eso
//!   la mera EXISTENCIA del `.lock` no significa "proceso vivo", y la antigüedad tampoco.
//! * Limpieza: para cada `.lock` válido (cabecera propia) intenta tomar el bloqueo. Si falla
//!   (`WouldBlock`) la operación sigue viva y NO se toca nada. Si lo consigue, la operación murió:
//!   se borra su `.tmp` y su `.lock`. Solo se tocan archivos con el nombre exacto de la
//!   aplicación y cuyo `.lock` empieza por la cabecera `MEGACOMPRESS-LOCK v1`.
//! * Un `.tmp` sin `.lock` NO se borra (podría ser de una versión anterior o ajeno): se registra.
//! * Margen anti-carrera: un `.lock` modificado hace menos de `LOCK_GRACE` no se recoge (cubre el
//!   instante entre `create_new` y `try_lock`). No es un criterio de obsolescencia: un lock
//!   antiguo y bloqueado jamás se toca.
//!
//! # Windows
//! `File::try_lock` usa `LockFileEx` y exige abrir el archivo con lectura/escritura (se hace). Un
//! archivo abierto sin `FILE_SHARE_DELETE` no se puede borrar mientras alguien lo tiene abierto,
//! así que además el borrado de un lock vivo falla de forma segura. Requiere Rust ≥ 1.89.
//!
//! # Publicación
//! `std::fs::rename` SOBRESCRIBE el destino existente (también en Windows). Por eso el nombre
//! final se RESERVA con `create_new` (atómico) y el temporal se renombra sobre nuestro propio
//! marcador. Nunca se pisa un archivo ajeno.

use std::collections::hash_map::RandomState;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use crate::error::{Error, Result};

pub const TMP_SUFFIX: &str = ".megacompress.tmp";
pub const LOCK_SUFFIX: &str = ".megacompress.lock";
const LOCK_HEADER: &[u8] = b"MEGACOMPRESS-LOCK v1\n";
const MAX_ATTEMPTS: u32 = 10_000;
/// Margen anti-carrera de la limpieza (ver documentación del módulo).
pub const LOCK_GRACE: Duration = Duration::from_secs(5);

static OP_COUNTER: AtomicU64 = AtomicU64::new(0);
static INSTANCE_ID: OnceLock<u64> = OnceLock::new();

/// Identificador aleatorio de esta instancia (proceso).
pub fn instance_id() -> u64 {
    *INSTANCE_ID.get_or_init(|| {
        let mut h = RandomState::new().build_hasher();
        h.write_u32(std::process::id());
        if let Ok(d) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            h.write_u128(d.as_nanos());
        }
        h.finish()
    })
}

/// Una operación en curso: posee el bloqueo y conoce la ruta de su temporal.
pub struct OpLock {
    file: Option<File>,
    lock_path: PathBuf,
    tmp_path: PathBuf,
}

impl OpLock {
    /// Crea y bloquea el `.lock` de una operación nueva en `dir`.
    pub fn acquire(dir: &Path, stem: &str, ext: &str) -> Result<OpLock> {
        for _ in 0..16 {
            let op = OP_COUNTER.fetch_add(1, Ordering::SeqCst);
            let id = format!("{:016x}-{op}", instance_id());
            let base = format!("{stem}.{ext}.{id}");
            let lock_path = dir.join(format!("{base}{LOCK_SUFFIX}"));
            let tmp_path = dir.join(format!("{base}{TMP_SUFFIX}"));

            let mut file = match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            };
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => {
                    let _ = fs::remove_file(&lock_path);
                    continue;
                }
                Err(TryLockError::Error(e)) => {
                    let _ = fs::remove_file(&lock_path);
                    return Err(e.into());
                }
            }
            file.write_all(LOCK_HEADER)?;
            file.write_all(format!("pid={}\n", std::process::id()).as_bytes())?;
            file.flush()?;
            // Si una limpieza ajena borró el .lock en la ventana create_new→try_lock, reintentar.
            if !lock_path.exists() {
                continue;
            }
            return Ok(OpLock {
                file: Some(file),
                lock_path,
                tmp_path,
            });
        }
        Err(Error::Internal(
            "no se pudo crear el bloqueo de la operación".into(),
        ))
    }

    pub fn tmp_path(&self) -> &Path {
        &self.tmp_path
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

impl Drop for OpLock {
    fn drop(&mut self) {
        // Orden: cerrar el handle (libera el bloqueo y permite borrar en Windows) y luego borrar.
        drop(self.file.take());
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// Borra el temporal al salir de ámbito salvo que se llame a `commit()`. Mantiene el `OpLock`
/// (y por tanto el bloqueo) vivo durante toda la operación.
pub struct OpGuard {
    lock: OpLock,
    keep_tmp: bool,
}

impl OpGuard {
    pub fn new(lock: OpLock) -> Self {
        Self {
            lock,
            keep_tmp: false,
        }
    }
    pub fn tmp_path(&self) -> &Path {
        self.lock.tmp_path()
    }
    /// El temporal ya fue renombrado al nombre final: no hay nada que borrar.
    pub fn disarm(&mut self) {
        self.keep_tmp = true;
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        if !self.keep_tmp {
            let _ = fs::remove_file(self.lock.tmp_path());
        }
        // `OpLock::drop` se ejecuta después y borra el .lock.
    }
}

/// Candidato n: `stem.ext`, `stem (1).ext`, `stem (2).ext`…
pub fn candidate(dir: &Path, stem: &str, ext: &str, n: u32) -> PathBuf {
    if n == 0 {
        dir.join(format!("{stem}.{ext}"))
    } else {
        dir.join(format!("{stem} ({n}).{ext}"))
    }
}

/// Mueve `tmp` a un nombre final único dentro de `dir`, sin sobrescribir nada.
pub fn commit_no_clobber(tmp: &Path, dir: &Path, stem: &str, ext: &str) -> Result<PathBuf> {
    for n in 0..MAX_ATTEMPTS {
        let dest = candidate(dir, stem, ext, n);
        match OpenOptions::new().write(true).create_new(true).open(&dest) {
            Ok(marker) => {
                drop(marker);
                return match fs::rename(tmp, &dest) {
                    Ok(()) => Ok(dest),
                    // Si el sistema no permite reemplazar el marcador (o alguien creó el destino
                    // en la carrera), se retira NUESTRO marcador y se prueba el siguiente nombre.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        let _ = fs::remove_file(&dest);
                        continue;
                    }
                    Err(e) => {
                        let _ = fs::remove_file(&dest); // quitar solo NUESTRO marcador
                        Err(e.into())
                    }
                };
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Output(
        "no se encontró un nombre de archivo libre".into(),
    ))
}

/// ¿El nombre tiene la forma exacta `<...>.<16 hex>-<n>.megacompress.lock`? Devuelve la base
/// (sin sufijo) para localizar el `.tmp` hermano.
fn parse_lock_name(name: &str) -> Option<&str> {
    let base = name.strip_suffix(LOCK_SUFFIX)?;
    let id = base.rsplit('.').next()?;
    let (inst, op) = id.split_once('-')?;
    let ok = inst.len() == 16
        && inst.bytes().all(|b| b.is_ascii_hexdigit())
        && !op.is_empty()
        && op.bytes().all(|b| b.is_ascii_digit());
    ok.then_some(base)
}

fn has_our_header(path: &Path) -> bool {
    let mut buf = vec![0u8; LOCK_HEADER.len()];
    match File::open(path).and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf == LOCK_HEADER,
        Err(_) => false,
    }
}

/// Resultado de una pasada de limpieza.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub removed_tmp: usize,
    pub skipped_active: usize,
    pub skipped_recent: usize,
    pub ignored_foreign: usize,
}

/// Limpieza con el margen anti-carrera por defecto.
pub fn cleanup_orphans(dir: &Path) -> CleanupReport {
    cleanup_orphans_with(dir, LOCK_GRACE)
}

/// Limpieza segura entre instancias (ver documentación del módulo).
pub fn cleanup_orphans_with(dir: &Path, grace: Duration) -> CleanupReport {
    let mut rep = CleanupReport::default();
    let Ok(rd) = fs::read_dir(dir) else {
        return rep;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(base) = parse_lock_name(&name) else {
            continue;
        };
        let lock_path = entry.path();
        if !has_our_header(&lock_path) {
            rep.ignored_foreign += 1;
            continue;
        }
        let recent = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_none_or(|age| age < grace);
        if recent {
            rep.skipped_recent += 1;
            continue;
        }
        // Tomar el bloqueo es la prueba de que NADIE lo tiene (el SO lo libera si el dueño murió).
        let Ok(f) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
            rep.skipped_active += 1; // no se pudo abrir: ante la duda, no tocar
            continue;
        };
        match f.try_lock() {
            Ok(()) => {
                let tmp = dir.join(format!("{base}{TMP_SUFFIX}"));
                if tmp.is_file() && fs::remove_file(&tmp).is_ok() {
                    rep.removed_tmp += 1;
                    tracing::info!("temporal abandonado eliminado: {}", tmp.display());
                }
                drop(f); // cerrar antes de borrar (Windows)
                let _ = fs::remove_file(&lock_path);
            }
            Err(TryLockError::WouldBlock) => rep.skipped_active += 1,
            Err(TryLockError::Error(e)) => {
                tracing::warn!(
                    "no se pudo comprobar el bloqueo {}: {e}",
                    lock_path.display()
                );
                rep.skipped_active += 1;
            }
        }
    }
    rep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_abandoned(dir: &Path, base: &str, with_header: bool) -> (PathBuf, PathBuf) {
        let lock = dir.join(format!("{base}{LOCK_SUFFIX}"));
        let tmp = dir.join(format!("{base}{TMP_SUFFIX}"));
        let mut content = Vec::new();
        if with_header {
            content.extend_from_slice(LOCK_HEADER);
        }
        content.extend_from_slice(b"pid=1\n");
        fs::write(&lock, content).unwrap();
        fs::write(&tmp, b"parcial").unwrap();
        (lock, tmp)
    }

    #[test]
    fn commit_does_not_overwrite_existing() {
        let d = tempfile::tempdir().unwrap();
        let existing = d.path().join("x.tar.zst");
        fs::write(&existing, b"ORIGINAL").unwrap();
        let lock = OpLock::acquire(d.path(), "x", "tar.zst").unwrap();
        fs::write(lock.tmp_path(), b"NUEVO").unwrap();
        let out = commit_no_clobber(lock.tmp_path(), d.path(), "x", "tar.zst").unwrap();
        assert_eq!(out.file_name().unwrap(), "x (1).tar.zst");
        assert_eq!(fs::read(&existing).unwrap(), b"ORIGINAL");
        assert_eq!(fs::read(&out).unwrap(), b"NUEVO");
    }

    #[test]
    fn commit_skips_multiple_existing() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("a.7z"), b"1").unwrap();
        fs::write(d.path().join("a (1).7z"), b"2").unwrap();
        let lock = OpLock::acquire(d.path(), "a", "7z").unwrap();
        fs::write(lock.tmp_path(), b"3").unwrap();
        let out = commit_no_clobber(lock.tmp_path(), d.path(), "a", "7z").unwrap();
        assert_eq!(out.file_name().unwrap(), "a (2).7z");
    }

    #[test]
    fn two_operations_get_distinct_names_and_locks() {
        let d = tempfile::tempdir().unwrap();
        let a = OpLock::acquire(d.path(), "z", "7z").unwrap();
        let b = OpLock::acquire(d.path(), "z", "7z").unwrap();
        assert_ne!(a.tmp_path(), b.tmp_path());
        assert_ne!(a.lock_path(), b.lock_path());
        assert!(a.lock_path().exists() && b.lock_path().exists());
    }

    #[test]
    fn guard_removes_tmp_and_lock_unless_disarmed() {
        let d = tempfile::tempdir().unwrap();
        let lock = OpLock::acquire(d.path(), "g", "7z").unwrap();
        let (tmp, lp) = (
            lock.tmp_path().to_path_buf(),
            lock.lock_path().to_path_buf(),
        );
        fs::write(&tmp, b"x").unwrap();
        drop(OpGuard::new(lock));
        assert!(!tmp.exists() && !lp.exists());

        let lock = OpLock::acquire(d.path(), "g", "7z").unwrap();
        let tmp = lock.tmp_path().to_path_buf();
        fs::write(&tmp, b"x").unwrap();
        let mut g = OpGuard::new(lock);
        g.disarm();
        drop(g);
        assert!(tmp.exists(), "disarm conserva el temporal");
    }

    #[test]
    fn cleanup_never_touches_an_active_operation() {
        let d = tempfile::tempdir().unwrap();
        let live = OpLock::acquire(d.path(), "vivo", "7z").unwrap(); // "otra instancia" viva
        fs::write(live.tmp_path(), b"escribiendo").unwrap();
        // Sin margen y con el lock "antiguo": igualmente NO se puede tocar porque está bloqueado.
        let rep = cleanup_orphans_with(d.path(), Duration::ZERO);
        assert_eq!(rep.removed_tmp, 0);
        // En Windows el bloqueo obligatorio impide incluso leer la cabecera: cae en "ajeno"
        // (también es el resultado seguro: no se toca).
        assert_eq!(rep.skipped_active + rep.ignored_foreign, 1);
        assert!(live.tmp_path().exists() && live.lock_path().exists());
    }

    #[test]
    fn cleanup_recovers_abandoned_lock() {
        let d = tempfile::tempdir().unwrap();
        let base = "viejo.7z.00000000deadbeef-7";
        let (lock, tmp) = write_abandoned(d.path(), base, true);
        let rep = cleanup_orphans_with(d.path(), Duration::ZERO);
        assert_eq!(rep.removed_tmp, 1);
        assert!(!tmp.exists() && !lock.exists());
    }

    #[test]
    fn cleanup_respects_recent_grace_window() {
        let d = tempfile::tempdir().unwrap();
        let (lock, tmp) = write_abandoned(d.path(), "r.7z.00000000deadbeef-1", true);
        let rep = cleanup_orphans_with(d.path(), Duration::from_secs(3600));
        assert_eq!(rep.skipped_recent, 1);
        assert!(lock.exists() && tmp.exists());
    }

    #[test]
    fn cleanup_ignores_foreign_files() {
        let d = tempfile::tempdir().unwrap();
        // Nombre con forma correcta pero SIN nuestra cabecera → ajeno.
        let (lock, tmp) = write_abandoned(d.path(), "ajeno.7z.00000000deadbeef-2", false);
        // Archivos que solo se parecen: ni siquiera se consideran.
        let other_tmp = d.path().join("importante.megacompress.tmp");
        let other_lock = d.path().join("nombre.malo.megacompress.lock");
        fs::write(&other_tmp, b"x").unwrap();
        fs::write(&other_lock, LOCK_HEADER).unwrap();
        let keep = d.path().join("datos.txt");
        fs::write(&keep, b"x").unwrap();

        let rep = cleanup_orphans_with(d.path(), Duration::ZERO);
        assert_eq!(rep.removed_tmp, 0);
        assert_eq!(rep.ignored_foreign, 1);
        assert!(
            lock.exists()
                && tmp.exists()
                && other_tmp.exists()
                && other_lock.exists()
                && keep.exists()
        );
    }

    #[test]
    fn lock_name_parser_is_strict() {
        assert!(parse_lock_name("a.7z.00000000deadbeef-0.megacompress.lock").is_some());
        assert!(parse_lock_name("a.7z.deadbeef-0.megacompress.lock").is_none());
        assert!(parse_lock_name("a.7z.00000000deadbeeg-0.megacompress.lock").is_none());
        assert!(parse_lock_name("a.7z.00000000deadbeef-x.megacompress.lock").is_none());
        assert!(parse_lock_name("a.7z.00000000deadbeef-0.txt").is_none());
    }
}
