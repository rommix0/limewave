//! Keyboard audition driven through the whole app, without an audio device.

use std::path::PathBuf;

use eframe::egui::{Key, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave_core::document::load_wav_bytes;
use limewave_core::testwav::{sine, wav_bytes};
use limewave_core::wav::SampleFormat;

/// App state plus a text field, so a test can give a text edit the focus.
struct State {
    app: LimewaveApp,
    text: String,
}

fn harness(with_file: bool) -> Harness<'static, State> {
    let mut app = LimewaveApp::for_test();
    if with_file {
        let bytes = wav_bytes(
            &[sine(220.0, 44100, 4410, 0.5)],
            44100,
            SampleFormat::Int { bits: 16 },
            Vec::new(),
        );
        let session = Session::new(load_wav_bytes(PathBuf::from("t.wav"), &bytes).unwrap());
        app.files = vec![FileEntry::Loaded(Box::new(session))];
        app.select(0);
    }
    let state = State {
        app,
        text: String::new(),
    };
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        .allow_missing_glyphs()
        .build_ui_state(
            |ui, st: &mut State| {
                st.app.show(ui);
                egui::Window::new("notes").show(ui.ctx(), |ui| {
                    let label = ui.label("Name");
                    ui.text_edit_singleline(&mut st.text).labelled_by(label.id);
                });
            },
            state,
        );
    harness.run();
    harness
}

use eframe::egui;

#[test]
fn qwerty_key_holds_and_releases_a_note() {
    let mut h = harness(false);
    assert!(h.state().app.ui.audition);
    h.key_down(Key::Z);
    h.run();
    assert!(h.state().app.ui.held.contains(&48));
    h.key_up(Key::Z);
    h.run();
    assert!(h.state().app.ui.held.is_empty());
}

#[test]
fn octave_keys_shift_and_clamp_the_base() {
    let mut h = harness(false);
    h.key_press(Key::Equals);
    h.run();
    assert_eq!(h.state().app.ui.octave_base, 60);
    for _ in 0..10 {
        h.key_press(Key::Equals);
        h.run();
    }
    assert_eq!(h.state().app.ui.octave_base, 96);
    for _ in 0..10 {
        h.key_press(Key::Minus);
        h.run();
    }
    assert_eq!(h.state().app.ui.octave_base, 12);
}

#[test]
fn keys_are_ignored_with_audition_off() {
    let mut h = harness(false);
    h.state_mut().app.ui.audition = false;
    h.key_down(Key::Z);
    h.run();
    assert!(h.state().app.ui.held.is_empty());
}

#[test]
fn keys_are_ignored_while_a_text_edit_has_focus() {
    let mut h = harness(false);
    h.get_by_role(egui::accesskit::Role::TextInput).focus();
    h.run();
    assert!(h.get_by_role(egui::accesskit::Role::TextInput).is_focused());
    h.key_down(Key::Z);
    h.run();
    assert!(h.state().app.ui.held.is_empty());
}

#[test]
fn audition_controls_are_disabled_without_an_engine() {
    let h = harness(true);
    for label in ["Play", "Play loop", "Stop", "Audition"] {
        assert!(
            h.get_by_label(label).accesskit_node().is_disabled(),
            "{label} should be disabled"
        );
    }
}
