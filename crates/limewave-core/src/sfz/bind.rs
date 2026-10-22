//! Binding an instrument's regions to the WAVs they play, reading a region's
//! tuning and loop as an edit state, and writing sample edits back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::model::{Instrument, Keycenter, LoopDir, RegionInfo, SfzLoopMode};
use super::paths::{find_sample, same_file_key, sample_path};
use super::write::format_number;
use crate::edit::{EditState, LoopMode};
use crate::notes::{note_name, parse_note};

/// One WAV and the regions that play it.
#[derive(Clone, PartialEq, Debug)]
pub struct SampleBinding {
    /// The file found on disk.
    pub path: PathBuf,
    /// Region numbers using this file, in file order.
    pub regions: Vec<usize>,
    /// Parallel to `regions`: does the region follow the sample's tuning and
    /// loop (true), or keep its own (spec 4.3)?
    pub follows: Vec<bool>,
}

/// Which WAV each region plays.
#[derive(Clone, PartialEq, Debug)]
pub struct Binding {
    /// Distinct WAVs, in order of first use.
    pub samples: Vec<SampleBinding>,
    /// Region n -> index into `samples`; `None` when the region names no
    /// sample or the file is missing.
    pub region_sample: Vec<Option<usize>>,
}

/// Finds each region's WAV under `sfz_dir`. A file named two ways (other
/// separators, other case) is one sample. Every region follows until
/// [`complete`] says otherwise.
pub fn bind(inst: &Instrument, sfz_dir: &Path) -> Binding {
    let default_path = inst.default_path();
    let mut samples: Vec<SampleBinding> = Vec::new();
    let mut by_file: HashMap<String, usize> = HashMap::new();
    let mut region_sample = Vec::new();
    for region in 0..inst.regions().len() {
        let found = inst
            .region_info(region)
            .sample
            .and_then(|s| find_sample(&sample_path(sfz_dir, &default_path, &s)));
        let index = found.map(|path| {
            let i = *by_file.entry(same_file_key(&path)).or_insert_with(|| {
                samples.push(SampleBinding {
                    path,
                    regions: Vec::new(),
                    follows: Vec::new(),
                });
                samples.len() - 1
            });
            samples[i].regions.push(region);
            samples[i].follows.push(true);
            i
        });
        region_sample.push(index);
    }
    Binding {
        samples,
        region_sample,
    }
}

/// The sample state a region asks for (spec 4.2): root key, fine tune, loop
/// points, direction and on/off come from the region's opcodes over `wav`'s
/// (the direction is forward unless `loop_type` says otherwise, spec 3.4);
/// everything else is `wav`'s.
pub fn region_edit(inst: &Instrument, region: usize, wav: &EditState) -> EditState {
    edit_from_info(&inst.region_info(region), wav)
}

/// [`region_edit`] for a region whose settings `info` are already read.
fn edit_from_info(info: &RegionInfo, wav: &EditState) -> EditState {
    let mut e = wav.clone();
    e.root = match info.keycenter {
        Keycenter::Note(n) => n,
        Keycenter::Sample => wav.root,
    };
    // `tune` raises playback; fine tune says how sharp the recording is. The
    // subtraction from 0.0 keeps a zero tune from becoming -0.
    e.cents = 0.0 - info.tune;
    // Whole semitones of fine tune move to the root, so cents stay within
    // +-50 and the pitch is unchanged.
    let semitones = (e.cents / 100.0).round();
    e.root = (i32::from(e.root) + semitones as i32).clamp(0, 127) as u8;
    e.cents -= 100.0 * semitones;

    let wav_loops = wav.loop_mode != LoopMode::Off;
    let on = match info.loop_mode {
        Some(SfzLoopMode::NoLoop | SfzLoopMode::OneShot) => false,
        Some(SfzLoopMode::Continuous | SfzLoopMode::Sustain) => true,
        None => wav_loops || info.loop_start.is_some() || info.loop_end.is_some(),
    };
    // Without `loop_type` a sampler loops forward, whatever the WAV says.
    let dir = match info.loop_dir {
        LoopDir::Forward => LoopMode::Forward,
        LoopDir::Backward => LoopMode::Backward,
        LoopDir::Alternate => LoopMode::PingPong,
    };
    let ls = info.loop_start.unwrap_or(wav.loop_start).min(wav.end);
    let le = info.loop_end.unwrap_or(wav.loop_end).min(wav.end);
    e.loop_start = ls.min(le);
    e.loop_end = le;
    e.loop_mode = if on && ls <= le { dir } else { LoopMode::Off };
    // Hold the state the app would force on its first edit (a loop of at
    // least 2 frames, cents in range), so reading is not itself an edit.
    e.clamped(wav.end + 1)
}

/// Whether a region with settings `info` asks for the same tuning and loop as `reference`: the
/// same pitch, to within the rounding a written `tune` brings, and the same loop: both off, or
/// both on with the same points and direction. A loop on one side only is a difference when the
/// region has loop points, its own or its group's; one switched on by its playback mode alone
/// loops as the sample does. A region whose playback mode (`one_shot` or `no_loop`) keeps its
/// loop off agrees on any loop.
fn agrees(info: &RegionInfo, wav: &EditState, reference: &EditState) -> bool {
    let e = edit_from_info(info, wav);
    let pitch = |s: &EditState| f64::from(s.root) * 100.0 + f64::from(s.cents);
    let loop_off_by_mode = matches!(
        info.loop_mode,
        Some(SfzLoopMode::NoLoop | SfzLoopMode::OneShot)
    );
    let own_points = info.loop_start.is_some() || info.loop_end.is_some();
    let loops_agree = match (e.loop_mode, reference.loop_mode) {
        (LoopMode::Off, LoopMode::Off) => true,
        (LoopMode::Off, _) | (_, LoopMode::Off) => !own_points,
        _ => {
            (e.loop_mode, e.loop_start, e.loop_end)
                == (
                    reference.loop_mode,
                    reference.loop_start,
                    reference.loop_end,
                )
        }
    };
    (pitch(&e) - pitch(reference)).abs() < 1e-3 && (loop_off_by_mode || loops_agree)
}

/// A loaded sample's WAV's own `initial_edit`, and the state its editor started from.
pub type SampleStates = (EditState, EditState);

/// Sets each sample's `follows` (spec 4.3): a region follows when it asks for the tuning and
/// loop of the sample's reference state. `states[i]` is sample `i`'s pair of the WAV's own
/// `initial_edit` and that reference, the state the sample's editor started from (fixed until
/// the instrument is saved or opened again); `None` while it loads or if it failed, and then
/// every region follows.
pub fn complete(b: &mut Binding, inst: &Instrument, states: &[Option<SampleStates>]) {
    let infos = inst.region_infos();
    for (i, s) in b.samples.iter_mut().enumerate() {
        s.follows = match states.get(i) {
            Some(Some((wav, reference))) => s
                .regions
                .iter()
                .map(|&region| agrees(&infos[region], wav, reference))
                .collect(),
            _ => vec![true; s.regions.len()],
        };
    }
}

/// Sets `name` on `region` to `value`.
fn put(inst: &mut Instrument, region: usize, name: &str, value: impl ToString) {
    inst.set(region, name, &value.to_string());
}

/// Moves the sample-relative opcodes a region has, own or inherited, by the
/// trim so they keep pointing at the same audio, clamped to the trimmed
/// length. A shifted inherited value is written at region level.
fn shift_for_trim(inst: &mut Instrument, region: usize, shift: i64, max: i64) {
    for name in ["offset", "end", "loop_start", "loop_end"] {
        let Some((text, _)) = inst.get(region, name) else {
            continue;
        };
        let Ok(v) = text.trim().parse::<usize>() else {
            continue;
        };
        let new = (v as i64 - shift).clamp(0, max);
        if new != v as i64 {
            put(inst, region, name, new);
        }
    }
}

/// `key` as key opcode text spelt like `existing`, the text the opcode has now: a number when
/// that is one, a note name when that is one, else a note name when `name_when_new` and a
/// number otherwise.
pub fn key_text(existing: Option<&str>, key: u8, name_when_new: bool) -> String {
    let name = match existing.map(str::trim) {
        Some(t) if t.starts_with(|c: char| c.is_ascii_digit()) => false,
        Some(t) if parse_note(t).is_some() => true,
        _ => name_when_new,
    };
    if name {
        note_name(key)
    } else {
        key.to_string()
    }
}

/// Writes root key, fine tune and loop onto a region that follows its sample:
/// the tuning opcodes only when the tuning changed, the loop opcodes only when
/// the loop did, and each only when it differs from what the region has now.
fn write_tuning_and_loop(
    inst: &mut Instrument,
    region: usize,
    before: &EditState,
    after: &EditState,
) {
    let info = inst.region_info(region);
    if (before.root, before.cents) != (after.root, after.cents) {
        let current_root = match info.keycenter {
            Keycenter::Note(n) => n,
            // `sample` resolved to the WAV's own root when the edit began.
            Keycenter::Sample => before.root,
        };
        if after.root != current_root {
            let existing = inst.get(region, "pitch_keycenter").map(|(text, _)| text);
            let text = key_text(existing.as_deref(), after.root, false);
            put(inst, region, "pitch_keycenter", text);
        }
        let tune = format_number(-f64::from(after.cents));
        if format_number(f64::from(info.tune)) != tune {
            put(inst, region, "tune", tune);
        }
    }

    let loop_changed = (before.loop_mode, before.loop_start, before.loop_end)
        != (after.loop_mode, after.loop_start, after.loop_end);
    if !loop_changed {
        return;
    }
    if after.loop_mode == LoopMode::Off {
        // The loop is gone, so a region looping on it stops; `one_shot` stays.
        let stop = match info.loop_mode {
            Some(SfzLoopMode::NoLoop | SfzLoopMode::OneShot) => false,
            Some(SfzLoopMode::Continuous | SfzLoopMode::Sustain) => true,
            // Unset plays as the WAV does, which looped only if it was on.
            None => before.loop_mode != LoopMode::Off,
        };
        if stop {
            put(inst, region, "loop_mode", "no_loop");
        }
        return;
    }
    // A region that played without a loop now loops; `one_shot`, the looping
    // modes and a `no_loop` the loop was already on for are the region's own
    // choice and stay.
    let start_looping = match info.loop_mode {
        None => true,
        Some(SfzLoopMode::NoLoop) => before.loop_mode == LoopMode::Off,
        Some(_) => false,
    };
    if start_looping {
        put(inst, region, "loop_mode", "loop_continuous");
    }
    let start = after.loop_start.saturating_sub(after.start);
    let end = after.loop_end.saturating_sub(after.start);
    if info.loop_start != Some(start) {
        put(inst, region, "loop_start", start);
    }
    if info.loop_end != Some(end) {
        put(inst, region, "loop_end", end);
    }
    let (dir, name) = match after.loop_mode {
        LoopMode::Backward => (LoopDir::Backward, "backward"),
        LoopMode::PingPong => (LoopDir::Alternate, "alternate"),
        _ => (LoopDir::Forward, "forward"),
    };
    // Against `before` too: a direction read on opening and changed must say so.
    let before_dir = match before.loop_mode {
        LoopMode::Backward => LoopDir::Backward,
        LoopMode::PingPong => LoopDir::Alternate,
        _ => LoopDir::Forward,
    };
    if info.loop_dir != dir || before_dir != dir {
        put(inst, region, "loop_type", name);
    }
}

/// Writes a sample edit into the instrument (spec 7.2). Trimming moves the
/// sample-relative opcodes of every region using it; a change to root key,
/// fine tune or loop sets explicit opcodes on the regions that follow it.
pub fn write_back(inst: &mut Instrument, b: &SampleBinding, before: &EditState, after: &EditState) {
    // Trim first: the loop points written below are already relative to the
    // new start and must not be shifted again.
    let shift = after.start as i64 - before.start as i64;
    if shift != 0 || after.end != before.end {
        let max = after.end.saturating_sub(after.start) as i64;
        for &region in &b.regions {
            shift_for_trim(inst, region, shift, max);
        }
    }
    let tuned = before.root != after.root
        || before.cents != after.cents
        || before.loop_mode != after.loop_mode
        || before.loop_start != after.loop_start
        || before.loop_end != after.loop_end;
    if tuned {
        for (&region, _) in b.regions.iter().zip(&b.follows).filter(|(_, f)| **f) {
            write_tuning_and_loop(inst, region, before, after);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sfz::model::Level;
    use crate::sfz::parse::parse;
    use crate::sfz::write::write;

    fn inst(text: &str) -> Instrument {
        Instrument::new(parse(text).unwrap())
    }

    /// A WAV-derived state: 1000 frames, root 64, 3 cents sharp, no loop.
    fn wav() -> EditState {
        EditState {
            root: 64,
            cents: 3.0,
            ..EditState::default_for(1000)
        }
    }

    fn looped(mode: LoopMode, ls: usize, le: usize) -> EditState {
        EditState {
            loop_mode: mode,
            loop_start: ls,
            loop_end: le,
            ..wav()
        }
    }

    fn value(i: &Instrument, region: usize, name: &str) -> Option<String> {
        i.get(region, name).map(|(v, _)| v)
    }

    /// A binding for regions `0..n`, all following.
    fn all_follow(n: usize) -> SampleBinding {
        SampleBinding {
            path: PathBuf::new(),
            regions: (0..n).collect(),
            follows: vec![true; n],
        }
    }

    fn touch(dir: &Path, rel: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"RIFF").unwrap();
    }

    #[test]
    fn one_wav_named_two_ways_binds_once() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Samples/A.wav");
        std::fs::write(dir.path().join("a.sfz"), "").unwrap();
        let i = inst("<region> sample=Samples\\A.wav\n<region> sample=samples/a.wav");
        let b = bind(&i, dir.path());
        assert_eq!(b.samples.len(), 1);
        assert_eq!(b.samples[0].regions, [0, 1]);
        assert_eq!(b.samples[0].follows, [true, true]);
        assert_eq!(b.region_sample, [Some(0), Some(0)]);
        assert!(b.samples[0].path.is_file());
    }

    #[test]
    fn samples_are_in_order_of_first_use() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "b.wav");
        touch(dir.path(), "a.wav");
        let i = inst("<region> sample=b.wav <region> sample=a.wav <region> sample=B.WAV");
        let b = bind(&i, dir.path());
        assert_eq!(b.region_sample, [Some(0), Some(1), Some(0)]);
        assert_eq!(b.samples[0].regions, [0, 2]);
        assert_eq!(b.samples[1].regions, [1]);
    }

    #[test]
    fn default_path_is_honoured() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Kit/a.wav");
        let i = inst("<control> default_path=Kit/\n<region> sample=a.wav");
        assert_eq!(bind(&i, dir.path()).region_sample, [Some(0)]);
    }

    #[test]
    fn missing_samples_are_none() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.wav");
        let i = inst(
            "<region> sample=a.wav <region> sample=gone.wav <region> key=60 <region> sample=*sine",
        );
        let b = bind(&i, dir.path());
        assert_eq!(b.region_sample, [Some(0), None, None, None]);
        assert_eq!(b.samples.len(), 1);
    }

    #[test]
    fn tune_14_reads_as_fine_minus_14() {
        let mut i = inst("<region> sample=a.wav tune=14");
        let before = region_edit(&i, 0, &wav());
        assert_eq!(before.cents, -14.0);
        let after = EditState {
            cents: -20.0,
            ..before.clone()
        };
        write_back(&mut i, &all_follow(1), &before, &after);
        assert_eq!(value(&i, 0, "tune").as_deref(), Some("20"));
        assert_eq!(region_edit(&i, 0, &wav()).cents, -20.0);
    }

    #[test]
    fn unset_tune_reads_as_zero_not_the_wavs_cents() {
        let e = region_edit(&inst("<region> sample=a.wav"), 0, &wav());
        assert_eq!(e.cents, 0.0);
        assert!(e.cents.is_sign_positive());
    }

    #[test]
    fn keycenter_overrides_wav_root() {
        let i = inst("<region> sample=a.wav pitch_keycenter=49");
        assert_eq!(region_edit(&i, 0, &wav()).root, 49);
        let i = inst("<group> pitch_keycenter=c3 <region> sample=a.wav");
        assert_eq!(region_edit(&i, 0, &wav()).root, 48);
        // Nothing sets it: the SFZ default, not the WAV's root.
        let i = inst("<region> sample=a.wav");
        assert_eq!(region_edit(&i, 0, &wav()).root, 60);
    }

    #[test]
    fn pitch_keycenter_sample_keeps_wav_root() {
        let mut i = inst("<region> sample=a.wav pitch_keycenter=sample");
        let before = region_edit(&i, 0, &wav());
        assert_eq!(before.root, 64);
        // Editing something else leaves `sample` in place.
        let after = EditState {
            cents: 7.0,
            ..before.clone()
        };
        write_back(&mut i, &all_follow(1), &before, &after);
        assert_eq!(value(&i, 0, "pitch_keycenter").as_deref(), Some("sample"));
        // Changing the root replaces it.
        let after = EditState {
            root: 65,
            ..before.clone()
        };
        write_back(&mut i, &all_follow(1), &before, &after);
        assert_eq!(value(&i, 0, "pitch_keycenter").as_deref(), Some("65"));
    }

    #[test]
    fn region_loop_opcodes_override_wav_loop() {
        let w = looped(LoopMode::Forward, 100, 200);
        let i = inst(
            "<region> loop_mode=loop_continuous loop_start=300 loop_end=400 loop_type=backward",
        );
        let e = region_edit(&i, 0, &w);
        assert_eq!(
            (e.loop_mode, e.loop_start, e.loop_end),
            (LoopMode::Backward, 300, 400)
        );
        let i = inst("<region> loop_mode=loop_sustain loop_type=alternate");
        let e = region_edit(&i, 0, &w);
        assert_eq!(
            (e.loop_mode, e.loop_start, e.loop_end),
            (LoopMode::PingPong, 100, 200)
        );
        // No opcode sets the direction: forward, the SFZ default, whatever the WAV says.
        let w = looped(LoopMode::PingPong, 100, 200);
        let e = region_edit(&inst("<region> loop_mode=loop_sustain"), 0, &w);
        assert_eq!(e.loop_mode, LoopMode::Forward);
        // Nothing says anything about looping: the WAV's loop points are used.
        let e = region_edit(&inst("<region> sample=a.wav"), 0, &w);
        assert_eq!(
            (e.loop_mode, e.loop_start, e.loop_end),
            (LoopMode::Forward, 100, 200)
        );
        // A WAV without a loop, but loop points in the instrument: loops forward.
        let e = region_edit(&inst("<region> loop_start=10 loop_end=20"), 0, &wav());
        assert_eq!(
            (e.loop_mode, e.loop_start, e.loop_end),
            (LoopMode::Forward, 10, 20)
        );
        // Looping forced on a WAV without a loop uses forward and the whole sample.
        let e = region_edit(&inst("<region> loop_mode=loop_continuous"), 0, &wav());
        assert_eq!(
            (e.loop_mode, e.loop_start, e.loop_end),
            (LoopMode::Forward, 0, 999)
        );
    }

    #[test]
    fn no_loop_turns_loop_off() {
        let w = looped(LoopMode::Forward, 100, 200);
        for mode in ["no_loop", "one_shot"] {
            let i = inst(&format!("<region> loop_mode={mode}"));
            assert_eq!(region_edit(&i, 0, &w).loop_mode, LoopMode::Off, "{mode}");
        }
    }

    #[test]
    fn loop_points_are_clamped_into_the_sample() {
        let w = wav();
        let e = region_edit(
            &inst("<region> loop_mode=loop_continuous loop_start=500 loop_end=5000"),
            0,
            &w,
        );
        assert_eq!(
            (e.loop_mode, e.loop_start, e.loop_end),
            (LoopMode::Forward, 500, 999)
        );
        // Start after end cannot be a loop.
        let e = region_edit(
            &inst("<region> loop_mode=loop_continuous loop_start=600 loop_end=500"),
            0,
            &w,
        );
        assert_eq!(e.loop_mode, LoopMode::Off);
        assert!(e.loop_start <= e.loop_end);
    }

    #[test]
    fn disagreeing_regions_keep_their_own() {
        let mut i = inst(
            "<region> sample=a.wav pitch_keycenter=49\n<region> sample=a.wav pitch_keycenter=50",
        );
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.wav");
        let mut b = bind(&i, dir.path());
        let w = wav();
        complete(&mut b, &i, &[Some((w.clone(), region_edit(&i, 0, &w)))]);
        assert_eq!(b.samples[0].follows, [true, false]);

        let before = region_edit(&i, 0, &w);
        let after = EditState {
            root: 55,
            ..before.clone()
        };
        write_back(&mut i, &b.samples[0], &before, &after);
        assert_eq!(value(&i, 0, "pitch_keycenter").as_deref(), Some("55"));
        assert_eq!(value(&i, 1, "pitch_keycenter").as_deref(), Some("50"));
    }

    #[test]
    fn regions_that_agree_follow_and_loading_samples_all_follow() {
        let i = inst(
            "<group> pitch_keycenter=60 <region> sample=a.wav <region> sample=a.wav \
             <region> sample=a.wav tune=5 <region> sample=a.wav loop_mode=loop_continuous \
             <region> sample=a.wav loop_mode=loop_continuous loop_start=10 loop_end=500",
        );
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.wav");
        let mut b = bind(&i, dir.path());
        complete(&mut b, &i, &[Some((wav(), region_edit(&i, 0, &wav())))]);
        // The last two loop where the sample does not: the first by its playback mode alone,
        // so it loops as the sample does; the second with loop points of its own.
        assert_eq!(b.samples[0].follows, [true, true, false, true, false]);
        complete(&mut b, &i, &[None]);
        assert_eq!(b.samples[0].follows, [true; 5]);
        complete(&mut b, &i, &[]);
        assert_eq!(b.samples[0].follows, [true; 5]);
    }

    #[test]
    fn regions_with_loop_off_agree_whatever_their_stale_points() {
        let i = inst(
            "<region> sample=a.wav loop_mode=no_loop \
             <region> sample=a.wav loop_mode=one_shot loop_start=3 loop_end=9",
        );
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.wav");
        let mut b = bind(&i, dir.path());
        complete(&mut b, &i, &[Some((wav(), region_edit(&i, 0, &wav())))]);
        assert_eq!(b.samples[0].follows, [true, true]);
        // The same against a sample that loops: their playback mode keeps them unlooped.
        let w = EditState {
            root: 60,
            cents: 0.0,
            ..looped(LoopMode::Forward, 100, 200)
        };
        complete(&mut b, &i, &[Some((w.clone(), w))]);
        assert_eq!(b.samples[0].follows, [true, true]);
    }

    #[test]
    fn a_region_without_a_loop_mode_that_does_not_loop_keeps_its_own_loop() {
        // The WAV loops, but this region's loop points are impossible, so it plays unlooped.
        let i = inst("<region> sample=a.wav pitch_keycenter=64 tune=-3 loop_start=900 loop_end=10");
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.wav");
        let mut b = bind(&i, dir.path());
        let w = looped(LoopMode::Forward, 100, 200);
        complete(&mut b, &i, &[Some((w.clone(), w))]);
        assert_eq!(b.samples[0].follows, [false]);
    }

    /// A one-region instrument written to ask for `w`, the way a new instrument's regions are.
    fn described(w: &EditState) -> Instrument {
        let mut i = inst("<region> sample=a.wav");
        let now = region_edit(&i, 0, w);
        write_back(&mut i, &all_follow(1), &now, w);
        i
    }

    #[test]
    fn a_region_written_for_a_fine_tune_that_rounds_still_follows() {
        let w = EditState {
            root: 60,
            cents: 13.725491,
            ..wav()
        };
        let i = described(&w);
        assert_eq!(value(&i, 0, "tune").as_deref(), Some("-13.7255"));
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.wav");
        let mut b = bind(&i, dir.path());
        complete(&mut b, &i, &[Some((w.clone(), w))]);
        assert_eq!(b.samples[0].follows, [true]);
    }

    #[test]
    fn a_region_written_for_exactly_fifty_cents_sharp_still_follows() {
        let w = EditState {
            root: 60,
            cents: 50.0,
            ..wav()
        };
        let i = described(&w);
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.wav");
        let mut b = bind(&i, dir.path());
        complete(&mut b, &i, &[Some((w.clone(), w))]);
        assert_eq!(b.samples[0].follows, [true]);
    }

    #[test]
    fn a_region_a_cent_away_does_not_follow() {
        let i = inst("<region> sample=a.wav pitch_keycenter=64 tune=-4");
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.wav");
        let mut b = bind(&i, dir.path());
        complete(&mut b, &i, &[Some((wav(), wav()))]);
        assert_eq!(b.samples[0].follows, [false]);
    }

    #[test]
    fn regions_are_compared_with_the_samples_reference_not_the_first_region() {
        let w = looped(LoopMode::Forward, 100, 200);
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.wav");
        let i = inst(
            "<region> sample=a.wav pitch_keycenter=60
<region> sample=a.wav pitch_keycenter=62",
        );
        let reference = region_edit(&i, 0, &w);
        // The first region is gone; the other still keeps its own root.
        let mut j = i.clone();
        j.delete_region(0);
        let mut b = bind(&j, dir.path());
        complete(&mut b, &j, &[Some((w.clone(), reference.clone()))]);
        assert_eq!(b.samples[0].follows, [false]);
        // A region whose loop is off by its playback mode still follows; a loop that
        // differs from the sample's does not.
        let k = inst(
            "<region> sample=a.wav pitch_keycenter=60 loop_mode=one_shot
             <region> sample=a.wav pitch_keycenter=60 loop_mode=no_loop
             <region> sample=a.wav pitch_keycenter=60 loop_mode=loop_continuous loop_start=0
             <region> sample=a.wav pitch_keycenter=60 loop_mode=loop_continuous",
        );
        let mut b = bind(&k, dir.path());
        complete(&mut b, &k, &[Some((w, reference))]);
        assert_eq!(b.samples[0].follows, [true, true, false, true]);
    }

    #[test]
    fn a_no_loop_region_starts_looping_only_when_the_loop_is_turned_on() {
        let text = "<region> sample=a.wav loop_mode=no_loop\n";
        let mut i = inst(text);
        let before = looped(LoopMode::Forward, 100, 200);
        let moved = looped(LoopMode::Forward, 100, 300);
        write_back(&mut i, &all_follow(1), &before, &moved);
        assert_eq!(value(&i, 0, "loop_mode").as_deref(), Some("no_loop"));
        let mut j = inst(text);
        write_back(&mut j, &all_follow(1), &wav(), &moved);
        assert_eq!(
            value(&j, 0, "loop_mode").as_deref(),
            Some("loop_continuous")
        );
    }

    #[test]
    fn untouched_sample_writes_nothing() {
        let text = "<group> volume=-3\n<region> sample=a.wav tune=4 loop_mode=loop_continuous\n";
        let mut i = inst(text);
        let before = region_edit(&i, 0, &looped(LoopMode::Forward, 10, 90));
        let was = write(&i.file);
        write_back(&mut i, &all_follow(1), &before, &before.clone());
        assert_eq!(write(&i.file), was);
        // An edit that is not tuning, loop or trim also writes nothing.
        let after = EditState {
            crossfade: 5,
            ..before.clone()
        };
        write_back(&mut i, &all_follow(1), &before, &after);
        assert_eq!(write(&i.file), was);
    }

    #[test]
    fn root_edit_writes_at_region_level_and_leaves_the_group() {
        let mut i = inst("<group> pitch_keycenter=48 <region> sample=a.wav\n<region> sample=b.wav");
        let before = region_edit(&i, 0, &wav());
        let after = EditState {
            root: 50,
            ..before.clone()
        };
        write_back(&mut i, &all_follow(1), &before, &after);
        assert_eq!(
            i.get(0, "pitch_keycenter"),
            Some(("50".into(), Level::Region))
        );
        assert_eq!(
            i.get(1, "pitch_keycenter"),
            Some(("48".into(), Level::Group))
        );
        // Tune and loop already say the right thing, so they are not added.
        assert_eq!(value(&i, 0, "tune"), None);
        assert_eq!(value(&i, 0, "loop_mode"), None);
    }

    #[test]
    fn turning_the_loop_off_keeps_one_shot() {
        let w = looped(LoopMode::Forward, 100, 200);
        let mut i = inst(
            "<region> loop_mode=one_shot\n<region> loop_mode=loop_continuous\n<region> sample=a.wav",
        );
        let after = EditState {
            loop_mode: LoopMode::Off,
            ..w.clone()
        };
        write_back(&mut i, &all_follow(3), &w, &after);
        assert_eq!(value(&i, 0, "loop_mode").as_deref(), Some("one_shot"));
        assert_eq!(value(&i, 1, "loop_mode").as_deref(), Some("no_loop"));
        // Unset played as the WAV's loop, so turning it off is written.
        assert_eq!(value(&i, 2, "loop_mode").as_deref(), Some("no_loop"));
        assert_eq!(value(&i, 1, "loop_start"), None);
    }

    #[test]
    fn turning_the_loop_on_writes_points_and_direction() {
        let w = wav();
        let mut i = inst("<region> loop_mode=loop_sustain\n<region> sample=a.wav");
        let after = EditState {
            loop_mode: LoopMode::PingPong,
            loop_start: 100,
            loop_end: 300,
            ..w.clone()
        };
        write_back(&mut i, &all_follow(2), &w, &after);
        // Sustain is kept; unset becomes continuous.
        assert_eq!(value(&i, 0, "loop_mode").as_deref(), Some("loop_sustain"));
        assert_eq!(
            value(&i, 1, "loop_mode").as_deref(),
            Some("loop_continuous")
        );
        for r in 0..2 {
            assert_eq!(value(&i, r, "loop_start").as_deref(), Some("100"));
            assert_eq!(value(&i, r, "loop_end").as_deref(), Some("300"));
            assert_eq!(value(&i, r, "loop_type").as_deref(), Some("alternate"));
        }
        // Back to forward: forward is written, since the direction changed.
        let before = after.clone();
        let after = EditState {
            loop_mode: LoopMode::Forward,
            ..after
        };
        let mut j = inst("<region> sample=a.wav\n<region> sample=a.wav loop_type=backward");
        write_back(&mut j, &all_follow(2), &before, &after);
        assert_eq!(value(&j, 0, "loop_type").as_deref(), Some("forward"));
        assert_eq!(value(&j, 1, "loop_type").as_deref(), Some("forward"));
        // Forward to forward writes nothing.
        let mut k = inst("<region> sample=a.wav");
        write_back(
            &mut k,
            &all_follow(1),
            &wav(),
            &looped(LoopMode::Forward, 1, 9),
        );
        assert_eq!(value(&k, 0, "loop_type"), None);
    }

    #[test]
    fn start_trim_shifts_offset_and_clamps() {
        let mut i = inst(
            "<region> offset=8\n<region> offset=300\n\
             <region> loop_mode=loop_continuous loop_start=150 loop_end=900 end=800",
        );
        let before = wav();
        let after = EditState {
            start: 100,
            end: 599,
            ..before.clone()
        };
        let mut b = all_follow(3);
        b.follows[2] = false; // a region keeping its own loop is trimmed too
        write_back(&mut i, &b, &before, &after);
        assert_eq!(value(&i, 0, "offset").as_deref(), Some("0"));
        assert_eq!(value(&i, 1, "offset").as_deref(), Some("200"));
        assert_eq!(value(&i, 2, "loop_start").as_deref(), Some("50"));
        // 900 - 100 is past the trimmed length: 500 frames, last index 499.
        assert_eq!(value(&i, 2, "loop_end").as_deref(), Some("499"));
        assert_eq!(value(&i, 2, "end").as_deref(), Some("499"));
        // No opcode is added where there was none.
        assert_eq!(value(&i, 0, "end"), None);
    }

    #[test]
    fn trim_shifts_inherited_values_into_the_region() {
        let mut i = inst(
            "<group> offset=300 <region> sample=a.wav
<region> sample=b.wav",
        );
        let before = wav();
        let after = EditState {
            start: 100,
            ..before.clone()
        };
        write_back(&mut i, &all_follow(1), &before, &after);
        assert_eq!(i.get(0, "offset"), Some(("200".into(), Level::Region)));
        // The group, and a region of another sample, are unchanged.
        assert_eq!(i.get(1, "offset"), Some(("300".into(), Level::Group)));
    }

    #[test]
    fn region_edit_is_already_what_the_app_would_clamp_to() {
        let w = wav();
        for text in [
            "<region> tune=80",
            "<region> tune=-130",
            "<region> loop_mode=loop_continuous loop_start=500 loop_end=500",
            "<region> loop_mode=loop_continuous loop_start=999 loop_end=999",
            "<region> pitch_keycenter=127 tune=-90",
        ] {
            let e = region_edit(&inst(text), 0, &w);
            assert_eq!(e.clone().clamped(1000), e, "{text}");
        }
    }

    #[test]
    fn whole_semitones_of_tune_move_to_the_root() {
        let w = wav();
        let e = region_edit(&inst("<region> pitch_keycenter=60 tune=80"), 0, &w);
        assert_eq!((e.root, e.cents), (59, 20.0));
        let e = region_edit(&inst("<region> pitch_keycenter=60 tune=-130"), 0, &w);
        assert_eq!((e.root, e.cents), (61, 30.0));
        // Within half a semitone nothing moves.
        let e = region_edit(&inst("<region> pitch_keycenter=60 tune=14"), 0, &w);
        assert_eq!((e.root, e.cents), (60, -14.0));
    }

    #[test]
    fn a_one_frame_loop_becomes_two_frames() {
        let e = region_edit(
            &inst("<region> loop_mode=loop_continuous loop_start=500 loop_end=500"),
            0,
            &wav(),
        );
        assert_eq!(e.loop_mode, LoopMode::Forward);
        assert_eq!((e.loop_start, e.loop_end), (500, 501));
    }

    #[test]
    fn a_wavs_backward_loop_opens_forward_and_a_change_writes_loop_type() {
        let w = looped(LoopMode::Backward, 100, 200);
        let mut i = inst("<region> sample=a.wav");
        let before = region_edit(&i, 0, &w);
        assert_eq!(before.loop_mode, LoopMode::Forward);
        // Moving the points keeps forward, which needs no `loop_type`.
        let moved = EditState {
            loop_start: 110,
            ..before.clone()
        };
        write_back(&mut i, &all_follow(1), &before, &moved);
        assert_eq!(value(&i, 0, "loop_type"), None);
        let after = EditState {
            loop_mode: LoopMode::Backward,
            ..moved.clone()
        };
        write_back(&mut i, &all_follow(1), &moved, &after);
        assert_eq!(value(&i, 0, "loop_type").as_deref(), Some("backward"));
        assert_eq!(region_edit(&i, 0, &w).loop_mode, LoopMode::Backward);
        // And back to forward is written, as the direction read on opening was backward.
        let mut j = inst("<region> sample=a.wav loop_type=backward");
        let before = region_edit(&j, 0, &w);
        let after = EditState {
            loop_mode: LoopMode::Forward,
            ..before.clone()
        };
        write_back(&mut j, &all_follow(1), &before, &after);
        assert_eq!(value(&j, 0, "loop_type").as_deref(), Some("forward"));
    }

    #[test]
    fn end_trim_alone_clamps_without_shifting() {
        let mut i = inst("<region> loop_end=900 offset=10");
        let before = wav();
        let after = EditState {
            end: 499,
            ..before.clone()
        };
        write_back(&mut i, &all_follow(1), &before, &after);
        assert_eq!(value(&i, 0, "loop_end").as_deref(), Some("499"));
        assert_eq!(value(&i, 0, "offset").as_deref(), Some("10"));
    }

    #[test]
    fn sample_edits_never_rewrite_one_shot() {
        let mut i = inst(
            "<region> sample=a.wav pitch_keycenter=60 loop_mode=one_shot\n\
             <region> sample=a.wav pitch_keycenter=60 loop_mode=no_loop\n\
             <region> sample=a.wav pitch_keycenter=60 loop_mode=loop_sustain",
        );
        let w = wav();
        let before = region_edit(&i, 0, &w);
        let after = EditState {
            root: 62,
            loop_mode: LoopMode::Forward,
            loop_start: 100,
            loop_end: 300,
            ..before.clone()
        };
        write_back(&mut i, &all_follow(3), &before, &after);
        assert_eq!(value(&i, 0, "loop_mode").as_deref(), Some("one_shot"));
        assert_eq!(value(&i, 0, "pitch_keycenter").as_deref(), Some("62"));
        assert_eq!(value(&i, 0, "loop_start").as_deref(), Some("100"));
        // A region that did not loop now plays the sample's loop.
        assert_eq!(
            value(&i, 1, "loop_mode").as_deref(),
            Some("loop_continuous")
        );
        assert_eq!(value(&i, 2, "loop_mode").as_deref(), Some("loop_sustain"));
    }

    #[test]
    fn a_loop_only_edit_leaves_folded_tuning_alone() {
        let mut i = inst("<region> sample=a.wav pitch_keycenter=60 tune=80");
        let before = region_edit(&i, 0, &wav());
        assert_eq!((before.root, before.cents), (59, 20.0));
        let after = EditState {
            loop_mode: LoopMode::Forward,
            loop_start: 100,
            loop_end: 300,
            ..before.clone()
        };
        write_back(&mut i, &all_follow(1), &before, &after);
        assert_eq!(value(&i, 0, "pitch_keycenter").as_deref(), Some("60"));
        assert_eq!(value(&i, 0, "tune").as_deref(), Some("80"));
        assert_eq!(value(&i, 0, "loop_start").as_deref(), Some("100"));
        // A tuning-only edit leaves the loop opcodes alone.
        let mut j = inst("<region> sample=a.wav");
        let before = region_edit(&j, 0, &looped(LoopMode::Forward, 10, 90));
        let after = EditState {
            cents: 5.0,
            ..before.clone()
        };
        write_back(&mut j, &all_follow(1), &before, &after);
        assert_eq!(value(&j, 0, "tune").as_deref(), Some("-5"));
        assert_eq!(value(&j, 0, "loop_mode"), None);
        assert_eq!(value(&j, 0, "loop_start"), None);
    }

    #[test]
    fn pitch_keycenter_keeps_the_files_spelling() {
        for (text, want) in [
            ("<region> pitch_keycenter=c4", "D4"),
            ("<region> key=c4", "D4"),
            ("<group> pitch_keycenter=c4 <region> sample=a.wav", "D4"),
            ("<region> pitch_keycenter=60", "62"),
            ("<region> pitch_keycenter=sample", "62"),
            ("<region> sample=a.wav", "62"),
        ] {
            let mut i = inst(text);
            let before = region_edit(&i, 0, &wav());
            let after = EditState {
                root: 62,
                ..before.clone()
            };
            write_back(&mut i, &all_follow(1), &before, &after);
            assert_eq!(
                value(&i, 0, "pitch_keycenter").as_deref(),
                Some(want),
                "{text}"
            );
        }
    }

    #[test]
    fn trim_and_loop_move_together_write_the_new_points_once() {
        let mut i = inst("<region> loop_mode=loop_continuous loop_start=100 loop_end=200");
        let before = looped(LoopMode::Forward, 100, 200);
        let after = EditState {
            start: 50,
            loop_start: 120,
            loop_end: 220,
            ..before.clone()
        };
        write_back(&mut i, &all_follow(1), &before, &after);
        assert_eq!(value(&i, 0, "loop_start").as_deref(), Some("70"));
        assert_eq!(value(&i, 0, "loop_end").as_deref(), Some("170"));
    }
}
