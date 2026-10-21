//! Edit state (trim and loop markers, tuning, effects), marker-move constraints and undo history.

use crate::effects::AppliedEffect;

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

    /// Forces the state to be valid for a file of `frames` frames.
    ///
    /// If a loop of at least 2 frames cannot fit, the loop is turned off.
    pub fn clamped(mut self, frames: usize) -> EditState {
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
/// clamped result. With `lock_length`, moving either loop marker shifts both.
pub fn move_marker(
    e: &EditState,
    m: Marker,
    to: usize,
    lock_length: bool,
    frames: usize,
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
    s.clamped(frames)
}

/// Linear undo/redo history of edit states, tracking which one was last saved.
pub struct History {
    entries: Vec<EditState>,
    cursor: usize,
    /// Index of the saved entry; `None` once a push dropped it from the history.
    saved: Option<usize>,
}

impl History {
    pub fn new(initial: EditState) -> History {
        History {
            entries: vec![initial],
            cursor: 0,
            saved: Some(0),
        }
    }

    pub fn current(&self) -> &EditState {
        &self.entries[self.cursor]
    }

    /// Records a new state, dropping any redo tail. A no-op if equal to the current state.
    pub fn push(&mut self, s: EditState) {
        if *self.current() == s {
            return;
        }
        self.entries.truncate(self.cursor + 1);
        if self.saved.is_some_and(|i| i > self.cursor) {
            self.saved = None;
        }
        self.entries.push(s);
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
    use crate::effects::Op;

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
    fn history_push_equal_is_noop() {
        let init = EditState::default_for(100);
        let mut h = History::new(init.clone());
        h.push(init);
        assert!(!h.undo());
        assert!(!h.is_dirty());
    }
}
