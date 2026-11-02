mod common;
use common::*;

use megacompress::engine::cancel::CancelFlag;
use megacompress::engine::mode::Mode;
use megacompress::engine::orchestrator::{run_quiet, Job};
use megacompress::error::Error;
use zip::CompressionMethod;

const MODES: [Mode; 2] = [Mode::Fast, Mode::Max];

fn roundtrip(entries: Vec<(String, Vec<u8>)>, method: CompressionMethod) {
    for mode in MODES {
        let d = tempfile::tempdir().unwrap();
        let z = zip_in(d.path(), "datos.zip");
        make_zip(&z, &entries, method, false);
        let before = file_bytes(&z);
        let rep = run_mode(&z, mode).unwrap_or_else(|e| panic!("{mode:?}: {e}"));
        assert!(rep.path.exists());
        assert_eq!(
            rep.path.extension().unwrap(),
            if mode == Mode::Fast { "zst" } else { "7z" }
        );
        assert_eq!(extract(&rep.path, mode), expected_map(&entries), "{mode:?}");
        assert_eq!(file_bytes(&z), before, "el original no debe cambiar");
        // Sin temporales residuales.
        assert!(listing(d.path())
            .iter()
            .all(|n| !n.ends_with(".megacompress.tmp")));
    }
}

#[test]
fn three_text_files_both_modes() {
    roundtrip(three_texts(), CompressionMethod::Deflated);
}

#[test]
fn stored_and_bzip2_inputs() {
    roundtrip(three_texts(), CompressionMethod::Stored);
    roundtrip(three_texts(), CompressionMethod::Bzip2);
}

#[test]
fn synthetic_images_both_modes() {
    // Ruido pseudoaleatorio con extensión .jpg/.png: sustituto sintético de imágenes ya comprimidas.
    let entries = vec![
        ("fotos/a.jpg".to_string(), noise(200_000, 1)),
        ("fotos/b.png".to_string(), noise(150_000, 2)),
        ("leeme.txt".to_string(), text(50, "x")),
    ];
    roundtrip(entries, CompressionMethod::Stored);
}

#[test]
fn thousands_of_small_entries_both_modes() {
    let entries: Vec<_> = (0..3000)
        .map(|i| {
            (
                format!("d{}/f{i}.txt", i % 17),
                format!("contenido {i}\n").repeat(1 + i % 5).into_bytes(),
            )
        })
        .collect();
    roundtrip(entries, CompressionMethod::Deflated);
}

#[test]
fn empty_files_and_directories_survive() {
    let entries = vec![
        ("vacio.txt".to_string(), Vec::new()),
        ("x/y.txt".to_string(), text(3, "y")),
    ];
    roundtrip(entries, CompressionMethod::Deflated);
}

#[test]
fn existing_destination_is_never_overwritten() {
    for mode in MODES {
        let d = tempfile::tempdir().unwrap();
        let z = zip_in(d.path(), "datos.zip");
        make_zip(&z, &three_texts(), CompressionMethod::Deflated, false);
        let ext = mode.extension();
        let existing = d.path().join(format!("datos.{ext}"));
        std::fs::write(&existing, b"NO TOCAR").unwrap();
        let rep = run_mode(&z, mode).unwrap();
        assert_eq!(std::fs::read(&existing).unwrap(), b"NO TOCAR");
        assert_eq!(
            rep.path.file_name().unwrap().to_string_lossy(),
            format!("datos (1).{ext}")
        );
    }
}

#[test]
fn insufficient_space_fails_before_writing_anything() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "datos.zip");
    make_zip(&z, &three_texts(), CompressionMethod::Deflated, false);
    let mut job = Job::new(z.clone(), Mode::Fast);
    job.free_space_override = Some(1024);
    let e = run_quiet(&job, &CancelFlag::new()).unwrap_err();
    assert!(matches!(e, Error::InsufficientSpace { .. }), "{e:?}");
    assert_eq!(listing(d.path()), vec!["datos.zip".to_string()]);
}
