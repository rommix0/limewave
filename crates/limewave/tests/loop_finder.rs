//! The loop finder's search state on a session (v0.8): a search on a worker thread, a preview
//! that leaves the history alone, Use loop as one undo step, and results that go stale when
//! the audio changes. Driven through the whole app; the tests wait for the worker between
//! frames.

use eframe::egui::vec2;
use egui_kittest::Harness;
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave::ui::loop_finder::{
    FinderPrefs, candidate_edit, end_preview, is_searching, is_stale, preview_candidate,
    start_search, use_candidate,
};
use limewave_core::dsp::rng::SplitMix64;
use limewave_core::edit::{EditState, LoopMode};
use limewave_core::effects::Effect;
use limewave_core::loop_finder::LoopCandidate;
use limewave_core::testwav::wav_bytes;
use limewave_core::wav::SampleFormat;

const RATE: u32 = 44_100;
/// 0.7 s: the length of the repeating tile.
const TILE: usize = 30_870;

/// `TILE` frames of seeded noise, repeated sample for sample to 3 s.
fn repeated_noise(seed: u64) -> Vec<f32> {
    let mut r = SplitMix64::new(seed);
    let tile: Vec<f32> = (0..TILE)
        .map(|_| (r.next_f32() * 2.0 - 1.0) * 0.5)
        .collect();
    tile.iter()
        .cycle()
        .take(3 * RATE as usize)
        .copied()
        .collect()
}

/// A harness showing 3 s of stereo noise that repeats exactly every `TILE` frames, a
/// different tile per channel.
fn harness() -> Harness<'static, LimewaveApp> {
    let bytes = wav_bytes(
        &[repeated_noise(1), repeated_noise(2)],
        RATE,
        SampleFormat::Int { bits: 16 },
        Vec::new(),
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repeat.wav");
    std::fs::write(&path, bytes).unwrap();
    let mut app = LimewaveApp::for_test();
    app.open_file(path);
    let mut h = Harness::builder()
        .with_size(vec2(1100.0, 860.0))
        .with_step_dt(0.25)
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

fn session_mut(app: &mut LimewaveApp) -> &mut Session {
    let FileEntry::Loaded(s) = &mut app.files[0] else {
        panic!("file not loaded");
    };
    s
}

/// Lets the search finish and the next frames see it.
fn settle(h: &mut Harness<'static, LimewaveApp>) {
    for _ in 0..2 {
        h.state_mut().wait_for_renders();
        h.run();
    }
}

/// Starts a search on the whole steady part with the default settings.
fn start(h: &mut Harness<'static, LimewaveApp>) {
    let key = h.state().render_key(0).unwrap();
    let ctx = h.ctx.clone();
    start_search(
        session_mut(h.state_mut()),
        &FinderPrefs::default(),
        None,
        key,
        &ctx,
    );
    assert!(is_searching(session(h.state())), "the search runs");
}

/// Searches and waits for the result.
fn search(h: &mut Harness<'static, LimewaveApp>) {
    start(h);
    settle(h);
    assert!(!is_searching(session(h.state())), "the search finished");
}

/// The candidate at `index` of the last result.
fn candidate(s: &Session, index: usize) -> LoopCandidate {
    let result = s.loop_finder.result.as_ref().expect("a result");
    result.outcome.as_ref().expect("candidates")[index].clone()
}

#[test]
fn use_candidate_is_one_undo_step() {
    let mut h = harness();
    search(&mut h);
    let s = session(h.state());
    let before: EditState = s.edit().clone();
    assert_eq!(before.loop_mode, LoopMode::Off);
    let cursor = s.history.cursor();
    let c = candidate(s, 0);

    use_candidate(session_mut(h.state_mut()), 0, None);
    h.run();
    let s = session(h.state());
    assert_eq!(s.edit().loop_mode, LoopMode::Forward, "Off turns Forward");
    assert_eq!(
        (s.edit().loop_start, s.edit().loop_end),
        (c.loop_start, c.loop_end)
    );
    assert_eq!(s.history.cursor(), cursor + 1, "one history entry");
    assert_eq!(s.preview, None);

    session_mut(h.state_mut()).undo(None);
    h.run();
    assert_eq!(*session(h.state()).edit(), before, "undo restores the loop");
}

#[test]
fn preview_does_not_touch_history() {
    let mut h = harness();
    search(&mut h);
    let s = session(h.state());
    let cursor = s.history.cursor();
    let c = candidate(s, 0);

    preview_candidate(session_mut(h.state_mut()), 0, None);
    h.run();
    let s = session(h.state());
    assert_eq!(s.loop_finder.selected, Some(0), "the row stays selected");
    assert_ne!(s.shown_edit(), s.edit(), "the preview shows");
    assert_eq!(
        (s.shown_edit().loop_start, s.shown_edit().loop_end),
        (c.loop_start, c.loop_end)
    );
    assert_eq!(s.published, *s.shown_edit(), "and plays");
    assert_eq!(s.history.cursor(), cursor, "the history is untouched");

    end_preview(session_mut(h.state_mut()), None);
    h.run();
    let s = session(h.state());
    assert_eq!(s.shown_edit(), s.edit());
    assert_eq!(s.published, *s.edit());
    assert_eq!(s.loop_finder.selected, None);
    assert_eq!(s.history.cursor(), cursor);
}

#[test]
fn an_effect_makes_results_stale() {
    let mut h = harness();
    search(&mut h);
    assert!(!is_stale(session(h.state())));
    let c = candidate(session(h.state()), 0);
    assert!(candidate_edit(session(h.state()), &c).is_some());

    assert!(session_mut(h.state_mut()).apply_effect(Effect::Gain { db: -6.0 }, None, None));
    h.run();
    let s = session(h.state());
    assert!(is_stale(s), "the audio changed");
    assert_eq!(candidate_edit(s, &c), None, "never applied to other audio");
}

#[test]
fn a_marker_move_does_not() {
    let mut h = harness();
    search(&mut h);
    let s = session(h.state());
    let moved = EditState {
        loop_end: s.edit().loop_end - 1000,
        ..s.edit().clone()
    };
    session_mut(h.state_mut()).apply(moved, None);
    h.run();
    let s = session(h.state());
    assert!(!is_stale(s), "a marker move leaves the audio alone");
    assert!(candidate_edit(s, &candidate(s, 0)).is_some());
}

#[test]
fn undo_during_a_search_discards_the_result() {
    let mut h = harness();
    assert!(session_mut(h.state_mut()).apply_effect(Effect::Gain { db: -6.0 }, None, None));
    h.run();
    start(&mut h);
    session_mut(h.state_mut()).undo(None);
    settle(&mut h);
    let s = session(h.state());
    assert!(s.loop_finder.result.is_some(), "the result came in");
    assert!(is_stale(s), "but for audio that is gone");
    assert_eq!(candidate_edit(s, &candidate(s, 0)), None);
}

#[test]
fn undo_while_previewing_clears_the_row() {
    let mut h = harness();
    // Something to undo.
    let s = session(h.state());
    let moved = EditState {
        end: s.edit().end - 100,
        ..s.edit().clone()
    };
    session_mut(h.state_mut()).apply(moved, None);
    h.run();
    search(&mut h);
    preview_candidate(session_mut(h.state_mut()), 0, None);
    h.run();
    assert_eq!(session(h.state()).loop_finder.selected, Some(0));

    session_mut(h.state_mut()).undo(None);
    h.run();
    let s = session(h.state());
    assert_eq!(
        s.loop_finder.selected, None,
        "the row is no longer previewed"
    );
    assert_eq!(s.preview, None);
}

#[test]
fn a_candidate_outside_the_trim_is_not_offered() {
    let mut h = harness();
    search(&mut h);
    let s = session(h.state());
    let c = candidate(s, 0);
    let trimmed = EditState {
        end: c.loop_end - 1,
        ..s.edit().clone()
    };
    session_mut(h.state_mut()).apply(trimmed, None);
    h.run();
    let s = session(h.state());
    assert!(!is_stale(s));
    assert_eq!(candidate_edit(s, &c), None, "the loop would not fit");
}

#[test]
fn a_marker_drag_over_a_preview_clears_the_row_but_keeps_the_drag() {
    let mut h = harness();
    search(&mut h);
    preview_candidate(session_mut(h.state_mut()), 0, None);
    h.run();
    let s = session(h.state());
    let dragged = EditState {
        loop_end: s.shown_edit().loop_end - 10,
        ..s.shown_edit().clone()
    };
    session_mut(h.state_mut()).set_preview(dragged.clone(), None);
    h.run();
    let s = session(h.state());
    assert_eq!(s.loop_finder.selected, None, "the row no longer shows");
    assert_eq!(s.preview.as_ref(), Some(&dragged), "the drag is left alone");
}
