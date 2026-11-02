#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod ui;

use eframe::egui;
use megacompress::logging;

fn main() -> eframe::Result<()> {
    let log_path = logging::init();
    tracing::info!("MegaCompress {} iniciado", env!("CARGO_PKG_VERSION"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 680.0])
            .with_min_inner_size([640.0, 480.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "MegaCompress",
        options,
        Box::new(move |cc| Ok(Box::new(ui::app::MegaApp::new(cc, log_path)))),
    )
}
