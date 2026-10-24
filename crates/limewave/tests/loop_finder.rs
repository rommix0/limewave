//! The loop finder (v0.8): the search state on a session (a search on a worker thread, a
//! preview that leaves the history alone, Use loop as one undo step, and results that go stale
//! when the audio changes) and the panel beside the seam view. Driven through the whole app;
//! the tests wait for the worker between frames.

use eframe::egui::{Key, vec2};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use limewave::app::{FileEntry, LimewaveApp, Session};
use limewave::ui::loop_finder::{
    FinderPrefs, candidate_edit, end_preview, is_searching, is_stale, overlay_brackets,
    preview_candidate, start_search, use_candidate,
};
use limewave_core::dsp::rng::SplitMix64;
use limewave_core::edit::{EditState, LoopMode};
use limewave_core::effects::Effect;
use limewave_core::loop_finder::LoopCandidate;
use limewave_core::testwav::{sine, wav_bytes};
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
    harness_for(&[repeated_noise(1), repeated_noise(2)], RATE)
}

/// A harness showing `channels` at `rate` on their own.
fn harness_for(channels: &[Vec<f32>], rate: u32) -> Harness<'static, LimewaveApp> {
    let bytes = wav_bytes(channels, rate, SampleFormat::Int { bits: 16 }, Vec::new());
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

#[test]
fn a_new_result_ends_the_preview_of_the_old_one() {
    let mut h = harness();
    search(&mut h);
    preview_candidate(session_mut(h.state_mut()), 0, None);
    h.run();
    assert!(session(h.state()).preview.is_some());

    start(&mut h);
    settle(&mut h);
    let s = session(h.state());
    assert_eq!(s.preview, None, "the preview went with its result");
    assert_eq!(s.shown_edit(), s.edit());
    assert_eq!(s.published, *s.edit());
    assert_eq!(s.loop_finder.selected, None);
}

// The panel.

/// The row text for candidate `c` at `rank` (from 1) of a sound that is not tonal.
fn row_label(rank: usize, c: &LoopCandidate) -> String {
    format!(
        "{rank}. {:.3} s \u{b7} {} frames \u{b7} {}\u{2013}{} \u{b7} {:?}",
        c.len() as f64 / f64::from(RATE),
        c.len(),
        c.loop_start,
        c.loop_end,
        c.quality
    )
}

/// Clicks Find loops and waits for the result.
fn find_loops(h: &mut Harness<'static, LimewaveApp>) {
    h.get_by_label("Find loops").click();
    h.run();
    settle(h);
    assert!(!is_searching(session(h.state())), "the search finished");
}

/// Clicks the row of candidate `index` (from 0).
fn click_row(h: &mut Harness<'static, LimewaveApp>, index: usize) {
    let label = row_label(index + 1, &candidate(session(h.state()), index));
    h.get_by_label(&label).click();
    h.run();
}

fn use_loop_disabled(h: &Harness<'static, LimewaveApp>) -> bool {
    h.get_by_label("Use loop").accesskit_node().is_disabled()
}

#[test]
fn find_loops_lists_candidates() {
    let mut h = harness();
    find_loops(&mut h);
    assert!(
        h.query_all_by_label_contains("Seamless").count() >= 1,
        "a seamless row"
    );
    let c = candidate(session(h.state()), 0);
    let label = row_label(1, &c);
    assert!(
        label.starts_with("1. 0.700 s \u{b7} 30870 frames \u{b7} "),
        "{label}"
    );
    assert!(label.ends_with(" \u{b7} Seamless"), "{label}");
    h.get_by_label(&label);
}

#[test]
fn clicking_a_row_previews_and_escape_ends_it() {
    let mut h = harness();
    find_loops(&mut h);
    h.state_mut().ui.selection = Some((1000, 2000));
    h.run();
    let cursor = session(h.state()).history.cursor();
    let c = candidate(session(h.state()), 0);

    click_row(&mut h, 0);
    let s = session(h.state());
    assert_eq!(s.shown_edit().loop_end, c.loop_end, "the row previews");
    assert_eq!(s.shown_edit().loop_start, c.loop_start);
    assert_eq!(s.history.cursor(), cursor, "the history is untouched");

    h.key_press(Key::Escape);
    h.run();
    let s = session(h.state());
    assert_eq!(s.shown_edit(), s.edit(), "Escape ends the preview");
    assert_eq!(s.loop_finder.selected, None);
    assert_eq!(
        h.state().ui.selection,
        Some((1000, 2000)),
        "and leaves the selection"
    );
}

#[test]
fn clicking_the_selected_row_again_ends_the_preview() {
    let mut h = harness();
    find_loops(&mut h);
    click_row(&mut h, 0);
    assert_ne!(session(h.state()).shown_edit(), session(h.state()).edit());
    click_row(&mut h, 0);
    let s = session(h.state());
    assert_eq!(s.shown_edit(), s.edit());
    assert_eq!(s.loop_finder.selected, None);
}

#[test]
fn arrow_down_moves_the_preview() {
    let mut h = harness();
    find_loops(&mut h);
    let second = candidate(session(h.state()), 1);
    click_row(&mut h, 0);
    h.key_press(Key::ArrowDown);
    h.run();
    let s = session(h.state());
    assert_eq!(
        (s.shown_edit().loop_start, s.shown_edit().loop_end),
        (second.loop_start, second.loop_end),
        "the preview moved to row 2"
    );
    assert_eq!(s.loop_finder.selected, Some(1));

    h.key_press(Key::ArrowUp);
    h.run();
    assert_eq!(session(h.state()).loop_finder.selected, Some(0), "and back");
}

#[test]
fn enter_applies_the_selected_row() {
    let mut h = harness();
    find_loops(&mut h);
    let c = candidate(session(h.state()), 0);
    let cursor = session(h.state()).history.cursor();
    click_row(&mut h, 0);
    h.key_press(Key::Enter);
    h.run();
    let s = session(h.state());
    assert_eq!(
        (s.edit().loop_start, s.edit().loop_end),
        (c.loop_start, c.loop_end)
    );
    assert_eq!(s.history.cursor(), cursor + 1, "one history entry");
    assert_eq!(s.preview, None);
}

#[test]
fn use_loop_button_applies() {
    let mut h = harness();
    find_loops(&mut h);
    assert!(use_loop_disabled(&h), "no row selected yet");
    let c = candidate(session(h.state()), 0);
    click_row(&mut h, 0);
    assert!(!use_loop_disabled(&h));
    h.get_by_label("Use loop").click();
    h.run();
    let s = session(h.state());
    assert_eq!(
        (s.edit().loop_start, s.edit().loop_end),
        (c.loop_start, c.loop_end)
    );
    assert_eq!(s.edit().loop_mode, LoopMode::Forward);
    assert_eq!(s.shown_edit(), s.edit(), "no preview is left");
}

#[test]
fn a_candidate_outside_start_end_cannot_be_used() {
    let mut h = harness();
    find_loops(&mut h);
    let s = session(h.state());
    let c = candidate(s, 0);
    let moved = EditState {
        start: c.loop_start + 1,
        ..s.edit().clone()
    };
    session_mut(h.state_mut()).apply(moved, None);
    h.run();
    assert!(
        session(h.state()).edit().start > c.loop_start,
        "Start moved"
    );

    click_row(&mut h, 0);
    let s = session(h.state());
    assert_eq!(s.shown_edit(), s.edit(), "nothing previews");
    assert!(use_loop_disabled(&h), "Use loop stays disabled");
    h.get_by_label("Use loop").hover();
    h.run();
    h.get_by_label("Outside Start\u{2013}End");
}

#[test]
fn hovering_the_quality_word_shows_its_measures() {
    let mut h = harness();
    find_loops(&mut h);
    let label = row_label(1, &candidate(session(h.state()), 0));
    let row = h.get_by_label(&label).rect();
    let shown =
        |h: &Harness<'static, LimewaveApp>| h.query_by_label_contains("Waveform match").is_some();
    // The rank starts the row; the quality word comes later in it.
    h.hover_at(eframe::egui::pos2(row.left() + 6.0, row.center().y));
    h.run();
    assert!(!shown(&h), "only the quality word explains itself");
    // The text may wrap, so try each line of the row.
    let mut found = false;
    for y in [row.top() + 8.0, row.bottom() - 8.0] {
        let mut x = row.left() + 6.0;
        while x < row.right() && !found {
            h.hover_at(eframe::egui::pos2(x, y));
            h.run();
            found = shown(&h);
            x += 8.0;
        }
    }
    assert!(found, "the quality word explains itself");
    h.get_by_label("Click: none \u{b7} Tone change: none \u{b7} Waveform match: 100 %");
}

#[test]
fn ping_pong_disables_find() {
    let mut h = harness();
    let s = session(h.state());
    let looped = EditState {
        loop_mode: LoopMode::PingPong,
        loop_start: 1000,
        loop_end: 50_000,
        ..s.edit().clone()
    };
    session_mut(h.state_mut()).apply(looped, None);
    h.run();
    assert_eq!(session(h.state()).edit().loop_mode, LoopMode::PingPong);
    assert!(h.get_by_label("Find loops").accesskit_node().is_disabled());
    h.get_by_label("The finder makes forward and backward loops");
}

#[test]
fn stale_list_says_search_again() {
    let mut h = harness();
    find_loops(&mut h);
    click_row(&mut h, 0);
    assert!(session_mut(h.state_mut()).apply_effect(Effect::Gain { db: -6.0 }, None, None));
    h.run();
    h.get_by_label("The sound has changed. Search again.");
    assert!(use_loop_disabled(&h));
    let label = row_label(1, &candidate(session(h.state()), 0));
    assert!(
        h.get_by_label(&label).accesskit_node().is_disabled(),
        "the list is drawn disabled"
    );
}

#[test]
fn a_long_selection_is_refused() {
    let rate = 8_000;
    let frames = 62 * rate as usize;
    let mut h = harness_for(&[sine(220.0, rate, frames, 0.5)], rate);
    h.state_mut().ui.selection = Some((0, frames - 1));
    h.run();
    h.get_by_label("Find loops").click();
    h.run();
    settle(&mut h);
    h.get_by_label("Select 60 seconds or less");
    assert!(session(h.state()).loop_finder.result.is_none());
}

#[test]
fn cancel_stops_the_search() {
    let mut h = harness();
    h.get_by_label("Find loops").click();
    h.step();
    h.step();
    assert!(is_searching(session(h.state())), "the search runs");
    h.get_by_label("Searching\u{2026}");
    h.get_by_label("Cancel").click();
    h.step();
    settle(&mut h);
    let s = session(h.state());
    assert!(!is_searching(s), "the search stopped");
    assert!(s.loop_finder.result.is_none(), "and left no result");
    h.get_by_label("Find loops");
}

#[test]
fn the_panel_says_what_it_searched() {
    let mut h = harness();
    find_loops(&mut h);
    let range = session(h.state())
        .loop_finder
        .result
        .as_ref()
        .unwrap()
        .range;
    let text = format!(
        "Searched the steady part, {:.2}\u{2013}{:.2} s",
        range.first as f64 / f64::from(RATE),
        range.last as f64 / f64::from(RATE)
    );
    h.get_by_label(&text);

    h.state_mut().ui.selection = Some((10_000, 90_000));
    h.run();
    find_loops(&mut h);
    h.get_by_label("Searched the selection");
}

#[test]
fn end_preview_leaves_a_marker_drag_alone() {
    let mut h = harness();
    search(&mut h);
    let s = session(h.state());
    let dragged = EditState {
        end: s.edit().end - 100,
        ..s.edit().clone()
    };
    session_mut(h.state_mut()).set_preview(dragged.clone(), None);
    end_preview(session_mut(h.state_mut()), None);
    assert_eq!(
        session(h.state()).preview.as_ref(),
        Some(&dragged),
        "only a candidate preview ends"
    );
}

// The overlay on the waveform and the overview.

#[test]
fn brackets_follow_results() {
    let mut h = harness();
    assert!(
        overlay_brackets(session(h.state())).is_empty(),
        "none before a search"
    );

    search(&mut h);
    let s = session(h.state());
    let candidates = s
        .loop_finder
        .result
        .as_ref()
        .unwrap()
        .outcome
        .as_ref()
        .unwrap()
        .clone();
    assert!(!candidates.is_empty());
    let spans = overlay_brackets(s);
    assert_eq!(spans.len(), candidates.len(), "one per candidate");
    for (span, c) in spans.iter().zip(&candidates) {
        assert_eq!((span.0, span.1), (c.loop_start, c.loop_end));
    }
    assert!(spans.iter().all(|span| !span.2), "none selected yet");

    preview_candidate(session_mut(h.state_mut()), 1, None);
    h.run();
    let spans = overlay_brackets(session(h.state()));
    let flagged: Vec<usize> = spans
        .iter()
        .enumerate()
        .filter(|(_, span)| span.2)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(flagged, vec![1], "the previewed candidate is flagged");

    assert!(session_mut(h.state_mut()).apply_effect(Effect::Gain { db: -6.0 }, None, None));
    h.run();
    assert!(
        overlay_brackets(session(h.state())).is_empty(),
        "none once the result is stale"
    );
}
