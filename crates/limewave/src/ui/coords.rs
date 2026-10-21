//! Frame/pixel mapping for the waveform views, plus marker hit testing.

use eframe::egui::{Pos2, Rect, Vec2, pos2};
use limewave_core::edit::{EditState, LoopMode, Marker};

/// The fewest frames per pixel (most zoomed in): 16 pixels per frame.
pub const MIN_FRAMES_PER_PX: f64 = 1.0 / 16.0;

/// Which part of the file a view shows: the frame at its left edge and the zoom.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewTransform {
    /// Frame (fractional) at the left edge.
    pub first: f64,
    pub frames_per_px: f64,
}

/// The widest zoom: the whole file across `width` pixels (never finer than the zoom limit).
fn max_frames_per_px(total: usize, width: f32) -> f64 {
    (total as f64 / f64::from(width.max(1.0))).max(MIN_FRAMES_PER_PX)
}

impl ViewTransform {
    /// A view of the whole file.
    pub fn fit(total: usize, width: f32) -> ViewTransform {
        ViewTransform {
            first: 0.0,
            frames_per_px: max_frames_per_px(total, width),
        }
    }

    pub fn frame_to_x(&self, f: f64, left: f32) -> f32 {
        left + ((f - self.first) / self.frames_per_px) as f32
    }

    pub fn x_to_frame(&self, x: f32, left: f32) -> f64 {
        self.first + f64::from(x - left) * self.frames_per_px
    }

    /// Zooms by `factor` (above 1 zooms in) keeping the frame under `x` where it is.
    pub fn zoom_around(&mut self, x: f32, left: f32, factor: f64, total: usize, width: f32) {
        if !(factor.is_finite() && factor > 0.0) {
            return;
        }
        let anchor = self.x_to_frame(x, left);
        self.frames_per_px =
            (self.frames_per_px / factor).clamp(MIN_FRAMES_PER_PX, max_frames_per_px(total, width));
        self.first = anchor - f64::from(x - left) * self.frames_per_px;
        *self = self.clamped(total, width);
    }

    /// Moves the view right by `px` pixels.
    pub fn scroll(&mut self, px: f32, total: usize, width: f32) {
        self.first += f64::from(px) * self.frames_per_px;
        *self = self.clamped(total, width);
    }

    /// This view forced to show part of the file at a legal zoom.
    pub fn clamped(self, total: usize, width: f32) -> ViewTransform {
        let max = max_frames_per_px(total, width);
        let frames_per_px = if self.frames_per_px.is_finite() {
            self.frames_per_px.clamp(MIN_FRAMES_PER_PX, max)
        } else {
            max
        };
        let last_first = (total as f64 - f64::from(width.max(1.0)) * frames_per_px).max(0.0);
        let first = if self.first.is_finite() {
            self.first.clamp(0.0, last_first)
        } else {
            0.0
        };
        ViewTransform {
            first,
            frames_per_px,
        }
    }
}

/// The marker nearest to pixel `x` within `tol_px`. Loop markers (shown only while the loop
/// is on) win ties against Start and End.
pub fn hit_marker(
    e: &EditState,
    t: &ViewTransform,
    left: f32,
    x: f32,
    tol_px: f32,
) -> Option<Marker> {
    let loops = e.loop_mode != LoopMode::Off;
    let candidates = [
        (Marker::LoopStart, e.loop_start, loops),
        (Marker::LoopEnd, e.loop_end, loops),
        (Marker::Start, e.start, true),
        (Marker::End, e.end, true),
    ];
    candidates
        .into_iter()
        .filter(|&(.., shown)| shown)
        .map(|(m, frame, _)| (m, (t.frame_to_x(frame as f64, left) - x).abs()))
        .filter(|&(_, dist)| dist <= tol_px)
        // `min_by` keeps the first of equal items, and loop markers come first.
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(m, _)| m)
}

/// The rectangle of a marker's flag: end-side flags hang left of `anchor`, start-side flags
/// extend right from it.
pub fn flag_rect(anchor: Pos2, flag_left: bool, size: Vec2) -> Rect {
    let min = if flag_left {
        pos2(anchor.x - size.x, anchor.y)
    } else {
        anchor
    };
    Rect::from_min_size(min, size)
}

/// The marker under `pos`: a flag containing it wins (the last drawn, topmost one among
/// overlapping flags), otherwise the marker whose line is near (`line`).
pub fn pick_marker(flags: &[(Marker, Rect)], line: Option<Marker>, pos: Pos2) -> Option<Marker> {
    flags
        .iter()
        .rev()
        .find(|(_, rect)| rect.contains(pos))
        .map(|&(m, _)| m)
        .or(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use limewave_core::edit::LoopMode;

    fn view(first: f64, fpp: f64) -> ViewTransform {
        ViewTransform {
            first,
            frames_per_px: fpp,
        }
    }

    #[test]
    fn frame_x_roundtrip() {
        let v = view(100.0, 0.5);
        for f in [100.0, 250.5, 1024.0, 3.25] {
            let back = v.x_to_frame(v.frame_to_x(f, 40.0), 40.0);
            assert!((back - f).abs() < 1e-6, "{f} came back as {back}");
        }
    }

    #[test]
    fn zoom_keeps_point_under_cursor() {
        let mut v = view(1000.0, 4.0);
        let (x, left) = (300.0, 20.0);
        let before = v.x_to_frame(x, left);
        v.zoom_around(x, left, 2.0, 100_000, 800.0);
        assert_eq!(v.frames_per_px, 2.0);
        assert!((v.x_to_frame(x, left) - before).abs() < 1e-6);
        v.zoom_around(x, left, 0.5, 100_000, 800.0);
        assert!((v.x_to_frame(x, left) - before).abs() < 1e-6);
    }

    #[test]
    fn zoom_and_scroll_clamped() {
        let (total, width) = (10_000, 500.0);
        let visible_end = |v: &ViewTransform| v.first + f64::from(width) * v.frames_per_px;
        let mut v = ViewTransform::fit(total, width);
        assert_eq!((v.first, v.frames_per_px), (0.0, 20.0));

        v.zoom_around(250.0, 0.0, 0.01, total, width);
        assert_eq!(v.frames_per_px, 20.0, "cannot show more than the file");
        v.zoom_around(250.0, 0.0, 1e9, total, width);
        assert_eq!(v.frames_per_px, MIN_FRAMES_PER_PX);

        v.scroll(-1e6, total, width);
        assert_eq!(v.first, 0.0);
        v.scroll(1e9, total, width);
        assert!(v.first >= 0.0 && visible_end(&v) <= total as f64 + 1e-6);
        assert!(visible_end(&v) > total as f64 - 1.0, "scrolled to the end");

        // An out-of-range transform is pulled back in.
        let v = view(-50.0, 1e6).clamped(total, width);
        assert_eq!((v.first, v.frames_per_px), (0.0, 20.0));
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (total, width) in [(0, 500.0), (500, 0.0), (0, 0.0), (1, -3.0)] {
            let mut v = ViewTransform::fit(total, width);
            v.zoom_around(10.0, 0.0, 2.0, total, width);
            v.scroll(50.0, total, width);
            let v = v.clamped(total, width);
            assert!(v.first.is_finite() && v.frames_per_px.is_finite());
            assert!(v.first >= 0.0 && v.frames_per_px >= MIN_FRAMES_PER_PX);
        }
    }

    #[test]
    fn flag_rect_hangs_left_for_end_side_flags() {
        let anchor = pos2(100.0, 30.0);
        let size = Vec2::new(30.0, 14.0);
        assert_eq!(flag_rect(anchor, false, size).min, anchor);
        let left = flag_rect(anchor, true, size);
        assert_eq!((left.min, left.max), (pos2(70.0, 30.0), pos2(100.0, 44.0)));
    }

    #[test]
    fn label_wins_over_a_nearby_line() {
        let start = Rect::from_min_size(pos2(10.0, 20.0), Vec2::new(30.0, 14.0));
        let end = Rect::from_min_size(pos2(30.0, 20.0), Vec2::new(30.0, 14.0));
        let flags = [(Marker::Start, start), (Marker::End, end)];
        let inside = pos2(15.0, 25.0);
        assert_eq!(
            pick_marker(&flags, Some(Marker::LoopStart), inside),
            Some(Marker::Start)
        );
        let outside = pos2(15.0, 60.0);
        assert_eq!(
            pick_marker(&flags, Some(Marker::LoopStart), outside),
            Some(Marker::LoopStart)
        );
        assert_eq!(pick_marker(&flags, None, outside), None);
        // Overlapping flags: the one drawn last is on top.
        assert_eq!(
            pick_marker(&flags, None, pos2(35.0, 25.0)),
            Some(Marker::End)
        );
    }

    fn edit(start: usize, loop_start: usize, loop_end: usize, end: usize) -> EditState {
        EditState {
            start,
            end,
            loop_mode: LoopMode::Forward,
            loop_start,
            loop_end,
            ..EditState::default_for(1000)
        }
    }

    #[test]
    fn hit_marker_prefers_loop_markers() {
        let e = edit(0, 0, 500, 999);
        let v = view(0.0, 1.0);
        assert_eq!(hit_marker(&e, &v, 10.0, 12.0, 6.0), Some(Marker::LoopStart));
        let e = edit(0, 100, 999, 999);
        assert_eq!(hit_marker(&e, &v, 0.0, 998.0, 6.0), Some(Marker::LoopEnd));
        // With the loop off only Start and End can be hit.
        let off = EditState {
            loop_mode: LoopMode::Off,
            ..edit(0, 0, 500, 999)
        };
        assert_eq!(hit_marker(&off, &v, 10.0, 12.0, 6.0), Some(Marker::Start));
        assert_eq!(hit_marker(&off, &v, 0.0, 500.0, 6.0), None);
    }

    #[test]
    fn hit_marker_picks_the_nearest_and_none_outside_tolerance() {
        let e = edit(100, 200, 205, 900);
        let v = view(0.0, 1.0);
        assert_eq!(hit_marker(&e, &v, 0.0, 203.0, 6.0), Some(Marker::LoopEnd));
        assert_eq!(hit_marker(&e, &v, 0.0, 201.0, 6.0), Some(Marker::LoopStart));
        assert_eq!(hit_marker(&e, &v, 0.0, 106.0, 6.0), Some(Marker::Start));
        assert_eq!(hit_marker(&e, &v, 0.0, 107.0, 6.0), None);
        assert_eq!(hit_marker(&e, &v, 0.0, 450.0, 6.0), None);
    }
}
