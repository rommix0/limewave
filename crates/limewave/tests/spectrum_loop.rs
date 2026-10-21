//! The Make loop from spectrum dialog (v0.3 §7.2), driven through the whole app. The dialog
//! renders on a worker thread; the tests set the pause before a render to zero and wait for
//! the workers between frames.

use std::time::Duration;

use eframe::egui::accesskit::Role;
use eframe::egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave_core::edit::LoopMode;
use limewave_core::testwav::{sine, wav_bytes};
use limewave_core::wav::SampleFormat;

const FRAMES: usize = 8000;
const RATE: u32 = 44100;
const SELECTION: (usize, usize) = (2000, 5999);

/// A harness showing a steady 220 Hz tone of 8000 frames, with `selection` and no pause
/// before dialog renders.
fn harness(selection: Option<(usize, usize)>) -> Harness<'static, LimewaveApp> {
    let bytes = wav_bytes(
        &[sine(220.0, RATE, FRAMES, 0.5)],
        RATE,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tone.wav");
    std::fs::write(&path, bytes).unwrap();
    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    app.ui.render_debounce = Duration::ZERO;
    let mut h = Harness::builder()
        .with_size(vec2(1100.0, 860.0))
        .with_step_dt(0.25)
        // The seam readout uses an arrow that the test font set lacks.
        .allow_missing_glyphs()
        .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app);
    h.run();
    h.state_mut().ui.selection = selection;
    h.run();
    h
}

fn session(app: &LimewaveApp) -> &Session {
    let FileEntry::Loaded(s) = &app.files[0] else {
        panic!("file not loaded");
    };
    s
}

/// Lets the dialog's analysis and render finish, frame by frame.
fn settle(h: &mut Harness<'static, LimewaveApp>) {
    for _ in 0..4 {
        h.state_mut().wait_for_renders();
        h.run();
    }
}

/// Opens Effects → Make loop from spectrum… and waits for its first render.
fn open_dialog(h: &mut Harness<'static, LimewaveApp>) {
    h.get_by_label("Effects").click();
    h.run();
    h.get_by_label("Make loop from spectrum…").click();
    h.run();
    assert!(h.state().ui.spectrum_dialog.is_some(), "the dialog is open");
    settle(h);
}

fn enabled(h: &Harness<'static, LimewaveApp>, label: &str) -> bool {
    !h.get_by_label(label).accesskit_node().is_disabled()
}

/// Whether the number field labelled `label` is enabled (its label is a node of its own).
fn spin_enabled(h: &Harness<'static, LimewaveApp>, label: &str) -> bool {
    !h.get_by_role_and_label(Role::SpinButton, label)
        .accesskit_node()
        .is_disabled()
}

/// Applies the open dialog's result and checks that it closed.
fn apply(h: &mut Harness<'static, LimewaveApp>) {
    assert!(enabled(h, "Apply"), "a result is ready");
    h.get_by_label("Apply").click();
    h.run();
    assert!(
        h.state().ui.spectrum_dialog.is_none(),
        "Apply closes the dialog"
    );
}

#[test]
fn spectrum_loop_end_to_end() {
    let mut h = harness(Some(SELECTION));
    let (before, original) = {
        let s = session(h.state());
        (s.edit().clone(), s.processed.clone())
    };
    open_dialog(&mut h);
    h.get_by_label_contains("2000–5999");
    h.get_by_label_contains("pitch 220");
    apply(&mut h);

    // T is 100 ms, limited to half the range; L is 3 s.
    let t = 4410.min((SELECTION.1 + 1 - SELECTION.0) / 2);
    let l = 3 * RATE as usize;
    let s = session(h.state());
    assert_eq!(s.frames(), FRAMES + t + l - 4000);
    let e = s.edit();
    assert_eq!(e.loop_mode, LoopMode::Forward);
    assert_eq!(
        (e.loop_start, e.loop_end, e.crossfade),
        (SELECTION.0 + t, SELECTION.0 + t + l - 1, 0),
        "loop 0 is the loop"
    );
    assert_eq!(
        s.processed[0][..SELECTION.0],
        original[0][..SELECTION.0],
        "the audio before the range is unchanged"
    );
    assert_eq!(s.history.cursor(), 1, "one history entry");

    h.state_mut().undo();
    h.run();
    let s = session(h.state());
    assert_eq!(s.frames(), FRAMES);
    assert_eq!(*s.edit(), before, "undo restores the markers and loop");
    assert_eq!(s.processed, original, "and the audio");
}

#[test]
fn detune_disabled_without_pitch_lock() {
    let mut h = harness(Some(SELECTION));
    open_dialog(&mut h);
    assert!(enabled(&h, "Pitch lock"), "a pitch was found");
    assert!(spin_enabled(&h, "Detune (cents)"));
    h.get_by_label("Pitch lock").click();
    h.run();
    let d = h.state().ui.spectrum_dialog.as_ref().unwrap();
    assert!(!d.settings.pitch_lock);
    assert!(
        !spin_enabled(&h, "Detune (cents)"),
        "Detune needs Pitch lock"
    );
}

/// Opens the dialog, turns Pitch lock off (Plain uses the seed), clicks New variation
/// `variations` times, applies, and hands back the file's audio. The edit is then undone.
fn plain_loop(h: &mut Harness<'static, LimewaveApp>, variations: usize) -> Vec<Vec<f32>> {
    open_dialog(h);
    h.get_by_label("Pitch lock").click();
    h.run();
    for _ in 0..variations {
        h.get_by_label("New variation").click();
        h.run();
    }
    settle(h);
    let seed = h.state().ui.spectrum_dialog.as_ref().unwrap().settings.seed;
    assert_eq!(
        seed,
        1 + variations as u64,
        "each click increments the seed"
    );
    apply(h);
    let audio = session(h.state()).processed.clone();
    h.state_mut().undo();
    h.run();
    audio
}

#[test]
fn new_variation_changes_the_audio_same_seed_does_not() {
    let mut h = harness(Some(SELECTION));
    let first = plain_loop(&mut h, 0);
    let again = plain_loop(&mut h, 0);
    assert_eq!(first, again, "the same seed gives the same audio");
    let varied = plain_loop(&mut h, 1);
    assert_eq!(varied[0].len(), first[0].len());
    assert_ne!(varied, first, "a new seed gives new audio");
}

#[test]
fn whole_file_when_nothing_is_selected() {
    let mut h = harness(None);
    let e = session(h.state()).edit().clone();
    open_dialog(&mut h);
    h.get_by_label_contains(&format!("{}–{}", e.start, e.end));
    h.get_by_label_contains("Start…End");
    apply(&mut h);

    // T is 100 ms, limited to half the range; there is no audio after End, so no exit.
    let t = 4410.min((e.end + 1 - e.start) / 2);
    let l = 3 * RATE as usize;
    let s = session(h.state());
    assert_eq!(s.frames(), FRAMES + t + l - (e.end + 1 - e.start));
    let after = s.edit();
    assert_eq!(
        (after.loop_start, after.loop_end),
        (e.start + t, e.start + t + l - 1)
    );
}

#[test]
fn preview_then_cancel_restores_the_session_buffer() {
    let mut h = harness(Some(SELECTION));
    let (source, rendered) = {
        let s = session(h.state());
        (s.source, s.rendered.clone())
    };
    open_dialog(&mut h);
    h.get_by_label("Preview").click();
    h.run();
    let s = session(h.state());
    let preview = s.dialog_preview.as_ref().expect("the preview is live");
    assert_ne!(preview.source, source, "under its own source");
    assert!(preview.rendered[0].len() > FRAMES, "with the loop");
    assert_eq!(s.frames(), FRAMES, "the session is untouched");

    h.get_by_label("Cancel").click();
    h.run();
    assert!(h.state().ui.spectrum_dialog.is_none());
    let s = session(h.state());
    assert!(s.dialog_preview.is_none(), "Cancel ends the preview");
    assert_eq!(s.source, source);
    assert_eq!(s.rendered, rendered, "the session buffer is back");
    assert_eq!(s.history.cursor(), 0);
}

#[test]
fn silence_shows_an_error_and_disables_apply() {
    let bytes = wav_bytes(
        &[vec![0.0; FRAMES]],
        RATE,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("silent.wav");
    std::fs::write(&path, bytes).unwrap();
    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    app.ui.render_debounce = Duration::ZERO;
    let mut h = Harness::builder()
        .with_size(vec2(1100.0, 860.0))
        .allow_missing_glyphs()
        .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app);
    h.run();
    open_dialog(&mut h);
    h.get_by_label_contains("is silent");
    assert!(!enabled(&h, "Apply"));
    assert!(!enabled(&h, "Preview"));
}

#[test]
fn the_dialog_keeps_its_place_while_rendering() {
    let mut h = harness(Some(SELECTION));
    open_dialog(&mut h);
    let at = h.get_by_label("Apply").rect();
    assert!(enabled(&h, "Apply"));

    // A long pause keeps the next render pending. Single frames: a run would wait it out.
    h.state_mut().ui.render_debounce = Duration::from_secs(3600);
    for (control, what) in [
        ("Pitch lock", "Pitch lock off"),
        ("New variation", "a new variation"),
        ("Pitch lock", "Pitch lock on"),
    ] {
        h.get_by_label(control).click();
        h.step();
        h.step();
        h.get_by_label("Rendering…");
        assert!(!enabled(&h, "Apply"), "nothing to apply yet");
        assert_eq!(
            h.get_by_label("Apply").rect(),
            at,
            "Apply stays put after {what}"
        );
    }

    // The result lands without moving it either.
    h.state_mut().ui.render_debounce = Duration::ZERO;
    settle(&mut h);
    assert!(enabled(&h, "Apply"));
    assert_eq!(
        h.get_by_label("Apply").rect(),
        at,
        "Apply stays put when the result lands"
    );
}
