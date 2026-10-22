//! The Change sample rate… dialog (v0.6). The user types any whole-hertz rate; nothing renders
//! until Convert is pressed, and then the conversion runs on a worker thread so the window
//! stays responsive. When the result lands it is applied as one history entry and the dialog
//! closes. Cancel and Escape close it, and a running render finishes unseen.

use std::hash::{DefaultHasher, Hash, Hasher};

use eframe::egui::{self, Button, ComboBox, TextEdit};
use limewave_core::dsp::resample::{MAX_RATE, MIN_RATE, resampled_len};
use limewave_core::edit::{EditState, resample_edit};

use crate::app::LimewaveApp;
use crate::render_worker::{RenderSlot, spawn_render};
use crate::theme::colors;
use crate::ui::dialog_kit::fixed_height;

/// The rates the Common rates menu offers, in hertz.
pub const COMMON_RATES: [u32; 9] = [
    8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000,
];

/// The lines under the field (why Convert is off, the length after conversion, the warning
/// about lost content, the converting note) share this much room, so the buttons stay put.
const MESSAGE_LINES: f32 = 3.0;

/// The dialog's state: the typed rate and the conversion, once Convert has started one.
#[derive(Default)]
pub struct RateDialogState {
    /// What is typed in the New rate field: ASCII digits only.
    pub text: String,
    /// The conversion started by Convert, rendered for the typed rate. A result of `None`
    /// means the rate could not be converted to.
    pub(crate) render: RenderSlot<Option<EditState>>,
    /// Whether the field has been given the keyboard focus yet.
    focused: bool,
}

/// The rate typed in `text`. The error is the message the dialog shows: nothing typed, or a
/// number outside `MIN_RATE..=MAX_RATE`. Leading zeros are fine.
pub fn parse_rate(text: &str) -> Result<u32, String> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Type a sample rate in Hz".into());
    }
    // Digits only, so a parse failure is a number too big for a `u32`.
    match text.parse::<u32>() {
        Ok(rate) if (MIN_RATE..=MAX_RATE).contains(&rate) => Ok(rate),
        _ => Err(format!("Use a rate from {MIN_RATE} to {MAX_RATE} Hz")),
    }
}

/// The settings hash of a render to `rate`.
fn rate_hash(rate: u32) -> u64 {
    let mut h = DefaultHasher::new();
    rate.hash(&mut h);
    h.finish()
}

/// The typed rate, if Convert can convert to it: valid and not the current rate.
fn usable(typed: &Result<u32, String>, current: u32) -> Option<u32> {
    typed.as_ref().ok().copied().filter(|&r| r != current)
}

/// The modal, while `app.ui.rate_dialog` is set. Convert starts the conversion; the result,
/// once it lands for the rate typed, is applied and closes the dialog. Cancel, Escape and a
/// click outside close it.
pub fn rate_dialog(ctx: &egui::Context, app: &mut LimewaveApp) {
    let Some(mut d) = app.ui.rate_dialog.take() else {
        return;
    };
    let Some(s) = app.selected_session() else {
        // The file is gone: nothing to convert.
        return;
    };
    let (current, frames) = (s.rate(), s.frames());
    let typed = parse_rate(&d.text);
    let rate = usable(&typed, current);
    let key = rate.and_then(|r| app.render_key(rate_hash(r)));

    // A finished conversion for the rate typed: apply it and close.
    if let Some(Some(edit)) = d.render.result(key) {
        app.apply_rate_edit(edit.clone());
        return;
    }
    let converting = d.render.is_rendering();
    let failed = d.render.failed(key) || matches!(d.render.result(key), Some(None));

    let mut start = false;
    let mut cancel = false;
    let before = d.text.clone();
    let modal = egui::Modal::new(egui::Id::unique("rate_dialog")).show(ctx, |ui| {
        ui.set_width(340.0);
        ui.heading("Change sample rate");
        ui.add_space(4.0);
        ui.label(format!("Current rate: {current} Hz"));
        ui.horizontal(|ui| {
            let label = ui.label("New rate");
            let field = ui
                .add(TextEdit::singleline(&mut d.text).desired_width(80.0))
                .labelled_by(label.id);
            ui.label("Hz");
            if !d.focused {
                field.request_focus();
                d.focused = true;
            }
            // Enter in the field converts, as Convert does.
            if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                start = true;
            }
        });
        ui.horizontal(|ui| {
            let label = ui.label("Common rates");
            let mut picked = None;
            ComboBox::from_id_salt("rate_common")
                .selected_text("Choose…")
                .show_ui(ui, |ui| {
                    for r in COMMON_RATES {
                        let on = d.text == r.to_string();
                        if ui.selectable_label(on, format!("{r} Hz")).clicked() {
                            picked = Some(r);
                        }
                    }
                })
                .response
                .labelled_by(label.id);
            if let Some(r) = picked {
                d.text = r.to_string();
            }
        });
        ui.add_space(4.0);
        let line = ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y;
        fixed_height(ui, MESSAGE_LINES * line, |ui| {
            if let Err(why) = &typed {
                ui.colored_label(colors::END, why);
            } else if rate.is_none() {
                ui.colored_label(colors::END, "That is the current rate");
            }
            if let Some(r) = rate {
                let after = resampled_len(frames, current, r);
                ui.label(format!(
                    "Length after: {after} frames ({:.2} s)",
                    after as f64 / f64::from(r)
                ));
                if r < current {
                    ui.colored_label(
                        colors::LOOP,
                        format!("Content above {} Hz will be removed", r / 2),
                    );
                }
            }
            if converting {
                ui.weak("Converting…");
            } else if failed {
                ui.colored_label(colors::END, "Conversion failed");
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let ready = rate.is_some() && !converting;
            start |= ui.add_enabled(ready, Button::new("Convert")).clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    d.text.retain(|c| c.is_ascii_digit());
    if d.text != before {
        // Another rate: a running conversion is not the one wanted any more.
        d.render.restart();
    }

    if cancel || modal.should_close() {
        return;
    }
    // Enter can come while the field holds a rate Convert cannot use; the text is the new one.
    if start
        && !d.render.is_rendering()
        && let Some(r) = usable(&parse_rate(&d.text), current)
        && let Some(key) = app.render_key(rate_hash(r))
        && let Some(s) = app.selected_session()
    {
        let (audio, e) = (s.processed.clone(), s.edit().clone());
        let (file_rate, original) = (s.doc.format.sample_rate, s.doc.frames);
        d.render.start(spawn_render(key, ctx.clone(), move || {
            resample_edit(&e, &audio, r, file_rate, original)
        }));
    }
    app.ui.rate_dialog = Some(d);
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGE: &str = "Use a rate from 1000 to 768000 Hz";
    const TYPE: &str = "Type a sample rate in Hz";

    #[test]
    fn parse_rate_accepts_a_range_of_whole_hertz() {
        assert_eq!(parse_rate(""), Err(TYPE.into()));
        assert_eq!(parse_rate("999"), Err(RANGE.into()));
        assert_eq!(parse_rate("1000"), Ok(1000));
        assert_eq!(parse_rate("044100"), Ok(44_100), "leading zeros");
        assert_eq!(parse_rate("768000"), Ok(768_000));
        assert_eq!(parse_rate("768001"), Err(RANGE.into()));
        assert_eq!(parse_rate("99999999999"), Err(RANGE.into()), "no panic");
        assert_eq!(parse_rate("0"), Err(RANGE.into()));
    }

    #[test]
    fn parse_rate_rejects_text_that_is_not_digits() {
        for text in ["4 4", "44,1", "-5", "+5", "1e3", " "] {
            assert_eq!(parse_rate(text), Err(TYPE.into()), "{text:?}");
        }
    }

    #[test]
    fn common_rates_are_all_in_range() {
        for r in COMMON_RATES {
            assert_eq!(parse_rate(&r.to_string()), Ok(r));
        }
    }

    #[test]
    fn convert_is_disabled_and_converting_shows_while_it_runs() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::{NodeT, Queryable};
        use limewave_core::testwav::{sine, wav_bytes};
        use limewave_core::wav::SampleFormat;

        let bytes = wav_bytes(
            &[sine(440.0, 44100, 4000, 0.5)],
            44100,
            SampleFormat::Int { bits: 16 },
            Vec::new(),
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rate.wav");
        std::fs::write(&path, bytes).unwrap();
        let mut app = LimewaveApp::for_test();
        app.open_file(path);
        let key = app.render_key(rate_hash(22_050)).unwrap();
        // A conversion that is held up until the test lets it go.
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let mut d = RateDialogState {
            text: "22050".into(),
            ..Default::default()
        };
        d.render
            .start(spawn_render(key, egui::Context::default(), move || {
                wait.recv().unwrap();
                None
            }));
        app.ui.rate_dialog = Some(d);
        let mut h = Harness::builder()
            .allow_missing_glyphs()
            .build_ui_state(|ui, app: &mut LimewaveApp| app.show(ui), app);
        h.run();
        h.get_by_label("Converting…");
        assert!(h.get_by_label("Convert").accesskit_node().is_disabled());
        assert!(!h.get_by_label("Cancel").accesskit_node().is_disabled());

        // Letting it go with no edit made says so, and Convert can try again.
        go.send(()).unwrap();
        h.state_mut().wait_for_renders();
        h.run();
        h.get_by_label("Conversion failed");
        assert!(!h.get_by_label("Convert").accesskit_node().is_disabled());
    }
}
