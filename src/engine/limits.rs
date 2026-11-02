//! Límites de seguridad (anti ZIP-bomb), centralizados. Se pueden ajustar sin tocar el código:
//! variables de entorno `MEGACOMPRESS_MAX_UNCOMPRESSED_GB`, `MEGACOMPRESS_MAX_RATIO`,
//! `MEGACOMPRESS_MAX_ENTRIES`, `MEGACOMPRESS_RATIO_MIN_MB`.

use crate::error::{Error, Result};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Suma máxima de tamaños descomprimidos declarados.
    pub max_total_uncompressed: u64,
    /// Relación máxima descomprimido:comprimido por entrada.
    pub max_ratio: u64,
    /// Las entradas más pequeñas que esto no se someten a la relación (no pueden ser una bomba
    /// por sí solas; evita falsos positivos con ficheros pequeños muy repetitivos).
    pub ratio_min_size: u64,
    pub max_entries: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_total_uncompressed: 100 * GIB,
            max_ratio: 1000,
            ratio_min_size: MIB,
            max_entries: 1_000_000,
        }
    }
}

impl Limits {
    /// Valores por defecto con las sustituciones de entorno que sean válidas.
    pub fn from_env() -> Self {
        let mut l = Self::default();
        let get = |k: &str| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        if let Some(gb) = get("MEGACOMPRESS_MAX_UNCOMPRESSED_GB") {
            l.max_total_uncompressed = gb.saturating_mul(GIB);
        }
        if let Some(r) = get("MEGACOMPRESS_MAX_RATIO") {
            l.max_ratio = r.max(1);
        }
        if let Some(n) = get("MEGACOMPRESS_MAX_ENTRIES") {
            l.max_entries = n;
        }
        if let Some(mb) = get("MEGACOMPRESS_RATIO_MIN_MB") {
            l.ratio_min_size = mb.saturating_mul(MIB);
        }
        l
    }

    /// Comprueba una entrada. `compressed` = tamaño comprimido declarado.
    pub fn check_entry(&self, name: &str, size: u64, compressed: u64, zip_size: u64) -> Result<()> {
        if compressed > zip_size {
            return Err(Error::Corrupt(format!(
                "«{name}» declara {compressed} bytes comprimidos, más que el propio ZIP ({zip_size}): tamaños inconsistentes"
            )));
        }
        if size >= self.ratio_min_size {
            // compressed == 0 con size > 0 equivale a relación infinita.
            let exceeds = compressed == 0 || size / compressed > self.max_ratio;
            if exceeds {
                return Err(Error::LimitExceeded(format!(
                    "«{name}» se expande más de {}:1 ({} bytes comprimidos → {} descomprimidos). Posible ZIP bomb.",
                    self.max_ratio, compressed, size
                )));
            }
        }
        Ok(())
    }

    pub fn check_entry_count(&self, count: u64) -> Result<()> {
        if count > self.max_entries {
            Err(Error::LimitExceeded(format!(
                "el ZIP tiene {count} entradas y el máximo permitido es {}",
                self.max_entries
            )))
        } else {
            Ok(())
        }
    }

    /// Suma con comprobación de desbordamiento y del tope total.
    pub fn add_total(&self, total: u64, size: u64) -> Result<u64> {
        let new = total.checked_add(size).ok_or_else(|| {
            Error::LimitExceeded("el tamaño total descomprimido desborda u64".into())
        })?;
        if new > self.max_total_uncompressed {
            return Err(Error::LimitExceeded(format!(
                "el tamaño total descomprimido supera {} bytes",
                self.max_total_uncompressed
            )));
        }
        Ok(new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let l = Limits::default();
        assert_eq!(l.max_total_uncompressed, 100 * GIB);
        assert_eq!(l.max_ratio, 1000);
        assert_eq!(l.max_entries, 1_000_000);
    }

    #[test]
    fn ratio_is_enforced_only_for_big_entries() {
        let l = Limits::default();
        assert!(l.check_entry("peq", 500_000, 10, 1 << 20).is_ok()); // < 1 MiB: no aplica
        assert!(l
            .check_entry("grande", 100 * MIB, 50 * 1024, 1 << 30)
            .is_err()); // ~2048:1
        assert!(l.check_entry("ok", 100 * MIB, 50 * MIB, 1 << 30).is_ok());
        assert!(l.check_entry("cero", 10 * MIB, 0, 1 << 30).is_err());
    }

    #[test]
    fn inconsistent_sizes_rejected() {
        let l = Limits::default();
        assert!(matches!(
            l.check_entry("x", 10, 5000, 100),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn totals_overflow_and_cap() {
        let l = Limits {
            max_total_uncompressed: 100,
            ..Limits::default()
        };
        assert_eq!(l.add_total(10, 20).unwrap(), 30);
        assert!(l.add_total(90, 20).is_err());
        assert!(Limits::default().add_total(u64::MAX, 1).is_err());
    }

    #[test]
    fn entry_count() {
        let l = Limits {
            max_entries: 3,
            ..Limits::default()
        };
        assert!(l.check_entry_count(3).is_ok());
        assert!(l.check_entry_count(4).is_err());
    }
}
