//! Waveform and overview views driven through the whole app with synthetic pointer input.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{
    Event, Key, Modifiers, MouseWheelSource, MouseWheelUnit, PointerButton, Pos2, Rect, TouchPhase,
    pos2,
};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave::ui::waveform::{flag_hit_point, sample_point};
use limewave_core::document::load_wav_bytes;
use limewave_core::dsp::{Crossing, is_crossing, is_upward};
use limewave_core::edit::{LoopMode, Marker};
use limewave_core::effects::{AppliedEffect, Effect, Op, Splice, SpliceKind};
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
    looped_harness_sized(eframe::egui::vec2(1100.0, 700.0), step_dt)
}

/// `looped_harness` in a window of `size`, with `step_dt` seconds between frames.
fn looped_harness_sized(size: eframe::egui::Vec2, step_dt: f32) -> Harness<'static, LimewaveApp> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loop.wav");
    std::fs::write(&path, looped_bytes()).unwrap();
    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    let mut harness = Harness::builder()
        .with_size(size)
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
fn backward_drag_snaps_to_downward_crossings() {
    let mut h = looped_harness();
    h.get_by_label("Backward").click();
    h.run();
    assert_eq!(session(&h).edit().loop_mode, LoopMode::Backward);

    let from = frame_pos(&h, LOOP_END as usize);
    drag(&mut h, from, from + eframe::egui::vec2(-60.0, 0.0));

    let s = session(&h);
    let moved = s.edit().loop_end;
    assert!(
        moved < LOOP_END as usize - 100,
        "the marker followed the drag"
    );
    assert!(
        is_crossing(&s.mono, moved + 1, Crossing::Downward),
        "loop end + 1 = {} is not a downward zero crossing",
        moved + 1
    );
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
    assert!(
        h.state().ui.selection.is_some(),
        "the drag selected instead"
    );
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

/// Sets the view to show `visible` frames from `first`, measured against the overview strip.
fn set_view(h: &mut Harness<'static, LimewaveApp>, first: f64, visible: f64) {
    let width = h.get_by_label("Overview").rect().width();
    let FileEntry::Loaded(s) = &mut h.state_mut().files[0] else {
        unreachable!()
    };
    let mut view = s.view.unwrap();
    view.first = first;
    view.frames_per_px = visible / f64::from(width);
    s.view = Some(view);
    h.run();
}

/// The overview strip's x for `frame` of the 8000-frame test file.
fn overview_x(h: &Harness<'static, LimewaveApp>, frame: f64) -> f32 {
    let overview = h.get_by_label("Overview").rect();
    overview.left() + (frame / 8000.0 * f64::from(overview.width())) as f32
}

/// Frames the view shows across the overview strip.
fn visible_frames(h: &Harness<'static, LimewaveApp>) -> f64 {
    let width = h.get_by_label("Overview").rect().width();
    session(h).view.unwrap().frames_per_px * f64::from(width)
}

#[test]
fn dragging_the_overview_box_right_edge_zooms_with_the_left_end_fixed() {
    let mut h = looped_harness();
    set_view(&mut h, 0.0, 2000.0);
    let y = h.get_by_label("Overview").rect().center().y;
    let from = pos2(overview_x(&h, 2000.0) - 1.0, y);
    let to = pos2(overview_x(&h, 4000.0) - 1.0, y);
    drag(&mut h, from, to);
    let view = session(&h).view.unwrap();
    assert_eq!(view.first, 0.0, "the left end stays put");
    let visible = visible_frames(&h);
    assert!(
        (visible - 4000.0).abs() < 8000.0 * 0.02,
        "visible = {visible}, expected about 4000"
    );
    assert!(!session(&h).history.is_dirty(), "zooming is not an edit");
}

#[test]
fn dragging_the_overview_box_left_edge_zooms_with_the_right_end_fixed() {
    let mut h = looped_harness();
    set_view(&mut h, 2000.0, 2000.0);
    let y = h.get_by_label("Overview").rect().center().y;
    let from = pos2(overview_x(&h, 2000.0) + 1.0, y);
    let to = pos2(overview_x(&h, 1000.0) + 1.0, y);
    drag(&mut h, from, to);
    let view = session(&h).view.unwrap();
    let right = view.first + visible_frames(&h);
    assert!(
        (view.first - 1000.0).abs() < 8000.0 * 0.02,
        "first = {}, expected about 1000",
        view.first
    );
    assert!(
        (right - 4000.0).abs() < 8000.0 * 0.02,
        "right end = {right}, expected about 4000"
    );
}

#[test]
fn an_edge_dragged_past_the_other_stops_at_the_closest_zoom() {
    let mut h = looped_harness();
    set_view(&mut h, 2000.0, 2000.0);
    let y = h.get_by_label("Overview").rect().center().y;
    let from = pos2(overview_x(&h, 4000.0) - 1.0, y);
    let to = pos2(overview_x(&h, 500.0), y);
    drag(&mut h, from, to);
    let view = session(&h).view.unwrap();
    assert!(
        (view.first - 2000.0).abs() < 1.0,
        "the left end stays put, first = {}",
        view.first
    );
    assert!(view.frames_per_px >= limewave::ui::coords::MIN_FRAMES_PER_PX);
    assert!(visible_frames(&h) < 200.0, "zoomed right in");
}

/// Zooms all the way in on `frame`, so the samples show as dots.
fn zoom_in_on(h: &mut Harness<'static, LimewaveApp>, frame: usize) {
    let FileEntry::Loaded(s) = &mut h.state_mut().files[0] else {
        unreachable!()
    };
    let mut view = s.view.unwrap();
    view.frames_per_px = 1.0 / 16.0;
    view.first = frame as f64 - 20.0;
    s.view = Some(view);
    h.run();
}

/// Where sample `frame` of the mono test file is drawn.
fn dot(h: &Harness<'static, LimewaveApp>, frame: usize) -> Pos2 {
    let s = session(h);
    sample_point(waveform_rect(h), &s.view.unwrap(), &s.processed, 0, frame)
}

/// The frames of the processed audio that differ from the file as opened.
fn changed_frames(h: &Harness<'static, LimewaveApp>) -> Vec<usize> {
    let s = session(h);
    (0..s.doc.audio[0].len())
        .filter(|&f| s.processed[0][f] != s.doc.audio[0][f])
        .collect()
}

#[test]
fn dragging_a_sample_dot_moves_that_sample_as_one_undo_step() {
    let mut h = looped_harness();
    zoom_in_on(&mut h, 2000);
    let from = dot(&h, 2000);
    let to = from + eframe::egui::vec2(0.0, -30.0);
    drag(&mut h, from, to);
    assert_eq!(changed_frames(&h), vec![2000]);
    let at = dot(&h, 2000);
    assert!(
        (at.y - to.y).abs() < 0.5,
        "the sample follows the pointer: {at:?}"
    );
    assert!(h.state().ui.selection.is_none(), "drawing does not select");

    h.state_mut().undo();
    h.run();
    assert!(changed_frames(&h).is_empty(), "one undo puts it back");
    assert!(!session(&h).history.is_dirty());
}

#[test]
fn a_sideways_pencil_drag_redraws_the_samples_it_passes() {
    let mut h = looped_harness();
    zoom_in_on(&mut h, 2000);
    let from = dot(&h, 2000);
    let to = pos2(dot(&h, 2010).x, from.y - 40.0);
    drag(&mut h, from, to);
    // The line starts on the grabbed sample, which stays where it was.
    assert_eq!(changed_frames(&h), (2001..=2010).collect::<Vec<_>>());
    let ys: Vec<f32> = (2000..=2010).map(|f| dot(&h, f).y).collect();
    assert!(
        ys.windows(2).all(|w| w[1] < w[0]),
        "a straight rising line: {ys:?}"
    );
    assert!((ys[10] - to.y).abs() < 0.5);
    // The stroke is one undo step.
    h.state_mut().undo();
    h.run();
    assert!(changed_frames(&h).is_empty());
}

#[test]
fn the_pencil_stops_at_full_scale_and_shift_draws_finely() {
    let mut h = looped_harness();
    zoom_in_on(&mut h, 2000);
    let from = dot(&h, 2000);
    let above = pos2(from.x, waveform_rect(&h).top() - 200.0);
    drag(&mut h, from, above);
    assert_eq!(session(&h).processed[0][2000], 1.0);

    let from = dot(&h, 2005);
    h.event(Event::ModifiersChanged(Modifiers::SHIFT));
    h.step();
    drag(&mut h, from, from + eframe::egui::vec2(0.0, 50.0));
    h.event(Event::ModifiersChanged(Modifiers::NONE));
    h.step();
    let moved = dot(&h, 2005).y - from.y;
    assert!(
        (moved - 5.0).abs() < 0.5,
        "a tenth of the pointer's 50 points: {moved}"
    );
}

#[test]
fn pressing_away_from_the_samples_still_selects_when_zoomed_in() {
    let mut h = looped_harness();
    zoom_in_on(&mut h, 2000);
    // Above the dot: below it the lane, a toolbar row shorter than it once was, would end.
    let from = dot(&h, 2000) + eframe::egui::vec2(0.0, -60.0);
    drag(&mut h, from, from + eframe::egui::vec2(80.0, 0.0));
    assert!(changed_frames(&h).is_empty());
    assert!(h.state().ui.selection.is_some());
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

/// An app showing a 1000-frame mono sine after a tail splice that makes it 1500 frames long.
fn tail_harness() -> Harness<'static, LimewaveApp> {
    let bytes = wav_bytes(
        &[sine(220.0, 44100, 1000, 0.5)],
        44100,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    );
    let mut s = Session::new(load_wav_bytes(PathBuf::from("short.wav"), &bytes).unwrap());
    let audio = vec![(0..600).map(|i| (i as f32 * 0.05).sin() * 0.25).collect()];
    let mut e = s.edit().clone();
    e.effects.push(AppliedEffect {
        op: Op::Splice(Arc::new(Splice {
            kind: SpliceKind::Tail,
            audio,
            generated: (100, 599),
        })),
        first: 900,
        last: 999,
    });
    s.apply(e, None);
    assert_eq!(s.frames(), 1500);
    assert_eq!(s.edit().end, 999, "the splice leaves End where it was");

    let mut app = LimewaveApp::for_test();
    app.files = vec![FileEntry::Loaded(Box::new(s))];
    app.select(0);
    let mut h = Harness::builder()
        .with_size(eframe::egui::vec2(1100.0, 700.0))
        .with_step_dt(0.25)
        // The seam readout uses an arrow that the test font set lacks.
        .allow_missing_glyphs()
        .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app);
    h.run();
    h
}

#[test]
fn end_marker_can_be_dragged_into_the_tail() {
    let mut h = tail_harness();
    assert_eq!(session(&h).view.unwrap().first, 0.0);
    // The whole file, tail included, is in view.
    let rect = waveform_rect(&h);
    let last = session(&h)
        .view
        .unwrap()
        .x_to_frame(rect.right(), rect.left());
    assert!(last >= 1499.0, "the view ends at frame {last}");

    let from = flag_pos(&h, Marker::End);
    drag(&mut h, from, pos2(rect.right() - 1.0, from.y));
    assert_eq!(
        session(&h).edit().end,
        1499,
        "dragged to the end of the tail"
    );

    // Typed into the inspector.
    let FileEntry::Loaded(s) = &mut h.state_mut().files[0] else {
        unreachable!()
    };
    s.undo(None);
    assert_eq!(s.edit().end, 999);
    h.run();
    h.get_by_label("End").set_value_accesskit(1499.0);
    h.run();
    assert_eq!(
        session(&h).edit().end,
        1499,
        "a typed End of 1499 is accepted"
    );
    assert!(
        h.get_by_label("1500 frames").rect().width() > 0.0,
        "the label counts the tail"
    );
}

/// Turns Draw mode on.
fn draw_on(h: &mut Harness<'static, LimewaveApp>) {
    h.state_mut().ui.draw = true;
    h.run();
}

/// The sample value (held to full scale) a drawing pointer at height `y` of the mono lane means.
fn value_at(h: &Harness<'static, LimewaveApp>, y: f32) -> f32 {
    let s = session(h);
    let (rect, view) = (waveform_rect(h), s.view.unwrap());
    let centre = sample_point(rect, &view, &[vec![0.0]], 0, 0).y;
    let top = sample_point(rect, &view, &[vec![1.0]], 0, 0).y;
    ((centre - y) / (centre - top)).clamp(-1.0, 1.0)
}

/// The height of the mono lane's zero line.
fn lane_centre(h: &Harness<'static, LimewaveApp>) -> f32 {
    let s = session(h);
    sample_point(waveform_rect(h), &s.view.unwrap(), &[vec![0.0]], 0, 0).y
}

#[test]
fn draw_mode_fills_every_frame_under_a_zoomed_out_stroke() {
    let mut h = looped_harness();
    draw_on(&mut h);
    let view = session(&h).view.unwrap();
    assert!(view.frames_per_px > 1.0, "zoomed out");
    let left = waveform_rect(&h).left();
    let centre = lane_centre(&h);
    let from = pos2(frame_pos(&h, 1500).x, centre - 20.0);
    let to = pos2(frame_pos(&h, 3500).x, centre + 20.0);
    drag(&mut h, from, to);

    let first = view.x_to_frame(from.x, left).round() as usize;
    let last = view.x_to_frame(to.x, left).round() as usize;
    assert_eq!(changed_frames(&h), (first..=last).collect::<Vec<_>>());
    let values: Vec<f32> = (first..=last)
        .map(|f| session(&h).processed[0][f])
        .collect();
    assert!(
        values.windows(2).all(|w| w[1] <= w[0] + 1e-6),
        "a falling line"
    );
    assert!((values[0] - value_at(&h, from.y)).abs() < 0.02);
    assert!((values[values.len() - 1] - value_at(&h, to.y)).abs() < 0.02);
    assert!(h.state().ui.selection.is_none(), "drawing does not select");

    h.state_mut().undo();
    h.run();
    assert!(changed_frames(&h).is_empty(), "one undo puts it back");
}

#[test]
fn draw_mode_starts_at_the_pointer_not_the_sample() {
    let mut h = looped_harness();
    zoom_in_on(&mut h, 2000);
    draw_on(&mut h);
    let from = dot(&h, 2000) + eframe::egui::vec2(0.0, -40.0);
    let to = pos2(dot(&h, 2001).x, from.y);
    drag(&mut h, from, to);
    let at = dot(&h, 2000);
    assert!(
        (at.y - from.y).abs() < 0.5,
        "the sample jumps to the press height: {at:?} vs {from:?}"
    );
}

#[test]
fn a_click_in_draw_mode_changes_no_audio() {
    let mut h = looped_harness();
    draw_on(&mut h);
    let at = frame_pos(&h, 3000);
    h.hover_at(at);
    h.step();
    for pressed in [true, false] {
        h.event(Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        });
        h.step();
    }
    assert!(changed_frames(&h).is_empty());
    assert_eq!(h.state().ui.play_from, Some(3000));
}

#[test]
fn a_draw_press_outside_every_lane_starts_nothing() {
    let mut h = looped_harness();
    draw_on(&mut h);
    let before = session(&h).edit().clone();
    // On the time ruler, above the lanes and the marker flags, away from every marker line.
    let y = waveform_rect(&h).top() + 8.0;
    let from = pos2(frame_pos(&h, 3000).x, y);
    let to = pos2(frame_pos(&h, 3600).x, y + 60.0);
    drag(&mut h, from, to);
    assert!(changed_frames(&h).is_empty(), "no audio changes");
    assert_eq!(h.state().ui.selection, None, "no selection");
    assert_eq!(session(&h).edit(), &before, "no marker moves");
    assert!(!session(&h).history.is_dirty(), "no history entry");
}

#[test]
fn markers_still_drag_in_draw_mode() {
    let mut h = looped_harness();
    draw_on(&mut h);
    let from = flag_pos(&h, Marker::LoopEnd);
    drag(&mut h, from, from + eframe::egui::vec2(-60.0, 0.0));
    assert!(session(&h).edit().loop_end < LOOP_END as usize - 100);
    assert!(changed_frames(&h).is_empty());
}

#[test]
fn a_draw_stroke_stops_at_full_scale_and_the_file_end() {
    let mut h = looped_harness();
    draw_on(&mut h);
    let rect = waveform_rect(&h);
    // Well clear of the end marker, which a press on its line would grab.
    let from = frame_pos(&h, 7000);
    drag(&mut h, from, pos2(rect.right() + 300.0, rect.top() - 300.0));
    let s = session(&h);
    assert_eq!(s.processed[0][7999], 1.0);
    assert_eq!(s.doc.frames, 8000);
    assert_eq!(s.processed[0].len(), 8000);
}

#[test]
fn escape_turns_draw_off_first() {
    let mut h = looped_harness();
    draw_on(&mut h);
    h.state_mut().ui.selection = Some((2000, 3000));
    h.key_press(Key::Escape);
    h.run();
    assert!(!h.state().ui.draw, "Draw is off");
    assert_eq!(h.state().ui.selection, Some((2000, 3000)));
    h.key_press(Key::Escape);
    h.run();
    assert_eq!(h.state().ui.selection, None);
}

#[test]
fn a_wrapped_toolbar_costs_only_its_own_row() {
    // Before the Draw toggle the waveform was 170 points tall at this size. The toolbar may wrap
    // one row now, but its last widgets must not be squeezed into the leftover width of a row.
    let h = looped_harness();
    let height = waveform_rect(&h).height();
    assert!(height >= 140.0, "the waveform is {height} points tall");
    assert!(h.get_by_label("Audition").rect().height() < 30.0);
}

#[test]
fn the_toolbar_fits_one_row_in_a_1280_window() {
    let h = looped_harness_sized(eframe::egui::vec2(1280.0, 700.0), 0.25);
    let open = h.get_by_label("Open file…").rect();
    assert_eq!(h.get_by_label("✏ Draw").rect().top(), open.top());
    assert_eq!(h.get_by_label("Audition").rect().top(), open.top());
    assert!(waveform_rect(&h).top() < 140.0);
}
