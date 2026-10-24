//! The loop finder (v0.8): a search for loop points on a worker thread, kept on the session it
//! was made for, with a preview that leaves the history alone and Use loop as one undo step.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui;
use limewave_core::edit::{EditState, LoopMode};
use limewave_core::loop_finder::steady::{SearchRange, search_range};
use limewave_core::loop_finder::{
    DEFAULT_MIN_SECONDS, Direction, FinderError, FinderSettings, LoopCandidate, MAX_CANDIDATES,
    find_loops,
};

use crate::app::Session;
use crate::audio::engine::AudioEngine;
use crate::render_worker::{RenderJob, RenderKey, spawn_render};

/// The finder's settings: global, not per file, and remembered between runs.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FinderPrefs {
    /// The shortest loop to propose, in seconds.
    pub min_s: f64,
    /// The longest loop to propose, in seconds, or `None` for no limit.
    pub max_s: Option<f64>,
    /// Tight ↔ Natural: 0 favours short loops, 1 long ones, 0.5 is neutral.
    pub preference: f32,
    /// Keep the loop end where it is and search for the start only.
    pub keep_end: bool,
}

impl Default for FinderPrefs {
    fn default() -> Self {
        FinderPrefs {
            min_s: DEFAULT_MIN_SECONDS,
            max_s: None,
            preference: 0.5,
            keep_end: false,
        }
    }
}

impl FinderPrefs {
    /// The settings as stored: `"min,max,preference,keep_end"`, with `max` empty for no limit
    /// and `keep_end` as `0` or `1`.
    pub fn to_storage_string(&self) -> String {
        let max = self.max_s.map_or_else(String::new, |m| m.to_string());
        let keep_end = if self.keep_end { "1" } else { "0" };
        format!("{},{max},{},{keep_end}", self.min_s, self.preference)
    }

    /// Reads [`FinderPrefs::to_storage_string`]'s form; `None` for anything else, including
    /// lengths that are not positive and a preference outside 0..=1.
    pub fn from_storage_string(s: &str) -> Option<Self> {
        let fields: Vec<&str> = s.split(',').collect();
        let [min, max, preference, keep_end] = fields[..] else {
            return None;
        };
        let length = |f: &str| f.parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0);
        let min_s = length(min)?;
        let max_s = if max.is_empty() {
            None
        } else {
            Some(length(max)?)
        };
        let preference = preference
            .parse::<f32>()
            .ok()
            .filter(|p| (0.0..=1.0).contains(p))?;
        let keep_end = match keep_end {
            "0" => false,
            "1" => true,
            _ => return None,
        };
        Some(FinderPrefs {
            min_s,
            max_s,
            preference,
            keep_end,
        })
    }
}

/// A finished search and what it was made for.
pub struct FinderResult {
    /// The [`Session::audio_version`] searched; the result is stale once that moves on.
    pub audio_version: u64,
    /// The range searched.
    pub range: SearchRange,
    /// The way the loops play.
    pub direction: Direction,
    /// The candidates, best first, or why there are none.
    pub outcome: Result<Vec<LoopCandidate>, FinderError>,
}

/// One session's loop finder: the running search, the last result and the row previewed.
#[derive(Default)]
pub struct FinderState {
    /// The running search and its cancel flag.
    job: Option<(RenderJob<FinderResult>, Arc<AtomicBool>)>,
    /// The last finished search, kept until the next one finishes.
    pub result: Option<FinderResult>,
    /// The candidate being previewed, an index into the result's candidates.
    pub selected: Option<usize>,
    /// Why the last search could not start: no range worth searching.
    pub range_error: Option<FinderError>,
}

impl Drop for FinderState {
    /// A closed file stops its search rather than letting it run on unseen.
    fn drop(&mut self) {
        if let Some((_, cancel)) = &self.job {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// `seconds` in frames at `rate`, at least the 2 frames a loop needs.
fn frames(seconds: f64, rate: u32) -> usize {
    ((seconds * f64::from(rate)).round() as usize).max(2)
}

/// Starts a search of `s`'s processed audio with `prefs`, over `selection` or, without one,
/// the steady part, cut to the current Start...End. A running search is cancelled first. When
/// there is no range to search, `range_error` says why and nothing starts. `key` is only what
/// the worker is filed under: a result is judged by its audio version.
///
/// A live candidate preview is left alone: the caller ends it first with [`end_preview`].
pub fn start_search(
    s: &mut Session,
    prefs: &FinderPrefs,
    selection: Option<(usize, usize)>,
    key: RenderKey,
    ctx: &egui::Context,
) {
    cancel_search(s);
    s.loop_finder.range_error = None;
    let rate = s.rate();
    let e = s.edit();
    let range = match search_range(&s.processed, rate, selection, (e.start, e.end)) {
        Ok(range) => range,
        Err(err) => {
            s.loop_finder.range_error = Some(err);
            s.loop_finder.result = None;
            s.loop_finder.selected = None;
            return;
        }
    };
    // Ping-pong never gets here: the panel disables the search for it.
    let direction = if e.loop_mode == LoopMode::Backward {
        Direction::Backward
    } else {
        Direction::Forward
    };
    let keep_end = (prefs.keep_end && e.loop_mode != LoopMode::Off).then_some(e.loop_end);
    let settings = FinderSettings {
        range: (range.first, range.last),
        keep_end,
        min_len: frames(prefs.min_s, rate),
        max_len: prefs.max_s.map(|m| frames(m, rate)),
        preference: prefs.preference,
        direction,
        max_candidates: MAX_CANDIDATES,
    };
    let audio = s.processed.clone();
    let audio_version = s.audio_version;
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let job = spawn_render(key, ctx.clone(), move || FinderResult {
        audio_version,
        range,
        direction,
        outcome: find_loops(&audio, rate, &settings, &flag),
    });
    s.loop_finder.job = Some((job, cancel));
}

/// Stops the running search, if any; its result is never seen. A live candidate preview is
/// left alone: the caller ends it first with [`end_preview`].
pub fn cancel_search(s: &mut Session) {
    if let Some((_, cancel)) = s.loop_finder.job.take() {
        cancel.store(true, Ordering::Relaxed);
    }
}

/// Takes the running search's result once it has finished, or waits for it when `block`, and
/// makes it the result, with no row selected. A worker that died without a result ends the
/// search and leaves `result` as it was. A live candidate preview is left alone; see
/// [`finish_search`].
pub fn poll_search(s: &mut Session, block: bool) {
    take_result(s, block);
}

/// [`poll_search`], returning whether a new result replaced the old one.
fn take_result(s: &mut Session, block: bool) -> bool {
    let st = &mut s.loop_finder;
    let Some((job, _)) = &st.job else {
        return false;
    };
    let got = if block {
        job.wait()
    } else {
        match job.ended() {
            Some(got) => got,
            None => return false,
        }
    };
    st.job = None;
    let Some(result) = got else { return false };
    st.result = Some(result);
    st.selected = None;
    true
}

/// [`poll_search`], and when the new result replaces one whose candidate is being previewed,
/// the preview ends too ([`end_preview`]): a preview never outlives the result it came from.
pub fn finish_search(s: &mut Session, block: bool, engine: Option<&AudioEngine>) {
    let live = previewing(s);
    if take_result(s, block) && live {
        end_preview(s, engine);
    }
}

/// Whether the selected candidate is what the preview shows.
fn previewing(s: &Session) -> bool {
    let frames = s.doc.frames;
    s.loop_finder
        .selected
        .and_then(|i| indexed_edit(s, i))
        .is_some_and(|e| s.preview.as_ref() == Some(&e.clamped(frames)))
}

/// Whether a search is running.
pub fn is_searching(s: &Session) -> bool {
    s.loop_finder.job.is_some()
}

/// Whether the result was made for audio that has since changed (an effect, or an undo or
/// redo across one). A marker move does not change the audio.
pub fn is_stale(s: &Session) -> bool {
    s.loop_finder
        .result
        .as_ref()
        .is_some_and(|r| r.audio_version != s.audio_version)
}

/// The current edit with `c`'s loop points: a loop that was Off turns Forward, and the
/// crossfade is cut to what the new loop allows. `None` when there is no current result, it
/// is stale, or `c` lies outside the current Start...End.
pub fn candidate_edit(s: &Session, c: &LoopCandidate) -> Option<EditState> {
    if s.loop_finder.result.is_none() || is_stale(s) {
        return None;
    }
    let e = s.edit();
    if c.loop_start < e.start || c.loop_end > e.end || c.loop_start >= c.loop_end {
        return None;
    }
    let mut next = e.clone();
    next.loop_start = c.loop_start;
    next.loop_end = c.loop_end;
    if next.loop_mode == LoopMode::Off {
        next.loop_mode = LoopMode::Forward;
    }
    next.crossfade = next.crossfade.min(next.max_crossfade());
    Some(next)
}

/// The edit candidate `index` of the result would make, as [`candidate_edit`].
fn indexed_edit(s: &Session, index: usize) -> Option<EditState> {
    let result = s.loop_finder.result.as_ref()?;
    let c = result.outcome.as_ref().ok()?.get(index)?;
    candidate_edit(s, c)
}

/// Previews candidate `index`: the views draw it and notes play it, as during a marker drag,
/// and the history is untouched. Does nothing when the candidate cannot be used.
pub fn preview_candidate(s: &mut Session, index: usize, engine: Option<&AudioEngine>) {
    let Some(e) = indexed_edit(s, index) else {
        return;
    };
    s.set_preview(e, engine);
    s.loop_finder.selected = Some(index);
}

/// Ends a candidate preview: the views and the sound go back to the current edit.
pub fn end_preview(s: &mut Session, engine: Option<&AudioEngine>) {
    s.cancel_preview(engine);
    s.loop_finder.selected = None;
}

/// Use loop: applies candidate `index` as one history entry, ending any preview. Does nothing
/// when the candidate cannot be used.
pub fn use_candidate(s: &mut Session, index: usize, engine: Option<&AudioEngine>) {
    let Some(e) = indexed_edit(s, index) else {
        return;
    };
    s.cancel_preview(engine);
    s.apply(e, engine);
    s.loop_finder.selected = None;
}

/// The finder's upkeep each frame for the selected session: picks up a finished search,
/// ending a preview of the result it replaces ([`finish_search`]), and forgets the selected
/// row once the preview no longer shows it (an undo, a marker drag or a stale result), leaving
/// that preview alone.
pub fn upkeep(s: &mut Session, engine: Option<&AudioEngine>) {
    finish_search(s, false, engine);
    if s.loop_finder.selected.is_some() && !previewing(s) {
        s.loop_finder.selected = None;
    }
}
