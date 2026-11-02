//! Salida Rápida: TAR en streaming → Zstandard (multihilo con `zstdmt`, checksum por frame).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use zstd::stream::write::Encoder;

use crate::engine::mode::Config;
use crate::engine::sink::{EntryMeta, Format, OutputInfo, Sink, StatusCb};
use crate::engine::BUF_SIZE;
use crate::error::{Error, Result};

type Enc = Encoder<'static, BufWriter<File>>;

pub struct TarZstSink {
    tmp: PathBuf,
    builder: Option<tar::Builder<Enc>>,
    status: Option<StatusCb>,
}

impl TarZstSink {
    fn say(&mut self, msg: &str) {
        if let Some(cb) = self.status.as_mut() {
            cb(msg);
        }
    }
}

impl TarZstSink {
    /// Crea el temporal con `create_new`: nunca reutiliza ni pisa un archivo existente.
    pub fn create(tmp: &Path, cfg: &Config) -> Result<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(tmp)?;
        let mut enc = Encoder::new(BufWriter::with_capacity(BUF_SIZE, file), cfg.zstd_level)?;
        enc.include_checksum(true)?;
        if cfg.zstd_workers > 0 {
            enc.multithread(cfg.zstd_workers)?;
        }
        Ok(Self {
            tmp: tmp.to_path_buf(),
            builder: Some(tar::Builder::new(enc)),
            status: None,
        })
    }

    fn builder(&mut self) -> Result<&mut tar::Builder<Enc>> {
        self.builder
            .as_mut()
            .ok_or_else(|| Error::Internal("el escritor TAR ya estaba cerrado".into()))
    }
}

impl Sink for TarZstSink {
    fn set_status_callback(&mut self, cb: StatusCb) {
        self.status = Some(cb);
    }

    fn begin(&mut self, _total_entries: u64, _total_bytes: u64) -> Result<()> {
        Ok(())
    }

    fn add_entry(&mut self, meta: EntryMeta, reader: &mut dyn Read) -> Result<()> {
        let mode = meta.unix_mode.map(|m| m & 0o7777).filter(|m| *m != 0);
        let b = self.builder()?;
        let mut h = tar::Header::new_gnu();
        h.set_mtime(meta.mtime.unwrap_or(0).max(0) as u64);
        if meta.is_dir {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_mode(mode.unwrap_or(0o755));
            let p = format!("{}/", meta.path);
            b.append_data(&mut h, &p, io::empty())?;
        } else {
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(meta.size);
            h.set_mode(mode.unwrap_or(0o644));
            b.append_data(&mut h, &meta.path, reader)?;
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<OutputInfo> {
        let builder = self
            .builder
            .take()
            .ok_or_else(|| Error::Internal("el escritor TAR ya estaba cerrado".into()))?;
        self.say("Finalizando TAR…");
        let enc = builder.into_inner()?; // escribe el final del TAR
        self.say("Cerrando frame Zstandard…");
        let mut w = enc.finish()?; // cierra el último frame zstd
        self.say("Vaciando buffers a disco…");
        w.flush()?;
        let file = w.into_inner().map_err(|e| Error::from(e.into_error()))?;
        self.say("Sincronizando el archivo…");
        file.sync_all()?;
        let size = file.metadata()?.len();
        Ok(OutputInfo {
            path: self.tmp.clone(),
            size,
            format: Format::TarZst,
        })
    }

    fn abort(mut self: Box<Self>) -> Result<()> {
        drop(self.builder.take()); // cierra el descriptor antes de borrar (Windows)
        let _ = fs::remove_file(&self.tmp);
        Ok(())
    }
}
