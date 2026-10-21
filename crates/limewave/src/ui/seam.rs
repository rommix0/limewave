//! The seam view: the audio up to the loop end (left) laid over the audio from the loop start
//! (right), centred on the seam, so a bad splice shows as a kink.

use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, Role, Sense, Shape, Slider, Stroke, WidgetInfo, pos2,
};
use limewave_core::dsp::{mono_sum, step_upward};
use limewave_core::edit::{EditState, LoopMode, Marker, move_marker};
use limewave_core::render::{SeamData, seam_data};
use limewave_core::wav::SampleFormat;

use super::waveform::cursor_color;
use crate::app::{Session, UiState};
use crate::audio::engine::{AudioEngine, Cursor};
use crate::theme::colors;

pub const MIN_HALF: usize = 16;
pub const MAX_HALF: usize = 2048;
pub const DEFAULT_HALF: usize = 128;

const BACKGROUND: Color32 = Color32::from_rgb(0x15, 0x17, 0x1d);
/// Sample dots appear above this many pixels per sample.
const DOT_MIN_SPACING: f32 = 6.0;
const MIN_PLOT_HEIGHT: f32 = 80.0;
const SIDE_MARGIN: f32 = 8.0;

/// Which channel the seam view draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SeamChannel {
    /// The average of all channels.
    #[default]
    Sum,
    Left,
    Right,
}

/// One channel (or the channel average) over `from..to`. `to` must not exceed any channel.
fn window(audio: &[Vec<f32>], channel: SeamChannel, from: usize, to: usize) -> Vec<f32> {
    let part = |c: &Vec<f32>| c[from..to].to_vec();
    let channels: Vec<Vec<f32>> = match channel {
        SeamChannel::Sum => audio.iter().map(part).collect(),
        SeamChannel::Left => audio.first().map(part).into_iter().collect(),
        SeamChannel::Right => audio.last().map(part).into_iter().collect(),
    };
    mono_sum(&channels)
}

/// The seam of `audio` for `channel`, reading only the samples around the two loop points.
/// `None` when the loop points do not lie inside the audio.
pub fn seam_window(
    audio: &[Vec<f32>],
    channel: SeamChannel,
    loop_start: usize,
    loop_end: usize,
    half: usize,
) -> Option<SeamData> {
    let len = audio.iter().map(Vec::len).min()?;
    if loop_start > loop_end || loop_end >= len {
        return None;
    }
    // Two small windows instead of a full-length mono sum: one ending/continuing at the loop
    // end, one at the loop start. Each is queried with both points at the same sample.
    let end_from = (loop_end + 1).saturating_sub(half);
    let end_to = (loop_end + 1 + half).min(len);
    let start_from = loop_start.saturating_sub(half);
    let start_to = (loop_start + half).min(len);
    let around_end = window(audio, channel, end_from, end_to);
    let around_start = window(audio, channel, start_from, start_to);
    let at_end = seam_data(&around_end, loop_end - end_from, loop_end - end_from, half);
    let at_start = seam_data(
        &around_start,
        loop_start - start_from,
        loop_start - start_from,
        half,
    );
    let jump = at_start.after_start.first()? - at_end.before_end.last()?;
    Some(SeamData {
        before_end: at_end.before_end,
        ghost_before_start: at_start.ghost_before_start,
        after_start: at_start.after_start,
        ghost_after_end: at_end.ghost_after_end,
        jump,
        natural_step: at_end.natural_step,
    })
}

/// A sample value as the user knows it: integer steps of the file's bit depth, or a float.
pub fn sample_text(v: f32, format: SampleFormat) -> String {
    match format {
        SampleFormat::Int { bits } => {
            let scale = 2f64.powi(i32::from(bits) - 1);
            format!("{}", (f64::from(v) * scale).round() as i64)
        }
        SampleFormat::Float { .. } => format!("{v:.4}"),
    }
}

/// `jump a → b · natural next c`: the value at the loop end, the value it jumps to, and the
/// value it would have moved to without the loop.
pub fn readout(seam: &SeamData, format: SampleFormat) -> Option<String> {
    let a = *seam.before_end.last()?;
    let b = *seam.after_start.first()?;
    let (a, b) = (sample_text(a, format), sample_text(b, format));
    Some(match seam.ghost_after_end.first() {
        Some(&c) => format!("jump {a} → {b} · natural next {}", sample_text(c, format)),
        None => format!("jump {a} → {b} · loop ends at the last sample"),
    })
}

#[derive(Clone, Copy)]
enum Nudge {
    Step(isize),
    Crossing(bool),
}

/// The edit with `marker` nudged, or `None` when it cannot move that way.
fn nudged(s: &Session, ui_state: &UiState, marker: Marker, nudge: Nudge) -> Option<EditState> {
    let e = s.edit();
    let current = match marker {
        Marker::LoopStart => e.loop_start,
        _ => e.loop_end,
    };
    let to = match nudge {
        Nudge::Step(d) => current.checked_add_signed(d)?,
        // The end marker is the last looped sample: the next sample (end + 1) is what lands
        // on the crossing.
        Nudge::Crossing(forward) if marker == Marker::LoopEnd => {
            step_upward(&s.mono, current + 1, forward)? - 1
        }
        Nudge::Crossing(forward) => step_upward(&s.mono, current, forward)?,
    };
    let moved = move_marker(e, marker, to, ui_state.lock_length, s.doc.frames);
    (moved != *e).then_some(moved)
}

fn nudge_row(
    ui: &mut egui::Ui,
    s: &mut Session,
    ui_state: &UiState,
    engine: Option<&AudioEngine>,
    marker: Marker,
    title: &str,
) {
    ui.label(title);
    let buttons = [
        (
            "zc<",
            "Previous upward zero crossing",
            Nudge::Crossing(false),
        ),
        ("-1", "One sample earlier", Nudge::Step(-1)),
        ("+1", "One sample later", Nudge::Step(1)),
        ("zc>", "Next upward zero crossing", Nudge::Crossing(true)),
    ];
    for (label, hint, nudge) in buttons {
        if ui.button(label).on_hover_text(hint).clicked()
            && let Some(e) = nudged(s, ui_state, marker, nudge)
        {
            s.apply(e, engine);
        }
    }
}

/// Draws the seam view of `s` filling the available space.
pub fn seam_view(
    ui: &mut egui::Ui,
    s: &mut Session,
    ui_state: &mut UiState,
    engine: Option<&AudioEngine>,
    cursors: &[Cursor],
) {
    let e = s.shown_edit().clone();
    if e.loop_mode == LoopMode::Off {
        ui.weak("Loop is off");
        return;
    }
    ui_state.seam_half = ui_state.seam_half.clamp(MIN_HALF, MAX_HALF);
    let seam = seam_window(
        &s.rendered,
        ui_state.seam_channel,
        e.loop_start,
        e.loop_end,
        ui_state.seam_half,
    );

    ui.horizontal_wrapped(|ui| {
        ui.strong("Seam");
        match seam.as_ref().and_then(|d| readout(d, s.doc.format.format)) {
            Some(text) => ui.label(text),
            None => ui.weak("no seam to show"),
        };
    });
    ui.horizontal_wrapped(|ui| {
        let name = ui.label("Half-width");
        let slider = ui.add(
            Slider::new(&mut ui_state.seam_half, MIN_HALF..=MAX_HALF)
                .logarithmic(true)
                .suffix(" samples"),
        );
        let _ = slider.labelled_by(name.id);
        if s.rendered.len() > 1 {
            for (channel, label) in [
                (SeamChannel::Sum, "Sum"),
                (SeamChannel::Left, "L"),
                (SeamChannel::Right, "R"),
            ] {
                ui.selectable_value(&mut ui_state.seam_channel, channel, label);
            }
        }
        ui.toggle_value(&mut ui_state.lock_length, "Lock length")
            .on_hover_text("Moving either loop marker moves both");
    });
    ui.horizontal_wrapped(|ui| {
        nudge_row(ui, s, ui_state, engine, Marker::LoopStart, "Loop start");
        ui.separator();
        nudge_row(ui, s, ui_state, engine, Marker::LoopEnd, "Loop end");
    });

    let size = egui::vec2(
        ui.available_width(),
        ui.available_height().max(MIN_PLOT_HEIGHT),
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Canvas, true, "Seam view"));
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, BACKGROUND);
    if let Some(seam) = &seam {
        draw(&painter, rect, seam, ui_state.seam_half);
        draw_cursors(
            &painter,
            rect,
            ui_state.seam_half,
            (e.loop_start, e.loop_end),
            cursors,
        );
    }
}

/// Cursors inside the window around the loop end (left half) or the loop start (right half), at
/// the x of the sample they are on. A frame sits where `draw` puts that sample: the loop end
/// next to the centre line on its left, the loop start next to it on its right.
fn draw_cursors(
    painter: &egui::Painter,
    rect: Rect,
    half: usize,
    (loop_start, loop_end): (usize, usize),
    cursors: &[Cursor],
) {
    let centre = rect.center().x;
    let spacing = ((rect.width() / 2.0 - SIDE_MARGIN).max(1.0)) / half as f32;
    let end = loop_end as f64 + 1.0;
    let start = loop_start as f64;
    for c in cursors {
        let before_end = (end - half as f64..end).contains(&c.frame);
        let after_start = (start..start + half as f64).contains(&c.frame);
        let mut xs = Vec::with_capacity(2);
        if before_end {
            xs.push(centre - spacing / 2.0 - ((end - 1.0 - c.frame) as f32) * spacing);
        }
        if after_start {
            xs.push(centre + spacing / 2.0 + ((c.frame - start) as f32) * spacing);
        }
        for x in xs {
            painter.vline(x, rect.y_range(), Stroke::new(1.0, cursor_color(c)));
        }
    }
}

fn faded(c: Color32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 110)
}

/// One run of samples as a line, dashed for the ghost runs, with dots when zoomed in.
fn draw_run(painter: &egui::Painter, points: &[Pos2], colour: Color32, dashed: bool, spacing: f32) {
    if points.len() < 2 {
        return;
    }
    if dashed {
        painter.extend(Shape::dashed_line(
            points,
            Stroke::new(1.0, faded(colour)),
            4.0,
            3.0,
        ));
    } else {
        painter.add(Shape::line(points.to_vec(), Stroke::new(1.5, colour)));
    }
    if spacing > DOT_MIN_SPACING {
        let dot = if dashed { faded(colour) } else { colour };
        painter.extend(points.iter().map(|&p| Shape::circle_filled(p, 2.0, dot)));
    }
}

fn draw(painter: &egui::Painter, rect: Rect, seam: &SeamData, half: usize) {
    let centre = rect.center();
    let spacing = ((rect.width() / 2.0 - SIDE_MARGIN).max(1.0)) / half as f32;
    let peak = [
        &seam.before_end,
        &seam.ghost_before_start,
        &seam.after_start,
        &seam.ghost_after_end,
    ]
    .into_iter()
    .flatten()
    .fold(0.0f32, |m, v| m.max(v.abs()))
    .max(1e-4);
    let y_of = |v: f32| centre.y - v / peak * rect.height() * 0.4;

    painter.hline(
        rect.x_range(),
        centre.y,
        Stroke::new(1.0, Color32::from_white_alpha(16)),
    );
    painter.extend(Shape::dashed_line(
        &[pos2(centre.x, rect.top()), pos2(centre.x, rect.bottom())],
        Stroke::new(1.0, Color32::from_white_alpha(40)),
        3.0,
        3.0,
    ));

    // Left runs end next to the centre line, right runs start just after it.
    let left = |run: &[f32]| -> Vec<Pos2> {
        let last = run.len() as f32 - 1.0;
        run.iter()
            .enumerate()
            .map(|(k, &v)| {
                pos2(
                    centre.x - spacing / 2.0 + (k as f32 - last) * spacing,
                    y_of(v),
                )
            })
            .collect()
    };
    let right = |run: &[f32]| -> Vec<Pos2> {
        run.iter()
            .enumerate()
            .map(|(k, &v)| pos2(centre.x + spacing / 2.0 + k as f32 * spacing, y_of(v)))
            .collect()
    };
    draw_run(
        painter,
        &left(&seam.ghost_before_start),
        colors::WAVE,
        true,
        spacing,
    );
    draw_run(
        painter,
        &right(&seam.ghost_after_end),
        colors::LOOP,
        true,
        spacing,
    );
    draw_run(
        painter,
        &left(&seam.before_end),
        colors::WAVE,
        false,
        spacing,
    );
    draw_run(
        painter,
        &right(&seam.after_start),
        colors::LOOP,
        false,
        spacing,
    );

    let font = FontId::proportional(10.0);
    painter.text(
        rect.left_top() + egui::vec2(6.0, 4.0),
        Align2::LEFT_TOP,
        "up to loop end",
        font.clone(),
        colors::WAVE,
    );
    painter.text(
        rect.right_top() + egui::vec2(-6.0, 4.0),
        Align2::RIGHT_TOP,
        "from loop start",
        font,
        colors::LOOP,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_text_scales_to_the_bit_depth() {
        let v = 8331.0 / 32768.0;
        assert_eq!(sample_text(v, SampleFormat::Int { bits: 16 }), "8331");
        assert_eq!(sample_text(-0.5, SampleFormat::Int { bits: 16 }), "-16384");
        assert_eq!(sample_text(0.5, SampleFormat::Int { bits: 24 }), "4194304");
        assert_eq!(sample_text(0.5, SampleFormat::Int { bits: 8 }), "64");
        assert_eq!(
            sample_text(0.123_456, SampleFormat::Float { bits: 32 }),
            "0.1235"
        );
        assert_eq!(sample_text(-0.0, SampleFormat::Int { bits: 16 }), "0");
    }

    fn ramp(n: usize) -> Vec<Vec<f32>> {
        let left: Vec<f32> = (0..n).map(|i| i as f32).collect();
        let right: Vec<f32> = (0..n).map(|i| -(i as f32)).collect();
        vec![left, right]
    }

    #[test]
    fn seam_window_matches_seam_data_on_the_full_signal() {
        let audio = ramp(500);
        let mono = mono_sum(&audio);
        for (ls, le, half) in [(100, 300, 32), (5, 10, 64), (0, 499, 16), (250, 251, 2048)] {
            let want = seam_data(&mono, ls, le, half);
            let got = seam_window(&audio, SeamChannel::Sum, ls, le, half).unwrap();
            assert_eq!(got, want, "loop {ls}..={le}, half {half}");
        }
    }

    #[test]
    fn seam_window_picks_channels() {
        let audio = ramp(100);
        let l = seam_window(&audio, SeamChannel::Left, 10, 50, 8).unwrap();
        let r = seam_window(&audio, SeamChannel::Right, 10, 50, 8).unwrap();
        assert_eq!(l.before_end.last(), Some(&50.0));
        assert_eq!(r.before_end.last(), Some(&-50.0));
        assert_eq!(l.jump, -40.0);
        // A mono file has no right channel; it falls back to the one it has.
        let mono = vec![audio[0].clone()];
        let m = seam_window(&mono, SeamChannel::Right, 10, 50, 8).unwrap();
        assert_eq!(m, l);
    }

    #[test]
    fn seam_window_rejects_bad_points() {
        let audio = ramp(100);
        assert!(seam_window(&audio, SeamChannel::Sum, 60, 50, 8).is_none());
        assert!(seam_window(&audio, SeamChannel::Sum, 10, 100, 8).is_none());
        assert!(seam_window(&[], SeamChannel::Sum, 0, 0, 8).is_none());
    }

    #[test]
    fn readout_text() {
        let mut x = vec![0.0f32; 3000];
        x[2915] = 8331.0 / 32768.0;
        x[2532] = 7381.0 / 32768.0;
        x[2916] = 7473.0 / 32768.0;
        let seam = seam_window(&[x.clone()], SeamChannel::Sum, 2532, 2915, 64).unwrap();
        let f = SampleFormat::Int { bits: 16 };
        assert_eq!(
            readout(&seam, f).unwrap(),
            "jump 8331 → 7381 · natural next 7473"
        );
        // At the last sample there is no natural next sample.
        let seam = seam_window(&[x], SeamChannel::Sum, 2532, 2999, 64).unwrap();
        assert!(
            readout(&seam, f)
                .unwrap()
                .ends_with("loop ends at the last sample")
        );
    }
}
