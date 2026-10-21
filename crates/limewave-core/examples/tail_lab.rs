//! Truncation test bench for Generate tail: cuts a WAV file short on purpose, renders tails,
//! writes WAVs to listen to and prints the measurements (v0.3 §8.3).
//!
//! ```text
//! cargo run --release -p limewave-core --example tail_lab -- <file.wav> --cut <seconds> \
//!     --mode decay|sustain --method smooth|harmonic|granular|all \
//!     [--body <s>] [--decay <s>] [--release <s>] [--darken <0..1>] [--seed <n>] [--out <dir>]
//! ```
//!
//! The files go to `target/tail-lab/<name>/` (or `--out`): `original.wav`, `cut.wav`,
//! `tail_<method>.wav` and, for sustain, `tail_<method>_heard.wav`. All the work is in
//! `limewave_core::tail::measure::lab_run`; this is only the argument parser.

use std::path::PathBuf;
use std::process::ExitCode;

use limewave_core::tail::measure::{LabArgs, lab_run};
use limewave_core::tail::{TailMethod, TailMode, TailSettings};

const USAGE: &str = "usage: tail_lab <file.wav> --cut <seconds> --mode decay|sustain \
--method smooth|harmonic|granular|all [--body <s>] [--decay <s>] [--release <s>] \
[--darken <0..1>] [--seed <n>] [--out <dir>]";

/// The value after the flag `name`.
fn value(it: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{name} needs a value"))
}

/// The number after the flag `name`.
fn number<T: std::str::FromStr>(
    it: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<T, String> {
    let v = value(it, name)?;
    v.parse()
        .map_err(|_| format!("{name}: {v:?} is not a number"))
}

fn parse(args: Vec<String>) -> Result<(PathBuf, LabArgs, Option<PathBuf>), String> {
    let mut file = None;
    let mut cut_s = None;
    let mut mode = None;
    let mut methods = None;
    let mut out = None;
    let mut settings = TailSettings::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--cut" => cut_s = Some(number(&mut it, "--cut")?),
            "--mode" => {
                mode = Some(match value(&mut it, "--mode")?.as_str() {
                    "decay" => TailMode::Decay,
                    "sustain" => TailMode::Sustain,
                    other => return Err(format!("--mode: {other:?} is not decay or sustain")),
                });
            }
            "--method" => {
                methods = Some(match value(&mut it, "--method")?.as_str() {
                    "granular" => vec![TailMethod::Granular],
                    "smooth" => vec![TailMethod::Smooth],
                    "harmonic" => vec![TailMethod::Harmonic],
                    "all" => vec![
                        TailMethod::Granular,
                        TailMethod::Smooth,
                        TailMethod::Harmonic,
                    ],
                    other => return Err(format!("--method: {other:?} is not a method")),
                });
            }
            "--body" => settings.body_s = number(&mut it, "--body")?,
            "--decay" => settings.decay_t60 = Some(number(&mut it, "--decay")?),
            "--release" => settings.release_t60 = Some(number(&mut it, "--release")?),
            "--darken" => settings.darken = number(&mut it, "--darken")?,
            "--seed" => settings.seed = number(&mut it, "--seed")?,
            "--out" => out = Some(PathBuf::from(value(&mut it, "--out")?)),
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ if file.is_none() => file = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument {arg:?}")),
        }
    }
    let lab = LabArgs {
        cut_s: cut_s.ok_or("--cut is required")?,
        mode: mode.ok_or("--mode is required")?,
        methods: methods.ok_or("--method is required")?,
        settings,
    };
    Ok((file.ok_or("no input file")?, lab, out))
}

fn main() -> ExitCode {
    let result = parse(std::env::args().skip(1).collect()).and_then(|(file, lab, out)| {
        let stem = file
            .file_stem()
            .map_or_else(|| "sample".into(), |s| s.to_string_lossy().into_owned());
        let dir = out.unwrap_or_else(|| PathBuf::from("target/tail-lab").join(stem));
        lab_run(&file, &lab, &dir)
    });
    match result {
        Ok(table) => {
            print!("{table}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("tail_lab: {e}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}
