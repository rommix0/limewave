//! Edit state (trim and loop markers, tuning, effects), marker-move constraints and undo history.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::dsp::resample::{MAX_RATE, MIN_RATE, resample, resampled_len};
use crate::effects::{
    AppliedEffect, Op, Resample, Splice, SpliceKind, frames_after, output_frames, rate_after,
    resample_map,
};

/// How the sample loops on playback.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LoopMode {
    Off,
    Forward,
    PingPong,
    Backward,
}

/// Crossfade curve shape.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FadeShape {
    EqualPower,
    Linear,
}

/// The editable state of a sample. `start`, `end` and the loop points are inclusive frame indices.
#[derive(Clone, PartialEq, Debug)]
pub struct EditState {
    pub start: usize,
    pub end: usize,
    pub loop_mode: LoopMode,
    pub loop_start: usize,
    pub loop_end: usize,
    pub crossfade: usize,
    pub fade_shape: FadeShape,
    pub root: u8,
    pub cents: f32,
    /// Effects replayed on the original audio, in order, before the crossfade and the trim.
    pub effects: Vec<AppliedEffect>,
}

impl EditState {
    /// The initial state for a file of `frames` frames: whole file, loop off.
    pub fn default_for(frames: usize) -> EditState {
        let end = frames.saturating_sub(1);
        EditState {
            start: 0,
            end,
            loop_mode: LoopMode::Off,
            loop_start: 0,
            loop_end: end,
            crossfade: 0,
            fade_shape: FadeShape::EqualPower,
            root: 60,
            cents: 0.0,
            effects: Vec::new(),
        }
    }

    /// Forces the state to be valid for an original file of `original_frames` frames.
    ///
    /// Each effect's range is cut to its own input, the length after the steps before it, and
    /// effects left with nothing to work on are dropped, as is a splice whose range no longer
    /// fits. The markers are then kept inside the length after every effect; if a loop of at
    /// least 2 frames cannot fit, the loop is turned off.
    pub fn clamped(mut self, original_frames: usize) -> EditState {
        // Effect ranges end inside their input; an empty one, or one wholly past the end, does
        // nothing and is dropped. Channel tools ignore their range and are kept as they are.
        let mut frames = original_frames;
        self.effects.retain_mut(|a| {
            let keep = match a.op {
                Op::StereoToMono(_) | Op::SwapChannels | Op::MonoToStereo => true,
                Op::Splice(_) | Op::Resample(_) => a.first <= a.last && a.last < frames,
                _ => {
                    a.last = a.last.min(frames.saturating_sub(1));
                    frames > 0 && a.first <= a.last
                }
            };
            if keep {
                frames = output_frames(a, frames);
            }
            keep
        });

        let last = frames.saturating_sub(1);
        self.end = self.end.min(last);
        self.start = self.start.min(self.end);
        self.cents = if self.cents.is_nan() {
            0.0
        } else {
            self.cents.clamp(-50.0, 50.0)
        };

        if self.loop_mode != LoopMode::Off && self.end - self.start < 1 {
            self.loop_mode = LoopMode::Off;
        }
        if self.loop_mode == LoopMode::Off {
            // Loop fields are kept, but must still lie inside the file.
            self.loop_end = self.loop_end.min(last);
            self.loop_start = self.loop_start.min(self.loop_end);
        } else {
            self.loop_start = self.loop_start.clamp(self.start, self.end - 1);
            self.loop_end = self.loop_end.clamp(self.loop_start + 1, self.end);
            self.crossfade = self.crossfade.min(self.max_crossfade());
        }
        self
    }

    /// The longest crossfade the loop allows: the fade reads `loop_start - N` and cannot be
    /// longer than the loop. Zero when the loop is off.
    pub fn max_crossfade(&self) -> usize {
        if self.loop_mode == LoopMode::Off {
            0
        } else {
            self.loop_len().min(self.loop_start)
        }
    }

    /// Loop length in frames (both points inclusive); 0 if the points are inverted.
    pub fn loop_len(&self) -> usize {
        if self.loop_end < self.loop_start {
            0
        } else {
            self.loop_end - self.loop_start + 1
        }
    }
}

/// A draggable marker.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Marker {
    Start,
    LoopStart,
    LoopEnd,
    End,
}

/// Moves `m` to frame `to`, respecting the constraints between markers, and returns the
/// clamped result (see [`EditState::clamped`] for `original_frames`). With `lock_length`,
/// moving either loop marker shifts both.
pub fn move_marker(
    e: &EditState,
    m: Marker,
    to: usize,
    lock_length: bool,
    original_frames: usize,
) -> EditState {
    let mut s = e.clone();
    let loop_on = e.loop_mode != LoopMode::Off;
    match m {
        Marker::Start => {
            let hi = if loop_on { e.loop_start } else { e.end };
            s.start = to.min(hi);
        }
        Marker::End => {
            let lo = if loop_on { e.loop_end } else { e.start };
            s.end = to.max(lo);
        }
        Marker::LoopStart | Marker::LoopEnd => {
            let len = e.loop_len();
            let span = e.end.saturating_sub(e.start) + 1;
            if lock_length && len >= 2 && len <= span {
                if m == Marker::LoopStart {
                    s.loop_start = to.clamp(e.start, e.end + 1 - len);
                    s.loop_end = s.loop_start + len - 1;
                } else {
                    s.loop_end = to.clamp(e.start + len - 1, e.end);
                    s.loop_start = s.loop_end + 1 - len;
                }
            } else if m == Marker::LoopStart {
                let hi = e.loop_end.saturating_sub(1).max(e.start);
                s.loop_start = to.clamp(e.start, hi);
            } else {
                let lo = (e.loop_start + 1).min(e.end);
                s.loop_end = to.clamp(lo, e.end);
            }
        }
    }
    s.clamped(original_frames)
}

/// Makes the selection the loop. A loop that is off becomes forward; other modes are kept.
/// Start and End move outward to include the selection.
pub fn set_loop_to_selection(
    e: &EditState,
    sel: (usize, usize),
    original_frames: usize,
) -> EditState {
    let (lo, hi) = (sel.0.min(sel.1), sel.0.max(sel.1));
    let loop_mode = match e.loop_mode {
        LoopMode::Off => LoopMode::Forward,
        mode => mode,
    };
    EditState {
        start: e.start.min(lo),
        end: e.end.max(hi),
        loop_mode,
        loop_start: lo,
        loop_end: hi,
        ..e.clone()
    }
    .clamped(original_frames)
}

/// Makes the selection the trim. Fails when the loop is on and does not lie inside it.
pub fn set_trim_to_selection(e: &EditState, sel: (usize, usize)) -> Result<EditState, String> {
    let (lo, hi) = (sel.0.min(sel.1), sel.0.max(sel.1));
    if e.loop_mode != LoopMode::Off && (e.loop_start < lo || e.loop_end > hi) {
        return Err("The loop lies outside the selection; move the loop or select more".into());
    }
    Ok(EditState {
        start: lo,
        end: hi,
        ..e.clone()
    })
}

/// Ends the sample `padding` frames after the loop end (at most the last frame after the
/// effects). Unchanged when the loop is off.
pub fn trim_after_loop(e: &EditState, padding: usize, original_frames: usize) -> EditState {
    if e.loop_mode == LoopMode::Off {
        return e.clone();
    }
    let last = frames_after(original_frames, &e.effects).saturating_sub(1);
    let end = e.loop_end.saturating_add(padding).min(last);
    move_marker(e, Marker::End, end, false, original_frames)
}

/// As [`trim_after_loop`], and also starts the sample at the loop start.
pub fn crop_to_loop(e: &EditState, padding: usize, original_frames: usize) -> EditState {
    let t = trim_after_loop(e, padding, original_frames);
    if t.loop_mode == LoopMode::Off {
        return t;
    }
    move_marker(&t, Marker::Start, t.loop_start, false, original_frames)
}

/// Cuts the file down to the selection now: the audio before and after it is removed, as
/// undoable effect steps. Start and End become the new first and last frame. A loop inside the
/// selection moves with the audio, a loop crossing an edge is clipped to it, and a loop left with
/// fewer than two frames is turned off. `channels` is the processed audio's channel count.
/// Fails when there is no audio or the selection already covers the whole file.
pub fn crop_to_selection(
    e: &EditState,
    sel: (usize, usize),
    channels: usize,
    original_frames: usize,
) -> Result<EditState, String> {
    let frames = frames_after(original_frames, &e.effects);
    if frames == 0 {
        return Err("There is no audio to crop".into());
    }
    let last = frames - 1;
    let (lo, hi) = (sel.0.min(sel.1).min(last), sel.0.max(sel.1).min(last));
    if lo == 0 && hi == last {
        return Err("The selection already covers the whole file".into());
    }
    let cut = |first, last| AppliedEffect {
        op: Op::Splice(Arc::new(Splice {
            kind: SpliceKind::Cut,
            audio: vec![Vec::new(); channels],
            generated: (0, 0),
        })),
        first,
        last,
    };
    let mut c = e.clone();
    // The part after the selection goes first, so the frame numbers before it stay valid.
    if hi < last {
        c.effects.push(cut(hi + 1, last));
    }
    if lo > 0 {
        c.effects.push(cut(0, lo - 1));
    }
    c.start = 0;
    c.end = hi - lo;
    if c.loop_mode != LoopMode::Off {
        let (ls, le) = (e.loop_start.max(lo), e.loop_end.min(hi));
        if le > ls {
            c.loop_start = ls - lo;
            c.loop_end = le - lo;
        } else {
            c.loop_mode = LoopMode::Off;
        }
    }
    Ok(c.clamped(original_frames))
}

/// Redraws samples by hand: `values` replace channel `channel` of `processed` from frame
/// `first` on, as one undoable effect step that keeps the file length. Values are held to full
/// scale. Returns `None` when the range does not fit, a value is not finite, or nothing
/// changes.
pub fn redraw_samples(
    e: &EditState,
    processed: &[Vec<f32>],
    channel: usize,
    first: usize,
    values: &[f32],
) -> Option<EditState> {
    let frames = processed.first().map_or(0, Vec::len);
    let last = first.checked_add(values.len())?.checked_sub(1)?;
    if channel >= processed.len() || last >= frames || values.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let mut audio: Vec<Vec<f32>> = processed.iter().map(|c| c[first..=last].to_vec()).collect();
    for (to, v) in audio[channel].iter_mut().zip(values) {
        *to = v.clamp(-1.0, 1.0);
    }
    if audio[channel] == processed[channel][first..=last] {
        return None;
    }
    let mut r = e.clone();
    r.effects.push(AppliedEffect {
        op: Op::Splice(Arc::new(Splice {
            kind: SpliceKind::Draw,
            audio,
            // An empty range: nothing is tinted as generated.
            generated: (1, 0),
        })),
        first,
        last,
    });
    Some(r)
}

/// Converts the processed audio to the rate `to`, as one undoable effect step. `file_rate` is
/// the rate of the file on disk; the rate converted from is [`rate_after`] the steps so far.
/// The audio is rendered here, from `processed`.
///
/// Start, End and the loop points move with the audio, scaled by `round(f * to / from)` (see
/// [`resample_map`]): Start and the loop start directly, End and the loop end as the position
/// one past them, and the crossfade by the same ratio. An end never lies before its start,
/// and a loop whose mapped points span fewer than 2 frames is turned off. Returns `None` when `to` is the current rate or outside
/// `MIN_RATE..=MAX_RATE`, the current rate is 0, or `processed` holds no frames.
pub fn resample_edit(
    e: &EditState,
    processed: &[Vec<f32>],
    to: u32,
    file_rate: u32,
    original_frames: usize,
) -> Option<EditState> {
    let from = rate_after(file_rate, &e.effects);
    let frames = processed.first().map_or(0, Vec::len);
    if to == from || !(MIN_RATE..=MAX_RATE).contains(&to) || from == 0 || frames == 0 {
        return None;
    }
    let step = AppliedEffect {
        op: Op::Resample(Arc::new(Resample {
            from,
            to,
            audio: resample(processed, from, to),
        })),
        first: 0,
        last: frames - 1,
    };
    let scale = |f| resample_map(std::slice::from_ref(&step), f);
    // An inclusive end marker maps as the position one past it, so an End on the last frame
    // lands on the new last frame and a loop keeps its length to within a frame. Both steps
    // saturate: a frame at the top of the range stays there, and position 0 stays at 0.
    let scale_end = |f: usize| scale(f.saturating_add(1)).saturating_sub(1);
    let mut r = e.clone();
    (r.start, r.end) = (scale(e.start), scale_end(e.end));
    (r.loop_start, r.loop_end) = (scale(e.loop_start), scale_end(e.loop_end));
    // At a tiny rate a start and its end can cross; an end never lies before its start.
    r.end = r.end.max(r.start);
    r.loop_end = r.loop_end.max(r.loop_start);
    r.crossfade = resampled_len(e.crossfade, from, to);
    // A loop that the scaling leaves under 2 frames cannot play.
    if r.loop_mode != LoopMode::Off && r.loop_end + 1 - r.loop_start < 2 {
        r.loop_mode = LoopMode::Off;
    }
    r.effects.push(step);
    Some(r.clamped(original_frames))
}

/// Source of entry stamps, shared by every history in the process so stamps from
/// different histories compare: a larger stamp was recorded later.
static NEXT_STAMP: AtomicU64 = AtomicU64::new(1);

/// Linear undo/redo history, tracking which entry was last saved. Every recorded
/// entry carries a stamp; an initial entry has stamp 0.
pub struct History<T = EditState> {
    entries: Vec<(T, u64)>,
    cursor: usize,
    /// Index of the saved entry; `None` once a push dropped it from the history.
    saved: Option<usize>,
}

impl<T: Clone + PartialEq> History<T> {
    pub fn new(initial: T) -> History<T> {
        History {
            entries: vec![(initial, 0)],
            cursor: 0,
            saved: Some(0),
        }
    }

    pub fn current(&self) -> &T {
        &self.entries[self.cursor].0
    }

    /// The state the history started from.
    pub fn first(&self) -> &T {
        &self.entries[0].0
    }

    /// The stamp of the current entry; 0 for the initial one.
    pub fn stamp(&self) -> u64 {
        self.entries[self.cursor].1
    }

    /// The stamp of the entry a redo would move to.
    pub fn redo_stamp(&self) -> Option<u64> {
        self.entries.get(self.cursor + 1).map(|e| e.1)
    }

    /// The index of the current entry; 0 is the state the file was opened with.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Records a new state, dropping any redo tail. A no-op if equal to the current state.
    pub fn push(&mut self, s: T) {
        if *self.current() == s {
            return;
        }
        self.entries.truncate(self.cursor + 1);
        if self.saved.is_some_and(|i| i > self.cursor) {
            self.saved = None;
        }
        self.entries
            .push((s, NEXT_STAMP.fetch_add(1, Ordering::Relaxed)));
        self.cursor += 1;
    }

    pub fn undo(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    pub fn redo(&mut self) -> bool {
        if self.cursor + 1 >= self.entries.len() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// The saved entry; `None` once a push dropped it from the history.
    pub fn saved(&self) -> Option<&T> {
        self.saved.map(|i| &self.entries[i].0)
    }

    pub fn mark_saved(&mut self) {
        self.saved = Some(self.cursor);
    }

    pub fn is_dirty(&self) -> bool {
        self.saved != Some(self.cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::{Splice, SpliceKind, map_frame, map_range, process};
    use std::sync::Arc;

    fn looped(frames: usize, end: usize, ls: usize, le: usize) -> EditState {
        EditState {
            end,
            loop_mode: LoopMode::Forward,
            loop_start: ls,
            loop_end: le,
            ..EditState::default_for(frames)
        }
    }

    fn at(start: usize) -> EditState {
        EditState {
            start,
            ..EditState::default_for(100)
        }
    }

    #[test]
    fn default_for_zero_frames_is_valid() {
        let d = EditState::default_for(0);
        assert_eq!((d.start, d.end), (0, 0));
        let c = d.clone().clamped(0);
        assert_eq!(c, d);
        assert_eq!(c.loop_mode, LoopMode::Off);
    }

    #[test]
    fn default_state_has_no_effects() {
        assert!(EditState::default_for(1000).effects.is_empty());
    }

    #[test]
    fn clamped_keeps_the_effects() {
        let mut e = EditState::default_for(100);
        e.effects.push(AppliedEffect {
            op: Op::Invert,
            first: 0,
            last: 99,
        });
        assert_eq!(e.clone().clamped(100).effects, e.effects);
    }

    #[test]
    fn clamped_fits_effect_ranges_to_the_file() {
        let fx = |op: Op, first: usize, last: usize| AppliedEffect { op, first, last };
        let mirror = Op::Reverse {
            mirror_markers: true,
        };
        let mut e = EditState::default_for(100);
        e.effects = vec![
            fx(Op::Invert, 10, 20),
            fx(Op::Invert, 90, usize::MAX),
            fx(mirror.clone(), 1, usize::MAX),
            // Wholly outside the file, or empty: dropped.
            fx(Op::Invert, 100, 200),
            fx(Op::Invert, usize::MAX, usize::MAX),
            fx(Op::Invert, 50, 40),
            // Channel tools ignore their range and are kept as they are.
            fx(Op::SwapChannels, 7, 3),
        ];
        assert_eq!(
            e.clamped(100).effects,
            vec![
                fx(Op::Invert, 10, 20),
                fx(Op::Invert, 90, 99),
                fx(mirror, 1, 99),
                fx(Op::SwapChannels, 7, 3),
            ]
        );

        let mut empty = EditState::default_for(0);
        empty.effects = vec![fx(Op::Invert, 0, 0), fx(Op::MonoToStereo, 0, 0)];
        assert_eq!(empty.clamped(0).effects, vec![fx(Op::MonoToStereo, 0, 0)]);
    }

    /// A tail that replaces 900..=999 of a 1000-frame file with 600 frames, making it 1500.
    fn lengthening_tail() -> AppliedEffect {
        AppliedEffect {
            op: Op::Splice(Arc::new(Splice {
                kind: SpliceKind::Tail,
                audio: vec![vec![0.0; 600]],
                generated: (100, 599),
            })),
            first: 900,
            last: 999,
        }
    }

    #[test]
    fn clamped_clamps_each_step_to_its_own_input() {
        let gain = AppliedEffect {
            op: Op::Scale(0.5),
            first: 1200,
            last: 1400,
        };
        let mut e = EditState::default_for(1000);
        e.effects = vec![lengthening_tail(), gain.clone()];
        e.end = usize::MAX;
        let c = e.clamped(1000);
        assert_eq!(c.effects, vec![lengthening_tail(), gain]);
        assert_eq!(c.end, 1499);
    }

    #[test]
    fn clamped_drops_a_splice_that_no_longer_fits() {
        let mut e = EditState::default_for(800);
        e.effects = vec![lengthening_tail()];
        assert!(e.clamped(800).effects.is_empty());
    }

    #[test]
    fn move_marker_reaches_the_new_end() {
        let mut e = EditState::default_for(1000);
        e.effects = vec![lengthening_tail()];
        assert_eq!(move_marker(&e, Marker::End, 1499, false, 1000).end, 1499);
        assert_eq!(move_marker(&e, Marker::End, 5000, false, 1000).end, 1499);
        let looped = EditState {
            loop_mode: LoopMode::Forward,
            loop_start: 100,
            loop_end: 500,
            ..e
        };
        assert_eq!(trim_after_loop(&looped, 2000, 1000).end, 1499);
        assert_eq!(
            set_loop_to_selection(&looped, (1300, 1450), 1000).loop_end,
            1450
        );
    }

    #[test]
    fn default_for_fills_the_file() {
        let d = EditState::default_for(1000);
        assert_eq!((d.start, d.end, d.loop_start, d.loop_end), (0, 999, 0, 999));
        assert_eq!(d.loop_mode, LoopMode::Off);
        assert_eq!(d.fade_shape, FadeShape::EqualPower);
        assert_eq!((d.crossfade, d.root, d.cents), (0, 60, 0.0));
    }

    #[test]
    fn move_loop_end_stops_at_end_marker() {
        let e = looped(1000, 800, 100, 500);
        let r = move_marker(&e, Marker::LoopEnd, 950, false, 1000);
        assert_eq!(r.loop_end, 800);
    }

    #[test]
    fn move_start_stops_at_loop_start() {
        let e = looped(1000, 800, 100, 500);
        let r = move_marker(&e, Marker::Start, 300, false, 1000);
        assert_eq!(r.start, 100);
    }

    #[test]
    fn move_end_stops_at_loop_end_and_file_end() {
        let e = looped(1000, 800, 100, 500);
        assert_eq!(move_marker(&e, Marker::End, 300, false, 1000).end, 500);
        assert_eq!(move_marker(&e, Marker::End, 5000, false, 1000).end, 999);
    }

    #[test]
    fn markers_with_loop_off_keep_loop_fields() {
        let mut e = EditState::default_for(1000);
        e.loop_start = 100;
        e.loop_end = 500;
        let r = move_marker(&e, Marker::Start, 700, false, 1000);
        assert_eq!((r.start, r.loop_start, r.loop_end), (700, 100, 500));
        assert_eq!(r.loop_mode, LoopMode::Off);
    }

    #[test]
    fn lock_length_moves_both() {
        let e = looped(1000, 800, 100, 199);
        let r = move_marker(&e, Marker::LoopStart, 150, true, 1000);
        assert_eq!((r.loop_start, r.loop_end), (150, 249));
        let r = move_marker(&e, Marker::LoopEnd, 300, true, 1000);
        assert_eq!((r.loop_start, r.loop_end), (201, 300));
    }

    #[test]
    fn lock_length_stops_at_bounds() {
        let e = looped(1000, 800, 100, 199);
        let r = move_marker(&e, Marker::LoopStart, 790, true, 1000);
        assert_eq!(r.loop_end, 800);
        assert_eq!(r.loop_len(), 100);
        let r = move_marker(&e, Marker::LoopEnd, 5, true, 1000);
        assert_eq!(r.loop_start, 0);
        assert_eq!(r.loop_len(), 100);
    }

    #[test]
    fn move_marker_reclamps_crossfade() {
        let mut e = looped(1000, 800, 100, 299);
        e.crossfade = 80;
        let r = move_marker(&e, Marker::LoopEnd, 149, false, 1000);
        assert_eq!(r.crossfade, 50);
    }

    #[test]
    fn max_crossfade_limited_by_loop_start() {
        let e = looped(1000, 999, 10, 509);
        assert_eq!(e.loop_len(), 500);
        assert_eq!(e.max_crossfade(), 10);
    }

    #[test]
    fn max_crossfade_zero_when_loop_off() {
        assert_eq!(EditState::default_for(1000).max_crossfade(), 0);
    }

    #[test]
    fn clamped_turns_loop_off_when_it_cannot_fit() {
        let mut e = looped(1, 0, 0, 0);
        e.cents = 80.0;
        let c = e.clamped(1);
        assert_eq!(c.loop_mode, LoopMode::Off);
        assert_eq!(c.cents, 50.0);
        let c = looped(10, 5, 5, 5).clamped(10);
        assert_eq!((c.loop_start, c.loop_end), (4, 5));
        assert_eq!(c.loop_mode, LoopMode::Forward);
    }

    #[test]
    fn clamped_pulls_everything_into_the_file() {
        let c = looped(100, 500, 300, 900).clamped(100);
        assert_eq!(c.end, 99);
        assert!(c.start <= c.loop_start && c.loop_start < c.loop_end && c.loop_end <= c.end);
    }

    #[test]
    fn history_undo_redo_and_dirty() {
        let (a, b, c) = (at(1), at(2), at(3));
        let mut h = History::new(EditState::default_for(100));
        assert!(!h.is_dirty());
        h.push(a.clone());
        h.push(b.clone());
        assert!(h.is_dirty());
        assert!(h.undo());
        assert_eq!(h.current(), &a);
        assert!(h.is_dirty());
        assert!(h.redo());
        assert_eq!(h.current(), &b);
        h.mark_saved();
        assert!(!h.is_dirty());
        h.push(c);
        assert!(h.is_dirty());
        assert!(h.undo());
        assert_eq!(h.current(), &b);
        assert!(!h.is_dirty());
    }

    #[test]
    fn history_push_drops_redo_and_dropped_saved_entry_stays_dirty() {
        let mut h = History::new(EditState::default_for(100));
        h.push(at(1));
        h.push(at(2));
        h.mark_saved();
        assert!(h.undo());
        h.push(at(5));
        assert!(!h.redo());
        assert!(h.is_dirty());
        assert!(h.undo());
        assert_eq!(h.current(), &at(1));
        assert!(h.is_dirty());
        assert!(h.undo());
        assert!(!h.undo());
    }

    #[test]
    fn history_cursor_counts_the_current_entry() {
        let mut h = History::new(EditState::default_for(100));
        assert_eq!(h.cursor(), 0);
        h.push(at(1));
        h.push(at(2));
        assert_eq!(h.cursor(), 2);
        h.undo();
        assert_eq!(h.cursor(), 1);
        h.push(at(1));
        assert_eq!(h.cursor(), 1, "an equal push adds nothing");
        h.redo();
        assert_eq!(h.cursor(), 2);
    }

    #[test]
    fn saved_is_the_saved_entry_until_a_push_drops_it() {
        let mut h = History::new(1);
        assert_eq!(h.saved(), Some(&1));
        h.push(2);
        h.mark_saved();
        h.push(3);
        assert_eq!(h.saved(), Some(&2));
        h.undo();
        h.undo();
        h.push(4);
        assert_eq!(h.saved(), None);
    }

    #[test]
    fn stamps_increase_and_undo_moves_them() {
        let mut h = History::new(EditState::default_for(100));
        assert_eq!(h.stamp(), 0);
        assert_eq!(h.redo_stamp(), None);
        h.push(at(1));
        let a = h.stamp();
        h.push(at(2));
        let b = h.stamp();
        assert!(a > 0 && b > a);
        h.push(at(2));
        assert_eq!(h.stamp(), b, "an equal push takes no stamp");
        assert!(h.undo());
        assert_eq!(h.stamp(), a);
        assert_eq!(h.redo_stamp(), Some(b));
        assert!(h.undo());
        assert_eq!(h.stamp(), 0);
        assert_eq!(h.redo_stamp(), Some(a));
        assert_eq!(h.first(), &EditState::default_for(100));
        // A later push, even on another history, is stamped after both.
        let mut other = History::new(7u32);
        other.push(8);
        assert!(other.stamp() > b);
        h.push(at(3));
        assert!(h.stamp() > other.stamp());
        assert_eq!(h.redo_stamp(), None);
    }

    #[test]
    fn history_works_for_other_types() {
        let mut h: History<u32> = History::new(1);
        h.push(2);
        h.push(3);
        assert_eq!((*h.current(), *h.first()), (3, 1));
        assert!(h.undo());
        assert_eq!(*h.current(), 2);
        assert!(h.is_dirty());
        h.mark_saved();
        assert!(!h.is_dirty());
        assert!(h.redo());
        assert!(h.is_dirty());
    }

    #[test]
    fn history_push_equal_is_noop() {
        let init = EditState::default_for(100);
        let mut h = History::new(init.clone());
        h.push(init);
        assert!(!h.undo());
        assert!(!h.is_dirty());
    }

    #[test]
    fn loop_to_selection_turns_the_loop_on_and_widens_the_trim() {
        let base = EditState {
            start: 100,
            end: 800,
            loop_start: 400,
            loop_end: 500,
            ..EditState::default_for(1000)
        };
        let e = set_loop_to_selection(&base, (50, 300), 1000);
        assert_eq!(e.loop_mode, LoopMode::Forward);
        assert_eq!((e.loop_start, e.loop_end), (50, 300));
        assert_eq!((e.start, e.end), (50, 800));

        for mode in [LoopMode::PingPong, LoopMode::Backward] {
            let m = EditState {
                loop_mode: mode,
                ..base.clone()
            };
            assert_eq!(set_loop_to_selection(&m, (50, 300), 1000).loop_mode, mode);
        }

        let e = set_loop_to_selection(&base, (10, 10), 1000);
        assert_eq!((e.loop_start, e.loop_end), (10, 11));
    }

    #[test]
    fn trim_to_selection_refuses_to_strand_the_loop() {
        let e = looped(1000, 999, 300, 500);
        assert!(set_trim_to_selection(&e, (200, 400)).is_err());
        assert_eq!(
            set_trim_to_selection(&e, (200, 400)).unwrap_err(),
            "The loop lies outside the selection; move the loop or select more"
        );
        let t = set_trim_to_selection(&e, (250, 600)).unwrap();
        assert_eq!((t.start, t.end), (250, 600));
        // A selection dragged right to left gives the same trim, and the same refusal.
        let t = set_trim_to_selection(&e, (600, 250)).unwrap();
        assert_eq!((t.start, t.end), (250, 600));
        assert!(set_trim_to_selection(&e, (400, 200)).is_err());

        let off = EditState::default_for(1000);
        let t = set_trim_to_selection(&off, (200, 400)).unwrap();
        assert_eq!((t.start, t.end), (200, 400));
    }

    #[test]
    fn trim_after_loop_and_crop_to_loop() {
        let e = EditState {
            start: 100,
            ..looped(1000, 999, 300, 500)
        };
        let t = trim_after_loop(&e, 16, 1000);
        assert_eq!((t.start, t.end), (100, 516));
        let c = crop_to_loop(&e, 16, 1000);
        assert_eq!((c.start, c.end), (300, 516));
        assert_eq!(trim_after_loop(&e, 0, 1000).end, 500);
        assert_eq!(trim_after_loop(&e, 900, 1000).end, 999);
        assert_eq!(crop_to_loop(&e, 900, 1000).end, 999);

        let off = EditState::default_for(1000);
        assert_eq!(trim_after_loop(&off, 16, 1000), off);
        assert_eq!(crop_to_loop(&off, 16, 1000), off);
    }

    /// `channels` channels of a distinct ramp each.
    fn ramps(frames: usize, channels: usize) -> Vec<Vec<f32>> {
        (0..channels)
            .map(|c| (0..frames).map(|i| (c * 10_000 + i) as f32).collect())
            .collect()
    }

    #[test]
    fn crop_to_selection_removes_the_audio_outside() {
        let audio = ramps(1000, 2);
        let e = EditState::default_for(1000);
        // A selection dragged right to left is ordered first.
        let c = crop_to_selection(&e, (599, 200), 2, 1000).unwrap();
        let out = crate::effects::process(&audio, &c.effects);
        assert_eq!(out[0], audio[0][200..600]);
        assert_eq!(out[1], audio[1][200..600]);
        assert_eq!((c.start, c.end), (0, 399));
        assert_eq!(c.clone().clamped(1000), c);
    }

    #[test]
    fn crop_to_selection_shifts_clips_or_drops_the_loop() {
        let crop = |ls, le| crop_to_selection(&looped(1000, 999, ls, le), (200, 599), 1, 1000);
        let inside = crop(300, 500).unwrap();
        assert_eq!(inside.loop_mode, LoopMode::Forward);
        assert_eq!((inside.loop_start, inside.loop_end), (100, 300));
        let crossing = crop(100, 400).unwrap();
        assert_eq!((crossing.loop_start, crossing.loop_end), (0, 200));
        assert_eq!(crossing.loop_mode, LoopMode::Forward);
        let after = crop(700, 800).unwrap();
        assert_eq!(after.loop_mode, LoopMode::Off);
        // A loop left with a single frame is turned off.
        assert_eq!(crop(599, 700).unwrap().loop_mode, LoopMode::Off);
        // A crossfade longer than the clipped loop allows is shortened.
        let mut faded = looped(1000, 999, 150, 700);
        faded.crossfade = 100;
        let c = crop_to_selection(&faded, (200, 599), 1, 1000).unwrap();
        assert!(c.crossfade <= c.max_crossfade());
        assert_eq!(c.clone().clamped(1000), c);
    }

    #[test]
    fn crop_to_selection_at_the_file_edges_and_of_the_whole_file() {
        let e = EditState::default_for(1000);
        let head = crop_to_selection(&e, (0, 499), 1, 1000).unwrap();
        assert_eq!(head.effects.len(), 1);
        assert_eq!(frames_after(1000, &head.effects), 500);
        let tail = crop_to_selection(&e, (500, 999), 1, 1000).unwrap();
        assert_eq!(tail.effects.len(), 1);
        assert_eq!(frames_after(1000, &tail.effects), 500);
        // A selection past the end is clamped to the file.
        let past = crop_to_selection(&e, (500, 5000), 1, 1000).unwrap();
        assert_eq!(past, tail);
        assert!(crop_to_selection(&e, (0, 999), 1, 1000).is_err());
        assert!(crop_to_selection(&EditState::default_for(0), (0, 0), 1, 0).is_err());
    }

    #[test]
    fn crop_to_selection_follows_an_earlier_splice() {
        let audio = ramps(1000, 1);
        let mut e = EditState::default_for(1000);
        e.effects.push(AppliedEffect {
            op: Op::Splice(Arc::new(Splice {
                kind: SpliceKind::Tail,
                audio: vec![(0..600).map(|i| -(i as f32)).collect()],
                generated: (0, 599),
            })),
            first: 900,
            last: 999,
        });
        let e = e.clamped(1000);
        let c = crop_to_selection(&e, (1000, 1499), 1, 1000).unwrap();
        let out = crate::effects::process(&audio, &c.effects);
        let want: Vec<f32> = (100..600).map(|i| -(i as f32)).collect();
        assert_eq!(out[0], want);
        assert_eq!((c.start, c.end), (0, 499));
    }

    fn quiet(frames: usize, channels: usize) -> Vec<Vec<f32>> {
        (0..channels)
            .map(|c| (0..frames).map(|i| (c * 1000 + i) as f32 / 1e4).collect())
            .collect()
    }

    #[test]
    fn redraw_samples_changes_only_the_drawn_channel_and_frames() {
        let audio = quiet(100, 2);
        let e = EditState::default_for(100);
        let r = redraw_samples(&e, &audio, 1, 40, &[0.5, -0.25]).unwrap();
        assert_eq!(r.effects.len(), 1);
        let out = crate::effects::process(&audio, &r.effects);
        let mut want = audio.clone();
        want[1][40] = 0.5;
        want[1][41] = -0.25;
        assert_eq!(out, want);
        // Hand-drawn samples are not tinted as generated audio.
        assert!(crate::effects::generated_ranges(&r.effects).is_empty());
        assert_eq!(r.clone().clamped(100), r);
    }

    #[test]
    fn redraw_samples_keeps_values_within_full_scale() {
        let audio = quiet(10, 1);
        let e = EditState::default_for(10);
        let r = redraw_samples(&e, &audio, 0, 3, &[1.5, -2.0]).unwrap();
        let out = crate::effects::process(&audio, &r.effects);
        assert_eq!(&out[0][3..5], &[1.0, -1.0]);
    }

    #[test]
    fn redraw_samples_refuses_no_change_and_out_of_range() {
        let audio = quiet(10, 1);
        let e = EditState::default_for(10);
        assert!(redraw_samples(&e, &audio, 0, 2, &[audio[0][2]]).is_none());
        assert!(redraw_samples(&e, &audio, 0, 2, &[]).is_none());
        assert!(redraw_samples(&e, &audio, 1, 2, &[0.5]).is_none());
        assert!(redraw_samples(&e, &audio, 0, 9, &[0.5, 0.5]).is_none());
        assert!(redraw_samples(&e, &audio, 0, 2, &[f32::NAN]).is_none());
    }

    #[test]
    fn rate_after_takes_the_last_conversion() {
        assert_eq!(rate_after(44_100, &[]), 44_100);
        let e = resample_edit(
            &EditState::default_for(4410),
            &[vec![0.0; 4410]],
            31_250,
            44_100,
            4410,
        )
        .unwrap();
        assert_eq!(rate_after(44_100, &e.effects), 31_250);
        // A second conversion starts from the first one's rate.
        let again = resample_edit(&e, &[vec![0.0; 3125]], 62_500, 44_100, 4410).unwrap();
        assert_eq!(rate_after(44_100, &again.effects), 62_500);
        let Op::Resample(ref r) = again.effects[1].op else {
            panic!("expected a conversion");
        };
        assert_eq!((r.from, r.to), (31_250, 62_500));
    }

    #[test]
    fn a_conversion_scales_markers_loop_and_crossfade() {
        let mut e = EditState::default_for(44_100);
        (
            e.start,
            e.end,
            e.loop_mode,
            e.loop_start,
            e.loop_end,
            e.crossfade,
        ) = (100, 40_000, LoopMode::Forward, 1_000, 30_000, 441);
        let r = resample_edit(&e, &[vec![0.0; 44_100]], 22_050, 44_100, 44_100).unwrap();
        assert_eq!(
            (r.start, r.end, r.loop_start, r.loop_end, r.crossfade),
            (50, 20_000, 500, 15_000, 221)
        );
        assert_eq!(frames_after(44_100, &r.effects), 22_050);
    }

    #[test]
    fn the_same_rate_or_an_out_of_range_rate_is_no_conversion() {
        let e = EditState::default_for(100);
        assert!(resample_edit(&e, &[vec![0.0; 100]], 44_100, 44_100, 100).is_none());
        assert!(resample_edit(&e, &[vec![0.0; 100]], 999, 44_100, 100).is_none());
        assert!(resample_edit(&e, &[vec![0.0; 100]], 768_001, 44_100, 100).is_none());
        // Nothing to convert, or a current rate of zero to convert from.
        assert!(resample_edit(&e, &[], 22_050, 44_100, 100).is_none());
        assert!(resample_edit(&e, &[Vec::new()], 22_050, 44_100, 100).is_none());
        assert!(resample_edit(&e, &[vec![0.0; 100]], 22_050, 0, 100).is_none());
    }

    #[test]
    fn a_tiny_rate_turns_a_short_loop_off_without_panicking() {
        let mut e = EditState::default_for(96_000);
        (e.loop_mode, e.loop_start, e.loop_end) = (LoopMode::Forward, 10, 60);
        let r = resample_edit(&e, &[vec![0.0; 96_000]], 1_000, 96_000, 96_000).unwrap();
        assert_eq!(r.loop_mode, LoopMode::Off);
        assert_eq!(r.end, 999);
        assert!(r.loop_start <= r.loop_end && r.loop_end <= r.end);
        assert!(r.start <= r.end);
    }

    #[test]
    fn crossing_markers_stay_in_order_at_a_tiny_rate() {
        let mut e = EditState::default_for(96_000);
        (e.start, e.end) = (50, 50);
        let r = resample_edit(&e, &[vec![0.0; 96_000]], 1_000, 96_000, 96_000).unwrap();
        assert!(r.start <= r.end);
        assert!(r.loop_start <= r.loop_end);
    }

    #[test]
    fn a_loop_the_mapping_squeezes_under_two_frames_is_turned_off() {
        let mut e = EditState::default_for(100);
        (e.loop_mode, e.loop_start, e.loop_end) = (LoopMode::Forward, 1, 3);
        let r = resample_edit(&e, &[vec![0.0; 100]], 1000, 2000, 100).unwrap();
        assert_eq!(r.loop_mode, LoopMode::Off);
    }

    #[test]
    fn an_end_on_the_last_frame_lands_on_the_new_last_frame() {
        let e = EditState::default_for(100);
        let r = resample_edit(&e, &[vec![0.0; 100]], 2000, 1000, 100).unwrap();
        assert_eq!((r.start, r.end, r.loop_end), (0, 199, 199));
        assert_eq!(frames_after(100, &r.effects), 200);
    }

    #[test]
    fn a_loop_keeps_its_length_to_within_a_frame() {
        let mut e = EditState::default_for(100);
        (e.loop_mode, e.loop_start, e.loop_end) = (LoopMode::Forward, 10, 29);
        let r = resample_edit(&e, &[vec![0.0; 100]], 1500, 1000, 100).unwrap();
        assert_eq!(r.loop_mode, LoopMode::Forward);
        let len = r.loop_end + 1 - r.loop_start;
        assert!((29..=31).contains(&len), "loop is {len} frames");
    }

    #[test]
    fn frames_map_through_a_conversion() {
        let r = resample_edit(
            &EditState::default_for(1000),
            &[vec![0.0; 1000]],
            1000,
            2000,
            1000,
        )
        .unwrap();
        assert_eq!(map_frame(&r.effects, 3), Some(2)); // 1.5 rounds up
        assert_eq!(map_frame(&r.effects, 1000), Some(500)); // end of file lands on the new length
        assert_eq!(map_range(&r.effects, 10, 21), Some((5, 11)));
        // The last frame is held inside the new audio; non-conversion steps are ignored.
        assert_eq!(map_frame(&r.effects, 999), Some(499));
        assert_eq!(resample_map(&r.effects, 999), 499);
        assert_eq!(resample_map(&[], 7), 7);
    }

    #[test]
    fn replaying_a_conversion_gives_its_audio() {
        let x = vec![(0..100).map(|i| i as f32 / 100.0).collect::<Vec<_>>()];
        let r = resample_edit(&EditState::default_for(100), &x, 2000, 1000, 100).unwrap();
        assert_eq!(process(&x, &r.effects)[0].len(), 200);
    }

    #[test]
    fn clamped_keeps_a_conversion_that_fits_its_input() {
        let r = resample_edit(
            &EditState::default_for(1000),
            &[vec![0.0; 1000]],
            1000,
            2000,
            1000,
        )
        .unwrap();
        assert_eq!(r.clone().clamped(1000), r);
        // Against a shorter original the step's range no longer fits and is dropped.
        assert!(r.clamped(500).effects.is_empty());
    }

    #[test]
    fn a_conversion_with_the_wrong_channel_count_is_skipped() {
        let step = AppliedEffect {
            op: Op::Resample(Arc::new(Resample {
                from: 1000,
                to: 2000,
                audio: vec![vec![0.0; 20]],
            })),
            first: 0,
            last: 9,
        };
        let two = vec![vec![0.5; 10]; 2];
        assert_eq!(process(&two, &[step]), two);
    }
}
