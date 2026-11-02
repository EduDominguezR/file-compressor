//! Errores del programa. Los `Display` están escritos para el usuario (español, sin códigos
//! técnicos). El detalle técnico va al log.
//!
//! Los errores de E/S se clasifican de forma centralizada en `From<io::Error>`:
//! disco lleno y permisos se distinguen por CÓDIGO de error del sistema (no por el texto).

use std::io;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Operación cancelada.")]
    Cancelled,

    #[error("El archivo no es un ZIP válido (no empieza con la cabecera de un ZIP).")]
    NotZip,

    #[error(
        "El ZIP está dividido en varios volúmenes. Únelos en un solo ZIP e inténtalo de nuevo."
    )]
    Multivolume,

    #[error("El ZIP está cifrado (entrada «{0}»). MegaCompress no puede reempaquetar archivos cifrados.")]
    Encrypted(String),

    #[error("El ZIP usa un método de compresión que no se puede procesar ({method}) en «{entry}». No se ha creado ningún archivo.")]
    UnsupportedMethod { entry: String, method: String },

    #[error("El ZIP está dañado: {0}")]
    Corrupt(String),

    #[error("El ZIP contiene una ruta peligrosa: «{0}». Por seguridad no se continúa.")]
    UnsafePath(String),

    #[error("El ZIP contiene entradas duplicadas o que colisionan al normalizar el nombre: «{0}». Al extraerlas una sobrescribiría a otra, por eso no se continúa.")]
    DuplicateEntry(String),

    #[error("El ZIP supera un límite de seguridad: {0}")]
    LimitExceeded(String),

    #[error("No hay espacio suficiente en el disco de destino: se necesitan al menos {needed} y hay {available} libres.")]
    InsufficientSpace { needed: String, available: String },

    #[error("No hay suficiente espacio disponible en el disco para completar la operación. El archivo original permanece intacto.")]
    DiskFull,

    #[error("Permiso denegado al acceder a un archivo o carpeta ({0}). El archivo original permanece intacto.")]
    PermissionDenied(String),

    #[error("Error de lectura o escritura: {0}")]
    Io(io::Error),

    #[error("No se pudo escribir el archivo de salida: {0}")]
    Output(String),

    #[error("La verificación del archivo resultante falló: {0}")]
    Verify(String),

    #[error("Error interno: {0}")]
    Internal(String),
}

/// ¿El error de E/S significa "no queda espacio"? Se basa en códigos del sistema:
/// `ErrorKind::StorageFull`, `ENOSPC` (28 en Linux/macOS) y `ERROR_DISK_FULL` (112) /
/// `ERROR_HANDLE_DISK_FULL` (39) en Windows. Un error de permisos o de dispositivo NO cuenta.
pub fn is_disk_full(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::StorageFull {
        return true;
    }
    match e.raw_os_error() {
        #[cfg(unix)]
        Some(28) => true,
        #[cfg(windows)]
        Some(112) | Some(39) => true,
        _ => false,
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        if is_disk_full(&e) {
            Error::DiskFull
        } else if e.kind() == io::ErrorKind::PermissionDenied {
            Error::PermissionDenied(e.to_string())
        } else {
            Error::Io(e)
        }
    }
}

impl From<zip::result::ZipError> for Error {
    fn from(e: zip::result::ZipError) -> Self {
        match e {
            zip::result::ZipError::Io(io) => Error::from(io),
            other => Error::Corrupt(other.to_string()),
        }
    }
}

impl From<sevenz_rust2::Error> for Error {
    fn from(e: sevenz_rust2::Error) -> Self {
        // `sevenz-rust2` no implementa `source()`, pero conserva el `io::Error` original en sus
        // variantes tipadas. Se inspecciona ese error directamente: disco lleno se decide por
        // CÓDIGO del sistema, nunca por el texto del mensaje.
        use sevenz_rust2::Error as SevenZ;
        let io_err = match &e {
            SevenZ::Io(io, _) | SevenZ::FileOpen(io, _) | SevenZ::MaybeBadPassword(io) => Some(io),
            _ => None,
        };
        if let Some(io_err) = io_err {
            if is_disk_full(io_err) {
                return Error::DiskFull;
            }
            if io_err.kind() == io::ErrorKind::PermissionDenied {
                return Error::PermissionDenied(io_err.to_string());
            }
        }
        Error::Output(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_full_kind_is_disk_full() {
        assert!(is_disk_full(&io::Error::from(io::ErrorKind::StorageFull)));
        assert!(matches!(
            Error::from(io::Error::from(io::ErrorKind::StorageFull)),
            Error::DiskFull
        ));
    }

    #[cfg(unix)]
    #[test]
    fn enospc_is_disk_full() {
        let e = io::Error::from_raw_os_error(28);
        assert!(is_disk_full(&e));
        assert!(matches!(Error::from(e), Error::DiskFull));
    }

    #[cfg(windows)]
    #[test]
    fn windows_disk_full_codes() {
        assert!(is_disk_full(&io::Error::from_raw_os_error(112)));
        assert!(is_disk_full(&io::Error::from_raw_os_error(39)));
    }

    #[test]
    fn permission_and_generic_errors_are_not_disk_full() {
        let p = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(!is_disk_full(&p));
        assert!(matches!(Error::from(p), Error::PermissionDenied(_)));
        let g = io::Error::other("no space left on device (solo texto)");
        assert!(!is_disk_full(&g), "no debe depender del texto");
        assert!(matches!(Error::from(g), Error::Io(_)));
        let n = io::Error::from(io::ErrorKind::NotFound);
        assert!(matches!(Error::from(n), Error::Io(_)));
    }

    #[test]
    fn disk_full_message_is_the_requested_one() {
        assert_eq!(
            Error::DiskFull.to_string(),
            "No hay suficiente espacio disponible en el disco para completar la operación. El archivo original permanece intacto."
        );
    }
}
