//! What the render dialogs (Generate tail…, Make loop from spectrum…) share: the fixed-height
//! areas that keep a modal still while it renders, the preview plot's drawing, and the row of
//! buttons.

use eframe::egui::{
    self, Button, Color32, Painter, Rect, Role, Sense, Stroke, WidgetInfo, pos2, vec2,
};

use crate::theme::colors;

/// How many min/max columns the worker makes for a waveform plot.
pub const PLOT_COLUMNS: usize = 480;
pub const PLOT_BACKGROUND: Color32 = Color32::from_rgb(0x15, 0x17, 0x1d);

/// Runs `add`, then pads below it to `height`, so what follows stays put when the content
/// changes (content taller than `height` simply grows).
pub fn fixed_height(ui: &mut egui::Ui, height: f32, add: impl FnOnce(&mut egui::Ui)) {
    let top = ui.cursor().top();
    add(ui);
    let used = ui.cursor().top() - top;
    if used < height {
        ui.add_space(height - used);
    }
}

/// The channel average's minimum and maximum in each of up to [`PLOT_COLUMNS`] columns over
/// `len` frames of `audio` from `first`, left to right; one column per frame when there are
/// fewer frames. `first` and `len` must lie inside every channel.
pub fn wave_columns(audio: &[Vec<f32>], first: usize, len: usize) -> Vec<(f32, f32)> {
    let count = PLOT_COLUMNS.min(len);
    let channels = audio.len().max(1) as f32;
    (0..count)
        .map(|c| {
            let a = first + c * len / count;
            let b = (first + (c + 1) * len / count).max(a + 1);
            (a..b).fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), i| {
                let v = audio.iter().map(|ch| ch[i]).sum::<f32>() / channels;
                (lo.min(v), hi.max(v))
            })
        })
        .collect()
}

/// Allocates a plot of the full width and `height`, named `name` for accessibility, and
/// paints its background. Hands back its rectangle and a painter clipped to it.
pub fn plot_canvas(ui: &mut egui::Ui, height: f32, name: &str) -> (Rect, Painter) {
    let width = ui.available_width().max(1.0);
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Canvas, true, name));
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, PLOT_BACKGROUND);
    (rect, painter)
}

/// Draws waveform `columns` (from [`wave_columns`]) across `rect`.
pub fn draw_wave(painter: &Painter, rect: Rect, columns: &[(f32, f32)]) {
    if columns.is_empty() {
        return;
    }
    let mid = rect.center().y;
    let half = rect.height() * 0.45;
    let column_w = rect.width() / columns.len() as f32;
    let wave = Stroke::new(column_w.clamp(1.0, 2.0), colors::WAVE);
    for (i, &(lo, hi)) in columns.iter().enumerate() {
        let x = rect.left() + (i as f32 + 0.5) * column_w;
        let (top, bottom) = (
            mid - hi.clamp(-1.0, 1.0) * half,
            mid - lo.clamp(-1.0, 1.0) * half,
        );
        painter.line_segment([pos2(x, top), pos2(x, bottom.max(top + 1.0))], wave);
    }
}

/// Shades the loop between `a` and `b` (x positions); drawn under the waveform.
pub fn draw_loop_fill(painter: &Painter, rect: Rect, a: f32, b: f32) {
    let fill =
        Color32::from_rgba_unmultiplied(colors::LOOP.r(), colors::LOOP.g(), colors::LOOP.b(), 28);
    painter.rect_filled(span(rect, a, b), 0.0, fill);
}

/// Draws the loop's two edges at `a` and `b` (x positions); drawn over the waveform.
pub fn draw_loop_edges(painter: &Painter, rect: Rect, a: f32, b: f32) {
    let stroke = Stroke::new(1.5, colors::LOOP);
    for x in [a, b] {
        painter.vline(x, rect.y_range(), stroke);
    }
}

/// The full-height strip of `rect` from `a` to `b`.
pub fn span(rect: Rect, a: f32, b: f32) -> Rect {
    Rect::from_x_y_ranges(a..=b, rect.y_range())
}

/// What the buttons asked for this frame.
#[derive(Default)]
pub struct Clicks {
    pub preview: bool,
    pub original: bool,
    pub stop: bool,
    pub apply: bool,
    pub cancel: bool,
}

/// The hover texts of the Preview, Play original and Stop buttons.
pub struct ButtonHints {
    pub preview: &'static str,
    pub original: &'static str,
    pub stop: &'static str,
}

/// The row Preview, Play original, Stop | Apply, Cancel. Preview and Apply need a result
/// (`ready`).
pub fn button_row(ui: &mut egui::Ui, ready: bool, hints: &ButtonHints) -> Clicks {
    let mut clicks = Clicks::default();
    ui.horizontal(|ui| {
        clicks.preview = ui
            .add_enabled(ready, Button::new("Preview"))
            .on_hover_text(hints.preview)
            .clicked();
        clicks.original = ui
            .button("Play original")
            .on_hover_text(hints.original)
            .clicked();
        clicks.stop = ui.button("Stop").on_hover_text(hints.stop).clicked();
        ui.separator();
        clicks.apply = ui.add_enabled(ready, Button::new("Apply")).clicked();
        clicks.cancel = ui.button("Cancel").clicked();
    });
    clicks
}
