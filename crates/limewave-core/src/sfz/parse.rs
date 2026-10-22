//! A lossless SFZ tokenizer. Comments and unknown headers are kept so a file
//! can be written back without losing what Limewave doesn't understand.

/// The kind of a `<header>`.
#[derive(Clone, PartialEq, Debug)]
pub enum HeaderKind {
    Control,
    Global,
    Master,
    Group,
    Region,
    /// Any other header; holds the name without the angle brackets.
    Other(String),
}

/// One piece of SFZ text. A header owns every token after it up to the next
/// header; tokens before the first header belong to no header.
#[derive(Clone, PartialEq, Debug)]
pub enum Token {
    Header(HeaderKind),
    /// `name=value`; the value is the raw trimmed text and may contain `$NAME`.
    Opcode {
        name: String,
        value: String,
    },
    /// The full comment text, including `//` or `/* … */`.
    Comment(String),
    /// `#define $NAME value`; the name includes the `$`.
    Define {
        name: String,
        value: String,
    },
}

/// A parsed SFZ file.
#[derive(Clone, PartialEq, Debug)]
pub struct SfzFile {
    pub tokens: Vec<Token>,
    /// Whether the text used `\r\n` line endings.
    pub crlf: bool,
}

/// Why an SFZ file could not be parsed.
#[derive(Debug, thiserror::Error)]
pub enum SfzError {
    #[error("This instrument uses #include, which Limewave can't open yet")]
    Include,
}

/// Decodes SFZ bytes: UTF-8 (a BOM is stripped), else Latin-1.
pub fn decode_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => bytes.iter().map(|&b| char::from(b)).collect(),
    }
}

/// Replaces `$NAME` occurrences using `defines`, preferring the longest name.
pub fn expand(value: &str, defines: &[(String, String)]) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let hit = defines
            .iter()
            .filter(|(name, _)| !name.is_empty() && rest.starts_with(name.as_str()))
            .max_by_key(|(name, _)| name.len());
        match hit {
            Some((name, with)) => {
                out.push_str(with);
                rest = &rest[name.len()..];
            }
            None => {
                out.push('$');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

fn header_kind(name: &str) -> HeaderKind {
    match name {
        "control" => HeaderKind::Control,
        "global" => HeaderKind::Global,
        "master" => HeaderKind::Master,
        "group" => HeaderKind::Group,
        "region" => HeaderKind::Region,
        other => HeaderKind::Other(other.to_owned()),
    }
}

/// Does a `name=` start at `i` (after any blanks)?
fn opcode_starts_at(chars: &[char], mut i: usize) -> bool {
    while chars.get(i).is_some_and(|c| c.is_whitespace()) {
        i += 1;
    }
    let start = i;
    while chars.get(i).copied().is_some_and(is_name_char) {
        i += 1;
    }
    i > start && chars.get(i) == Some(&'=')
}

fn starts_with(chars: &[char], i: usize, pat: &str) -> bool {
    pat.chars()
        .enumerate()
        .all(|(k, p)| chars.get(i + k) == Some(&p))
}

fn line_end(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i] != '\n' {
        i += 1;
    }
    i
}

/// Parses SFZ text into tokens.
pub fn parse(text: &str) -> Result<SfzFile, SfzError> {
    let chars: Vec<char> = text.chars().collect();
    let collect = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if starts_with(&chars, i, "//") {
            let end = line_end(&chars, i);
            tokens.push(Token::Comment(collect(i, end).trim_end().to_owned()));
            i = end;
        } else if starts_with(&chars, i, "/*") {
            let mut end = i + 2;
            while end < chars.len() && !starts_with(&chars, end, "*/") {
                end += 1;
            }
            let end = (end + 2).min(chars.len());
            tokens.push(Token::Comment(collect(i, end)));
            i = end;
        } else if c == '<' {
            let mut end = i + 1;
            while end < chars.len() && !matches!(chars[end], '>' | '<' | '\n') {
                end += 1;
            }
            if chars.get(end) == Some(&'>') {
                tokens.push(Token::Header(header_kind(&collect(i + 1, end))));
                i = end + 1;
            } else {
                i += 1;
            }
        } else if starts_with(&chars, i, "#include") {
            return Err(SfzError::Include);
        } else if starts_with(&chars, i, "#define")
            && chars.get(i + 7).is_some_and(|c| c.is_whitespace())
        {
            let mut end = i + 7;
            while end < chars.len() && chars[end] != '\n' && !starts_with(&chars, end, "//") {
                end += 1;
            }
            let line = collect(i + 7, end);
            let line = line.trim();
            let (name, value) = line
                .split_once(char::is_whitespace)
                .map_or((line, ""), |(n, v)| (n, v.trim()));
            if !name.is_empty() {
                tokens.push(Token::Define {
                    name: name.to_owned(),
                    value: value.to_owned(),
                });
            }
            i = end;
        } else {
            let mut end = i;
            while chars.get(end).copied().is_some_and(is_name_char) {
                end += 1;
            }
            if end > i && chars.get(end) == Some(&'=') {
                let name = collect(i, end);
                let start = end + 1;
                let mut stop = start;
                while stop < chars.len() {
                    let ch = chars[stop];
                    if matches!(ch, '\n' | '<')
                        || starts_with(&chars, stop, "//")
                        || starts_with(&chars, stop, "/*")
                        || (ch.is_whitespace() && opcode_starts_at(&chars, stop))
                    {
                        break;
                    }
                    stop += 1;
                }
                let value = collect(start, stop).trim().to_owned();
                tokens.push(Token::Opcode { name, value });
                i = stop;
            } else {
                // A stray token: skip it.
                i = end.max(i + 1);
                while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '<' {
                    i += 1;
                }
            }
        }
    }
    Ok(SfzFile {
        tokens,
        crlf: text.contains("\r\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(name: &str, value: &str) -> Token {
        Token::Opcode {
            name: name.into(),
            value: value.into(),
        }
    }

    #[test]
    fn values_run_to_the_next_opcode_or_line_end() {
        let f = parse("<region> sample=Samples\\EP A1 mp.wav\nlokey=12 hikey=48").unwrap();
        assert_eq!(
            f.tokens,
            vec![
                Token::Header(HeaderKind::Region),
                op("sample", "Samples\\EP A1 mp.wav"),
                op("lokey", "12"),
                op("hikey", "48"),
            ]
        );
    }

    #[test]
    fn comments_are_kept_in_place() {
        let f = parse("// x\n<region> lokey=1 // note\n/* a\nb */ hikey=2").unwrap();
        assert_eq!(
            f.tokens,
            vec![
                Token::Comment("// x".into()),
                Token::Header(HeaderKind::Region),
                op("lokey", "1"),
                Token::Comment("// note".into()),
                Token::Comment("/* a\nb */".into()),
                op("hikey", "2"),
            ]
        );
    }

    #[test]
    fn defines_are_kept_and_expandable() {
        let f = parse("#define $KEY 60\n<region> key=$KEY").unwrap();
        assert_eq!(
            f.tokens[0],
            Token::Define {
                name: "$KEY".into(),
                value: "60".into()
            }
        );
        let defs = vec![
            ("$KEY".to_owned(), "60".to_owned()),
            ("$KEY2".to_owned(), "62".to_owned()),
        ];
        assert_eq!(expand("$KEY", &defs), "60");
        assert_eq!(expand("$KEY2", &defs), "62");
        assert_eq!(expand("a$KEY-$OTHER", &defs), "a60-$OTHER");
    }

    #[test]
    fn include_is_refused() {
        assert!(matches!(
            parse("#include \"x.sfz\""),
            Err(SfzError::Include)
        ));
    }

    #[test]
    fn unknown_headers_are_kept() {
        let f = parse("<curve> v000=0").unwrap();
        assert_eq!(
            f.tokens[0],
            Token::Header(HeaderKind::Other("curve".into()))
        );
    }

    #[test]
    fn crlf_is_detected() {
        let f = parse("<region>\r\nsample=a.wav\r\nlokey=1\r\n").unwrap();
        assert!(f.crlf);
        assert_eq!(f.tokens[1], op("sample", "a.wav"));
        assert_eq!(f.tokens[2], op("lokey", "1"));
        assert!(!parse("<region>\nlokey=1\n").unwrap().crlf);
    }

    #[test]
    fn latin1_bytes_decode() {
        assert_eq!(decode_text(&[0x63, 0xE9]), "cé");
        assert_eq!(decode_text(&[0xEF, 0xBB, 0xBF, 0x63]), "c");
        assert_eq!(decode_text("cé".as_bytes()), "cé");
    }

    #[test]
    fn malformed_input_never_panics() {
        for s in ["<", "<region", "=", "a=", "=b", "/*", "#define", "\u{0}"] {
            let _ = parse(s);
        }
        let mut x: u32 = 12345;
        let bytes: Vec<u8> = (0..1000)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                (x >> 24) as u8
            })
            .collect();
        let _ = parse(&decode_text(&bytes));
    }
}
