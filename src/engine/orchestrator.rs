//! Pipeline: [lector ZIP] → [validador CRC] → [Sink] → [cierre] → [verificación integral] →
//! [publicación segura]. Un hilo lector secuencial alimenta a un único escritor (el propio Sink):
//! el orden del contenedor es determinista. El paralelismo lo aportan `zstdmt` y los trozos LZMA2.
//!
//! Garantías: el resultado solo se publica si el contenedor está cerrado Y la verificación (CRC32
//! de cada archivo recuperado contra el CRC32 del ZIP original) terminó sin discrepancias. Ante
//! cualquier error o cancelación se borra el temporal y el ZIP original no se toca.

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use sevenz_rust2::encoder_options::Lzma2Options;
use sevenz_rust2::{ArchiveEntry, ArchiveWriter};

use crate::engine::cancel::CancelFlag;
use crate::engine::limits::Limits;
use crate::engine::mode::{system_resources, Config, Mode};
use crate::engine::progress::{Phase, Snapshot, Tracker};
use crate::engine::reader_zip::{self, ZipInfo, COMPRESSED_THRESHOLD};
use crate::engine::sink::{Format, Sink};
use crate::engine::sink_7z_lzma2::SevenZSink;
use crate::engine::sink_tar_zst::TarZstSink;
use crate::engine::{Msg, Repaint, BUF_SIZE};
use crate::error::{Error, Result};
use crate::fs::atomic::{cleanup_orphans, commit_no_clobber, OpGuard, OpLock};
use crate::fs::space::{free_space, plan};
use crate::verify::{archive_check, zst_check};

/// Si la estimación de "Máxima" supera esto, la UI pide confirmación.
pub const SLOW_WARNING: Duration = Duration::from_secs(2 * 3600);
const SAMPLE_BYTES: usize = 10 * 1024 * 1024;
const SAMPLE_MIN_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct Analysis {
    pub info: ZipInfo,
    /// > 70 % de los bytes en extensiones ya comprimidas.
    pub mostly_compressed: bool,
    pub precompressed_fraction: f64,
    /// Tamaño final estimado extrapolando una muestra de ~10 MB (ESTIMACIÓN; `None` si no hay
    /// muestra suficiente).
    pub est_size_fast: Option<u64>,
    pub est_size_max: Option<u64>,
    /// Tiempo estimado de "Máxima" (heurística: muestra × factor de hilos).
    pub est_max: Option<Duration>,
    /// Temporales abandonados que se eliminaron en el destino al analizar.
    pub cleaned_orphans: usize,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub original_size: u64,
    pub output_size: u64,
    pub elapsed: Duration,
    pub format: Format,
    pub path: PathBuf,
    pub entries: u64,
    pub skipped_symlinks: u64,
    /// Qué se verificó realmente (texto para el usuario).
    pub verification: String,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub zip: PathBuf,
    pub mode: Mode,
    /// Carpeta de destino; por defecto la del ZIP.
    pub out_dir: Option<PathBuf>,
    pub limits: Limits,
    /// Solo tests: fuerza una configuración concreta.
    pub config: Option<Config>,
    /// Solo tests: simula el espacio libre del destino.
    pub free_space_override: Option<u64>,
}

impl Job {
    pub fn new(zip: PathBuf, mode: Mode) -> Self {
        Self {
            zip,
            mode,
            out_dir: None,
            limits: Limits::from_env(),
            config: None,
            free_space_override: None,
        }
    }

    fn resolved_out_dir(&self) -> PathBuf {
        if let Some(d) = &self.out_dir {
            return d.clone();
        }
        match self.zip.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        }
    }
}

/// Validación previa (estado "Validating"): magic bytes, directorio central, métodos, rutas,
/// límites, duplicados, heurística de "ya comprimido" y muestra de tamaño/tiempo.
/// Se ejecuta en un hilo aparte (nunca en el de la interfaz).
pub fn analyze(path: &Path, dest_dir: &Path, limits: &Limits) -> Result<Analysis> {
    let cleaned = cleanup_orphans(dest_dir).removed_tmp;
    let info = reader_zip::inspect(path, limits)?;
    let fraction = info.precompressed_fraction();
    let (ram, cores) = system_resources();
    let cfg_max = Config::for_system(Mode::Max, ram, cores);
    let cfg_fast = Config::for_system(Mode::Fast, ram, cores);

    let sample = read_sample(path);
    let (mut est_fast, mut est_max, mut est_time) = (None, None, None);
    if let Some(sample) = sample {
        let n = sample.len() as f64;
        if let Ok(c) = zstd::bulk::compress(&sample, cfg_fast.zstd_level) {
            est_fast = Some((info.total_bytes as f64 * (c.len() as f64 / n)) as u64);
        }
        if let Some((bytes, secs)) = sample_max(&sample, &cfg_max) {
            est_max = Some((info.total_bytes as f64 * (bytes as f64 / n)) as u64);
            let single_bps = n / secs.max(1e-3);
            // Heurística (NO medida): escalado sublineal por hilos del LZMA2 multihilo.
            let eff = 1.0 + 0.7 * (cfg_max.lzma_threads.saturating_sub(1)) as f64;
            est_time = Some(Duration::from_secs_f64(
                info.total_bytes as f64 / (single_bps * eff).max(1.0),
            ));
        }
    }
    Ok(Analysis {
        info,
        mostly_compressed: fraction > COMPRESSED_THRESHOLD,
        precompressed_fraction: fraction,
        est_size_fast: est_fast,
        est_size_max: est_max,
        est_max: est_time,
        cleaned_orphans: cleaned,
    })
}

/// Lee ~10 MB iniciales descomprimidos. `None` si es demasiado poco para extrapolar.
fn read_sample(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(BufReader::with_capacity(BUF_SIZE, file)).ok()?;
    let mut sample: Vec<u8> = Vec::with_capacity(SAMPLE_BYTES);
    for i in 0..zip.len() {
        if sample.len() >= SAMPLE_BYTES {
            break;
        }
        let mut f = zip.by_index(i).ok()?;
        if f.is_dir() {
            continue;
        }
        let room = (SAMPLE_BYTES - sample.len()) as u64;
        let _ = Read::by_ref(&mut f).take(room).read_to_end(&mut sample);
    }
    (sample.len() >= SAMPLE_MIN_BYTES).then_some(sample)
}

/// Comprime la muestra con LZMA2 (1 hilo, en memoria): (bytes resultantes, segundos).
fn sample_max(sample: &[u8], cfg: &Config) -> Option<(u64, f64)> {
    let mut w = ArchiveWriter::new(std::io::Cursor::new(Vec::new())).ok()?;
    w.set_content_methods(vec![Lzma2Options::from_level(cfg.lzma_level).into()]);
    let t0 = Instant::now();
    w.push_archive_entry(ArchiveEntry::new_file("muestra"), Some(sample))
        .ok()?;
    let out = w.finish().ok()?;
    Some((out.into_inner().len() as u64, t0.elapsed().as_secs_f64()))
}

/// Envío de mensajes + despertar de la UI.
struct Notifier {
    tx: Sender<Msg>,
    repaint: Option<Repaint>,
}

impl Notifier {
    fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
        if let Some(r) = &self.repaint {
            r();
        }
    }
    fn log(&self, text: impl Into<String>) {
        let text = text.into();
        tracing::info!("{text}");
        self.send(Msg::Log(text));
    }
}

fn finalizing_snapshot(status: &str) -> Snapshot {
    Snapshot {
        phase: Phase::Finalizing,
        done: 0,
        total: 0,
        entries_done: 0,
        entries_total: 0,
        speed_bps: 0.0,
        eta: None,
        elapsed: Duration::ZERO,
        status: status.to_string(),
        current: None,
    }
}

/// Ejecuta el trabajo completo. Emite `Msg::Progress` / `Msg::Log` por `tx`.
/// NO envía `Msg::Finished` (lo hace `spawn`); devuelve el resultado.
pub fn run(
    job: &Job,
    cancel: &CancelFlag,
    tx: Sender<Msg>,
    repaint: Option<Repaint>,
) -> Result<Report> {
    let started = Instant::now();
    let n = Notifier {
        tx: tx.clone(),
        repaint: repaint.clone(),
    };

    n.log("Leyendo directorio ZIP…");
    n.log("Validando entradas, límites de seguridad y nombres…");
    let info = reader_zip::inspect(&job.zip, &job.limits)?;
    n.log(format!(
        "ZIP válido: {} entradas, {} bytes descomprimidos.",
        info.entries, info.total_bytes
    ));
    cancel.check()?;

    let out_dir = job.resolved_out_dir();
    let stem = job
        .zip
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "salida".into());
    let ext = job.mode.extension();
    let cleaned = cleanup_orphans(&out_dir).removed_tmp;
    if cleaned > 0 {
        n.log(format!(
            "Se eliminaron {cleaned} temporales abandonados de ejecuciones anteriores."
        ));
    }

    // Espacio ANTES de empezar (el error de disco lleno DURANTE la escritura se gestiona aparte).
    let avail = job.free_space_override.or_else(|| free_space(&out_dir));
    let space = plan(&info, job.mode, avail);
    if let Some(w) = space.warning() {
        n.log(format!("Aviso de espacio: {w}"));
    }
    space.ensure_feasible()?;

    let cfg = job.config.clone().unwrap_or_else(|| {
        let (ram, cores) = system_resources();
        Config::for_system(job.mode, ram, cores)
    });
    n.log(format!(
        "Modo {:?}: zstd nivel {} ({} hilos) · LZMA2 nivel {} ({} hilos, bloque {} MB) · RAM estimada {} MB",
        job.mode,
        cfg.zstd_level,
        cfg.zstd_workers,
        cfg.lzma_level,
        cfg.lzma_threads,
        cfg.block_bytes / (1024 * 1024),
        cfg.estimated_ram() / (1024 * 1024)
    ));

    // Bloqueo + temporal de ESTA operación (borrados automáticamente si algo falla).
    let guard_lock = OpLock::acquire(&out_dir, &stem, ext)?;
    let mut guard = OpGuard::new(guard_lock);
    let tmp = guard.tmp_path().to_path_buf();

    let mut sink: Box<dyn Sink> = match job.mode {
        Mode::Fast => Box::new(TarZstSink::create(&tmp, &cfg)?),
        Mode::Max => Box::new(SevenZSink::create(&tmp, &cfg, cancel.clone())?),
    };
    {
        let (tx2, rp2) = (tx.clone(), repaint.clone());
        sink.set_status_callback(Box::new(move |status: &str| {
            let _ = tx2.send(Msg::Progress(finalizing_snapshot(status)));
            if let Some(r) = &rp2 {
                r();
            }
        }));
    }

    let (tx_p, rp_p) = (tx.clone(), repaint.clone());
    let mut last_log = Instant::now();
    let mut tracker = Tracker::new(
        info.total_bytes,
        info.entries,
        Box::new(move |snap: Snapshot| {
            // Línea de registro real "Comprimiendo entrada X de Y" como mucho cada 750 ms.
            if last_log.elapsed() >= Duration::from_millis(750) {
                last_log = Instant::now();
                if let Some(cur) = &snap.current {
                    let line = format!(
                        "Comprimiendo entrada {} de {}: {cur}",
                        snap.entries_done + 1,
                        snap.entries_total
                    );
                    let _ = tx_p.send(Msg::Log(line));
                }
            }
            let _ = tx_p.send(Msg::Progress(snap));
            if let Some(r) = &rp_p {
                r();
            }
        }),
    );

    let streamed = match reader_zip::stream(&job.zip, &info, sink.as_mut(), cancel, &mut tracker) {
        Ok(s) => s,
        Err(e) => {
            fail_cleanup(&n, cancel, &e);
            let _ = sink.abort();
            return Err(e);
        }
    };
    let mut expected = streamed.expected;

    n.log("Finalizando contenedor…");
    let out = match sink.finish() {
        Ok(o) => o,
        Err(e) => {
            fail_cleanup(&n, cancel, &e);
            return Err(e);
        }
    };
    cancel.check()?;

    // Verificación integral: CRC32 de lo RECUPERADO contra el CRC32 del ZIP ORIGINAL.
    n.log("Verificando CRC32 de todo el contenido recuperado…");
    let (vb, ve) = (expected.total_bytes(), expected.file_count());
    let (tx_v, rp_v) = (tx.clone(), repaint.clone());
    let mut vtracker = Tracker::for_phase(
        Phase::Verifying,
        vb,
        ve,
        Box::new(move |snap: Snapshot| {
            let _ = tx_v.send(Msg::Progress(snap));
            if let Some(r) = &rp_v {
                r();
            }
        }),
    );
    vtracker.flush(); // la UI pasa a "Verificación" aunque tarde en llegar el primer evento
    let verified = match job.mode {
        Mode::Fast => zst_check::verify_tar_zst(&out.path, &mut expected, cancel, |p| {
            vtracker.set_absolute(p.bytes_done, p.entries_done)
        }),
        Mode::Max => archive_check::verify_7z_full(&out.path, &mut expected, cancel, |p| {
            vtracker.set_absolute(p.bytes_done, p.entries_done)
        }),
    };
    if let Err(e) = verified {
        n.log(format!("La verificación falló: {e}"));
        return Err(e);
    }
    cancel.check()?;

    n.log("Publicando archivo final…");
    let final_path = commit_no_clobber(&tmp, &out_dir, &stem, ext)?;
    guard.disarm();
    n.log("Operación completada.");
    tracing::info!("terminado: {} ({} B)", final_path.display(), out.size);

    Ok(Report {
        original_size: info.zip_size,
        output_size: out.size,
        elapsed: started.elapsed(),
        format: out.format,
        path: final_path,
        entries: info.entries,
        skipped_symlinks: streamed.skipped_symlinks,
        verification: "Verificado completo: el CRC32 y el tamaño de cada archivo recuperado del resultado coinciden con los del ZIP original.".into(),
    })
}

/// Ante un error de escritura: si es disco lleno se activa la cancelación (no más escrituras).
fn fail_cleanup(n: &Notifier, cancel: &CancelFlag, e: &Error) {
    if matches!(e, Error::DiskFull) {
        cancel.cancel();
    }
    if !matches!(e, Error::Cancelled) {
        n.log(format!("Error: {e}"));
    }
}

/// Variante sin canal de eventos (tests y uso programático).
pub fn run_quiet(job: &Job, cancel: &CancelFlag) -> Result<Report> {
    let (tx, _rx) = crossbeam_channel::unbounded();
    run(job, cancel, tx, None)
}

/// Lanza `run` en un hilo y envía `Msg::Finished` al terminar. Si el hilo muere por un pánico,
/// también se informa (nunca se queda la interfaz esperando).
pub fn spawn(job: Job, cancel: CancelFlag, tx: Sender<Msg>, repaint: Option<Repaint>) {
    let spawned = std::thread::Builder::new()
        .name("megacompress-engine".into())
        .spawn({
            let (tx, repaint) = (tx.clone(), repaint.clone());
            move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&job, &cancel, tx.clone(), repaint.clone())
                }));
                let result = match outcome {
                    Ok(r) => r,
                    Err(payload) => {
                        // El pánico se convierte en un error normal que la UI puede mostrar.
                        let detail = payload
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "el motor terminó de forma inesperada".into());
                        tracing::error!("pánico en el hilo del motor: {detail}");
                        Err(Error::Internal(format!(
                            "el motor terminó de forma inesperada: {detail}"
                        )))
                    }
                };
                if let Err(e) = &result {
                    tracing::error!("trabajo terminado con error: {e:?}");
                }
                let _ = tx.send(Msg::Finished(result));
                if let Some(r) = &repaint {
                    r();
                }
            }
        });
    if let Err(e) = spawned {
        let _ = tx.send(Msg::Finished(Err(Error::from(e))));
        if let Some(r) = &repaint {
            r();
        }
    }
}

/// Mide el espacio libre de `dir` en un hilo aparte y lo envía como `Msg::FreeSpace`.
pub fn spawn_free_space(dir: PathBuf, tx: Sender<Msg>, repaint: Option<Repaint>) {
    let _ = std::thread::Builder::new()
        .name("megacompress-space".into())
        .spawn(move || {
            let bytes = free_space(&dir);
            let _ = tx.send(Msg::FreeSpace { dir, bytes });
            if let Some(r) = &repaint {
                r();
            }
        });
}

/// Análisis en un hilo aparte → `Msg::Analysis`.
pub fn spawn_analyze(
    path: PathBuf,
    dest: PathBuf,
    limits: Limits,
    tx: Sender<Msg>,
    repaint: Option<Repaint>,
) {
    let spawned = std::thread::Builder::new()
        .name("megacompress-analyze".into())
        .spawn({
            let (tx, repaint) = (tx.clone(), repaint.clone());
            move || {
                let r = analyze(&path, &dest, &limits);
                let _ = tx.send(Msg::Analysis(r));
                if let Some(rp) = &repaint {
                    rp();
                }
            }
        });
    if let Err(e) = spawned {
        let _ = tx.send(Msg::Analysis(Err(Error::from(e))));
    }
}
