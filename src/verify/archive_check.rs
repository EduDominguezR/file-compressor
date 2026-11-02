//! Verificación INTEGRAL de la salida .7z (siempre, antes de publicar):
//! abre el contenedor, descomprime cada entrada en streaming, calcula el CRC32 REAL del contenido
//! recuperado y lo compara con el CRC32 y el tamaño del ZIP ORIGINAL (no con el CRC que guarda el
//! propio .7z). Detecta entradas que faltan, inesperadas, duplicadas, de otro tamaño o alteradas.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use sevenz_rust2::{ArchiveReader, Password};

use crate::engine::cancel::CancelFlag;
use crate::engine::BUF_SIZE;
use crate::error::{Error, Result};
use crate::verify::crc::{compare_entry, Crc32, Expected, VerifyProgress};

pub fn verify_7z_full(
    path: &Path,
    expected: &mut Expected,
    cancel: &CancelFlag,
    mut on_progress: impl FnMut(VerifyProgress),
) -> Result<()> {
    let file = File::open(path)?;
    let mut reader =
        ArchiveReader::new(BufReader::with_capacity(BUF_SIZE, file), Password::empty()).map_err(
            |e| {
                Error::Verify(format!(
                    "no se pudo abrir el .7z (¿truncado o dañado?): {e}"
                ))
            },
        )?;

    let mut prog = VerifyProgress {
        entries_total: expected.file_count(),
        bytes_total: expected.total_bytes(),
        ..Default::default()
    };
    let mut failure: Option<Error> = None;
    let mut buf = vec![0u8; BUF_SIZE];

    let result = reader.for_each_entries(|entry, data| {
        // Directorios y archivos vacíos no tienen flujo de datos: los archivos vacíos se comparan
        // aparte (más abajo), así no dependen de si el recorrido los visita o no.
        if entry.is_directory() || !entry.has_stream() {
            return Ok(true);
        }
        let name = entry.name().to_string();
        let mut crc = Crc32::new();
        loop {
            match data.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    crc.update(&buf[..n]);
                    prog.bytes_done += n as u64;
                    on_progress(prog);
                    if cancel.is_cancelled() {
                        failure = Some(Error::Cancelled);
                        return Ok(false);
                    }
                }
                Err(e) => {
                    failure = Some(Error::Verify(format!("lectura fallida en «{name}»: {e}")));
                    return Ok(false);
                }
            }
        }
        let (size, got) = (crc.len(), crc.finalize());
        if let Err(msg) = compare_entry(expected, &name, size, got) {
            failure = Some(Error::Verify(msg));
            return Ok(false);
        }
        prog.entries_done += 1;
        on_progress(prog);
        Ok(true)
    });

    if let Some(e) = failure {
        return Err(e);
    }
    result.map_err(|e| Error::Verify(format!("descompresión fallida: {e}")))?;

    // Archivos vacíos: tamaño 0 y CRC32 0 (el CRC32 de una entrada vacía).
    for f in reader
        .archive()
        .files
        .iter()
        .filter(|f| !f.is_directory() && !f.has_stream())
    {
        compare_entry(expected, f.name(), 0, 0).map_err(Error::Verify)?;
        prog.entries_done += 1;
    }
    if expected.remaining() != 0 {
        return Err(Error::Verify(format!(
            "faltan {} entradas del ZIP original en el contenedor",
            expected.remaining()
        )));
    }
    on_progress(prog);
    Ok(())
}
