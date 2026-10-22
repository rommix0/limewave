//! MIDI note names. Middle C (MIDI 60) is `C4`.

const NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// Name of a MIDI note using sharps, e.g. 60 is `C4` and 0 is `C-1`.
pub fn note_name(n: u8) -> String {
    let octave = i32::from(n / 12) - 1;
    format!("{}{octave}", NAMES[usize::from(n % 12)])
}

/// Parses a note name (`C4`, `c#4`, `Db4`, `C-1`) or a plain number (`60`).
/// Returns `None` for anything outside MIDI notes 0..=127.
pub fn parse_note(s: &str) -> Option<u8> {
    let s = s.trim();
    let first = s.chars().next()?;
    if first.is_ascii_digit() {
        return s.parse::<u8>().ok().filter(|n| *n <= 127);
    }
    let class: i64 = match first.to_ascii_uppercase() {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let mut rest = &s[first.len_utf8()..];
    let accidental: i64 = if let Some(r) = rest.strip_prefix('#') {
        rest = r;
        1
    } else if let Some(r) = rest.strip_prefix('b') {
        rest = r;
        -1
    } else {
        0
    };
    let digits = rest.strip_prefix('-').unwrap_or(rest);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let octave = i64::from(rest.parse::<i32>().ok()?);
    let n = (octave + 1) * 12 + class + accidental;
    u8::try_from(n).ok().filter(|n| *n <= 127)
}

/// Frequency in Hz of a MIDI note in equal temperament (A4 = note 69 = 440 Hz).
pub fn note_hz(n: u8) -> f64 {
    440.0 * 2f64.powf((f64::from(n) - 69.0) / 12.0)
}

/// Whole number of frames in one cycle of a note at a sample rate, at least 1.
pub fn cycle_frames(n: u8, rate: u32) -> usize {
    (f64::from(rate) / note_hz(n)).round().max(1.0) as usize
}

/// Pitch error in cents of a `frames`-long cycle against the note's frequency.
pub fn cycle_cents(n: u8, rate: u32, frames: usize) -> f64 {
    1200.0 * (f64::from(rate) / (frames as f64 * note_hz(n))).log2()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_names() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(61), "C#4");
        assert_eq!(note_name(0), "C-1");
        assert_eq!(note_name(127), "G9");
        assert_eq!(parse_note("Db4"), Some(61));
    }

    #[test]
    fn parse_note_accepts_names_and_numbers() {
        assert_eq!(parse_note("C4"), Some(60));
        assert_eq!(parse_note("c#4"), Some(61));
        assert_eq!(parse_note("  a4 "), Some(69));
        assert_eq!(parse_note("Bb3"), Some(58));
        assert_eq!(parse_note("C-1"), Some(0));
        assert_eq!(parse_note("G9"), Some(127));
        assert_eq!(parse_note("60"), Some(60));
        assert_eq!(parse_note("0"), Some(0));
        assert_eq!(parse_note("127"), Some(127));
    }

    #[test]
    fn parse_note_rejects_out_of_range_and_garbage() {
        assert_eq!(parse_note("128"), None);
        assert_eq!(parse_note("-1"), None);
        assert_eq!(parse_note("G#9"), None);
        assert_eq!(parse_note("Cb-1"), None);
        assert_eq!(parse_note("H4"), None);
        assert_eq!(parse_note("C"), None);
        assert_eq!(parse_note("C#"), None);
        assert_eq!(parse_note("C+4"), None);
        assert_eq!(parse_note("C4x"), None);
        assert_eq!(parse_note("C99999999999"), None);
        assert_eq!(parse_note(""), None);
        assert_eq!(parse_note("é4"), None);
    }

    #[test]
    fn names_round_trip_for_every_note() {
        for n in 0..=127u8 {
            assert_eq!(parse_note(&note_name(n)), Some(n));
        }
    }

    #[test]
    fn note_frequencies_and_cycle_lengths() {
        assert!((note_hz(69) - 440.0).abs() < 1e-9);
        assert!((note_hz(60) - 261.625_565).abs() < 1e-5);
        assert_eq!(cycle_frames(69, 44_100), 100);
        assert_eq!(cycle_frames(60, 48_000), 183);
        assert_eq!(cycle_frames(127, 8_000), 1);
        let cents = cycle_cents(69, 44_100, 100);
        let want = 1200.0 * (44_100.0f64 / (100.0 * 440.0)).log2();
        assert!((cents - want).abs() < 0.1);
        assert!(cents > 0.0, "100 frames is slightly sharp of A4");
    }
}
