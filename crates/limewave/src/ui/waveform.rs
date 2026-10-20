//! The main waveform: time ruler, one lane per channel, markers, and the mouse handling for
//! dragging markers, zooming and scrolling.

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, PointerButton, Pos2, Rect, Role, Sense, Shape,
    Stroke, WidgetInfo, pos2, vec2,
};
use limewave_core::dsp::{PeakPyramid, snap_loop_end, snap_loop_start};
use limewave_core::edit::{EditState, LoopMode, Marker, move_marker};

use super::coords::{ViewTransform, hit_marker};
use crate::app::{Session, UiState};
use crate::audio::engine::AudioEngine;
use crate::theme::colors;

const BACKGROUND: Color32 = Color32::from_rgb(0x15, 0x17, 0x1d);
const RULER_HEIGHT: f32 = 22.0;
/// How close (in pixels) a press must be to grab a marker.
const GRAB_TOLERANCE: f32 = 6.0;
/// One wheel notch in points, as egui reports it.
const POINTS_PER_NOTCH: f32 = 50.0;
/// Zoom per wheel notch.
const ZOOM_PER_NOTCH: f64 = 1.25;
/// Pixels between ruler labels, at least.
const RULER_LABEL_SPACING: f64 = 90.0;

/// The marker being dragged and how far (in frames) the marker was from the pointer when
/// grabbed.
#[derive(Clone, Copy)]
struct Drag {
    marker: Marker,
    grab_offset: f64,
}

fn with_alpha(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

/// The snap search window: 2 ms of frames, at least one.
fn snap_window(rate: u32) -> usize {
    ((rate as f32 * 0.002).round() as usize).max(1)
}

fn marker_frame(e: &EditState, m: Marker) -> usize {
    match m {
        Marker::Start => e.start,
        Marker::End => e.end,
        Marker::LoopStart => e.loop_start,
        Marker::LoopEnd => e.loop_end,
    }
}

/// Draws the waveform of `s` filling the available space and handles its input.
pub fn waveform_view(
    ui: &mut egui::Ui,
    s: &mut Session,
    ui_state: &mut UiState,
    engine: Option<&AudioEngine>,
) {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    response.widget_info(|| WidgetInfo::labeled(Role::Canvas, true, "Waveform"));
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, BACKGROUND);

    let total = s.doc.frames;
    let width = rect.width();
    let left = rect.left();
    if total == 0 || width < 1.0 || rect.height() <= RULER_HEIGHT {
        return;
    }
    let mut view = s
        .view
        .unwrap_or_else(|| ViewTransform::fit(total, width))
        .clamped(total, width);

    // Wheel: zoom around the cursor; horizontal scroll moves the view.
    if response.hovered()
        && let Some(pos) = response.hover_pos()
    {
        let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
        let factor = ZOOM_PER_NOTCH.powf(f64::from(scroll.y / POINTS_PER_NOTCH)) * f64::from(zoom);
        if factor != 1.0 {
            view.zoom_around(pos.x, left, factor, total, width);
        }
        if scroll.x != 0.0 {
            view.scroll(-scroll.x, total, width);
        }
    }

    // Markers: grab on press, follow the pointer, one history entry on release.
    let drag_id = response.id.with("drag");
    if response.drag_started_by(PointerButton::Primary)
        && let Some(press) = ui.input(|i| i.pointer.press_origin())
    {
        let e = s.shown_edit();
        let drag = hit_marker(e, &view, left, press.x, GRAB_TOLERANCE).map(|marker| Drag {
            marker,
            grab_offset: marker_frame(e, marker) as f64 - view.x_to_frame(press.x, left),
        });
        ui.data_mut(|d| d.insert_temp(drag_id, drag));
    }
    let drag: Option<Drag> = ui.data(|d| d.get_temp::<Option<Drag>>(drag_id)).flatten();
    if let Some(drag) = drag {
        if response.dragged_by(PointerButton::Primary)
            && let Some(pos) = response.interact_pointer_pos()
        {
            let target = view.x_to_frame(pos.x, left) + drag.grab_offset;
            let shift = ui.input(|i| i.modifiers.shift);
            let moved = dragged_edit(s, ui_state, drag.marker, target, ui_state.snap && !shift);
            s.set_preview(moved, engine);
        }
        if response.drag_stopped() {
            ui.data_mut(|d| d.insert_temp(drag_id, None::<Drag>));
            s.commit_preview(engine);
        }
    }

    s.view = Some(view);

    let hovered = match drag {
        Some(drag) => Some(drag.marker),
        None => response
            .hover_pos()
            .and_then(|p| hit_marker(s.shown_edit(), &view, left, p.x, GRAB_TOLERANCE)),
    };
    if hovered.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    }

    let ruler_color = ui.visuals().weak_text_color();
    draw(&painter, rect, s, &view, hovered, ruler_color);
}

/// The edit after dragging `marker` to the (fractional) frame `target`, snapped onto an
/// upward zero crossing when `snap` is set and the marker is a loop marker.
fn dragged_edit(
    s: &Session,
    ui_state: &UiState,
    marker: Marker,
    target: f64,
    snap: bool,
) -> EditState {
    let mut to = target.round().max(0.0) as usize;
    let window = snap_window(s.doc.format.sample_rate);
    if snap {
        match marker {
            Marker::LoopStart => to = snap_loop_start(&s.mono, to, window),
            Marker::LoopEnd => to = snap_loop_end(&s.mono, to, window),
            Marker::Start | Marker::End => {}
        }
    }
    move_marker(s.edit(), marker, to, ui_state.lock_length, s.doc.frames)
}

fn draw(
    painter: &egui::Painter,
    rect: Rect,
    s: &Session,
    view: &ViewTransform,
    hovered: Option<Marker>,
    ruler_color: Color32,
) {
    let left = rect.left();
    let width = rect.width();
    let e = s.shown_edit();
    let lanes_top = rect.top() + RULER_HEIGHT;
    let lanes = Rect::from_min_max(pos2(left, lanes_top), rect.max);
    let loop_on = e.loop_mode != LoopMode::Off;
    let x_of = |f: usize| view.frame_to_x(f as f64, left);
    let visible_x = |f: usize| x_of(f).clamp(left, rect.right());

    // Lanes, one per channel.
    let channels = s.doc.audio.len().max(1);
    let lane_height = lanes.height() / channels as f32;
    for (i, (channel, peaks)) in s.doc.audio.iter().zip(&s.peaks).enumerate() {
        let lane = Rect::from_min_size(
            pos2(left, lanes_top + i as f32 * lane_height),
            vec2(width, lane_height),
        );
        if i > 0 {
            painter.hline(lane.x_range(), lane.top(), Stroke::new(1.0, colors::PANEL));
        }
        painter.hline(
            lane.x_range(),
            lane.center().y,
            Stroke::new(1.0, with_alpha(Color32::WHITE, 16)),
        );
        draw_lane(painter, lane, view, channel, peaks);
    }

    // Outside the trim, and the loop region.
    let shade = with_alpha(Color32::BLACK, 90);
    let before = Rect::from_x_y_ranges(left..=visible_x(e.start), lanes.y_range());
    let after = Rect::from_x_y_ranges(visible_x(e.end)..=rect.right(), lanes.y_range());
    painter.rect_filled(before, 0.0, shade);
    painter.rect_filled(after, 0.0, shade);
    if loop_on {
        let region = Rect::from_x_y_ranges(
            visible_x(e.loop_start)..=visible_x(e.loop_end),
            lanes.y_range(),
        );
        painter.rect_filled(region, 0.0, with_alpha(colors::LOOP, 26));
        draw_crossfade(painter, lanes, view, e);
    }

    draw_ruler(painter, rect, view, s.doc.format.sample_rate, ruler_color);

    // Markers: loop markers only while the loop is on.
    let mut markers = vec![
        (Marker::Start, colors::START, "START", 0),
        (Marker::End, colors::END, "END", 0),
    ];
    if loop_on {
        markers.push((Marker::LoopStart, colors::LOOP, "LOOP", 1));
        markers.push((Marker::LoopEnd, colors::LOOP, "LOOP", 1));
    }
    for (marker, color, label, row) in markers {
        let x = x_of(marker_frame(e, marker));
        if x < left - 1.0 || x > rect.right() + 1.0 {
            continue;
        }
        let width = if hovered == Some(marker) { 2.5 } else { 1.5 };
        painter.vline(x, rect.y_range(), Stroke::new(width, color));
        // End-side flags hang to the left of their line, start-side flags to the right.
        let flag_left = matches!(marker, Marker::End | Marker::LoopEnd);
        let anchor = pos2(x, lanes_top + 2.0 + row as f32 * 15.0);
        draw_flag(painter, anchor, flag_left, label, color);
    }
}

fn draw_flag(painter: &egui::Painter, anchor: Pos2, flag_left: bool, label: &str, color: Color32) {
    let galley =
        painter.layout_no_wrap(label.to_owned(), FontId::proportional(10.0), colors::PANEL);
    let size = galley.size() + vec2(8.0, 2.0);
    let min = if flag_left {
        pos2(anchor.x - size.x, anchor.y)
    } else {
        anchor
    };
    let flag = Rect::from_min_size(min, size);
    painter.rect_filled(flag, 2.0, color);
    painter.galley(flag.min + vec2(4.0, 1.0), galley, colors::PANEL);
}

/// The crossfade as a ramp over the frames it replaces, ending at the loop end.
fn draw_crossfade(painter: &egui::Painter, lanes: Rect, view: &ViewTransform, e: &EditState) {
    if e.crossfade == 0 {
        return;
    }
    let after_end = e.loop_end + 1;
    let x1 = view.frame_to_x(after_end as f64, lanes.left());
    let x0 = view.frame_to_x(after_end.saturating_sub(e.crossfade) as f64, lanes.left());
    if x1 < lanes.left() || x0 > lanes.right() {
        return;
    }
    let ramp = vec![
        pos2(x0, lanes.bottom()),
        pos2(x1, lanes.bottom()),
        pos2(x1, lanes.top()),
    ];
    painter.add(Shape::convex_polygon(
        ramp,
        with_alpha(colors::XFADE, 70),
        Stroke::new(1.5, colors::XFADE),
    ));
}

/// One channel: a min/max bar per pixel column, or the real samples when zoomed in.
fn draw_lane(
    painter: &egui::Painter,
    lane: Rect,
    view: &ViewTransform,
    channel: &[f32],
    peaks: &PeakPyramid,
) {
    let left = lane.left();
    let mid = lane.center().y;
    let half = lane.height() * 0.45;
    let y_of = |v: f32| mid - v.clamp(-1.0, 1.0) * half;
    let stroke = Stroke::new(1.0, colors::WAVE);
    let total = channel.len();

    if view.frames_per_px < 1.0 {
        let first = (view.x_to_frame(lane.left(), left).floor().max(1.0) as usize - 1).min(total);
        let last = (view.x_to_frame(lane.right(), left).ceil().max(0.0) as usize + 1).min(total);
        let points: Vec<Pos2> = (first..last)
            .map(|f| pos2(view.frame_to_x(f as f64, left), y_of(channel[f])))
            .collect();
        let spacing = (1.0 / view.frames_per_px) as f32;
        let radius = (spacing / 3.0).clamp(1.0, 2.5);
        painter.extend(
            points
                .iter()
                .map(|&p| Shape::circle_filled(p, radius, colors::WAVE)),
        );
        painter.add(Shape::line(points, stroke));
        return;
    }

    let columns = lane.width().ceil() as usize;
    let mut bars = Vec::with_capacity(columns);
    for col in 0..columns {
        let x = left + col as f32;
        let f0 = view.x_to_frame(x, left);
        let f1 = view.x_to_frame(x + 1.0, left);
        if f1 <= 0.0 {
            continue;
        }
        if f0 >= total as f64 {
            break;
        }
        let from = f0.max(0.0).floor() as usize;
        let to = (f1.ceil() as usize).max(from + 1);
        let (lo, hi) = peaks.min_max(channel, from, to);
        let top = y_of(hi);
        let bottom = y_of(lo).max(top + 1.0);
        bars.push(Shape::line_segment(
            [pos2(x + 0.5, top), pos2(x + 0.5, bottom)],
            stroke,
        ));
    }
    painter.extend(bars);
}

/// Tick positions as `(x offset from the left edge, label)` for the time ruler.
pub fn ruler_ticks(view: &ViewTransform, rate: u32, width: f32) -> Vec<(f32, String)> {
    let rate = f64::from(rate.max(1));
    let step = nice_step(RULER_LABEL_SPACING * view.frames_per_px / rate);
    let decimals = (-step.log10().floor()).max(0.0) as usize;
    let first = view.first / rate;
    let last = view.x_to_frame(width, 0.0) / rate;
    let mut ticks = Vec::new();
    let mut k = (first / step).ceil().max(0.0) as u64;
    while k as f64 * step <= last && ticks.len() < 1000 {
        let t = k as f64 * step;
        ticks.push((view.frame_to_x(t * rate, 0.0), format!("{t:.decimals$}s")));
        k += 1;
    }
    ticks
}

/// The smallest 1, 2 or 5 times a power of ten that is at least `min`.
fn nice_step(min: f64) -> f64 {
    if !(min.is_finite() && min > 0.0) {
        return 1.0;
    }
    let magnitude = 10f64.powf(min.log10().floor());
    [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|m| m * magnitude)
        .find(|&step| step >= min)
        .unwrap_or(10.0 * magnitude)
}

fn draw_ruler(
    painter: &egui::Painter,
    rect: Rect,
    view: &ViewTransform,
    rate: u32,
    color: Color32,
) {
    let bar = Rect::from_min_size(rect.min, vec2(rect.width(), RULER_HEIGHT));
    painter.rect_filled(bar, 0.0, colors::PANEL);
    for (dx, label) in ruler_ticks(view, rate, rect.width()) {
        let x = rect.left() + dx;
        painter.vline(
            x,
            (bar.bottom() - 6.0)..=bar.bottom(),
            Stroke::new(1.0, color),
        );
        painter.text(
            pos2(x + 3.0, bar.top() + 2.0),
            Align2::LEFT_TOP,
            label,
            FontId::proportional(10.0),
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_steps_follow_the_one_two_five_ladder() {
        for (min, want) in [
            (0.8, 1.0),
            (1.0, 1.0),
            (1.1, 2.0),
            (3.0, 5.0),
            (7.0, 10.0),
            (0.03, 0.05),
        ] {
            assert!((nice_step(min) - want).abs() < 1e-12, "{min}");
        }
        assert_eq!(nice_step(f64::NAN), 1.0);
        assert_eq!(nice_step(0.0), 1.0);
    }

    #[test]
    fn ruler_labels_have_sensible_precision() {
        // 10 frames per pixel at 1 kHz: 100 px per second, so labels every second.
        let view = ViewTransform {
            first: 0.0,
            frames_per_px: 10.0,
        };
        let ticks = ruler_ticks(&view, 1000, 450.0);
        let labels: Vec<_> = ticks.iter().map(|t| t.1.as_str()).collect();
        assert_eq!(labels, ["0s", "1s", "2s", "3s", "4s"]);
        assert_eq!(ticks[2].0, 200.0);

        // Zoomed in far enough for sub-second labels.
        let view = ViewTransform {
            first: 500.0,
            frames_per_px: 0.5,
        };
        let ticks = ruler_ticks(&view, 1000, 400.0);
        assert_eq!(ticks[0].1, "0.50s");
        assert!(ticks.iter().all(|t| t.0 >= 0.0 && t.0 <= 400.0));
    }

    #[test]
    fn snap_window_is_two_milliseconds() {
        assert_eq!(snap_window(44100), 88);
        assert_eq!(snap_window(48000), 96);
        assert_eq!(snap_window(100), 1);
    }
}
