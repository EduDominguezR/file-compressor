mod common;
use common::*;
use megacompress::engine::mode::Mode;
use megacompress::error::Error;
use zip::CompressionMethod;

fn only_zip_remains(dir: &std::path::Path) {
    assert_eq!(
        listing(dir),
        vec!["x.zip".to_string()],
        "no debe quedar salida parcial ni temporales"
    );
}

#[test]
fn not_a_zip_is_rejected() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    std::fs::write(&z, b"esto no es un zip").unwrap();
    for mode in [Mode::Fast, Mode::Max] {
        assert!(matches!(run_mode(&z, mode), Err(Error::NotZip)));
    }
    only_zip_remains(d.path());
}

#[test]
fn truncated_zip_is_corrupt() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    make_zip(&z, &three_texts(), CompressionMethod::Deflated, false);
    let bytes = file_bytes(&z);
    std::fs::write(&z, &bytes[..bytes.len() - 30]).unwrap();
    for mode in [Mode::Fast, Mode::Max] {
        let e = run_mode(&z, mode).unwrap_err();
        assert!(matches!(e, Error::Corrupt(_) | Error::Io(_)), "{e:?}");
    }
    only_zip_remains(d.path());
}

#[test]
fn flipped_data_byte_fails_crc_and_leaves_no_output() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    let marker = vec![b'Q'; 4096];
    make_zip(
        &z,
        &[("q.bin".to_string(), marker)],
        CompressionMethod::Stored,
        false,
    );
    let mut bytes = file_bytes(&z);
    let pos = bytes.windows(8).position(|w| w == [b'Q'; 8]).unwrap() + 100;
    bytes[pos] = b'R';
    std::fs::write(&z, bytes).unwrap();
    for mode in [Mode::Fast, Mode::Max] {
        let e = run_mode(&z, mode).unwrap_err();
        assert!(matches!(e, Error::Corrupt(_)), "{mode:?}: {e:?}");
    }
    only_zip_remains(d.path());
}

#[test]
fn unsupported_method_is_rejected_cleanly() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    make_zip(&z, &three_texts(), CompressionMethod::Stored, false);
    patch_method(&z, 14); // LZMA
    for mode in [Mode::Fast, Mode::Max] {
        let e = run_mode(&z, mode).unwrap_err();
        assert!(matches!(e, Error::UnsupportedMethod { .. }), "{e:?}");
        assert!(e.to_string().contains("LZMA"), "{e}");
    }
    only_zip_remains(d.path());
}

#[test]
fn parent_dir_path_is_rejected() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    make_zip(
        &z,
        &[("xx/evil.txt".to_string(), b"mal".to_vec())],
        CompressionMethod::Stored,
        false,
    );
    patch_bytes(&z, b"xx/evil.txt", b"../evil.txt");
    for mode in [Mode::Fast, Mode::Max] {
        let e = run_mode(&z, mode).unwrap_err();
        assert!(matches!(e, Error::UnsafePath(_)), "{e:?}");
    }
    only_zip_remains(d.path());
}

#[test]
fn encrypted_flag_is_rejected() {
    let d = tempfile::tempdir().unwrap();
    let z = zip_in(d.path(), "x.zip");
    make_zip(&z, &three_texts(), CompressionMethod::Stored, false);
    // Activa el bit 0 (cifrado) de los "general purpose flags" en cabeceras locales y centrales.
    let mut b = file_bytes(&z);
    let mut i = 0;
    while i + 12 <= b.len() {
        if b[i..i + 4] == [0x50, 0x4B, 0x03, 0x04] {
            b[i + 6] |= 1;
        } else if b[i..i + 4] == [0x50, 0x4B, 0x01, 0x02] {
            b[i + 8] |= 1;
        }
        i += 1;
    }
    std::fs::write(&z, b).unwrap();
    let e = run_mode(&z, Mode::Fast).unwrap_err();
    assert!(matches!(e, Error::Encrypted(_)), "{e:?}");
    only_zip_remains(d.path());
}
