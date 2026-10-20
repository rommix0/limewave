//! QWERTY keyboard to MIDI note mapping for auditioning.

use eframe::egui::Key;

/// Semitone offsets above `base` for the lower (Z) row, in piano-key order.
const LOWER_ROW: [(Key, u8); 13] = [
    (Key::Z, 0),
    (Key::S, 1),
    (Key::X, 2),
    (Key::D, 3),
    (Key::C, 4),
    (Key::V, 5),
    (Key::G, 6),
    (Key::B, 7),
    (Key::H, 8),
    (Key::N, 9),
    (Key::J, 10),
    (Key::M, 11),
    (Key::Comma, 12),
];

/// Semitone offsets above `base + 12` for the upper (Q) row.
const UPPER_ROW: [(Key, u8); 13] = [
    (Key::Q, 0),
    (Key::Num2, 1),
    (Key::W, 2),
    (Key::Num3, 3),
    (Key::E, 4),
    (Key::R, 5),
    (Key::Num5, 6),
    (Key::T, 7),
    (Key::Num6, 8),
    (Key::Y, 9),
    (Key::Num7, 10),
    (Key::U, 11),
    (Key::I, 12),
];

/// The MIDI note a key plays: the Z row starts at `base`, the Q row at `base + 12`.
/// Notes above 127 are not playable.
pub fn qwerty_note(key: Key, base: u8) -> Option<u8> {
    let offset = LOWER_ROW
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, o)| u16::from(*o))
        .or_else(|| {
            UPPER_ROW
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, o)| u16::from(*o) + 12)
        })?;
    u8::try_from(u16::from(base) + offset)
        .ok()
        .filter(|n| *n <= 127)
}

/// Octave change (in semitones) requested by a key: Minus is down, Equals is up.
pub fn octave_shift(key: Key) -> Option<i8> {
    match key {
        Key::Minus => Some(-12),
        Key::Equals => Some(12),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwerty_mapping() {
        assert_eq!(qwerty_note(Key::Z, 48), Some(48));
        assert_eq!(qwerty_note(Key::Q, 48), Some(60));
        assert_eq!(qwerty_note(Key::Comma, 48), Some(60));
        assert_eq!(qwerty_note(Key::A, 48), None);
        assert_eq!(qwerty_note(Key::I, 48), Some(72));
        assert_eq!(qwerty_note(Key::M, 48), Some(59));
        assert_eq!(qwerty_note(Key::Num2, 48), Some(61));
        assert_eq!(qwerty_note(Key::I, 120), None);
    }

    #[test]
    fn octave_keys() {
        assert_eq!(octave_shift(Key::Minus), Some(-12));
        assert_eq!(octave_shift(Key::Equals), Some(12));
        assert_eq!(octave_shift(Key::Z), None);
    }
}
