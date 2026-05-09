//! RF64 in-place promotion (EBU TECH 3306).
//!
//! When a recording crosses the configured threshold (default ~3.5 GB —
//! safely under the 4 GiB cap on a u32 chunk size), the writer patches
//! the file header in place: the 8-byte RIFF marker becomes RF64 with a
//! sentinel size, and the reserved 28-byte JUNK body becomes a ds64 body
//! carrying the true 64-bit sizes. No file rewrite, no data move.
//!
//! ds64 body layout (28 bytes, RF64 spec §3):
//!
//! ```text
//! 0..8    riff_size    u64 LE
//! 8..16   data_size    u64 LE
//! 16..24  sample_count u64 LE
//! 24..28  table_length u32 LE  (= 0 here; we have no extra chunks)
//! ```
//!
//! See `docs/modules/record-audio.md` §4.6.

const RF64_MAGIC: &[u8; 4] = b"RF64";
const DS64_MAGIC: &[u8; 4] = b"ds64";
const DS64_BODY_SIZE: u32 = 28;

/// Sentinel value written to the legacy 32-bit `chunk_size` and `data_size`
/// fields on RF64-promoted files. Real sizes live in the ds64 body.
pub(crate) const RF64_SIZE_SENTINEL: u32 = 0xFFFF_FFFF;

/// Build the 8-byte RF64 marker that overwrites `RIFF` + `chunk_size` at
/// file offset 0.
pub(crate) fn build_rf64_marker() -> [u8; 8] {
    let mut m = [0u8; 8];
    m[0..4].copy_from_slice(RF64_MAGIC);
    m[4..8].copy_from_slice(&RF64_SIZE_SENTINEL.to_le_bytes());
    m
}

/// Build the 36-byte ds64 chunk (4-byte id + 4-byte size + 28-byte body)
/// that overwrites `JUNK` + reserved zeros at file offset 12.
pub(crate) fn build_ds64_chunk(
    riff_size: u64,
    data_size: u64,
    sample_count: u64,
) -> [u8; 36] {
    let mut buf = [0u8; 36];
    buf[0..4].copy_from_slice(DS64_MAGIC);
    buf[4..8].copy_from_slice(&DS64_BODY_SIZE.to_le_bytes());
    buf[8..16].copy_from_slice(&riff_size.to_le_bytes());
    buf[16..24].copy_from_slice(&data_size.to_le_bytes());
    buf[24..32].copy_from_slice(&sample_count.to_le_bytes());
    buf[32..36].copy_from_slice(&0u32.to_le_bytes());
    buf
}
