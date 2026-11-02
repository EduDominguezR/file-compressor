//! Verificación de la salida .tar.zst: descomprime TODO en streaming (libzstd valida el checksum
//! de cada frame), recorre el TAR y compara tamaño y CRC32 de cada archivo con el del ZIP ORIGINAL.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::engine::cancel::CancelFlag;
use crate::engine::BUF_SIZE;
use crate::error::{Error, Result};
use crate::verify::crc::{compare_entry, Crc32, Expected, VerifyProgress};

pub fn verify_tar_zst(
    path: &Path,
    expected: &mut Expected,
    cancel: &CancelFlag,
    mut on_progress: impl FnMut(VerifyProgress),
) -> Result<()> {
    let entries_total = expected.file_count();
    let bytes_total = expected.total_bytes();
    let file = File::open(path)?;
    let dec = zstd::stream::read::Decoder::new(file)
        .map_err(|e| Error::Verify(format!("cabecera zstd inválida: {e}")))?;
    let mut archive = tar::Archive::new(dec);
    let mut buf = vec![0u8; BUF_SIZE];
    let mut prog = VerifyProgress {
        entries_total,
        bytes_total,
        ..Default::default()
    };

    let entries = archive
        .entries()
        .map_err(|e| Error::Verify(format!("TAR ilegible: {e}")))?;
    for entry in entries {
        cancel.check()?;
        let mut entry = entry.map_err(|e| Error::Verify(format!("TAR dañado o truncado: {e}")))?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let name = entry
            .path()
            .map_err(|e| Error::Verify(format!("ruta ilegible en el TAR: {e}")))?
            .to_string_lossy()
            .replace('\\', "/");
        let mut crc = Crc32::new();
        loop {
            let n = entry
                .read(&mut buf)
                .map_err(|e| Error::Verify(format!("datos dañados en «{name}»: {e}")))?;
            if n == 0 {
                break;
            }
            crc.update(&buf[..n]);
            prog.bytes_done += n as u64;
            on_progress(prog);
            cancel.check()?;
        }
        let (size, got) = (crc.len(), crc.finalize());
        compare_entry(expected, &name, size, got).map_err(Error::Verify)?;
        prog.entries_done += 1;
        on_progress(prog);
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
