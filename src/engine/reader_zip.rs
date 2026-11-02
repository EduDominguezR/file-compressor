//! Lector ZIP secuencial (un solo hilo): abre el ZIP en solo lectura, valida el directorio
//! central y entrega cada entrada al `Sink` en streaming (buffer ~1 MB).
//!
//! Zip64: lo gestiona el crate `zip` al leer el directorio central.
//! CRC32: se verifica por entrada contra el directorio central; si falla, se aborta.

use std::collections::hash_map::RandomState;
use std::collections::HashSet;
use std::fs::File;
use std::hash::BuildHasher;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use zip::{CompressionMethod, ZipArchive};

use crate::engine::cancel::CancelFlag;
use crate::engine::limits::Limits;
use crate::engine::progress::Tracker;
use crate::engine::sink::{EntryMeta, Sink};
use crate::engine::BUF_SIZE;
use crate::error::{Error, Result};
use crate::fs::safe_path::{fold_key, sanitize_entry_path};
use crate::verify::crc::{Crc32, Expected};

/// Extensiones que casi siempre ya vienen comprimidas.
const COMPRESSED_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "mp4", "mkv", "zip", "7z", "rar", "gz", "zst", "xz", "pdf", "webp",
    "mov", "mp3", "bz2", "tgz",
];

/// Umbral de la heurística "ya comprimido".
pub const COMPRESSED_THRESHOLD: f64 = 0.70;

/// Cada cuántos bytes leídos se consulta la bandera de cancelación (≥ 1 MB).
const CANCEL_CHECK_EVERY: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ZipInfo {
    pub zip_size: u64,
    /// Entradas con ruta útil (archivos + directorios).
    pub entries: u64,
    pub files: u64,
    /// Suma de tamaños descomprimidos (base del progreso).
    pub total_bytes: u64,
    /// Bytes descomprimidos en extensiones ya comprimidas.
    pub precompressed_bytes: u64,
    pub symlinks: u64,
    /// Suma de tamaños comprimidos declarados.
    pub compressed_total: u64,
}

impl ZipInfo {
    pub fn precompressed_fraction(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.precompressed_bytes as f64 / self.total_bytes as f64
        }
    }
}

/// Valida por magic bytes (no por extensión): `PK\x03\x04` o `PK\x05\x06` (ZIP vacío).
/// `PK\x07\x08` al inicio indica un archivo multivolumen/spanned.
pub fn check_magic(path: &Path) -> Result<()> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    if n < 4 || magic[0] != b'P' || magic[1] != b'K' {
        return Err(Error::NotZip);
    }
    match (magic[2], magic[3]) {
        (3, 4) | (5, 6) => {}
        (7, 8) => return Err(Error::Multivolume),
        _ => return Err(Error::NotZip),
    }
    // Partes hermanas típicas de un ZIP dividido: nombre.z01
    if path.with_extension("z01").exists() {
        return Err(Error::Multivolume);
    }
    Ok(())
}

// ── Detección de duplicados en el directorio central ─────────────────────────────
//
// `zip 2.6.1` guarda las entradas en un `IndexMap` indexado por nombre: si un ZIP
// contiene dos entradas con el MISMO nombre, la segunda SUSTITUYE a la primera y
// `ZipArchive::len()` ya no las cuenta. Eso haría que MegaCompress reempaquetara una
// sola copia y la verificación no notara la pérdida. Por eso se recorre el directorio
// central directamente (solo cabeceras, sin descomprimir) antes de usar el crate.

const EOCD_SIG: [u8; 4] = *b"PK\x05\x06";
const ZIP64_EOCD_SIG: [u8; 4] = *b"PK\x06\x06";
const ZIP64_LOCATOR_SIG: [u8; 4] = *b"PK\x06\x07";
const CENTRAL_SIG: [u8; 4] = *b"PK\x01\x02";
const EOCD_MIN: usize = 22;
const ZIP64_EOCD_LEN: usize = 56;
const ZIP64_LOCATOR_LEN: usize = 20;

fn find_last(hay: &[u8], needle: &[u8; 4]) -> Option<usize> {
    hay.windows(4).rposition(|w| w == needle)
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}

fn le64(b: &[u8], at: usize) -> Option<u64> {
    let mut v = [0u8; 8];
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = *b.get(at + i)?;
    }
    Some(u64::from_le_bytes(v))
}

fn read_at(file: &mut File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut out = vec![0u8; len];
    let mut got = 0usize;
    while got < len {
        let n = file.read(&mut out[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    out.truncate(got);
    Ok(out)
}

/// Recorre el directorio central y rechaza nombres repetidos byte a byte. Devuelve el número
/// de registros del directorio central (que puede ser mayor que `ZipArchive::len()` si el crate
/// ha colapsado duplicados).
fn scan_raw_central(path: &Path, limits: &Limits) -> Result<u64> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len < EOCD_MIN as u64 {
        return Err(Error::Corrupt(
            "el ZIP es demasiado pequeño para tener un directorio central".into(),
        ));
    }

    // El EOCD está en los últimos 65 557 bytes (comentario máximo) más los registros ZIP64.
    let tail_len = (file_len as usize).min(65_557 + ZIP64_EOCD_LEN + ZIP64_LOCATOR_LEN);
    let tail_start = file_len - tail_len as u64;
    let tail = read_at(&mut file, tail_start, tail_len)?;
    let eocd_rel = find_last(&tail, &EOCD_SIG).ok_or_else(|| {
        Error::Corrupt("no se encontró el final del directorio central del ZIP".into())
    })?;
    if eocd_rel + EOCD_MIN > tail.len() {
        return Err(Error::Corrupt(
            "el final del directorio central está truncado".into(),
        ));
    }
    let eocd_pos = tail_start + eocd_rel as u64;

    let total16 = le16(&tail, eocd_rel + 10).unwrap_or(0);
    let cd_size32 = le32(&tail, eocd_rel + 12).unwrap_or(0);
    let cd_off32 = le32(&tail, eocd_rel + 16).unwrap_or(0);

    let (total, cd_size, cd_end, cd_off) =
        if total16 == u16::MAX || cd_size32 == u32::MAX || cd_off32 == u32::MAX {
            let loc_rel = find_last(&tail[..eocd_rel], &ZIP64_LOCATOR_SIG).ok_or_else(|| {
                Error::Corrupt("el ZIP usa ZIP64 pero falta el localizador ZIP64".into())
            })?;
            if loc_rel + ZIP64_LOCATOR_LEN > tail.len() {
                return Err(Error::Corrupt("localizador ZIP64 truncado".into()));
            }
            let z64_off = le64(&tail, loc_rel + 8)
                .ok_or_else(|| Error::Corrupt("localizador ZIP64 ilegible".into()))?;
            let z64 = read_at(&mut file, z64_off, ZIP64_EOCD_LEN)?;
            if z64.len() < ZIP64_EOCD_LEN || z64[..4] != ZIP64_EOCD_SIG {
                return Err(Error::Corrupt("registro ZIP64 ilegible".into()));
            }
            let total = le64(&z64, 32)
                .ok_or_else(|| Error::Corrupt("ZIP64 sin número de entradas".into()))?;
            let size = le64(&z64, 40)
                .ok_or_else(|| Error::Corrupt("ZIP64 sin tamaño de directorio".into()))?;
            let off = le64(&z64, 48)
                .ok_or_else(|| Error::Corrupt("ZIP64 sin desplazamiento de directorio".into()))?;
            (total, size, z64_off, off)
        } else {
            (
                u64::from(total16),
                u64::from(cd_size32),
                eocd_pos,
                u64::from(cd_off32),
            )
        };

    limits.check_entry_count(total)?;
    let mut cd_start = cd_end
        .checked_sub(cd_size)
        .ok_or_else(|| Error::Corrupt("tamaño del directorio central incoherente".into()))?;

    // Verificación de cordura: el primer registro debe empezar con la firma central.
    if total > 0 {
        let first = read_at(&mut file, cd_start, 4)?;
        if first != CENTRAL_SIG {
            // ZIP con datos antepuestos o con un registro de firma entre el directorio y el EOCD:
            // se intenta la posición absoluta que declara el EOCD. Si tampoco encaja, se falla
            // cerrado (mejor rechazar que perder duplicados silenciosamente).
            let fallback = cd_off;
            let first2 = read_at(&mut file, fallback, 4)?;
            if first2 == CENTRAL_SIG {
                cd_start = fallback;
            } else {
                return Err(Error::Corrupt(
                    "no se pudo localizar el directorio central del ZIP".into(),
                ));
            }
        }
    }

    let (h1, h2) = (RandomState::new(), RandomState::new());
    let mut seen: HashSet<u128> = HashSet::with_capacity(total.min(1 << 20) as usize);
    let mut header = [0u8; 46];
    let mut name: Vec<u8> = Vec::new();
    let mut pos = cd_start;
    for _ in 0..total {
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut header)?;
        if header[..4] != CENTRAL_SIG {
            return Err(Error::Corrupt(
                "registro del directorio central dañado".into(),
            ));
        }
        let name_len = le16(&header, 28).unwrap_or(0) as usize;
        let extra_len = le16(&header, 30).unwrap_or(0) as usize;
        let comment_len = le16(&header, 32).unwrap_or(0) as usize;
        name.resize(name_len, 0);
        file.read_exact(&mut name)?;

        let key = (u128::from(h1.hash_one(&name)) << 64) | u128::from(h2.hash_one(&name));
        if !seen.insert(key) {
            let shown = String::from_utf8_lossy(&name).replace('\\', "/");
            return Err(Error::DuplicateEntry(shown));
        }

        pos = pos
            .checked_add(46 + name_len as u64 + extra_len as u64 + comment_len as u64)
            .ok_or_else(|| Error::Corrupt("directorio central desbordado".into()))?;
    }
    Ok(total)
}

fn describe_method(m: CompressionMethod) -> String {
    let dbg = format!("{m:?}");
    let id: Option<u16> = dbg
        .trim_start_matches("Unsupported(")
        .trim_end_matches(')')
        .parse()
        .ok();
    match id {
        Some(9) => "Deflate64".into(),
        Some(14) => "LZMA".into(),
        Some(95) => "XZ".into(),
        Some(98) => "PPMd".into(),
        Some(99) => "AES".into(),
        Some(n) => format!("método {n}"),
        None => dbg,
    }
}

fn check_method(entry: &str, m: CompressionMethod) -> Result<()> {
    match m {
        CompressionMethod::Stored
        | CompressionMethod::Deflated
        | CompressionMethod::Bzip2
        | CompressionMethod::Zstd => Ok(()),
        other => Err(Error::UnsupportedMethod {
            entry: entry.to_string(),
            method: describe_method(other),
        }),
    }
}

fn is_symlink(unix_mode: Option<u32>) -> bool {
    unix_mode.is_some_and(|m| m & 0o170000 == 0o120000)
}

fn has_precompressed_ext(name: &str) -> bool {
    let last = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match last.rsplit_once('.') {
        Some((_, ext)) => COMPRESSED_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// Días desde 1970-01-01 (algoritmo de Howard Hinnant) → segundos Unix. La hora DOS no lleva
/// zona horaria: se trata como UTC.
pub fn civil_to_unix(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = (if y2 >= 0 { y2 } else { y2 - 399 }) / 400;
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + h * 3_600 + mi * 60 + s
}

/// Acepta tanto `DateTime` como `Option<DateTime>` (cambia según la versión de `zip`).
fn mtime_from(lm: impl Into<Option<zip::DateTime>>) -> Option<i64> {
    let d: zip::DateTime = lm.into()?;
    Some(civil_to_unix(
        d.year() as i64,
        d.month() as i64,
        d.day() as i64,
        d.hour() as i64,
        d.minute() as i64,
        d.second() as i64,
    ))
}

fn open_zip(path: &Path) -> Result<ZipArchive<BufReader<File>>> {
    // File::open = solo lectura. El original nunca se abre con permisos de escritura.
    let file = File::open(path)?;
    Ok(ZipArchive::new(BufReader::with_capacity(BUF_SIZE, file))?)
}

/// Valida el directorio central sin descomprimir nada: métodos, cifrado, rutas peligrosas,
/// duplicados/colisiones y límites anti ZIP-bomb (ver `Limits`).
///
/// Memoria: un `HashSet<u128>` con un resumen por entrada (≈ 32 bytes/entrada; acotado por
/// `limits.max_entries`) para detectar colisiones sin guardar las rutas.
pub fn inspect(path: &Path, limits: &Limits) -> Result<ZipInfo> {
    check_magic(path)?;
    let zip_size = std::fs::metadata(path)?.len();
    // Antes de abrir con el crate: detectar duplicados que `zip` colapsaría en su IndexMap.
    let raw_entries = scan_raw_central(path, limits)?;
    let mut zip = open_zip(path)?;
    limits.check_entry_count(zip.len() as u64)?;
    if raw_entries > zip.len() as u64 {
        return Err(Error::DuplicateEntry(
            "el ZIP contiene entradas distintas que el lector colapsaría en el mismo nombre".into(),
        ));
    }

    let mut info = ZipInfo {
        zip_size,
        entries: 0,
        files: 0,
        total_bytes: 0,
        precompressed_bytes: 0,
        symlinks: 0,
        compressed_total: 0,
    };
    let (h1, h2) = (RandomState::new(), RandomState::new());
    let mut seen: HashSet<u128> = HashSet::with_capacity(zip.len().min(1 << 20));

    for i in 0..zip.len() {
        let f = zip.by_index_raw(i)?;
        let name = f.name().to_string();
        if f.encrypted() {
            return Err(Error::Encrypted(name));
        }
        check_method(&name, f.compression())?;
        let clean = sanitize_entry_path(&name)?;
        if clean.is_empty() {
            continue;
        }
        // Identidad inequívoca: dos entradas que colisionan tras normalizar se rechazan.
        let folded = fold_key(&clean);
        let key = (u128::from(h1.hash_one(&folded)) << 64) | u128::from(h2.hash_one(&folded));
        if !seen.insert(key) {
            return Err(Error::DuplicateEntry(clean));
        }
        info.entries += 1;
        if f.is_dir() {
            continue;
        }
        let (size, compressed) = (f.size(), f.compressed_size());
        limits.check_entry(&clean, size, compressed, zip_size)?;
        info.total_bytes = limits.add_total(info.total_bytes, size)?;
        info.compressed_total = info
            .compressed_total
            .checked_add(compressed)
            .ok_or_else(|| {
                Error::Corrupt("tamaños comprimidos inconsistentes (desbordamiento)".into())
            })?;
        if info.compressed_total > zip_size {
            return Err(Error::Corrupt(
                "las entradas declaran más datos comprimidos que el propio ZIP (entradas solapadas o tamaños inconsistentes)"
                    .into(),
            ));
        }
        if is_symlink(f.unix_mode()) {
            info.symlinks += 1;
        }
        info.files += 1;
        if has_precompressed_ext(&clean) {
            info.precompressed_bytes += size;
        }
    }
    Ok(info)
}

/// Lector que envuelve la entrada del ZIP: cuenta progreso, calcula CRC32, comprueba el tamaño
/// declarado y consulta la cancelación. Los fallos tipados se guardan en `failure` porque el
/// `Sink` solo ve un `io::Error`.
pub struct CheckedReader<'a> {
    inner: &'a mut dyn Read,
    name: &'a str,
    expected_size: u64,
    expected_crc: u32,
    crc: Option<Crc32>,
    read: u64,
    since_check: u64,
    done: bool,
    cancel: &'a CancelFlag,
    tracker: &'a mut Tracker,
    pub failure: Option<Error>,
}

impl<'a> CheckedReader<'a> {
    pub fn new(
        inner: &'a mut dyn Read,
        name: &'a str,
        expected_size: u64,
        expected_crc: u32,
        cancel: &'a CancelFlag,
        tracker: &'a mut Tracker,
    ) -> Self {
        Self {
            inner,
            name,
            expected_size,
            expected_crc,
            crc: Some(Crc32::new()),
            read: 0,
            since_check: 0,
            done: false,
            cancel,
            tracker,
            failure: None,
        }
    }

    fn fail(&mut self, e: Error) -> io::Error {
        self.failure = Some(e);
        io::Error::other("megacompress: lectura abortada")
    }

    /// Si el sink no leyó hasta el final, lee y descarta el resto para forzar la comprobación
    /// de tamaño y CRC.
    pub fn drain_and_verify(&mut self) -> Result<()> {
        let mut sink = [0u8; 64 * 1024];
        while !self.done {
            if self.read(&mut sink).is_err() {
                return Err(self
                    .failure
                    .take()
                    .unwrap_or_else(|| Error::Internal("lectura abortada".into())));
            }
        }
        Ok(())
    }
}

impl Read for CheckedReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        let n = match self.inner.read(buf) {
            Ok(n) => n,
            Err(e) => {
                let msg = format!("no se pudo leer «{}»: {e}", self.name);
                return Err(self.fail(Error::Corrupt(msg)));
            }
        };
        if n == 0 {
            let got = self.crc.take().map(|c| c.finalize()).unwrap_or(0);
            if self.read != self.expected_size {
                let msg = format!(
                    "«{}» tiene {} bytes y el directorio central dice {}",
                    self.name, self.read, self.expected_size
                );
                return Err(self.fail(Error::Corrupt(msg)));
            }
            if got != self.expected_crc {
                let msg = format!("CRC incorrecto en «{}» (datos corruptos)", self.name);
                return Err(self.fail(Error::Corrupt(msg)));
            }
            self.done = true;
            return Ok(0);
        }
        if self.read + n as u64 > self.expected_size {
            let msg = format!("«{}» contiene más datos de los declarados", self.name);
            return Err(self.fail(Error::Corrupt(msg)));
        }
        if let Some(c) = self.crc.as_mut() {
            c.update(&buf[..n]);
        }
        self.read += n as u64;
        self.tracker.add_bytes(n as u64);
        self.since_check += n as u64;
        if self.since_check >= CANCEL_CHECK_EVERY {
            self.since_check = 0;
            if self.cancel.is_cancelled() {
                return Err(self.fail(Error::Cancelled));
            }
        }
        Ok(n)
    }
}

pub struct StreamResult {
    pub expected: Expected,
    pub skipped_symlinks: u64,
}

/// Recorre el ZIP y alimenta al sink. Nunca usa `read_to_end` sobre el ZIP entero: el sink lee
/// por bloques de la entrada.
pub fn stream(
    path: &Path,
    info: &ZipInfo,
    sink: &mut dyn Sink,
    cancel: &CancelFlag,
    tracker: &mut Tracker,
) -> Result<StreamResult> {
    let mut zip = open_zip(path)?;
    sink.begin(info.entries, info.total_bytes)?;
    let mut expected = Expected::default();
    let mut skipped = 0u64;

    for i in 0..zip.len() {
        cancel.check()?;
        let mut f = zip.by_index(i)?;
        let raw_name = f.name().to_string();
        let rel = sanitize_entry_path(&raw_name)?;
        if rel.is_empty() {
            continue;
        }
        tracker.begin_entry(&rel);
        let is_dir = f.is_dir();
        let unix_mode = f.unix_mode();
        let size = f.size();
        let crc32 = f.crc32();

        // Los enlaces simbólicos no se reproducen (su destino podría escapar del árbol).
        // Se omiten, se cuentan y se avisa en el resultado.
        if !is_dir && is_symlink(unix_mode) {
            skipped += 1;
            tracker.add_bytes(size);
            tracker.entry_done();
            tracing::warn!("enlace simbólico omitido: {rel}");
            continue;
        }

        let meta = EntryMeta {
            path: rel.clone(),
            size: if is_dir { 0 } else { size },
            mtime: mtime_from(f.last_modified()),
            unix_mode,
            crc32,
            method: format!("{:?}", f.compression()),
            is_dir,
        };

        if is_dir {
            sink.add_entry(meta, &mut io::empty())?;
        } else {
            if !expected.insert(&rel, size, crc32) {
                return Err(Error::DuplicateEntry(rel));
            }
            let mut rd = CheckedReader::new(&mut f, &rel, size, crc32, cancel, tracker);
            let res = sink.add_entry(meta, &mut rd);
            if let Some(e) = rd.failure.take() {
                return Err(e); // el error tipado (CRC, cancelación…) tiene prioridad
            }
            res?;
            rd.drain_and_verify()?;
        }
        tracker.entry_done();
    }
    tracker.flush();
    Ok(StreamResult {
        expected,
        skipped_symlinks: skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_to_unix(1970, 1, 1, 0, 0, 0), 0);
        assert_eq!(civil_to_unix(2000, 3, 1, 0, 0, 0), 951_868_800);
        assert_eq!(civil_to_unix(2024, 2, 29, 12, 0, 0), 1_709_208_000);
    }

    #[test]
    fn precompressed_extensions() {
        assert!(has_precompressed_ext("a/b/foto.JPG"));
        assert!(has_precompressed_ext("video.mkv"));
        assert!(!has_precompressed_ext("notas.txt"));
        assert!(!has_precompressed_ext("sin_extension"));
    }

    #[test]
    fn symlink_mode() {
        assert!(is_symlink(Some(0o120777)));
        assert!(!is_symlink(Some(0o100644)));
        assert!(!is_symlink(None));
    }

    #[test]
    fn magic_rejects_non_zip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.zip");
        std::fs::write(&p, b"hola mundo, esto no es un zip").unwrap();
        assert!(matches!(check_magic(&p), Err(Error::NotZip)));
        std::fs::write(&p, b"PK\x07\x08rest").unwrap();
        assert!(matches!(check_magic(&p), Err(Error::Multivolume)));
        std::fs::write(&p, b"PK\x05\x06rest").unwrap();
        assert!(check_magic(&p).is_ok());
    }
}
