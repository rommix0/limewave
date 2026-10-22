//! The folder-mode file list: one row per file with a thumbnail, name, root key and chips.

use eframe::egui::{
    self, Align, Color32, Layout, RichText, ScrollArea, Sense, Stroke, Ui, UiBuilder, vec2,
};
use limewave_core::edit::LoopMode;
use limewave_core::notes::note_name;

use crate::app::{FileEntry, Session};
use crate::theme::colors;

const ROW_HEIGHT: f32 = 40.0;
const THUMB: egui::Vec2 = vec2(44.0, 26.0);

/// Draws the list and returns the row the user clicked, if any.
pub fn file_list(ui: &mut Ui, files: &[FileEntry], selected: Option<usize>) -> Option<usize> {
    if files.is_empty() {
        ui.weak("No .wav files in this folder.");
        return None;
    }
    let mut clicked = None;
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, files.len(), |ui, range| {
            for i in range {
                if row(ui, &files[i], selected == Some(i)) {
                    clicked = Some(i);
                }
            }
        });
    clicked
}

fn row(ui: &mut Ui, entry: &FileEntry, selected: bool) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    if selected {
        ui.painter()
            .rect_filled(rect, 4.0, colors::NEON.gamma_multiply(0.18));
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, Color32::from_white_alpha(10));
    }
    let mut inner = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(6.0, 3.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    // Labels must not take the click away from the row.
    inner.style_mut().interaction.selectable_labels = false;
    let name = entry.file_name();
    match entry {
        FileEntry::Loaded(s) => {
            thumbnail(&mut inner, s);
            inner.vertical(|ui| {
                ui.horizontal(|ui| {
                    if s.unsaved() {
                        dirty_dot(ui);
                    }
                    ui.add(egui::Label::new(name).truncate());
                });
                chips(ui, s);
            });
        }
        FileEntry::Loading(_) => {
            inner.add_space(THUMB.x + 8.0);
            inner.vertical(|ui| {
                ui.label(name);
                ui.weak("loading…");
            });
        }
        FileEntry::Failed(_, err) => {
            inner.add_space(THUMB.x + 8.0);
            inner.vertical(|ui| {
                ui.add(egui::Label::new(name).truncate());
                ui.label(RichText::new("failed").small().color(colors::END));
            });
            return response.on_hover_text(err.as_str()).clicked();
        }
    }
    response.clicked()
}

/// The unsaved-changes marker: a small filled circle (drawn, as not every font has the glyph).
fn dirty_dot(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.0, colors::LOOP);
}

fn chips(ui: &mut Ui, s: &Session) {
    let edit = s.edit();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(
            RichText::new(note_name(edit.root))
                .small()
                .strong()
                .color(colors::NEON),
        );
        if s.doc.smpl.is_some() {
            ui.label(RichText::new("smpl").small().color(colors::LIME));
        }
        if edit.loop_mode == LoopMode::Off {
            ui.label(RichText::new("no loop").small().weak());
        } else {
            ui.label(RichText::new("looped").small().color(colors::LOOP));
        }
    });
}

/// A tiny whole-file waveform built from the session's peak pyramids.
fn thumbnail(ui: &mut Ui, s: &Session) {
    let (rect, _) = ui.allocate_exact_size(THUMB, Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_black_alpha(90));
    let columns = THUMB.x as usize;
    let mid = rect.center().y;
    let half = rect.height() / 2.0 - 1.0;
    let stroke = Stroke::new(1.0, colors::WAVE);
    let frames = s.frames();
    for x in 0..columns {
        let from = x * frames / columns;
        let to = ((x + 1) * frames / columns).max(from + 1);
        let (lo, hi) = s
            .peaks
            .iter()
            .zip(&s.processed)
            .map(|(p, ch)| p.min_max(ch, from, to))
            .fold((0.0f32, 0.0f32), |(lo, hi), (a, b)| (lo.min(a), hi.max(b)));
        let px = rect.left() + x as f32 + 0.5;
        painter.line_segment(
            [
                egui::pos2(px, mid - hi.clamp(-1.0, 1.0) * half),
                egui::pos2(px, mid - lo.clamp(-1.0, 1.0) * half - 0.5),
            ],
            stroke,
        );
    }
}
