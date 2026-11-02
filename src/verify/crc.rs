//! CRC32 de lectura y el manifiesto de lo ESPERADO (lo que declara el directorio central del ZIP
//! original: tamaño y CRC32 de cada entrada).
//!
//! # Identidad de las entradas
//! La identidad de una entrada es su ruta normalizada, y el lector ZIP RECHAZA los ZIP con rutas
//! duplicadas o que colisionan al normalizar (mayúsculas/minúsculas, `a//b` vs `a/b`). Por tanto
//! cada clave del manifiesto es única; un duplicado es un error, no una sobrescritura silenciosa.
//!
//! # Memoria
//! En vez de guardar las rutas completas, el manifiesto guarda un resumen de 128 bits (dos SipHash
//! 64 con claves aleatorias del proceso) → `u128 → (u64 tamaño, u32 crc)`. Coste ≈ 50 bytes por
//! entrada en el peor caso (con el límite por defecto de 1 000 000 de entradas, ≈ 50 MB). El
//! tamaño está acotado por `Limits::max_entries`. Efecto secundario: si falta una entrada, el
//! informe da el número de entradas que faltan, no sus nombres. Una colisión de 128 bits se
//! considera despreciable (~10⁻²⁷ con 10⁶ entradas).

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::BuildHasher;

#[derive(Debug)]
pub struct Expected {
    s1: RandomState,
    s2: RandomState,
    map: HashMap<u128, (u64, u32)>,
    total_files: u64,
    total_bytes: u64,
}

impl Default for Expected {
    fn default() -> Self {
        Self {
            s1: RandomState::new(),
            s2: RandomState::new(),
            map: HashMap::new(),
            total_files: 0,
            total_bytes: 0,
        }
    }
}

impl Expected {
    fn digest(&self, path: &str) -> u128 {
        (u128::from(self.s1.hash_one(path)) << 64) | u128::from(self.s2.hash_one(path))
    }

    /// Registra un archivo. Devuelve `false` si la ruta ya estaba (duplicado).
    pub fn insert(&mut self, path: &str, size: u64, crc: u32) -> bool {
        let k = self.digest(path);
        if self.map.insert(k, (size, crc)).is_some() {
            return false;
        }
        self.total_files += 1;
        self.total_bytes = self.total_bytes.saturating_add(size);
        true
    }

    /// Archivos registrados (no directorios).
    pub fn file_count(&self) -> u64 {
        self.total_files
    }

    /// Suma de tamaños esperados (base de la barra de verificación).
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Consume la entrada esperada de `path`. Un segundo `take` de la misma ruta da `None`, lo
    /// que detecta duplicados en el contenedor verificado.
    pub fn take(&mut self, path: &str) -> Option<(u64, u32)> {
        let k = self.digest(path);
        self.map.remove(&k)
    }

    /// Entradas esperadas que no han aparecido todavía.
    pub fn remaining(&self) -> usize {
        self.map.len()
    }
}

/// Acumulador CRC32 incremental.
pub struct Crc32 {
    h: crc32fast::Hasher,
    len: u64,
}

impl Crc32 {
    pub fn new() -> Self {
        Self {
            h: crc32fast::Hasher::new(),
            len: 0,
        }
    }
    pub fn update(&mut self, data: &[u8]) {
        self.h.update(data);
        self.len += data.len() as u64;
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn finalize(self) -> u32 {
        self.h.finalize()
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

/// Estado de la verificación para informar de progreso (fase 3).
#[derive(Debug, Clone, Copy, Default)]
pub struct VerifyProgress {
    pub entries_done: u64,
    pub entries_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// Compara lo recuperado de una entrada con lo esperado. Compartido por TAR y 7z.
pub fn compare_entry(
    expected: &mut Expected,
    name: &str,
    size: u64,
    crc: u32,
) -> std::result::Result<(), String> {
    match expected.take(name) {
        Some((exp_size, exp_crc)) => {
            if exp_size != size {
                Err(format!(
                    "«{name}»: tamaño recuperado {size} ≠ original {exp_size}"
                ))
            } else if exp_crc != crc {
                Err(format!(
                    "«{name}»: CRC32 recuperado {crc:08X} ≠ CRC32 del ZIP original {exp_crc:08X}"
                ))
            } else {
                Ok(())
            }
        }
        None => Err(format!(
            "entrada inesperada o duplicada en el contenedor: «{name}»"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_value() {
        let mut c = Crc32::new();
        c.update(b"123456789");
        assert_eq!(c.len(), 9);
        assert_eq!(c.finalize(), 0xCBF4_3926);
    }

    #[test]
    fn manifest_detects_duplicates_and_consumes_once() {
        let mut e = Expected::default();
        assert!(e.insert("a", 1, 10));
        assert!(!e.insert("a", 2, 20), "duplicado");
        assert!(e.insert("b", 5, 50));
        assert_eq!(e.file_count(), 2);
        assert_eq!(e.total_bytes(), 6);
        assert_eq!(e.remaining(), 2);
        assert_eq!(e.take("b"), Some((5, 50)));
        assert_eq!(
            e.take("b"),
            None,
            "segunda aparición = duplicado en el contenedor"
        );
        assert_eq!(e.take("zzz"), None);
        assert_eq!(e.remaining(), 1);
    }

    #[test]
    fn compare_entry_reports_each_kind_of_mismatch() {
        let mut e = Expected::default();
        e.insert("ok", 3, 0xAB);
        e.insert("size", 3, 0xAB);
        e.insert("crc", 3, 0xAB);
        assert!(compare_entry(&mut e, "ok", 3, 0xAB).is_ok());
        assert!(compare_entry(&mut e, "size", 4, 0xAB)
            .unwrap_err()
            .contains("tamaño"));
        assert!(compare_entry(&mut e, "crc", 3, 0xAC)
            .unwrap_err()
            .contains("CRC32"));
        assert!(compare_entry(&mut e, "otra", 1, 1)
            .unwrap_err()
            .contains("inesperada"));
    }
}
