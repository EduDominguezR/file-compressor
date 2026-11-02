//! Protección contra Zip Slip y nombres inválidos: normaliza y valida rutas de entradas antes de
//! escribirlas en el TAR/7z de salida. Reglas de Windows aplicadas en TODAS las plataformas (el
//! archivo de salida puede extraerse en Windows).
//!
//! Rechaza: rutas absolutas (`/x`, `\x`), UNC (`\\srv\x`, `//srv/x`), unidades (`C:\x`), `..`,
//! bytes NUL y caracteres de control, `:` en cualquier componente (unidades y Alternate Data
//! Streams), `< > " | ? *`, componentes que terminan en punto o espacio, y nombres reservados de
//! Windows (CON, PRN, AUX, NUL, COM1–9, LPT1–9, CONIN$, CONOUT$, COM¹²³, LPT¹²³), con o sin
//! extensión (`CON.txt`).
//!
//! Las colisiones tras normalizar (`a//b` vs `a/b`, `A.txt` vs `a.txt`) las detecta
//! `fold_key` + el detector de duplicados del lector ZIP.

use unicode_normalization::UnicodeNormalization;

use crate::error::{Error, Result};

const RESERVED: [&str; 28] = [
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "CONIN$",
    "CONOUT$",
    "COM\u{b9}",
    "COM\u{b2}",
    "COM\u{b3}",
    "LPT\u{b9}",
];
const RESERVED_EXTRA: [&str; 2] = ["LPT\u{b2}", "LPT\u{b3}"];
const INVALID_CHARS: [char; 6] = ['<', '>', '"', '|', '?', '*'];

/// Devuelve la ruta normalizada con `/` y sin `.`/vacíos/barra final.
/// Devuelve `Ok("")` si la ruta no contiene componentes (p. ej. `./`): el llamador la omite.
pub fn sanitize_entry_path(raw: &str) -> Result<String> {
    let bad = || Error::UnsafePath(raw.to_string());

    if raw.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(bad());
    }
    let s = raw.replace('\\', "/");
    if s.starts_with('/') {
        return Err(bad()); // absoluta y UNC
    }

    let mut parts: Vec<String> = Vec::new();
    for comp in s.split('/') {
        match comp {
            "" | "." => continue,
            ".." => return Err(bad()),
            c => {
                // NFC: dos formas Unicode equivalentes deben producir el mismo nombre en el
                // contenedor (y la misma clave de colisión). La validación se hace DESPUÉS de
                // normalizar, sobre el nombre que realmente se escribirá.
                let nfc: String = c.nfc().collect();
                if nfc.contains(':') || nfc.contains(INVALID_CHARS) {
                    return Err(bad()); // unidad, ADS o carácter no válido
                }
                if nfc.ends_with('.') || nfc.ends_with(' ') {
                    return Err(bad());
                }
                if is_reserved(&nfc) {
                    return Err(bad());
                }
                parts.push(nfc);
            }
        }
    }
    Ok(parts.join("/"))
}

fn is_reserved(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or("").trim_end();
    let up = stem.to_uppercase();
    RESERVED.contains(&up.as_str()) || RESERVED_EXTRA.contains(&up.as_str())
}

/// Clave de comparación para detectar colisiones en sistemas de archivos que no distinguen
/// mayúsculas (Windows, macOS por defecto).
pub fn fold_key(normalized: &str) -> String {
    normalized.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_paths() {
        assert_eq!(sanitize_entry_path("a/b/c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(sanitize_entry_path("dir/").unwrap(), "dir");
        assert_eq!(sanitize_entry_path("./a//b/./c").unwrap(), "a/b/c");
        assert_eq!(sanitize_entry_path("a\\b\\c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(sanitize_entry_path("./").unwrap(), "");
        assert_eq!(
            sanitize_entry_path("console/ok.txt").unwrap(),
            "console/ok.txt"
        );
        assert_eq!(sanitize_entry_path("a/.oculto").unwrap(), "a/.oculto");
        assert_eq!(
            sanitize_entry_path("ñandú/café.txt").unwrap(),
            "ñandú/café.txt"
        );
    }

    #[test]
    fn rejects_parent_dirs() {
        for p in ["../x", "a/../../x", "a/..", "a\\..\\x"] {
            assert!(sanitize_entry_path(p).is_err(), "{p}");
        }
    }

    #[test]
    fn rejects_absolute_unc_and_drives() {
        for p in [
            "/etc/passwd",
            "\\windows\\x",
            "C:\\x",
            "c:x",
            "//server/share/x",
            "\\\\srv\\share\\x",
        ] {
            assert!(sanitize_entry_path(p).is_err(), "{p}");
        }
    }

    #[test]
    fn rejects_alternate_data_streams() {
        for p in ["a.txt:evil", "dir/a.txt:Zone.Identifier", "a/b:c/d"] {
            assert!(sanitize_entry_path(p).is_err(), "{p}");
        }
    }

    #[test]
    fn rejects_trailing_dot_or_space() {
        for p in ["abc.", "dir./x", "abc ", "a/b /c"] {
            assert!(sanitize_entry_path(p).is_err(), "{p:?}");
        }
    }

    #[test]
    fn rejects_invalid_characters_and_controls() {
        for p in [
            "a<b", "a>b", "a\"b", "a|b", "a?b", "a*b", "a\0b", "a\nb", "a\u{7}b",
        ] {
            assert!(sanitize_entry_path(p).is_err(), "{p:?}");
        }
    }

    #[test]
    fn rejects_windows_reserved() {
        for n in [
            "CON",
            "con",
            "NUL.txt",
            "a/PRN",
            "x/aux.log",
            "COM1",
            "LPT9.tar.gz",
            "CON.txt",
            "Com5.x",
            "CONIN$",
            "COM\u{b9}",
        ] {
            assert!(sanitize_entry_path(n).is_err(), "{n}");
        }
        assert!(sanitize_entry_path("COM10").is_ok());
        assert!(sanitize_entry_path("CONSOLE").is_ok());
        assert!(sanitize_entry_path("xCON").is_ok());
    }

    #[test]
    fn unicode_names_are_normalized_to_nfc() {
        let decomposed = "cafe\u{301}/\u{fb01}chero.txt"; // e + acento combinante, ﬁ ligadura
        let composed = "caf\u{e9}/\u{fb01}chero.txt";
        assert_eq!(sanitize_entry_path(decomposed).unwrap(), composed);
        assert_eq!(sanitize_entry_path(composed).unwrap(), composed);
        assert_eq!(
            fold_key(&sanitize_entry_path(decomposed).unwrap()),
            fold_key(composed)
        );
    }

    #[test]
    fn normalization_collisions_share_key() {
        let a = sanitize_entry_path("a//b/./C.txt").unwrap();
        let b = sanitize_entry_path("a/b/c.TXT").unwrap();
        assert_ne!(a, b);
        assert_eq!(fold_key(&a), fold_key(&b));
        assert_eq!(
            sanitize_entry_path("a/b/./x").unwrap(),
            sanitize_entry_path("a//b/x").unwrap()
        );
    }
}
