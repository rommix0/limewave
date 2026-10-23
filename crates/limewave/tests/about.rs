//! The About window, driven through the whole app.

use eframe::egui::{Key, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use limewave::app::LimewaveApp;
use limewave::ui::about_dialog::COPYRIGHT;

fn harness() -> Harness<'static, LimewaveApp> {
    let mut h = Harness::builder()
        .with_size(vec2(1100.0, 700.0))
        // The seam readout uses an arrow that the test font set lacks.
        .allow_missing_glyphs()
        .build_ui_state(
            |ui, app: &mut LimewaveApp| app.show(ui),
            LimewaveApp::for_test(),
        );
    h.run();
    h
}

/// Clicks About in the toolbar.
fn open_about(h: &mut Harness<'static, LimewaveApp>) {
    h.get_by_label("About").click();
    h.run();
    assert!(h.state().ui.about_open, "the window is open");
}

#[test]
fn about_shows_the_version_and_the_credit() {
    let mut h = harness();
    open_about(&mut h);
    h.get_by_label(&format!("Version {}", env!("CARGO_PKG_VERSION")));
    h.get_by_label("Copyright (c) 2026 Anthony C. Bartman (@rommix0)");
    assert_eq!(
        COPYRIGHT,
        "Copyright (c) 2026 Anthony C. Bartman (@rommix0)"
    );
    h.get_by_label("Close").click();
    h.run();
    assert!(!h.state().ui.about_open, "Close closes it");
}

#[test]
fn escape_closes_about() {
    let mut h = harness();
    open_about(&mut h);
    h.key_press(Key::Escape);
    h.run();
    assert!(!h.state().ui.about_open);
}
