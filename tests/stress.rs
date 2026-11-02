//! PRUEBAS DE ESTRÉS / MANUALES. Todas `#[ignore]`: NO se ejecutan con `cargo test`.
//!
//! ```text
//! cargo test --release --test stress -- --ignored --nocapture --test-threads=1
//! ```
//! Variables: `MEGACOMPRESS_STRESS_GB` (tamaño de los ZIP grandes, por defecto 1),
//! `MEGACOMPRESS_STRESS_DIR` (carpeta de trabajo con espacio suficiente; por defecto la temporal),
//! `MEGACOMPRESS_SMALL_FS_DIR` (carpeta en un sistema de archivos PEQUEÑO para el test de disco
//! lleno real, p. ej. un tmpfs de 100 MB).
//! Imprimen tiempos; NO afirman rendimiento. Los resultados medidos se anotan en el README.
mod common;
use common::*;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use megacompress::engine::cancel::CancelFlag;
use megacompress::engine::mode::Mode;
use megacompress::engine::orchestrator::{run_quiet, Job};
use megacompress::error::Error;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

fn workdir() -> tempfile::TempDir {
    match std::env::var("MEGACOMPRESS_STRESS_DIR") {
        Ok(d) => tempfile::tempdir_in(d).unwrap(),
        Err(_) => tempfile::tempdir().unwrap(),
    }
}

fn gb() -> u64 {
    std::env::var("MEGACOMPRESS_STRESS_GB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

/// Escribe un ZIP en streaming (nunca se mantiene en RAM): `entries` archivos de `entry_bytes`,
/// mezcla de ruido y texto repetido para que comprima algo.
fn write_zip(path: &Path, entries: u64, entry_bytes: u64, method: CompressionMethod) {
    let mut w = ZipWriter::new(std::io::BufWriter::new(
        std::fs::File::create(path).unwrap(),
    ));
    let opts = SimpleFileOptions::default()
        .compression_method(method)
        .large_file(entry_bytes >= (1 << 32));
    let chunk = noise(1 << 20, 7);
    let txt = text(20_000, "estres");
    for i in 0..entries {
        w.start_file(format!("d{}/f{i}.bin", i % 100), opts)
            .unwrap();
        let mut left = entry_bytes;
        let mut k = 0usize;
        while left > 0 {
            let src: &[u8] = if (i + k as u64).is_multiple_of(2) {
                &chunk
            } else {
                &txt
            };
            let n = left.min(src.len() as u64) as usize;
            w.write_all(&src[..n]).unwrap();
            left -= n as u64;
            k += 1;
        }
    }
    w.finish().unwrap();
}

fn measure(zip: &Path, mode: Mode) {
    let t = Instant::now();
    let rep = run_quiet(&Job::new(zip.to_path_buf(), mode), &CancelFlag::new());
    match rep {
        Ok(r) => eprintln!(
            "[{mode:?}] ok en {:.1}s · original {} B → {} B · {}",
            t.elapsed().as_secs_f64(),
            r.original_size,
            r.output_size,
            r.path.display()
        ),
        Err(e) => panic!("[{mode:?}] falló: {e}"),
    }
}

fn big_zip(total_gb: u64) -> (tempfile::TempDir, PathBuf) {
    let d = workdir();
    let z = d.path().join("grande.zip");
    let per = 256u64 << 20; // entradas de 256 MiB
    write_zip(&z, (total_gb << 30) / per, per, CompressionMethod::Stored);
    (d, z)
}

#[test]
#[ignore]
fn stress_zip_sizes_fast() {
    let (_d, z) = big_zip(gb());
    measure(&z, Mode::Fast);
}

#[test]
#[ignore]
fn stress_zip_sizes_max() {
    let (_d, z) = big_zip(gb());
    measure(&z, Mode::Max);
}

#[test]
#[ignore]
fn stress_single_huge_file_both_modes() {
    let d = workdir();
    let z = d.path().join("uno.zip");
    write_zip(&z, 1, gb() << 30, CompressionMethod::Stored);
    measure(&z, Mode::Fast);
    measure(&z, Mode::Max);
}

#[test]
#[ignore]
fn stress_real_zip64_over_4gib() {
    let d = workdir();
    let z = d.path().join("zip64.zip");
    write_zip(&z, 1, (4u64 << 30) + (512 << 20), CompressionMethod::Stored); // 4,5 GiB
    measure(&z, Mode::Fast);
}

#[test]
#[ignore]
fn stress_many_entries() {
    let d = workdir();
    let z = d.path().join("muchas.zip");
    write_zip(&z, 900_000, 64, CompressionMethod::Stored);
    measure(&z, Mode::Fast);
    measure(&z, Mode::Max);
}

#[test]
#[ignore]
fn stress_real_disk_full() {
    let Ok(small) = std::env::var("MEGACOMPRESS_SMALL_FS_DIR") else {
        eprintln!(
            "define MEGACOMPRESS_SMALL_FS_DIR (sistema de archivos pequeño) para esta prueba"
        );
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let z = src.path().join("x.zip");
    write_zip(&z, 8, 64 << 20, CompressionMethod::Stored); // 512 MiB de ruido/texto
    let before = file_bytes(&z);
    for mode in [Mode::Fast, Mode::Max] {
        let mut job = Job::new(z.clone(), mode);
        job.out_dir = Some(PathBuf::from(&small));
        job.free_space_override = Some(u64::MAX); // saltar la comprobación previa: probar ENOSPC real
        let e = run_quiet(&job, &CancelFlag::new()).unwrap_err();
        assert!(matches!(e, Error::DiskFull), "{mode:?}: {e:?}");
        assert!(listing(Path::new(&small))
            .iter()
            .all(|n| !n.contains("megacompress")));
    }
    assert_eq!(file_bytes(&z), before);
}
