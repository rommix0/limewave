//! Cue markers: the `cue ` chunk and the `labl`, `note` and `ltxt` entries of `LIST/adtl`
//! that hang off it. Saving keeps them and moves their sample positions with the audio
//! (spec v0.2 §4.8).

use crate::error::WavError;
use crate::riff::{Chunk, find, list_type};

const RECORD_LEN: usize = 24;
const SUB_HEADER_LEN: usize = 8;
/// The `adtl` entries that belong to one cue; their first `u32` is the cue id.
const CUE_ENTRIES: [&[u8; 4]; 3] = [b"labl", b"note", b"ltxt"];

/// One 24-byte record of a `cue ` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CuePoint {
    pub id: u32,
    /// Sample position, counted from the start of the audio.
    pub position: u32,
    /// The chunk the position counts in, normally `data`.
    pub chunk_id: [u8; 4],
    pub chunk_start: u32,
    pub block_start: u32,
    pub sample_offset: u32,
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// Reads a `cue ` chunk: a `u32` count, then that many 24-byte records. Bytes after the
/// last record are ignored.
pub fn parse_cue(data: &[u8]) -> Result<Vec<CuePoint>, WavError> {
    if data.len() < 4 {
        return Err(WavError::Malformed("cue chunk is too short".into()));
    }
    let count = u32_at(data, 0) as usize;
    count
        .checked_mul(RECORD_LEN)
        .and_then(|n| n.checked_add(4))
        .filter(|n| *n <= data.len())
        .ok_or_else(|| {
            WavError::Malformed(format!(
                "cue chunk claims {count} points but has only {} bytes",
                data.len()
            ))
        })?;
    Ok((0..count)
        .map(|i| {
            let at = 4 + i * RECORD_LEN;
            CuePoint {
                id: u32_at(data, at),
                position: u32_at(data, at + 4),
                chunk_id: [data[at + 8], data[at + 9], data[at + 10], data[at + 11]],
                chunk_start: u32_at(data, at + 12),
                block_start: u32_at(data, at + 16),
                sample_offset: u32_at(data, at + 20),
            }
        })
        .collect())
}

/// The bytes of a `cue ` chunk holding `points`.
pub fn cue_bytes(points: &[CuePoint]) -> Vec<u8> {
    let count = u32::try_from(points.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(4 + points.len() * RECORD_LEN);
    out.extend_from_slice(&count.to_le_bytes());
    for p in points {
        out.extend_from_slice(&p.id.to_le_bytes());
        out.extend_from_slice(&p.position.to_le_bytes());
        out.extend_from_slice(&p.chunk_id);
        out.extend_from_slice(&p.chunk_start.to_le_bytes());
        out.extend_from_slice(&p.block_start.to_le_bytes());
        out.extend_from_slice(&p.sample_offset.to_le_bytes());
    }
    out
}

/// The entries of a `LIST/adtl` chunk (`data` includes the `adtl` type): id, size, data and a
/// pad byte after an odd size. A missing final pad byte and trailing zeros are tolerated; an
/// entry that does not fit is an error.
fn parse_adtl(data: &[u8]) -> Result<Vec<Chunk>, String> {
    let body = data.get(4..).unwrap_or_default();
    let mut subs = Vec::new();
    // Trailing zero bytes are padding, not entries; find where they start once.
    let tail = body.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
    let mut pos = 0;
    while pos < tail {
        if pos + SUB_HEADER_LEN > body.len() {
            return Err(format!(
                "LIST/adtl has {} stray bytes at its end",
                body.len() - pos
            ));
        }
        let id = [body[pos], body[pos + 1], body[pos + 2], body[pos + 3]];
        let size = u32_at(body, pos + 4) as usize;
        let start = pos + SUB_HEADER_LEN;
        let end = start
            .checked_add(size)
            .filter(|e| *e <= body.len())
            .ok_or_else(|| {
                format!(
                    "LIST/adtl entry '{}' runs past the end of the list",
                    String::from_utf8_lossy(&id)
                )
            })?;
        subs.push(Chunk {
            id,
            data: body[start..end].to_vec(),
        });
        pos = end + (size & 1);
    }
    Ok(subs)
}

fn adtl_bytes(subs: &[Chunk]) -> Vec<u8> {
    let mut out = b"adtl".to_vec();
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

/// The cue an `adtl` entry belongs to, when it is a `labl`, `note` or `ltxt` with an id.
fn entry_cue(sub: &Chunk) -> Option<u32> {
    (CUE_ENTRIES.contains(&&sub.id) && sub.data.len() >= 4).then(|| u32_at(&sub.data, 0))
}

fn adtl_index(chunks: &[Chunk]) -> Option<usize> {
    chunks.iter().position(|c| list_type(c) == Some(*b"adtl"))
}

/// Whether the first `cue ` chunk and the first `LIST/adtl` can be read, as
/// [`remap_markers`] needs. The reason is in the `Err`.
pub fn check_markers(chunks: &[Chunk]) -> Result<(), String> {
    if let Some(i) = find(chunks, b"cue ") {
        parse_cue(&chunks[i].data).map_err(|e| e.to_string())?;
    }
    if let Some(i) = adtl_index(chunks) {
        parse_adtl(&chunks[i].data)?;
    }
    Ok(())
}

/// Moves the cue positions in the first `cue ` chunk and first `LIST/adtl` to follow the
/// audio. `map` sends an original frame to where its sound lands (a reverse mirrors it);
/// `start..=end` are the saved frames, and positions are counted from `start`.
///
/// - A point cue is kept when `start <= map(position) <= end + 1`; `end + 1` is the exclusive
///   end, where some editors put an end-of-file marker, and it becomes the saved length. Its
///   `position` and `sample_offset` become `map(old) - start`, each from its own old value.
///   (Ranges below are not given this extra frame.)
/// - A cue with an `ltxt` range `[p, p + len - 1]` maps both ends, orders them, clips them to
///   `start..=end` and is kept when anything is left. Its position becomes the clipped low end
///   minus `start`, and the `ltxt` length the clipped length.
/// - A dropped cue also loses its `labl`, `note` and `ltxt` entries.
///
/// When nothing would change, the chunks are left byte for byte. When a chunk cannot be
/// read, both are left untouched and the reason is returned.
pub fn remap_markers(
    chunks: &mut [Chunk],
    map: &dyn Fn(usize) -> usize,
    start: usize,
    end: usize,
) -> Result<(), String> {
    let Some(cue_at) = find(chunks, b"cue ") else {
        return Ok(());
    };
    let points = parse_cue(&chunks[cue_at].data).map_err(|e| e.to_string())?;
    let adtl_at = adtl_index(chunks);
    let mut subs = match adtl_at {
        Some(i) => parse_adtl(&chunks[i].data)?,
        None => Vec::new(),
    };

    let shift = |v: usize| u32::try_from(v.saturating_sub(start)).unwrap_or(u32::MAX);
    let mut adtl_changed = false;
    let mut kept = Vec::with_capacity(points.len());
    for p in &points {
        // The first `ltxt` with a sample length makes the cue a range.
        let range = subs.iter().position(|s| {
            &s.id == b"ltxt"
                && s.data.len() >= 8
                && u32_at(&s.data, 0) == p.id
                && u32_at(&s.data, 4) > 0
        });
        let moved = match range {
            None => {
                let at = map(p.position as usize);
                (start..=end.saturating_add(1))
                    .contains(&at)
                    .then_some(CuePoint {
                        position: shift(at),
                        sample_offset: shift(map(p.sample_offset as usize)),
                        ..*p
                    })
            }
            Some(i) => {
                let len = u32_at(&subs[i].data, 4);
                let low = p.position as usize;
                let high = low.saturating_add(len as usize - 1);
                let (a, b) = (map(low), map(high));
                let (a, b) = (a.min(b).max(start), a.max(b).min(end));
                (a <= b).then(|| {
                    let new_len = u32::try_from(b - a + 1).unwrap_or(u32::MAX);
                    if new_len != len {
                        subs[i].data[4..8].copy_from_slice(&new_len.to_le_bytes());
                        adtl_changed = true;
                    }
                    CuePoint {
                        position: shift(a),
                        // Writers normally repeat the position here; keep it that way.
                        sample_offset: if p.sample_offset == p.position {
                            shift(a)
                        } else {
                            shift(map(p.sample_offset as usize))
                        },
                        ..*p
                    }
                })
            }
        };
        kept.extend(moved);
    }

    // Entries of a cue that was dropped go with it; those of unknown cues stay.
    let before = subs.len();
    subs.retain(|s| {
        entry_cue(s)
            .is_none_or(|id| kept.iter().any(|k| k.id == id) || !points.iter().any(|p| p.id == id))
    });
    adtl_changed |= subs.len() != before;

    if kept != points {
        let mut data = cue_bytes(&kept);
        data.extend_from_slice(&chunks[cue_at].data[4 + points.len() * RECORD_LEN..]);
        chunks[cue_at].data = data;
    }
    if let (true, Some(i)) = (adtl_changed, adtl_at) {
        chunks[i].data = adtl_bytes(&subs);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(id: u32, at: u32) -> CuePoint {
        CuePoint {
            id,
            position: at,
            chunk_id: *b"data",
            chunk_start: 0,
            block_start: 0,
            sample_offset: at,
        }
    }

    fn cue_chunk(points: &[CuePoint]) -> Chunk {
        Chunk {
            id: *b"cue ",
            data: cue_bytes(points),
        }
    }

    fn labl(id: u32, text: &str) -> Chunk {
        let mut data = id.to_le_bytes().to_vec();
        data.extend_from_slice(text.as_bytes());
        data.push(0);
        Chunk { id: *b"labl", data }
    }

    fn ltxt(id: u32, len: u32) -> Chunk {
        let mut data = id.to_le_bytes().to_vec();
        data.extend_from_slice(&len.to_le_bytes());
        data.extend_from_slice(b"rgn \0\0\0\0\0\0\0\0");
        Chunk { id: *b"ltxt", data }
    }

    fn adtl_chunk(subs: &[Chunk]) -> Chunk {
        Chunk {
            id: *b"LIST",
            data: adtl_bytes(subs),
        }
    }

    fn cues(chunks: &[Chunk]) -> Vec<CuePoint> {
        parse_cue(&chunks[0].data).unwrap()
    }

    fn subs(chunks: &[Chunk]) -> Vec<Chunk> {
        parse_adtl(&chunks[1].data).unwrap()
    }

    #[test]
    fn parse_and_write_round_trip() {
        let points = vec![point(1, 10), point(7, 99_999)];
        let bytes = cue_bytes(&points);
        assert_eq!(bytes.len(), 4 + 2 * 24);
        assert_eq!(parse_cue(&bytes).unwrap(), points);
        assert_eq!(parse_cue(&cue_bytes(&[])).unwrap(), vec![]);
    }

    #[test]
    fn bad_cue_count_is_an_error() {
        let mut bytes = cue_bytes(&[point(1, 10)]);
        bytes[0..4].copy_from_slice(&5u32.to_le_bytes());
        assert!(matches!(parse_cue(&bytes), Err(WavError::Malformed(_))));
        assert!(parse_cue(&[1, 0]).is_err());
        assert!(parse_cue(&u32::MAX.to_le_bytes()).is_err());
    }

    #[test]
    fn head_trim_shifts_cues() {
        let mut chunks = vec![cue_chunk(&[point(1, 500)])];
        remap_markers(&mut chunks, &|f| f, 100, 999).unwrap();
        let c = cues(&chunks);
        assert_eq!((c[0].position, c[0].sample_offset), (400, 400));
    }

    #[test]
    fn cues_outside_the_trim_are_dropped_with_their_labels() {
        let mut chunks = vec![
            cue_chunk(&[point(1, 50), point(2, 500)]),
            adtl_chunk(&[labl(1, "a"), labl(2, "b")]),
        ];
        remap_markers(&mut chunks, &|f| f, 100, 999).unwrap();
        let c = cues(&chunks);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].id, c[0].position), (2, 400));
        assert_eq!(subs(&chunks), vec![labl(2, "b")]);
        // A cue after End goes too.
        let mut chunks = vec![cue_chunk(&[point(1, 500), point(2, 901)])];
        remap_markers(&mut chunks, &|f| f, 0, 899).unwrap();
        assert_eq!(cues(&chunks), vec![point(1, 500)]);
    }

    #[test]
    fn end_of_file_cue_is_kept_at_the_saved_length() {
        // A point cue on the exclusive end (frame 1000 of 1000) survives; one frame later does not.
        let mut chunks = vec![cue_chunk(&[point(1, 1000), point(2, 1001)])];
        remap_markers(&mut chunks, &|f| f, 100, 999).unwrap();
        assert_eq!(cues(&chunks), vec![point(1, 900)]);
        // A range ending past the audio is still clipped, not extended.
        let mut chunks = vec![cue_chunk(&[point(1, 990)]), adtl_chunk(&[ltxt(1, 20)])];
        remap_markers(&mut chunks, &|f| f, 0, 999).unwrap();
        assert_eq!(u32_at(&subs(&chunks)[0].data, 4), 10);
    }

    #[test]
    fn huge_zero_filled_adtl_parses_in_linear_time() {
        // Zero bytes then one nonzero byte used to cost quadratic time.
        let mut data = b"adtl".to_vec();
        data.resize(4 + (1 << 20), 0);
        data.push(1);
        let started = std::time::Instant::now();
        let _ = parse_adtl(&data);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn reverse_mirrors_cues_and_ltxt_ranges() {
        let mut chunks = vec![
            cue_chunk(&[point(1, 200), point(2, 10)]),
            adtl_chunk(&[ltxt(1, 101)]),
        ];
        remap_markers(&mut chunks, &|f| 999 - f, 0, 999).unwrap();
        let c = cues(&chunks);
        assert_eq!((c[0].position, c[0].sample_offset), (699, 699));
        assert_eq!(c[1].position, 989);
        assert_eq!(u32_at(&subs(&chunks)[0].data, 4), 101);
    }

    #[test]
    fn ltxt_range_crossing_start_is_clipped() {
        let mut chunks = vec![cue_chunk(&[point(1, 80)]), adtl_chunk(&[ltxt(1, 101)])];
        remap_markers(&mut chunks, &|f| f, 100, 999).unwrap();
        assert_eq!(cues(&chunks)[0].position, 0);
        assert_eq!(u32_at(&subs(&chunks)[0].data, 4), 81);
        // A range entirely before Start goes, with its entry.
        let mut chunks = vec![cue_chunk(&[point(1, 10)]), adtl_chunk(&[ltxt(1, 20)])];
        remap_markers(&mut chunks, &|f| f, 100, 999).unwrap();
        assert!(cues(&chunks).is_empty());
        assert!(subs(&chunks).is_empty());
    }

    #[test]
    fn identity_remap_leaves_bytes_alone() {
        // A nonzero tail after the records and extra zero bytes must survive.
        let mut cue = cue_bytes(&[point(1, 10), point(2, 20)]);
        cue.extend_from_slice(&[9, 9, 9]);
        let mut adtl = adtl_bytes(&[labl(1, "abc"), ltxt(2, 5)]);
        adtl.extend_from_slice(&[0, 0, 0]);
        let mut chunks = vec![
            Chunk {
                id: *b"cue ",
                data: cue,
            },
            Chunk {
                id: *b"LIST",
                data: adtl,
            },
        ];
        let before = chunks.clone();
        remap_markers(&mut chunks, &|f| f, 0, 999).unwrap();
        assert_eq!(chunks, before);
    }

    #[test]
    fn malformed_cue_is_kept_unchanged() {
        let mut bad = cue_bytes(&[point(1, 500)]);
        bad[0..4].copy_from_slice(&9u32.to_le_bytes());
        let mut chunks = vec![
            Chunk {
                id: *b"cue ",
                data: bad,
            },
            adtl_chunk(&[labl(1, "a")]),
        ];
        let before = chunks.clone();
        assert!(remap_markers(&mut chunks, &|f| f, 100, 999).is_err());
        assert_eq!(chunks, before);
        assert!(check_markers(&chunks).is_err());
    }

    #[test]
    fn truncated_adtl_is_kept_unchanged() {
        let mut adtl = adtl_bytes(&[labl(1, "a")]);
        adtl.extend_from_slice(b"labl\xff\0\0\0\x01\0\0\0");
        let mut chunks = vec![
            cue_chunk(&[point(1, 500), point(2, 5)]),
            Chunk {
                id: *b"LIST",
                data: adtl,
            },
        ];
        let before = chunks.clone();
        assert!(remap_markers(&mut chunks, &|f| f, 100, 999).is_err());
        assert_eq!(chunks, before);
        assert!(check_markers(&chunks).is_err());
    }

    #[test]
    fn files_without_cues_and_unknown_entries_are_untouched() {
        let mut chunks = vec![adtl_chunk(&[labl(1, "a")])];
        let before = chunks.clone();
        remap_markers(&mut chunks, &|f| f, 100, 999).unwrap();
        assert_eq!(chunks, before);

        // Entries of other kinds, and labels of cues that do not exist, stay.
        let other = Chunk {
            id: *b"xxxx",
            data: vec![1, 0, 0, 0],
        };
        let mut chunks = vec![
            cue_chunk(&[point(1, 50)]),
            adtl_chunk(&[labl(1, "a"), labl(9, "orphan"), other.clone()]),
        ];
        remap_markers(&mut chunks, &|f| f, 100, 999).unwrap();
        assert_eq!(subs(&chunks), vec![labl(9, "orphan"), other]);
    }
}
