//! The Generate tail dialog (v0.3 §7.1), driven through the whole app. The dialog renders on a
//! worker thread; the tests set the pause before a render to zero and wait for the workers
//! between frames.

use std::time::Duration;

use eframe::egui::accesskit::Role;
use eframe::egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave_core::edit::LoopMode;
use limewave_core::riff::Chunk;
use limewave_core::smpl::{Smpl, SmplLoop};
use limewave_core::tail::{TailMethod, TailMode};
use limewave_core::testwav::{sine, wav_bytes};
use limewave_core::wav::SampleFormat;

const FRAMES: usize = 8000;

/// A WAV of `audio` (one channel at 44.1 kHz), with a forward `smpl` loop when `looped`.
fn bytes(audio: Vec<f32>, looped: Option<(u32, u32)>) -> Vec<u8> {
    let chunks = looped
        .map(|(start, end)| {
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
                    start,
                    end,
                    fraction: 0,
                    play_count: 0,
                }],
                sampler_data: Vec::new(),
            };
            vec![Chunk {
                id: *b"smpl",
                data: smpl.to_bytes(),
            }]
        })
        .unwrap_or_default();
    wav_bytes(&[audio], 44100, SampleFormat::Int { bits: 16 }, chunks)
}

/// A steady 220 Hz tone of 8000 frames.
fn tone() -> Vec<f32> {
    sine(220.0, 44100, FRAMES, 0.5)
}

/// A harness showing the WAV `bytes` on its own, with no pause before dialog renders.
fn harness(bytes: &[u8]) -> Harness<'static, LimewaveApp> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tail.wav");
    std::fs::write(&path, bytes).unwrap();
    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    app.ui.render_debounce = Duration::ZERO;
    let mut h = Harness::builder()
        .with_size(vec2(1100.0, 760.0))
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

/// Lets the dialog's analysis and render finish, frame by frame.
fn settle(h: &mut Harness<'static, LimewaveApp>) {
    for _ in 0..4 {
        h.state_mut().wait_for_renders();
        h.run();
    }
}

/// Opens Effects → Generate tail… and waits for its first render.
fn open_dialog(h: &mut Harness<'static, LimewaveApp>) {
    h.get_by_label("Effects").click();
    h.run();
    h.get_by_label("Generate tail…").click();
    h.run();
    assert!(h.state().ui.tail_dialog.is_some(), "the dialog is open");
    settle(h);
}

fn apply_enabled(h: &Harness<'static, LimewaveApp>) -> bool {
    !h.get_by_label("Apply").accesskit_node().is_disabled()
}

#[test]
fn generate_tail_sustain_end_to_end() {
    let mut h = harness(&bytes(tone(), Some((1000, 4999))));
    let before = session(h.state()).edit().clone();
    open_dialog(&mut h);

    // The steady tone suggests Sustain, and the tag sits just right of it on its row.
    let sustain = h.get_by_role_and_label(Role::RadioButton, "Sustain").rect();
    let tagged = h.get_all_by_label("Suggested").any(|n| {
        let r = n.rect();
        r.left() >= sustain.right() - 1.0
            && r.left() - sustain.right() < 40.0
            && (r.center().y - sustain.center().y).abs() < 4.0
    });
    assert!(tagged, "\"Suggested\" next to Sustain");
    let d = h.state().ui.tail_dialog.as_ref().unwrap();
    assert_eq!(d.settings.mode, TailMode::Sustain, "preselected");
    assert_eq!(d.settings.method, TailMethod::Smooth, "tonal: Smooth");
    assert_eq!(d.settings.release_t60, None, "fitted until edited");
    assert!(apply_enabled(&h));
    h.get_by_label("Apply").click();
    h.run();
    assert!(
        h.state().ui.tail_dialog.is_none(),
        "Apply closes the dialog"
    );

    let s = session(h.state());
    let frames = s.frames();
    assert!(frames > FRAMES, "the file grew: {frames}");
    let e = s.edit();
    assert_eq!(e.end, frames - 1, "End on the new last frame");
    assert_eq!(e.loop_mode, LoopMode::Forward);
    let generated = s.tint_ranges();
    assert!(
        generated
            .iter()
            .any(|&(a, b)| a <= e.loop_start && e.loop_end <= b && e.loop_start >= FRAMES - 1),
        "loop {}..={} inside the generated audio {generated:?}",
        e.loop_start,
        e.loop_end
    );
    assert_eq!(s.history.cursor(), 1, "one history entry");

    h.state_mut().undo();
    h.run();
    let s = session(h.state());
    assert_eq!(s.frames(), FRAMES);
    assert_eq!(*s.edit(), before, "undo restores the old loop and End");
}

#[test]
fn generate_tail_decay_keeps_an_existing_loop() {
    let mut h = harness(&bytes(tone(), Some((1000, 4999))));
    let before = session(h.state()).edit().clone();
    open_dialog(&mut h);
    h.get_by_role_and_label(Role::RadioButton, "Decay").click();
    h.run();
    h.get_by_role_and_label(Role::RadioButton, "Granular")
        .click();
    h.run();
    settle(&mut h);
    let d = h.state().ui.tail_dialog.as_ref().unwrap();
    assert_eq!(d.settings.mode, TailMode::Decay);
    assert_eq!(d.settings.method, TailMethod::Granular);
    assert!(apply_enabled(&h));
    h.get_by_label("Apply").click();
    h.run();

    let s = session(h.state());
    assert!(s.frames() > FRAMES, "the file grew");
    let e = s.edit();
    assert_eq!(e.end, s.frames() - 1);
    assert_eq!(
        (e.loop_mode, e.loop_start, e.loop_end, e.crossfade),
        (
            before.loop_mode,
            before.loop_start,
            before.loop_end,
            before.crossfade
        ),
        "the loop is unchanged"
    );
}

#[test]
fn tail_dialog_shows_an_error_for_silence() {
    let mut h = harness(&bytes(vec![0.0; FRAMES], None));
    open_dialog(&mut h);
    h.get_by_label_contains("is silent");
    assert!(!apply_enabled(&h), "Apply is disabled");
    h.get_by_label("Apply").click();
    h.run();
    assert!(
        h.state().ui.tail_dialog.is_some(),
        "a disabled Apply does nothing"
    );
    assert_eq!(session(h.state()).frames(), FRAMES);
}

#[test]
fn cancel_leaves_everything_unchanged() {
    let mut h = harness(&bytes(tone(), None));
    let (cursor, source) = {
        let s = session(h.state());
        (s.history.cursor(), s.source)
    };
    open_dialog(&mut h);
    assert!(apply_enabled(&h), "a result is ready");
    h.get_by_label("Cancel").click();
    h.run();
    assert!(h.state().ui.tail_dialog.is_none());
    let s = session(h.state());
    assert_eq!(s.history.cursor(), cursor);
    assert!(!s.history.is_dirty());
    assert_eq!(s.frames(), FRAMES);
    assert_eq!(s.source, source);
    assert!(s.dialog_preview.is_none());
}

#[test]
fn preview_then_cancel_restores_the_session_buffer() {
    let mut h = harness(&bytes(tone(), None));
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
    assert!(preview.rendered[0].len() > FRAMES, "with the tail");
    assert_eq!(s.frames(), FRAMES, "the session is untouched");

    h.get_by_label("Cancel").click();
    h.run();
    let s = session(h.state());
    assert!(s.dialog_preview.is_none(), "Cancel ends the preview");
    assert_eq!(s.source, source);
    assert_eq!(s.rendered, rendered, "the session buffer is back");
    assert_eq!(s.history.cursor(), 0);
}

/// Where the Apply button is.
fn apply_rect(h: &Harness<'static, LimewaveApp>) -> eframe::egui::Rect {
    h.get_by_label("Apply").rect()
}

#[test]
fn the_dialog_keeps_its_place_while_rendering() {
    let mut h = harness(&bytes(tone(), None));
    open_dialog(&mut h);
    let at = apply_rect(&h);
    assert!(apply_enabled(&h));

    // A long pause keeps the next render pending. Single frames: a run would wait it out.
    h.state_mut().ui.render_debounce = Duration::from_secs(3600);
    for (radio, what) in [("Granular", "a method change"), ("Decay", "a mode change")] {
        h.get_by_role_and_label(Role::RadioButton, radio).click();
        h.step();
        h.step();
        h.get_by_label("Rendering…");
        assert!(!apply_enabled(&h), "nothing to apply yet");
        assert_eq!(apply_rect(&h), at, "Apply stays put after {what}");
    }

    // The result lands without moving it either.
    h.state_mut().ui.render_debounce = Duration::ZERO;
    settle(&mut h);
    assert!(apply_enabled(&h));
    assert_eq!(apply_rect(&h), at, "Apply stays put when the result lands");
}
