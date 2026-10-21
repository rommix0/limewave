//! The Generate tail… dialog (v0.3 §7.1). It analyses the audio before the cut (the End
//! marker) and renders the tail on worker threads, so the window stays responsive. Changing a
//! setting renders again after a pause; Preview plays the result without touching the
//! session, and Apply records it as one history entry.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::RangeInclusive;
use std::time::Duration;

use eframe::egui::{
    self, Button, Color32, DragValue, RichText, Role, Sense, Stroke, TextStyle, WidgetInfo, pos2,
    vec2,
};
use limewave_core::edit::{EditState, LoopMode};
use limewave_core::effects::apply_effect;
use limewave_core::tail::{
    TailAnalysis, TailError, TailMethod, TailMode, TailRender, TailSettings, TailWarning, analyse,
    render_tail,
};

use crate::app::{LimewaveApp, Session};
use crate::render_worker::{RenderKey, RenderSlot, spawn_render};
use crate::theme::colors;

/// The preview, and Play original, start this many seconds before the cut (v0.3 §7.1).
const PREVIEW_LEAD_S: f64 = 1.0;

/// The ranges of the settings, in seconds (v0.3 §3). The body range is the core's.
const DECAY_T60_S: RangeInclusive<f32> = 0.01..=60.0;
const BODY_S: RangeInclusive<f32> = 0.1..=30.0;
const RELEASE_T60_S: RangeInclusive<f32> = 0.0..=60.0;

/// The preview plot: its height and how many min/max columns the worker makes for it.
const PLOT_HEIGHT: f32 = 110.0;
const PLOT_COLUMNS: usize = 480;
const PLOT_BACKGROUND: Color32 = Color32::from_rgb(0x15, 0x17, 0x1d);

/// The settings area is as tall as the Sustain settings (four rows) in either mode, and the
/// result area as the plot plus this many text lines: the summary and up to four warnings.
const SETTINGS_ROWS: f32 = 4.0;
const RESULT_LINES: f32 = 5.0;
const GRID_SPACING: f32 = 4.0;

/// The Generate tail… dialog's state, while it is open.
#[derive(Default)]
pub struct TailDialogState {
    /// What the user asked for. The time fields stay `None` (the fitted value) until edited.
    pub settings: TailSettings,
    /// The analysis of the source region, made on a worker when the dialog opens.
    pub analysis: RenderSlot<Result<TailAnalysis, TailError>>,
    /// The tail rendered for the current settings.
    pub render: RenderSlot<Result<TailResult, TailError>>,
    /// Whether the suggested mode and method have been taken from the analysis yet.
    suggested_taken: bool,
}

/// A rendered tail and the preview plot made from it.
pub struct TailResult {
    pub render: TailRender,
    pub plot: TailPlot,
}

/// What the preview plot draws: the last second before the cut, then the tail.
pub struct TailPlot {
    /// The first frame drawn: 1 s before the cut, or Start.
    pub first: usize,
    /// Frames drawn, from `first` to the new end of the file.
    pub len: usize,
    /// The channel average's minimum and maximum in each column, left to right.
    pub columns: Vec<(f32, f32)>,
}

impl TailPlot {
    /// The plot of `audio` (the file with the tail applied) from `first` to its end.
    fn new(audio: &[Vec<f32>], first: usize) -> TailPlot {
        let frames = audio.iter().map(Vec::len).min().unwrap_or(0);
        let first = first.min(frames);
        let len = frames - first;
        let count = PLOT_COLUMNS.min(len);
        let channels = audio.len().max(1) as f32;
        let columns = (0..count)
            .map(|c| {
                let a = first + c * len / count;
                let b = (first + (c + 1) * len / count).max(a + 1);
                (a..b).fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), i| {
                    let v = audio.iter().map(|ch| ch[i]).sum::<f32>() / channels;
                    (lo.min(v), hi.max(v))
                })
            })
            .collect();
        TailPlot {
            first,
            len,
            columns,
        }
    }
}

/// Where the tail goes in the selected file: the cut (the first new frame, just after End)
/// and the frame the previews start from, 1 s before the cut but not before Start.
fn cut_and_from(s: &Session) -> (usize, usize) {
    let e = s.edit();
    let cut = e.end.min(s.frames().saturating_sub(1)) + 1;
    let lead = (PREVIEW_LEAD_S * f64::from(s.doc.format.sample_rate)) as usize;
    let from = cut.saturating_sub(lead).max(e.start.min(cut - 1));
    (cut, from)
}

/// A hash of every input to a render that the history does not cover: the settings and the
/// selection, which the source region falls back to.
fn settings_hash(s: &TailSettings, selection: Option<(usize, usize)>) -> u64 {
    let mut h = DefaultHasher::new();
    std::mem::discriminant(&s.mode).hash(&mut h);
    std::mem::discriminant(&s.method).hash(&mut h);
    for v in [s.decay_t60, s.release_t60] {
        v.map(f32::to_bits).hash(&mut h);
    }
    for v in [s.body_s, s.darken] {
        v.to_bits().hash(&mut h);
    }
    s.loop_on.hash(&mut h);
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

/// What the buttons asked for this frame.
#[derive(Default)]
struct Clicks {
    preview: bool,
    original: bool,
    stop: bool,
    apply: bool,
    cancel: bool,
}

/// The Generate tail… modal for `app.ui.tail_dialog`. It starts the analysis and the
/// renders, draws the settings and the result, and handles its buttons. Every way of
/// closing it ends a live preview.
pub fn tail_dialog(ctx: &egui::Context, app: &mut LimewaveApp) {
    let Some(mut d) = app.ui.tail_dialog.take() else {
        return;
    };
    let selection = app.ui.selection;
    let Some(s) = app.selected_session() else {
        // The file is gone: nothing to generate a tail for.
        app.end_preview();
        return;
    };
    let (cut, from) = cut_and_from(s);
    let rate = s.doc.format.sample_rate;

    // The analysis, as soon as the dialog opens.
    let analysis_key = app.render_key(selection_hash(selection));
    if let Some(key) = analysis_key
        && d.analysis.wants(key, Duration::ZERO, ctx)
    {
        let (audio, e) = (s.processed.clone(), s.edit().clone());
        d.analysis.start(spawn_render(key, ctx.clone(), move || {
            analyse(&audio, &e, selection, rate)
        }));
    }
    let analysis = d.analysis.result(analysis_key).copied();
    if let Some(Ok(a)) = analysis
        && !d.suggested_taken
    {
        d.settings.mode = a.suggested_mode;
        d.settings.method = a.suggested_method;
        d.suggested_taken = true;
    }

    // The render, once the analysis has worked and the settings have paused.
    let render_key = app.render_key(settings_hash(&d.settings, selection));
    if let Some(Ok(_)) = analysis
        && let Some(key) = render_key
        && d.render.wants(key, app.ui.render_debounce, ctx)
    {
        let (audio, e) = (s.processed.clone(), s.edit().clone());
        let settings = d.settings;
        d.render.start(spawn_render(key, ctx.clone(), move || {
            let render = render_tail(&audio, &e, selection, rate, &settings)?;
            let mut out = audio;
            apply_effect(&mut out, &render.step);
            let plot = TailPlot::new(&out, from);
            Ok(TailResult { render, plot })
        }));
    }

    let before = d.settings;
    let mut clicks = Clicks::default();
    let modal = egui::Modal::new(egui::Id::unique("tail_dialog")).show(ctx, |ui| {
        ui.set_width(480.0);
        ui.heading("Generate tail");
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
                ui.label(analysis_line(&a, rate, selection.is_some()));
                Some(a)
            }
        };
        let result = d.render.result(render_key);
        if let Some(a) = a {
            ui.add_space(4.0);
            mode_and_method(ui, &mut d.settings, &a);
            ui.add_space(4.0);
            // The settings and the result keep a fixed height, so the modal (centred on the
            // screen) does not move, and its buttons stay under the pointer, while a new
            // result renders or the mode changes.
            let body = ui.text_style_height(&TextStyle::Body);
            let row = ui.spacing().interact_size.y.max(body);
            let spacing = ui.spacing().item_spacing.y;
            let settings_h = SETTINGS_ROWS * (row + GRID_SPACING + 2.0) + spacing;
            fixed_height(ui, settings_h, |ui| settings_grid(ui, &mut d.settings, &a));
            ui.add_space(6.0);
            let result_h = PLOT_HEIGHT + spacing + RESULT_LINES * (body + spacing + 2.0);
            let mode = d.settings.mode;
            fixed_height(ui, result_h, |ui| match result {
                Some(Ok(r)) => {
                    plot(ui, Some((&r.plot, &r.render.edit, mode)), cut);
                    let added = (r.render.edit.end + 1).saturating_sub(cut);
                    ui.weak(format!(
                        "The tail adds {:.2} s.",
                        added as f64 / f64::from(rate.max(1))
                    ));
                    for w in &r.render.warnings {
                        ui.colored_label(colors::LOOP, warning_text(*w));
                    }
                }
                Some(Err(err)) => {
                    plot(ui, None, cut);
                    ui.colored_label(colors::END, err.to_string());
                }
                None => {
                    plot(ui, None, cut);
                    ui.weak("Rendering…");
                }
            });
        }
        let ready = matches!(result, Some(Ok(_)));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            clicks.preview = ui
                .add_enabled(ready, Button::new("Preview"))
                .on_hover_text(
                    "Play the result from 1 s before the cut. Stop leaves the loop and plays \
                     the generated release",
                )
                .clicked();
            clicks.original = ui
                .button("Play original")
                .on_hover_text("Play the untouched audio from the same point")
                .clicked();
            clicks.stop = ui
                .button("Stop")
                .on_hover_text("Release the note: a held preview plays the generated release")
                .clicked();
            ui.separator();
            clicks.apply = ui.add_enabled(ready, Button::new("Apply")).clicked();
            clicks.cancel = ui.button("Cancel").clicked();
        });
    });
    if d.settings != before {
        d.render.restart();
    }

    let close = clicks.cancel || modal.should_close();
    act(app, d, &clicks, close, render_key, from);
}

/// Carries out the buttons clicked this frame, after any settings change has restarted the
/// render. Apply applies the result rendered for `render_key` and closes the dialog; with no
/// such result (a setting changed in the same frame) it does nothing, and the dialog stays
/// open until the new result lands. `close` (Cancel, Escape) closes it. Closing always ends
/// a live preview.
fn act(
    app: &mut LimewaveApp,
    d: TailDialogState,
    clicks: &Clicks,
    close: bool,
    render_key: Option<RenderKey>,
    from: usize,
) {
    let ready = d.render.result(render_key).and_then(|r| r.as_ref().ok());
    if clicks.preview
        && let Some(r) = ready
    {
        // Published only now: a voice holding on an earlier preview fades as the new one
        // starts on the same key.
        let (step, edit) = (r.render.step.clone(), r.render.edit.clone());
        app.preview_step(&step, &edit);
        app.play_preview(from);
    }
    if clicks.original {
        app.play_original(from);
    }
    if clicks.stop {
        app.release_root();
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
        app.ui.tail_dialog = Some(d);
    }
}

/// "Source 1600..=7999 (145 ms, automatic); tonal, 220.0 Hz; steady".
fn analysis_line(a: &TailAnalysis, rate: u32, from_selection: bool) -> String {
    let (first, last) = a.source;
    let ms = (last + 1 - first) as f64 * 1000.0 / f64::from(rate.max(1));
    let origin = if from_selection {
        "selection"
    } else {
        "automatic"
    };
    let character = match a.character.period {
        Some(p) if p > 0.0 => format!("tonal, {:.1} Hz", rate as f32 / p),
        _ => "noisy".to_owned(),
    };
    let decay = match a.fit.t60 {
        Some(t) if !a.fit.steady => format!("decay {t:.1} s"),
        _ => "steady".to_owned(),
    };
    format!("Source {first}..={last} ({ms:.0} ms, {origin}); {character}; {decay}")
}

/// The Mode and Method radios, each with a "Suggested" tag after the analysis's pick.
fn mode_and_method(ui: &mut egui::Ui, s: &mut TailSettings, a: &TailAnalysis) {
    ui.horizontal(|ui| {
        ui.label("Mode");
        for (mode, name, hint) in [
            (
                TailMode::Decay,
                "Decay",
                "Continue the sound and let it fade to silence",
            ),
            (
                TailMode::Sustain,
                "Sustain",
                "Hold the sound steady with a seamless loop, then an optional release",
            ),
        ] {
            ui.radio_value(&mut s.mode, mode, name).on_hover_text(hint);
            if a.suggested_mode == mode {
                suggested_tag(ui);
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("Method");
        for (method, name, hint) in [
            (
                TailMethod::Smooth,
                "Smooth",
                "Freeze the spectrum at the cut and let each frequency fade at its own rate",
            ),
            (
                TailMethod::Harmonic,
                "Harmonic",
                "Rebuild the sound from its harmonics, plus a quiet noise layer",
            ),
            (
                TailMethod::Granular,
                "Granular",
                "Stitch short pieces of the sample's own audio together",
            ),
        ] {
            ui.radio_value(&mut s.method, method, name)
                .on_hover_text(hint);
            if a.suggested_method == method {
                suggested_tag(ui);
            }
        }
    });
}

fn suggested_tag(ui: &mut egui::Ui) {
    ui.label(RichText::new("Suggested").small().color(colors::LIME))
        .on_hover_text("What the analysis of the source points to");
}

/// The settings of the chosen mode (v0.3 §3).
fn settings_grid(ui: &mut egui::Ui, s: &mut TailSettings, a: &TailAnalysis) {
    egui::Grid::new("tail_settings")
        .num_columns(2)
        .spacing([10.0, GRID_SPACING])
        .show(ui, |ui| match s.mode {
            TailMode::Decay => {
                fitted_row(
                    ui,
                    "Decay time (s)",
                    &mut s.decay_t60,
                    a.default_decay_t60(),
                    DECAY_T60_S,
                    "Seconds to fall 60 dB",
                );
                darken_row(ui, &mut s.darken, "Make the highs die away faster");
            }
            TailMode::Sustain => {
                let label = ui.label("Body length (s)");
                ui.add(
                    DragValue::new(&mut s.body_s)
                        .range(BODY_S)
                        .speed(0.01)
                        .max_decimals(2),
                )
                .labelled_by(label.id)
                .on_hover_text("Steady audio at the level of the cut");
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut s.loop_on, "Loop")
                    .on_hover_text("Set loop 0 to the body: Forward, no crossfade");
                ui.end_row();
                fitted_row(
                    ui,
                    "Release (s)",
                    &mut s.release_t60,
                    a.default_release_t60(),
                    RELEASE_T60_S,
                    "Seconds for the release after the body to fall 60 dB; 0 for none",
                );
                darken_row(
                    ui,
                    &mut s.darken,
                    "Make the highs of the release die away faster",
                );
            }
        });
}

/// A time in seconds that shows `fitted` until it is edited; `value` stays `None` until then.
fn fitted_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<f32>,
    fitted: f32,
    range: RangeInclusive<f32>,
    hint: &str,
) {
    let label = ui.label(label);
    ui.horizontal(|ui| {
        let mut v = value.unwrap_or(fitted);
        let r = ui
            .add(
                DragValue::new(&mut v)
                    .range(range)
                    .speed(0.01)
                    .max_decimals(2),
            )
            .labelled_by(label.id)
            .on_hover_text(hint);
        if r.changed() {
            *value = Some(v);
        }
        if value.is_none() {
            ui.weak("fitted");
        }
    });
    ui.end_row();
}

/// Darken, stored 0–1 and shown in percent.
fn darken_row(ui: &mut egui::Ui, darken: &mut f32, hint: &str) {
    let label = ui.label("Darken (%)");
    let mut pct = *darken * 100.0;
    let r = ui
        .add(
            DragValue::new(&mut pct)
                .range(0.0..=100.0)
                .speed(0.5)
                .max_decimals(0),
        )
        .labelled_by(label.id)
        .on_hover_text(hint);
    if r.changed() {
        *darken = pct / 100.0;
    }
    ui.end_row();
}

/// Runs `add`, then pads below it to `height`, so what follows stays put when the content
/// changes (content taller than `height` simply grows).
fn fixed_height(ui: &mut egui::Ui, height: f32, add: impl FnOnce(&mut egui::Ui)) {
    let top = ui.cursor().top();
    add(ui);
    let used = ui.cursor().top() - top;
    if used < height {
        ui.add_space(height - used);
    }
}

/// The warning lines of v0.3 §7.1, short enough for one line each.
fn warning_text(w: TailWarning) -> &'static str {
    match w {
        TailWarning::LoopCrossfadeFallback => "No seamless loop fitted; the loop uses a crossfade.",
        TailWarning::OverlapShortened => "The join was shortened to stay clear of the loop.",
        TailWarning::ShortSource => "The source region is short; repetition may be audible.",
        TailWarning::NoisySourceForMethod => {
            "This method suits pitched sounds; the source is noisy."
        }
    }
}

/// The preview plot: the last second before the cut, then the tail (tinted), with the cut
/// marked and, in Sustain, the loop. `None` draws the empty plot, while there is no result.
fn plot(ui: &mut egui::Ui, shown: Option<(&TailPlot, &EditState, TailMode)>, cut: usize) {
    let width = ui.available_width().max(1.0);
    let (rect, response) = ui.allocate_exact_size(vec2(width, PLOT_HEIGHT), Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Canvas, true, "Tail preview"));
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, PLOT_BACKGROUND);
    let Some((p, edit, mode)) = shown else {
        return;
    };
    if p.len == 0 || p.columns.is_empty() {
        return;
    }
    let x_of = |frame: usize| {
        let t = frame.saturating_sub(p.first) as f32 / p.len as f32;
        rect.left() + t.clamp(0.0, 1.0) * rect.width()
    };
    let span = |a: f32, b: f32| egui::Rect::from_x_y_ranges(a..=b, rect.y_range());
    painter.rect_filled(span(x_of(cut), rect.right()), 0.0, colors::generated_tint());
    let looped = mode == TailMode::Sustain && edit.loop_mode != LoopMode::Off;
    if looped {
        let (a, b) = (x_of(edit.loop_start), x_of(edit.loop_end + 1));
        let fill = Color32::from_rgba_unmultiplied(
            colors::LOOP.r(),
            colors::LOOP.g(),
            colors::LOOP.b(),
            28,
        );
        painter.rect_filled(span(a, b), 0.0, fill);
    }
    let mid = rect.center().y;
    let half = rect.height() * 0.45;
    let column_w = rect.width() / p.columns.len() as f32;
    let wave = Stroke::new(column_w.clamp(1.0, 2.0), colors::WAVE);
    for (i, &(lo, hi)) in p.columns.iter().enumerate() {
        let x = rect.left() + (i as f32 + 0.5) * column_w;
        let (top, bottom) = (
            mid - hi.clamp(-1.0, 1.0) * half,
            mid - lo.clamp(-1.0, 1.0) * half,
        );
        painter.line_segment([pos2(x, top), pos2(x, bottom.max(top + 1.0))], wave);
    }
    if looped {
        let stroke = Stroke::new(1.5, colors::LOOP);
        for x in [x_of(edit.loop_start), x_of(edit.loop_end + 1)] {
            painter.vline(x, rect.y_range(), stroke);
        }
    }
    painter.vline(x_of(cut), rect.y_range(), Stroke::new(1.5, colors::END));
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
    fn dialog_with_result(app: &LimewaveApp) -> (TailDialogState, Option<RenderKey>) {
        let mut d = TailDialogState::default();
        let key = app.render_key(settings_hash(&d.settings, None));
        let s = app.selected_session().unwrap();
        let render = render_tail(&s.processed, s.edit(), None, 44100, &d.settings).unwrap();
        let plot = TailPlot::new(&s.processed, 0);
        let result = Ok(TailResult { render, plot });
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
        // A setting committed in the same frame as the click restarts the render, so there
        // is nothing to apply: the dialog stays open, and Apply waits for the new result.
        let mut app = tone_app();
        let (mut d, key) = dialog_with_result(&app);
        d.settings.darken = 0.5;
        d.render.restart();
        act(&mut app, d, &apply, false, key, 0);
        assert!(app.ui.tail_dialog.is_some(), "still open");
        let s = app.selected_session().unwrap();
        assert_eq!(
            (s.history.cursor(), s.frames()),
            (0, 8000),
            "nothing applied"
        );

        // With the result, Apply applies it and closes. (Each frame takes the dialog out
        // first, as `tail_dialog` does.)
        app.ui.tail_dialog = None;
        let (d, key) = dialog_with_result(&app);
        act(&mut app, d, &apply, false, key, 0);
        assert!(app.ui.tail_dialog.is_none(), "closed");
        let s = app.selected_session().unwrap();
        assert_eq!(s.history.cursor(), 1, "applied");
        assert!(s.frames() > 8000);
    }

    #[test]
    fn settings_hash_covers_every_setting_and_the_selection() {
        let s = TailSettings::default();
        let base = settings_hash(&s, None);
        assert_eq!(base, settings_hash(&s, None), "stable");
        let changed = [
            TailSettings {
                mode: TailMode::Sustain,
                ..s
            },
            TailSettings {
                method: TailMethod::Smooth,
                ..s
            },
            TailSettings {
                decay_t60: Some(1.0),
                ..s
            },
            TailSettings { body_s: 3.0, ..s },
            TailSettings {
                loop_on: !s.loop_on,
                ..s
            },
            TailSettings {
                release_t60: Some(0.0),
                ..s
            },
            TailSettings { darken: 0.5, ..s },
            TailSettings { seed: 9, ..s },
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
    fn plot_columns_cover_first_to_the_end() {
        let audio = vec![(0..1000).map(|i| i as f32 / 1000.0).collect::<Vec<f32>>()];
        let p = TailPlot::new(&audio, 200);
        assert_eq!((p.first, p.len), (200, 800));
        assert_eq!(p.columns.len(), PLOT_COLUMNS);
        assert_eq!(p.columns[0].0, 0.2);
        assert!((p.columns.last().unwrap().1 - 0.999).abs() < 1e-6);
        // Fewer frames than columns: one column per frame; none past the end.
        assert_eq!(TailPlot::new(&audio, 990).columns.len(), 10);
        assert!(TailPlot::new(&audio, 5000).columns.is_empty());
    }
}
