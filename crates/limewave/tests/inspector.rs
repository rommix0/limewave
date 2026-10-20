//! Inspector and seam view driven through the whole app.

use std::path::PathBuf;

use eframe::egui::{Key, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave_core::document::load_wav_bytes;
use limewave_core::edit::LoopMode;
use limewave_core::riff::Chunk;
use limewave_core::smpl::{Smpl, SmplLoop};
use limewave_core::testwav::{sine, wav_bytes};
use limewave_core::wav::SampleFormat;

fn smpl_chunk(start: u32, end: u32) -> Chunk {
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
    Chunk {
        id: *b"smpl",
        data: smpl.to_bytes(),
    }
}

/// A harness showing `bytes` as the only open file, after a few frames.
fn harness(bytes: &[u8]) -> Harness<'static, LimewaveApp> {
    let session = Session::new(load_wav_bytes(PathBuf::from("t.wav"), bytes).unwrap());
    let mut app = LimewaveApp::for_test();
    app.files = vec![FileEntry::Loaded(Box::new(session))];
    app.select(0);
    let mut harness = Harness::builder()
        .with_size(vec2(1280.0, 800.0))
        // The seam readout uses an arrow that the test font set lacks.
        .allow_missing_glyphs()
        .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app);
    harness.run();
    harness
}

fn session<'a>(h: &'a Harness<'static, LimewaveApp>) -> &'a Session {
    let FileEntry::Loaded(s) = &h.state().files[0] else {
        panic!("file not loaded");
    };
    s
}

/// A 440 Hz sine, 16-bit mono, with a forward loop 1000..=5000.
fn looped() -> Vec<u8> {
    wav_bytes(
        &[sine(440.0, 44100, 8000, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        vec![smpl_chunk(1000, 5000)],
    )
}

fn unlooped() -> Vec<u8> {
    wav_bytes(
        &[sine(440.0, 44100, 8000, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    )
}

#[test]
fn inspector_root_edit_applies() {
    let mut h = harness(&looped());
    assert_eq!(session(&h).edit().root, 60);
    assert!(!session(&h).history.is_dirty());

    h.get_by_label("Root key").focus();
    h.run();
    h.get_by_label("Root key").type_text("D4");
    h.run();
    h.key_press(Key::Enter);
    h.run();

    assert_eq!(session(&h).edit().root, 62);
    assert!(session(&h).history.is_dirty());
}

#[test]
fn invalid_root_text_reverts() {
    let mut h = harness(&looped());
    h.get_by_label("Root key").focus();
    h.run();
    h.get_by_label("Root key").type_text("zz");
    h.run();
    h.key_press(Key::Enter);
    h.run();
    assert_eq!(session(&h).edit().root, 60);
    assert!(!session(&h).history.is_dirty());
}

#[test]
fn octave_buttons_shift_the_root_by_twelve() {
    let mut h = harness(&looped());
    h.get_by_label("2×").click();
    h.run();
    assert_eq!(session(&h).edit().root, 72);
    h.get_by_label("½").click();
    h.run();
    h.get_by_label("½").click();
    h.run();
    assert_eq!(session(&h).edit().root, 48);
}

/// 16-bit mono with the values of a known bad splice: loop 2532..=2915, last loop sample
/// 8331, first loop sample 7381, and 7473 after the loop end.
fn bad_splice() -> Vec<u8> {
    let mut x = vec![0.0f32; 4000];
    x[2915] = 8331.0 / 32768.0;
    x[2532] = 7381.0 / 32768.0;
    x[2916] = 7473.0 / 32768.0;
    wav_bytes(
        &[x],
        44100,
        SampleFormat::Int { bits: 16 },
        vec![smpl_chunk(2532, 2915)],
    )
}

#[test]
fn seam_readout_shows_jump_and_natural_next() {
    let h = harness(&bad_splice());
    let s = session(&h);
    assert_eq!((s.edit().loop_start, s.edit().loop_end), (2532, 2915));
    h.get_by_label("jump 8331 → 7381 · natural next 7473");
}

#[test]
fn seam_view_says_when_the_loop_is_off() {
    let h = harness(&unlooped());
    assert_eq!(session(&h).edit().loop_mode, LoopMode::Off);
    h.get_by_label("Loop is off");
    assert!(h.query_by_label_contains("jump ").is_none());
}

#[test]
fn nudging_the_loop_end_is_one_undo_step() {
    let mut h = harness(&looped());
    // The seam view lists the loop start buttons first, then the loop end ones.
    h.get_all_by_label("+1").nth(1).unwrap().click();
    h.run();
    assert_eq!(session(&h).edit().loop_end, 5001);
    assert_eq!(session(&h).edit().loop_start, 1000);

    h.get_all_by_label("-1").next().unwrap().click();
    h.run();
    assert_eq!(session(&h).edit().loop_start, 999);

    let FileEntry::Loaded(s) = &mut h.state_mut().files[0] else {
        unreachable!()
    };
    s.undo(None);
    assert_eq!(s.edit().loop_start, 1000);
    s.undo(None);
    assert_eq!(s.edit().loop_end, 5000);
    assert!(!s.history.is_dirty());
}

#[test]
fn crossing_nudge_puts_the_next_sample_on_an_upward_crossing() {
    use limewave_core::dsp::is_upward;
    let mut h = harness(&looped());
    h.get_all_by_label("zc>").nth(1).unwrap().click();
    h.run();
    let s = session(&h);
    assert!(s.edit().loop_end > 5000);
    assert!(is_upward(&s.mono, s.edit().loop_end + 1));
}

#[test]
fn mode_selector_turns_the_loop_off_and_on() {
    let mut h = harness(&looped());
    h.get_by_label("Off").click();
    h.run();
    assert_eq!(session(&h).edit().loop_mode, LoopMode::Off);
    h.get_by_label("Ping-pong").click();
    h.run();
    assert_eq!(session(&h).edit().loop_mode, LoopMode::PingPong);
}

#[test]
fn file_metadata_warns_about_markers() {
    let cue = Chunk {
        id: *b"cue ",
        data: vec![0, 0, 0, 0],
    };
    let bytes = wav_bytes(
        &[sine(440.0, 44100, 2000, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        vec![cue],
    );
    let h = harness(&bytes);
    h.get_by_label("Markers in this file will be removed on save");
}

#[test]
fn crossfade_field_in_ms_makes_the_seam_continuous() {
    use limewave::ui::seam::{SeamChannel, seam_window};
    let mut h = harness(&looped());
    let jump = |h: &Harness<'static, LimewaveApp>| {
        let s = session(h);
        let e = s.edit();
        seam_window(&s.rendered, SeamChannel::Sum, e.loop_start, e.loop_end, 64)
            .unwrap()
            .jump
            .abs()
    };
    let before = jump(&h);
    assert!(
        before > 0.1,
        "the test loop starts as a bad splice: {before}"
    );

    // The field is the second widget named "Crossfade", after the slider.
    h.get_all_by_label("Crossfade")
        .nth(1)
        .unwrap()
        .set_value_accesskit(5.0);
    h.run();
    let s = session(&h);
    assert_eq!(s.edit().crossfade, 221, "5 ms at 44100 Hz");
    assert!(s.history.is_dirty());
    let max_step = s
        .mono
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max);
    let after = jump(&h);
    assert!(
        after <= max_step + 1e-4,
        "crossfaded seam jumps {after}, steps are up to {max_step}"
    );
}
