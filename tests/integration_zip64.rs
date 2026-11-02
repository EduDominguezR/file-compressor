//! Zip64: estas pruebas fuerzan cabeceras/extras Zip64 en entradas pequeñas (`large_file(true)`),
//! lo que ejercita la ruta de lectura Zip64 del crate `zip`. NO prueban entradas > 4 GiB reales
//! (esa prueba es manual; ver README).
mod common;
use common::*;
use megacompress::engine::mode::Mode;
use zip::CompressionMethod;

#[test]
fn zip64_headers_both_modes() {
    let entries = three_texts();
    for mode in [Mode::Fast, Mode::Max] {
        let d = tempfile::tempdir().unwrap();
        let z = zip_in(d.path(), "z64.zip");
        make_zip(&z, &entries, CompressionMethod::Deflated, true);
        let rep = run_mode(&z, mode).unwrap();
        assert_eq!(extract(&rep.path, mode), expected_map(&entries));
    }
}
