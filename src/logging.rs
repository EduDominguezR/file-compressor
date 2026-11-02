//! Log a `%APPDATA%\MegaCompress\logs\megacompress.log` (Windows) o el equivalente de
//! `dirs::data_dir()` en Linux/macOS. Sin rotación (limitación conocida).

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Mutex;

/// Devuelve la ruta del log si se pudo inicializar. Si falla, la app sigue sin log.
pub fn init() -> Option<PathBuf> {
    let dir = dirs::data_dir()?.join("MegaCompress").join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("megacompress.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .ok()?;
    Some(path)
}
