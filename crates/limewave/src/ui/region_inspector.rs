//! The inspector's Region section (spec 5.3): the selected region's keys, velocities, group,
//! volume, playback mode and envelope, and the opcodes Limewave keeps without using.

use eframe::egui::{
    self, CollapsingHeader, ComboBox, DragValue, Grid, Response, RichText, Ui, text::CCursor,
    text::CCursorRange,
};
use limewave_core::notes::{note_name, parse_note};
use limewave_core::sfz::{Level, SfzLoopMode};

use crate::audio::voice::{Adsr, Playback};
use crate::instrument::InstrumentState;
use crate::ui::inspector::adsr_fields;
use crate::ui::regions::group_title;

/// What the user changed in the Region section. Every edit but `Group` and `Playback` is a
/// preview that `Commit` makes one undo step; the section sends `Commit` straight after an edit
/// that was typed or clicked, and when a drag ends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RegionEdit {
    Keys(u8, u8),
    Vel(u8, u8),
    Group(usize),
    Volume(f32),
    Playback(Playback),
    Envelope(Adsr),
    Commit,
}

/// What the inspector says of a region that keeps its own tuning and loop (spec 4.3).
pub const OWN_TUNING: &str = "This region keeps its own tuning and loop";

/// The selected region of an open instrument, as the inspector needs it.
pub struct RegionContext<'a> {
    pub st: &'a InstrumentState,
    pub region: usize,
    /// How many regions play this region's sample (0 when it has none).
    pub shared_by: usize,
    /// Whether the region follows its sample's tuning and loop (false: it keeps its own,
    /// spec 4.3).
    pub follows: bool,
}

impl<'a> RegionContext<'a> {
    pub fn new(st: &'a InstrumentState, region: usize) -> Self {
        let sample = st.binding.region_sample.get(region).copied().flatten();
        let (shared_by, follows) = sample.map_or((0, true), |s| {
            let b = &st.binding.samples[s];
            let at = b.regions.iter().position(|&r| r == region);
            (b.regions.len(), at.is_none_or(|i| b.follows[i]))
        });
        RegionContext {
            st,
            region,
            shared_by,
            follows,
        }
    }
}

/// The note under the Tuning and Loop headings: who shares the sample, or that this region
/// keeps its own values.
pub fn region_note(ui: &mut Ui, region: Option<&RegionContext>) {
    let Some(r) = region else {
        return;
    };
    if !r.follows {
        ui.label(RichText::new(OWN_TUNING).strong());
    } else if r.shared_by > 1 {
        ui.weak(format!("Shared by {} regions", r.shared_by));
    }
}

/// Pushes `edit` (and the `Commit` that ends it, unless a drag is still going) when `r` was
/// changed, or just `Commit` when a drag ended without a last change.
fn push_edit(out: &mut Vec<RegionEdit>, r: &Response, edit: impl FnOnce() -> RegionEdit) {
    if r.changed() {
        out.push(edit());
        if !r.dragged() {
            out.push(RegionEdit::Commit);
        }
    } else if r.drag_stopped() {
        out.push(RegionEdit::Commit);
    }
}

/// `a` and `b` as (low, high).
fn ordered(a: u8, b: u8) -> (u8, u8) {
    (a.min(b), a.max(b))
}

/// A note name as editable text. Enter or leaving the field gives a valid note back;
/// anything else, or Escape, puts the old one back.
fn note_field(ui: &mut Ui, name: &Response, value: u8) -> Option<u8> {
    let id = ui.make_persistent_id(("region_note", name.id));
    let focused = ui.memory(|m| m.has_focus(id));
    let mut text = if focused {
        ui.data(|d| d.get_temp::<String>(id))
    } else {
        None
    }
    .unwrap_or_else(|| note_name(value));
    let mut out = egui::TextEdit::singleline(&mut text)
        .id(id)
        .desired_width(48.0)
        .show(ui);
    let response = Response::clone(&out.response).labelled_by(name.id);
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
    if response.lost_focus() && !cancelled {
        parse_note(&text).filter(|&n| n != value)
    } else {
        None
    }
}

/// The Region section for `region` of the instrument as it is shown now (a drag preview
/// included). Returns what the user changed this frame, in order.
pub fn region_section(ui: &mut Ui, st: &InstrumentState, region: usize) -> Vec<RegionEdit> {
    let inst = st.shown();
    let info = inst.region_info(region);
    let mut out = Vec::new();
    ui.heading("Region");

    Grid::new("region_basics")
        .num_columns(2)
        .spacing([10.0, 3.0])
        .show(ui, |ui| {
            // Typed in either order: the lower note is the low key.
            let (lo, hi) = (info.lokey, info.hikey);
            let name = ui.label("Low key");
            if let Some(n) = note_field(ui, &name, lo) {
                let (a, b) = ordered(n, hi);
                out.extend([RegionEdit::Keys(a, b), RegionEdit::Commit]);
            }
            ui.end_row();
            let name = ui.label("High key");
            if let Some(n) = note_field(ui, &name, hi) {
                let (a, b) = ordered(lo, n);
                out.extend([RegionEdit::Keys(a, b), RegionEdit::Commit]);
            }
            ui.end_row();

            let (mut lo, mut hi) = (info.lovel, info.hivel);
            let name = ui.label("Low velocity");
            let field = ui.add(
                DragValue::new(&mut lo)
                    .range(1..=hi)
                    .clamp_existing_to_range(false)
                    .speed(0.5),
            );
            push_edit(&mut out, &field.labelled_by(name.id), || {
                RegionEdit::Vel(lo, hi)
            });
            ui.end_row();
            let name = ui.label("High velocity");
            let field = ui.add(
                DragValue::new(&mut hi)
                    .range(info.lovel..=127)
                    .clamp_existing_to_range(false)
                    .speed(0.5),
            );
            push_edit(&mut out, &field.labelled_by(name.id), || {
                RegionEdit::Vel(info.lovel, hi)
            });
            ui.end_row();

            let name = ui.label("Group");
            if let Some(g) = group_combo(ui, &name, st, region) {
                out.push(RegionEdit::Group(g));
            }
            ui.end_row();

            let name = ui.label("Volume");
            let mut db = info.volume;
            let field = ui.add(
                DragValue::new(&mut db)
                    .range(-144.0..=6.0)
                    .clamp_existing_to_range(false)
                    .speed(0.1)
                    .max_decimals(2)
                    .suffix(" dB"),
            );
            push_edit(&mut out, &field.labelled_by(name.id), || {
                RegionEdit::Volume(db)
            });
            ui.end_row();
        });

    ui.horizontal(|ui| {
        ui.label("Playback");
        let now = match info.loop_mode {
            Some(SfzLoopMode::OneShot) => Playback::OneShot,
            Some(SfzLoopMode::Sustain) => Playback::Sustain,
            _ => Playback::Normal,
        };
        for (mode, label, hint) in [
            (Playback::Normal, "Normal", "Plays until released"),
            (
                Playback::OneShot,
                "One-shot",
                "Plays the whole sample on a note and ignores note-off",
            ),
            (
                Playback::Sustain,
                "Sustain loop",
                "Loops while the key is held and plays on past the loop end on release",
            ),
        ] {
            if ui
                .selectable_label(now == mode, label)
                .on_hover_text(hint)
                .clicked()
                && now != mode
            {
                out.push(RegionEdit::Playback(mode));
            }
        }
    });

    ui.label(RichText::new("Envelope").strong());
    let mut adsr = Adsr {
        attack_ms: info.ampeg.attack * 1000.0,
        decay_ms: info.ampeg.decay * 1000.0,
        sustain: info.ampeg.sustain / 100.0,
        release_ms: info.ampeg.release * 1000.0,
    };
    let fields = adsr_fields(ui, &mut adsr);
    if fields.changed {
        out.push(RegionEdit::Envelope(adsr));
        if !fields.dragged {
            out.push(RegionEdit::Commit);
        }
    } else if fields.drag_stopped {
        out.push(RegionEdit::Commit);
    }

    other_settings(ui, st, region);
    out
}

/// The region's group as a list of the instrument's groups. Returns the group chosen, if it is
/// another one.
fn group_combo(ui: &mut Ui, name: &Response, st: &InstrumentState, region: usize) -> Option<usize> {
    let groups = st.shown().groups().len();
    let current = st.shown().group_of(region);
    let selected = current.map_or_else(|| "No group".to_owned(), |g| group_title(st, g));
    let mut chosen = None;
    ui.add_enabled_ui(groups > 0, |ui| {
        let combo = ComboBox::from_id_salt("region_group")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for g in 0..groups {
                    if ui
                        .selectable_label(current == Some(g), group_title(st, g))
                        .clicked()
                        && current != Some(g)
                    {
                        chosen = Some(g);
                    }
                }
            });
        combo.response.labelled_by(name.id);
    });
    chosen
}

/// Whether Limewave keeps opcode `name` but does not play it (spec 6.3): filters, other
/// envelopes, LFOs, pan and width, sequences and randomising, crossfades, keyswitches,
/// controller-driven values, effects and pitch bend.
pub fn not_heard(name: &str) -> bool {
    let cc_digit = name
        .as_bytes()
        .windows(3)
        .any(|w| &w[..2] == b"cc" && w[2].is_ascii_digit());
    // The four `ampeg_*` times Limewave plays are not in the list this is asked about.
    [
        "fil",
        "pitcheg_",
        "seq_",
        "xf",
        "sw_",
        "bend_",
        "ampeg_",
        "eq",
        "cutoff",
        "resonance",
        "pan",
        "width",
        "trigger",
    ]
    .iter()
    .any(|p| name.starts_with(p))
        || ["lorand", "hirand"].contains(&name)
        || ["lfo", "_oncc", "effect"].iter().any(|p| name.contains(p))
        || cc_digit
}

/// "region", "group", ...: where an opcode was set.
fn level_text(level: Level) -> &'static str {
    match level {
        Level::Region => "region",
        Level::Group => "group",
        Level::Master => "master",
        Level::Global => "global",
    }
}

/// The collapsed read-only list of every opcode that applies to the region and has no
/// control of its own (spec 5.3).
fn other_settings(ui: &mut Ui, st: &InstrumentState, region: usize) {
    CollapsingHeader::new("Other settings (kept as written)")
        .default_open(false)
        .show(ui, |ui| {
            let others = st.shown().other_settings(region);
            if others.is_empty() {
                ui.weak("None");
                return;
            }
            Grid::new("region_other")
                .num_columns(3)
                .spacing([10.0, 3.0])
                .show(ui, |ui| {
                    for (name, value, level) in others {
                        ui.label(&name);
                        ui.add(egui::Label::new(value).wrap());
                        let mut origin = level_text(level).to_owned();
                        if not_heard(&name) {
                            origin.push_str(", not heard in Limewave");
                        }
                        ui.weak(origin);
                        ui.end_row();
                    }
                });
        });
}

/// The Region section alone, for a region whose sample is not loaded (there is no sample to
/// show the other sections for).
pub fn region_only(ui: &mut Ui, ctx: &RegionContext) -> Vec<RegionEdit> {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| region_section(ui, ctx.st, ctx.region))
        .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unplayed_opcodes_are_marked() {
        for name in [
            "fil_type",
            "fil_veltrack",
            "fillfo_freq",
            "fileg_attack",
            "pitcheg_depth",
            "pitchlfo_depth",
            "amplfo_freq",
            "pan",
            "pan_oncc10",
            "width",
            "seq_length",
            "lorand",
            "hirand",
            "xfin_lokey",
            "xfout_hikey",
            "sw_last",
            "sw_lokey",
            "volume_oncc7",
            "locc64",
            "hicc1",
            "on_locc20",
            "effect1",
            "bend_up",
            "ampeg_hold",
            "cutoff",
            "resonance",
            "eq1_gain",
            "trigger",
            "pan_law",
            "width_oncc4",
        ] {
            assert!(not_heard(name), "{name}");
        }
    }

    #[test]
    fn other_opcodes_are_not_marked() {
        for name in [
            "group",
            "off_by",
            "polyphony",
            "default_path",
            "accent1",
            "key_label",
            "expand",
            "span_x",
        ] {
            assert!(!not_heard(name), "{name}");
        }
    }

    #[test]
    fn keys_are_ordered_low_first() {
        assert_eq!(ordered(70, 60), (60, 70));
        assert_eq!(ordered(60, 70), (60, 70));
        assert_eq!(ordered(5, 5), (5, 5));
    }
}
