//! The whole-file strip above the waveform: shows the visible range as a box you can drag.

use eframe::egui::{
    self, Color32, CursorIcon, PointerButton, Rect, Role, Sense, Shape, Stroke, StrokeKind,
    WidgetInfo, pos2, vec2,
};
use seamlime_core::edit::LoopMode;

use super::coords::ViewTransform;
use crate::app::Session;
use crate::theme::colors;

/// Height of the strip in points.
pub const OVERVIEW_HEIGHT: f32 = 40.0;

const BACKGROUND: Color32 = Color32::from_rgb(0x15, 0x17, 0x1d);

/// Where inside the visible box a drag grabbed it, in frames.
#[derive(Clone, Copy)]
struct Grab(f64);

fn with_alpha(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

pub fn overview(ui: &mut egui::Ui, s: &mut Session) {
    let width = ui.available_width().max(1.0);
    let (rect, response) =
        ui.allocate_exact_size(vec2(width, OVERVIEW_HEIGHT), Sense::click_and_drag());
    response.widget_info(|| WidgetInfo::labeled(Role::Canvas, true, "Overview"));
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, BACKGROUND);

    let total = s.doc.frames;
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

    // Dragging the box scrolls; pressing elsewhere centres the box on the pointer.
    let grab_id = response.id.with("grab");
    if response.drag_started_by(PointerButton::Primary)
        && let Some(pos) = ui.input(|i| i.pointer.press_origin())
    {
        let at = frame_at(pos.x);
        let inside = at >= view.first && at <= view.first + visible;
        let grab = if inside {
            at - view.first
        } else {
            visible / 2.0
        };
        ui.data_mut(|d| d.insert_temp(grab_id, Grab(grab)));
    }
    if response.dragged_by(PointerButton::Primary)
        && let Some(pos) = response.interact_pointer_pos()
        && let Some(Grab(grab)) = ui.data(|d| d.get_temp(grab_id))
    {
        view.first = frame_at(pos.x) - grab;
    } else if response.clicked_by(PointerButton::Primary)
        && let Some(pos) = response.interact_pointer_pos()
    {
        view.first = frame_at(pos.x) - visible / 2.0;
    }
    let view = view.clamped(total, width);
    s.view = Some(view);

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
            .zip(&s.doc.audio)
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

    // The loop and the trim markers.
    let e = s.shown_edit();
    let x_of = |f: usize| left + (f as f64 / frames_per_px_overview) as f32;
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
    marker(e.start, colors::START);
    marker(e.end, colors::END);

    // The visible range.
    let x0 = left + (view.first / frames_per_px_overview) as f32;
    let x1 = (x0 + (visible / frames_per_px_overview) as f32).max(x0 + 3.0);
    let view_box = Rect::from_x_y_ranges(x0..=x1, rect.y_range());
    painter.rect_filled(view_box, 2.0, with_alpha(colors::NEON, 30));
    painter.rect_stroke(
        view_box,
        2.0,
        Stroke::new(1.5, colors::NEON),
        StrokeKind::Inside,
    );
    if response.dragged() {
        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    } else if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::Grab);
    }
}
