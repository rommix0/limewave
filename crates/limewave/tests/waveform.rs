//! Waveform and overview views driven through the whole app with synthetic pointer input.

use std::path::PathBuf;
use std::time::Instant;

use eframe::egui::{
    Event, Key, Modifiers, MouseWheelSource, MouseWheelUnit, PointerButton, Pos2, Rect, TouchPhase,
    pos2,
};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave::ui::waveform::flag_hit_point;
use limewave_core::document::load_wav_bytes;
use limewave_core::dsp::is_upward;
use limewave_core::edit::{LoopMode, Marker};
use limewave_core::effects::Effect;
use limewave_core::riff::Chunk;
use limewave_core::smpl::{Smpl, SmplLoop};
use limewave_core::testwav::{sine, wav_bytes};
use limewave_core::wav::SampleFormat;

const LOOP_START: u32 = 1000;
const LOOP_END: u32 = 5000;

/// A 440 Hz mono sine with a forward loop in its `smpl` chunk.
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
            start: LOOP_START,
            end: LOOP_END,
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

/// An app showing the looped file on its own (no file list), after one frame.
fn looped_harness() -> Harness<'static, LimewaveApp> {
    looped_harness_with_dt(0.25)
}

/// `looped_harness` with `step_dt` seconds between frames.
fn looped_harness_with_dt(step_dt: f32) -> Harness<'static, LimewaveApp> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loop.wav");
    std::fs::write(&path, looped_bytes()).unwrap();
    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    let mut harness = Harness::builder()
        .with_size(eframe::egui::vec2(1100.0, 700.0))
        .with_step_dt(step_dt)
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

fn waveform_rect(h: &Harness<'static, LimewaveApp>) -> Rect {
    h.get_by_label("Waveform").rect()
}

/// Screen position of `frame` on the waveform's vertical middle.
fn frame_pos(h: &Harness<'static, LimewaveApp>, frame: usize) -> Pos2 {
    let rect = waveform_rect(h);
    let view = session(h).view.expect("the view is set after a frame");
    pos2(view.frame_to_x(frame as f64, rect.left()), rect.center().y)
}

/// Presses at `from`, moves in steps to `to` and releases.
fn drag(h: &mut Harness<'static, LimewaveApp>, from: Pos2, to: Pos2) {
    h.hover_at(from);
    h.step();
    h.drag_at(from);
    h.step();
    for i in 1..=4 {
        h.hover_at(from + (to - from) * (i as f32 / 4.0));
        h.step();
    }
    h.drop_at(to);
    h.step();
}

/// Two clicks at `at`, a frame apart: build the harness with a `step_dt` of well under 0.3 s
/// for them to count as a double-click.
fn double_click(h: &mut Harness<'static, LimewaveApp>, at: Pos2) {
    h.hover_at(at);
    h.step();
    for _ in 0..2 {
        for pressed in [true, false] {
            h.event(Event::PointerButton {
                pos: at,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            });
        }
        h.step();
    }
}

/// Where the flag of `marker` is, in the current view.
fn flag_pos(h: &Harness<'static, LimewaveApp>, marker: Marker) -> Pos2 {
    let s = session(h);
    flag_hit_point(waveform_rect(h), &s.view.unwrap(), s.edit(), marker)
}

#[test]
fn drag_loop_marker_snaps_and_is_one_undo_step() {
    let mut h = looped_harness();
    let before = session(&h).edit().clone();
    assert_eq!(before.loop_mode, LoopMode::Forward);
    assert_eq!(before.loop_end, LOOP_END as usize);

    let from = frame_pos(&h, LOOP_END as usize);
    drag(&mut h, from, from + eframe::egui::vec2(-60.0, 0.0));

    let s = session(&h);
    let moved = s.edit().loop_end;
    assert!(
        moved < LOOP_END as usize - 100,
        "the marker followed the drag"
    );
    assert!(
        is_upward(&s.mono, moved + 1),
        "loop end + 1 = {} is not an upward zero crossing",
        moved + 1
    );
    assert_eq!(
        s.edit().loop_start,
        before.loop_start,
        "only one marker moved"
    );
    assert!(s.preview.is_none(), "the preview is cleared on release");
    assert!(s.history.is_dirty());

    let FileEntry::Loaded(s) = &mut h.state_mut().files[0] else {
        unreachable!()
    };
    s.undo(None);
    assert_eq!(*s.edit(), before, "one undo restores the original");
    assert!(!s.history.is_dirty(), "the drag was a single history entry");
}

#[test]
fn shift_drag_ignores_snap_and_snap_off_lands_on_the_cursor() {
    let mut h = looped_harness();
    let rect = waveform_rect(&h);
    let from = frame_pos(&h, LOOP_END as usize);
    let to = from + eframe::egui::vec2(-60.0, 0.0);

    h.event(Event::ModifiersChanged(Modifiers::SHIFT));
    h.step();
    drag(&mut h, from, to);
    h.event(Event::ModifiersChanged(Modifiers::NONE));
    h.step();

    let view = session(&h).view.unwrap();
    let wanted = view.x_to_frame(to.x, rect.left()).round() as usize;
    assert_eq!(session(&h).edit().loop_end, wanted);
}

#[test]
fn dragging_empty_space_changes_nothing() {
    let mut h = looped_harness();
    let from = frame_pos(&h, 3000);
    drag(&mut h, from, from + eframe::egui::vec2(80.0, 0.0));
    let s = session(&h);
    assert!(!s.history.is_dirty());
    assert!(s.preview.is_none());
}

#[test]
fn dragging_a_marker_by_its_label_is_one_undo_step() {
    let mut h = looped_harness();
    let before = session(&h).edit().clone();

    let from = flag_pos(&h, Marker::LoopEnd);
    drag(&mut h, from, from + eframe::egui::vec2(-60.0, 0.0));

    let s = session(&h);
    let moved = s.edit().loop_end;
    assert!(
        moved < LOOP_END as usize - 100,
        "the flag dragged its marker"
    );
    assert!(
        is_upward(&s.mono, moved + 1),
        "loop end + 1 = {} is not an upward zero crossing",
        moved + 1
    );
    assert_eq!(s.edit().loop_start, before.loop_start);
    assert!(s.preview.is_none());

    let FileEntry::Loaded(s) = &mut h.state_mut().files[0] else {
        unreachable!()
    };
    s.undo(None);
    assert_eq!(*s.edit(), before, "one undo restores the original");
    assert!(!s.history.is_dirty());
}

#[test]
fn double_clicking_a_label_types_an_exact_position() {
    let mut h = looped_harness_with_dt(0.05);
    let before = session(&h).edit().clone();

    let at = flag_pos(&h, Marker::LoopEnd);
    double_click(&mut h, at);
    h.run();
    assert!(
        h.get_by_label("Loop end position").is_focused(),
        "the field takes the focus when it opens"
    );
    h.get_by_label("Loop end position").type_text("4000");
    h.run();
    h.key_press(Key::Enter);
    h.run();

    let s = session(&h);
    assert_eq!(s.edit().loop_end, 4000, "no snapping on typed positions");
    assert_eq!(s.edit().loop_start, before.loop_start);
    assert!(h.state().ui.marker_entry.is_none(), "the field closed");
    let FileEntry::Loaded(s) = &mut h.state_mut().files[0] else {
        unreachable!()
    };
    s.undo(None);
    assert_eq!(*s.edit(), before, "one undo restores the original");
    assert!(!s.history.is_dirty());
}

#[test]
fn escape_and_bad_text_leave_a_typed_position_unchanged() {
    let mut h = looped_harness_with_dt(0.05);
    let before = session(&h).edit().clone();

    let at = flag_pos(&h, Marker::LoopEnd);
    double_click(&mut h, at);
    h.run();
    h.get_by_label("Loop end position").type_text("4000");
    h.run();
    h.key_press(Key::Escape);
    h.run();
    assert_eq!(*session(&h).edit(), before);
    assert!(!session(&h).history.is_dirty());
    assert!(h.state().ui.marker_entry.is_none(), "Escape closed it");

    let at = flag_pos(&h, Marker::Start);
    double_click(&mut h, at);
    h.run();
    h.get_by_label("Start position").type_text("abc");
    h.run();
    h.key_press(Key::Enter);
    h.run();
    assert_eq!(*session(&h).edit(), before);
    assert!(!session(&h).history.is_dirty());
    assert!(h.state().ui.marker_entry.is_none(), "bad text closed it");
}

#[test]
fn wheel_zooms_around_the_cursor() {
    let mut h = looped_harness();
    let rect = waveform_rect(&h);
    let at = pos2(rect.left() + 700.0, rect.center().y);
    let before_view = session(&h).view.unwrap();
    let under_cursor = before_view.x_to_frame(at.x, rect.left());

    h.hover_at(at);
    h.step();
    h.event(Event::MouseWheel {
        unit: MouseWheelUnit::Line,
        delta: eframe::egui::vec2(0.0, 2.0),
        phase: TouchPhase::Move,
        source: MouseWheelSource::Wheel,
        modifiers: Modifiers::NONE,
    });
    h.run();

    let after_view = session(&h).view.unwrap();
    assert!(
        after_view.frames_per_px < before_view.frames_per_px,
        "wheel up zooms in"
    );
    let still = after_view.x_to_frame(at.x, rect.left());
    assert!(
        (still - under_cursor).abs() <= after_view.frames_per_px * 2.0,
        "frame under the cursor moved from {under_cursor} to {still}"
    );
}

#[test]
fn dragging_the_overview_box_scrolls_the_view() {
    let mut h = looped_harness();
    let overview = h.get_by_label("Overview").rect();
    let waveform = waveform_rect(&h);
    // Zoom in 4x on the left edge so the box covers a quarter of the strip.
    {
        let FileEntry::Loaded(s) = &mut h.state_mut().files[0] else {
            unreachable!()
        };
        let mut view = s.view.unwrap();
        view.zoom_around(
            waveform.left(),
            waveform.left(),
            4.0,
            s.doc.frames,
            waveform.width(),
        );
        s.view = Some(view);
    }
    h.run();
    assert_eq!(session(&h).view.unwrap().first, 0.0);

    let from = pos2(overview.left() + 20.0, overview.center().y);
    drag(&mut h, from, from + eframe::egui::vec2(200.0, 0.0));
    let view = session(&h).view.unwrap();
    let expected = 200.0 / f64::from(overview.width()) * 8000.0;
    assert!(
        (view.first - expected).abs() < 8000.0 * 0.02,
        "first = {}, expected about {expected}",
        view.first
    );
    assert!(!session(&h).history.is_dirty(), "scrolling is not an edit");
}

#[test]
fn opening_a_long_stereo_file_renders_quickly() {
    let t = Instant::now();
    let frames = 60 * 48_000;
    let left = sine(220.0, 48_000, frames, 0.6);
    let right = sine(331.0, 48_000, frames, 0.4);
    let bytes = wav_bytes(
        &[left, right],
        48_000,
        SampleFormat::Int { bits: 24 },
        Vec::new(),
    );
    let generated = t.elapsed();

    let t = Instant::now();
    let session = Session::new(load_wav_bytes(PathBuf::from("long.wav"), &bytes).unwrap());
    let loaded = t.elapsed();

    let mut app = LimewaveApp::for_test();
    app.files = vec![FileEntry::Loaded(Box::new(session))];
    app.select(0);
    let mut h = Harness::builder()
        .with_size(eframe::egui::vec2(1100.0, 700.0))
        // The seam readout uses an arrow that the test font set lacks.
        .allow_missing_glyphs()
        .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app);
    let t = Instant::now();
    h.step();
    let first_frame = t.elapsed();
    eprintln!(
        "60 s 48 kHz stereo 24-bit: generate {generated:?}, load+peaks {loaded:?}, first frame {first_frame:?}"
    );
    assert_eq!(session_frames(&h), frames);
    h.get_by_label("Waveform");
    assert!(
        first_frame.as_secs_f64() < 2.0,
        "first frame took {first_frame:?}"
    );
}

#[test]
fn applying_an_effect_to_a_long_file_is_quick() {
    let frames = 60 * 48_000;
    let bytes = wav_bytes(
        &[
            sine(220.0, 48_000, frames, 0.6),
            sine(331.0, 48_000, frames, 0.4),
        ],
        48_000,
        SampleFormat::Int { bits: 24 },
        Vec::new(),
    );
    let mut session = Session::new(load_wav_bytes(PathBuf::from("long.wav"), &bytes).unwrap());

    let t = Instant::now();
    assert!(session.apply_effect(Effect::Gain { db: -6.0 }, None, None));
    let applied = t.elapsed();
    eprintln!("60 s 48 kHz stereo 24-bit: apply gain {applied:?}");
    assert!(applied.as_secs_f64() < 2.0, "applying took {applied:?}");
}

fn session_frames(h: &Harness<'static, LimewaveApp>) -> usize {
    session(h).doc.frames
}
