//! The piano strip at the bottom of the window.

use std::collections::BTreeSet;

use eframe::egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, vec2};
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

fn is_black(note: u8) -> bool {
    matches!(note % 12, 1 | 3 | 6 | 8 | 10)
}

fn white_count() -> f32 {
    (white_index(LAST_NOTE) - white_index(FIRST_NOTE) + 1) as f32
}

/// The rectangle of `note` inside a strip occupying `strip`. Black keys overlap the white keys
/// beside them. Notes outside 36..=96 get an empty rectangle.
pub fn key_rect(strip: Rect, note: u8) -> Rect {
    if !(FIRST_NOTE..=LAST_NOTE).contains(&note) {
        return Rect::NOTHING;
    }
    let w = strip.width() / white_count();
    let x = (white_index(note) - white_index(FIRST_NOTE)) as f32 * w;
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
fn note_at(strip: Rect, pos: Pos2) -> Option<u8> {
    let notes = || FIRST_NOTE..=LAST_NOTE;
    notes()
        .filter(|n| is_black(*n))
        .chain(notes().filter(|n| !is_black(*n)))
        .find(|n| key_rect(strip, *n).contains(pos))
}

/// Draws the strip over the available area. The root key is `LOOP` coloured and held keys
/// `START` green. Returns a key press as `(note, true)` and its release as `(note, false)`;
/// a key is released when the button goes up or the pointer slides off it.
pub fn keyboard_strip(ui: &mut Ui, root: u8, held: &BTreeSet<u8>) -> Option<(u8, bool)> {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    let id = response.id.with("pressed_note");
    let mut active: Option<u8> = ui.data(|d| d.get_temp(id)).flatten();

    let under_pointer = response
        .is_pointer_button_down_on()
        .then(|| ui.input(|i| i.pointer.interact_pos()))
        .flatten()
        .and_then(|p| note_at(rect, p));
    let mut event = None;
    match (active, under_pointer) {
        (None, Some(n)) => {
            active = Some(n);
            event = Some((n, true));
        }
        (Some(n), under) if under != Some(n) => {
            active = None;
            event = Some((n, false));
        }
        _ => {}
    }
    ui.data_mut(|d| d.insert_temp(id, active));

    if ui.is_rect_visible(rect) {
        paint(ui, rect, root, held, active);
    }
    event
}

fn paint(ui: &Ui, rect: Rect, root: u8, held: &BTreeSet<u8>, pressed: Option<u8>) {
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
    for n in (FIRST_NOTE..=LAST_NOTE).filter(|n| !is_black(*n)) {
        let r = key_rect(rect, n);
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
    for n in (FIRST_NOTE..=LAST_NOTE).filter(|n| is_black(*n)) {
        painter.rect(key_rect(rect, n), 0.0, fill(n), outline, StrokeKind::Inside);
    }
    // A lit root key keeps its colour; show that it is also held with a green rim.
    if lit(root) {
        painter.rect_stroke(
            key_rect(rect, root),
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

    fn button(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn keyboard_strip_click_returns_note() {
        // State: the events returned so far and the strip's rectangle.
        let mut harness = Harness::builder()
            .with_size(vec2(900.0, 120.0))
            .build_ui_state(
                |ui, (events, strip): &mut (Vec<(u8, bool)>, Rect)| {
                    *strip = Rect::from_min_size(ui.cursor().min, ui.available_size());
                    events.extend(keyboard_strip(ui, 60, &BTreeSet::new()));
                },
                (Vec::new(), Rect::NOTHING),
            );
        harness.run();
        // The lower part of a white key is clear of the black keys.
        let r = key_rect(harness.state().1, 60);
        let pos = Pos2::new(r.center().x, r.bottom() - 4.0);
        harness.hover_at(pos);
        harness.run();
        harness.event(button(pos, true));
        harness.run();
        assert_eq!(harness.state().0, vec![(60, true)]);
        harness.event(button(pos, false));
        harness.run();
        assert_eq!(harness.state().0, vec![(60, true), (60, false)]);
    }

    #[test]
    fn black_keys_sit_over_white_keys() {
        let strip = Rect::from_min_size(Pos2::ZERO, vec2(720.0, 100.0));
        let black = key_rect(strip, 61);
        assert_eq!(note_at(strip, black.center()), Some(61));
        let white = key_rect(strip, 60);
        assert_eq!(
            note_at(strip, Pos2::new(white.left() + 1.0, 95.0)),
            Some(60)
        );
        assert_eq!(note_at(strip, Pos2::new(-5.0, 50.0)), None);
        assert_eq!(key_rect(strip, 20), Rect::NOTHING);
    }
}
