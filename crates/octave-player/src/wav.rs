//! Minimal RIFF / RF64 WAVE header parser, restricted to the
//! 32-bit-float PCM layout `octave-recorder` writes.
//!
//! See `docs/modules/playback-audio.md` §5.2.
//!
//! What we parse:
//! - `RIFF` and `RF64` magic.
//! - `ds64` chunk (RF64 only) for true 64-bit `data_size`.
//! - `fmt ` chunk: `WAVE_FORMAT_IEEE_FLOAT` (0x0003) or
//!   `WAVE_FORMAT_EXTENSIBLE` (0xFFFE) with the
//!   `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` GUID.
//! - `data` chunk: byte offset and length.
//!
//! What we do NOT parse:
//! - Lossy-format `fmt ` (anything other than 32-bit float).
//! - `bext`, `cue `, `LIST`, etc. — silently skipped.
//! - Files where `data` precedes `fmt ` (technically legal but no
//!   real-world tool emits them; reject explicitly).

use std::io::{Read, Seek, SeekFrom};

use thiserror::Error;

const RIFF_MAGIC: [u8; 4] = *b"RIFF";
const RF64_MAGIC: [u8; 4] = *b"RF64";
const WAVE_FORM: [u8; 4] = *b"WAVE";
const DS64_ID: [u8; 4] = *b"ds64";
const FMT_ID: [u8; 4] = *b"fmt ";
const DATA_ID: [u8; 4] = *b"data";

const FORMAT_TAG_IEEE_FLOAT: u16 = 0x0003;
const FORMAT_TAG_EXTENSIBLE: u16 = 0xFFFE;
const RF64_SIZE_SENTINEL: u32 = 0xFFFF_FFFF;

/// First 4 bytes of `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`
/// (`00000003-0000-0010-8000-00aa00389b71`). The full 16-byte GUID is
/// distinctive in its first 4 bytes — we check those plus the rest of
/// the GUID for safety.
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00,
    0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// Result of parsing a WAVE header. Sufficient to seek and read
/// interleaved 32-bit float samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WavMeta {
    pub sample_rate: u32,
    pub channels: u16,
    /// Byte offset of the first sample of the `data` chunk's body.
    pub data_offset: u64,
    /// Length of the `data` chunk's body in bytes. For RF64 files this
    /// comes from the `ds64` chunk; for plain RIFF, from the `data`
    /// chunk header. Either way, frame_count = data_bytes / (channels * 4).
    pub data_bytes: u64,
}

impl WavMeta {
    pub fn frame_count(&self) -> u64 {
        let frame_size = u64::from(self.channels) * 4;
        self.data_bytes.checked_div(frame_size).unwrap_or(0)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("io: {0}")]
    Io(String),
    #[error("not a WAVE file: {0}")]
    NotWav(String),
    #[error("unsupported WAVE format: {0}")]
    Unsupported(String),
}

impl From<std::io::Error> for ParseError {
    fn from(e: std::io::Error) -> Self {
        ParseError::Io(e.to_string())
    }
}

/// Parse a WAVE / RF64 header from `r`. Leaves the reader positioned
/// somewhere inside the file (callers should `seek` to the data offset
/// themselves). Walks chunks linearly; tolerates and skips unknown
/// chunks before `data`. Errors fast on the first thing that isn't a
/// 32-bit float WAVE.
pub(crate) fn parse_header<R: Read + Seek>(r: &mut R) -> Result<WavMeta, ParseError> {
    // ---------- RIFF / RF64 envelope ----------
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    let is_rf64 = match magic {
        RIFF_MAGIC => false,
        RF64_MAGIC => true,
        _ => return Err(ParseError::NotWav(format!("magic = {magic:?}, expected RIFF or RF64"))),
    };

    // 4 bytes of legacy chunk_size (or sentinel for RF64) — we don't
    // need this; the per-chunk walk gives us authoritative sizes.
    let mut chunk_size_bytes = [0u8; 4];
    r.read_exact(&mut chunk_size_bytes)?;
    if is_rf64 && u32::from_le_bytes(chunk_size_bytes) != RF64_SIZE_SENTINEL {
        return Err(ParseError::NotWav(
            "RF64 chunk_size field is not 0xFFFFFFFF sentinel".into(),
        ));
    }

    let mut form = [0u8; 4];
    r.read_exact(&mut form)?;
    if form != WAVE_FORM {
        return Err(ParseError::NotWav(format!("form = {form:?}, expected WAVE")));
    }

    // ---------- chunk walk ----------
    let mut sample_rate: Option<u32> = None;
    let mut channels: Option<u16> = None;
    let mut data_bytes_from_ds64: Option<u64> = None;

    let (data_offset, data_bytes) = loop {
        let mut id = [0u8; 4];
        if let Err(e) = r.read_exact(&mut id) {
            return Err(ParseError::NotWav(format!("end of file before data chunk: {e}")));
        }
        let mut size_bytes = [0u8; 4];
        r.read_exact(&mut size_bytes)?;
        let size32 = u32::from_le_bytes(size_bytes);

        match id {
            DS64_ID => {
                if !is_rf64 {
                    return Err(ParseError::NotWav("ds64 chunk in non-RF64 file".into()));
                }
                if size32 < 28 {
                    return Err(ParseError::NotWav(format!("ds64 size {size32} < 28")));
                }
                // ds64 body: riff_size u64, data_size u64, sample_count u64,
                // table_length u32 (= 0). We only care about data_size.
                let mut body = [0u8; 28];
                r.read_exact(&mut body)?;
                let data_size = u64::from_le_bytes(body[8..16].try_into().unwrap());
                data_bytes_from_ds64 = Some(data_size);
                // Skip any extra bytes in the ds64 chunk beyond the
                // 28-byte body (tables we don't consume).
                if size32 > 28 {
                    skip_with_pad(r, u64::from(size32 - 28))?;
                }
            }
            FMT_ID => {
                let (sr, ch) = parse_fmt(r, size32)?;
                sample_rate = Some(sr);
                channels = Some(ch);
            }
            DATA_ID => {
                let offset = r.stream_position()?;
                let bytes = if is_rf64 && size32 == RF64_SIZE_SENTINEL {
                    data_bytes_from_ds64.ok_or_else(|| ParseError::NotWav(
                        "RF64 data chunk uses sentinel size but no ds64 chunk seen".into(),
                    ))?
                } else {
                    u64::from(size32)
                };
                break (offset, bytes);
            }
            _ => {
                // Unknown chunk — skip its body (with RIFF pad byte).
                skip_with_pad(r, u64::from(size32))?;
            }
        }
    };

    let sample_rate = sample_rate.ok_or_else(|| ParseError::NotWav(
        "no fmt chunk before data".into(),
    ))?;
    let channels = channels.ok_or_else(|| ParseError::NotWav(
        "no fmt chunk before data".into(),
    ))?;

    // Final alignment check: data_bytes must be a whole number of frames.
    let frame_size = u64::from(channels) * 4;
    if frame_size == 0 || data_bytes % frame_size != 0 {
        return Err(ParseError::NotWav(format!(
            "data_bytes {data_bytes} is not a whole number of {channels}-channel frames"
        )));
    }

    Ok(WavMeta {
        sample_rate,
        channels,
        data_offset,
        data_bytes,
    })
}

fn parse_fmt<R: Read + Seek>(r: &mut R, size: u32) -> Result<(u32, u16), ParseError> {
    if size < 16 {
        return Err(ParseError::NotWav(format!("fmt size {size} < 16")));
    }
    let mut base = [0u8; 16];
    r.read_exact(&mut base)?;
    let format_tag = u16::from_le_bytes([base[0], base[1]]);
    let channels = u16::from_le_bytes([base[2], base[3]]);
    let sample_rate = u32::from_le_bytes([base[4], base[5], base[6], base[7]]);
    // bytes 8..12: byte_rate (we don't validate)
    // bytes 12..14: block_align (we don't validate)
    let bits_per_sample = u16::from_le_bytes([base[14], base[15]]);

    if bits_per_sample != 32 {
        return Err(ParseError::Unsupported(format!(
            "bits_per_sample = {bits_per_sample}, expected 32"
        )));
    }

    match format_tag {
        FORMAT_TAG_IEEE_FLOAT => {
            // No extension bytes expected; skip any padding past the 16
            // we read (defensive — tools sometimes pad to even).
            if size > 16 {
                skip_with_pad(r, u64::from(size - 16))?;
            }
        }
        FORMAT_TAG_EXTENSIBLE => {
            // EXTENSIBLE: cb_size u16, valid_bits_per_sample u16,
            // channel_mask u32, subtype_guid 16 bytes. Total ext = 24.
            if size < 16 + 24 {
                return Err(ParseError::Unsupported(format!(
                    "EXTENSIBLE fmt size {size} < 40"
                )));
            }
            let mut ext = [0u8; 24];
            r.read_exact(&mut ext)?;
            let guid: [u8; 16] = ext[8..24].try_into().unwrap();
            if guid != KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                return Err(ParseError::Unsupported(
                    "EXTENSIBLE subtype is not KSDATAFORMAT_SUBTYPE_IEEE_FLOAT".into(),
                ));
            }
            // Skip any trailing fmt bytes past the 16+24 we consumed.
            if size > 40 {
                skip_with_pad(r, u64::from(size - 40))?;
            }
        }
        other => {
            return Err(ParseError::Unsupported(format!(
                "format_tag = 0x{other:04X}, expected IEEE_FLOAT (0x0003) or EXTENSIBLE (0xFFFE)"
            )));
        }
    }

    Ok((sample_rate, channels))
}

/// Skip `n` bytes plus the RIFF pad byte (chunks are aligned to 2).
fn skip_with_pad<R: Seek>(r: &mut R, n: u64) -> std::io::Result<()> {
    let pad = n & 1;
    r.seek(SeekFrom::Current(i64::try_from(n + pad).expect("chunk fits i64")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    /// Hand-build a minimal 80-byte plain RIFF header matching the
    /// recorder's writer, then append `audio` bytes. Not a full
    /// production header — just enough to drive the parser.
    fn build_riff_with_audio(sample_rate: u32, channels: u16, audio: &[u8]) -> Vec<u8> {
        let mut h = Vec::with_capacity(80 + audio.len());
        let block_align: u16 = channels * 4;
        let byte_rate: u32 = sample_rate * u32::from(block_align);
        let data_size: u32 = u32::try_from(audio.len()).unwrap();
        let chunk_size: u32 = 72 + data_size;

        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&chunk_size.to_le_bytes());
        h.extend_from_slice(b"WAVE");
        // JUNK reserve area, mirrors recorder layout.
        h.extend_from_slice(b"JUNK");
        h.extend_from_slice(&28u32.to_le_bytes());
        h.extend_from_slice(&[0u8; 28]);
        // fmt
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&FORMAT_TAG_IEEE_FLOAT.to_le_bytes());
        h.extend_from_slice(&channels.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&byte_rate.to_le_bytes());
        h.extend_from_slice(&block_align.to_le_bytes());
        h.extend_from_slice(&32u16.to_le_bytes());
        // data
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_size.to_le_bytes());

        h.write_all(audio).unwrap();
        h
    }

    fn build_rf64_with_audio(sample_rate: u32, channels: u16, audio: &[u8]) -> Vec<u8> {
        let mut h = Vec::with_capacity(80 + audio.len());
        let block_align: u16 = channels * 4;
        let byte_rate: u32 = sample_rate * u32::from(block_align);
        let data_size: u64 = audio.len() as u64;
        let frames: u64 = data_size / u64::from(block_align);
        let riff_size: u64 = 72 + data_size;

        h.extend_from_slice(b"RF64");
        h.extend_from_slice(&RF64_SIZE_SENTINEL.to_le_bytes());
        h.extend_from_slice(b"WAVE");
        // ds64 chunk (id + size + 28-byte body)
        h.extend_from_slice(b"ds64");
        h.extend_from_slice(&28u32.to_le_bytes());
        h.extend_from_slice(&riff_size.to_le_bytes());
        h.extend_from_slice(&data_size.to_le_bytes());
        h.extend_from_slice(&frames.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes()); // table_length
        // fmt
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&FORMAT_TAG_IEEE_FLOAT.to_le_bytes());
        h.extend_from_slice(&channels.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&byte_rate.to_le_bytes());
        h.extend_from_slice(&block_align.to_le_bytes());
        h.extend_from_slice(&32u16.to_le_bytes());
        // data with sentinel
        h.extend_from_slice(b"data");
        h.extend_from_slice(&RF64_SIZE_SENTINEL.to_le_bytes());

        h.write_all(audio).unwrap();
        h
    }

    #[test]
    fn parses_recorder_layout_riff_stereo_48k() {
        let audio: Vec<u8> = (0..16).flat_map(|i: u32| (i as f32).to_le_bytes()).collect();
        let bytes = build_riff_with_audio(48_000, 2, &audio);
        let mut c = Cursor::new(bytes);
        let meta = parse_header(&mut c).unwrap();
        assert_eq!(meta.sample_rate, 48_000);
        assert_eq!(meta.channels, 2);
        assert_eq!(meta.data_offset, 80);
        assert_eq!(meta.data_bytes, audio.len() as u64);
        assert_eq!(meta.frame_count(), 8); // 16 samples / 2 channels
    }

    #[test]
    fn parses_rf64_layout_with_ds64_data_size() {
        let audio: Vec<u8> = (0..40).flat_map(|i: u32| (i as f32).to_le_bytes()).collect();
        let bytes = build_rf64_with_audio(96_000, 2, &audio);
        let mut c = Cursor::new(bytes);
        let meta = parse_header(&mut c).unwrap();
        assert_eq!(meta.sample_rate, 96_000);
        assert_eq!(meta.channels, 2);
        // RF64 layout offset: 4(RF64) + 4(sentinel) + 4(WAVE) +
        // 4(ds64) + 4(28) + 28 + 4(fmt ) + 4(16) + 16 + 4(data) +
        // 4(sentinel) = 80 (same as RIFF since ds64 is sized to JUNK).
        assert_eq!(meta.data_offset, 80);
        assert_eq!(meta.data_bytes, audio.len() as u64);
    }

    #[test]
    fn rejects_wrong_magic() {
        let bytes = b"WAVEjunkjunkjunkjunk".to_vec();
        let mut c = Cursor::new(bytes);
        let err = parse_header(&mut c).unwrap_err();
        assert!(matches!(err, ParseError::NotWav(_)));
    }

    #[test]
    fn rejects_non_float_format_tag() {
        // Build a header with format_tag = 1 (PCM int) instead of 3.
        let mut bytes = build_riff_with_audio(48_000, 2, &[]);
        // format_tag is at offset 56 in the recorder layout.
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        let mut c = Cursor::new(bytes);
        let err = parse_header(&mut c).unwrap_err();
        assert!(matches!(err, ParseError::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn rejects_non_32_bit_format() {
        let mut bytes = build_riff_with_audio(48_000, 2, &[]);
        // bits_per_sample is at offset 70.
        bytes[70..72].copy_from_slice(&16u16.to_le_bytes());
        let mut c = Cursor::new(bytes);
        let err = parse_header(&mut c).unwrap_err();
        assert!(matches!(err, ParseError::Unsupported(_)));
    }

    #[test]
    fn rejects_truncated_header() {
        let bytes = b"RIFF\x00\x00\x00\x00WAV".to_vec(); // truncated
        let mut c = Cursor::new(bytes);
        assert!(parse_header(&mut c).is_err());
    }

    #[test]
    fn rejects_data_bytes_not_whole_frames() {
        // 10 bytes of audio, 2-channel stereo (8 bytes/frame) → not aligned.
        let audio = vec![0u8; 10];
        let bytes = build_riff_with_audio(48_000, 2, &audio);
        let mut c = Cursor::new(bytes);
        let err = parse_header(&mut c).unwrap_err();
        assert!(matches!(err, ParseError::NotWav(_)));
    }

    #[test]
    fn skips_unknown_chunks_before_data() {
        // Insert a fake "bext" chunk between fmt and data.
        let mut h = Vec::new();
        let audio: Vec<u8> = (0..8).flat_map(|i: u32| (i as f32).to_le_bytes()).collect();
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&0u32.to_le_bytes()); // chunk_size, not validated
        h.extend_from_slice(b"WAVE");
        // fmt first (clean 16-byte fmt chunk)
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&FORMAT_TAG_IEEE_FLOAT.to_le_bytes());
        h.extend_from_slice(&2u16.to_le_bytes());        // channels
        h.extend_from_slice(&48_000u32.to_le_bytes());   // sample_rate
        h.extend_from_slice(&(48_000u32 * 8).to_le_bytes()); // byte_rate
        h.extend_from_slice(&8u16.to_le_bytes());        // block_align
        h.extend_from_slice(&32u16.to_le_bytes());       // bits
        // bext chunk with arbitrary 5-byte payload + 1-byte RIFF pad
        h.extend_from_slice(b"bext");
        h.extend_from_slice(&5u32.to_le_bytes());
        h.extend_from_slice(&[0u8, 1, 2, 3, 4]);
        h.push(0u8); // RIFF pad (chunks align to 2)
        // data
        h.extend_from_slice(b"data");
        h.extend_from_slice(&u32::try_from(audio.len()).unwrap().to_le_bytes());
        h.extend_from_slice(&audio);

        let mut c = Cursor::new(h);
        let meta = parse_header(&mut c).unwrap();
        assert_eq!(meta.channels, 2);
        assert_eq!(meta.frame_count(), 4);
    }
}
