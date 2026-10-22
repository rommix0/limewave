//! The left panel while an instrument is open: its regions by group, and the WAVs no region
//! uses yet (spec 5.1).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use eframe::egui::{
    self, Align, Color32, Layout, RichText, ScrollArea, Sense, Ui, UiBuilder, vec2,
};
use limewave_core::edit::LoopMode;
use limewave_core::notes::note_name;
use limewave_core::sfz::model::{Instrument, Keycenter, RegionInfo, SfzLoopMode};
use limewave_core::sfz::parse::Token;
use limewave_core::sfz::paths::same_file_key;

use crate::app::FileEntry;
use crate::instrument::InstrumentState;
use crate::theme::colors;
use crate::ui::filelist::dirty_dot;

const ROW_HEIGHT: f32 = 40.0;

/// What the user did in the region list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionListAction {
    Select(usize),
    AddGroup,
    DeleteRegion(usize),
    /// Add a region for this WAV, which no region plays yet.
    AddUnassigned(PathBuf),
    Close,
}

/// What one region row shows.
struct Row {
    region: usize,
    name: String,
    root: String,
    range: String,
    looped: bool,
    unsaved: bool,
    missing: bool,
}

/// The WAVs of the instrument's folders, and the loaded files, that no region plays: sorted by
/// name, each file once.
pub fn unassigned_wavs(st: &InstrumentState, files: &[FileEntry]) -> Vec<PathBuf> {
    let bound: HashSet<String> = st
        .binding
        .samples
        .iter()
        .map(|s| same_file_key(&s.path))
        .collect();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for sample in &st.binding.samples {
        let dir = sample.path.parent().unwrap_or(Path::new("")).to_path_buf();
        if !dirs.iter().any(|d| same_file_key(d) == same_file_key(&dir)) {
            dirs.push(dir);
        }
    }
    let mut found: Vec<PathBuf> = Vec::new();
    let mut seen = bound;
    let mut add = |path: PathBuf| {
        if seen.insert(same_file_key(&path)) {
            found.push(path);
        }
    };
    for dir in &dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let wav = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("wav"));
            if wav && path.is_file() {
                add(path);
            }
        }
    }
    // A region's WAV that was deleted stays open, wherever it lies.
    for file in files {
        if matches!(file, FileEntry::Loaded(s) if !s.untitled) {
            add(file.path().to_path_buf());
        }
    }
    found.sort_by_cached_key(|p| (file_name(p).to_lowercase(), same_file_key(p)));
    found
}

/// What identifies a region across edits: its group and its own tokens, header included.
fn region_keys(inst: &Instrument) -> Vec<String> {
    let tokens = &inst.file.tokens;
    inst.regions()
        .into_iter()
        .enumerate()
        .map(|(n, h)| {
            let end = tokens[h + 1..]
                .iter()
                .position(|t| matches!(t, Token::Header(_)))
                .map_or(tokens.len(), |p| h + 1 + p);
            format!("{:?}{:?}", inst.group_of(n), &tokens[h..end])
        })
        .collect()
}

/// Per region, whether a save would write something for it: its sample is unsaved, or the
/// region is not as it was in the saved instrument. Regions are compared by content, as their
/// numbers shift when others are added or removed. Everything counts while the instrument has
/// never been saved.
pub fn unsaved_regions(st: &InstrumentState, files: &[FileEntry]) -> Vec<bool> {
    let count = st.inst().regions().len();
    let changed: Vec<bool> = match st.history.saved() {
        Some(saved) if !st.untitled => {
            if st.history.is_dirty() {
                let old: HashSet<String> = region_keys(saved).into_iter().collect();
                region_keys(st.inst())
                    .iter()
                    .map(|k| !old.contains(k))
                    .collect()
            } else {
                vec![false; count]
            }
        }
        _ => vec![true; count],
    };
    changed
        .into_iter()
        .enumerate()
        .map(|(n, c)| {
            c || st
                .region_file(n)
                .and_then(|i| files.get(i))
                .is_some_and(FileEntry::is_dirty)
        })
        .collect()
}

/// [`unassigned_wavs`], read from the folders again only when the bound samples or the open
/// files changed since the instrument's stored list. Returns the list with the key it belongs to.
pub fn unassigned_cached(st: &InstrumentState, files: &[FileEntry]) -> (Vec<String>, Vec<PathBuf>) {
    let key: Vec<String> = st
        .binding
        .samples
        .iter()
        .map(|s| format!("b{}", s.path.display()))
        .chain(files.iter().map(|f| {
            let state = if matches!(f, FileEntry::Loaded(_)) {
                'l'
            } else {
                'o'
            };
            format!("{state}{}", f.path().display())
        }))
        .collect();
    match &st.unassigned {
        Some((k, list)) if *k == key => (key, list.clone()),
        _ => {
            let list = unassigned_wavs(st, files);
            (key, list)
        }
    }
}

/// The last part of `path`.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// The text of a `sample=` value after its last separator.
fn text_name(text: &str) -> &str {
    text.rsplit(['/', '\\']).next().unwrap_or(text)
}

/// The title of group `g`: its label, else "Group N", then its velocity range when it sets one.
pub(crate) fn group_title(st: &InstrumentState, g: usize) -> String {
    let inst = st.inst();
    let mut title = match inst.group_get(g, "group_label") {
        Some(label) if !label.trim().is_empty() => label,
        _ => format!("Group {}", g + 1),
    };
    let vel = |name: &str| {
        inst.group_get(g, name)
            .and_then(|v| v.trim().parse::<u8>().ok())
    };
    let (lo, hi) = (vel("lovel"), vel("hivel"));
    if lo.is_some() || hi.is_some() {
        title.push_str(&format!(
            " \u{b7} vel {}\u{2013}{}",
            lo.unwrap_or(1),
            hi.unwrap_or(127)
        ));
    }
    title
}

/// A region's key range: "F#2–A#2", or one note.
fn key_range(lo: u8, hi: u8) -> String {
    if lo == hi {
        note_name(lo)
    } else {
        format!("{}\u{2013}{}", note_name(lo), note_name(hi))
    }
}

fn row_data(
    st: &InstrumentState,
    files: &[FileEntry],
    region: usize,
    info: &RegionInfo,
    unsaved: bool,
) -> Row {
    let sample_index = st.binding.region_sample.get(region).copied().flatten();
    let loaded = match st.region_file(region).and_then(|i| files.get(i)) {
        Some(FileEntry::Loaded(s)) => Some(s),
        _ => None,
    };
    let name = match sample_index {
        Some(i) => file_name(&st.binding.samples[i].path),
        None => info
            .sample
            .as_deref()
            .map_or_else(|| "(no sample)".to_owned(), |s| text_name(s).to_owned()),
    };
    let root = match (info.keycenter, loaded) {
        (Keycenter::Note(n), _) => note_name(n),
        (Keycenter::Sample, Some(s)) => note_name(s.edit().root),
        (Keycenter::Sample, None) => "?".to_owned(),
    };
    let looped = match info.loop_mode {
        Some(SfzLoopMode::Continuous | SfzLoopMode::Sustain) => true,
        Some(SfzLoopMode::NoLoop | SfzLoopMode::OneShot) => false,
        None => loaded.is_some_and(|s| s.edit().loop_mode != LoopMode::Off),
    };
    Row {
        region,
        name,
        root,
        range: key_range(info.lokey, info.hikey),
        looped,
        unsaved,
        missing: sample_index.is_none(),
    }
}

/// Draws the header, the regions by group and the Unassigned WAVs, and returns what the user
/// did, if anything.
pub fn region_list(
    ui: &mut Ui,
    st: &InstrumentState,
    files: &[FileEntry],
    unassigned: &[PathBuf],
) -> Option<RegionListAction> {
    let mut action = None;
    let inst = st.inst();
    let regions = inst.regions().len();
    let groups = inst.groups().len();
    ui.horizontal(|ui| {
        if st.unsaved(files) {
            dirty_dot(ui);
        }
        let name = if st.untitled {
            "Untitled".into()
        } else {
            file_name(&st.path)
        };
        ui.add(egui::Label::new(RichText::new(name).strong()).truncate());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Close instrument").clicked() {
                action = Some(RegionListAction::Close);
            }
        });
    });
    ui.weak(format!(
        "{regions} region{} \u{b7} {groups} group{}",
        if regions == 1 { "" } else { "s" },
        if groups == 1 { "" } else { "s" },
    ));
    ui.horizontal(|ui| {
        if ui.button("Add group").clicked() {
            action = Some(RegionListAction::AddGroup);
        }
        if let Some(r) = st.selected_region.filter(|&r| r < regions)
            && ui.button("Delete region").clicked()
        {
            action = Some(RegionListAction::DeleteRegion(r));
        }
    });
    ui.separator();

    // Each region's row is worked out once, here.
    let group_of: Vec<Option<usize>> = (0..regions).map(|r| inst.group_of(r)).collect();
    let unsaved = unsaved_regions(st, files);
    let rows: Vec<(Option<usize>, Row)> = (0..regions)
        .map(|r| {
            (
                group_of[r],
                row_data(st, files, r, &inst.region_info(r), unsaved[r]),
            )
        })
        .collect();

    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if regions == 0 && groups == 0 {
                ui.weak("No regions.");
            }
            let ungrouped: Vec<&Row> = rows
                .iter()
                .filter(|(g, _)| g.is_none())
                .map(|(_, row)| row)
                .collect();
            if !ungrouped.is_empty() {
                ui.strong("No group");
                for row in ungrouped {
                    region_row(ui, row, st.selected_region == Some(row.region), &mut action);
                }
            }
            for g in 0..groups {
                ui.strong(group_title(st, g));
                for (_, row) in rows.iter().filter(|(rg, _)| *rg == Some(g)) {
                    region_row(ui, row, st.selected_region == Some(row.region), &mut action);
                }
                ui.add_space(4.0);
            }
            ui.separator();
            egui::CollapsingHeader::new(format!("Unassigned WAVs ({})", unassigned.len()))
                .default_open(false)
                .show(ui, |ui| {
                    if unassigned.is_empty() {
                        ui.weak("Every WAV in these folders is used.");
                    }
                    for path in unassigned {
                        ui.horizontal(|ui| {
                            if ui.button("Add").clicked() {
                                action = Some(RegionListAction::AddUnassigned(path.clone()));
                            }
                            ui.add(egui::Label::new(file_name(path)).truncate())
                                .on_hover_text(path.to_string_lossy());
                        });
                    }
                });
        });
    action
}

fn region_row(ui: &mut Ui, row: &Row, selected: bool, action: &mut Option<RegionListAction>) {
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
            .layout(Layout::top_down(Align::Min)),
    );
    // Labels must not take the click away from the row.
    inner.style_mut().interaction.selectable_labels = false;
    inner.add(egui::Label::new(&row.name).truncate());
    inner.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(
            RichText::new(&row.root)
                .small()
                .strong()
                .color(colors::NEON),
        );
        ui.label(RichText::new(&row.range).small());
        if row.looped {
            ui.label(RichText::new("\u{27f2}").small().color(colors::LOOP));
        }
        if row.unsaved {
            dirty_dot(ui);
        }
        if row.missing {
            ui.label(RichText::new("missing").small().color(colors::END));
        }
    });
    if response.clicked() {
        *action = Some(RegionListAction::Select(row.region));
    }
}
