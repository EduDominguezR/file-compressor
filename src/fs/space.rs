//! Estimación de espacio. Distingue tres cosas que NO deben confundirse:
//!
//! 1. **Espacio de trabajo** (`work_upper`): lo máximo que podría ocupar el temporal durante la
//!    operación. La salida se escribe en un único temporal que luego se renombra, por lo que el
//!    espacio de trabajo y el tamaño final coinciden; este valor es una COTA SUPERIOR teórica
//!    (datos incompresibles + cabeceras del contenedor), no una predicción.
//! 2. **Tamaño final estimado** (`likely`): heurística (≈ tamaño del ZIP + cabeceras). Puede ser
//!    bastante menor o mayor; el tamaño final no se puede garantizar.
//! 3. **Espacio disponible real** (`available`): lo que informa el sistema para el destino.
//!
//! Política (documentada en AUDIT.md):
//! * `available < floor`  → error `InsufficientSpace` (ni el mínimo plausible cabe).
//! * `floor ≤ available < work_upper` → AVISO y se continúa (no se rechazan archivos legítimos:
//!   lo normal es que la salida sea mucho menor que la cota superior). Si el disco se llena a
//!   mitad, el error `DiskFull` se gestiona y no queda nada a medias.
//! * `available ≥ work_upper` → sin avisos.

use std::path::Path;

use sysinfo::Disks;

use crate::engine::mode::Mode;
use crate::engine::reader_zip::ZipInfo;
use crate::error::{Error, Result};
use crate::util::human_size;

const MARGIN: u64 = 64 * 1024 * 1024;

/// Sobrecoste por entrada de los contenedores (TAR: cabecera 512 + relleno ≤ 512; 7z: metadatos).
fn per_entry_overhead(mode: Mode) -> u64 {
    match mode {
        Mode::Fast => 1024,
        Mode::Max => 128,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    /// Se puede continuar, pero el espacio podría no bastar en el peor caso.
    Warn(String),
    Insufficient,
}

#[derive(Debug, Clone)]
pub struct SpacePlan {
    pub zip_size: u64,
    pub uncompressed: u64,
    pub entries: u64,
    pub mode: Mode,
    /// Cota superior del espacio de trabajo (= tamaño máximo posible del temporal/final).
    pub work_upper: u64,
    /// Estimación heurística del tamaño final.
    pub likely: u64,
    /// Mínimo por debajo del cual es inviable intentar.
    pub floor: u64,
    /// `None` si no se pudo determinar.
    pub available: Option<u64>,
    pub verdict: Verdict,
}

pub fn plan(info: &ZipInfo, mode: Mode, available: Option<u64>) -> SpacePlan {
    let overhead = info.entries.saturating_mul(per_entry_overhead(mode));
    // Peor caso: datos incompresibles. zstd/LZMA2 añaden ~1 % como mucho al almacenar bloques.
    let expansion = info.total_bytes / 100;
    let work_upper = info
        .total_bytes
        .saturating_add(expansion)
        .saturating_add(overhead)
        .saturating_add(MARGIN);
    let likely = info.zip_size.saturating_add(overhead);
    let floor = (info.zip_size / 2)
        .saturating_add(overhead)
        .saturating_add(MARGIN)
        .min(work_upper);

    let verdict = match available {
        None => Verdict::Warn(
            "No se pudo determinar el espacio libre del destino; se continuará y, si el disco se llena, la operación se detendrá sin dejar archivos incompletos."
                .into(),
        ),
        Some(a) if a < floor => Verdict::Insufficient,
        Some(a) if a < work_upper => Verdict::Warn(format!(
            "El espacio libre ({}) es menor que el peor caso teórico ({}). Lo normal es que el resultado ocupe bastante menos, pero no puede garantizarse. Si el disco se llena, la operación se detiene y el original queda intacto.",
            human_size(a),
            human_size(work_upper)
        )),
        Some(_) => Verdict::Ok,
    };

    SpacePlan {
        zip_size: info.zip_size,
        uncompressed: info.total_bytes,
        entries: info.entries,
        mode,
        work_upper,
        likely,
        floor,
        available,
        verdict,
    }
}

impl SpacePlan {
    /// `Err(InsufficientSpace)` solo si el veredicto es `Insufficient`.
    pub fn ensure_feasible(&self) -> Result<()> {
        if self.verdict == Verdict::Insufficient {
            Err(Error::InsufficientSpace {
                needed: human_size(self.floor),
                available: human_size(self.available.unwrap_or(0)),
            })
        } else {
            Ok(())
        }
    }

    pub fn warning(&self) -> Option<&str> {
        match &self.verdict {
            Verdict::Warn(m) => Some(m),
            _ => None,
        }
    }
}

/// Espacio libre del disco que contiene `dir`. `None` si no se pudo determinar.
/// (Operación potencialmente lenta: no llamar desde el hilo de la interfaz.)
pub fn free_space(dir: &Path) -> Option<u64> {
    let canon = dir.canonicalize().ok()?;
    let mut s = canon.to_string_lossy().into_owned();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        s = stripped.to_string(); // prefijo verbatim de Windows
    }
    let norm = |x: &str| {
        if cfg!(windows) {
            x.to_lowercase()
        } else {
            x.to_string()
        }
    };
    let target = norm(&s);

    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter_map(|d| {
            let mp = norm(&d.mount_point().to_string_lossy());
            target
                .starts_with(&mp)
                .then(|| (mp.len(), d.available_space()))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, avail)| avail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(zip: u64, total: u64, entries: u64) -> ZipInfo {
        ZipInfo {
            zip_size: zip,
            entries,
            files: entries,
            total_bytes: total,
            precompressed_bytes: 0,
            symlinks: 0,
            compressed_total: zip,
        }
    }

    #[test]
    fn uses_uncompressed_size_for_upper_bound_not_zip_size() {
        let p = plan(
            &info(1_000_000, 50_000_000_000, 10),
            Mode::Fast,
            Some(u64::MAX),
        );
        assert!(p.work_upper > 50_000_000_000);
        assert!(p.likely < 2_000_000);
        assert_eq!(p.verdict, Verdict::Ok);
    }

    #[test]
    fn warns_instead_of_rejecting_when_between_floor_and_upper() {
        let i = info(10_000_000_000, 40_000_000_000, 100);
        let p = plan(&i, Mode::Max, Some(20_000_000_000));
        assert!(matches!(p.verdict, Verdict::Warn(_)), "{:?}", p.verdict);
        assert!(p.ensure_feasible().is_ok());
        assert!(p.warning().is_some());
    }

    #[test]
    fn rejects_when_below_plausible_floor() {
        let i = info(10_000_000_000, 40_000_000_000, 100);
        let p = plan(&i, Mode::Max, Some(1_000_000_000));
        assert_eq!(p.verdict, Verdict::Insufficient);
        assert!(matches!(
            p.ensure_feasible(),
            Err(Error::InsufficientSpace { .. })
        ));
    }

    #[test]
    fn unknown_space_warns_but_does_not_block() {
        let p = plan(&info(1_000, 2_000, 1), Mode::Fast, None);
        assert!(matches!(p.verdict, Verdict::Warn(_)));
        assert!(p.ensure_feasible().is_ok());
    }

    #[test]
    fn tar_overhead_is_larger_than_7z_per_entry() {
        let i = info(1_000, 1_000, 1_000_000);
        assert!(
            plan(&i, Mode::Fast, Some(u64::MAX)).work_upper
                > plan(&i, Mode::Max, Some(u64::MAX)).work_upper
        );
    }
}
