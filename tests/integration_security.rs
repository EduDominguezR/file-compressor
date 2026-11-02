//! Seguridad: rutas peligrosas, nombres inválidos, colisiones, ZIP bombs y límites.
//! Cada ZIP "malicioso" se fabrica parcheando bytes de un ZIP válido (mismo largo de nombre).
mod common;
use common::*;
use megacompress::engine::cancel::CancelFlag;
use megacompress::engine::limits::Limits;
use megacompress::engine::mode::Mode;
use megacompress::engine::orchestrator::{run_quiet, Job};
use megacompress::error::Error;
use zip::CompressionMethod;

fn zip_with_name(original: &str, patched: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    make_zip(
        &z,
        &[(original.to_string(), b"contenido".to_vec())],
        CompressionMethod::Stored,
        false,
    );
    patch_bytes(&z, original.as_bytes(), patched.as_bytes());
    (d, z)
}

fn assert_clean(dir: &std::path::Path) {
    assert_eq!(
        listing(dir),
        vec!["x.zip".to_string()],
        "sin salida parcial ni temporales"
    );
}

#[test]
fn rejects_dangerous_names_in_both_modes() {
    let cases = [
        ("xx/evil.txt", "../evil.txt"),       // ..
        ("Xetc/passwd", "/etc/passwd"),       // absoluta
        ("xxserver/share", "//server/share"), // UNC
        ("a_b.txt", "a:b.txt"),               // ADS / unidad
        ("xxx.txt", "CON.txt"),               // reservado
        ("file.txt", "file.tx."),             // termina en punto
        ("file.tx_", "file.tx "),             // termina en espacio
        ("a_b.txt", "a?b.txt"),               // carácter inválido
    ];
    for (orig, bad) in cases {
        let (d, z) = zip_with_name(orig, bad);
        for mode in [Mode::Fast, Mode::Max] {
            let e = run_mode(&z, mode).unwrap_err();
            assert!(matches!(e, Error::UnsafePath(_)), "{bad:?} {mode:?}: {e:?}");
        }
        assert_clean(d.path());
    }
}

#[test]
fn rejects_exact_duplicate_names() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    let entries = vec![
        ("one.txt".to_string(), b"1".to_vec()),
        ("two.txt".to_string(), b"2".to_vec()),
    ];
    make_zip(&z, &entries, CompressionMethod::Stored, false);
    patch_bytes(&z, b"two.txt", b"one.txt");
    let e = run_mode(&z, Mode::Fast).unwrap_err();
    assert!(matches!(e, Error::DuplicateEntry(_)), "{e:?}");
    assert_clean(d.path());
}

#[test]
fn rejects_unicode_collision_after_nfc_normalization() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    // Dos formas Unicode canónicamente equivalentes: NFC debe colapsarlas y rechazarlas.
    let entries = vec![
        ("cafe\u{301}.txt".to_string(), b"1".to_vec()), // e + acento combinante
        ("caf\u{e9}.txt".to_string(), b"2".to_vec()),   // é precompuesta
    ];
    make_zip(&z, &entries, CompressionMethod::Stored, false);
    let e = run_mode(&z, Mode::Fast).unwrap_err();
    assert!(matches!(e, Error::DuplicateEntry(_)), "{e:?}");
    assert_clean(d.path());
}

#[test]
fn rejects_collision_after_case_folding() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    let entries = vec![
        ("dir/a.txt".to_string(), b"1".to_vec()),
        ("dir/b.txt".to_string(), b"2".to_vec()),
    ];
    make_zip(&z, &entries, CompressionMethod::Stored, false);
    patch_bytes(&z, b"dir/b.txt", b"dir/A.txt");
    let e = run_mode(&z, Mode::Max).unwrap_err();
    assert!(matches!(e, Error::DuplicateEntry(_)), "{e:?}");
    assert_clean(d.path());
}

fn run_with_limits(
    z: &std::path::Path,
    limits: Limits,
) -> megacompress::error::Result<megacompress::engine::orchestrator::Report> {
    let mut job = Job::new(z.to_path_buf(), Mode::Fast);
    job.limits = limits;
    run_quiet(&job, &CancelFlag::new())
}

#[test]
fn rejects_too_many_entries() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    make_zip(&z, &three_texts(), CompressionMethod::Deflated, false);
    let e = run_with_limits(
        &z,
        Limits {
            max_entries: 2,
            ..Limits::default()
        },
    )
    .unwrap_err();
    assert!(matches!(e, Error::LimitExceeded(_)), "{e:?}");
    assert_clean(d.path());
}

#[test]
fn rejects_total_uncompressed_over_limit() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    make_zip(&z, &three_texts(), CompressionMethod::Deflated, false);
    let e = run_with_limits(
        &z,
        Limits {
            max_total_uncompressed: 1000,
            ..Limits::default()
        },
    )
    .unwrap_err();
    assert!(matches!(e, Error::LimitExceeded(_)), "{e:?}");
    assert_clean(d.path());
}

#[test]
fn rejects_excessive_expansion_ratio() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    // 20 MiB de ceros con Deflate ≈ 20 KB → ~1000:1; con límite 100:1 debe rechazarse.
    make_zip(
        &z,
        &[("ceros.bin".to_string(), vec![0u8; 20 * 1024 * 1024])],
        CompressionMethod::Deflated,
        false,
    );
    let e = run_with_limits(
        &z,
        Limits {
            max_ratio: 100,
            ..Limits::default()
        },
    )
    .unwrap_err();
    assert!(matches!(e, Error::LimitExceeded(_)), "{e:?}");
    assert!(e.to_string().contains("ZIP bomb"));
    assert_clean(d.path());
}

#[test]
fn legit_highly_compressible_file_passes_with_default_ratio_when_small() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    // 512 KiB de ceros: por debajo de ratio_min_size → no se somete a la relación.
    make_zip(
        &z,
        &[("ceros.bin".to_string(), vec![0u8; 512 * 1024])],
        CompressionMethod::Deflated,
        false,
    );
    assert!(run_with_limits(&z, Limits::default()).is_ok());
}

#[test]
fn overlapping_or_inconsistent_sizes_are_rejected() {
    // Ensancha el "tamaño comprimido" del directorio central más allá del propio ZIP.
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    make_zip(
        &z,
        &[("a.txt".to_string(), vec![b'a'; 100])],
        CompressionMethod::Stored,
        false,
    );
    let mut b = file_bytes(&z);
    let mut i = 0;
    while i + 24 <= b.len() {
        if b[i..i + 4] == [0x50, 0x4B, 0x01, 0x02] {
            b[i + 20..i + 24].copy_from_slice(&0x7FFF_FFF0u32.to_le_bytes()); // comprimido enorme
        }
        i += 1;
    }
    std::fs::write(&z, b).unwrap();
    let e = run_mode(&z, Mode::Fast).unwrap_err();
    assert!(matches!(e, Error::Corrupt(_)), "{e:?}");
    assert_clean(d.path());
}

#[test]
fn symlink_entries_are_omitted_and_counted() {
    use std::fs::File;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    let mut w = ZipWriter::new(File::create(&z).unwrap());
    let normal = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    w.start_file("normal.txt", normal).unwrap();
    w.write_all(b"hola").unwrap();
    w.start_file("enlace", normal).unwrap();
    w.write_all(b"../fuera.txt").unwrap();
    w.finish().unwrap();

    // `unix_permissions()` del escritor recorta los bits de tipo (deja 0o777). Para fabricar
    // un enlace simbólico real se parchea la entrada del directorio central: sistema Unix y
    // atributos externos 0o120777.
    let mut b = file_bytes(&z);
    let mut i = 0;
    let mut patched = false;
    while i + 46 <= b.len() {
        if b[i..i + 4] == [0x50, 0x4B, 0x01, 0x02] {
            let name_len = u16::from_le_bytes([b[i + 28], b[i + 29]]) as usize;
            if i + 46 + name_len <= b.len() && &b[i + 46..i + 46 + name_len] == b"enlace" {
                b[i + 4..i + 6].copy_from_slice(&0x0314u16.to_le_bytes());
                b[i + 38..i + 42].copy_from_slice(&(0o120777u32 << 16).to_le_bytes());
                patched = true;
                break;
            }
        }
        i += 1;
    }
    assert!(patched, "no se encontró la entrada central de «enlace»");
    std::fs::write(&z, b).unwrap();

    let report = run_mode(&z, Mode::Fast).unwrap();
    assert_eq!(report.skipped_symlinks, 1);
    let extracted = extract_tar_zst(&report.path);
    assert_eq!(
        extracted.get("normal.txt").map(Vec::as_slice),
        Some(&b"hola"[..])
    );
    assert!(!extracted.contains_key("enlace"));
}
