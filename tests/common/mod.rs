#![allow(dead_code)]
//! Utilidades de fixtures. Todo se genera en directorios temporales.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use megacompress::engine::cancel::CancelFlag;
use megacompress::engine::mode::Mode;
use megacompress::engine::orchestrator::{run_quiet, Job, Report};
use megacompress::error::Result;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Bytes pseudoaleatorios deterministas (incompresibles; sirven de "imagen" sintética).
pub fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

pub fn text(n: usize, tag: &str) -> Vec<u8> {
    (0..n)
        .map(|i| format!("{tag} línea {i}: lorem ipsum dolor sit amet\n"))
        .collect::<String>()
        .into_bytes()
}

pub fn make_zip(
    path: &Path,
    entries: &[(String, Vec<u8>)],
    method: CompressionMethod,
    large: bool,
) {
    let mut w = ZipWriter::new(File::create(path).unwrap());
    let opts = SimpleFileOptions::default()
        .compression_method(method)
        .large_file(large);
    for (name, data) in entries {
        w.start_file(name.as_str(), opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap();
}

pub fn three_texts() -> Vec<(String, Vec<u8>)> {
    vec![
        ("a.txt".into(), text(300, "A")),
        ("carpeta/b.txt".into(), text(500, "B")),
        ("carpeta/sub/c.txt".into(), text(100, "C")),
    ]
}

pub fn run_mode(zip: &Path, mode: Mode) -> Result<Report> {
    run_quiet(&Job::new(zip.to_path_buf(), mode), &CancelFlag::new())
}

pub fn extract_tar_zst(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let dec = zstd::stream::read::Decoder::new(File::open(path).unwrap()).unwrap();
    let mut ar = tar::Archive::new(dec);
    let mut out = BTreeMap::new();
    for e in ar.entries().unwrap() {
        let mut e = e.unwrap();
        if e.header().entry_type().is_dir() {
            continue;
        }
        let name = e.path().unwrap().to_string_lossy().into_owned();
        let mut v = Vec::new();
        e.read_to_end(&mut v).unwrap();
        out.insert(name, v);
    }
    out
}

pub fn extract_7z(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut r = sevenz_rust2::ArchiveReader::new(
        File::open(path).unwrap(),
        sevenz_rust2::Password::empty(),
    )
    .unwrap();
    let mut out = BTreeMap::new();
    r.for_each_entries(|entry, reader| {
        if !entry.is_directory() {
            let mut v = Vec::new();
            reader.read_to_end(&mut v).expect("lectura 7z");
            out.insert(entry.name().to_string(), v);
        }
        Ok(true)
    })
    .unwrap();
    out
}

pub fn extract(path: &Path, mode: Mode) -> BTreeMap<String, Vec<u8>> {
    match mode {
        Mode::Fast => extract_tar_zst(path),
        Mode::Max => extract_7z(path),
    }
}

pub fn expected_map(entries: &[(String, Vec<u8>)]) -> BTreeMap<String, Vec<u8>> {
    entries.iter().cloned().collect()
}

pub fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

pub fn file_bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

/// Reemplaza todas las apariciones de `from` por `to` (misma longitud) en un archivo.
pub fn patch_bytes(path: &Path, from: &[u8], to: &[u8]) {
    assert_eq!(from.len(), to.len());
    let mut data = std::fs::read(path).unwrap();
    let mut i = 0;
    let mut n = 0;
    while i + from.len() <= data.len() {
        if &data[i..i + from.len()] == from {
            data[i..i + from.len()].copy_from_slice(to);
            i += from.len();
            n += 1;
        } else {
            i += 1;
        }
    }
    assert!(n > 0, "no se encontró el patrón a parchear");
    std::fs::write(path, data).unwrap();
}

/// Cambia el campo "método de compresión" de las cabeceras locales y del directorio central.
pub fn patch_method(path: &Path, new_method: u16) {
    let mut d = std::fs::read(path).unwrap();
    let m = new_method.to_le_bytes();
    let mut i = 0;
    while i + 4 <= d.len() {
        if d[i..i + 4] == [0x50, 0x4B, 0x03, 0x04] && i + 10 <= d.len() {
            d[i + 8] = m[0];
            d[i + 9] = m[1];
        } else if d[i..i + 4] == [0x50, 0x4B, 0x01, 0x02] && i + 12 <= d.len() {
            d[i + 10] = m[0];
            d[i + 11] = m[1];
        }
        i += 1;
    }
    std::fs::write(path, d).unwrap();
}

pub fn zip_in(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}
