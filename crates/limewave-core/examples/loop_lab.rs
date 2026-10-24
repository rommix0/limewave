//! Loop finder bench: searches a WAV file for forward loops, prints the candidates beside the
//! file's stored loop and writes each loop out to listen to (v0.8).
//!
//! ```text
//! cargo run --release -p limewave-core --example loop_lab -- <file.wav> [--keep-end <frame>] \
//!     [--range <first> <last>] [--min <s>] [--max <s>] [--preference <0..1>] [--out <dir>]
//! ```
//!
//! The files go to `target/loop-lab/<name>/` (or `--out`): `candidate_<n>.wav` and, when the
//! file has a loop, `stored.wav`, each a second of lead-in and then three passes of the loop.
//! All the work is in `limewave_core::loop_finder::lab::lab_run`; this is only the argument
//! parser.

use std::path::PathBuf;
use std::process::ExitCode;

use limewave_core::loop_finder::DEFAULT_MIN_SECONDS;
use limewave_core::loop_finder::lab::{LabArgs, lab_run};

const USAGE: &str = "usage: loop_lab <file.wav> [--keep-end <frame>] [--range <first> <last>] \
[--min <s>] [--max <s>] [--preference <0..1>] [--out <dir>]";

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
    let mut out = None;
    let mut lab = LabArgs {
        keep_end: None,
        range: None,
        min_s: DEFAULT_MIN_SECONDS,
        max_s: None,
        preference: 0.5,
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--keep-end" => lab.keep_end = Some(number(&mut it, "--keep-end")?),
            "--range" => {
                let first = number(&mut it, "--range")?;
                let last = number(&mut it, "--range")?;
                lab.range = Some((first, last));
            }
            "--min" => lab.min_s = number(&mut it, "--min")?,
            "--max" => lab.max_s = Some(number(&mut it, "--max")?),
            "--preference" => lab.preference = number(&mut it, "--preference")?,
            "--out" => out = Some(PathBuf::from(value(&mut it, "--out")?)),
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ if file.is_none() => file = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected argument {arg:?}")),
        }
    }
    Ok((file.ok_or("no input file")?, lab, out))
}

fn main() -> ExitCode {
    let result = parse(std::env::args().skip(1).collect()).and_then(|(file, lab, out)| {
        let stem = file
            .file_stem()
            .map_or_else(|| "sample".into(), |s| s.to_string_lossy().into_owned());
        let dir = out.unwrap_or_else(|| PathBuf::from("target/loop-lab").join(stem));
        lab_run(&file, &lab, &dir)
    });
    match result {
        Ok(table) => {
            print!("{table}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("loop_lab: {e}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}
