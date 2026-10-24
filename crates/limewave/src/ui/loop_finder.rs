//! The loop finder (v0.8): a search for loop points on a worker thread, kept on the session it
//! was made for, with a preview that leaves the history alone and Use loop as one undo step.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::text::CCursor;
use eframe::egui::{
    self, Button, Checkbox, Color32, DragValue, EventFilter, Key, Modifiers, Rect, Role,
    ScrollArea, Sense, Slider, StrokeKind, TextStyle, WidgetInfo, pos2, vec2,
};
use limewave_core::edit::{EditState, LoopMode};
use limewave_core::loop_finder::measure::{FAIR, GOOD};
use limewave_core::loop_finder::steady::{RangeKind, SearchRange, search_range};
use limewave_core::loop_finder::{
    DEFAULT_MIN_SECONDS, Direction, FinderError, FinderSettings, LoopCandidate, MAX_CANDIDATES,
    MAX_SEARCH_SECONDS, Quality, SeamMeasures, find_loops,
};

use crate::app::{Session, UiState};
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
    /// The row last clicked or moved to in the list, even one that could not be previewed.
    pub picked: Option<usize>,
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
            s.loop_finder.picked = None;
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
    st.picked = None;
    true
}

/// [`poll_search`], and when the new result replaces one whose candidate is being previewed,
/// the preview ends too ([`end_preview`]): a preview never outlives the result it came from.
pub fn finish_search(s: &mut Session, block: bool, engine: Option<&AudioEngine>) {
    let live = previewing(s);
    if take_result(s, block) && live {
        s.cancel_preview(engine);
    }
}

/// Whether the selected candidate is what the preview shows: a candidate preview is live.
pub fn previewing(s: &Session) -> bool {
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

/// Ends a candidate preview: the views and the sound go back to the current edit. Any other
/// preview, such as a marker drag, is left alone, so this is always safe to call.
pub fn end_preview(s: &mut Session, engine: Option<&AudioEngine>) {
    if previewing(s) {
        s.cancel_preview(engine);
    }
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

// The panel beside the seam view.

/// The shortest Longest that counts as a limit, in seconds; anything below it is "no limit".
const MIN_LONGEST: f64 = 0.01;

/// What the list says when the result was made for audio that has since changed.
const STALE_TEXT: &str = "The sound has changed. Search again.";

/// The id of the list row for candidate `index`.
fn row_id(index: usize) -> egui::Id {
    egui::Id::unique("loop_finder_row").with(index)
}

/// The candidate row that has keyboard focus, if any. The app lets its shortcuts and the
/// computer keyboard through while a row has focus, as it would with nothing focused.
pub fn focused_row(ctx: &egui::Context) -> Option<usize> {
    let focused = ctx.memory(|m| m.focused())?;
    (0..MAX_CANDIDATES).find(|&i| row_id(i) == focused)
}

/// The word for `q`.
fn quality_word(q: Quality) -> &'static str {
    match q {
        Quality::Seamless => "Seamless",
        Quality::Good => "Good",
        Quality::Fair => "Fair",
        Quality::Rough => "Rough",
    }
}

/// A row's text, and the character range of its quality word.
fn row_text(rank: usize, c: &LoopCandidate, rate: u32) -> (String, std::ops::Range<usize>) {
    let secs = c.len() as f64 / f64::from(rate);
    let mut text = format!(
        "{rank}. {secs:.3} s \u{b7} {} frames \u{b7} {}\u{2013}{} \u{b7} ",
        c.len(),
        c.loop_start,
        c.loop_end
    );
    let word = quality_word(c.quality);
    let start = text.chars().count();
    text.push_str(word);
    let end = start + word.chars().count();
    if let Some(cycles) = c.cycles() {
        text.push_str(&format!(" \u{b7} {cycles:.1} cycles"));
    }
    (text, start..end)
}

/// The measures behind a quality word in plain words, with the Good and Fair limits as the
/// boundaries between none, slight and clear.
fn measures_text(m: &SeamMeasures) -> String {
    let word = |value: f32, good: f32, fair: f32| {
        if value <= good {
            "none"
        } else if value <= fair {
            "slight"
        } else {
            "clear"
        }
    };
    format!(
        "Click: {} \u{b7} Tone change: {} \u{b7} Waveform match: {:.0} %",
        word(m.click, GOOD.click, FAIR.click),
        word(m.tone, GOOD.tone, FAIR.tone),
        (m.matching * 100.0).max(0.0)
    )
}

/// The panel's line for the range searched.
fn range_text(range: &SearchRange, rate: u32) -> String {
    let secs = |frame: usize| frame as f64 / f64::from(rate);
    let span = format!("{:.2}\u{2013}{:.2} s", secs(range.first), secs(range.last));
    match range.kind {
        RangeKind::Selection => "Searched the selection".to_owned(),
        RangeKind::Steady => format!("Searched the steady part, {span}"),
        RangeKind::NoSteadyPart => {
            format!("No steady part found; searched the middle of the sound, {span}")
        }
    }
}

/// What the panel says for `err`; nothing for a cancelled search.
fn error_text(err: FinderError) -> Option<&'static str> {
    match err {
        FinderError::Silent => Some("Nothing to loop in this range"),
        FinderError::RangeTooShort => Some("The range is shorter than the minimum loop length"),
        FinderError::RangeTooLong => Some("Select 60 seconds or less"),
        FinderError::BadSettings => Some("The shortest loop is longer than the longest"),
        FinderError::Cancelled => None,
    }
}

/// Reads Longest as typed: a length in seconds, or "no limit" (or nothing) for 0.
fn parse_longest(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("no limit") {
        return Some(0.0);
    }
    text.trim_end_matches('s').trim().parse().ok()
}

/// Whether candidate `index` of a current result lies outside Start...End.
fn outside_trim(s: &Session, index: usize) -> bool {
    let Some(Ok(candidates)) = s.loop_finder.result.as_ref().map(|r| &r.outcome) else {
        return false;
    };
    let e = s.edit();
    !is_stale(s)
        && candidates
            .get(index)
            .is_some_and(|c| c.loop_start < e.start || c.loop_end > e.end)
}

/// Moves to row `index`: previews it, or ends the preview when it cannot be used.
fn pick(s: &mut Session, index: usize, engine: Option<&AudioEngine>) {
    s.loop_finder.picked = Some(index);
    if indexed_edit(s, index).is_some() {
        preview_candidate(s, index, engine);
    } else {
        end_preview(s, engine);
    }
}

/// The Loop finder panel for `s`: Find loops (or Searching… and Cancel), Use loop, the
/// settings, what was searched, and the list of candidates. `key` is what a search started
/// now is filed under; without one Find loops is disabled.
pub fn finder_panel(
    ui: &mut egui::Ui,
    s: &mut Session,
    ui_state: &mut UiState,
    engine: Option<&AudioEngine>,
    key: Option<RenderKey>,
) {
    let loop_mode = s.edit().loop_mode;
    let ping_pong = loop_mode == LoopMode::PingPong;
    ui.horizontal_wrapped(|ui| {
        ui.strong("Loop finder");
        if is_searching(s) {
            ui.label("Searching\u{2026}");
            if ui.button("Cancel").clicked() {
                end_preview(s, engine);
                cancel_search(s);
            }
        } else {
            let find = ui.add_enabled(!ping_pong && key.is_some(), Button::new("Find loops"));
            if let (true, Some(key)) = (find.clicked(), key) {
                end_preview(s, engine);
                start_search(s, &ui_state.finder, ui_state.selection, key, ui.ctx());
            }
        }
        let usable = s
            .loop_finder
            .selected
            .filter(|&i| indexed_edit(s, i).is_some());
        let mut use_loop = ui
            .add_enabled(usable.is_some(), Button::new("Use loop"))
            .on_hover_text("Apply the selected loop as one undo step");
        if usable.is_none() && s.loop_finder.picked.is_some_and(|i| outside_trim(s, i)) {
            use_loop = use_loop.on_disabled_hover_text("Outside Start\u{2013}End");
        }
        if let (true, Some(i)) = (use_loop.clicked(), usable) {
            use_candidate(s, i, engine);
        }
    });
    if ping_pong {
        ui.weak("The finder makes forward and backward loops");
    }

    let prefs = &mut ui_state.finder;
    ui.add_enabled(
        loop_mode != LoopMode::Off,
        Checkbox::new(&mut prefs.keep_end, "Keep loop end"),
    )
    .on_hover_text("Search for a loop start only");
    ui.horizontal_wrapped(|ui| {
        let name = ui.label("Shortest");
        let most = prefs.max_s.unwrap_or(MAX_SEARCH_SECONDS).max(MIN_LONGEST);
        ui.add(
            DragValue::new(&mut prefs.min_s)
                .range(MIN_LONGEST..=most)
                .speed(0.01)
                .fixed_decimals(2)
                .suffix(" s"),
        )
        .labelled_by(name.id);
        let name = ui.label("Longest");
        let mut max = prefs.max_s.unwrap_or(0.0);
        ui.add(
            DragValue::new(&mut max)
                .range(0.0..=MAX_SEARCH_SECONDS)
                .speed(0.01)
                .custom_formatter(|v, _| {
                    if v < MIN_LONGEST {
                        "no limit".to_owned()
                    } else {
                        format!("{v:.2} s")
                    }
                })
                .custom_parser(parse_longest),
        )
        .labelled_by(name.id);
        prefs.max_s = (max >= MIN_LONGEST).then_some(max);
    });
    ui.horizontal(|ui| {
        let name = ui.label("Tight");
        ui.add(Slider::new(&mut prefs.preference, 0.0..=1.0).show_value(false))
            .labelled_by(name.id)
            .on_hover_text("Tight favours short loops; Natural favours long ones");
        ui.label("Natural");
    });

    let rate = s.rate();
    let stale = is_stale(s);
    let result = s.loop_finder.result.as_ref();
    if let Some(r) = result {
        ui.weak(range_text(&r.range, rate));
    }
    let message = match (s.loop_finder.range_error, result) {
        (Some(err), _) => error_text(err),
        (None, Some(_)) if stale => Some(STALE_TEXT),
        (None, Some(r)) => match &r.outcome {
            Err(err) => error_text(*err),
            Ok(candidates) if candidates.is_empty() => Some("No loops found"),
            Ok(_) => None,
        },
        (None, None) => None,
    };
    if let Some(message) = message {
        ui.label(message);
    }
    let rows: Vec<LoopCandidate> = result
        .and_then(|r| r.outcome.as_ref().ok())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        return;
    }

    // Up and down move the preview while a row has focus, and Enter applies it.
    if let Some(i) = focused_row(ui.ctx()).filter(|&i| !stale && i < rows.len()) {
        let filter = EventFilter {
            vertical_arrows: true,
            ..Default::default()
        };
        ui.memory_mut(|m| m.set_focus_lock_filter(row_id(i), filter));
        let (up, down, enter) = ui.input_mut(|input| {
            (
                input.consume_key(Modifiers::NONE, Key::ArrowUp),
                input.consume_key(Modifiers::NONE, Key::ArrowDown),
                input.consume_key(Modifiers::NONE, Key::Enter),
            )
        });
        let to = if down {
            Some(i + 1).filter(|&n| n < rows.len())
        } else if up {
            i.checked_sub(1)
        } else {
            None
        };
        if enter {
            use_candidate(s, i, engine);
        } else if let Some(to) = to {
            pick(s, to, engine);
            ui.memory_mut(|m| m.request_focus(row_id(to)));
        }
    }

    let selected = s.loop_finder.selected;
    ui.add_enabled_ui(!stale, |ui| {
        ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (i, c) in rows.iter().enumerate() {
                    let row = candidate_row(ui, i, c, rate, selected == Some(i));
                    if row.double_clicked() {
                        use_candidate(s, i, engine);
                    } else if row.clicked() {
                        row.request_focus();
                        if selected == Some(i) {
                            end_preview(s, engine);
                        } else {
                            pick(s, i, engine);
                        }
                    }
                }
            });
    });
}

/// One selectable row of the list. Its text is also its accessible label; hovering the
/// quality word shows the measures behind it.
fn candidate_row(
    ui: &mut egui::Ui,
    index: usize,
    c: &LoopCandidate,
    rate: u32,
    selected: bool,
) -> egui::Response {
    let (text, word) = row_text(index + 1, c, rate);
    let pad = ui.spacing().button_padding;
    let width = ui.available_width();
    let galley = ui.painter().layout(
        text.clone(),
        TextStyle::Button.resolve(ui.style()),
        Color32::PLACEHOLDER,
        (width - 2.0 * pad.x).max(0.0),
    );
    let (_, rect) = ui.allocate_space(vec2(width, galley.size().y + 2.0 * pad.y));
    let response = ui.interact(rect, row_id(index), Sense::click());
    let enabled = ui.is_enabled();
    response.widget_info(|| WidgetInfo::selected(Role::Button, enabled, selected, &text));
    let text_pos = rect.min + pad;
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact_selectable(&response, selected);
        if selected || response.hovered() || response.has_focus() {
            ui.painter().rect(
                rect,
                visuals.corner_radius,
                visuals.weak_bg_fill,
                visuals.bg_stroke,
                StrokeKind::Inside,
            );
        }
        ui.painter()
            .galley(text_pos, galley.clone(), visuals.text_color());
    }
    // The word never breaks, so its end marks its row; its start may sit at the end of the
    // row above when the break falls just before it.
    let from = galley.pos_from_cursor(CCursor::new(word.start));
    let to = galley.pos_from_cursor(CCursor::new(word.end));
    let min = if (from.min.y - to.min.y).abs() < 0.5 {
        from.min
    } else {
        pos2(0.0, to.min.y)
    };
    let word_rect = Rect::from_min_max(min, to.max).translate(text_pos.to_vec2());
    ui.interact(word_rect, row_id(index).with("quality"), Sense::hover())
        .on_hover_text(measures_text(&c.measures));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_reads_no_limit_and_seconds() {
        assert_eq!(parse_longest("no limit"), Some(0.0));
        assert_eq!(parse_longest(""), Some(0.0));
        assert_eq!(parse_longest("1.50 s"), Some(1.5));
        assert_eq!(parse_longest("2"), Some(2.0));
        assert_eq!(parse_longest("long"), None);
    }

    #[test]
    fn measures_use_the_good_and_fair_limits() {
        let m = SeamMeasures {
            click: GOOD.click,
            tone: FAIR.tone,
            matching: 0.874,
        };
        assert_eq!(
            measures_text(&m),
            "Click: none \u{b7} Tone change: slight \u{b7} Waveform match: 87 %"
        );
        let m = SeamMeasures {
            click: FAIR.click + 0.1,
            tone: 0.0,
            matching: -0.2,
        };
        assert_eq!(
            measures_text(&m),
            "Click: clear \u{b7} Tone change: none \u{b7} Waveform match: 0 %"
        );
    }
}
