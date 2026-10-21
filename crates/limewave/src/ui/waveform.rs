//! The main waveform: time ruler, one lane per channel, markers, and the mouse handling for
//! dragging markers, zooming and scrolling.

use std::sync::Arc;

use eframe::egui::{
    self, Align2, Area, Color32, CursorIcon, FontId, Frame, Galley, Id, Key, Order, Painter,
    PointerButton, Pos2, Rect, Response, Role, Sense, Shape, Stroke, TextEdit, WidgetInfo, pos2,
    text::{CCursor, CCursorRange},
    text_edit::TextEditState,
    vec2,
};
use limewave_core::dsp::{
    PeakPyramid, loop_crossing, snap_loop_end, snap_loop_end_with, snap_loop_start,
    snap_loop_start_with,
};
use limewave_core::edit::{EditState, LoopMode, Marker, move_marker};

use super::coords::{ViewTransform, flag_rect, hit_marker, pick_marker};
use crate::app::{Session, UiState};
use crate::audio::engine::{AudioEngine, Cursor};
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

/// What a drag on the waveform is doing, decided by where it was pressed.
#[derive(Clone, Copy)]
enum Gesture {
    Marker(Drag),
    /// Dragging out a selection from the (fractional) frame under the press.
    Select {
        anchor: f64,
    },
}

fn with_alpha(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

/// The snap search window: 2 ms of frames, at least one.
fn snap_window(rate: u32) -> usize {
    ((rate as f32 * 0.002).round() as usize).max(1)
}

/// A marker's flag: the coloured label at the top of its line, which is also a handle.
struct Flag {
    marker: Marker,
    color: Color32,
    galley: Arc<Galley>,
    rect: Rect,
}

/// Height of a flag row in pixels.
const FLAG_ROW: f32 = 15.0;

/// End-side flags hang to the left of their line, start-side flags to the right.
fn flag_hangs_left(m: Marker) -> bool {
    matches!(m, Marker::End | Marker::LoopEnd)
}

/// Where a marker's flag is anchored: on its line, in a row below the ruler (Start and End on
/// the first, the loop markers on the second).
fn flag_anchor(rect: Rect, view: &ViewTransform, e: &EditState, m: Marker) -> Pos2 {
    let row = match m {
        Marker::Start | Marker::End => 0.0,
        Marker::LoopStart | Marker::LoopEnd => 1.0,
    };
    pos2(
        view.frame_to_x(marker_frame(e, m) as f64, rect.left()),
        rect.top() + RULER_HEIGHT + 2.0 + row * FLAG_ROW,
    )
}

/// A point inside marker `m`'s flag, for tests: a flag has at least 8 px of padding around its
/// text, so 10 px from the line on the side the flag extends to, in the middle of its row, is
/// on it.
pub fn flag_hit_point(rect: Rect, view: &ViewTransform, e: &EditState, m: Marker) -> Pos2 {
    let anchor = flag_anchor(rect, view, e, m);
    let dx = if flag_hangs_left(m) { -10.0 } else { 10.0 };
    anchor + vec2(dx, 6.0)
}

/// The flags of the markers that are shown and within the view, in drawing order.
fn marker_flags(painter: &Painter, rect: Rect, view: &ViewTransform, e: &EditState) -> Vec<Flag> {
    let mut markers = vec![
        (Marker::Start, colors::START, "START"),
        (Marker::End, colors::END, "END"),
    ];
    if e.loop_mode != LoopMode::Off {
        markers.push((Marker::LoopStart, colors::LOOP, "LOOP"));
        markers.push((Marker::LoopEnd, colors::LOOP, "LOOP"));
    }
    markers
        .into_iter()
        .filter_map(|(marker, color, label)| {
            let anchor = flag_anchor(rect, view, e, marker);
            if anchor.x < rect.left() - 1.0 || anchor.x > rect.right() + 1.0 {
                return None;
            }
            let galley =
                painter.layout_no_wrap(label.to_owned(), FontId::proportional(10.0), colors::PANEL);
            let size = galley.size() + vec2(8.0, 2.0);
            Some(Flag {
                marker,
                color,
                galley,
                rect: flag_rect(anchor, flag_hangs_left(marker), size),
            })
        })
        .collect()
}

fn flag_rects(flags: &[Flag]) -> Vec<(Marker, Rect)> {
    flags.iter().map(|f| (f.marker, f.rect)).collect()
}

/// The marker under the pointer at `pos`: a flag, else a line within the grab tolerance.
fn marker_at(
    e: &EditState,
    view: &ViewTransform,
    left: f32,
    flags: &[(Marker, Rect)],
    pos: Pos2,
) -> Option<Marker> {
    let line = hit_marker(e, view, left, pos.x, GRAB_TOLERANCE);
    pick_marker(flags, line, pos)
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
    cursors: &[Cursor],
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

    // Markers: grab a flag or line on press, follow the pointer, one history entry on release.
    let flags = flag_rects(&marker_flags(&painter, rect, &view, s.shown_edit()));
    if response.double_clicked_by(PointerButton::Primary)
        && let Some(pos) = response.interact_pointer_pos()
        && let Some(marker) = pick_marker(&flags, None, pos)
    {
        let text = marker_frame(s.shown_edit(), marker).to_string();
        ui_state.marker_entry = Some((marker, text));
        // The field takes the focus and selects its text in the frame it first appears.
        ui.data_mut(|d| d.insert_temp(entry_id(), true));
    }
    let drag_id = response.id.with("drag");
    if response.drag_started_by(PointerButton::Primary)
        && let Some(press) = ui.input(|i| i.pointer.press_origin())
    {
        ui_state.message = None;
        let e = s.shown_edit();
        let gesture = match marker_at(e, &view, left, &flags, press) {
            Some(marker) => Gesture::Marker(Drag {
                marker,
                grab_offset: marker_frame(e, marker) as f64 - view.x_to_frame(press.x, left),
            }),
            None => Gesture::Select {
                anchor: view.x_to_frame(press.x, left),
            },
        };
        ui.data_mut(|d| d.insert_temp(drag_id, Some(gesture)));
    }
    let gesture: Option<Gesture> = ui
        .data(|d| d.get_temp::<Option<Gesture>>(drag_id))
        .flatten();
    if let Some(gesture) = gesture {
        let pointer = response
            .dragged_by(PointerButton::Primary)
            .then(|| response.interact_pointer_pos())
            .flatten();
        let shift = ui.input(|i| i.modifiers.shift);
        match (gesture, pointer) {
            (Gesture::Marker(drag), Some(pos)) => {
                let target = view.x_to_frame(pos.x, left) + drag.grab_offset;
                let moved = dragged_edit(s, ui_state, drag.marker, target, ui_state.snap && !shift);
                s.set_preview(moved, engine);
            }
            (Gesture::Select { anchor }, Some(pos)) => {
                let to = view.x_to_frame(pos.x, left);
                ui_state.selection =
                    Some(dragged_selection(s, anchor, to, ui_state.snap && !shift));
            }
            _ => {}
        }
        if response.drag_stopped() {
            ui.data_mut(|d| d.insert_temp(drag_id, None::<Gesture>));
            if matches!(gesture, Gesture::Marker(_)) {
                s.commit_preview(engine);
            }
        }
    }

    // A click on empty space sets the play-from point; Shift extends the selection instead.
    if response.clicked_by(PointerButton::Primary)
        && let Some(pos) = response.interact_pointer_pos()
        && marker_at(s.shown_edit(), &view, left, &flags, pos).is_none()
    {
        ui_state.message = None;
        let frame = frame_at(&view, pos.x, left, total);
        let shift = ui.input(|i| i.modifiers.shift);
        match (shift, ui_state.selection, ui_state.play_from) {
            (true, Some((a, b)), _) => ui_state.selection = Some((a.min(frame), b.max(frame))),
            (true, None, Some(from)) => {
                ui_state.selection = Some((from.min(frame), from.max(frame)));
            }
            _ => {
                ui_state.play_from = Some(frame);
                ui_state.selection = None;
            }
        }
    }

    s.view = Some(view);

    // The flags again: a drag may have moved their markers.
    let flags = marker_flags(&painter, rect, &view, s.shown_edit());
    let hovered = match gesture {
        Some(Gesture::Marker(drag)) => Some(drag.marker),
        Some(Gesture::Select { .. }) => None,
        None => response
            .hover_pos()
            .and_then(|p| marker_at(s.shown_edit(), &view, left, &flag_rects(&flags), p)),
    };
    if hovered.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    }

    let ruler_color = ui.visuals().weak_text_color();
    draw(&painter, rect, s, &view, hovered, ruler_color, &flags);
    draw_selection(&painter, rect, &view, ui_state);
    for c in cursors {
        let x = view.frame_to_x(c.frame, left);
        if (left..=rect.right()).contains(&x) {
            painter.vline(x, rect.y_range(), Stroke::new(1.0, cursor_color(c)));
        }
    }

    marker_entry_field(ui, s, ui_state, engine, rect, &flags);
}

fn entry_id() -> Id {
    Id::NULL.with("marker_entry")
}

/// The accessible name of the position field of `m`.
fn entry_label(m: Marker) -> &'static str {
    match m {
        Marker::Start => "Start position",
        Marker::LoopStart => "Loop start position",
        Marker::LoopEnd => "Loop end position",
        Marker::End => "End position",
    }
}

/// The small floating field for typing a marker's exact frame. Enter applies a valid frame
/// number as one history entry (unsnapped); Escape, invalid text or leaving the field closes
/// it unchanged.
fn marker_entry_field(
    ui: &mut egui::Ui,
    s: &mut Session,
    ui_state: &mut UiState,
    engine: Option<&AudioEngine>,
    rect: Rect,
    flags: &[Flag],
) {
    let Some((marker, mut text)) = ui_state.marker_entry.take() else {
        return;
    };
    let loop_marker = matches!(marker, Marker::LoopStart | Marker::LoopEnd);
    if loop_marker && s.shown_edit().loop_mode == LoopMode::Off {
        return;
    }
    // Under its flag, or at the top-left when the marker is scrolled out of view.
    let pos = flags
        .iter()
        .find(|f| f.marker == marker)
        .map_or(rect.min + vec2(8.0, RULER_HEIGHT + 4.0), |f| {
            pos2(f.rect.left(), f.rect.bottom() + 2.0)
        });
    let id = entry_id();
    // A new `Area` skips its first frame, which drops a focus request made in it, so keep
    // asking (and selecting the text) until the focus has survived a frame.
    let pending = ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    let just_opened = pending && !ui.memory(|m| m.has_focus(id));
    if pending && !just_opened {
        ui.data_mut(|d| d.remove_temp::<bool>(id));
    }
    let mut response = None;
    Area::new(id.with("area"))
        .order(Order::Foreground)
        .fixed_pos(pos)
        .show(ui.ctx(), |ui| {
            Frame::popup(ui.style()).show(ui, |ui| {
                if just_opened {
                    ui.memory_mut(|m| m.request_focus(id));
                    let all =
                        CCursorRange::two(CCursor::new(0), CCursor::new(text.chars().count()));
                    let mut state = TextEditState::load(ui.ctx(), id).unwrap_or_default();
                    state.cursor.set_char_range(Some(all));
                    state.store(ui.ctx(), id);
                }
                let out = TextEdit::singleline(&mut text)
                    .id(id)
                    .desired_width(72.0)
                    .show(ui);
                response =
                    Some(Response::clone(&out.response).accessible_name(entry_label(marker)));
            });
        });
    let Some(response) = response else {
        return;
    };
    let (enter, escape) = ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
    if escape || (response.lost_focus() && !just_opened) {
        if enter
            && !escape
            && let Ok(frame) = text.trim().parse::<usize>()
        {
            let e = move_marker(s.edit(), marker, frame, ui_state.lock_length, s.doc.frames);
            if e != *s.edit() {
                s.apply(e, engine);
            }
        }
        return;
    }
    ui_state.marker_entry = Some((marker, text));
}

/// The colour of a playback cursor: neon, as opaque as the voice's envelope level.
pub fn cursor_color(c: &Cursor) -> Color32 {
    with_alpha(
        colors::NEON,
        (c.level.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

/// The edit after dragging `marker` to the (fractional) frame `target`, snapped onto a zero
/// crossing when `snap` is set and the marker is a loop marker: upward, or downward for a
/// backward loop.
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
        let crossing = loop_crossing(s.edit().loop_mode);
        match marker {
            Marker::LoopStart => to = snap_loop_start_with(&s.mono, to, window, crossing),
            Marker::LoopEnd => to = snap_loop_end_with(&s.mono, to, window, crossing),
            Marker::Start | Marker::End => {}
        }
    }
    move_marker(s.edit(), marker, to, ui_state.lock_length, s.doc.frames)
}

/// The frame under screen `x`, rounded and clamped to the file.
fn frame_at(view: &ViewTransform, x: f32, left: f32, total: usize) -> usize {
    view.x_to_frame(x, left)
        .round()
        .clamp(0.0, total.saturating_sub(1) as f64) as usize
}

/// The inclusive selection dragged out between the (fractional) frames `a` and `b`: ordered
/// and clamped to the file, and with `snap` its first edge snapped like a loop start and its
/// last like a loop end.
fn dragged_selection(s: &Session, a: f64, b: f64, snap: bool) -> (usize, usize) {
    let last_frame = s.doc.frames.saturating_sub(1) as f64;
    let at = |f: f64| f.round().clamp(0.0, last_frame) as usize;
    let (mut first, mut last) = (at(a.min(b)), at(a.max(b)));
    if snap {
        let window = snap_window(s.doc.format.sample_rate);
        first = snap_loop_start(&s.mono, first, window);
        last = snap_loop_end(&s.mono, last, window).max(first);
    }
    (first, last)
}

/// The selection as a light overlay across every lane, and the play-from point as a thin grey
/// line with a small triangle at the top.
fn draw_selection(painter: &egui::Painter, rect: Rect, view: &ViewTransform, ui_state: &UiState) {
    let left = rect.left();
    if let Some((first, last)) = ui_state.selection {
        let x = |f: usize| view.frame_to_x(f as f64, left).clamp(left, rect.right());
        let lanes = Rect::from_x_y_ranges(
            x(first)..=x(last + 1),
            (rect.top() + RULER_HEIGHT)..=rect.bottom(),
        );
        painter.rect_filled(lanes, 0.0, with_alpha(Color32::WHITE, 30));
    }
    if let Some(from) = ui_state.play_from {
        let x = view.frame_to_x(from as f64, left);
        if (left..=rect.right()).contains(&x) {
            let grey = Color32::from_gray(190);
            painter.vline(x, rect.y_range(), Stroke::new(1.0, grey));
            painter.add(Shape::convex_polygon(
                vec![
                    pos2(x - 5.0, rect.top()),
                    pos2(x + 5.0, rect.top()),
                    pos2(x, rect.top() + 7.0),
                ],
                grey,
                Stroke::NONE,
            ));
        }
    }
}

fn draw(
    painter: &egui::Painter,
    rect: Rect,
    s: &Session,
    view: &ViewTransform,
    hovered: Option<Marker>,
    ruler_color: Color32,
    flags: &[Flag],
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
    let channels = s.processed.len().max(1);
    let lane_height = lanes.height() / channels as f32;
    for (i, (channel, peaks)) in s.processed.iter().zip(&s.peaks).enumerate() {
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
    for flag in flags {
        let x = x_of(marker_frame(e, flag.marker));
        let width = if hovered == Some(flag.marker) {
            2.5
        } else {
            1.5
        };
        painter.vline(x, rect.y_range(), Stroke::new(width, flag.color));
        painter.rect_filled(flag.rect, 2.0, flag.color);
        painter.galley(
            flag.rect.min + vec2(4.0, 1.0),
            flag.galley.clone(),
            colors::PANEL,
        );
    }
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
