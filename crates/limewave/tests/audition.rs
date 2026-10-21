//! Keyboard audition driven through the whole app, without an audio device.

use std::path::PathBuf;

use std::sync::atomic::Ordering;

use eframe::egui::{Event, Key, Modifiers, PointerButton, Pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave::audio::midi::decode_last_note;
use limewave::ui::keyboard::key_rect;
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

fn root_of(h: &Harness<'static, State>) -> u8 {
    let FileEntry::Loaded(s) = &h.state().app.files[0] else {
        panic!("file not loaded");
    };
    s.shown_edit().root
}

fn is_dirty(h: &Harness<'static, State>) -> bool {
    let FileEntry::Loaded(s) = &h.state().app.files[0] else {
        panic!("file not loaded");
    };
    s.history.is_dirty()
}

#[test]
fn right_clicking_a_key_sets_the_root_as_one_undo_step() {
    let mut h = harness(true);
    assert_eq!(root_of(&h), 60);
    // The lower part of D4 is clear of the black keys.
    let strip = h.get_by_label("Piano keyboard").rect();
    let d4 = key_rect(strip, 62);
    let at = Pos2::new(d4.center().x, d4.bottom() - 4.0);
    h.hover_at(at);
    h.run();
    for pressed in [true, false] {
        h.event(Event::PointerButton {
            pos: at,
            button: PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::NONE,
        });
    }
    h.run();
    assert_eq!(root_of(&h), 62);
    // One undo is enough to be back where the file started.
    h.state_mut().app.undo();
    h.run();
    assert_eq!(root_of(&h), 60);
    assert!(!is_dirty(&h));
}

#[test]
fn midi_learn_sets_the_root_from_the_next_note() {
    let mut h = harness(true);
    let seen = h.state().app.midi_last_note.load(Ordering::Relaxed);
    h.state_mut().app.ui.midi_learn = Some(decode_last_note(seen).map_or(0, |(s, _)| s));
    h.run();
    // Nothing yet: still armed, root unchanged.
    assert!(h.state().app.ui.midi_learn.is_some());
    assert_eq!(root_of(&h), 60);
    // Sequence 1, note 67.
    h.state()
        .app
        .midi_last_note
        .store((1 << 8) | 67, Ordering::Relaxed);
    h.run();
    assert_eq!(root_of(&h), 67);
    assert_eq!(h.state().app.ui.midi_learn, None);
    h.state_mut().app.undo();
    h.run();
    assert_eq!(root_of(&h), 60);
    assert!(!is_dirty(&h));
}

#[test]
fn escape_cancels_midi_learn_and_a_later_note_is_ignored() {
    let mut h = harness(true);
    h.state_mut().app.ui.midi_learn = Some(0);
    h.run();
    h.key_press(Key::Escape);
    h.run();
    assert_eq!(h.state().app.ui.midi_learn, None);
    h.state()
        .app
        .midi_last_note
        .store((1 << 8) | 67, Ordering::Relaxed);
    h.run();
    assert_eq!(root_of(&h), 60);
}
