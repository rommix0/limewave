//! Selection, the play-from point and the Edit menu, driven through the whole app.

use eframe::egui::{Event, Key, Modifiers, PointerButton, Pos2, Rect, pos2, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave::ui::save_dialog::{SaveRequest, perform_save};
use limewave::ui::waveform::flag_hit_point;
use limewave_core::dsp::is_upward;
use limewave_core::edit::{LoopMode, Marker};
use limewave_core::riff::Chunk;
use limewave_core::save::Destination;
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

/// An app showing the WAV file `bytes` on its own (no file list).
fn app_with(bytes: &[u8]) -> LimewaveApp {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.wav");
    std::fs::write(&path, bytes).unwrap();
    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    app
}

/// A harness around the looped file, after one frame.
fn looped_harness() -> Harness<'static, LimewaveApp> {
    let mut harness = Harness::builder()
        .with_size(vec2(1100.0, 700.0))
        .with_step_dt(0.25)
        // The seam readout uses an arrow that the test font set lacks.
        .allow_missing_glyphs()
        .build_ui_state(
            |ui, app: &mut LimewaveApp| app.show(ui),
            app_with(&looped_bytes()),
        );
    harness.run();
    harness
}

fn session(app: &LimewaveApp) -> &Session {
    let FileEntry::Loaded(s) = &app.files[0] else {
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
    let view = session(h.state())
        .view
        .expect("the view is set after a frame");
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

/// A quick press and release at `at`.
fn click(h: &mut Harness<'static, LimewaveApp>, at: Pos2) {
    h.hover_at(at);
    h.step();
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

#[test]
fn drag_on_empty_space_selects_and_click_sets_play_from() {
    let mut h = looped_harness();
    let from = frame_pos(&h, 3000);
    drag(&mut h, from, from + vec2(80.0, 0.0));

    let Some((a, b)) = h.state().ui.selection else {
        panic!("the drag made no selection");
    };
    assert!(a < b, "{a}..{b}");
    assert!(
        !session(h.state()).history.is_dirty(),
        "a selection is not an edit"
    );
    assert!(session(h.state()).preview.is_none());

    let at = frame_pos(&h, 2000);
    click(&mut h, at);
    let from = h
        .state()
        .ui
        .play_from
        .expect("the click set the play-from point");
    assert!(from.abs_diff(2000) <= 2, "play-from {from}");
    assert_eq!(h.state().ui.selection, None, "a click clears the selection");
    assert!(!session(h.state()).history.is_dirty());
}

#[test]
fn clicking_a_marker_does_not_set_the_play_from_point() {
    let mut h = looped_harness();
    let at = frame_pos(&h, LOOP_END as usize);
    click(&mut h, at);
    assert_eq!(h.state().ui.play_from, None);
}

#[test]
fn shift_click_extends_the_selection() {
    let mut h = looped_harness();
    h.state_mut().ui.selection = Some((2000, 3000));
    let at = frame_pos(&h, 4000);
    h.event(Event::ModifiersChanged(Modifiers::SHIFT));
    h.step();
    click(&mut h, at);
    let (a, b) = h.state().ui.selection.unwrap();
    assert_eq!(a, 2000);
    assert!(b.abs_diff(4000) <= 2, "{b}");

    // With no selection, the play-from point and the clicked frame make one.
    h.state_mut().ui.selection = None;
    h.state_mut().ui.play_from = Some(1500);
    let at = frame_pos(&h, 500);
    click(&mut h, at);
    let (a, b) = h.state().ui.selection.unwrap();
    assert!(a.abs_diff(500) <= 2, "{a}");
    assert_eq!(b, 1500);
    h.event(Event::ModifiersChanged(Modifiers::NONE));
    h.step();
}

#[test]
fn selection_edges_snap_when_snap_is_on() {
    let mut h = looped_harness();
    assert!(h.state().ui.snap);
    let from = frame_pos(&h, 3000);
    drag(&mut h, from, from + vec2(80.0, 0.0));

    let (a, b) = h.state().ui.selection.unwrap();
    let mono = &session(h.state()).mono;
    assert!(is_upward(mono, a), "first edge {a}");
    assert!(is_upward(mono, b + 1), "last edge {b}");
}

#[test]
fn dragging_right_to_left_gives_an_ordered_clamped_selection() {
    let mut h = looped_harness();
    let rect = waveform_rect(&h);
    let from = frame_pos(&h, 6000);
    drag(&mut h, from, pos2(rect.left() - 40.0, from.y));

    let (first, last) = h.state().ui.selection.unwrap();
    assert!(first <= last);
    assert_eq!(first, 0, "clamped to the start of the file");
    assert!(last.abs_diff(6000) <= 100, "last {last}");
}

#[test]
fn set_loop_to_selection_is_one_undo_step() {
    let mut app = app_with(&looped_bytes());
    let before = session(&app).edit().clone();
    app.ui.selection = Some((2000, 3999));
    app.set_loop_to_selection();

    let e = session(&app).edit();
    assert_eq!((e.loop_start, e.loop_end), (2000, 3999));
    assert_eq!(e.loop_mode, LoopMode::Forward);
    assert!(session(&app).history.is_dirty());

    app.undo();
    let e = session(&app).edit();
    assert_eq!((e.loop_start, e.loop_end), (1000, 5000));
    assert_eq!(*e, before);
    assert!(!session(&app).history.is_dirty());
}

#[test]
fn set_trim_to_selection_refuses_and_says_why() {
    let mut app = app_with(&looped_bytes());
    let before = session(&app).edit().clone();
    app.ui.selection = Some((2000, 3000));
    app.set_trim_to_selection();

    assert_eq!(*session(&app).edit(), before);
    assert!(!session(&app).history.is_dirty());
    let message = app.ui.message.as_deref().expect("a reason is shown");
    assert!(message.contains("loop"), "{message}");

    // Around the loop it works, as one undo step, and the message is gone.
    app.ui.selection = Some((500, 6000));
    app.set_trim_to_selection();
    let e = session(&app).edit();
    assert_eq!((e.start, e.end), (500, 6000));
    assert_eq!(app.ui.message, None);
    app.undo();
    assert_eq!(*session(&app).edit(), before);
}

#[test]
fn edit_menu_crop_to_loop_uses_the_padding() {
    let mut h = looped_harness();
    h.get_by_label("Edit").click();
    h.run();
    h.get_by_label("Crop to loop").click();
    h.run();

    let e = session(h.state()).edit();
    assert_eq!(e.start, 1000);
    assert_eq!(e.end, 5016);
    assert!(session(h.state()).history.is_dirty());

    // Having used the menu leaves the shortcuts working.
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run();
    assert_eq!(h.state().ui.selection, Some((1000, 5016)));
}

#[test]
fn edit_menu_selection_items_need_a_selection() {
    let mut h = looped_harness();
    h.get_by_label("Edit").click();
    h.run();
    assert!(
        h.get_by_label("Set loop to selection")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        h.get_by_label("Set Start/End to selection")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        h.get_by_label("Play selection")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        !h.get_by_label("Crop to loop")
            .accesskit_node()
            .is_disabled()
    );

    h.state_mut().ui.selection = Some((2000, 3000));
    h.run();
    h.get_by_label("Set loop to selection").click();
    h.run();
    let e = session(h.state()).edit();
    assert_eq!((e.loop_start, e.loop_end), (2000, 3000));
}

#[test]
fn select_all_and_escape() {
    let mut h = looped_harness();
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run();
    let e = session(h.state()).edit();
    assert_eq!(h.state().ui.selection, Some((e.start, e.end)));

    h.state_mut().ui.play_from = Some(1234);
    h.key_press(Key::Escape);
    h.run();
    assert_eq!(h.state().ui.selection, None);
    assert_eq!(h.state().ui.play_from, None);
}

#[test]
fn play_starts_from_the_play_from_point() {
    let mut app = app_with(&looped_bytes());
    let start = session(&app).edit().start;
    assert_eq!(app.play_start(false), Some(start));
    app.ui.play_from = Some(2000);
    assert_eq!(app.play_start(false), Some(2000));
    assert_eq!(app.play_start(true), Some(LOOP_START as usize));
}

#[test]
fn auto_trim_silence_moves_start_and_end() {
    let mut audio = vec![0.0; 1000];
    audio.extend(sine(440.0, 44100, 4000, 0.5));
    audio.extend(vec![0.0; 1000]);
    let mut app = app_with(&wav_bytes(
        &[audio],
        44100,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    ));
    app.auto_trim_silence(-60.0);

    let e = session(&app).edit();
    assert!(e.start.abs_diff(1000) <= 1, "start {}", e.start);
    assert!(e.end.abs_diff(4999) <= 1, "end {}", e.end);
    app.undo();
    assert!(!session(&app).history.is_dirty(), "one undo step");
    assert_eq!(session(&app).edit().start, 0);
}

#[test]
fn switching_files_clears_the_selection_and_play_from_point() {
    let mut app = app_with(&looped_bytes());
    app.ui.selection = Some((10, 20));
    app.ui.play_from = Some(5);
    app.select(0);
    assert_eq!(app.ui.selection, None);
    assert_eq!(app.ui.play_from, None);
}

#[test]
fn shortcuts_leave_a_focused_text_field_alone() {
    let mut h = Harness::builder()
        .with_size(vec2(1100.0, 700.0))
        .with_step_dt(0.05)
        .allow_missing_glyphs()
        .build_ui_state(
            |ui, app: &mut LimewaveApp| app.show(ui),
            app_with(&looped_bytes()),
        );
    h.run();
    h.state_mut().ui.play_from = Some(1234);

    // Double-clicking a label opens the position field, which takes the focus.
    let s = session(h.state());
    let at = flag_hit_point(
        waveform_rect(&h),
        &s.view.unwrap(),
        s.edit(),
        Marker::LoopEnd,
    );
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
    h.run();
    assert!(h.get_by_label("Loop end position").is_focused());

    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run();
    assert_eq!(h.state().ui.selection, None, "Ctrl+A selects the text");
    h.key_press(Key::Escape);
    h.run();
    assert_eq!(
        h.state().ui.play_from,
        Some(1234),
        "Escape only closes the field"
    );
}

#[test]
fn saving_the_file_clears_the_selection_and_play_from_point() {
    let mut app = app_with(&looped_bytes());
    app.ui.selection = Some((10, 20));
    app.ui.play_from = Some(5);
    let out = tempfile::tempdir().unwrap();
    let req = SaveRequest {
        indices: vec![0],
        destination: Destination::Folder(out.path().to_path_buf()),
        trim_after_loop: Some(16),
    };
    let results = perform_save(&mut app, req);
    assert!(results.iter().all(|(_, r)| r.is_ok()), "{results:?}");
    assert_eq!(app.ui.selection, None);
    assert_eq!(app.ui.play_from, None);
}
