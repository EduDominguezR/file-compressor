//! MegaCompress: motor y utilidades. La UI vive en el binario (`main.rs` + `ui/`).
//! Se separa en librería para poder ejecutar los tests de integración contra el motor.

pub mod app_state;
pub mod engine;
pub mod error;
pub mod fs;
pub mod logging;
pub mod util;
pub mod verify;
