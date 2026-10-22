//! The SFZ instrument model: regions with their inherited settings, read and
//! edited directly on the token stream so unknown text survives a save.

use std::ops::Range;

use super::parse::{HeaderKind, SfzFile, Token, expand};
use crate::notes::parse_note;

/// Where an effective opcode was found, nearest first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Region,
    Group,
    Master,
    Global,
}

/// A region's `pitch_keycenter`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Keycenter {
    Note(u8),
    /// `pitch_keycenter=sample`: use the pitch stored in the WAV.
    Sample,
}

/// An SFZ `loop_mode`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SfzLoopMode {
    NoLoop,
    OneShot,
    Continuous,
    Sustain,
}

/// An SFZ `loop_type`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LoopDir {
    Forward,
    Backward,
    Alternate,
}

/// Amplitude envelope: times in seconds, `sustain` in percent.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Ampeg {
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
}

/// A region's effective settings after inheritance, with SFZ defaults filled in.
#[derive(Clone, PartialEq, Debug)]
pub struct RegionInfo {
    pub sample: Option<String>,
    pub lokey: u8,
    pub hikey: u8,
    pub lovel: u8,
    pub hivel: u8,
    pub keycenter: Keycenter,
    pub tune: f32,
    pub transpose: i32,
    pub volume: f32,
    pub amp_veltrack: f32,
    pub offset: usize,
    pub end: Option<usize>,
    pub loop_mode: Option<SfzLoopMode>,
    pub loop_dir: LoopDir,
    pub loop_start: Option<usize>,
    pub loop_end: Option<usize>,
    pub ampeg: Ampeg,
    /// False when `trigger` is anything but `attack` (release triggers etc.).
    pub attack_trigger: bool,
}

/// Opcodes the editor understands, so `other_settings` leaves them out.
const KNOWN: [&str; 25] = [
    "sample",
    "lokey",
    "hikey",
    "key",
    "lovel",
    "hivel",
    "pitch_keycenter",
    "tune",
    "transpose",
    "volume",
    "amp_veltrack",
    "offset",
    "end",
    "loop_mode",
    "loopmode",
    "loop_type",
    "loop_start",
    "loopstart",
    "loop_end",
    "loopend",
    "ampeg_attack",
    "ampeg_decay",
    "ampeg_sustain",
    "ampeg_release",
    "trigger",
];

/// The one spelling Limewave treats an opcode under.
fn canonical(name: &str) -> &str {
    match name {
        "loopmode" => "loop_mode",
        "loopstart" => "loop_start",
        "loopend" => "loop_end",
        other => other,
    }
}

/// Does opcode `tok` supply the value of canonical opcode `canon`?
fn supplies(canon: &str, tok: &str) -> bool {
    canonical(tok) == canon
        || (tok == "key" && matches!(canon, "lokey" | "hikey" | "pitch_keycenter"))
}

fn is_scope_header(t: &Token) -> bool {
    matches!(
        t,
        Token::Header(
            HeaderKind::Group | HeaderKind::Master | HeaderKind::Global | HeaderKind::Control
        )
    )
}

fn parse_vel(s: &str) -> Option<u8> {
    s.trim().parse::<u8>().ok().filter(|v| *v <= 127)
}

fn parse_f32(s: &str) -> Option<f32> {
    s.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

fn parse_time(s: &str) -> Option<f32> {
    parse_f32(s).filter(|v| *v >= 0.0)
}

fn parse_frame(s: &str) -> Option<usize> {
    s.trim().parse::<usize>().ok()
}

/// An SFZ instrument: the token stream plus the views the editor needs.
#[derive(Clone, PartialEq, Debug)]
pub struct Instrument {
    pub file: SfzFile,
}

impl Instrument {
    pub fn new(file: SfzFile) -> Instrument {
        Instrument { file }
    }

    fn header_indices(&self, want: &HeaderKind) -> Vec<usize> {
        self.file
            .tokens
            .iter()
            .enumerate()
            .filter(|(_, t)| matches!(t, Token::Header(k) if k == want))
            .map(|(i, _)| i)
            .collect()
    }

    /// Token index of each `<region>` header, in order; "region n" is `regions()[n]`.
    pub fn regions(&self) -> Vec<usize> {
        self.header_indices(&HeaderKind::Region)
    }

    /// Token index of each `<group>` header.
    pub fn groups(&self) -> Vec<usize> {
        self.header_indices(&HeaderKind::Group)
    }

    /// Index of the first header after token `h`, else the token count.
    fn span_end(&self, h: usize) -> usize {
        self.file.tokens[h + 1..]
            .iter()
            .position(|t| matches!(t, Token::Header(_)))
            .map_or(self.file.tokens.len(), |p| h + 1 + p)
    }

    /// Where a group's regions end: the next scope header, else the file end.
    fn group_end(&self, group: usize) -> usize {
        let start = self.groups()[group];
        self.file.tokens[start + 1..]
            .iter()
            .position(is_scope_header)
            .map_or(self.file.tokens.len(), |p| start + 1 + p)
    }

    /// A region's header index, and the header indices of its group, master
    /// and global (a later master or global ends an earlier group).
    fn ancestors(&self, region: usize) -> Option<(usize, [Option<usize>; 3])> {
        let h = *self.regions().get(region)?;
        let (mut group, mut master, mut global) = (None, None, None);
        for i in (0..h).rev() {
            match &self.file.tokens[i] {
                Token::Header(HeaderKind::Group)
                    if group.is_none() && master.is_none() && global.is_none() =>
                {
                    group = Some(i);
                }
                Token::Header(HeaderKind::Master) if master.is_none() && global.is_none() => {
                    master = Some(i);
                }
                Token::Header(HeaderKind::Global) if global.is_none() => global = Some(i),
                _ => {}
            }
        }
        Some((h, [group, master, global]))
    }

    /// Region n -> group number (index into `groups()`); `None` when no group applies.
    pub fn group_of(&self, region: usize) -> Option<usize> {
        let (_, [group, _, _]) = self.ancestors(region)?;
        self.groups().iter().position(|&g| Some(g) == group)
    }

    /// The token ranges holding a region's settings, nearest level first.
    fn levels(&self, region: usize) -> Vec<(Level, Range<usize>)> {
        let Some((h, [group, master, global])) = self.ancestors(region) else {
            return Vec::new();
        };
        let mut out = vec![(Level::Region, h + 1..self.span_end(h))];
        for (level, header) in [
            (Level::Group, group),
            (Level::Master, master),
            (Level::Global, global),
        ] {
            if let Some(i) = header {
                out.push((level, i + 1..self.span_end(i)));
            }
        }
        out
    }

    fn defines(&self) -> Vec<(String, String)> {
        self.file
            .tokens
            .iter()
            .filter_map(|t| match t {
                Token::Define { name, value } => Some((name.clone(), value.clone())),
                _ => None,
            })
            .collect()
    }

    /// The value `canon` has at each level that sets it, nearest first, expanded.
    fn level_values(&self, region: usize, canon: &str) -> Vec<(String, Level)> {
        let defines = self.defines();
        self.levels(region)
            .into_iter()
            .filter_map(|(level, range)| {
                self.file.tokens[range]
                    .iter()
                    .rev()
                    .find_map(|t| match t {
                        Token::Opcode { name, value } if supplies(canon, name) => {
                            Some(expand(value, &defines))
                        }
                        _ => None,
                    })
                    .map(|v| (v, level))
            })
            .collect()
    }

    /// The effective value of an opcode (aliases understood, `#define`s
    /// expanded) and the level it came from.
    pub fn get(&self, region: usize, name: &str) -> Option<(String, Level)> {
        self.level_values(region, canonical(name))
            .into_iter()
            .next()
    }

    /// The nearest level whose value parses.
    fn pick<T>(&self, region: usize, name: &str, parse: impl Fn(&str) -> Option<T>) -> Option<T> {
        self.level_values(region, name)
            .iter()
            .find_map(|(v, _)| parse(v))
    }

    /// A region's effective settings with SFZ defaults filled in.
    pub fn region_info(&self, region: usize) -> RegionInfo {
        let float = |name: &str, default: f32, parse: fn(&str) -> Option<f32>| {
            self.pick(region, name, parse).unwrap_or(default)
        };
        RegionInfo {
            sample: self.pick(region, "sample", |s| {
                let s = s.trim();
                (!s.is_empty()).then(|| s.to_owned())
            }),
            lokey: self.pick(region, "lokey", parse_note).unwrap_or(0),
            hikey: self.pick(region, "hikey", parse_note).unwrap_or(127),
            lovel: self.pick(region, "lovel", parse_vel).unwrap_or(1),
            hivel: self.pick(region, "hivel", parse_vel).unwrap_or(127),
            keycenter: self
                .pick(region, "pitch_keycenter", |s| {
                    if s.trim().eq_ignore_ascii_case("sample") {
                        Some(Keycenter::Sample)
                    } else {
                        parse_note(s).map(Keycenter::Note)
                    }
                })
                .unwrap_or(Keycenter::Note(60)),
            tune: float("tune", 0.0, parse_f32),
            transpose: self
                .pick(region, "transpose", |s| s.trim().parse::<i32>().ok())
                .unwrap_or(0),
            volume: float("volume", 0.0, parse_f32),
            amp_veltrack: float("amp_veltrack", 100.0, parse_f32),
            offset: self.pick(region, "offset", parse_frame).unwrap_or(0),
            end: self.pick(region, "end", parse_frame),
            loop_mode: self.pick(region, "loop_mode", |s| match s.trim() {
                "no_loop" => Some(SfzLoopMode::NoLoop),
                "one_shot" => Some(SfzLoopMode::OneShot),
                "loop_continuous" => Some(SfzLoopMode::Continuous),
                "loop_sustain" => Some(SfzLoopMode::Sustain),
                _ => None,
            }),
            loop_dir: self
                .pick(region, "loop_type", |s| match s.trim() {
                    "forward" => Some(LoopDir::Forward),
                    "backward" => Some(LoopDir::Backward),
                    "alternate" => Some(LoopDir::Alternate),
                    _ => None,
                })
                .unwrap_or(LoopDir::Forward),
            loop_start: self.pick(region, "loop_start", parse_frame),
            loop_end: self.pick(region, "loop_end", parse_frame),
            ampeg: Ampeg {
                attack: float("ampeg_attack", 0.0, parse_time),
                decay: float("ampeg_decay", 0.0, parse_time),
                sustain: self
                    .pick(region, "ampeg_sustain", |s| {
                        parse_f32(s).filter(|v| (0.0..=100.0).contains(v))
                    })
                    .unwrap_or(100.0),
                release: float("ampeg_release", 0.0, parse_time),
            },
            attack_trigger: self
                .get(region, "trigger")
                .is_none_or(|(v, _)| v.trim() == "attack"),
        }
    }

    /// Sets an opcode on the region itself, in place if it has one (keeping its
    /// spelling) or appended to the region. Inherited levels are never touched.
    pub fn set(&mut self, region: usize, name: &str, value: &str) {
        let Some(&h) = self.regions().get(region) else {
            return;
        };
        let canon = canonical(name);
        let mut end = self.span_end(h);
        if matches!(canon, "lokey" | "hikey" | "pitch_keycenter") {
            let key = (h + 1..end).rev().find_map(|i| match &self.file.tokens[i] {
                Token::Opcode { name, value } if name == "key" => Some((i, value.clone())),
                _ => None,
            });
            if let Some((i, v)) = key {
                let op = |n: &str| Token::Opcode {
                    name: n.to_owned(),
                    value: v.clone(),
                };
                let split = [op("lokey"), op("hikey"), op("pitch_keycenter")];
                self.file.tokens.splice(i..=i, split);
                end += 2;
            }
        }
        let existing = (h + 1..end).rev().find(|&i| {
            matches!(&self.file.tokens[i], Token::Opcode { name, .. }
                if canonical(name) == canon)
        });
        match existing {
            Some(i) => {
                if let Token::Opcode { value: v, .. } = &mut self.file.tokens[i] {
                    *v = value.to_owned();
                }
            }
            None => self.file.tokens.insert(
                end,
                Token::Opcode {
                    name: canon.to_owned(),
                    value: value.to_owned(),
                },
            ),
        }
    }

    /// Appends an empty `<group>` and returns its group number.
    pub fn add_group(&mut self) -> usize {
        self.file.tokens.push(Token::Header(HeaderKind::Group));
        self.groups().len() - 1
    }

    /// Inserts a region at the end of `group` (or the end of the file) and
    /// returns its region number.
    pub fn add_region(&mut self, group: Option<usize>, sample: &str, key: u8) -> usize {
        let at = match group {
            Some(g) if g < self.groups().len() => self.group_end(g),
            _ => self.file.tokens.len(),
        };
        let op = |n: &str, v: String| Token::Opcode {
            name: n.to_owned(),
            value: v,
        };
        let new = [
            Token::Header(HeaderKind::Region),
            op("sample", sample.to_owned()),
            op("pitch_keycenter", key.to_string()),
            op("lokey", key.to_string()),
            op("hikey", key.to_string()),
        ];
        self.file.tokens.splice(at..at, new);
        self.regions().iter().filter(|&&r| r < at).count()
    }

    /// Takes a region's header and the tokens after it out of the file. A
    /// `#define` is file-wide, so it stays where it was.
    fn take_region(&mut self, h: usize) -> Vec<Token> {
        let end = self.span_end(h);
        let mut taken = Vec::new();
        let mut at = 0;
        self.file.tokens.retain_mut(|t| {
            let inside = (h..end).contains(&at);
            at += 1;
            if inside && !matches!(t, Token::Define { .. }) {
                taken.push(t.clone());
                false
            } else {
                true
            }
        });
        taken
    }

    /// Removes a region's header and the tokens after it up to the next
    /// header, comments included. `#define`s in that span stay.
    pub fn delete_region(&mut self, region: usize) {
        if let Some(&h) = self.regions().get(region) {
            self.take_region(h);
        }
    }

    /// Moves a region to the end of `group` and returns its new region number.
    pub fn move_region(&mut self, region: usize, group: usize) -> usize {
        let (Some(&h), true) = (self.regions().get(region), group < self.groups().len()) else {
            return region;
        };
        let moved = self.take_region(h);
        let at = self.group_end(group);
        self.file.tokens.splice(at..at, moved);
        self.regions().iter().filter(|&&r| r < at).count()
    }

    /// Effective opcodes the editor has no control for, nearest level winning:
    /// the region's first, then group, master and global, each in file order.
    pub fn other_settings(&self, region: usize) -> Vec<(String, String, Level)> {
        let defines = self.defines();
        let mut out: Vec<(String, String, Level)> = Vec::new();
        for (level, range) in self.levels(region) {
            let nearer = out.len();
            for t in &self.file.tokens[range] {
                let Token::Opcode { name, value } = t else {
                    continue;
                };
                if KNOWN.contains(&name.as_str()) {
                    continue;
                }
                let value = expand(value, &defines);
                match out.iter().position(|(n, _, _)| n == name) {
                    // Set again within this level: the later one wins.
                    Some(p) if p >= nearer => out[p].1 = value,
                    // Already set at a nearer level.
                    Some(_) => {}
                    None => out.push((name.clone(), value, level)),
                }
            }
        }
        out
    }

    /// What `<group>` number `group` itself sets for `name` (aliases understood, `#define`s
    /// expanded); nothing is inherited from master or global.
    pub fn group_get(&self, group: usize, name: &str) -> Option<String> {
        let h = *self.groups().get(group)?;
        let canon = canonical(name);
        let defines = self.defines();
        self.file.tokens[h + 1..self.span_end(h)]
            .iter()
            .rev()
            .find_map(|t| match t {
                Token::Opcode { name, value } if supplies(canon, name) => {
                    Some(expand(value, &defines))
                }
                _ => None,
            })
    }

    /// The `<control>` `default_path`, expanded; empty when there is none.
    pub fn default_path(&self) -> String {
        let defines = self.defines();
        let mut path = String::new();
        let mut in_control = false;
        for t in &self.file.tokens {
            match t {
                Token::Header(k) => in_control = *k == HeaderKind::Control,
                Token::Opcode { name, value } if in_control && name == "default_path" => {
                    path = expand(value, &defines);
                }
                _ => {}
            }
        }
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sfz::parse::parse;

    fn inst(text: &str) -> Instrument {
        Instrument::new(parse(text).unwrap())
    }

    fn op(name: &str, value: &str) -> Token {
        Token::Opcode {
            name: name.into(),
            value: value.into(),
        }
    }

    #[test]
    fn group_get_reads_only_the_group_itself() {
        let i = inst(
            "<global> lovel=9 <group> group_label=Soft lovel=2 hivel=50 <region> sample=a.wav <group> <region> sample=b.wav",
        );
        assert_eq!(i.group_get(0, "group_label").as_deref(), Some("Soft"));
        assert_eq!(i.group_get(0, "lovel").as_deref(), Some("2"));
        assert_eq!(i.group_get(0, "hivel").as_deref(), Some("50"));
        assert_eq!(
            i.group_get(1, "lovel"),
            None,
            "global's lovel is not the group's"
        );
        assert_eq!(i.group_get(2, "lovel"), None);
    }

    #[test]
    fn region_inherits_group_master_global() {
        let i = inst(
            "<global> volume=-3 <master> tune=5 <group> lokey=40 <region> sample=a.wav hikey=50",
        );
        let r = i.region_info(0);
        assert_eq!(r.volume, -3.0);
        assert_eq!(r.tune, 5.0);
        assert_eq!(r.lokey, 40);
        assert_eq!(r.hikey, 50);
        assert_eq!(r.sample.as_deref(), Some("a.wav"));
        assert_eq!(i.get(0, "lokey").unwrap().1, Level::Group);
        assert_eq!(i.get(0, "tune").unwrap().1, Level::Master);
        assert_eq!(i.get(0, "volume").unwrap().1, Level::Global);
        assert_eq!(i.get(0, "hikey").unwrap().1, Level::Region);
    }

    #[test]
    fn defaults_apply_when_nothing_is_set() {
        let r = inst("<region> sample=a.wav").region_info(0);
        assert_eq!((r.lokey, r.hikey, r.lovel, r.hivel), (0, 127, 1, 127));
        assert_eq!(r.keycenter, Keycenter::Note(60));
        assert_eq!(r.amp_veltrack, 100.0);
        assert_eq!(r.ampeg.sustain, 100.0);
        assert_eq!(r.loop_mode, None);
        assert_eq!(r.loop_dir, LoopDir::Forward);
        assert!(r.attack_trigger);
        assert_eq!((r.offset, r.end), (0, None));
    }

    #[test]
    fn key_sets_range_and_center() {
        let r = inst("<region> sample=a.wav key=c4").region_info(0);
        assert_eq!((r.lokey, r.hikey), (60, 60));
        assert_eq!(r.keycenter, Keycenter::Note(60));
        let r = inst("<region> key=60 hikey=64").region_info(0);
        assert_eq!((r.lokey, r.hikey), (60, 64));
        let r = inst("<region> pitch_keycenter=sample").region_info(0);
        assert_eq!(r.keycenter, Keycenter::Sample);
    }

    #[test]
    fn bad_values_fall_back_to_inherited_then_default() {
        let i = inst("<group> lokey=40 volume=-2 <region> lokey=999 volume=loud hivel=200");
        let r = i.region_info(0);
        assert_eq!(r.lokey, 40);
        assert_eq!(r.volume, -2.0);
        assert_eq!(r.hivel, 127);
    }

    #[test]
    fn loop_aliases_and_modes() {
        let r =
            inst("<region> loopmode=loop_continuous loopstart=10 loopend=99 loop_type=alternate")
                .region_info(0);
        assert_eq!(r.loop_mode, Some(SfzLoopMode::Continuous));
        assert_eq!((r.loop_start, r.loop_end), (Some(10), Some(99)));
        assert_eq!(r.loop_dir, LoopDir::Alternate);
        let r = inst("<region> trigger=release ampeg_release=0.5").region_info(0);
        assert!(!r.attack_trigger);
        assert_eq!(r.ampeg.release, 0.5);
    }

    #[test]
    fn set_writes_at_region_level_only() {
        let mut i = inst("<group> lokey=40 <region> sample=a.wav // c\n<region> sample=b.wav");
        i.set(0, "lokey", "50");
        assert_eq!(i.get(0, "lokey"), Some(("50".into(), Level::Region)));
        assert_eq!(i.file.tokens[1], op("lokey", "40"));
        // Appended after the region's last token, before the next header.
        assert_eq!(i.file.tokens[5], op("lokey", "50"));
        assert_eq!(i.file.tokens[6], Token::Header(HeaderKind::Region));
        assert_eq!(i.get(1, "lokey"), Some(("40".into(), Level::Group)));
    }

    #[test]
    fn set_edits_in_place_and_keeps_alias_spelling() {
        let mut i = inst("<region> loopstart=10 hikey=5");
        i.set(0, "loop_start", "20");
        assert_eq!(i.file.tokens[1], op("loopstart", "20"));
        assert_eq!(i.file.tokens.len(), 3);
    }

    #[test]
    fn set_on_key_splits_it() {
        let mut i = inst("<region> sample=a.wav key=60 volume=1");
        i.set(0, "hikey", "64");
        assert_eq!(
            i.file.tokens[2..5],
            [
                op("lokey", "60"),
                op("hikey", "64"),
                op("pitch_keycenter", "60")
            ]
        );
        assert_eq!(i.file.tokens[5], op("volume", "1"));
        let r = i.region_info(0);
        assert_eq!((r.lokey, r.hikey), (60, 64));
    }

    #[test]
    fn defines_expand_on_read() {
        let i = inst("#define $V -6\n<region> volume=$V");
        assert_eq!(i.region_info(0).volume, -6.0);
        assert_eq!(i.get(0, "volume").unwrap().0, "-6");
        let i = inst("#define $K 60\n#define $K 62\n<region> key=$K");
        assert_eq!(i.region_info(0).lokey, 62);
    }

    fn two_by_two() -> Instrument {
        inst(
            "<group> <region> sample=a.wav <region> sample=b.wav \
             <group> <region> sample=c.wav <region> sample=d.wav",
        )
    }

    fn samples(i: &Instrument) -> Vec<String> {
        (0..i.regions().len())
            .map(|r| i.region_info(r).sample.unwrap())
            .collect()
    }

    #[test]
    fn add_delete_move_regions() {
        let mut i = two_by_two();
        assert_eq!(i.regions().len(), 4);
        assert_eq!(
            [0, 1, 2, 3].map(|r| i.group_of(r)),
            [Some(0), Some(0), Some(1), Some(1)]
        );

        // Added at the end of group 0, before group 1.
        let n = i.add_region(Some(0), "e.wav", 64);
        assert_eq!(n, 2);
        assert_eq!(samples(&i), ["a.wav", "b.wav", "e.wav", "c.wav", "d.wav"]);
        assert_eq!(i.group_of(2), Some(0));
        let r = i.region_info(2);
        assert_eq!(
            (r.lokey, r.hikey, r.keycenter),
            (64, 64, Keycenter::Note(64))
        );

        i.delete_region(0);
        assert_eq!(samples(&i), ["b.wav", "e.wav", "c.wav", "d.wav"]);

        // Move b into group 1: it lands after d.
        let n = i.move_region(0, 1);
        assert_eq!(n, 3);
        assert_eq!(samples(&i), ["e.wav", "c.wav", "d.wav", "b.wav"]);
        assert_eq!(
            [0, 1, 2, 3].map(|r| i.group_of(r)),
            [Some(0), Some(1), Some(1), Some(1)]
        );

        // Move c back to group 0, which is earlier in the file.
        let n = i.move_region(1, 0);
        assert_eq!(n, 1);
        assert_eq!(samples(&i), ["e.wav", "c.wav", "d.wav", "b.wav"]);
        assert_eq!(i.group_of(1), Some(0));
        assert_eq!(i.group_of(2), Some(1));
    }

    #[test]
    fn delete_keeps_defines_between_regions() {
        let mut i = inst("<region> sample=a.wav\n#define $X 3\n<region> volume=$X");
        i.delete_region(0);
        assert_eq!(i.regions().len(), 1);
        assert_eq!(i.region_info(0).volume, 3.0);
        assert_eq!(i.file.tokens.len(), 3);
    }

    #[test]
    fn move_keeps_defines_in_place() {
        let mut i = inst(
            "<group> <region> sample=a.wav\n#define $X 3\n<region> volume=$X \
             <group> <region> sample=c.wav",
        );
        let n = i.move_region(0, 1);
        assert_eq!(n, 2);
        // The define stays before the region that uses it, outside the moved span.
        assert_eq!(
            i.file.tokens[..4],
            [
                Token::Header(HeaderKind::Group),
                Token::Define {
                    name: "$X".into(),
                    value: "3".into()
                },
                Token::Header(HeaderKind::Region),
                op("volume", "$X"),
            ]
        );
        assert_eq!(i.region_info(0).volume, 3.0);
        assert_eq!(i.region_info(2).sample.as_deref(), Some("a.wav"));
        assert_eq!(i.group_of(2), Some(1));
    }

    #[test]
    fn groups_and_loose_regions() {
        let mut i = inst("<region> sample=a.wav");
        assert_eq!(i.group_of(0), None);
        let g = i.add_group();
        assert_eq!(g, 0);
        let n = i.add_region(Some(g), "b.wav", 60);
        assert_eq!(n, 1);
        let n = i.add_region(None, "c.wav", 61);
        assert_eq!(n, 2);
        assert_eq!([0, 1, 2].map(|r| i.group_of(r)), [None, Some(0), Some(0)]);
    }

    #[test]
    fn delete_takes_trailing_comments() {
        let mut i = inst("<region> sample=a.wav // a\n<region> sample=b.wav");
        i.delete_region(0);
        assert_eq!(
            i.file.tokens,
            vec![Token::Header(HeaderKind::Region), op("sample", "b.wav")]
        );
    }

    #[test]
    fn a_master_ends_the_previous_group() {
        let i = inst("<group> lokey=40 <master> tune=2 <region> sample=a.wav");
        assert_eq!(i.group_of(0), None);
        assert_eq!(i.region_info(0).lokey, 0);
        assert_eq!(i.region_info(0).tune, 2.0);
    }

    #[test]
    fn other_settings_lists_unknown_opcodes_with_levels() {
        let i = inst(
            "<group> ampeg_sustain=99.5 cutoff=500 pan=10 \
             <region> sample=a.wav pitcheg_attack=1 pan=-5 volume=2",
        );
        assert_eq!(
            i.other_settings(0),
            vec![
                ("pitcheg_attack".into(), "1".into(), Level::Region),
                ("pan".into(), "-5".into(), Level::Region),
                ("cutoff".into(), "500".into(), Level::Group),
            ]
        );
        assert_eq!(i.region_info(0).ampeg.sustain, 99.5);
    }

    #[test]
    fn default_path_reads_the_control_header() {
        let i = inst("<control> default_path=Samples\\ <region> sample=a.wav");
        assert_eq!(i.default_path(), "Samples\\");
        assert_eq!(inst("<region> sample=a.wav").default_path(), "");
        // A region does not inherit control opcodes.
        assert!(i.get(0, "default_path").is_none());
    }

    #[test]
    fn out_of_range_regions_are_harmless() {
        let mut i = inst("<region> sample=a.wav");
        assert!(i.get(5, "sample").is_none());
        i.set(5, "volume", "1");
        i.delete_region(5);
        assert_eq!(i.move_region(5, 0), 5);
        assert_eq!(i.region_info(5).lokey, 0);
        assert_eq!(i.file.tokens.len(), 2);
    }
}
