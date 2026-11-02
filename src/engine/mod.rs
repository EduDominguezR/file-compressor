pub mod cancel;
pub mod limits;
pub mod mode;
pub mod orchestrator;
pub mod progress;
pub mod reader_zip;
pub mod sink;
pub mod sink_7z_lzma2;
pub mod sink_tar_zst;

use std::sync::Arc;

use crate::error::Result;

/// Tamaño de buffer de E/S (1 MB). Ajustable aquí.
pub const BUF_SIZE: usize = 1024 * 1024;

/// Callback para despertar la UI (p. ej. `ctx.request_repaint()`).
pub type Repaint = Arc<dyn Fn() + Send + Sync>;

/// Mensajes del motor hacia la UI.
pub enum Msg {
    Analysis(Result<orchestrator::Analysis>),
    /// Espacio libre del destino (medido fuera del hilo de la interfaz).
    FreeSpace {
        dir: std::path::PathBuf,
        bytes: Option<u64>,
    },
    /// Progreso de la fase en curso (compresión, finalización o verificación).
    Progress(progress::Snapshot),
    /// Línea real del registro del motor.
    Log(String),
    Finished(Result<orchestrator::Report>),
}
