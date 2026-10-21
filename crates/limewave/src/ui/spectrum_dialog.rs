//! The Make loop from spectrum… dialog (v0.3 §7.2). It analyses the source range (the
//! selection, or Start…End) and renders the loop on worker threads, so the window stays
//! responsive. Changing a setting renders again after a pause; Preview plays the result
//! without touching the session, and Apply records it as one history entry.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Duration;

use eframe::egui::{
    self, Button, Checkbox, Color32, ComboBox, DragValue, Shape, Stroke, TextStyle, pos2,
};
use limewave_core::dsp::spectrum::welch_power;
use limewave_core::effects::apply_effect;
use limewave_core::spectrum_loop::{
    SpectrumAnalysis, SpectrumLoopError, SpectrumLoopRender, SpectrumLoopSettings,
    SpectrumLoopWarning, analyse_range, render_spectrum_loop,
};

use crate::app::{LimewaveApp, Session};
use crate::render_worker::{RenderKey, RenderSlot, spawn_render};
use crate::theme::colors;
use crate::ui::dialog_kit::{
    ButtonHints, Clicks, PLOT_COLUMNS, button_row, draw_loop_edges, draw_loop_fill, draw_wave,
    fixed_height, plot_canvas, wave_columns,
};

/// The FFT sizes offered besides Auto (v0.3 §5.2).
const FFT_SIZES: [usize; 9] = [256, 512, 1024, 2048, 4096, 8192, 16_384, 32_768, 65_536];

/// The setting ranges (v0.3 §5.2).
const DETUNE_CENTS: std::ops::RangeInclusive<f32> = 0.0..=50.0;
const LOOP_S: std::ops::RangeInclusive<f32> = 0.1..=30.0;
const TRANSITION_MS: std::ops::RangeInclusive<f32> = 10.0..=1000.0;

/// The preview plots: the waveform, and the spectrum under it, whose frequency axis is
/// logarithmic from 20 Hz to the Nyquist frequency and whose level axis spans 90 dB below the
/// source's loudest bin.
const WAVE_HEIGHT: f32 = 80.0;
const SPECTRUM_HEIGHT: f32 = 70.0;
const SPECTRUM_MIN_HZ: f64 = 20.0;
const SPECTRUM_RANGE_DB: f32 = 90.0;

/// The result area is as tall as the plots plus this many text lines: the summary and up to
/// three warnings.
const RESULT_LINES: f32 = 4.0;
const GRID_SPACING: f32 = 4.0;

/// The hover texts of the Preview, Play original and Stop buttons.
const BUTTON_HINTS: ButtonHints = ButtonHints {
    preview: "Play the result from the start of the range: the transition in, then the loop \
              repeating until Stop",
    original: "Play the untouched source range once",
    stop: "Stop playing",
};

/// The Make loop from spectrum… dialog's state, while it is open.
#[derive(Default)]
pub struct SpectrumDialogState {
    /// What the user asked for. Pitch lock is used only when a pitch was found.
    pub settings: SpectrumLoopSettings,
    /// The analysis of the source range at the automatic FFT size, made on a worker when the
    /// dialog opens: the range, and the pitch found.
    pub analysis: RenderSlot<Result<SpectrumAnalysis, SpectrumLoopError>>,
    /// The loop rendered for the current settings.
    pub render: RenderSlot<Result<SpectrumResult, SpectrumLoopError>>,
}

/// A rendered loop and the preview plots made from it.
pub struct SpectrumResult {
    pub render: SpectrumLoopRender,
    pub plot: SpectrumPlot,
}

/// What the preview plots draw: the new audio that replaces the range (the entry transition,
/// the loop and the exit transition), and the loop's averaged spectrum over the source's.
pub struct SpectrumPlot {
    /// The first frame drawn: the start of the range.
    pub first: usize,
    /// Frames drawn: the length of the replacement.
    pub len: usize,
    /// The channel average's minimum and maximum in each column, left to right.
    pub wave: Vec<(f32, f32)>,
    /// The loop's and the source range's spectra, in dB below the source's loudest bin, one
    /// value per column.
    pub loop_db: Vec<f32>,
    pub source_db: Vec<f32>,
}

impl SpectrumPlot {
    /// The plots for `render`, made from `audio` (the file before it) and `out` (after it).
    fn new(audio: &[Vec<f32>], out: &[Vec<f32>], render: &SpectrumLoopRender, rate: u32) -> Self {
        let frames = |a: &[Vec<f32>]| a.iter().map(Vec::len).min().unwrap_or(0);
        let first = render.step.first.min(frames(out));
        // The audio after the replacement is the original's after the step's last frame.
        let after = frames(audio).saturating_sub(render.step.last + 1);
        let len = frames(out).saturating_sub(after).saturating_sub(first);
        let n = render.analysis.fft_size;
        let (a, last) = render.analysis.range;
        let source = summed_power(audio, a, last, n);
        let looped = summed_power(out, render.edit.loop_start, render.edit.loop_end, n);
        let reference = source.iter().copied().fold(0.0, f64::max);
        let bin_hz = f64::from(rate) / n.max(1) as f64;
        let nyquist = f64::from(rate) / 2.0;
        SpectrumPlot {
            first,
            len,
            wave: wave_columns(out, first, len),
            loop_db: spectrum_columns(&looped, bin_hz, nyquist, reference),
            source_db: spectrum_columns(&source, bin_hz, nyquist, reference),
        }
    }
}

/// The Welch power of `first..=last`, summed over the channels.
fn summed_power(audio: &[Vec<f32>], first: usize, last: usize, n: usize) -> Vec<f64> {
    let mut total = vec![0.0; n / 2 + 1];
    for ch in audio {
        let p = welch_power(ch, first, last, n);
        total
            .iter_mut()
            .zip(p)
            .for_each(|(t, v)| *t += f64::from(v));
    }
    total
}

/// `power` in [`PLOT_COLUMNS`] columns on a logarithmic frequency axis from 20 Hz to
/// `nyquist`: the loudest bin in each column, in dB relative to `reference`, and no lower than
/// the plot's floor.
fn spectrum_columns(power: &[f64], bin_hz: f64, nyquist: f64, reference: f64) -> Vec<f32> {
    let floor = -SPECTRUM_RANGE_DB;
    if power.is_empty() || nyquist <= SPECTRUM_MIN_HZ || bin_hz <= 0.0 || reference <= 0.0 {
        return vec![floor; PLOT_COLUMNS];
    }
    let last = power.len() - 1;
    let (lo, hi) = (SPECTRUM_MIN_HZ.ln(), nyquist.ln());
    let hz = |c: usize| (lo + (hi - lo) * c as f64 / PLOT_COLUMNS as f64).exp();
    (0..PLOT_COLUMNS)
        .map(|c| {
            let k0 = ((hz(c) / bin_hz).round() as usize).min(last);
            let k1 = ((hz(c + 1) / bin_hz).round() as usize).clamp(k0, last);
            let p = power[k0..=k1].iter().copied().fold(0.0, f64::max);
            let db = 10.0 * (p / reference).max(1e-30).log10();
            (db as f32).max(floor)
        })
        .collect()
}

/// The source range in the selected file: the selection, ordered and clamped to the file, or
/// Start…End, as the core takes it.
fn source_range(s: &Session, selection: Option<(usize, usize)>) -> (usize, usize) {
    let e = s.edit();
    let last_frame = s.frames().saturating_sub(1);
    let (p, q) = selection.unwrap_or((e.start, e.end));
    (p.min(q).min(last_frame), p.max(q).min(last_frame))
}

/// A hash of every input to a render that the history does not cover: the settings and the
/// selection, which the range falls back from.
fn settings_hash(s: &SpectrumLoopSettings, selection: Option<(usize, usize)>) -> u64 {
    let mut h = DefaultHasher::new();
    s.fft_size.hash(&mut h);
    s.pitch_lock.hash(&mut h);
    for v in [s.detune_cents, s.loop_s, s.transition_ms] {
        v.to_bits().hash(&mut h);
    }
    s.seed.hash(&mut h);
    selection.hash(&mut h);
    h.finish()
}

/// A hash of the analysis's only input outside the history: the selection.
fn selection_hash(selection: Option<(usize, usize)>) -> u64 {
    let mut h = DefaultHasher::new();
    selection.hash(&mut h);
    h.finish()
}

/// The Make loop from spectrum… modal for `app.ui.spectrum_dialog`. It starts the analysis
/// and the renders, draws the settings and the result, and handles its buttons. Every way of
/// closing it ends a live preview.
pub fn spectrum_dialog(ctx: &egui::Context, app: &mut LimewaveApp) {
    let Some(mut d) = app.ui.spectrum_dialog.take() else {
        return;
    };
    let selection = app.ui.selection;
    let Some(s) = app.selected_session() else {
        // The file is gone: nothing to loop.
        app.end_preview();
        return;
    };
    let range = source_range(s, selection);
    let rate = s.doc.format.sample_rate;

    // The analysis, as soon as the dialog opens.
    let analysis_key = app.render_key(selection_hash(selection));
    if let Some(key) = analysis_key
        && d.analysis.wants(key, Duration::ZERO, ctx)
    {
        let (audio, e) = (s.processed.clone(), s.edit().clone());
        d.analysis.start(spawn_render(key, ctx.clone(), move || {
            analyse_range(&audio, &e, selection, rate, None)
        }));
    }
    let analysis = d.analysis.result(analysis_key).copied();
    let has_pitch = matches!(analysis, Some(Ok(a)) if a.pitch_hz.is_some());

    // The render, once the analysis has worked and the settings have paused. Pitch lock is
    // used only when a pitch was found.
    let effective = SpectrumLoopSettings {
        pitch_lock: d.settings.pitch_lock && has_pitch,
        ..d.settings
    };
    let render_key = app.render_key(settings_hash(&effective, selection));
    if let Some(Ok(_)) = analysis
        && let Some(key) = render_key
        && d.render.wants(key, app.ui.render_debounce, ctx)
    {
        let (audio, e) = (s.processed.clone(), s.edit().clone());
        d.render.start(spawn_render(key, ctx.clone(), move || {
            let render = render_spectrum_loop(&audio, &e, selection, rate, &effective)?;
            let mut out = audio.clone();
            apply_effect(&mut out, &render.step);
            let plot = SpectrumPlot::new(&audio, &out, &render, rate);
            Ok(SpectrumResult { render, plot })
        }));
    }

    let before = d.settings;
    let mut clicks = Clicks::default();
    let modal = egui::Modal::new(egui::Id::unique("spectrum_dialog")).show(ctx, |ui| {
        ui.set_width(480.0);
        ui.heading("Make loop from spectrum");
        let a = match analysis {
            None => {
                ui.weak("Analysing…");
                None
            }
            Some(Err(err)) => {
                ui.colored_label(colors::END, err.to_string());
                None
            }
            Some(Ok(a)) => {
                ui.label(header_line(&a, rate, selection.is_some()));
                Some(a)
            }
        };
        let result = d.render.result(render_key);
        if let Some(a) = a {
            ui.add_space(4.0);
            settings_grid(ui, &mut d.settings, &a, rate);
            ui.add_space(6.0);
            // The result keeps a fixed height, so the modal (centred on the screen) does not
            // move, and its buttons stay under the pointer, while a new result renders.
            let body = ui.text_style_height(&TextStyle::Body);
            let spacing = ui.spacing().item_spacing.y;
            let result_h = WAVE_HEIGHT
                + SPECTRUM_HEIGHT
                + 2.0 * spacing
                + RESULT_LINES * (body + spacing + 2.0);
            fixed_height(ui, result_h, |ui| match result {
                Some(Ok(r)) => {
                    plots(ui, Some(r));
                    ui.weak(summary_line(&r.render, &effective, rate));
                    for w in &r.render.warnings {
                        ui.colored_label(colors::LOOP, warning_text(*w));
                    }
                }
                Some(Err(err)) => {
                    plots(ui, None);
                    ui.colored_label(colors::END, err.to_string());
                }
                None => {
                    plots(ui, None);
                    ui.weak("Rendering…");
                }
            });
        }
        let ready = matches!(result, Some(Ok(_)));
        ui.add_space(8.0);
        clicks = button_row(ui, ready, &BUTTON_HINTS);
    });
    if d.settings != before {
        d.render.restart();
    }

    let close = clicks.cancel || modal.should_close();
    act(app, d, &clicks, close, render_key, range);
}

/// Carries out the buttons clicked this frame, after any settings change has restarted the
/// render. Apply applies the result rendered for `render_key` and closes the dialog; with no
/// such result (a setting changed in the same frame) it does nothing, and the dialog stays
/// open until the new result lands. `close` (Cancel, Escape) closes it. Closing always ends
/// a live preview. `range` is the source range, for Play original.
fn act(
    app: &mut LimewaveApp,
    d: SpectrumDialogState,
    clicks: &Clicks,
    close: bool,
    render_key: Option<RenderKey>,
    range: (usize, usize),
) {
    let ready = d.render.result(render_key).and_then(|r| r.as_ref().ok());
    if clicks.preview
        && let Some(r) = ready
    {
        // From the start of the range, so the transition into the loop is heard; the preview
        // then repeats the loop until Stop.
        let (step, edit) = (r.render.step.clone(), r.render.edit.clone());
        app.preview_step(&step, &edit);
        app.play_preview(step.first, true);
    }
    if clicks.original {
        app.play_original_range(range.0, range.1);
    }
    if clicks.stop {
        // The preview never leaves the loop on its own: Stop cuts it with a short fade, as the
        // toolbar's Stop does.
        app.release_all();
    }
    let applied = match ready {
        Some(r) if clicks.apply => {
            let edit = r.render.edit.clone();
            app.end_preview();
            app.apply_dialog_edit(edit);
            true
        }
        _ => false,
    };
    if applied || close {
        app.end_preview();
    } else {
        app.ui.spectrum_dialog = Some(d);
    }
}

/// "Selection 2000–5999 (90.7 ms); pitch 220.0 Hz", or "Start…End 0–7999 (181.4 ms); no pitch".
fn header_line(a: &SpectrumAnalysis, rate: u32, from_selection: bool) -> String {
    let (first, last) = a.range;
    let ms = (last + 1 - first) as f64 * 1000.0 / f64::from(rate.max(1));
    let origin = if from_selection {
        "Selection"
    } else {
        "Start…End"
    };
    let pitch = match a.pitch_hz {
        Some(hz) => format!("pitch {hz:.1} Hz"),
        None => "no pitch".to_owned(),
    };
    format!("{origin} {first}–{last} ({ms:.1} ms); {pitch}")
}

/// "Steady loop of 3.00 s, entry 45 ms; FFT 2048 (21.5 Hz bins), 5 frames", for `r` rendered
/// with `s`.
fn summary_line(r: &SpectrumLoopRender, s: &SpectrumLoopSettings, rate: u32) -> String {
    let rate_f = f64::from(rate.max(1));
    let e = &r.edit;
    let loop_s = (e.loop_end + 1 - e.loop_start) as f64 / rate_f;
    let entry_ms = e.loop_start.saturating_sub(r.step.first) as f64 * 1000.0 / rate_f;
    // As the core picks it; with Pitch lock on, a render that fell back to Plain reports no
    // pitch.
    let flavour = if !s.pitch_lock || r.analysis.pitch_hz.is_none() {
        "Plain"
    } else if s.detune_cents > 0.0 {
        "Ensemble"
    } else {
        "Steady"
    };
    let a = &r.analysis;
    format!(
        "{flavour} loop of {loop_s:.2} s, entry {entry_ms:.0} ms; FFT {} ({:.1} Hz bins), {} \
         frames",
        a.fft_size, a.bin_hz, a.frames
    )
}

/// The settings of v0.3 §5.2.
fn settings_grid(ui: &mut egui::Ui, s: &mut SpectrumLoopSettings, a: &SpectrumAnalysis, rate: u32) {
    let has_pitch = a.pitch_hz.is_some();
    egui::Grid::new("spectrum_settings")
        .num_columns(2)
        .spacing([10.0, GRID_SPACING])
        .show(ui, |ui| {
            fft_row(ui, s, a, rate);

            ui.label("");
            let mut lock = s.pitch_lock && has_pitch;
            let r = ui
                .add_enabled(has_pitch, Checkbox::new(&mut lock, "Pitch lock"))
                .on_hover_text(
                    "Build the loop from the harmonics of the pitch found. Off: the averaged \
                     spectrum with random phases, for unpitched sounds",
                )
                .on_disabled_hover_text("No pitch was found, so the plain spectrum is used");
            if r.changed() {
                s.pitch_lock = lock;
            }
            ui.end_row();

            let label = ui.label("Detune (cents)");
            ui.add_enabled(
                lock,
                DragValue::new(&mut s.detune_cents)
                    .range(DETUNE_CENTS)
                    .speed(0.1)
                    .max_decimals(1),
            )
            .labelled_by(label.id)
            .on_hover_text("0 holds each harmonic steady; more spreads it into a moving ensemble")
            .on_disabled_hover_text("Needs Pitch lock");
            ui.end_row();

            let label = ui.label("Loop length (s)");
            ui.add(
                DragValue::new(&mut s.loop_s)
                    .range(LOOP_S)
                    .speed(0.01)
                    .max_decimals(2),
            )
            .labelled_by(label.id)
            .on_hover_text("The length of the new loop");
            ui.end_row();

            let label = ui.label("Transition (ms)");
            ui.add(
                DragValue::new(&mut s.transition_ms)
                    .range(TRANSITION_MS)
                    .speed(1.0)
                    .max_decimals(0),
            )
            .labelled_by(label.id)
            .on_hover_text(
                "How long the original takes to turn into the loop, and back after it; at most \
                 half the range",
            );
            ui.end_row();

            ui.label("");
            let steady = lock && s.detune_cents == 0.0;
            if ui
                .add_enabled(!steady, Button::new("New variation"))
                .on_hover_text("Pick new random phases")
                .on_disabled_hover_text("A steady pitched loop has no random phases")
                .clicked()
            {
                s.seed = s.seed.wrapping_add(1);
            }
            ui.end_row();
        });
}

/// The FFT size combo box: Auto and the powers of two, each with its bin width. Sizes longer
/// than the range are greyed out, as the core would shorten them to fit.
fn fft_row(ui: &mut egui::Ui, s: &mut SpectrumLoopSettings, a: &SpectrumAnalysis, rate: u32) {
    let size_text = |n: usize| format!("{n} ({:.1} Hz)", rate as f32 / n as f32);
    let auto_text = format!("Auto: {}", size_text(a.fft_size));
    let range_len = a.range.1 + 1 - a.range.0;
    let label = ui.label("FFT size");
    let selected = s.fft_size.map_or_else(|| auto_text.clone(), size_text);
    ComboBox::from_id_salt("spectrum_fft_size")
        .selected_text(selected)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut s.fft_size, None, auto_text);
            for n in FFT_SIZES {
                ui.add_enabled_ui(n <= range_len, |ui| {
                    ui.selectable_value(&mut s.fft_size, Some(n), size_text(n))
                        .on_disabled_hover_text("Longer than the range");
                });
            }
        })
        .response
        .labelled_by(label.id)
        .on_hover_text(
            "The analysis size, with its bin width. Larger sizes give sharper harmonics but \
             average fewer frames",
        );
    ui.end_row();
}

/// The warning lines of v0.3 §5.2, short enough for one line each.
fn warning_text(w: SpectrumLoopWarning) -> &'static str {
    match w {
        SpectrumLoopWarning::FewFrames => {
            "Fewer than 3 frames fit in the range; the average is rough. Try a smaller FFT."
        }
        SpectrumLoopWarning::LoopTooShortForDetune => {
            "The loop is too short to resolve the detune; make it longer."
        }
        SpectrumLoopWarning::NoPitch => "No usable pitch; the loop uses the plain spectrum.",
    }
}

/// The preview plots: the replacement's waveform with the loop marked, and under it the
/// loop's spectrum over a faint copy of the source's. `None` draws them empty, while there
/// is no result.
fn plots(ui: &mut egui::Ui, shown: Option<&SpectrumResult>) {
    let (rect, painter) = plot_canvas(ui, WAVE_HEIGHT, "Loop preview");
    if let Some(r) = shown.filter(|r| r.plot.len > 0) {
        let p = &r.plot;
        let x_of = |frame: usize| {
            let t = frame.saturating_sub(p.first) as f32 / p.len as f32;
            rect.left() + t.clamp(0.0, 1.0) * rect.width()
        };
        let e = &r.render.edit;
        let (a, b) = (x_of(e.loop_start), x_of(e.loop_end + 1));
        draw_loop_fill(&painter, rect, a, b);
        draw_wave(&painter, rect, &p.wave);
        draw_loop_edges(&painter, rect, a, b);
    }
    let (rect, painter) = plot_canvas(ui, SPECTRUM_HEIGHT, "Loop spectrum");
    if let Some(r) = shown {
        let faint = Color32::from_rgba_unmultiplied(
            colors::WAVE.r(),
            colors::WAVE.g(),
            colors::WAVE.b(),
            90,
        );
        spectrum_line(&painter, rect, &r.plot.source_db, Stroke::new(1.0, faint));
        spectrum_line(
            &painter,
            rect,
            &r.plot.loop_db,
            Stroke::new(1.25, colors::LIME),
        );
    }
}

/// Draws `db` (one value per column, 0 at the top, the floor at the bottom) across `rect`.
fn spectrum_line(painter: &egui::Painter, rect: egui::Rect, db: &[f32], stroke: Stroke) {
    if db.is_empty() {
        return;
    }
    let column_w = rect.width() / db.len() as f32;
    let points = db
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let t = (-v / SPECTRUM_RANGE_DB).clamp(0.0, 1.0);
            pos2(
                rect.left() + (i as f32 + 0.5) * column_w,
                rect.top() + 2.0 + t * (rect.height() - 4.0),
            )
        })
        .collect();
    painter.add(Shape::line(points, stroke));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::app::FileEntry;
    use limewave_core::document::load_wav_bytes;
    use limewave_core::testwav::{sine, wav_bytes};
    use limewave_core::wav::SampleFormat;

    /// An app showing a steady 220 Hz tone of 8000 frames.
    fn tone_app() -> LimewaveApp {
        let bytes = wav_bytes(
            &[sine(220.0, 44100, 8000, 0.5)],
            44100,
            SampleFormat::Int { bits: 16 },
            Vec::new(),
        );
        let doc = load_wav_bytes(PathBuf::from("t.wav"), &bytes).unwrap();
        let mut app = LimewaveApp::for_test();
        app.files = vec![FileEntry::Loaded(Box::new(Session::new(doc)))];
        app.select(0);
        app
    }

    /// A dialog holding a finished render for its settings, and that render's key.
    fn dialog_with_result(app: &LimewaveApp) -> (SpectrumDialogState, Option<RenderKey>) {
        let mut d = SpectrumDialogState::default();
        d.settings.loop_s = 0.2;
        let key = app.render_key(settings_hash(&d.settings, None));
        let s = app.selected_session().unwrap();
        let render =
            render_spectrum_loop(&s.processed, s.edit(), None, 44100, &d.settings).unwrap();
        let mut out = s.processed.clone();
        apply_effect(&mut out, &render.step);
        let plot = SpectrumPlot::new(&s.processed, &out, &render, 44100);
        let result = Ok(SpectrumResult { render, plot });
        d.render
            .start(spawn_render(key.unwrap(), egui::Context::default(), || {
                result
            }));
        d.render.finish(key, true);
        assert!(d.render.result(key).is_some());
        (d, key)
    }

    #[test]
    fn apply_closes_the_dialog_only_when_it_applies() {
        let apply = Clicks {
            apply: true,
            ..Clicks::default()
        };
        // A setting committed in the same frame as the click restarts the render: nothing to
        // apply, so the dialog stays open.
        let mut app = tone_app();
        let (mut d, key) = dialog_with_result(&app);
        d.settings.seed = 2;
        d.render.restart();
        act(&mut app, d, &apply, false, key, (0, 7999));
        assert!(app.ui.spectrum_dialog.is_some(), "still open");
        let s = app.selected_session().unwrap();
        assert_eq!(
            (s.history.cursor(), s.frames()),
            (0, 8000),
            "nothing applied"
        );

        // With the result, Apply applies it and closes.
        app.ui.spectrum_dialog = None;
        let (d, key) = dialog_with_result(&app);
        act(&mut app, d, &apply, false, key, (0, 7999));
        assert!(app.ui.spectrum_dialog.is_none(), "closed");
        let s = app.selected_session().unwrap();
        assert_eq!(s.history.cursor(), 1, "applied");
    }

    #[test]
    fn settings_hash_covers_every_setting_and_the_selection() {
        let s = SpectrumLoopSettings::default();
        let base = settings_hash(&s, None);
        assert_eq!(base, settings_hash(&s, None), "stable");
        let changed = [
            SpectrumLoopSettings {
                fft_size: Some(1024),
                ..s
            },
            SpectrumLoopSettings {
                pitch_lock: !s.pitch_lock,
                ..s
            },
            SpectrumLoopSettings {
                detune_cents: 10.0,
                ..s
            },
            SpectrumLoopSettings { loop_s: 1.0, ..s },
            SpectrumLoopSettings {
                transition_ms: 50.0,
                ..s
            },
            SpectrumLoopSettings { seed: 9, ..s },
        ];
        for c in changed {
            assert_ne!(settings_hash(&c, None), base, "{c:?}");
        }
        assert_ne!(
            settings_hash(&s, Some((0, 10))),
            base,
            "the selection counts"
        );
    }

    #[test]
    fn spectrum_columns_put_a_tone_in_its_column() {
        // 64 bins of 100 Hz: a peak at 1 kHz (bin 10), Nyquist 6.4 kHz.
        let mut power = vec![1e-12; 65];
        power[10] = 1.0;
        let db = spectrum_columns(&power, 100.0, 6400.0, 1.0);
        assert_eq!(db.len(), PLOT_COLUMNS);
        let loudest = db
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        // 1 kHz on a log axis from 20 Hz to 6.4 kHz.
        let expect = (1000f64 / 20.0).ln() / (6400f64 / 20.0).ln() * PLOT_COLUMNS as f64;
        assert!(
            (loudest as f64 - expect).abs() < 6.0,
            "{loudest} vs {expect}"
        );
        assert_eq!(db[loudest], 0.0);
        assert_eq!(db[0], -SPECTRUM_RANGE_DB, "the floor");
        // Nothing to show: the floor everywhere.
        assert!(
            spectrum_columns(&[], 100.0, 6400.0, 1.0)
                .iter()
                .all(|&v| v == -SPECTRUM_RANGE_DB)
        );
    }

    #[test]
    fn plot_covers_the_replacement() {
        let app = tone_app();
        let s = app.selected_session().unwrap();
        let settings = SpectrumLoopSettings {
            loop_s: 0.2,
            ..SpectrumLoopSettings::default()
        };
        let sel = Some((2000, 5999));
        let render = render_spectrum_loop(&s.processed, s.edit(), sel, 44100, &settings).unwrap();
        let mut out = s.processed.clone();
        apply_effect(&mut out, &render.step);
        let p = SpectrumPlot::new(&s.processed, &out, &render, 44100);
        // Entry and exit of 2000 frames each (half the range), and the loop.
        assert_eq!((p.first, p.len), (2000, 2000 + 8820 + 2000));
        assert_eq!(p.wave.len(), PLOT_COLUMNS);
        // The loop's spectrum follows the source's: both peak in the same column.
        let peak = |db: &[f32]| {
            db.iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap()
                .0
        };
        assert!(peak(&p.loop_db).abs_diff(peak(&p.source_db)) <= 2);
    }
}
