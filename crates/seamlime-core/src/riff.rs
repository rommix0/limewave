//! RIFF/WAVE chunk layer: splits a WAV file into raw chunks and writes them
//! back, preserving unknown chunks and their order.

use crate::error::WavError;

/// One RIFF chunk: a four-byte id and its payload (without header or padding).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub id: [u8; 4],
    pub data: Vec<u8>,
}

/// A parsed RIFF/WAVE file. The form type is always `WAVE`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RiffFile {
    pub chunks: Vec<Chunk>,
    /// Lenient fixes applied while parsing.
    pub warnings: Vec<String>,
}

const HEADER_LEN: usize = 12;
const CHUNK_HEADER_LEN: usize = 8;

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn id_text(id: &[u8; 4]) -> String {
    String::from_utf8_lossy(id).into_owned()
}

/// Splits a WAV file into chunks. Tolerates a missing final pad byte, chunk
/// sizes that run past the end of the data, and a wrong RIFF size.
pub fn parse_riff(bytes: &[u8]) -> Result<RiffFile, WavError> {
    if bytes.starts_with(b"RF64") {
        return Err(WavError::Rf64Unsupported);
    }
    if bytes.len() < HEADER_LEN || &bytes[0..4] != b"RIFF" {
        return Err(WavError::NotRiff);
    }
    if &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWave);
    }

    let mut warnings = Vec::new();
    let riff_size = read_u32(&bytes[4..8]) as usize;
    let declared_end = riff_size.saturating_add(CHUNK_HEADER_LEN);
    let end = if declared_end < HEADER_LEN || declared_end > bytes.len() {
        warnings.push(format!(
            "RIFF size {} does not match the file length {}; using the file length",
            riff_size,
            bytes.len()
        ));
        bytes.len()
    } else {
        if declared_end < bytes.len() {
            warnings.push(format!(
                "ignored {} trailing bytes after the RIFF data",
                bytes.len() - declared_end
            ));
        }
        declared_end
    };

    let mut chunks = Vec::new();
    let mut pos = HEADER_LEN;
    while pos + CHUNK_HEADER_LEN <= end {
        let id = [bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]];
        let declared = read_u32(&bytes[pos + 4..pos + 8]) as usize;
        let start = pos + CHUNK_HEADER_LEN;
        let available = end - start;
        let len = if declared > available {
            warnings.push(format!(
                "chunk '{}' claims {} bytes but only {} remain; clamped",
                id_text(&id),
                declared,
                available
            ));
            available
        } else {
            declared
        };
        chunks.push(Chunk {
            id,
            data: bytes[start..start + len].to_vec(),
        });
        pos = start + len;
        if len % 2 == 1 {
            if pos < end {
                pos += 1;
            } else if len == declared {
                warnings.push(format!(
                    "chunk '{}' is missing its final pad byte",
                    id_text(&id)
                ));
            }
        }
    }
    if pos < end {
        warnings.push(format!("ignored {} stray bytes at the end", end - pos));
    }

    Ok(RiffFile { chunks, warnings })
}

/// Serialises chunks as a RIFF/WAVE file, padding odd-sized chunks to an even
/// length and recomputing the RIFF size.
pub fn write_riff(file: &RiffFile) -> Vec<u8> {
    let body_len: usize = file
        .chunks
        .iter()
        .map(|c| CHUNK_HEADER_LEN + c.data.len() + (c.data.len() & 1))
        .sum();
    let mut out = Vec::with_capacity(HEADER_LEN + body_len);
    out.extend_from_slice(b"RIFF");
    let riff_size = u32::try_from(4 + body_len).unwrap_or(u32::MAX);
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    for chunk in &file.chunks {
        out.extend_from_slice(&chunk.id);
        let size = u32::try_from(chunk.data.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&chunk.data);
        if chunk.data.len() % 2 == 1 {
            out.push(0);
        }
    }
    out
}

/// The list type (the four bytes after the id) of a `LIST` chunk.
pub fn list_type(chunk: &Chunk) -> Option<[u8; 4]> {
    if &chunk.id != b"LIST" || chunk.data.len() < 4 {
        return None;
    }
    Some([chunk.data[0], chunk.data[1], chunk.data[2], chunk.data[3]])
}

/// Index of the first chunk with the given id.
pub fn find(chunks: &[Chunk], id: &[u8; 4]) -> Option<usize> {
    chunks.iter().position(|c| &c.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_bytes(id: &[u8; 4], data: &[u8], pad: bool) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        if pad && data.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn wave(body: &[u8]) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn parse_minimal_wave() {
        let mut body = chunk_bytes(b"fmt ", &[1; 16], true);
        body.extend(chunk_bytes(b"data", &[9, 8, 7, 6], true));
        let file = parse_riff(&wave(&body)).unwrap();
        assert_eq!(file.chunks.len(), 2);
        assert_eq!(&file.chunks[0].id, b"fmt ");
        assert_eq!(file.chunks[0].data, vec![1; 16]);
        assert_eq!(&file.chunks[1].id, b"data");
        assert_eq!(file.chunks[1].data, vec![9, 8, 7, 6]);
        assert!(file.warnings.is_empty());
    }

    #[test]
    fn roundtrip_preserves_unknown_chunks_and_order() {
        let mut body = chunk_bytes(b"fmt ", &[1; 16], true);
        body.extend(chunk_bytes(b"abcd", &[1, 2, 3], true));
        body.extend(chunk_bytes(b"data", &[9, 8, 7, 6], true));
        body.extend(chunk_bytes(b"zzzz", &[5, 5], true));
        let bytes = wave(&body);
        let file = parse_riff(&bytes).unwrap();
        assert_eq!(&file.chunks[1].id, b"abcd");
        assert_eq!(file.chunks[1].data, vec![1, 2, 3]);
        assert_eq!(write_riff(&file), bytes);
    }

    #[test]
    fn missing_pad_byte_is_tolerated() {
        let mut body = chunk_bytes(b"fmt ", &[1; 16], true);
        body.extend(chunk_bytes(b"abcd", &[1, 2, 3], false));
        let file = parse_riff(&wave(&body)).unwrap();
        assert_eq!(file.chunks.len(), 2);
        assert_eq!(file.chunks[1].data, vec![1, 2, 3]);
        assert!(!file.warnings.is_empty());
    }

    #[test]
    fn oversized_chunk_is_clamped() {
        let mut body = chunk_bytes(b"fmt ", &[1; 16], true);
        let mut data = chunk_bytes(b"data", &[1, 2, 3, 4], true);
        data[4..8].copy_from_slice(&1000u32.to_le_bytes());
        body.extend(data);
        let file = parse_riff(&wave(&body)).unwrap();
        assert_eq!(file.chunks.len(), 2);
        assert_eq!(file.chunks[1].data, vec![1, 2, 3, 4]);
        assert!(!file.warnings.is_empty());
    }

    #[test]
    fn rejects_non_riff_and_rf64() {
        assert_eq!(
            parse_riff(b"RF64\0\0\0\0WAVEfmt ").unwrap_err(),
            WavError::Rf64Unsupported
        );
        assert_eq!(
            parse_riff(b"this is definitely not a wav file").unwrap_err(),
            WavError::NotRiff
        );
        assert_eq!(parse_riff(b"").unwrap_err(), WavError::NotRiff);
        assert_eq!(
            parse_riff(b"RIFF\x04\0\0\0AVI ").unwrap_err(),
            WavError::NotWave
        );
    }

    #[test]
    fn list_type_reads_info() {
        let list = Chunk {
            id: *b"LIST",
            data: b"INFOISFT\x02\0\0\0a\0".to_vec(),
        };
        assert_eq!(list_type(&list), Some(*b"INFO"));
        let other = Chunk {
            id: *b"data",
            data: b"INFO".to_vec(),
        };
        assert_eq!(list_type(&other), None);
        let short = Chunk {
            id: *b"LIST",
            data: vec![1, 2],
        };
        assert_eq!(list_type(&short), None);
    }

    #[test]
    fn find_returns_first_match() {
        let chunks = vec![
            Chunk {
                id: *b"fmt ",
                data: vec![],
            },
            Chunk {
                id: *b"data",
                data: vec![],
            },
        ];
        assert_eq!(find(&chunks, b"data"), Some(1));
        assert_eq!(find(&chunks, b"smpl"), None);
    }
}
