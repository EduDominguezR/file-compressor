//! Integridad: los verificadores comparan el CRC32 REAL del contenido recuperado contra el del
//! ZIP original. Se fabrican contenedores correctos y alterados a mano.
use std::fs::File;
use std::path::Path;

use megacompress::engine::cancel::CancelFlag;
use megacompress::error::Error;
use megacompress::verify::archive_check::verify_7z_full;
use megacompress::verify::crc::Expected;
use megacompress::verify::zst_check::verify_tar_zst;

type Entries<'a> = Vec<(&'a str, Vec<u8>)>;

fn expected(entries: &Entries) -> Expected {
    let mut e = Expected::default();
    for (n, d) in entries {
        assert!(e.insert(n, d.len() as u64, crc32fast::hash(d)));
    }
    e
}

fn base() -> Entries<'static> {
    vec![
        ("a.txt", b"hola mundo".to_vec()),
        ("d/b.bin", (0..=255u8).cycle().take(5000).collect()),
    ]
}

fn make_tar_zst(path: &Path, entries: &Entries) {
    let enc = zstd::Encoder::new(File::create(path).unwrap(), 3).unwrap();
    let mut b = tar::Builder::new(enc);
    for (n, d) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_size(d.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, n, &d[..]).unwrap();
    }
    b.into_inner().unwrap().finish().unwrap();
}

fn make_7z(path: &Path, entries: &Entries) {
    let mut w = sevenz_rust2::ArchiveWriter::new(File::create(path).unwrap()).unwrap();
    for (n, d) in entries {
        w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file(n), Some(&d[..]))
            .unwrap();
    }
    w.finish().unwrap();
}

fn check_tar(path: &Path, exp: &mut Expected) -> megacompress::error::Result<()> {
    verify_tar_zst(path, exp, &CancelFlag::new(), |_| {})
}
fn check_7z(path: &Path, exp: &mut Expected) -> megacompress::error::Result<()> {
    verify_7z_full(path, exp, &CancelFlag::new(), |_| {})
}

fn is_verify_err(r: megacompress::error::Result<()>) -> String {
    match r {
        Err(Error::Verify(m)) => m,
        other => panic!("se esperaba Error::Verify, hay {other:?}"),
    }
}

// ── tar.zst ─────────────────────────────────────────────────────────────────────────────────
#[test]
fn tar_ok() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("o.tar.zst");
    make_tar_zst(&p, &base());
    check_tar(&p, &mut expected(&base())).unwrap();
}

#[test]
fn tar_altered_content_same_size_is_detected_by_crc() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("o.tar.zst");
    let mut bad = base();
    bad[0].1 = b"HOLA MUNDO".to_vec(); // mismo tamaño, otro contenido
    make_tar_zst(&p, &bad);
    assert!(is_verify_err(check_tar(&p, &mut expected(&base()))).contains("CRC32"));
}

#[test]
fn tar_different_size_missing_unexpected_and_duplicate() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("o.tar.zst");

    let mut sz = base();
    sz[0].1.push(b'!');
    make_tar_zst(&p, &sz);
    assert!(is_verify_err(check_tar(&p, &mut expected(&base()))).contains("tamaño"));

    let mut missing = base();
    missing.pop();
    make_tar_zst(&p, &missing);
    assert!(is_verify_err(check_tar(&p, &mut expected(&base()))).contains("faltan"));

    let mut extra = base();
    extra.push(("sobra.txt", b"x".to_vec()));
    make_tar_zst(&p, &extra);
    assert!(is_verify_err(check_tar(&p, &mut expected(&base()))).contains("inesperada"));

    let mut dup = base();
    dup.push(("a.txt", b"hola mundo".to_vec()));
    make_tar_zst(&p, &dup);
    assert!(is_verify_err(check_tar(&p, &mut expected(&base()))).contains("duplicada"));
}

#[test]
fn tar_truncated_container_is_detected() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("o.tar.zst");
    let big: Entries = vec![(
        "big.bin",
        (0..200_000u32).flat_map(|i| i.to_le_bytes()).collect(),
    )];
    make_tar_zst(&p, &big);
    let bytes = std::fs::read(&p).unwrap();
    std::fs::write(&p, &bytes[..bytes.len() / 2]).unwrap();
    is_verify_err(check_tar(&p, &mut expected(&big)));
}

// ── 7z ──────────────────────────────────────────────────────────────────────────────────────
#[test]
fn sevenz_ok() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("o.7z");
    make_7z(&p, &base());
    check_7z(&p, &mut expected(&base())).unwrap();
}

#[test]
fn sevenz_altered_content_missing_and_unexpected() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("o.7z");

    let mut bad = base();
    bad[1].1[10] ^= 0xFF; // mismo tamaño, un byte distinto
    make_7z(&p, &bad);
    assert!(is_verify_err(check_7z(&p, &mut expected(&base()))).contains("CRC32"));

    let mut missing = base();
    missing.pop();
    make_7z(&p, &missing);
    assert!(is_verify_err(check_7z(&p, &mut expected(&base()))).contains("faltan"));

    let mut extra = base();
    extra.push(("sobra.txt", b"x".to_vec()));
    make_7z(&p, &extra);
    assert!(is_verify_err(check_7z(&p, &mut expected(&base()))).contains("inesperada"));
}

#[test]
fn sevenz_different_size_is_detected() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("o.7z");
    let mut sz = base();
    sz[0].1.push(b'!');
    make_7z(&p, &sz);
    assert!(is_verify_err(check_7z(&p, &mut expected(&base()))).contains("tamaño"));
}

#[test]
fn sevenz_truncated_container_is_detected() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("o.7z");
    make_7z(&p, &base());
    let bytes = std::fs::read(&p).unwrap();
    std::fs::write(&p, &bytes[..bytes.len() - 20]).unwrap(); // pierde la cabecera final
    is_verify_err(check_7z(&p, &mut expected(&base())));
}
