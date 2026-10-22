//! The key map (spec 5.2): each region of the open instrument as a box over the keys, and
//! velocities, it plays, above a keyboard of all 128 keys.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use eframe::egui::{
    Color32, CursorIcon, DragValue, PointerButton, Pos2, Rangef, Rect, Role, Sense, Stroke,
    StrokeKind, Ui, WidgetInfo, vec2,
};

use crate::app::FileEntry;
use crate::instrument::InstrumentState;
use crate::theme::colors;
use crate::ui::keyboard::{KeyEvent, is_black, key_rect_in, keyboard_strip_range};

/// Every MIDI key: the map covers C-1 to G9.
const ALL_KEYS: RangeInclusive<u8> = 0..=127;
/// How close to a box's edge the pointer grabs it, in points.
const GRAB: f32 = 4.0;
/// The height of the keyboard under the boxes.
const KEYBOARD_HEIGHT: f32 = 36.0;
/// Box tints, one per group in turn; regions before the first group take the first.
const GROUP_TINTS: [Color32; 6] = [
    colors::NEON,
    colors::LIME,
    colors::XFADE,
    colors::LOOP,
    colors::WAVE,
    colors::END,
];

/// A side of a region's box.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// What the key map asks of the app for one frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyMapAction {
    /// A box was clicked: select its region.
    Select(usize),
    /// An edge drag moved a region's key range to these keys (a preview, not yet an edit).
    SetKeys(usize, u8, u8),
    /// An edge drag moved a region's velocity range (a preview, not yet an edit).
    SetVel(usize, u8, u8),
    /// The edge drag ended: the previewed ranges become one undo step.
    Commit,
    /// Something happened on the keyboard.
    Key(KeyEvent),
    AutoMap,
    FillGaps,
}

/// The horizontal span of `note` on the map. Each key owns the part of the keyboard no other
/// key covers, so the spans of neighbouring keys meet without overlapping.
fn column(area: Rect, note: u8) -> Rangef {
    let key = |n: u8| key_rect_in(area, ALL_KEYS, n);
    let r = key(note);
    if is_black(note) {
        return r.x_range();
    }
    let left = if note > 0 && is_black(note - 1) {
        key(note - 1).right()
    } else {
        r.left()
    };
    let right = if note < 127 && is_black(note + 1) {
        key(note + 1).left()
    } else {
        r.right()
    };
    Rangef::new(left, right)
}

/// The key whose column holds `x`; past either end, the end key.
fn note_at_x(area: Rect, x: f32) -> u8 {
    if x < area.left() {
        return 0;
    }
    (0..=127u8)
        .find(|&n| x < column(area, n).max)
        .unwrap_or(127)
}

/// The velocity boundary at height `y`: 0 at the bottom of the map, 127 at the top.
fn velocity_at_y(area: Rect, y: f32) -> f32 {
    (area.bottom() - y) / area.height().max(1.0) * 127.0
}

/// A region's box in the map `area`: across the columns of `lokey..=hikey`, and either up the
/// velocities `lovel..=hivel` (velocity 127 at the top) or, with `keys_only`, in row `lane`
/// of `lanes` from the top.
#[allow(clippy::too_many_arguments)]
pub fn box_rect(
    area: Rect,
    keys_only: bool,
    lane: usize,
    lanes: usize,
    lokey: u8,
    hikey: u8,
    lovel: u8,
    hivel: u8,
) -> Rect {
    let x = Rangef::new(column(area, lokey).min, column(area, hikey).max);
    let y = if keys_only {
        let h = area.height() / lanes.max(1) as f32;
        let top = area.top() + lane as f32 * h;
        Rangef::new(top, top + h)
    } else {
        let at = |v: f32| area.bottom() - v / 127.0 * area.height();
        Rangef::new(at(f32::from(hivel)), at(f32::from(lovel) - 1.0))
    };
    Rect::from_x_y_ranges(x, y)
}

/// The box edge under `pos`, within [`GRAB`] of it: `boxes` are (region, rectangle) in drawing
/// order, and a later box wins as it is drawn on top. Top and bottom edges count only when
/// the map shows velocities.
pub fn edge_at(boxes: &[(usize, Rect)], pos: Pos2, keys_only: bool) -> Option<(usize, Edge)> {
    let near = |a: f32, b: f32| (a - b).abs() <= GRAB;
    boxes.iter().rev().find_map(|&(region, r)| {
        let edge = if r.y_range().contains(pos.y) && near(pos.x, r.left()) {
            Edge::Left
        } else if r.y_range().contains(pos.y) && near(pos.x, r.right()) {
            Edge::Right
        } else if !keys_only && r.x_range().contains(pos.x) && near(pos.y, r.top()) {
            Edge::Top
        } else if !keys_only && r.x_range().contains(pos.x) && near(pos.y, r.bottom()) {
            Edge::Bottom
        } else {
            return None;
        };
        Some((region, edge))
    })
}

/// Rows for key ranges drawn without velocity: each range takes the first row whose ranges
/// all end below it. Returns each range's row and the row count (at least 1).
fn lanes(ranges: &[(u8, u8)]) -> (Vec<usize>, usize) {
    let mut order: Vec<usize> = (0..ranges.len()).collect();
    order.sort_by_key(|&i| (ranges[i].0, i));
    let mut ends: Vec<u8> = Vec::new();
    let mut lane = vec![0; ranges.len()];
    for i in order {
        let (lo, hi) = ranges[i];
        lane[i] = match ends.iter().position(|&end| end < lo) {
            Some(l) => {
                ends[l] = hi;
                l
            }
            None => {
                ends.push(hi);
                ends.len() - 1
            }
        };
    }
    (lane, ends.len().max(1))
}

/// Draws the key map panel: the Velocity, view and mapping controls, the region boxes, and a
/// keyboard of every key along the bottom that plays like the plain strip (spec 5.2).
/// `sounding` holds the regions playing now, `held` the keys held down; the keyboard plays
/// only when `can_play`.
#[allow(clippy::too_many_arguments)]
pub fn key_map(
    ui: &mut Ui,
    st: &InstrumentState,
    files: &[FileEntry],
    sounding: &BTreeSet<usize>,
    held: &BTreeSet<u8>,
    can_play: bool,
    velocity: &mut u8,
    keys_only: &mut bool,
) -> Vec<KeyMapAction> {
    let mut actions = Vec::new();
    ui.horizontal(|ui| {
        let label = ui.label("Velocity");
        ui.add(DragValue::new(velocity).range(1..=127))
            .labelled_by(label.id);
        ui.separator();
        ui.selectable_value(keys_only, false, "Keys \u{d7} velocity");
        ui.selectable_value(keys_only, true, "Keys only");
        ui.separator();
        if ui.button("Auto-map from root keys").clicked() {
            actions.push(KeyMapAction::AutoMap);
        }
        if ui.button("Fill gaps").clicked() {
            actions.push(KeyMapAction::FillGaps);
        }
    });

    let inst = st.shown();
    let infos: Vec<_> = (0..inst.regions().len())
        .map(|r| inst.region_info(r))
        .collect();
    let ranges: Vec<(u8, u8)> = infos.iter().map(|i| (i.lokey, i.hikey)).collect();
    let (lane, lane_count) = lanes(&ranges);

    ui.spacing_mut().item_spacing.y = 0.0;
    let map_height = (ui.available_height() - KEYBOARD_HEIGHT).max(16.0);
    let (area, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), map_height),
        Sense::click_and_drag(),
    );
    response.widget_info(|| WidgetInfo::labeled(Role::Canvas, true, "Key map"));

    // Boxes in drawing order: the selected region last, on top.
    let mut boxes: Vec<(usize, Rect)> = infos
        .iter()
        .enumerate()
        .map(|(r, i)| {
            let rect = box_rect(
                area, *keys_only, lane[r], lane_count, i.lokey, i.hikey, i.lovel, i.hivel,
            );
            (r, rect)
        })
        .collect();
    boxes.sort_by_key(|&(r, _)| Some(r) == st.selected_region);

    actions.extend(interact(
        ui, &response, area, st, &infos, &boxes, *keys_only,
    ));

    if ui.is_rect_visible(area) {
        paint(ui, area, st, &ranges, &boxes, sounding);
    }

    let root = st
        .selected_region
        .and_then(|r| st.region_file(r))
        .and_then(|i| match files.get(i) {
            Some(FileEntry::Loaded(s)) => Some(s.shown_edit().root),
            _ => None,
        })
        .unwrap_or(60);
    ui.scope(|ui| {
        // Not disabled when silent: right-clicking a key still sets the root.
        if !can_play {
            ui.multiply_opacity(0.5);
        }
        if let Some(e) = keyboard_strip_range(ui, ALL_KEYS, root, held, can_play) {
            actions.push(KeyMapAction::Key(e));
        }
    });
    actions
}

/// Clicks select (again at the same spot: the next box under the pointer), and dragging a
/// box's edge previews a new range until the button goes up.
fn interact(
    ui: &Ui,
    response: &eframe::egui::Response,
    area: Rect,
    st: &InstrumentState,
    infos: &[limewave_core::sfz::RegionInfo],
    boxes: &[(usize, Rect)],
    keys_only: bool,
) -> Vec<KeyMapAction> {
    let mut actions = Vec::new();
    let drag_id = response.id.with("edge_drag");
    let mut dragging: Option<(usize, Edge)> = ui.data(|d| d.get_temp(drag_id)).flatten();
    if response.drag_started_by(PointerButton::Primary) {
        dragging = ui
            .input(|i| i.pointer.press_origin())
            .and_then(|p| edge_at(boxes, p, keys_only));
    }
    if let Some((region, edge)) = dragging
        && let Some(info) = infos.get(region)
        && let Some(pos) = ui.input(|i| i.pointer.latest_pos())
    {
        let (lo, hi, lv, hv) = (info.lokey, info.hikey, info.lovel, info.hivel);
        let vel = velocity_at_y(area, pos.y).round().clamp(0.0, 127.0) as u8;
        let action = match edge {
            Edge::Left => KeyMapAction::SetKeys(region, note_at_x(area, pos.x).min(hi), hi),
            Edge::Right => KeyMapAction::SetKeys(region, lo, note_at_x(area, pos.x).max(lo)),
            Edge::Top => KeyMapAction::SetVel(region, lv, vel.clamp(lv, 127)),
            Edge::Bottom => KeyMapAction::SetVel(region, (vel + 1).min(127).clamp(1, hv), hv),
        };
        let unchanged = matches!(action, KeyMapAction::SetKeys(_, a, b) if (a, b) == (lo, hi))
            || matches!(action, KeyMapAction::SetVel(_, a, b) if (a, b) == (lv, hv));
        if !unchanged {
            actions.push(action);
        }
    }
    if response.drag_stopped_by(PointerButton::Primary) {
        if dragging.take().is_some() {
            actions.push(KeyMapAction::Commit);
        }
    } else if !response.dragged_by(PointerButton::Primary) {
        dragging = None;
    }
    ui.data_mut(|d| d.insert_temp(drag_id, dragging));

    let hover_edge = dragging.or_else(|| {
        response
            .hover_pos()
            .and_then(|p| edge_at(boxes, p, keys_only))
    });
    if let Some((_, edge)) = hover_edge {
        ui.ctx().set_cursor_icon(match edge {
            Edge::Left | Edge::Right => CursorIcon::ResizeHorizontal,
            Edge::Top | Edge::Bottom => CursorIcon::ResizeVertical,
        });
    }

    let click_id = response.id.with("last_click");
    if response.clicked()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let mut under: Vec<usize> = boxes
            .iter()
            .filter(|(_, r)| r.contains(pos))
            .map(|&(region, _)| region)
            .collect();
        under.sort_unstable();
        let last: Option<Pos2> = ui.data(|d| d.get_temp(click_id));
        let same_spot = last.is_some_and(|p| p.distance(pos) <= GRAB);
        let at = st
            .selected_region
            .and_then(|s| under.iter().position(|&r| r == s));
        let pick = match at {
            Some(i) if same_spot => Some(under[(i + 1) % under.len()]),
            Some(_) => None,
            None => under.first().copied(),
        };
        if let Some(region) = pick {
            actions.push(KeyMapAction::Select(region));
        }
        ui.data_mut(|d| d.insert_temp(click_id, pos));
    }
    actions
}

fn paint(
    ui: &Ui,
    area: Rect,
    st: &InstrumentState,
    ranges: &[(u8, u8)],
    boxes: &[(usize, Rect)],
    sounding: &BTreeSet<usize>,
) {
    let painter = ui.painter_at(area);
    painter.rect_filled(area, 0.0, Color32::from_gray(22));

    // Keys no region covers, at any velocity, are hatched.
    let mut covered = [false; 128];
    for &(lo, hi) in ranges {
        for k in lo.min(127)..=hi.min(127) {
            covered[usize::from(k)] = true;
        }
    }
    let hatch = Stroke::new(1.0, Color32::from_gray(60));
    let mut k = 0usize;
    while k < 128 {
        if covered[k] {
            k += 1;
            continue;
        }
        let first = k;
        while k < 128 && !covered[k] {
            k += 1;
        }
        let x = Rangef::new(
            column(area, first as u8).min,
            column(area, (k - 1) as u8).max,
        );
        let run = Rect::from_x_y_ranges(x, area.y_range());
        let p = painter.with_clip_rect(run);
        let mut start = run.left() - run.height();
        while start < run.right() {
            p.line_segment(
                [
                    Pos2::new(start, run.bottom()),
                    Pos2::new(start + run.height(), run.top()),
                ],
                hatch,
            );
            start += 6.0;
        }
    }

    let inst = st.shown();
    for &(region, rect) in boxes {
        let group = inst.group_of(region).map_or(0, |g| g + 1);
        let tint = GROUP_TINTS[group % GROUP_TINTS.len()];
        let missing = st
            .binding
            .region_sample
            .get(region)
            .is_none_or(Option::is_none);
        let alpha = if sounding.contains(&region) {
            0.8
        } else if missing {
            0.08
        } else {
            0.25
        };
        painter.rect(
            rect,
            2.0,
            tint.gamma_multiply(alpha),
            Stroke::new(1.0, tint.gamma_multiply(0.8)),
            StrokeKind::Inside,
        );
        if Some(region) == st.selected_region {
            painter.rect_stroke(
                rect,
                2.0,
                Stroke::new(2.0, Color32::WHITE),
                StrokeKind::Inside,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 75 white keys of 10 points each, 127 points tall: one point per velocity.
    fn area() -> Rect {
        Rect::from_min_size(Pos2::new(0.0, 10.0), vec2(750.0, 127.0))
    }

    #[test]
    fn box_rect_spans_keys_and_velocity() {
        let a = area();
        // C4 is white key 35 (x 350); E4 ends where F4 begins, at white key 38.
        let full = box_rect(a, false, 0, 1, 60, 64, 1, 127);
        assert_eq!(full.left(), 350.0);
        assert_eq!(full.right(), 380.0);
        assert_eq!((full.top(), full.bottom()), (a.top(), a.bottom()));
        let soft = box_rect(a, false, 0, 1, 60, 64, 1, 64);
        assert_eq!(soft.top(), a.bottom() - 64.0);
        let loud = box_rect(a, false, 0, 1, 60, 64, 65, 127);
        assert_eq!(loud.bottom(), soft.top(), "neighbouring velocities meet");
        // Neighbouring keys meet too: C4's column ends where C#4's begins.
        let c = box_rect(a, false, 0, 1, 60, 60, 1, 127);
        let c_sharp = box_rect(a, false, 0, 1, 61, 61, 1, 127);
        assert_eq!(c.right(), c_sharp.left());
        assert_eq!(
            c_sharp.right(),
            box_rect(a, false, 0, 1, 62, 62, 1, 127).left()
        );
        // Keys only: the second of two rows is the lower half, whatever the velocities.
        let row = box_rect(a, true, 1, 2, 60, 64, 1, 10);
        assert_eq!((row.top(), row.bottom()), (a.center().y, a.bottom()));
        assert_eq!(box_rect(a, false, 0, 1, 0, 127, 1, 127), a);
    }

    #[test]
    fn edge_at_finds_right_edge() {
        let a = area();
        let first = box_rect(a, false, 0, 1, 40, 50, 1, 127);
        let second = box_rect(a, false, 0, 1, 51, 70, 1, 64);
        let boxes = [(0, first), (1, second)];
        let y = a.bottom() - 100.0;
        assert_eq!(
            edge_at(&boxes, Pos2::new(first.right() - 2.0, y), false),
            Some((0, Edge::Right))
        );
        assert_eq!(
            edge_at(&boxes, Pos2::new(first.right() - 10.0, y), false),
            None
        );
        // Where two edges meet, the box drawn last wins.
        let low = a.bottom() - 10.0;
        assert_eq!(
            edge_at(&boxes, Pos2::new(first.right(), low), false),
            Some((1, Edge::Left))
        );
        assert_eq!(
            edge_at(
                &boxes,
                Pos2::new(second.center().x, second.top() + 1.0),
                false
            ),
            Some((1, Edge::Top))
        );
        assert_eq!(
            edge_at(
                &boxes,
                Pos2::new(second.center().x, second.top() + 1.0),
                true
            ),
            None,
            "no velocity edges in Keys only"
        );
    }

    #[test]
    fn note_at_x_follows_the_columns() {
        let a = area();
        for n in [0, 1, 59, 60, 61, 64, 65, 127] {
            assert_eq!(note_at_x(a, column(a, n).center()), n);
        }
        assert_eq!(note_at_x(a, -5.0), 0);
        assert_eq!(note_at_x(a, 900.0), 127);
    }

    #[test]
    fn overlapping_ranges_take_separate_lanes() {
        let (lane, count) = lanes(&[(0, 60), (50, 70), (61, 127), (71, 80)]);
        assert_eq!(lane, [0, 1, 0, 1]);
        assert_eq!(count, 2);
        assert_eq!(lanes(&[]), (vec![], 1));
    }
}
