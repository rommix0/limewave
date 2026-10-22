//! The New… dialog: a blank, silent sample to draw on.

use eframe::egui::{self, Button, ComboBox, DragValue, TextEdit};
use limewave_core::MAX_NEW_SECONDS;
use limewave_core::notes::{cycle_cents, cycle_frames, note_name, parse_note};
use limewave_core::wav::SampleFormat;

use crate::app::LimewaveApp;
use crate::theme::colors;

/// The sample rates the dialog offers.
pub const RATES: [u32; 6] = [22_050, 32_000, 44_100, 48_000, 88_200, 96_000];

/// The sample formats a new file or a recording can have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BitDepth {
    #[default]
    Int16,
    Int24,
    Float32,
}

impl BitDepth {
    pub const ALL: [BitDepth; 3] = [BitDepth::Int16, BitDepth::Int24, BitDepth::Float32];

    pub fn format(self) -> SampleFormat {
        match self {
            BitDepth::Int16 => SampleFormat::Int { bits: 16 },
            BitDepth::Int24 => SampleFormat::Int { bits: 24 },
            BitDepth::Float32 => SampleFormat::Float { bits: 32 },
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BitDepth::Int16 => "16-bit",
            BitDepth::Int24 => "24-bit",
            BitDepth::Float32 => "32-bit float",
        }
    }
}

/// What the length field counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LengthUnit {
    Frames,
    #[default]
    Millis,
}

/// The dialog's values, while it is open.
#[derive(Clone, Debug, PartialEq)]
pub struct NewDialogState {
    pub length: f64,
    pub unit: LengthUnit,
    /// The note whose one cycle the length is set to; empty when the length is free.
    pub cycle_note: String,
    pub rate: u32,
    pub stereo: bool,
    pub bits: BitDepth,
    pub root: u8,
    pub root_text: String,
    pub whole_loop: bool,
}

impl Default for NewDialogState {
    fn default() -> Self {
        NewDialogState {
            length: 1000.0,
            unit: LengthUnit::Millis,
            cycle_note: String::new(),
            rate: 44_100,
            stereo: false,
            bits: BitDepth::default(),
            root: 60,
            root_text: note_name(60),
            whole_loop: false,
        }
    }
}

impl NewDialogState {
    /// The length in frames, or `None` when it is empty or longer than the limit.
    pub fn frames(&self) -> Option<usize> {
        let frames = match self.unit {
            LengthUnit::Frames => self.length,
            LengthUnit::Millis => self.length * f64::from(self.rate) / 1000.0,
        }
        .round();
        let max = f64::from(MAX_NEW_SECONDS) * f64::from(self.rate);
        (frames >= 1.0 && frames <= max).then_some(frames as usize)
    }

    /// Sets the length to one cycle of the note typed in `text`, with that note as the root
    /// key and a loop over the whole file. Text that is not a note changes only the text.
    pub fn set_cycle_note(&mut self, text: &str) {
        self.cycle_note = text.to_owned();
        if let Some(n) = parse_note(text) {
            self.unit = LengthUnit::Frames;
            self.length = cycle_frames(n, self.rate) as f64;
            self.root = n;
            self.root_text = note_name(n);
            self.whole_loop = true;
        }
    }

    /// The length was typed or dragged: it no longer follows a note.
    pub fn length_edited(&mut self) {
        self.cycle_note.clear();
    }

    /// How far the cycle's pitch is from the note, in cents, while a note is set.
    pub fn cents(&self) -> Option<f64> {
        let n = parse_note(&self.cycle_note)?;
        Some(cycle_cents(n, self.rate, self.frames()?))
    }

    /// A new rate keeps the cycle when a note is set, else the duration.
    fn set_rate(&mut self, rate: u32) {
        let old = self.rate;
        self.rate = rate;
        if parse_note(&self.cycle_note).is_some() {
            let text = self.cycle_note.clone();
            self.set_cycle_note(&text);
        } else if self.unit == LengthUnit::Frames {
            self.length = (self.length * f64::from(rate) / f64::from(old)).round();
        }
    }

    /// Switches the unit and converts the length so the duration stays.
    fn set_unit(&mut self, unit: LengthUnit) {
        if unit == self.unit {
            return;
        }
        let per_ms = f64::from(self.rate) / 1000.0;
        self.length = match unit {
            LengthUnit::Frames => (self.length * per_ms).round(),
            LengthUnit::Millis => (self.length / per_ms * 10.0).round() / 10.0,
        };
        self.unit = unit;
    }
}

/// The New… dialog, while `app.ui.new_dialog` is set. Create adds the blank sample; Cancel,
/// Escape and a click outside close it.
pub fn new_dialog(ctx: &egui::Context, app: &mut LimewaveApp) {
    let Some(mut d) = app.ui.new_dialog.take() else {
        return;
    };
    let mut create = false;
    let mut cancel = false;
    let frames = d.frames();
    let modal = egui::Modal::new(egui::Id::unique("new_dialog")).show(ctx, |ui| {
        ui.set_width(340.0);
        ui.heading("New sample");
        ui.add_space(4.0);
        length_row(ui, &mut d);
        cycle_row(ui, &mut d);
        ui.horizontal(|ui| {
            let label = ui.label("Sample rate");
            let rate = d.rate;
            let mut picked = rate;
            ComboBox::from_id_salt("new_rate")
                .selected_text(format!("{rate} Hz"))
                .show_ui(ui, |ui| {
                    for r in RATES {
                        ui.selectable_value(&mut picked, r, format!("{r} Hz"));
                    }
                })
                .response
                .labelled_by(label.id);
            if picked != rate {
                d.set_rate(picked);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Channels");
            ui.selectable_value(&mut d.stereo, false, "Mono");
            ui.selectable_value(&mut d.stereo, true, "Stereo");
        });
        ui.horizontal(|ui| {
            let label = ui.label("Bit depth");
            ComboBox::from_id_salt("new_bits")
                .selected_text(d.bits.label())
                .show_ui(ui, |ui| {
                    for b in BitDepth::ALL {
                        ui.selectable_value(&mut d.bits, b, b.label());
                    }
                })
                .response
                .labelled_by(label.id);
        });
        ui.horizontal(|ui| {
            let label = ui.label("Root key");
            ui.add(TextEdit::singleline(&mut d.root_text).desired_width(60.0))
                .labelled_by(label.id)
                .on_hover_text("A note name such as C4 or A#3, or a MIDI number");
            match parse_note(&d.root_text) {
                Some(n) => d.root = n,
                None => {
                    ui.colored_label(colors::END, "Not a note");
                }
            }
        });
        ui.checkbox(&mut d.whole_loop, "Loop the whole file");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            create = ui
                .add_enabled(frames.is_some(), Button::new("Create"))
                .clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if create && let Some(frames) = d.frames() {
        let channels = if d.stereo { 2 } else { 1 };
        app.create_new(frames, d.rate, channels, d.bits, d.root, d.whole_loop);
    }
    if !(create || cancel || modal.should_close()) {
        app.ui.new_dialog = Some(d);
    }
}

/// The length value and unit, with the limit beside them.
fn length_row(ui: &mut egui::Ui, d: &mut NewDialogState) {
    ui.horizontal(|ui| {
        let label = ui.label("Length");
        let max = f64::from(MAX_NEW_SECONDS) * f64::from(d.rate);
        let drag = match d.unit {
            LengthUnit::Frames => DragValue::new(&mut d.length)
                .range(0.0..=max)
                .max_decimals(0)
                .speed(10.0),
            LengthUnit::Millis => DragValue::new(&mut d.length)
                .range(0.0..=f64::from(MAX_NEW_SECONDS) * 1000.0)
                .max_decimals(1)
                .speed(5.0),
        };
        if ui.add(drag).labelled_by(label.id).changed() {
            d.length_edited();
        }
        let mut unit = d.unit;
        ui.selectable_value(&mut unit, LengthUnit::Millis, "ms");
        ui.selectable_value(&mut unit, LengthUnit::Frames, "frames");
        d.set_unit(unit);
        ui.weak(format!("up to {MAX_NEW_SECONDS} s"));
    });
}

/// The one-cycle-of-a-note shortcut, with the pitch it gives.
fn cycle_row(ui: &mut egui::Ui, d: &mut NewDialogState) {
    ui.horizontal(|ui| {
        let label = ui.label("One cycle of");
        let mut text = d.cycle_note.clone();
        let edit = ui
            .add(
                TextEdit::singleline(&mut text)
                    .hint_text("e.g. A4")
                    .desired_width(60.0),
            )
            .labelled_by(label.id)
            .on_hover_text(
                "Sets the length to one cycle of this note, the root key to the note and \
                 loops the whole file",
            );
        if edit.changed() {
            d.set_cycle_note(&text);
        }
        if let Some(cents) = d.cents() {
            ui.weak(format!("{cents:+.1} cents"));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_cycle_of_a4_sets_length_root_and_loop() {
        let mut d = NewDialogState::default();
        d.set_cycle_note("A4");
        assert_eq!(d.frames(), Some(100));
        assert_eq!(d.unit, LengthUnit::Frames);
        assert_eq!(d.root, 69);
        assert_eq!(d.root_text, "A4");
        assert!(d.whole_loop);
        assert!(d.cents().is_some());
    }

    #[test]
    fn lengths_outside_the_limits_have_no_frames() {
        let mut d = NewDialogState {
            length: 0.0,
            ..NewDialogState::default()
        };
        assert_eq!(d.frames(), None);
        d.length = 600_001.0;
        assert_eq!(d.frames(), None);
        d.length = 600_000.0;
        assert_eq!(d.frames(), Some(26_460_000));
        d.unit = LengthUnit::Frames;
        d.length = 0.0;
        assert_eq!(d.frames(), None);
        d.length = 1.0;
        assert_eq!(d.frames(), Some(1));
    }

    #[test]
    fn editing_the_length_clears_the_cycle_note() {
        let mut d = NewDialogState::default();
        d.set_cycle_note("A4");
        assert!(!d.cycle_note.is_empty());
        d.length_edited();
        assert!(d.cycle_note.is_empty());
        assert_eq!(d.cents(), None);
    }

    #[test]
    fn a_new_rate_keeps_the_cycle_or_the_duration() {
        let mut d = NewDialogState::default();
        d.set_cycle_note("A4");
        d.set_rate(88_200);
        assert_eq!(d.frames(), Some(200));
        let mut d = NewDialogState::default();
        d.set_rate(88_200);
        assert_eq!(d.frames(), Some(88_200));
        d.unit = LengthUnit::Frames;
        d.length = 88_200.0;
        d.set_rate(44_100);
        assert_eq!(d.frames(), Some(44_100));
    }
}
