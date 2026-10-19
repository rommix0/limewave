//! Seamlime application library.

pub mod app;
pub mod audio;
pub mod folder;
pub mod theme;
pub mod ui;

use eframe::egui;

const ICON_PNG: &[u8] = include_bytes!("../../../assets/icon/icon-256.png");

/// Opens the application window and runs until it is closed.
pub fn run() -> eframe::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Seamlime")
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([900.0, 560.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Seamlime",
        options,
        Box::new(|cc| Ok(Box::new(app::SeamlimeApp::new(cc)))),
    )
}
