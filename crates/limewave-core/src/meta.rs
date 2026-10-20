//! Metadata chunks: `LIST/INFO` text tags and the root-note and tuning
//! fields of the `inst` and `acid` chunks.

use crate::riff::{Chunk, list_type};

const INFO: &[u8; 4] = b"INFO";
const SUB_HEADER_LEN: usize = 8;

/// The sub-chunks of a `LIST` body (after the list type), and how cleanly it parsed.
struct SubChunks {
    subs: Vec<Chunk>,
    /// Every byte of the body was accounted for (trailing zero bytes allowed).
    complete: bool,
    /// Some odd-length entry was not followed by its pad byte.
    missing_pad: bool,
}

/// Whether a whole sub-chunk header fits at `at` and its id looks like one: four printable
/// ASCII characters, the first a letter or digit.
fn plausible_id(body: &[u8], at: usize) -> bool {
    at + SUB_HEADER_LEN <= body.len()
        && body[at].is_ascii_alphanumeric()
        && body[at..at + 4].iter().all(|b| (0x20..=0x7e).contains(b))
}

/// Splits the body of a `LIST` chunk (after the list type) into sub-chunks. After an
/// odd-length entry the pad byte is skipped, unless only the unpadded position holds a
/// plausible next header (a writer left the pad byte out). A sub-chunk that does not fit
/// ends the scan.
fn parse_sub_chunks(body: &[u8]) -> SubChunks {
    let mut subs = Vec::new();
    let mut missing_pad = false;
    let mut pos = 0;
    while pos + SUB_HEADER_LEN <= body.len() {
        let id = [body[pos], body[pos + 1], body[pos + 2], body[pos + 3]];
        let size = u32::from_le_bytes([body[pos + 4], body[pos + 5], body[pos + 6], body[pos + 7]])
            as usize;
        let start = pos + SUB_HEADER_LEN;
        let Some(end) = start.checked_add(size).filter(|e| *e <= body.len()) else {
            break;
        };
        subs.push(Chunk {
            id,
            data: body[start..end].to_vec(),
        });
        pos = end;
        if size % 2 == 1 {
            if end == body.len() || (!plausible_id(body, end + 1) && plausible_id(body, end)) {
                missing_pad = true;
            } else {
                pos += 1;
            }
        }
    }
    let complete = body
        .get(pos..)
        .is_some_and(|rest| rest.iter().all(|b| *b == 0));
    SubChunks {
        subs,
        complete,
        missing_pad,
    }
}

fn write_sub_chunks(subs: &[Chunk]) -> Vec<u8> {
    let mut out = INFO.to_vec();
    for sub in subs {
        out.extend_from_slice(&sub.id);
        let size = u32::try_from(sub.data.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&sub.data);
        if sub.data.len() % 2 == 1 {
            out.push(0);
        }
    }
    out
}

fn info_index(chunks: &[Chunk]) -> Option<usize> {
    chunks.iter().position(|c| list_type(c) == Some(*INFO))
}

/// Reads a text tag (e.g. `INAM`, `ISFT`) from the first `LIST/INFO` chunk.
/// The value runs up to the first NUL byte.
pub fn info_get(chunks: &[Chunk], key: &[u8; 4]) -> Option<String> {
    let list = &chunks[info_index(chunks)?];
    let sub = parse_sub_chunks(&list.data[4..])
        .subs
        .into_iter()
        .find(|s| &s.id == key)?;
    let text = sub.data.split(|b| *b == 0).next().unwrap_or(&[]);
    Some(String::from_utf8_lossy(text).into_owned())
}

/// Sets a text tag in the first `LIST/INFO` chunk, replacing an existing value
/// in place or appending it. Creates a `LIST/INFO` chunk when there is none. An INFO
/// list that cannot be read to its end is left byte for byte as it is (the tag is not
/// set), so no entry is ever lost; [`info_warnings`] reports it on load.
pub fn info_set(chunks: &mut Vec<Chunk>, key: &[u8; 4], value: &str) {
    let mut data = value.as_bytes().to_vec();
    data.push(0);
    let new = Chunk { id: *key, data };

    let Some(i) = info_index(chunks) else {
        chunks.push(Chunk {
            id: *b"LIST",
            data: write_sub_chunks(&[new]),
        });
        return;
    };
    let parsed = parse_sub_chunks(&chunks[i].data[4..]);
    if !parsed.complete {
        return;
    }
    let mut subs = parsed.subs;
    match subs.iter_mut().find(|s| &s.id == key) {
        Some(existing) => *existing = new,
        None => subs.push(new),
    }
    chunks[i].data = write_sub_chunks(&subs);
}

/// Warnings about the first `LIST/INFO` chunk: entries missing their pad byte (fixed on
/// save), or a list that cannot be read to its end (kept as is; its software tag is not
/// updated).
pub fn info_warnings(chunks: &[Chunk]) -> Vec<String> {
    let Some(i) = info_index(chunks) else {
        return Vec::new();
    };
    let parsed = parse_sub_chunks(&chunks[i].data[4..]);
    let mut warnings = Vec::new();
    if !parsed.complete {
        warnings.push(
            "the LIST/INFO chunk could not be read to its end; it will be saved unchanged,              without the software tag"
                .to_owned(),
        );
    } else if parsed.missing_pad {
        warnings.push("LIST/INFO entries without their pad byte were read; fixed on save".into());
    }
    warnings
}

/// Writes the root note and fine tune into an `inst` chunk body. The `inst`
/// fine tune is the correction applied on playback, so it is the negated
/// recorded pitch offset in cents. No-op if the body is shorter than 7 bytes.
pub fn sync_inst(data: &mut [u8], root: u8, cents: f32) {
    if data.len() < 7 {
        return;
    }
    let tune = (-cents.round()).clamp(-50.0, 50.0) as i8;
    data[0] = root;
    data[1] = tune as u8;
}

/// Writes the root note (u16 at byte 4) into an `acid` chunk body. No-op if
/// the body is shorter than 6 bytes.
pub fn sync_acid(data: &mut [u8], root: u8) {
    if data.len() < 6 {
        return;
    }
    data[4..6].copy_from_slice(&u16::from(root).to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(id: &[u8; 4], value: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&(value.len() as u32).to_le_bytes());
        v.extend_from_slice(value);
        if value.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn list(kind: &[u8; 4], subs: &[Vec<u8>]) -> Chunk {
        let mut data = kind.to_vec();
        for s in subs {
            data.extend_from_slice(s);
        }
        Chunk { id: *b"LIST", data }
    }

    fn info_chunk() -> Chunk {
        list(
            b"INFO",
            &[sub(b"INAM", b"0051\0"), sub(b"ISFT", b"Other Tool 1.0\0")],
        )
    }

    fn data_chunk() -> Chunk {
        Chunk {
            id: *b"data",
            data: vec![1, 2, 3, 4],
        }
    }

    #[test]
    fn info_get_reads_inam_and_isft() {
        let chunks = vec![data_chunk(), info_chunk()];
        assert_eq!(info_get(&chunks, b"INAM").as_deref(), Some("0051"));
        assert_eq!(
            info_get(&chunks, b"ISFT").as_deref(),
            Some("Other Tool 1.0")
        );
        assert_eq!(info_get(&chunks, b"ICMT"), None);
    }

    #[test]
    fn info_get_handles_missing_nul_and_malformed_lists() {
        let no_nul = list(b"INFO", &[sub(b"INAM", b"abc")]);
        assert_eq!(info_get(&[no_nul], b"INAM").as_deref(), Some("abc"));

        // Declared size runs past the end: skipped, no panic.
        let mut bad = b"INFOINAM".to_vec();
        bad.extend_from_slice(&1000u32.to_le_bytes());
        bad.extend_from_slice(b"xy");
        let bad = Chunk {
            id: *b"LIST",
            data: bad,
        };
        assert_eq!(info_get(&[bad], b"INAM"), None);

        // Header cut short, empty INFO list.
        let short = Chunk {
            id: *b"LIST",
            data: b"INFOIN".to_vec(),
        };
        assert_eq!(info_get(&[short], b"INAM"), None);
        let empty = Chunk {
            id: *b"LIST",
            data: b"INFO".to_vec(),
        };
        assert_eq!(info_get(&[empty], b"INAM"), None);

        // Invalid UTF-8 decodes lossily.
        let lossy = list(b"INFO", &[sub(b"INAM", b"a\xffb\0")]);
        assert_eq!(info_get(&[lossy], b"INAM").as_deref(), Some("a\u{fffd}b"));
    }

    #[test]
    fn info_set_replaces_isft_keeps_inam() {
        let mut chunks = vec![data_chunk(), info_chunk()];
        info_set(&mut chunks, b"ISFT", "Limewave 0.1.0");
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            info_get(&chunks, b"ISFT").as_deref(),
            Some("Limewave 0.1.0")
        );
        assert_eq!(info_get(&chunks, b"INAM").as_deref(), Some("0051"));
        let subs = parse_sub_chunks(&chunks[1].data[4..]).subs;
        let ids: Vec<_> = subs.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![*b"INAM", *b"ISFT"]);
        // The declared sub-chunk data includes the NUL terminator.
        assert_eq!(subs[1].data, b"Limewave 0.1.0\0");
    }

    #[test]
    fn info_set_pads_odd_values_and_appends_new_keys() {
        let mut chunks = vec![info_chunk()];
        info_set(&mut chunks, b"ICMT", "ab"); // "ab\0" is 3 bytes: odd
        assert_eq!(info_get(&chunks, b"ICMT").as_deref(), Some("ab"));
        let data = &chunks[0].data;
        assert_eq!(&data[data.len() - 12..], b"ICMT\x03\0\0\0ab\0\0");
        let subs = parse_sub_chunks(&data[4..]).subs;
        let ids: Vec<_> = subs.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![*b"INAM", *b"ISFT", *b"ICMT"]);
        assert_eq!(data.len() % 2, 0);
    }

    #[test]
    fn info_set_creates_list_when_missing() {
        let mut chunks = vec![data_chunk()];
        info_set(&mut chunks, b"ISFT", "Limewave 0.1.0");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.iter().filter(|c| &c.id == b"LIST").count(), 1);
        assert_eq!(list_type(&chunks[1]), Some(*b"INFO"));
        assert_eq!(
            info_get(&chunks, b"ISFT").as_deref(),
            Some("Limewave 0.1.0")
        );
        let subs = parse_sub_chunks(&chunks[1].data[4..]).subs;
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn info_set_ignores_adtl_list() {
        let adtl = list(b"adtl", &[sub(b"ISFT", b"not info\0")]);
        let adtl_bytes = adtl.data.clone();
        let mut chunks = vec![adtl, info_chunk(), data_chunk()];
        info_set(&mut chunks, b"ISFT", "Limewave 0.1.0");
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].data, adtl_bytes);
        assert_eq!(
            info_get(&chunks, b"ISFT").as_deref(),
            Some("Limewave 0.1.0")
        );
        assert_eq!(info_get(&chunks, b"INAM").as_deref(), Some("0051"));
        assert_eq!(chunks[2], data_chunk());
    }

    /// A sub-chunk written without the pad byte after an odd-length value.
    fn unpadded(id: &[u8; 4], value: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&(value.len() as u32).to_le_bytes());
        v.extend_from_slice(value);
        v
    }

    #[test]
    fn info_entry_missing_its_pad_byte_keeps_every_tag() {
        let lax = list(
            b"INFO",
            &[
                unpadded(b"INAM", b"Lead\0"),
                unpadded(b"ICMT", b"odd\0x"),
                sub(b"ISFT", b"Other Tool 1.0\0"),
                unpadded(b"IART", b"Me\0"),
            ],
        );
        let mut chunks = vec![lax];
        assert_eq!(info_get(&chunks, b"ICMT").as_deref(), Some("odd"));
        assert_eq!(info_get(&chunks, b"IART").as_deref(), Some("Me"));
        info_set(&mut chunks, b"ISFT", "Limewave 0.1.0");
        assert_eq!(info_get(&chunks, b"INAM").as_deref(), Some("Lead"));
        assert_eq!(info_get(&chunks, b"ICMT").as_deref(), Some("odd"));
        assert_eq!(info_get(&chunks, b"IART").as_deref(), Some("Me"));
        assert_eq!(
            info_get(&chunks, b"ISFT").as_deref(),
            Some("Limewave 0.1.0")
        );
        // Rewritten with the pad bytes in place.
        let want = list(
            b"INFO",
            &[
                sub(b"INAM", b"Lead\0"),
                sub(b"ICMT", b"odd\0x"),
                sub(b"ISFT", b"Limewave 0.1.0\0"),
                sub(b"IART", b"Me\0"),
            ],
        );
        assert_eq!(chunks, vec![want]);
        assert!(info_warnings(&chunks).is_empty());
    }

    #[test]
    fn info_list_that_cannot_be_read_fully_is_kept_verbatim() {
        let mut body = sub(b"INAM", b"Lead\0");
        body.extend_from_slice(&[0x01, 0x02, 0x03, 0x04, 0xff, 0xff, 0, 0, 7, 7]);
        let bad = list(b"INFO", &[body]);
        let mut chunks = vec![bad.clone()];
        assert_eq!(info_get(&chunks, b"INAM").as_deref(), Some("Lead"));
        info_set(&mut chunks, b"ISFT", "Limewave 0.1.0");
        assert_eq!(chunks, vec![bad], "kept byte for byte");
        assert_eq!(info_get(&chunks, b"ISFT"), None);
        assert_eq!(info_warnings(&chunks).len(), 1);
    }

    #[test]
    fn sync_inst_sets_note_and_flips_tune() {
        let mut data = [0u8; 7];
        sync_inst(&mut data, 48, -12.0);
        assert_eq!(data, [48, 12, 0, 0, 0, 0, 0]);

        let mut data = [9u8; 7];
        sync_inst(&mut data, 60, 20.4);
        assert_eq!(data, [60, (-20i8) as u8, 9, 9, 9, 9, 9]);
    }

    #[test]
    fn sync_inst_clamps_and_ignores_short_data() {
        let mut data = [0u8; 7];
        sync_inst(&mut data, 60, 200.0);
        assert_eq!(data[1] as i8, -50);
        sync_inst(&mut data, 60, -200.0);
        assert_eq!(data[1] as i8, 50);
        sync_inst(&mut data, 60, f32::NAN);
        assert_eq!(data[1], 0);

        let mut short = [7u8; 6];
        sync_inst(&mut short, 48, 10.0);
        assert_eq!(short, [7; 6]);
    }

    #[test]
    fn sync_acid_sets_root() {
        let mut data = [0xAAu8; 24];
        sync_acid(&mut data, 48);
        assert_eq!(data[4..6], 48u16.to_le_bytes());
        assert_eq!(data[..4], [0xAA; 4]);
        assert_eq!(data[6..], [0xAA; 18]);

        let mut short = [1u8; 5];
        sync_acid(&mut short, 48);
        assert_eq!(short, [1; 5]);
    }
}
