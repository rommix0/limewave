//! The About window: the app's name and version, who made it, and its licence.

use eframe::egui;

use crate::app::LimewaveApp;

/// The copyright line, made by the build script from the authors in the workspace manifest.
pub const COPYRIGHT: &str = env!("LIMEWAVE_COPYRIGHT");

/// The modal, while `app.ui.about_open` is set. Close, Escape and a click outside close it.
pub fn about_dialog(ctx: &egui::Context, app: &mut LimewaveApp) {
    if !app.ui.about_open {
        return;
    }
    let mut close = false;
    let modal = egui::Modal::new(egui::Id::unique("about_dialog")).show(ctx, |ui| {
        ui.set_width(340.0);
        ui.heading("Limewave");
        ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
        ui.add_space(4.0);
        ui.label("A loop and one-shot WAV sample editor");
        ui.add_space(8.0);
        ui.label(COPYRIGHT);
        ui.weak(
            "Licensed under the MIT licence or the Apache License, Version 2.0, at your option.",
        );
        ui.add_space(8.0);
        close = ui.button("Close").clicked();
    });
    if close || modal.should_close() {
        app.ui.about_open = false;
    }
}
