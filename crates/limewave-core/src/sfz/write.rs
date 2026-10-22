//! Writes an [`SfzFile`] back to SFZ text, keeping every token in order.

use super::parse::{HeaderKind, SfzFile, Token};

/// Formats a number plainly: integers without a point, anything else with up
/// to 6 significant digits and no trailing zeros.
pub fn format_number(v: f64) -> String {
    if !v.is_finite() {
        return "0".to_owned();
    }
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let decimals = (5 - v.abs().log10().floor() as i32).max(0) as usize;
    let s = format!("{v:.decimals$}");
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        &s
    };
    if s == "-0" { "0" } else { s }.to_owned()
}

/// Whether opcode `name=value` survives being written and read again: reading it back gives
/// one region header and exactly that opcode. False for a value that holds `=`, `<`, a comment
/// start, a `$` (which a `#define` can change) or line breaks.
pub fn value_round_trips(name: &str, value: &str) -> bool {
    let Ok(file) = super::parse::parse(&format!("<region>{name}={value}")) else {
        return false;
    };
    !value.contains('$')
        && matches!(
            file.tokens.as_slice(),
            [
                Token::Header(HeaderKind::Region),
                Token::Opcode { name: n, value: v },
            ] if n == name && v == value
        )
}

fn header_name(kind: &HeaderKind) -> &str {
    match kind {
        HeaderKind::Control => "control",
        HeaderKind::Global => "global",
        HeaderKind::Master => "master",
        HeaderKind::Group => "group",
        HeaderKind::Region => "region",
        HeaderKind::Other(name) => name,
    }
}

/// Writes the file with one token per line and a blank line before every
/// header but the first line. Line endings follow `file.crlf`.
pub fn write(file: &SfzFile) -> String {
    let eol = if file.crlf { "\r\n" } else { "\n" };
    let mut out = String::new();
    for token in &file.tokens {
        match token {
            Token::Header(kind) => {
                if !out.is_empty() {
                    out.push_str(eol);
                }
                out.push('<');
                out.push_str(header_name(kind));
                out.push('>');
            }
            Token::Opcode { name, value } => {
                out.push_str(name);
                out.push('=');
                out.push_str(value);
            }
            // A block comment may span lines; make them match the file.
            Token::Comment(text) => out.push_str(&text.replace("\r\n", "\n").replace('\n', eol)),
            Token::Define { name, value } => {
                out.push_str("#define ");
                out.push_str(name);
                if !value.is_empty() {
                    out.push(' ');
                    out.push_str(value);
                }
            }
        }
        out.push_str(eol);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sfz::{Instrument, parse};

    #[test]
    fn only_values_that_read_back_as_written_round_trip() {
        assert!(value_round_trips("sample", "EP A1 mp.wav"));
        assert!(value_round_trips("sample", "samples/EP A1.wav"));
        assert!(!value_round_trips("sample", "x y=z.wav"));
        assert!(!value_round_trips("sample", "a<b.wav"));
        assert!(!value_round_trips("sample", "a//b.wav"));
        assert!(!value_round_trips("sample", "a/*b.wav"));
        assert!(!value_round_trips("sample", "$x.wav"));
    }

    const EPIANO: &str = "\
<control>
default_path=Samples/

<global>
ampeg_release=0.35

<group>
lovel=1 hivel=70
ampeg_sustain=80 ampeg_decay=1.5
<region> sample=Samples\\Some Name A1 mp.wav
lokey=24 hikey=30 pitch_keycenter=27 tune=-12 volume=-3.5
<region> sample=Samples\\Some Name C2 mp.wav
lokey=31 hikey=36 pitch_keycenter=33

<group>
lovel=71 hivel=127
ampeg_sustain=100
<region> sample=Samples\\Some Name A1 ff.wav
lokey=24 hikey=30 pitch_keycenter=27 tune=8
<region> sample=Samples\\Some Name C2 ff.wav
lokey=31 hikey=36 pitch_keycenter=33 volume=2
";

    const COMMENTED: &str = "\
// Made-up instrument
#define $ROOT 60
#define $LOOP 1200 // trailing note
/* a block
   comment */
<group> volume=-2 // quiet
<region> sample=a b.wav pitch_keycenter=$ROOT
loop_start=$LOOP // mid-region
<curve> v000=0 v127=1
";

    const LAYERED: &str = "\
<control> default_path=Samples/ octave_offset=0
<global> loop_mode=one_shot
<master> volume=1.5
<group> lovel=1 hivel=127
<region> sample=x.wav key=60
<region> sample=y.wav key=62 loop_mode=loop_continuous
";

    fn fixtures() -> [&'static str; 3] {
        [EPIANO, COMMENTED, LAYERED]
    }

    #[test]
    fn round_trip_keeps_tokens() {
        for text in fixtures() {
            let f = parse(text).unwrap();
            assert_eq!(parse(&write(&f)).unwrap(), f, "{text}");
        }
    }

    #[test]
    fn write_is_stable() {
        for text in fixtures() {
            let once = write(&parse(text).unwrap());
            let twice = write(&parse(&once).unwrap());
            assert_eq!(once, twice, "{text}");
        }
    }

    #[test]
    fn layout_is_one_token_per_line() {
        let out =
            write(&parse("<region> sample=a.wav lokey=1 // c\n<region> sample=b.wav").unwrap());
        assert_eq!(
            out,
            "<region>\nsample=a.wav\nlokey=1\n// c\n\n<region>\nsample=b.wav\n"
        );
    }

    #[test]
    fn crlf_round_trips() {
        let text = EPIANO.replace('\n', "\r\n");
        let f = parse(&text).unwrap();
        assert!(f.crlf);
        let out = write(&f);
        assert!(out.ends_with("\r\n"));
        assert!(!out.replace("\r\n", "").contains('\n'));
        assert_eq!(parse(&out).unwrap(), f);
    }

    #[test]
    fn crlf_block_comment_gets_no_doubled_returns() {
        let f = parse("<region>\r\n/* a\r\nb */\r\nlokey=1\r\n").unwrap();
        let out = write(&f);
        assert!(!out.contains("\r\r"), "{out:?}");
        assert_eq!(parse(&out).unwrap(), f);

        // A comment with bare line feeds in a CRLF file gets CRLF.
        let mut g = parse("<region>\r\nlokey=1\r\n").unwrap();
        g.tokens.insert(1, Token::Comment("/* a\nb */".into()));
        let out = write(&g);
        assert!(out.contains("/* a\r\nb */"), "{out:?}");
        assert!(!out.replace("\r\n", "").contains('\n'));
    }

    #[test]
    fn format_number_cases() {
        assert_eq!(format_number(60.0), "60");
        assert_eq!(format_number(-10.6), "-10.6");
        assert_eq!(format_number(0.0625), "0.0625");
        assert_eq!(format_number(99.5), "99.5");
        assert_eq!(format_number(1.0 / 3.0), "0.333333");
        assert_eq!(format_number(-0.0), "0");
        assert_eq!(format_number(123456.78), "123457");
        assert_eq!(format_number(9.9999999), "10");
    }

    #[test]
    fn one_edit_changes_one_line() {
        let f = parse(EPIANO).unwrap();
        let before = write(&f);
        let mut inst = Instrument::new(f);
        inst.set(1, "hikey", "50");
        let after = write(&inst.file);
        let (a, b): (Vec<_>, Vec<_>) = (before.lines().collect(), after.lines().collect());
        assert_eq!(a.len(), b.len());
        let diff: Vec<_> = a.iter().zip(&b).filter(|(x, y)| x != y).collect();
        assert_eq!(diff.len(), 1);
        assert_eq!(*diff[0].0, "hikey=36");
        assert_eq!(*diff[0].1, "hikey=50");
    }
}
