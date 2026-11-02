//! Contrato entre el lector ZIP y los escritores de salida.
//! El lector NO conoce Zstd ni LZMA2: solo habla con `Sink`.

use std::io::Read;
use std::path::PathBuf;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct EntryMeta {
    /// Ruta relativa ya normalizada (separador `/`, sin `..`).
    pub path: String,
    pub size: u64,
    /// Segundos Unix (la hora DOS del ZIP se interpreta como UTC; best-effort).
    pub mtime: Option<i64>,
    /// Permisos Unix si el ZIP los trae (best-effort).
    pub unix_mode: Option<u32>,
    pub crc32: u32,
    /// Método de compresión original dentro del ZIP (informativo).
    pub method: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    TarZst,
    SevenZ,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::TarZst => "TAR + Zstandard (.tar.zst)",
            Format::SevenZ => "7z (LZMA2 sólido, .7z)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OutputInfo {
    /// Ruta del archivo producido (el temporal hasta que el orquestador lo publica).
    pub path: PathBuf,
    pub size: u64,
    pub format: Format,
}

/// Callback con el paso REAL de finalización en curso (p. ej. "Cerrando frame Zstandard").
pub type StatusCb = Box<dyn FnMut(&str) + Send>;

pub trait Sink: Send {
    /// Los sinks que tienen pasos de cierre observables los comunican por aquí (opcional).
    fn set_status_callback(&mut self, _cb: StatusCb) {}

    fn begin(&mut self, total_entries: u64, total_bytes: u64) -> Result<()>;
    /// Para directorios (`meta.is_dir`) `reader` está vacío.
    fn add_entry(&mut self, meta: EntryMeta, reader: &mut dyn Read) -> Result<()>;
    fn finish(self: Box<Self>) -> Result<OutputInfo>;
    /// Descarta todo lo escrito (borra el temporal).
    fn abort(self: Box<Self>) -> Result<()>;
}
