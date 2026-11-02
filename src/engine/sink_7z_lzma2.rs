//! Salida Máxima: 7z con LZMA2 SÓLIDO (`sevenz-rust2`, Rust puro).
//!
//! Auditoría: `ArchiveWriter::push_archive_entries(entries, readers)` exige todos los lectores del
//! bloque a la vez, pero el `ZipArchive` solo entrega una entrada cada vez. Por eso:
//!   * las entradas pequeñas se bufferizan en RAM hasta `block_bytes` y se escriben como UN bloque
//!     sólido (el diccionario se reinicia entre bloques);
//!   * una entrada ≥ `block_bytes` se transmite en streaming como su propio bloque
//!     (un archivo de 15 GB NO ocupa 15 GB de RAM);
//!   * la RAM del bloque está acotada por `block_bytes`.
//!
//! La cancelación se consulta entre bloques y cada ~1 MB al leer entradas grandes; un bloque ya
//! enviado al codificador no es interrumpible (la latencia de cancelación llega a un bloque).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sevenz_rust2::encoder_options::Lzma2Options;
use sevenz_rust2::{ArchiveEntry, ArchiveWriter, NtTime, SourceReader};

use crate::engine::cancel::CancelFlag;
use crate::engine::mode::{Config, LZMA_CHUNK};
use crate::engine::sink::{EntryMeta, Format, OutputInfo, Sink, StatusCb};
use crate::engine::BUF_SIZE;
use crate::error::{Error, Result};

pub struct SevenZSink {
    tmp: PathBuf,
    writer: Option<ArchiveWriter<BufWriter<File>>>,
    block_bytes: u64,
    pending: Vec<(ArchiveEntry, Vec<u8>)>,
    pending_bytes: u64,
    cancel: CancelFlag,
    status: Option<StatusCb>,
}

impl SevenZSink {
    fn say(&mut self, msg: &str) {
        if let Some(cb) = self.status.as_mut() {
            cb(msg);
        }
    }

    pub fn create(tmp: &Path, cfg: &Config, cancel: CancelFlag) -> Result<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(tmp)?;
        let mut writer = ArchiveWriter::new(BufWriter::with_capacity(BUF_SIZE, file))?;
        writer.set_encrypt_header(false);
        let opts = if cfg.lzma_threads > 1 {
            Lzma2Options::from_level_mt(cfg.lzma_level, cfg.lzma_threads, LZMA_CHUNK)
        } else {
            Lzma2Options::from_level(cfg.lzma_level)
        };
        writer.set_content_methods(vec![opts.into()]);
        Ok(Self {
            tmp: tmp.to_path_buf(),
            writer: Some(writer),
            block_bytes: cfg.block_bytes,
            pending: Vec::new(),
            pending_bytes: 0,
            cancel,
            status: None,
        })
    }

    fn writer(&mut self) -> Result<&mut ArchiveWriter<BufWriter<File>>> {
        self.writer
            .as_mut()
            .ok_or_else(|| Error::Internal("el escritor 7z ya estaba cerrado".into()))
    }

    fn make_entry(meta: &EntryMeta) -> ArchiveEntry {
        let mut e = if meta.is_dir {
            ArchiveEntry::new_directory(&meta.path)
        } else {
            ArchiveEntry::new_file(&meta.path)
        };
        if let Some(secs) = meta.mtime {
            if secs >= 0 {
                if let Ok(t) =
                    NtTime::try_from(SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
                {
                    e.has_last_modified_date = true;
                    e.last_modified_date = t;
                }
            }
        }
        e
    }

    /// Escribe el bloque sólido pendiente.
    fn flush_block(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.cancel.check()?;
        let (entries, data): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.pending).into_iter().unzip();
        self.pending_bytes = 0;
        let readers: Vec<SourceReader<Cursor<Vec<u8>>>> = data
            .into_iter()
            .map(|d| SourceReader::new(Cursor::new(d)))
            .collect();
        self.writer()?.push_archive_entries(entries, readers)?;
        Ok(())
    }
}

impl Sink for SevenZSink {
    fn set_status_callback(&mut self, cb: StatusCb) {
        self.status = Some(cb);
    }

    fn begin(&mut self, _total_entries: u64, _total_bytes: u64) -> Result<()> {
        Ok(())
    }

    fn add_entry(&mut self, meta: EntryMeta, reader: &mut dyn Read) -> Result<()> {
        let entry = Self::make_entry(&meta);

        // Directorios y archivos vacíos: sin flujo de datos.
        if meta.is_dir || meta.size == 0 {
            self.writer()?
                .push_archive_entry(entry, None::<io::Empty>)?;
            return Ok(());
        }

        // Entrada grande: su propio bloque, en streaming (sin bufferizar).
        if meta.size >= self.block_bytes {
            self.flush_block()?;
            self.writer()?.push_archive_entry(entry, Some(reader))?;
            return Ok(());
        }

        // Entrada pequeña: al bloque sólido en curso.
        if self.pending_bytes + meta.size > self.block_bytes {
            self.flush_block()?;
        }
        // Acotado: `size < block_bytes` y el lector no entrega más de `size` bytes
        // (CheckedReader lo impone), así que esto NO es un read_to_end sin límite.
        let mut data = Vec::with_capacity(meta.size as usize);
        reader.read_to_end(&mut data)?;
        self.pending_bytes += data.len() as u64;
        self.pending.push((entry, data));
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<OutputInfo> {
        self.say("Comprimiendo el último bloque sólido…");
        self.flush_block()?;
        let writer = self
            .writer
            .take()
            .ok_or_else(|| Error::Internal("el escritor 7z ya estaba cerrado".into()))?;
        self.say("Escribiendo metadatos del archivo 7z…");
        let mut buffered = writer.finish()?;
        self.say("Cerrando el contenedor…");
        buffered.flush()?;
        let file = buffered
            .into_inner()
            .map_err(|e| Error::from(e.into_error()))?;
        self.say("Sincronizando el archivo…");
        file.sync_all()?;
        let size = file.metadata()?.len();
        Ok(OutputInfo {
            path: self.tmp.clone(),
            size,
            format: Format::SevenZ,
        })
    }

    fn abort(mut self: Box<Self>) -> Result<()> {
        self.pending.clear();
        drop(self.writer.take());
        let _ = fs::remove_file(&self.tmp);
        Ok(())
    }
}
