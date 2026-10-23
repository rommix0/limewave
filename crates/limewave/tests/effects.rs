//! The Effects menu, its dialogs and the clipping warning, driven through the whole app.

use eframe::egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave_core::effects::{AppliedEffect, Effect, FadeCurve, MonoSource, Op, peak};
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
fn fade_dialogs_apply_the_chosen_curve_to_start_end() {
    let mut h = looped_harness();
    assert_eq!(h.state().ui.effect_params.fade_curve, FadeCurve::Linear);
    for (item, curve_label, curve, op) in [
        (
            "Fade in…",
            "S-curve",
            FadeCurve::SCurve,
            Op::FadeIn as fn(FadeCurve) -> Op,
        ),
        (
            "Fade out…",
            "Exponential",
            FadeCurve::Exponential,
            Op::FadeOut,
        ),
    ] {
        menu_click(&mut h, item);
        h.get_by_label(curve_label).click();
        h.run();
        assert_eq!(h.state().ui.effect_params.fade_curve, curve, "remembered");
        h.get_by_label("Apply").click();
        h.run();
        assert_eq!(h.state().ui.effect_dialog, None);
        let e = session(h.state()).edit();
        // No selection: the fades take Start...End, the whole file here.
        assert_eq!(
            e.effects.last(),
            Some(&AppliedEffect {
                op: op(curve),
                first: 0,
                last: 7999,
            })
        );
    }
    let s = session(h.state());
    assert_eq!(s.processed[0][0], 0.0, "faded in from silence");
    assert!(s.processed[0][7999].abs() < 1e-6, "faded out to silence");
}

#[test]
fn normalize_dialog_applies_the_target_peak() {
    let mut h = looped_harness();
    h.state_mut().ui.effect_params.normalize_db = -6.0;
    menu_click(&mut h, "Normalize…");
    h.get_by_label("Apply").click();
    h.run();
    assert_eq!(h.state().ui.effect_dialog, None);
    let s = session(h.state());
    let target = 10f32.powf(-6.0 / 20.0);
    let got = peak(&s.processed, 0, 7999);
    assert!((got - target).abs() < 1e-4, "{got}");
    assert!(s.history.is_dirty());
}

#[test]
fn normalize_dialog_on_silence_says_why_nothing_happened() {
    let mut h = harness_for(app_with(&wav_bytes(
        &[vec![0.0; 1000]],
        44100,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    )));
    menu_click(&mut h, "Normalize…");
    h.get_by_label("Apply").click();
    h.run();
    assert_eq!(h.state().ui.effect_dialog, None);
    assert!(!session(h.state()).history.is_dirty());
    let message = h.state().ui.message.as_deref().expect("a reason is shown");
    assert!(message.contains("silent"), "{message}");
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
    assert!(!app.can_apply(Effect::StereoToMono(MonoSource::Mix)));
    assert!(!app.can_apply(Effect::SwapChannels));
    assert!(app.can_apply(Effect::MonoToStereo));

    menu_click(&mut h, "Mono to stereo");
    let app = h.state();
    assert_eq!(session(app).processed.len(), 2);
    assert!(app.can_apply(Effect::StereoToMono(MonoSource::Mix)));
    assert!(app.can_apply(Effect::SwapChannels));
    assert!(!app.can_apply(Effect::MonoToStereo));
}

#[test]
fn stereo_to_mono_dialog_keeps_the_chosen_channel() {
    let left = sine(440.0, 44100, 2000, 0.5);
    let right = sine(220.0, 44100, 2000, 0.25);
    let mut h = harness_for(app_with(&wav_bytes(
        &[left, right],
        44100,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    )));
    assert_eq!(h.state().ui.effect_params.mono_source, MonoSource::Mix);
    for (label, pick, channel) in [
        ("Right", MonoSource::Right, 1),
        ("Left", MonoSource::Left, 0),
    ] {
        menu_click(&mut h, "Stereo to mono…");
        h.get_by_label(label).click();
        h.run();
        h.get_by_label("Apply").click();
        h.run();
        assert_eq!(h.state().ui.effect_dialog, None);
        assert_eq!(h.state().ui.effect_params.mono_source, pick, "remembered");
        let s = session(h.state());
        assert_eq!(s.processed, vec![s.doc.audio[channel].clone()], "{label}");
        h.state_mut().undo();
        h.run();
    }

    // Mix averages the two.
    menu_click(&mut h, "Stereo to mono…");
    h.get_by_label("Mix").click();
    h.run();
    h.get_by_label("Apply").click();
    h.run();
    let s = session(h.state());
    let (l, r) = (&s.doc.audio[0], &s.doc.audio[1]);
    assert!((s.processed[0][100] - (l[100] + r[100]) / 2.0).abs() < 1e-6);
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

/// A 440 Hz sine of 8000 frames with a one-frame spike at 4000, one copy per channel.
fn spiked(channels: usize) -> Vec<u8> {
    let mut audio = sine(440.0, 44100, 8000, 0.5);
    audio[4000] = 0.95;
    wav_bytes(
        &vec![audio; channels],
        44100,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    )
}

/// The history entries after the one the file opened with.
fn steps(app: &LimewaveApp) -> usize {
    session(app).history.cursor()
}

#[test]
fn repair_click_fixes_the_selection_in_one_undo_step() {
    let mut h = harness_for(app_with(&spiked(1)));
    let before = session(h.state()).processed.clone();
    assert!(before[0][4000] > 0.9, "the spike is there");
    h.state_mut().ui.selection = Some((3995, 4005));
    h.state_mut().ui.play_from = Some(4002);
    menu_click(&mut h, "Repair click");

    let app = h.state();
    let s = session(app);
    assert!(s.processed[0][4000].abs() < 0.6, "{}", s.processed[0][4000]);
    assert_eq!(
        s.processed[0][3994], before[0][3994],
        "outside is untouched"
    );
    assert_eq!(s.processed[0][4006], before[0][4006]);
    assert_eq!(steps(app), 1, "one history step");
    assert_eq!(app.ui.selection, Some((3995, 4005)), "the selection stays");
    assert_eq!(app.ui.play_from, Some(4002), "so does the play-from point");

    h.state_mut().undo();
    let s = session(h.state());
    assert_eq!(s.processed, before, "undo restores bit for bit");
    assert_eq!(steps(h.state()), 0);
}

#[test]
fn repair_click_offers_channels_for_stereo() {
    let mut h = harness_for(app_with(&spiked(2)));
    let before = session(h.state()).processed.clone();
    h.state_mut().ui.selection = Some((3995, 4005));
    h.get_by_label("Effects").click();
    h.run();
    h.get_by_label("Repair click").click();
    h.run();
    for choice in ["Both channels", "Left only", "Right only"] {
        h.get_by_label(choice);
    }
    h.get_by_label("Left only").click();
    h.run();

    let s = session(h.state());
    assert!(s.processed[0][4000].abs() < 0.6, "the left is repaired");
    assert_eq!(s.processed[1], before[1], "the right is unchanged");
    assert_eq!(steps(h.state()), 1);
}

#[test]
fn repair_click_says_why_it_refused() {
    let mut h = harness_for(app_with(&spiked(1)));
    h.state_mut().ui.selection = Some((2500, 5499));
    menu_click(&mut h, "Repair click");

    let app = h.state();
    let message = app.ui.message.as_deref().expect("a reason is shown");
    assert!(
        message.starts_with("Repair click works on selections up to 50 ms"),
        "{message}"
    );
    assert_eq!(steps(app), 0, "the history is unchanged");
}

#[test]
fn repair_click_is_disabled_without_a_selection() {
    let mut h = harness_for(app_with(&spiked(1)));
    h.get_by_label("Effects").click();
    h.run();
    assert!(
        h.get_by_label("Repair click")
            .accesskit_node()
            .is_disabled(),
        "no selection"
    );
    assert_eq!(steps(h.state()), 0);
}
