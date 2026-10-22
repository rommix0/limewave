//! The inspector: file metadata as read, tuning and loop settings, and the Save buttons.

use eframe::egui::{
    self, DragValue, Grid, Response, RichText, ScrollArea, Slider, TextEdit, Ui, text::CCursor,
    text::CCursorRange,
};
use limewave_core::edit::{EditState, FadeShape, LoopMode, Marker, move_marker};
use limewave_core::notes::{note_name, parse_note};
use limewave_core::save::Destination;
use limewave_core::sfz::region_edit;
use limewave_core::wav::SampleFormat;

use crate::app::{Session, UiState};
use crate::audio::engine::AudioEngine;
use crate::audio::voice::Adsr;
use crate::theme::colors;
use crate::ui::region_inspector::{RegionContext, RegionEdit, region_note, region_section};
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
pub(crate) fn format_text(s: &Session) -> String {
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
    format!("{sample}, {} Hz, {channels}", s.rate())
}

fn loop_mode_text(m: LoopMode) -> &'static str {
    match m {
        LoopMode::Off => "none",
        LoopMode::Forward => "forward",
        LoopMode::PingPong => "ping-pong",
        LoopMode::Backward => "backward",
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

/// What the user asked of the app in the inspector this frame.
#[derive(Default, Debug, PartialEq)]
pub struct InspectorAction {
    /// The Save button clicked.
    pub save: Option<SaveCommand>,
    /// The "Set from MIDI" toggle was clicked.
    pub toggle_midi_learn: bool,
    /// What the user changed in the Region section, in order.
    pub region: Vec<RegionEdit>,
}

/// Draws the inspector of `s`. With an instrument open, `region` is its selected region: the
/// Region section comes first, Tuning and Loop say who shares the sample (and show a region
/// that keeps its own values read-only), and the session envelope is left out because each
/// region has its own. Returns what the user clicked that the app has to act on.
pub fn inspector(
    ui: &mut Ui,
    s: &mut Session,
    ui_state: &mut UiState,
    engine: Option<&AudioEngine>,
    region: Option<&RegionContext>,
) -> InspectorAction {
    // A region that keeps its own tuning and loop shows those, and cannot change them here.
    let read_only = region.is_some_and(|r| !r.follows);
    let mut shown = s.shown_edit().clone();
    if let Some(r) = region.filter(|r| !r.follows) {
        let own = region_edit(r.st.shown(), r.region, &s.doc.initial_edit());
        shown = EditState {
            root: own.root,
            cents: own.cents,
            loop_mode: own.loop_mode,
            loop_start: own.loop_start,
            loop_end: own.loop_end,
            ..shown
        };
    }
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut edits = Vec::new();
            if let Some(r) = region {
                edits = region_section(ui, r.st, r.region);
                ui.separator();
            }
            file_section(ui, s);
            ui.separator();
            ui.heading("Tuning");
            region_note(ui, region);
            let toggle_midi_learn = ui
                .add_enabled_ui(!read_only, |ui| {
                    tuning_section(ui, s, ui_state, engine, &shown)
                })
                .inner;
            ui.separator();
            ui.heading("Loop");
            region_note(ui, region);
            ui.add_enabled_ui(!read_only, |ui| {
                loop_section(ui, s, ui_state, engine, &shown);
            });
            ui.separator();
            if region.is_none() {
                envelope_section(ui, &mut ui_state.envelope, &mut ui_state.release_after_loop);
                ui.separator();
            }
            InspectorAction {
                save: save_section(ui, s),
                toggle_midi_learn,
                region: edits,
            }
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
        // Only ever an effective destination; sessions remember the folder.
        Some(Destination::ReplaceSaved) => "Save replaces this file".to_owned(),
        Some(Destination::File(path)) => format!("Save writes to {}", path.display()),
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
    if s.will_clip() {
        ui.colored_label(colors::END, "Will clip when saved");
    }
}

/// The root key as editable text. Enter or leaving the field applies a valid note name;
/// anything else, or Escape, puts the old key back.
fn root_field(ui: &mut Ui, s: &mut Session, engine: Option<&AudioEngine>, root: u8) {
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

/// Returns whether the "Set from MIDI" toggle was clicked.
fn tuning_section(
    ui: &mut Ui,
    s: &mut Session,
    ui_state: &UiState,
    engine: Option<&AudioEngine>,
    shown: &EditState,
) -> bool {
    root_field(ui, s, engine, shown.root);
    let mut toggled = false;
    if ui_state.midi_connected {
        let listening = ui_state.midi_learn.is_some();
        let text = if listening {
            "Listening…"
        } else {
            "Set from MIDI"
        };
        toggled = ui
            .selectable_label(listening, text)
            .on_hover_text("The next note from the MIDI device becomes the root key")
            .clicked();
    }
    ui.horizontal(|ui| {
        let name = ui.label("Fine tune");
        let mut cents = shown.cents;
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
    toggled
}

fn loop_section(
    ui: &mut Ui,
    s: &mut Session,
    ui_state: &UiState,
    engine: Option<&AudioEngine>,
    shown: &EditState,
) {
    // Fields range over the file as it is now; marker moves take the original count.
    let last = s.frames().saturating_sub(1);
    let original_frames = s.doc.frames;
    let loop_on = shown.loop_mode != LoopMode::Off;
    let backward = shown.loop_mode == LoopMode::Backward;

    ui.horizontal(|ui| {
        for (mode, label) in [
            (LoopMode::Off, "Off"),
            (LoopMode::Forward, "Fwd"),
            (LoopMode::PingPong, "Ping-pong"),
            (LoopMode::Backward, "Backward"),
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
            move_marker(e, marker, v, ui_state.lock_length, original_frames)
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

    ui.add_enabled_ui(loop_on && !backward, |ui| {
        let rate = s.rate();
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
        if !backward {
            ui.label(
                RichText::new("Crossfade is mixed into the audio when you save (destructive).")
                    .color(colors::XFADE),
            );
        }
        if s.just_saved {
            ui.label(RichText::new("crossfade applied in saved file").color(colors::LIME));
        }
    });
    if backward {
        // The crossfade setting is kept, so switching back to a forward loop restores it.
        ui.label(RichText::new("Crossfade is off for backward loops").color(colors::XFADE));
        ui.weak("Some samplers ignore backward loops and play them forward.");
    }
}

/// What the user did to the four envelope fields this frame.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdsrFields {
    pub changed: bool,
    pub dragged: bool,
    pub drag_stopped: bool,
}

/// Attack, decay and release in ms and sustain in %: the controls of the audition envelope,
/// also used for a region's envelope.
pub fn adsr_fields(ui: &mut Ui, envelope: &mut Adsr) -> AdsrFields {
    fn time(ui: &mut Ui, name: &str, ms: &mut f32) -> Response {
        ui.horizontal(|ui| {
            let label = ui.label(name);
            let field = ui.add(
                DragValue::new(ms)
                    .range(0.0..=10_000.0)
                    .clamp_existing_to_range(false)
                    .speed(1.0)
                    .max_decimals(1)
                    .suffix(" ms"),
            );
            field.labelled_by(label.id)
        })
        .inner
    }
    let attack = time(ui, "Attack", &mut envelope.attack_ms);
    let decay = time(ui, "Decay", &mut envelope.decay_ms);
    let sustain = ui
        .horizontal(|ui| {
            let label = ui.label("Sustain");
            let mut percent = envelope.sustain * 100.0;
            let field = ui.add(
                DragValue::new(&mut percent)
                    .range(0.0..=100.0)
                    .clamp_existing_to_range(false)
                    .speed(0.5)
                    .max_decimals(1)
                    .suffix(" %"),
            );
            let field = field.labelled_by(label.id);
            // Only a change is written back, so the %-to-fraction round trip cannot drift.
            if field.changed() {
                envelope.sustain = percent / 100.0;
            }
            field
        })
        .inner;
    let release = time(ui, "Release", &mut envelope.release_ms);
    let all = [attack, decay, sustain, release];
    AdsrFields {
        changed: all.iter().any(Response::changed),
        dragged: all.iter().any(Response::dragged),
        drag_stopped: all.iter().any(Response::drag_stopped),
    }
}

/// The audition envelope, then "Release plays after loop end" (v0.3 §7.3).
fn envelope_section(ui: &mut Ui, envelope: &mut Adsr, release_after_loop: &mut bool) {
    ui.heading("Envelope");
    adsr_fields(ui, envelope);
    ui.checkbox(release_after_loop, "Release plays after loop end")
        .on_hover_text(RELEASE_AFTER_LOOP_HINT);
}

/// The hover text of "Release plays after loop end" (v0.3 §7.3).
const RELEASE_AFTER_LOOP_HINT: &str = concat!(
    "When a looped note is released, it stops looping and plays on through the loop end ",
    "into the rest of the audio, as a sampler with a sustain loop does. The envelope's ",
    "release still applies; a long release lets the whole release be heard.",
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_after_loop_hint_has_single_spaces() {
        let hint = RELEASE_AFTER_LOOP_HINT;
        assert!(!hint.contains("  ") && !hint.contains('\n'), "{hint}");
        assert!(hint.ends_with("be heard."));
    }

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
