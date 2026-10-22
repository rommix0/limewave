//! The folder-mode file list: the folder's instruments, then one row per file with a
//! thumbnail, name, root key and chips.

use std::path::{Path, PathBuf};

use eframe::egui::{
    self, Align, Color32, Layout, RichText, ScrollArea, Sense, Stroke, Ui, UiBuilder, vec2,
};
use limewave_core::edit::LoopMode;
use limewave_core::notes::note_name;

use crate::app::{FileEntry, Session};
use crate::theme::colors;

const ROW_HEIGHT: f32 = 40.0;
const THUMB: egui::Vec2 = vec2(44.0, 26.0);

/// A row the user clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileListClick {
    /// An index into the files.
    File(usize),
    /// An index into the instruments.
    Instrument(usize),
}

/// Draws the list, `instruments` first, and returns the row the user clicked, if any.
pub fn file_list(
    ui: &mut Ui,
    files: &[FileEntry],
    instruments: &[PathBuf],
    selected: Option<usize>,
) -> Option<FileListClick> {
    if files.is_empty() && instruments.is_empty() {
        ui.weak("No .wav files in this folder.");
        return None;
    }
    let n = instruments.len();
    let mut clicked = None;
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, ROW_HEIGHT, n + files.len(), |ui, range| {
            for i in range {
                if i < n {
                    if instrument_row(ui, &instruments[i]) {
                        clicked = Some(FileListClick::Instrument(i));
                    }
                } else if row(ui, &files[i - n], selected == Some(i - n)) {
                    clicked = Some(FileListClick::File(i - n));
                }
            }
        });
    clicked
}

/// A row for an `.sfz` file: a keyboard glyph, its name and an "instrument" chip.
fn instrument_row(ui: &mut Ui, path: &Path) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT), Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, Color32::from_white_alpha(10));
    }
    let mut inner = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(6.0, 3.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    inner.style_mut().interaction.selectable_labels = false;
    keys_glyph(&mut inner);
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    inner.vertical(|ui| {
        ui.add(egui::Label::new(name).truncate());
        ui.label(RichText::new("instrument").small().color(colors::LIME));
    });
    response.on_hover_text("Open this instrument").clicked()
}

/// A small keyboard drawn in the thumbnail's place (drawn, as not every font has a glyph).
fn keys_glyph(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(THUMB, Sense::hover());
    let painter = ui.painter_at(rect);
    let keys = rect.shrink2(vec2(6.0, 2.0));
    painter.rect_filled(keys, 2.0, Color32::from_gray(220));
    let white = keys.width() / 7.0;
    for k in 1..7 {
        let x = keys.left() + white * k as f32;
        painter.line_segment(
            [egui::pos2(x, keys.top()), egui::pos2(x, keys.bottom())],
            Stroke::new(1.0, Color32::from_gray(90)),
        );
    }
    // Black keys sit after white keys 1, 2, 4, 5 and 6 of the octave.
    for k in [1, 2, 4, 5, 6] {
        let x = keys.left() + white * k as f32;
        let black = egui::Rect::from_min_max(
            egui::pos2(x - white * 0.3, keys.top()),
            egui::pos2(x + white * 0.3, keys.top() + keys.height() * 0.6),
        );
        painter.rect_filled(black, 0.0, Color32::from_gray(30));
    }
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
