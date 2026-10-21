//! The Effects menu, its dialogs and the clipping warning, driven through the whole app.

use eframe::egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave_core::effects::Effect;
use limewave_core::riff::Chunk;
use limewave_core::smpl::{Smpl, SmplLoop};
use limewave_core::testwav::{sine, wav_bytes};
use limewave_core::wav::SampleFormat;

/// A 440 Hz mono sine of 8000 frames with a forward loop at 1000..=5000.
fn looped_bytes() -> Vec<u8> {
    let smpl = Smpl {
        manufacturer: 0,
        product: 0,
        sample_period: 22_675,
        unity_note: 60,
        pitch_fraction: 0,
        smpte_format: 0,
        smpte_offset: 0,
        loops: vec![SmplLoop {
            cue_id: 0,
            loop_type: 0,
            start: 1000,
            end: 5000,
            fraction: 0,
            play_count: 0,
        }],
        sampler_data: Vec::new(),
    };
    wav_bytes(
        &[sine(440.0, 44100, 8000, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        vec![Chunk {
            id: *b"smpl",
            data: smpl.to_bytes(),
        }],
    )
}

/// An app showing the WAV file `bytes` on its own (no file list).
fn app_with(bytes: &[u8]) -> LimewaveApp {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.wav");
    std::fs::write(&path, bytes).unwrap();
    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    app
}

/// A harness around `app`, after one frame.
fn harness_for(app: LimewaveApp) -> Harness<'static, LimewaveApp> {
    let mut harness = Harness::builder()
        .with_size(vec2(1100.0, 700.0))
        .with_step_dt(0.25)
        // The seam readout uses an arrow that the test font set lacks.
        .allow_missing_glyphs()
        .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app);
    harness.run();
    harness
}

fn looped_harness() -> Harness<'static, LimewaveApp> {
    harness_for(app_with(&looped_bytes()))
}

fn session(app: &LimewaveApp) -> &Session {
    let FileEntry::Loaded(s) = &app.files[0] else {
        panic!("file not loaded");
    };
    s
}

/// Opens the Effects menu and clicks `item`.
fn menu_click(h: &mut Harness<'static, LimewaveApp>, item: &str) {
    h.get_by_label("Effects").click();
    h.run();
    h.get_by_label(item).click();
    h.run();
}

#[test]
fn gain_dialog_applies_to_the_selection() {
    let mut h = looped_harness();
    h.state_mut().ui.selection = Some((1000, 1999));
    h.state_mut().ui.effect_params.gain_db = 6.0206;
    menu_click(&mut h, "Gain…");
    assert!(h.state().ui.effect_dialog.is_some(), "the dialog is open");
    h.get_by_label("Apply").click();
    h.run();

    let s = session(h.state());
    let (src, out) = (&s.doc.audio[0], &s.processed[0]);
    assert!((out[1500] - 2.0 * src[1500]).abs() < 1e-4, "{}", out[1500]);
    assert_eq!(out[500], src[500]);
    assert_eq!(out[2500], src[2500]);
    assert_eq!(h.state().ui.effect_dialog, None, "Apply closes the dialog");
    assert!(s.history.is_dirty());

    h.state_mut().undo();
    let s = session(h.state());
    assert_eq!(s.processed[0][1500], s.doc.audio[0][1500]);
    assert!(!s.history.is_dirty(), "one undo restores");
}

#[test]
fn cancel_closes_the_dialog_without_applying() {
    let mut h = looped_harness();
    menu_click(&mut h, "Normalize…");
    assert!(h.state().ui.effect_dialog.is_some());
    h.get_by_label("Cancel").click();
    h.run();
    assert_eq!(h.state().ui.effect_dialog, None);
    assert!(!session(h.state()).history.is_dirty());
}

#[test]
fn invert_from_the_menu_without_selection_processes_the_whole_file() {
    let mut h = looped_harness();
    menu_click(&mut h, "Invert polarity");

    let s = session(h.state());
    for f in [0, 500, 3000, 7999] {
        assert_eq!(s.processed[0][f], -s.doc.audio[0][f], "frame {f}");
    }
    assert!(s.history.is_dirty());
}

#[test]
fn reverse_from_the_menu_mirrors_the_loop() {
    // The loop is 1000..=5000 in 8000 frames; Reverse works on Start...End (0..=7999).
    let mut h = looped_harness();
    menu_click(&mut h, "Reverse");

    let e = session(h.state()).edit();
    assert_eq!((e.loop_start, e.loop_end), (2999, 6999));
}

#[test]
fn clipping_warning_after_a_big_gain() {
    let mut h = looped_harness();
    assert!(h.query_by_label("Will clip when saved").is_none());
    assert!(h.state_mut().apply_effect(Effect::Gain { db: 12.0 }));
    h.run();
    assert!(session(h.state()).will_clip());
    h.get_by_label("Will clip when saved");

    h.state_mut().undo();
    h.run();
    assert!(h.query_by_label("Will clip when saved").is_none());
}

#[test]
fn dialog_predicts_clipping_for_integer_files() {
    let mut h = looped_harness();
    h.state_mut().ui.effect_params.gain_db = 12.0;
    menu_click(&mut h, "Gain…");
    h.get_by_label("Will clip when saved");
    h.get_by_label("Cancel").click();
    h.run();

    h.state_mut().ui.effect_params.gain_db = 3.0;
    menu_click(&mut h, "Gain…");
    assert!(h.query_by_label("Will clip when saved").is_none());
}

#[test]
fn channel_tools_follow_the_channel_count() {
    let mut h = looped_harness();
    let app = h.state();
    assert!(!app.can_apply(Effect::StereoToMono));
    assert!(!app.can_apply(Effect::SwapChannels));
    assert!(app.can_apply(Effect::MonoToStereo));

    menu_click(&mut h, "Mono to stereo");
    let app = h.state();
    assert_eq!(session(app).processed.len(), 2);
    assert!(app.can_apply(Effect::StereoToMono));
    assert!(app.can_apply(Effect::SwapChannels));
    assert!(!app.can_apply(Effect::MonoToStereo));
}

#[test]
fn auto_trim_dialog_uses_the_threshold() {
    let mut audio = vec![0.0; 1000];
    audio.extend(sine(440.0, 44100, 4000, 0.5));
    audio.extend(vec![0.0; 1000]);
    let mut h = harness_for(app_with(&wav_bytes(
        &[audio],
        44100,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    )));
    assert_eq!(h.state().ui.effect_params.silence_db, -60.0);
    menu_click(&mut h, "Auto-trim silence…");
    h.get_by_label("Apply").click();
    h.run();

    let e = session(h.state()).edit();
    assert!(e.start.abs_diff(1000) <= 1, "start {}", e.start);
    assert!(e.end.abs_diff(4999) <= 1, "end {}", e.end);
    assert_eq!(h.state().ui.effect_dialog, None);
}
