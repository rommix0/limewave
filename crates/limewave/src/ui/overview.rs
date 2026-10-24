//! The whole-file strip above the waveform: shows the visible range as a box you can drag to
//! scroll, or resize by its edges to zoom.

use eframe::egui::{
    self, Color32, CursorIcon, PointerButton, Rect, Role, Sense, Shape, Stroke, StrokeKind,
    WidgetInfo, pos2, vec2,
};
use limewave_core::edit::LoopMode;

use super::coords::ViewTransform;
use super::waveform::cursor_color;
use crate::app::Session;
use crate::audio::engine::Cursor;
use crate::theme::colors;

/// Height of the strip in points.
pub const OVERVIEW_HEIGHT: f32 = 40.0;

const BACKGROUND: Color32 = Color32::from_rgb(0x15, 0x17, 0x1d);

/// What a drag on the strip holds.
#[derive(Clone, Copy)]
enum Grab {
    /// The box, this many frames from its left end: dragging scrolls.
    Move(f64),
    /// The box's left edge, with the right end (a frame) kept in place: dragging zooms.
    Left(f64),
    /// The box's right edge, with the left end (a frame) kept in place: dragging zooms.
    Right(f64),
}

/// How close to an edge of the box, in points, a press grabs that edge.
const EDGE_GRAB_PX: f32 = 5.0;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Edge {
    Left,
    Right,
}

/// Which edge of the box spanning `x0..=x1` a press at `x` grabs, if any. A box too narrow to
/// have an inside left to scroll by is only resized from just outside its edges.
fn edge_at(x: f32, x0: f32, x1: f32) -> Option<Edge> {
    let roomy = x1 - x0 > 3.0 * EDGE_GRAB_PX;
    let near = |edge: f32| (x - edge).abs() <= EDGE_GRAB_PX;
    if near(x0) && (roomy || x < x0) && (x - x0).abs() <= (x - x1).abs() {
        Some(Edge::Left)
    } else if near(x1) && (roomy || x > x1) {
        Some(Edge::Right)
    } else {
        None
    }
}

fn with_alpha(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

pub fn overview(ui: &mut egui::Ui, s: &mut Session, cursors: &[Cursor]) {
    let width = ui.available_width().max(1.0);
    let (rect, response) =
        ui.allocate_exact_size(vec2(width, OVERVIEW_HEIGHT), Sense::click_and_drag());
    response.widget_info(|| WidgetInfo::labeled(Role::Canvas, true, "Overview"));
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, BACKGROUND);

    let total = s.frames();
    if total == 0 {
        return;
    }
    let left = rect.left();
    let frames_per_px_overview = total as f64 / f64::from(width);
    let mut view = s
        .view
        .unwrap_or_else(|| ViewTransform::fit(total, width))
        .clamped(total, width);
    let visible = f64::from(width) * view.frames_per_px;
    let frame_at = |x: f32| f64::from(x - left) * frames_per_px_overview;

    let box_x = |view: &ViewTransform| {
        let x0 = left + (view.first / frames_per_px_overview) as f32;
        let x1 = x0 + (f64::from(width) * view.frames_per_px / frames_per_px_overview) as f32;
        (x0, x1.max(x0 + 3.0))
    };

    // Dragging an edge of the box zooms with the other end kept in place; dragging the box
    // scrolls; pressing elsewhere centres the box on the pointer.
    let grab_id = response.id.with("grab");
    let (x0, x1) = box_x(&view);
    if response.drag_started_by(PointerButton::Primary)
        && let Some(pos) = ui.input(|i| i.pointer.press_origin())
    {
        let at = frame_at(pos.x);
        let grab = match edge_at(pos.x, x0, x1) {
            Some(Edge::Left) => Grab::Left(view.first + visible),
            Some(Edge::Right) => Grab::Right(view.first),
            _ if at >= view.first && at <= view.first + visible => Grab::Move(at - view.first),
            _ => Grab::Move(visible / 2.0),
        };
        ui.data_mut(|d| d.insert_temp(grab_id, grab));
    }
    let mut resizing = false;
    if response.dragged_by(PointerButton::Primary)
        && let Some(pos) = response.interact_pointer_pos()
        && let Some(grab) = ui.data(|d| d.get_temp::<Grab>(grab_id))
    {
        let at = frame_at(pos.x).clamp(0.0, total as f64);
        let span_px = f64::from(width);
        match grab {
            Grab::Move(grab) => view.first = frame_at(pos.x) - grab,
            Grab::Left(right) => {
                view.frames_per_px = (right - at) / span_px;
                view = view.clamped(total, width);
                view.first = right - span_px * view.frames_per_px;
                resizing = true;
            }
            Grab::Right(left) => {
                view.frames_per_px = (at - left) / span_px;
                view = view.clamped(total, width);
                view.first = left;
                resizing = true;
            }
        }
    } else if response.clicked_by(PointerButton::Primary)
        && let Some(pos) = response.interact_pointer_pos()
    {
        view.first = frame_at(pos.x) - visible / 2.0;
    }
    let view = view.clamped(total, width);
    s.view = Some(view);
    let over_edge = response
        .hover_pos()
        .is_some_and(|p| edge_at(p.x, x0, x1).is_some());

    // Peaks of every channel, one column per pixel.
    let mid = rect.center().y;
    let half = rect.height() * 0.45;
    let columns = width.ceil() as usize;
    let wave = with_alpha(colors::WAVE, 170);
    let mut shapes = Vec::with_capacity(columns);
    for col in 0..columns {
        let from = (col as f64 * frames_per_px_overview) as usize;
        let to = (((col + 1) as f64 * frames_per_px_overview).ceil() as usize).max(from + 1);
        let (lo, hi) = s
            .peaks
            .iter()
            .zip(&s.processed)
            .map(|(p, c)| p.min_max(c, from, to))
            .fold((0.0f32, 0.0f32), |(lo, hi), (a, b)| (lo.min(a), hi.max(b)));
        let x = left + col as f32 + 0.5;
        let top = mid - hi.clamp(-1.0, 1.0) * half;
        let bottom = (mid - lo.clamp(-1.0, 1.0) * half).max(top + 1.0);
        shapes.push(Shape::line_segment(
            [pos2(x, top), pos2(x, bottom)],
            Stroke::new(1.0, wave),
        ));
    }
    painter.extend(shapes);

    // Generated audio, the loop and the trim markers.
    let x_of = |f: usize| left + (f as f64 / frames_per_px_overview) as f32;
    for (first, last) in s.tint_ranges() {
        let tint = Rect::from_x_y_ranges(x_of(first)..=x_of(last + 1), rect.y_range());
        painter.rect_filled(tint, 0.0, colors::generated_tint());
    }
    let e = s.shown_edit();
    let marker = |f: usize, c: Color32| {
        let x = x_of(f).clamp(left, rect.right());
        painter.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(1.0, c),
        );
    };
    if e.loop_mode != LoopMode::Off {
        let region = Rect::from_x_y_ranges(x_of(e.loop_start)..=x_of(e.loop_end), rect.y_range());
        painter.rect_filled(region, 0.0, with_alpha(colors::LOOP, 26));
        marker(e.loop_start, colors::LOOP);
        marker(e.loop_end, colors::LOOP);
    }
    super::loop_finder::draw_finder_overlay(&painter, rect, x_of, s);
    marker(e.start, colors::START);
    marker(e.end, colors::END);

    for c in cursors {
        let x = left + (c.frame / frames_per_px_overview) as f32;
        if (left..=rect.right()).contains(&x) {
            painter.vline(x, rect.y_range(), Stroke::new(1.0, cursor_color(c)));
        }
    }

    // The visible range.
    let (x0, x1) = box_x(&view);
    let view_box = Rect::from_x_y_ranges(x0..=x1, rect.y_range());
    painter.rect_filled(view_box, 2.0, with_alpha(colors::NEON, 30));
    painter.rect_stroke(
        view_box,
        2.0,
        Stroke::new(1.5, colors::NEON),
        StrokeKind::Inside,
    );
    if resizing || (over_edge && !response.dragged()) {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    } else if response.dragged() {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    } else if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::Grab);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_are_grabbed_near_the_box_ends() {
        // A roomy box from 100 to 300.
        assert_eq!(edge_at(97.0, 100.0, 300.0), Some(Edge::Left));
        assert_eq!(edge_at(104.0, 100.0, 300.0), Some(Edge::Left));
        assert_eq!(edge_at(296.0, 100.0, 300.0), Some(Edge::Right));
        assert_eq!(edge_at(305.0, 100.0, 300.0), Some(Edge::Right));
        assert_eq!(edge_at(200.0, 100.0, 300.0), None);
        assert_eq!(edge_at(90.0, 100.0, 300.0), None);
    }

    #[test]
    fn a_narrow_box_is_resized_only_from_outside_its_edges() {
        // 8 points wide: pressing inside scrolls it.
        assert_eq!(edge_at(103.0, 100.0, 108.0), None);
        assert_eq!(edge_at(97.0, 100.0, 108.0), Some(Edge::Left));
        assert_eq!(edge_at(111.0, 100.0, 108.0), Some(Edge::Right));
    }
}
