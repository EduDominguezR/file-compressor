//! Modos y configuración adaptativa por RAM. El nivel NUNCA se expone al usuario.
//!
//! | RAM total | Rápido                         | Máxima                          |
//! |-----------|--------------------------------|---------------------------------|
//! | < 4 GB    | zstd 3, workers ≤ 2            | LZMA2 nivel 5, bloque 64 MB     |
//! | 4–8 GB    | zstd 4, workers ≤ 4            | LZMA2 nivel 6, bloque 128 MB    |
//! | > 8 GB    | zstd 5, workers = núcleos      | LZMA2 nivel 6, bloque 256 MB    |
//!
//! Si la RAM estimada del pipeline supera 1 GB se reducen primero workers/hilos, después el
//! tamaño de bloque y, solo como último recurso, el nivel.

use sysinfo::System;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Presupuesto objetivo de RAM del pipeline.
pub const RAM_BUDGET: u64 = GIB;
/// Tamaño de cada trozo independiente en LZMA2 multihilo (≥ diccionario).
pub const LZMA_CHUNK: u64 = 32 * MIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Zstandard → .tar.zst
    Fast,
    /// LZMA2 sólido → .7z
    Max,
}

impl Mode {
    pub fn extension(self) -> &'static str {
        match self {
            Mode::Fast => "tar.zst",
            Mode::Max => "7z",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub mode: Mode,
    pub zstd_level: i32,
    pub zstd_workers: u32,
    pub lzma_level: u32,
    pub lzma_threads: u32,
    /// Tamaño máximo (sin comprimir) de un bloque sólido; también es el tope de RAM del bloque.
    pub block_bytes: u64,
}

impl Config {
    pub fn for_system(mode: Mode, total_ram: u64, cores: usize) -> Config {
        let cores = cores.max(1) as u32;
        let (zl, zw, ll, lt, block) = if total_ram < 4 * GIB {
            (3, cores.min(2), 5, 1, 64 * MIB)
        } else if total_ram <= 8 * GIB {
            (4, cores.min(4), 6, cores.min(2), 128 * MIB)
        } else {
            (5, cores, 6, cores.min(4), 256 * MIB)
        };
        let mut cfg = Config {
            mode,
            zstd_level: zl,
            zstd_workers: zw,
            lzma_level: ll,
            lzma_threads: lt,
            block_bytes: block,
        };
        cfg.fit_budget();
        cfg
    }

    /// Estimación (conservadora, NO medida) de la RAM del pipeline.
    pub fn estimated_ram(&self) -> u64 {
        match self.mode {
            // ~48 MB por worker de zstdmt + base (ventana, buffers de tar y E/S).
            Mode::Fast => 32 * MIB + self.zstd_workers as u64 * 48 * MIB,
            // bloque bufferizado + (estado del encoder + trozo) por hilo.
            Mode::Max => {
                16 * MIB
                    + self.block_bytes
                    + self.lzma_threads as u64 * (lzma_encoder_mem(self.lzma_level) + LZMA_CHUNK)
            }
        }
    }

    /// Reduce workers → bloque → nivel hasta entrar en el presupuesto.
    pub fn fit_budget(&mut self) {
        while self.estimated_ram() > RAM_BUDGET {
            match self.mode {
                Mode::Fast => {
                    if self.zstd_workers > 1 {
                        self.zstd_workers -= 1;
                    } else if self.zstd_level > 1 {
                        self.zstd_level -= 1;
                    } else {
                        break;
                    }
                }
                Mode::Max => {
                    if self.lzma_threads > 1 {
                        self.lzma_threads -= 1;
                    } else if self.block_bytes > 16 * MIB {
                        self.block_bytes /= 2;
                    } else if self.lzma_level > 1 {
                        self.lzma_level -= 1;
                    } else {
                        break;
                    }
                }
            }
        }
    }
}

/// Memoria aproximada de un encoder LZMA2 por nivel (tabla de xz; orientativa).
fn lzma_encoder_mem(level: u32) -> u64 {
    match level {
        0..=3 => 32 * MIB,
        4 => 48 * MIB,
        5 | 6 => 96 * MIB,
        7 => 190 * MIB,
        8 => 370 * MIB,
        _ => 680 * MIB,
    }
}

/// (RAM total en bytes, núcleos lógicos).
pub fn system_resources() -> (u64, usize) {
    let mut sys = System::new();
    sys.refresh_memory();
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    (sys.total_memory(), cores)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_ram_table() {
        let c = Config::for_system(Mode::Fast, 2 * GIB, 16);
        assert_eq!((c.zstd_level, c.zstd_workers), (3, 2));
        let m = Config::for_system(Mode::Max, 2 * GIB, 16);
        assert_eq!((m.lzma_level, m.block_bytes), (5, 64 * MIB));
    }

    #[test]
    fn mid_ram_table() {
        let c = Config::for_system(Mode::Fast, 6 * GIB, 16);
        assert_eq!((c.zstd_level, c.zstd_workers), (4, 4));
        let m = Config::for_system(Mode::Max, 6 * GIB, 16);
        assert_eq!((m.lzma_level, m.block_bytes), (6, 128 * MIB));
    }

    #[test]
    fn high_ram_table_uses_all_cores_for_fast_until_budget() {
        let c = Config::for_system(Mode::Fast, 32 * GIB, 4);
        assert_eq!((c.zstd_level, c.zstd_workers), (5, 4));
        let m = Config::for_system(Mode::Max, 32 * GIB, 16);
        assert_eq!(m.lzma_level, 6);
        assert_eq!(m.block_bytes, 256 * MIB);
    }

    #[test]
    fn budget_reduces_workers_before_level() {
        let c = Config::for_system(Mode::Fast, 64 * GIB, 64);
        assert!(c.estimated_ram() <= RAM_BUDGET);
        assert!(c.zstd_workers < 64);
        assert_eq!(c.zstd_level, 5); // el nivel no se tocó
        let m = Config::for_system(Mode::Max, 64 * GIB, 64);
        assert!(m.estimated_ram() <= RAM_BUDGET);
        assert_eq!(m.lzma_level, 6);
    }

    #[test]
    fn never_zero_workers() {
        let c = Config::for_system(Mode::Fast, GIB, 1);
        assert!(c.zstd_workers >= 1);
    }
}
