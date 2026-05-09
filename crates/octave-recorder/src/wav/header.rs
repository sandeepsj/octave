//! Byte layout for the 80-byte plain RIFF/WAVE header.
//!
//! Layout (offsets in bytes):
//!
//! ```text
//!  0..4   "RIFF"           — patched to "RF64" on promotion
//!  4..8   chunk_size u32   — patched at finalize, or 0xFFFFFFFF for RF64
//!  8..12  "WAVE"
//! 12..16  "JUNK"           — repurposed as "ds64" on promotion
//! 16..20  body_size u32 = 28
//! 20..48  zeros            — repurposed as ds64 body on promotion
//! 48..52  "fmt "
//! 52..56  fmt_size u32 = 16
//! 56..58  format_tag u16 = 0x0003 (IEEE_FLOAT)
//! 58..60  channels u16
//! 60..64  sample_rate u32
//! 64..68  byte_rate u32 = sample_rate * channels * 4
//! 68..70  block_align u16 = channels * 4
//! 70..72  bits_per_sample u16 = 32
//! 72..76  "data"
//! 76..80  data_size u32    — patched at finalize, or 0xFFFFFFFF for RF64
//! ```
//!
//! See `docs/modules/record-audio.md` §4.5.

const RIFF: &[u8; 4] = b"RIFF";
const WAVE: &[u8; 4] = b"WAVE";
const JUNK: &[u8; 4] = b"JUNK";
const FMT_: &[u8; 4] = b"fmt ";
const DATA: &[u8; 4] = b"data";

const FORMAT_TAG_IEEE_FLOAT: u16 = 0x0003;
const BITS_PER_SAMPLE: u16 = 32;
const FMT_SUBCHUNK_SIZE: u32 = 16;
const JUNK_RESERVED_BODY_SIZE: u32 = 28;

pub(crate) const HEADER_SIZE: u64 = 80;

/// Build the 80-byte initial header. Size fields are placeholders; the
/// writer patches them in [`crate::wav::WavWriter::finalize`] (and in
/// [`crate::wav::WavWriter::promote_to_rf64`] when the file crosses
/// the RF64 threshold).
pub(crate) fn build_initial_header(sample_rate: u32, channels: u16) -> [u8; 80] {
    let mut h = [0u8; 80];
    let block_align: u16 = channels * 4;
    let byte_rate: u32 = sample_rate * u32::from(block_align);

    h[0..4].copy_from_slice(RIFF);
    h[4..8].copy_from_slice(&0u32.to_le_bytes());
    h[8..12].copy_from_slice(WAVE);
    h[12..16].copy_from_slice(JUNK);
    h[16..20].copy_from_slice(&JUNK_RESERVED_BODY_SIZE.to_le_bytes());
    // h[20..48] stays zero; gets repurposed as ds64 body on promotion.
    h[48..52].copy_from_slice(FMT_);
    h[52..56].copy_from_slice(&FMT_SUBCHUNK_SIZE.to_le_bytes());
    h[56..58].copy_from_slice(&FORMAT_TAG_IEEE_FLOAT.to_le_bytes());
    h[58..60].copy_from_slice(&channels.to_le_bytes());
    h[60..64].copy_from_slice(&sample_rate.to_le_bytes());
    h[64..68].copy_from_slice(&byte_rate.to_le_bytes());
    h[68..70].copy_from_slice(&block_align.to_le_bytes());
    h[70..72].copy_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    h[72..76].copy_from_slice(DATA);
    h[76..80].copy_from_slice(&0u32.to_le_bytes());
    h
}

/// Compute the legacy 32-bit `chunk_size` field for a plain RIFF file.
///
/// `chunk_size = file_size - 8 = HEADER_SIZE - 8 + audio_bytes = 72 + audio_bytes`.
/// Caller must ensure `72 + audio_bytes <= u32::MAX`; guaranteed when RF64
/// promotion happens before the threshold (default 3.5 GB) is crossed.
pub(crate) fn riff_chunk_size(audio_bytes: u64) -> u32 {
    u32::try_from(72u64 + audio_bytes)
        .expect("riff_chunk_size: caller must promote to RF64 before audio exceeds u32::MAX - 72")
}
