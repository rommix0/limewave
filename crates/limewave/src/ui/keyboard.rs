//! The piano strip at the bottom of the window.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use eframe::egui::{
    Align2, Color32, FontId, PointerButton, Pos2, Rect, Role, Sense, Stroke, StrokeKind, Ui,
    WidgetInfo, vec2,
};
use limewave_core::notes::note_name;

use crate::theme::colors;

/// The lowest and highest MIDI notes the strip draws.
pub const FIRST_NOTE: u8 = 36;
pub const LAST_NOTE: u8 = 96;

/// Black keys are this fraction of a white key wide and of the strip tall.
const BLACK_WIDTH: f32 = 0.6;
const BLACK_HEIGHT: f32 = 0.6;

/// Index of the white key at or below `note`, counted from MIDI note 0.
fn white_index(note: u8) -> u32 {
    const BELOW: [u32; 12] = [0, 0, 1, 1, 2, 3, 3, 4, 4, 5, 5, 6];
    u32::from(note / 12) * 7 + BELOW[usize::from(note % 12)]
}

pub(crate) fn is_black(note: u8) -> bool {
    matches!(note % 12, 1 | 3 | 6 | 8 | 10)
}

/// White keys in `keys`, which should start and end on white keys.
fn white_count(keys: &RangeInclusive<u8>) -> f32 {
    (white_index(*keys.end()) - white_index(*keys.start()) + 1) as f32
}

/// The rectangle of `note` inside a strip occupying `strip`. Black keys overlap the white keys
/// beside them. Notes outside 36..=96 get an empty rectangle.
pub fn key_rect(strip: Rect, note: u8) -> Rect {
    key_rect_in(strip, FIRST_NOTE..=LAST_NOTE, note)
}

/// The rectangle of `note` inside a strip of the notes `keys` occupying `strip`, as
/// [`key_rect`]; notes outside `keys` get an empty rectangle.
pub fn key_rect_in(strip: Rect, keys: RangeInclusive<u8>, note: u8) -> Rect {
    if !keys.contains(&note) {
        return Rect::NOTHING;
    }
    let w = strip.width() / white_count(&keys);
    let x = (white_index(note) - white_index(*keys.start())) as f32 * w;
    if is_black(note) {
        let cx = strip.left() + x + w;
        Rect::from_min_size(
            Pos2::new(cx - w * BLACK_WIDTH / 2.0, strip.top()),
            vec2(w * BLACK_WIDTH, strip.height() * BLACK_HEIGHT),
        )
    } else {
        Rect::from_min_size(
            Pos2::new(strip.left() + x, strip.top()),
            vec2(w, strip.height()),
        )
    }
}

/// The note under `pos`, black keys first because they sit on top.
fn note_at(strip: Rect, keys: &RangeInclusive<u8>, pos: Pos2) -> Option<u8> {
    let notes = || keys.clone();
    notes()
        .filter(|n| is_black(*n))
        .chain(notes().filter(|n| !is_black(*n)))
        .find(|n| key_rect_in(strip, keys.clone(), *n).contains(pos))
}

/// What the strip reports for one frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyEvent {
    /// The left button went down on a key.
    Press(u8),
    /// The left button went up, or the pointer slid off the key.
    Release(u8),
    /// A key was right-clicked: make it the root key.
    SetRoot(u8),
    /// The right button was dragged across two or more keys: these, lowest first.
    SetRange(u8, u8),
}

/// [`keyboard_strip_range`] over the notes 36..=96.
pub fn keyboard_strip(
    ui: &mut Ui,
    root: u8,
    held: &BTreeSet<u8>,
    can_play: bool,
) -> Option<KeyEvent> {
    keyboard_strip_range(ui, FIRST_NOTE..=LAST_NOTE, root, held, can_play)
}

/// Draws a strip of the notes `keys` over the available area. The root key is `LOOP`
/// coloured and held keys `START` green. A left press on a key is [`KeyEvent::Press`], and
/// the key is released when the button goes up or the pointer slides off it. A right click
/// gives [`KeyEvent::SetRoot`] and never plays; a right drag across two or more keys gives
/// [`KeyEvent::SetRange`] when the button goes up. Unless `can_play`, a left press does
/// nothing, so no key lights up silently.
pub fn keyboard_strip_range(
    ui: &mut Ui,
    keys: RangeInclusive<u8>,
    root: u8,
    held: &BTreeSet<u8>,
    can_play: bool,
) -> Option<KeyEvent> {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    response.widget_info(|| WidgetInfo::labeled(Role::Canvas, true, "Piano keyboard"));
    let id = response.id.with("pressed_note");
    let mut active: Option<u8> = ui.data(|d| d.get_temp(id)).flatten();

    // Only the left button plays: a right press must not start a note.
    let under_pointer = (can_play
        && response.is_pointer_button_down_on()
        && ui.input(|i| i.pointer.primary_down()))
    .then(|| ui.input(|i| i.pointer.interact_pos()))
    .flatten()
    .and_then(|p| note_at(rect, &keys, p));
    let mut event = None;
    match (active, under_pointer) {
        (None, Some(n)) => {
            active = Some(n);
            event = Some(KeyEvent::Press(n));
        }
        (Some(n), under) if under != Some(n) => {
            active = None;
            event = Some(KeyEvent::Release(n));
        }
        _ => {}
    }
    if event.is_none()
        && response.secondary_clicked()
        && let Some(n) = response
            .interact_pointer_pos()
            .and_then(|p| note_at(rect, &keys, p))
    {
        event = Some(KeyEvent::SetRoot(n));
    }
    ui.data_mut(|d| d.insert_temp(id, active));

    // The keys a right drag went from and has reached. The pointer is held on the strip, so
    // dragging past an end reaches the last key.
    let range_id = response.id.with("right_drag");
    let mut range: Option<(u8, u8)> = ui.data(|d| d.get_temp(range_id)).flatten();
    let on_strip = |p: Pos2| note_at(rect, &keys, rect.shrink(0.5).clamp(p));
    if response.drag_started_by(PointerButton::Secondary) {
        range = ui
            .input(|i| i.pointer.press_origin())
            .and_then(on_strip)
            .map(|n| (n, n));
    }
    if let Some((_, to)) = &mut range
        && let Some(n) = ui.input(|i| i.pointer.latest_pos()).and_then(on_strip)
    {
        *to = n;
    }
    if response.drag_stopped_by(PointerButton::Secondary) {
        if let Some((from, to)) = range.take()
            && from != to
            && event.is_none()
        {
            event = Some(KeyEvent::SetRange(from.min(to), from.max(to)));
        }
    } else if !response.dragged_by(PointerButton::Secondary) {
        range = None;
    }
    ui.data_mut(|d| d.insert_temp(range_id, range));

    if ui.is_rect_visible(rect) {
        paint(ui, rect, &keys, root, held, active);
        if let Some((from, to)) = range {
            let span = key_rect_in(rect, keys.clone(), from.min(to)).union(key_rect_in(
                rect,
                keys.clone(),
                from.max(to),
            ));
            ui.painter_at(rect).rect_filled(
                span.with_min_y(rect.top()).with_max_y(rect.bottom()),
                0.0,
                colors::NEON.gamma_multiply(0.35),
            );
        }
    }
    event
}

fn paint(
    ui: &Ui,
    rect: Rect,
    keys: &RangeInclusive<u8>,
    root: u8,
    held: &BTreeSet<u8>,
    pressed: Option<u8>,
) {
    let painter = ui.painter_at(rect);
    let lit = |n: u8| held.contains(&n) || pressed == Some(n);
    let fill = |n: u8| {
        if n == root {
            colors::LOOP
        } else if lit(n) {
            colors::START
        } else if is_black(n) {
            Color32::from_gray(30)
        } else {
            Color32::from_gray(225)
        }
    };
    let outline = Stroke::new(1.0, Color32::from_gray(70));
    let key = |n: u8| key_rect_in(rect, keys.clone(), n);
    for n in keys.clone().filter(|n| !is_black(*n)) {
        let r = key(n);
        painter.rect(r, 0.0, fill(n), outline, StrokeKind::Inside);
        if lit(n) && n != root {
            painter.rect_filled(
                r.with_min_y(r.bottom() - r.height() * 0.25),
                0.0,
                colors::START,
            );
        }
        if n % 12 == 0 {
            painter.text(
                r.center_bottom() - vec2(0.0, 2.0),
                Align2::CENTER_BOTTOM,
                note_name(n),
                FontId::proportional(10.0),
                Color32::from_gray(60),
            );
        }
    }
    for n in keys.clone().filter(|n| is_black(*n)) {
        painter.rect(key(n), 0.0, fill(n), outline, StrokeKind::Inside);
    }
    // A lit root key keeps its colour; show that it is also held with a green rim.
    if lit(root) {
        painter.rect_stroke(
            key(root),
            0.0,
            Stroke::new(2.0, colors::START),
            StrokeKind::Inside,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;

    use eframe::egui::{Event, Modifiers, PointerButton};

    fn button(pos: Pos2, which: PointerButton, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: which,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    /// A harness around the strip (root C4) collecting its events, and a point on the lower
    /// part of the C4 key, which is clear of the black keys.
    fn strip_harness(can_play: bool) -> (Harness<'static, (Vec<KeyEvent>, Rect)>, Pos2) {
        // State: the events returned so far and the strip's rectangle.
        let mut harness = Harness::builder()
            .with_size(vec2(900.0, 120.0))
            .build_ui_state(
                move |ui, (events, strip): &mut (Vec<KeyEvent>, Rect)| {
                    *strip = Rect::from_min_size(ui.cursor().min, ui.available_size());
                    events.extend(keyboard_strip(ui, 60, &BTreeSet::new(), can_play));
                },
                (Vec::new(), Rect::NOTHING),
            );
        harness.run();
        let r = key_rect(harness.state().1, 60);
        let pos = Pos2::new(r.center().x, r.bottom() - 4.0);
        harness.hover_at(pos);
        harness.run();
        (harness, pos)
    }

    #[test]
    fn keyboard_strip_click_returns_note() {
        let (mut harness, pos) = strip_harness(true);
        harness.event(button(pos, PointerButton::Primary, true));
        harness.run();
        assert_eq!(harness.state().0, vec![KeyEvent::Press(60)]);
        harness.event(button(pos, PointerButton::Primary, false));
        harness.run();
        assert_eq!(
            harness.state().0,
            vec![KeyEvent::Press(60), KeyEvent::Release(60)]
        );
    }

    #[test]
    fn right_click_sets_root_without_playing() {
        let (mut harness, pos) = strip_harness(true);
        harness.event(button(pos, PointerButton::Secondary, true));
        harness.run();
        harness.event(button(pos, PointerButton::Secondary, false));
        harness.run();
        assert_eq!(harness.state().0, vec![KeyEvent::SetRoot(60)]);
    }

    #[test]
    fn a_strip_that_cannot_play_ignores_left_presses_but_sets_the_root() {
        let (mut harness, pos) = strip_harness(false);
        harness.event(button(pos, PointerButton::Primary, true));
        harness.run();
        harness.event(button(pos, PointerButton::Primary, false));
        harness.run();
        assert_eq!(harness.state().0, vec![], "no key pressed or lit");
        harness.event(button(pos, PointerButton::Secondary, true));
        harness.run();
        harness.event(button(pos, PointerButton::Secondary, false));
        harness.run();
        assert_eq!(harness.state().0, vec![KeyEvent::SetRoot(60)]);
    }

    #[test]
    fn right_drag_gives_set_range() {
        let (mut harness, c4) = strip_harness(true);
        let e4 = {
            let r = key_rect(harness.state().1, 64);
            Pos2::new(r.center().x, r.bottom() - 4.0)
        };
        harness.event(button(c4, PointerButton::Secondary, true));
        harness.run();
        for i in 1..=4 {
            harness.hover_at(c4 + (e4 - c4) * (i as f32 / 4.0));
            harness.run();
        }
        harness.event(button(e4, PointerButton::Secondary, false));
        harness.run();
        assert_eq!(harness.state().0, vec![KeyEvent::SetRange(60, 64)]);

        // Leftwards gives the same range, low key first.
        harness.state_mut().0.clear();
        harness.hover_at(e4);
        harness.run();
        harness.event(button(e4, PointerButton::Secondary, true));
        harness.run();
        for i in 1..=4 {
            harness.hover_at(e4 + (c4 - e4) * (i as f32 / 4.0));
            harness.run();
        }
        harness.event(button(c4, PointerButton::Secondary, false));
        harness.run();
        assert_eq!(harness.state().0, vec![KeyEvent::SetRange(60, 64)]);
    }

    #[test]
    fn a_ranged_strip_places_keys_within_its_range() {
        let strip = Rect::from_min_size(Pos2::ZERO, vec2(750.0, 40.0));
        // 0..=127 has 75 white keys: 10 px each.
        assert_eq!(key_rect_in(strip, 0..=127, 0).left(), 0.0);
        assert_eq!(key_rect_in(strip, 0..=127, 127).right(), 750.0);
        assert_eq!(key_rect_in(strip, 0..=127, 60).left(), 350.0);
        assert_eq!(key_rect_in(strip, 0..=127, 128), Rect::NOTHING);
        assert_eq!(
            key_rect(strip, 60),
            key_rect_in(strip, FIRST_NOTE..=LAST_NOTE, 60)
        );
    }

    #[test]
    fn black_keys_sit_over_white_keys() {
        let strip = Rect::from_min_size(Pos2::ZERO, vec2(720.0, 100.0));
        let black = key_rect(strip, 61);
        let keys = FIRST_NOTE..=LAST_NOTE;
        assert_eq!(note_at(strip, &keys, black.center()), Some(61));
        let white = key_rect(strip, 60);
        assert_eq!(
            note_at(strip, &keys, Pos2::new(white.left() + 1.0, 95.0)),
            Some(60)
        );
        assert_eq!(note_at(strip, &keys, Pos2::new(-5.0, 50.0)), None);
        assert_eq!(key_rect(strip, 20), Rect::NOTHING);
    }
}
