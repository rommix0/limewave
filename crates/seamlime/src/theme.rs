//! Colours and the dark visual style.

use eframe::egui::{self, Theme, ThemePreference};

/// The palette shared by every view.
pub mod colors {
    use eframe::egui::Color32;

    pub const PANEL: Color32 = Color32::from_rgb(0x1b, 0x1e, 0x25);
    pub const WAVE: Color32 = Color32::from_rgb(0x5f, 0xb3, 0xff);
    pub const LOOP: Color32 = Color32::from_rgb(0xff, 0xc4, 0x00);
    pub const START: Color32 = Color32::from_rgb(0x4a, 0xde, 0x80);
    pub const END: Color32 = Color32::from_rgb(0xf8, 0x71, 0x71);
    pub const XFADE: Color32 = Color32::from_rgb(0xc0, 0x84, 0xfc);
    pub const NEON: Color32 = Color32::from_rgb(0x22, 0xd3, 0xee);
    pub const LIME: Color32 = Color32::from_rgb(0xa3, 0xe6, 0x35);
}

/// Forces the dark theme and paints panels with the Seamlime panel colour.
pub fn apply(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = colors::PANEL;
    visuals.window_fill = colors::PANEL;
    visuals.hyperlink_color = colors::NEON;
    visuals.selection.bg_fill = colors::NEON.gamma_multiply(0.35);
    ctx.set_visuals_of(Theme::Dark, visuals);
    ctx.set_theme(ThemePreference::Dark);
}
