//! The Edit and Effects menus, and the effect dialogs.

use std::ops::RangeInclusive;

use eframe::egui::{self, Button, DragValue};
use limewave_core::edit::LoopMode;
use limewave_core::effects::{Effect, FadeCurve, MonoSource};
use limewave_core::wav::SampleFormat;

use crate::app::{LimewaveApp, SELECT_ALL_SHORTCUT};
use crate::theme::colors;
use crate::ui::rate_dialog::RateDialogState;
use crate::ui::spectrum_dialog::SpectrumDialogState;
use crate::ui::tail_dialog::TailDialogState;

/// The toolbar's Edit menu. Selection items need a selection and loop items a loop; an item
/// that cannot run is greyed out.
pub fn edit_menu(ui: &mut egui::Ui, app: &mut LimewaveApp) {
    let loaded = app.selected_session().is_some();
    let looped = app
        .selected_session()
        .is_some_and(|s| s.shown_edit().loop_mode != LoopMode::Off);
    let selected = loaded && app.ui.selection.is_some();
    let can_play = selected && app.engine.is_some();
    ui.menu_button("Edit", |ui| {
        let shortcut = ui.ctx().format_shortcut(&SELECT_ALL_SHORTCUT);
        let select_all = Button::new("Select all").shortcut_text(shortcut);
        if ui.add_enabled(loaded, select_all).clicked() {
            app.select_all();
        }
        if ui
            .add_enabled(can_play, Button::new("Play selection"))
            .on_hover_text("Play the selection once at the root key")
            .clicked()
        {
            app.play_selection();
        }
        if ui
            .add_enabled(selected, Button::new("Set loop to selection"))
            .clicked()
        {
            app.set_loop_to_selection();
        }
        if ui
            .add_enabled(selected, Button::new("Set Start/End to selection"))
            .clicked()
        {
            app.set_trim_to_selection();
        }
        if ui
            .add_enabled(selected, Button::new("Crop to selection"))
            .on_hover_text("Remove the audio outside the selection now (undoable)")
            .clicked()
        {
            app.crop_to_selection();
        }
        ui.separator();
        if ui
            .add_enabled(looped, Button::new("Trim after loop end"))
            .on_hover_text("End = loop end + padding")
            .clicked()
        {
            app.trim_after_loop();
        }
        if ui
            .add_enabled(looped, Button::new("Crop to loop"))
            .on_hover_text("Start = loop start, End = loop end + padding")
            .clicked()
        {
            app.crop_to_loop();
        }
        ui.horizontal(|ui| {
            let label = ui.label("Padding");
            ui.add(
                DragValue::new(&mut app.ui.save_dialog.padding)
                    .range(0..=4096)
                    .suffix(" frames"),
            )
            .labelled_by(label.id)
            .on_hover_text("Frames kept after the loop end (also used by Save as…)");
        });
    });
}

/// The dialog's value controls, remembered for the session.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectParams {
    pub gain_db: f32,
    pub normalize_db: f32,
    pub fade_curve: FadeCurve,
    pub silence_db: f32,
    pub mono_source: MonoSource,
}

impl Default for EffectParams {
    fn default() -> Self {
        EffectParams {
            gain_db: 0.0,
            normalize_db: -0.1,
            fade_curve: FadeCurve::Linear,
            silence_db: -60.0,
            mono_source: MonoSource::Mix,
        }
    }
}

/// The effects that ask for a value first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectDialog {
    Gain,
    Normalize,
    FadeIn,
    FadeOut,
    StereoToMono,
    AutoTrim,
}

/// The toolbar's Effects menu. Gain, Normalize, the fades, Stereo to mono, Auto-trim,
/// Generate tail, Make loop from spectrum and Change sample rate open a dialog; the rest apply
/// at once (Repair click offers a channel choice for stereo). A channel tool that does not fit
/// the file is greyed out.
pub fn effects_menu(ui: &mut egui::Ui, app: &mut LimewaveApp) {
    let loaded = app.selected_session().is_some();
    ui.menu_button("Effects", |ui| {
        for (label, dialog) in [
            ("Gain…", EffectDialog::Gain),
            ("Normalize…", EffectDialog::Normalize),
            ("Fade in…", EffectDialog::FadeIn),
            ("Fade out…", EffectDialog::FadeOut),
        ] {
            if ui.add_enabled(loaded, Button::new(label)).clicked() {
                app.ui.effect_dialog = Some(dialog);
            }
        }
        for (label, effect) in [
            ("Invert polarity", Effect::Invert),
            ("Remove DC offset", Effect::RemoveDc),
            ("Reverse", Effect::Reverse),
        ] {
            if ui.add_enabled(loaded, Button::new(label)).clicked() {
                app.apply_effect(effect);
            }
        }
        ui.separator();
        let to_mono = app.can_apply(Effect::StereoToMono(MonoSource::Mix));
        if ui
            .add_enabled(to_mono, Button::new("Stereo to mono…"))
            .clicked()
        {
            app.ui.effect_dialog = Some(EffectDialog::StereoToMono);
        }
        for (label, effect) in [
            ("Swap left and right", Effect::SwapChannels),
            ("Mono to stereo", Effect::MonoToStereo),
        ] {
            let enabled = app.can_apply(effect.clone());
            if ui.add_enabled(enabled, Button::new(label)).clicked() {
                app.apply_effect(effect);
            }
        }
        ui.separator();
        if ui
            .add_enabled(loaded, Button::new("Auto-trim silence…"))
            .clicked()
        {
            app.ui.effect_dialog = Some(EffectDialog::AutoTrim);
        }
        ui.separator();
        repair_click_items(ui, app);
        ui.separator();
        if ui
            .add_enabled(loaded, Button::new("Generate tail…"))
            .on_hover_text("Replace everything after End with new audio made from the sound")
            .clicked()
        {
            app.ui.tail_dialog = Some(TailDialogState::default());
        }
        if ui
            .add_enabled(loaded, Button::new("Make loop from spectrum…"))
            .on_hover_text(
                "Replace the selection, or Start…End, with a seamless loop made from its \
                 average spectrum",
            )
            .clicked()
        {
            app.ui.spectrum_dialog = Some(SpectrumDialogState::default());
        }
        ui.separator();
        if ui
            .add_enabled(loaded, Button::new("Change sample rate…"))
            .on_hover_text("Convert the whole sample to another rate, keeping its pitch")
            .clicked()
        {
            app.ui.rate_dialog = Some(RateDialogState::default());
        }
    });
}

/// The Repair click entry: a button, or for a stereo file a submenu choosing the channels.
/// It needs a loaded sample and a selection.
fn repair_click_items(ui: &mut egui::Ui, app: &mut LimewaveApp) {
    let channels = app.selected_session().map_or(0, |s| s.processed.len());
    let enabled = channels > 0 && app.ui.selection.is_some();
    let hover = "Rebuild the selection (up to 50 ms) from the audio on either side";
    if channels == 2 {
        ui.add_enabled_ui(enabled, |ui| {
            ui.menu_button("Repair click", |ui| {
                for (label, picked) in [
                    ("Both channels", &[0, 1][..]),
                    ("Left only", &[0]),
                    ("Right only", &[1]),
                ] {
                    if ui.button(label).clicked() {
                        app.repair_click(picked);
                    }
                }
            })
            .response
            .on_hover_text(hover);
        });
    } else if ui
        .add_enabled(enabled, Button::new("Repair click"))
        .on_hover_text(hover)
        .clicked()
    {
        let all: Vec<usize> = (0..channels).collect();
        app.repair_click(&all);
    }
}

/// The effect the open dialog applies with the remembered values; `None` for Auto-trim,
/// which is not an effect.
fn dialog_effect(dialog: EffectDialog, p: &EffectParams) -> Option<Effect> {
    match dialog {
        EffectDialog::Gain => Some(Effect::Gain { db: p.gain_db }),
        EffectDialog::Normalize => Some(Effect::Normalize {
            target_db: p.normalize_db,
        }),
        EffectDialog::FadeIn => Some(Effect::FadeIn(p.fade_curve)),
        EffectDialog::FadeOut => Some(Effect::FadeOut(p.fade_curve)),
        EffectDialog::StereoToMono => Some(Effect::StereoToMono(p.mono_source)),
        EffectDialog::AutoTrim => None,
    }
}

/// The modal that asks for the value of the effect in `app.ui.effect_dialog`. Apply runs it
/// and closes; Cancel, Escape and a click outside close it.
pub fn effect_dialog(ctx: &egui::Context, app: &mut LimewaveApp) {
    let Some(dialog) = app.ui.effect_dialog else {
        return;
    };
    // Gain and Normalize can push an integer file's peak past full scale.
    let effect = dialog_effect(dialog, &app.ui.effect_params);
    let selection = app.ui.selection;
    let clips = app.selected_session_mut().is_some_and(|s| {
        let integer = matches!(s.doc.format.format, SampleFormat::Int { .. });
        let scaling = matches!(dialog, EffectDialog::Gain | EffectDialog::Normalize);
        integer
            && scaling
            && effect
                .and_then(|e| s.predicted_peak(e, selection))
                .is_some_and(|p| p > 1.0)
    });
    let mut apply = false;
    let mut cancel = false;
    let p = &mut app.ui.effect_params;
    let modal = egui::Modal::new(egui::Id::unique("effect_dialog")).show(ctx, |ui| {
        ui.set_width(300.0);
        match dialog {
            EffectDialog::Gain => {
                ui.heading("Gain");
                value_row(ui, "Gain (dB)", &mut p.gain_db, -48.0..=48.0);
            }
            EffectDialog::Normalize => {
                ui.heading("Normalize");
                value_row(ui, "Target peak (dBFS)", &mut p.normalize_db, -48.0..=0.0);
            }
            EffectDialog::FadeIn | EffectDialog::FadeOut => {
                ui.heading(if dialog == EffectDialog::FadeIn {
                    "Fade in"
                } else {
                    "Fade out"
                });
                ui.horizontal(|ui| {
                    for (curve, name) in [
                        (FadeCurve::Linear, "Linear"),
                        (FadeCurve::Exponential, "Exponential"),
                        (FadeCurve::SCurve, "S-curve"),
                    ] {
                        ui.selectable_value(&mut p.fade_curve, curve, name);
                    }
                });
            }
            EffectDialog::StereoToMono => {
                ui.heading("Stereo to mono");
                ui.label("Keep");
                ui.horizontal(|ui| {
                    for (pick, name) in [
                        (MonoSource::Left, "Left"),
                        (MonoSource::Right, "Right"),
                        (MonoSource::Mix, "Mix"),
                    ] {
                        ui.selectable_value(&mut p.mono_source, pick, name)
                            .on_hover_text(match pick {
                                MonoSource::Left => "Keep the left channel",
                                MonoSource::Right => "Keep the right channel",
                                MonoSource::Mix => "Average the channels",
                            });
                    }
                });
            }
            EffectDialog::AutoTrim => {
                ui.heading("Auto-trim silence");
                value_row(ui, "Threshold (dBFS)", &mut p.silence_db, -96.0..=0.0);
            }
        }
        if clips {
            ui.colored_label(colors::END, "Will clip when saved");
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            apply = ui.button("Apply").clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if apply {
        let p = app.ui.effect_params.clone();
        match dialog_effect(dialog, &p) {
            Some(effect) => {
                let normalize = matches!(effect, Effect::Normalize { .. });
                if !app.apply_effect(effect) && normalize {
                    app.ui.message = Some("Nothing to normalize: the audio is silent".to_owned());
                }
            }
            None => app.auto_trim_silence(p.silence_db),
        }
    }
    if apply || cancel || modal.should_close() {
        app.ui.effect_dialog = None;
    }
}

/// A labelled decibel value.
fn value_row(ui: &mut egui::Ui, label: &str, value: &mut f32, range: RangeInclusive<f32>) {
    ui.horizontal(|ui| {
        let label = ui.label(label);
        ui.add(DragValue::new(value).range(range).speed(0.1).suffix(" dB"))
            .labelled_by(label.id);
    });
}
