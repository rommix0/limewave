//! The Change sample rate dialog (v0.6), driven through the whole app. Convert renders on a
//! worker thread; the tests wait for it between frames.

use eframe::egui::{Key, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave_core::testwav::{sine, wav_bytes};
use limewave_core::wav::SampleFormat;

const FRAMES: usize = 8000;

/// A harness showing a 440 Hz mono 16-bit sine of 8000 frames at 44.1 kHz on its own.
fn harness() -> Harness<'static, LimewaveApp> {
    let bytes = wav_bytes(
        &[sine(440.0, 44100, FRAMES, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rate.wav");
    std::fs::write(&path, bytes).unwrap();
    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    let mut h = Harness::builder()
        .with_size(vec2(1100.0, 700.0))
        .with_step_dt(0.25)
        // The seam readout uses an arrow that the test font set lacks.
        .allow_missing_glyphs()
        .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app);
    h.run();
    h
}

fn session(app: &LimewaveApp) -> &Session {
    let FileEntry::Loaded(s) = &app.files[0] else {
        panic!("file not loaded");
    };
    s
}

/// Opens Effects → Change sample rate….
fn open_dialog(h: &mut Harness<'static, LimewaveApp>) {
    h.get_by_label("Effects").click();
    h.run();
    h.get_by_label("Change sample rate…").click();
    h.run();
    assert!(h.state().ui.rate_dialog.is_some(), "the dialog is open");
}

/// Types `text` into the New rate field.
fn type_rate(h: &mut Harness<'static, LimewaveApp>, text: &str) {
    h.get_by_label("New rate").focus();
    h.run();
    h.get_by_label("New rate").type_text(text);
    h.run();
}

fn field_text(h: &Harness<'static, LimewaveApp>) -> String {
    h.state().ui.rate_dialog.as_ref().unwrap().text.clone()
}

fn convert_enabled(h: &Harness<'static, LimewaveApp>) -> bool {
    !h.get_by_label("Convert").accesskit_node().is_disabled()
}

#[test]
fn rate_dialog_converts_a_file() {
    let mut h = harness();
    let cursor = session(h.state()).history.cursor();
    open_dialog(&mut h);
    h.get_by_label("Current rate: 44100 Hz");
    type_rate(&mut h, "31250");
    assert_eq!(field_text(&h), "31250");
    assert!(convert_enabled(&h));
    h.get_by_label("Length after: 5669 frames (0.18 s)");

    h.get_by_label("Convert").click();
    h.run();
    h.state_mut().wait_for_renders();
    h.run();

    assert!(h.state().ui.rate_dialog.is_none(), "the result closes it");
    let s = session(h.state());
    assert_eq!(s.rate(), 31_250);
    assert_eq!(s.frames(), 5669);
    assert_eq!(s.history.cursor(), cursor + 1, "one undo step");
    h.state_mut().undo();
    assert_eq!(session(h.state()).rate(), 44_100);
}

#[test]
fn cancel_and_escape_close_the_dialog_without_converting() {
    let mut h = harness();
    open_dialog(&mut h);
    h.get_by_label("Cancel").click();
    h.run();
    assert!(h.state().ui.rate_dialog.is_none());

    open_dialog(&mut h);
    type_rate(&mut h, "22050");
    h.get_by_label("Convert").click();
    h.run();
    h.key_press(Key::Escape);
    h.run();
    assert!(h.state().ui.rate_dialog.is_none(), "Escape closes it");
    // The render finishes unseen.
    h.state_mut().wait_for_renders();
    h.run();
    assert_eq!(session(h.state()).rate(), 44_100);
}

#[test]
fn rate_dialog_explains_why_convert_is_disabled() {
    let mut h = harness();
    open_dialog(&mut h);
    assert!(!convert_enabled(&h), "empty field");
    h.get_by_label("Type a sample rate in Hz");

    type_rate(&mut h, "500");
    assert!(!convert_enabled(&h), "below the range");
    h.get_by_label("Use a rate from 1000 to 768000 Hz");

    h.state_mut().ui.rate_dialog.as_mut().unwrap().text = "44100".into();
    h.run();
    assert!(!convert_enabled(&h), "the current rate");
    h.get_by_label("That is the current rate");

    h.state_mut().ui.rate_dialog.as_mut().unwrap().text = "48000".into();
    h.run();
    assert!(convert_enabled(&h));
}

#[test]
fn the_field_accepts_only_digits() {
    let mut h = harness();
    open_dialog(&mut h);
    type_rate(&mut h, "4a,4 1");
    assert_eq!(field_text(&h), "441");
}

#[test]
fn common_rates_fill_the_field() {
    let mut h = harness();
    open_dialog(&mut h);
    h.get_by_label("Common rates").click();
    h.run();
    h.get_by_label("22050 Hz").click();
    h.run();
    assert_eq!(field_text(&h), "22050");
    h.get_by_label("Content above 11025 Hz will be removed");
    assert!(convert_enabled(&h));
}
