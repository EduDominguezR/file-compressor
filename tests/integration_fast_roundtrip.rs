//! Prueba mínima de punta a punta pedida en la Fase 1:
//! ZIP pequeño → modo Rápido (.tar.zst) → extracción → bytes y CRC32 idénticos.
//! Incluye un archivo vacío, una ruta Unicode y una carpeta anidada.
mod common;
use common::*;

use megacompress::engine::cancel::CancelFlag;
use megacompress::engine::mode::Mode;
use megacompress::engine::orchestrator::{run_quiet, Job};
use zip::CompressionMethod;

fn entries() -> Vec<(String, Vec<u8>)> {
    vec![
        ("hola.txt".to_string(), text(2_000, "hola")),
        ("vacio.bin".to_string(), Vec::new()),
        ("carpeta/caf\u{e9}.txt".to_string(), text(300, "unicode")),
        ("carpeta/sub/datos.bin".to_string(), noise(32_000, 42)),
    ]
}

#[test]
fn fast_mode_roundtrip_preserves_bytes_and_crc32() {
    let d = tempfile::tempdir().unwrap();
    let zip = zip_in(d.path(), "pequeno.zip");
    let entries = entries();
    make_zip(&zip, &entries, CompressionMethod::Deflated, false);
    let original_bytes = file_bytes(&zip);

    let report = run_quiet(&Job::new(zip.clone(), Mode::Fast), &CancelFlag::new()).unwrap();
    assert!(report.path.exists());
    assert_eq!(report.path.extension().unwrap(), "zst");

    let extracted = extract_tar_zst(&report.path);
    let expected = expected_map(&entries);
    assert_eq!(extracted.len(), expected.len(), "faltan o sobran entradas");
    for (name, want) in &expected {
        let got = extracted
            .get(name)
            .unwrap_or_else(|| panic!("falta «{name}»"));
        assert_eq!(got, want, "bytes distintos en «{name}»");
        assert_eq!(
            crc32fast::hash(got),
            crc32fast::hash(want),
            "CRC32 distinto en «{name}»"
        );
    }

    // El ZIP original nunca se modifica y no quedan temporales ni bloqueos.
    assert_eq!(file_bytes(&zip), original_bytes);
    assert!(
        listing(d.path())
            .iter()
            .all(|n| !n.contains("megacompress")),
        "quedaron temporales: {:?}",
        listing(d.path())
    );
}

#[test]
fn max_mode_records_zero_byte_files_without_corrupting_the_stream() {
    let d = tempfile::tempdir().unwrap();
    let zip = zip_in(d.path(), "vacios.zip");
    let entries = vec![
        ("a_vacio.txt".to_string(), Vec::new()),
        ("contenido.txt".to_string(), text(500, "x")),
        ("z_vacio.bin".to_string(), Vec::new()),
    ];
    make_zip(&zip, &entries, CompressionMethod::Deflated, false);

    let report = run_quiet(&Job::new(zip, Mode::Max), &CancelFlag::new()).unwrap();
    assert_eq!(report.path.extension().unwrap(), "7z");

    let extracted = extract_7z(&report.path);
    let expected = expected_map(&entries);
    assert_eq!(extracted, expected);
    assert_eq!(extracted.get("a_vacio.txt").map(Vec::len), Some(0));
    assert_eq!(extracted.get("z_vacio.bin").map(Vec::len), Some(0));
}
