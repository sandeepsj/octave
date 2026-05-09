//! 32-bit float WAV writer with in-place RF64 promotion.
//!
//! The writer thread owns one of these per recording. It's pure-data: no
//! threads, no RT-safety concern, no audio backend coupling. Performance
//! comes from passing interleaved `f32` slices directly through a single
//! `write` call — `bytemuck::cast_slice` makes the slice→bytes view free
//! on little-endian platforms (compile-time asserted).
//!
//! See `docs/modules/record-audio.md` §3.5 (writer-thread loop), §4.5
//! (header layout), and §4.6 (RF64 auto-promotion).

mod header;
mod rf64;

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use bytemuck::cast_slice;

use header::{HEADER_SIZE, build_initial_header, riff_chunk_size};
use rf64::{RF64_SIZE_SENTINEL, build_ds64_chunk, build_rf64_marker};

/// Default RF64 promotion threshold: 3.5 GB. The 4 GiB cap on a u32
/// `chunk_size` field is the hard ceiling; we promote with a half-gig
/// safety margin so a single in-flight buffer can't push us past it.
pub(crate) const DEFAULT_RF64_THRESHOLD_BYTES: u64 = 3_500_000_000;

const _: () = assert!(
    cfg!(target_endian = "little"),
    "octave-recorder WAV writer relies on native f32 layout matching WAV's little-endian on-disk format",
);

/// Writer for a 32-bit float interleaved WAV file (mono or stereo for v0.1).
///
/// Reserves a 36-byte ds64-shaped chunk at file open as a JUNK chunk so
/// promotion to RF64 is a 44-byte in-place patch with no data move.
pub(crate) struct WavWriter {
    file: File,
    channels: u16,
    frames_written: u64,
    rf64_threshold_bytes: u64,
    promoted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FinalizedWav {
    pub frame_count: u64,
    pub bytes_written: u64,
    pub promoted_to_rf64: bool,
}

impl WavWriter {
    /// Create a new file with the default 3.5 GB RF64 promotion threshold.
    pub(crate) fn create(path: &Path, sample_rate: u32, channels: u16) -> io::Result<Self> {
        Self::create_with_threshold(path, sample_rate, channels, DEFAULT_RF64_THRESHOLD_BYTES)
    }

    /// Create a new file with a custom RF64 promotion threshold.
    /// Used by tests to trigger promotion without writing 3.5 GB.
    pub(crate) fn create_with_threshold(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        rf64_threshold_bytes: u64,
    ) -> io::Result<Self> {
        assert!(
            (1..=2).contains(&channels),
            "v0.1 plain header supports 1 or 2 channels; EXTENSIBLE form for ≥3 channels lands next turn",
        );
        let mut file = File::create(path)?;
        file.write_all(&build_initial_header(sample_rate, channels))?;
        Ok(Self {
            file,
            channels,
            frames_written: 0,
            rf64_threshold_bytes,
            promoted: false,
        })
    }

    /// Append interleaved `f32` frames. The slice length must be a whole
    /// multiple of `channels`. Triggers RF64 promotion in place once the
    /// running file size crosses the threshold.
    pub(crate) fn write_frames(&mut self, samples: &[f32]) -> io::Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let ch = usize::from(self.channels);
        debug_assert_eq!(samples.len() % ch, 0, "samples must contain whole frames");

        let bytes: &[u8] = cast_slice(samples);
        self.file.write_all(bytes)?;
        self.frames_written += (samples.len() / ch) as u64;

        if !self.promoted && self.total_bytes() >= self.rf64_threshold_bytes {
            self.promote_to_rf64()?;
        }
        Ok(())
    }

    /// Patch size fields, fsync, return summary.
    pub(crate) fn finalize(mut self) -> io::Result<FinalizedWav> {
        self.patch_size_fields()?;
        self.file.sync_all()?;
        Ok(FinalizedWav {
            frame_count: self.frames_written,
            bytes_written: self.total_bytes(),
            promoted_to_rf64: self.promoted,
        })
    }

    pub(crate) fn channels(&self) -> u16 {
        self.channels
    }

    fn audio_byte_count(&self) -> u64 {
        self.frames_written * u64::from(self.channels) * 4
    }

    fn total_bytes(&self) -> u64 {
        HEADER_SIZE + self.audio_byte_count()
    }

    fn promote_to_rf64(&mut self) -> io::Result<()> {
        let audio_bytes = self.audio_byte_count();
        let riff_size = HEADER_SIZE - 8 + audio_bytes;

        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&build_rf64_marker())?;

        self.file.seek(SeekFrom::Start(12))?;
        self.file.write_all(&build_ds64_chunk(riff_size, audio_bytes, self.frames_written))?;

        self.file.seek(SeekFrom::End(0))?;
        self.promoted = true;
        Ok(())
    }

    fn patch_size_fields(&mut self) -> io::Result<()> {
        let audio_bytes = self.audio_byte_count();

        if self.promoted {
            let riff_size = HEADER_SIZE - 8 + audio_bytes;
            self.file.seek(SeekFrom::Start(12))?;
            self.file.write_all(&build_ds64_chunk(riff_size, audio_bytes, self.frames_written))?;
            self.file.seek(SeekFrom::Start(76))?;
            self.file.write_all(&RF64_SIZE_SENTINEL.to_le_bytes())?;
        } else {
            let data_size = u32::try_from(audio_bytes)
                .expect("plain RIFF: audio_bytes must fit u32 (RF64 threshold guards this)");
            self.file.seek(SeekFrom::Start(4))?;
            self.file.write_all(&riff_chunk_size(audio_bytes).to_le_bytes())?;
            self.file.seek(SeekFrom::Start(76))?;
            self.file.write_all(&data_size.to_le_bytes())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    use tempfile::tempdir;

    use crate::test_support::sine_stereo;

    #[test]
    fn round_trip_stereo_through_hound_is_bit_exact() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sine.wav");

        let frames = sine_stereo(480, 48_000, 440.0);
        let mut w = WavWriter::create(&path, 48_000, 2).unwrap();
        w.write_frames(&frames).unwrap();
        let fin = w.finalize().unwrap();

        assert_eq!(fin.frame_count, 480);
        assert!(!fin.promoted_to_rf64);
        assert_eq!(fin.bytes_written, 80 + 480 * 2 * 4);

        let mut reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, hound::SampleFormat::Float);

        let read: Vec<f32> = reader
            .samples::<f32>()
            .map(Result::unwrap)
            .collect();
        assert_eq!(read.len(), frames.len());
        for (i, (got, want)) in read.iter().zip(frames.iter()).enumerate() {
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "sample {i}: got {got:?} want {want:?}",
            );
        }
    }

    #[test]
    fn empty_recording_finalizes_to_a_valid_zero_sample_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.wav");

        let w = WavWriter::create(&path, 48_000, 2).unwrap();
        let fin = w.finalize().unwrap();

        assert_eq!(fin.frame_count, 0);
        assert_eq!(fin.bytes_written, 80);
        assert!(!fin.promoted_to_rf64);

        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.len(), 0);
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_rate, 48_000);
    }

    #[test]
    fn rf64_promotion_patches_header_in_place() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rf64.wav");

        // Threshold of 1024 bytes promotes after the first write that
        // pushes total_bytes (header 80 + audio) past 1024.
        let mut w = WavWriter::create_with_threshold(&path, 48_000, 2, 1024).unwrap();
        let frames = sine_stereo(500, 48_000, 220.0);
        w.write_frames(&frames).unwrap();
        let fin = w.finalize().unwrap();

        assert!(fin.promoted_to_rf64);
        assert_eq!(fin.frame_count, 500);
        let audio_bytes: u64 = 500 * 2 * 4;
        assert_eq!(fin.bytes_written, 80 + audio_bytes);

        let mut bytes = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut bytes).unwrap();

        assert_eq!(&bytes[0..4], b"RF64");
        assert_eq!(&bytes[4..8], &0xFFFF_FFFFu32.to_le_bytes());
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"ds64");
        assert_eq!(&bytes[16..20], &28u32.to_le_bytes());

        let riff_size = u64::from_le_bytes(bytes[20..28].try_into().unwrap());
        assert_eq!(riff_size, 72 + audio_bytes);

        let data_size = u64::from_le_bytes(bytes[28..36].try_into().unwrap());
        assert_eq!(data_size, audio_bytes);

        let sample_count = u64::from_le_bytes(bytes[36..44].try_into().unwrap());
        assert_eq!(sample_count, 500);

        let table_length = u32::from_le_bytes(bytes[44..48].try_into().unwrap());
        assert_eq!(table_length, 0);

        let legacy_data_size = u32::from_le_bytes(bytes[76..80].try_into().unwrap());
        assert_eq!(legacy_data_size, 0xFFFF_FFFF);
    }

    #[test]
    fn rf64_promotion_does_not_lose_audio_bytes() {
        // Promotion seeks back to write the header, then must seek to EOF
        // before continuing. This regression test writes more samples
        // *after* promotion and asserts the audio data is intact.
        let dir = tempdir().unwrap();
        let path = dir.path().join("rf64-continue.wav");

        let mut w = WavWriter::create_with_threshold(&path, 48_000, 2, 1024).unwrap();
        let pre = sine_stereo(200, 48_000, 220.0);
        let post = sine_stereo(300, 48_000, 880.0);
        w.write_frames(&pre).unwrap();
        w.write_frames(&post).unwrap();
        let fin = w.finalize().unwrap();

        assert!(fin.promoted_to_rf64);
        assert_eq!(fin.frame_count, 500);

        let mut bytes = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut bytes).unwrap();

        let audio: &[f32] = cast_slice(&bytes[80..]);
        assert_eq!(audio.len(), 500 * 2);
        let mut expected = pre;
        expected.extend_from_slice(&post);
        for (i, (got, want)) in audio.iter().zip(expected.iter()).enumerate() {
            assert_eq!(got.to_bits(), want.to_bits(), "sample {i}");
        }
    }

    #[test]
    fn plain_riff_size_fields_match_audio_bytes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("plain.wav");

        let frames = sine_stereo(1000, 48_000, 440.0);
        let mut w = WavWriter::create(&path, 48_000, 2).unwrap();
        w.write_frames(&frames).unwrap();
        let _ = w.finalize().unwrap();

        let mut bytes = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut bytes).unwrap();

        let audio_bytes: u32 = 1000 * 2 * 4;
        assert_eq!(&bytes[0..4], b"RIFF");
        let chunk_size = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(chunk_size, 72 + audio_bytes);
        let data_size = u32::from_le_bytes(bytes[76..80].try_into().unwrap());
        assert_eq!(data_size, audio_bytes);
    }
}
