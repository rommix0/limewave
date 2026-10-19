//! The inspector: file metadata as read, tuning and loop settings, and the Save buttons.

use eframe::egui::{
    self, DragValue, Grid, Response, RichText, ScrollArea, Slider, TextEdit, Ui, text::CCursor,
    text::CCursorRange,
};
use seamlime_core::edit::{EditState, FadeShape, LoopMode, Marker, move_marker};
use seamlime_core::notes::{note_name, parse_note};
use seamlime_core::save::Destination;
use seamlime_core::wav::SampleFormat;

use crate::app::{Session, UiState};
use crate::audio::engine::AudioEngine;
use crate::theme::colors;
use crate::ui::save_dialog::SaveCommand;

/// Crossfade length in milliseconds for `frames` frames.
pub fn frames_to_ms(frames: usize, rate: u32) -> f64 {
    frames as f64 * 1000.0 / f64::from(rate.max(1))
}

/// Crossfade length in frames for `ms` milliseconds, at most `max` frames.
pub fn ms_to_frames(ms: f64, rate: u32, max: usize) -> usize {
    let frames = (ms.max(0.0) * f64::from(rate) / 1000.0).round();
    (frames as usize).min(max)
}

/// The root key `n` moved by `semitones`, or `None` when it leaves the MIDI range.
fn shifted_root(n: u8, semitones: i16) -> Option<u8> {
    u8::try_from(i16::from(n) + semitones)
        .ok()
        .filter(|&r| r <= 127)
}

/// "16-bit PCM, 44100 Hz, mono" and the like.
fn format_text(s: &Session) -> String {
    let f = &s.doc.format;
    let sample = match f.format {
        SampleFormat::Int { bits } => format!("{bits}-bit PCM"),
        SampleFormat::Float { bits } => format!("{bits}-bit float"),
    };
    let channels = match f.channels {
        1 => "mono".to_owned(),
        2 => "stereo".to_owned(),
        n => format!("{n} channels"),
    };
    format!("{sample}, {} Hz, {channels}", f.sample_rate)
}

fn loop_mode_text(m: LoopMode) -> &'static str {
    match m {
        LoopMode::Off => "none",
        LoopMode::Forward => "forward",
        LoopMode::PingPong => "ping-pong",
    }
}

/// Records the result of a widget the user changed. A drag is previewed and becomes one
/// history entry on release; anything else (typing, a click) is its own entry.
fn track(
    s: &mut Session,
    engine: Option<&AudioEngine>,
    response: &Response,
    edit: impl FnOnce(&EditState) -> EditState,
) {
    if response.changed() {
        let e = edit(s.edit());
        if response.dragged() {
            s.set_preview(e, engine);
        } else {
            s.apply(e, engine);
        }
    }
    if response.drag_stopped() {
        s.commit_preview(engine);
    }
}

/// Draws the inspector of `s`. Returns the Save button clicked, if any.
pub fn inspector(
    ui: &mut Ui,
    s: &mut Session,
    ui_state: &UiState,
    engine: Option<&AudioEngine>,
) -> Option<SaveCommand> {
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            file_section(ui, s);
            ui.separator();
            tuning_section(ui, s, engine);
            ui.separator();
            loop_section(ui, s, ui_state, engine);
            ui.separator();
            save_section(ui, s)
        })
        .inner
}

fn save_section(ui: &mut Ui, s: &Session) -> Option<SaveCommand> {
    ui.heading("Save");
    let mut command = None;
    ui.horizontal(|ui| {
        if ui.button("Save").clicked() {
            command = Some(SaveCommand::Save);
        }
        if ui.button("Save as…").clicked() {
            command = Some(SaveCommand::SaveAs);
        }
    });
    let place = match &s.destination {
        Some(Destination::Folder(dir)) => format!("Save writes to {}", dir.display()),
        Some(Destination::OverwriteWithBackup) => {
            "Save overwrites the file, keeping a .bak copy".to_owned()
        }
        None => "Save asks where to write the file".to_owned(),
    };
    ui.add(egui::Label::new(RichText::new(place).weak()).wrap());
    command
}

fn file_section(ui: &mut Ui, s: &Session) {
    ui.heading("File");
    let read = s.doc.initial_edit();
    let has_smpl = s.doc.smpl.is_some();
    let dash = || "-".to_owned();
    let root = if has_smpl {
        note_name(read.root)
    } else {
        dash()
    };
    let tune = if has_smpl {
        format!("{:+.1} ¢", read.cents)
    } else {
        dash()
    };
    let looped = match read.loop_mode {
        LoopMode::Off => "none".to_owned(),
        mode => format!(
            "{}..={} ({})",
            read.loop_start,
            read.loop_end,
            loop_mode_text(mode)
        ),
    };
    Grid::new("file_meta")
        .num_columns(2)
        .spacing([10.0, 3.0])
        .show(ui, |ui| {
            for (name, value) in [
                ("Root", root),
                ("Tune", tune),
                ("Loop", looped),
                ("Name", s.doc.name.clone().unwrap_or_else(dash)),
                ("Written by", s.doc.software.clone().unwrap_or_else(dash)),
                ("Format", format_text(s)),
            ] {
                ui.weak(name);
                ui.add(egui::Label::new(value).wrap());
                ui.end_row();
            }
        });
    for warning in &s.doc.warnings {
        ui.colored_label(colors::LOOP, warning);
    }
    if s.doc.has_markers {
        ui.colored_label(colors::END, "Markers in this file will be removed on save");
    }
}

/// The root key as editable text. Enter or leaving the field applies a valid note name;
/// anything else, or Escape, puts the old key back.
fn root_field(ui: &mut Ui, s: &mut Session, engine: Option<&AudioEngine>) {
    let root = s.shown_edit().root;
    ui.horizontal(|ui| {
        let label = ui.label("Root key");
        let id = ui.make_persistent_id("root_key_text");
        let was_focused = ui.memory(|m| m.has_focus(id));
        let mut text = if was_focused {
            ui.data(|d| d.get_temp::<String>(id))
        } else {
            None
        }
        .unwrap_or_else(|| note_name(root));
        let mut out = TextEdit::singleline(&mut text)
            .id(id)
            .desired_width(48.0)
            .show(ui);
        let _ = Response::clone(&out.response).labelled_by(label.id);
        let response = &out.response;
        if response.gained_focus() {
            let all = CCursorRange::two(CCursor::new(0), CCursor::new(text.chars().count()));
            out.state.cursor.set_char_range(Some(all));
            out.state.store(ui.ctx(), id);
        }
        if response.has_focus() {
            ui.data_mut(|d| d.insert_temp(id, text.clone()));
        } else {
            ui.data_mut(|d| d.remove_temp::<String>(id));
        }
        let cancelled = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if response.lost_focus()
            && !cancelled
            && let Some(n) = parse_note(&text)
            && n != root
        {
            let e = EditState {
                root: n,
                ..s.edit().clone()
            };
            s.apply(e, engine);
        }
        for (label, hint, semitones) in [("½", "Down an octave", -12), ("2×", "Up an octave", 12)]
        {
            let target = shifted_root(root, semitones);
            let button = ui.add_enabled(target.is_some(), egui::Button::new(label));
            if button.on_hover_text(hint).clicked()
                && let Some(n) = target
            {
                let e = EditState {
                    root: n,
                    ..s.edit().clone()
                };
                s.apply(e, engine);
            }
        }
    });
}

fn tuning_section(ui: &mut Ui, s: &mut Session, engine: Option<&AudioEngine>) {
    ui.heading("Tuning");
    root_field(ui, s, engine);
    ui.horizontal(|ui| {
        let name = ui.label("Fine tune");
        let mut cents = s.shown_edit().cents;
        let slider = ui.add(
            Slider::new(&mut cents, -50.0..=50.0)
                .show_value(false)
                .step_by(1.0),
        );
        let slider = slider.labelled_by(name.id);
        track(s, engine, &slider, |e| EditState { cents, ..e.clone() });
        let field = ui.add(
            DragValue::new(&mut cents)
                .range(-50.0..=50.0)
                .speed(0.1)
                .fixed_decimals(1)
                .suffix(" ¢"),
        );
        let field = field.labelled_by(name.id);
        track(s, engine, &field, |e| EditState { cents, ..e.clone() });
    });
}

fn loop_section(ui: &mut Ui, s: &mut Session, ui_state: &UiState, engine: Option<&AudioEngine>) {
    ui.heading("Loop");
    let shown = s.shown_edit().clone();
    let frames = s.doc.frames;
    let last = frames.saturating_sub(1);
    let loop_on = shown.loop_mode != LoopMode::Off;

    ui.horizontal(|ui| {
        for (mode, label) in [
            (LoopMode::Off, "Off"),
            (LoopMode::Forward, "Fwd"),
            (LoopMode::PingPong, "Ping-pong"),
        ] {
            if ui
                .selectable_label(shown.loop_mode == mode, label)
                .clicked()
                && shown.loop_mode != mode
            {
                let e = EditState {
                    loop_mode: mode,
                    ..s.edit().clone()
                };
                s.apply(e, engine);
            }
        }
    });

    // A frame-position field that moves `marker` through the usual marker constraints.
    let position = |ui: &mut Ui, s: &mut Session, label: &str, marker: Marker, value: usize| {
        let name = ui.label(label);
        let mut v = value;
        let r = ui.add(DragValue::new(&mut v).range(0..=last).speed(1.0));
        let r = r.labelled_by(name.id);
        track(s, engine, &r, |e| {
            move_marker(e, marker, v, ui_state.lock_length, frames)
        });
        ui.end_row();
    };
    Grid::new("loop_points")
        .num_columns(2)
        .spacing([10.0, 3.0])
        .show(ui, |ui| {
            position(ui, s, "Start", Marker::Start, shown.start);
            position(ui, s, "End", Marker::End, shown.end);
            // Not `add_enabled_ui`: a nested ui would break the grid rows.
            if !loop_on {
                ui.disable();
            }
            position(ui, s, "Loop start", Marker::LoopStart, shown.loop_start);
            position(ui, s, "Loop end", Marker::LoopEnd, shown.loop_end);
        });

    ui.add_enabled_ui(loop_on, |ui| {
        let rate = s.doc.format.sample_rate;
        let max = shown.max_crossfade();
        let mut ms = frames_to_ms(shown.crossfade, rate);
        ui.horizontal(|ui| {
            let name = ui.label("Crossfade");
            ui.add_enabled_ui(max > 0, |ui| {
                let slider = ui.add(
                    Slider::new(&mut ms, 0.0..=frames_to_ms(max, rate).max(0.001))
                        .show_value(false),
                );
                let slider = slider.labelled_by(name.id);
                track(s, engine, &slider, |e| EditState {
                    crossfade: ms_to_frames(ms, rate, max),
                    ..e.clone()
                });
                let field = ui.add(
                    DragValue::new(&mut ms)
                        .range(0.0..=frames_to_ms(max, rate))
                        .speed(0.1)
                        .max_decimals(2)
                        .suffix(" ms"),
                );
                let field = field.labelled_by(name.id);
                track(s, engine, &field, |e| EditState {
                    crossfade: ms_to_frames(ms, rate, max),
                    ..e.clone()
                });
            });
        });
        ui.horizontal(|ui| {
            ui.label("Shape");
            for (shape, label) in [
                (FadeShape::EqualPower, "Equal power"),
                (FadeShape::Linear, "Linear"),
            ] {
                if ui
                    .selectable_label(shown.fade_shape == shape, label)
                    .clicked()
                    && shown.fade_shape != shape
                {
                    let e = EditState {
                        fade_shape: shape,
                        ..s.edit().clone()
                    };
                    s.apply(e, engine);
                }
            }
        });
        ui.label(
            RichText::new("Crossfade is mixed into the audio when you save (destructive).")
                .color(colors::XFADE),
        );
        if s.just_saved {
            ui.label(RichText::new("crossfade applied in saved file").color(colors::LIME));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossfade_ms_round_trips_through_frames() {
        assert_eq!(ms_to_frames(10.0, 44100, 10_000), 441);
        assert_eq!(
            ms_to_frames(10.0, 44100, 100),
            100,
            "clamped to the maximum"
        );
        assert_eq!(ms_to_frames(-3.0, 44100, 100), 0);
        assert_eq!(ms_to_frames(0.01, 44100, 100), 0);
        for frames in [0, 1, 441, 12_345] {
            let ms = frames_to_ms(frames, 48000);
            assert_eq!(ms_to_frames(ms, 48000, usize::MAX), frames);
        }
    }

    #[test]
    fn octave_shift_stays_in_midi_range() {
        assert_eq!(shifted_root(60, 12), Some(72));
        assert_eq!(shifted_root(60, -12), Some(48));
        assert_eq!(shifted_root(5, -12), None);
        assert_eq!(shifted_root(120, 12), None);
        assert_eq!(shifted_root(115, 12), Some(127));
    }
}
