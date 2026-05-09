//! [`FileSource`] — reads 32-bit float WAV / RF64 files written by
//! `octave-recorder` (or any tool emitting the same byte layout).
//!
//! See `docs/modules/playback-audio.md` §5.1 (trait), §5.2 (parser).

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::source::{PlaybackSource, SeekError};
use crate::wav::{ParseError, WavMeta, parse_header};

/// Errors returned by [`FileSource::open`].
#[derive(Debug, Error)]
pub enum OpenFileError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a usable WAV: {0}")]
    Parse(#[from] ParseError),
}

/// Streams 32-bit float interleaved samples from a WAV / RF64 file on
/// disk. The reader is buffered (`BufReader`) so the per-pull syscall
/// cost amortizes across many `pull` calls.
#[derive(Debug)]
pub struct FileSource {
    reader: BufReader<File>,
    meta: WavMeta,
    /// Frame index of the next frame to be returned by `pull`.
    cursor: u64,
    /// Memoized total frames; identical to `meta.frame_count()`.
    total_frames: u64,
    /// Path retained for error messages; not used after open.
    #[allow(dead_code)]
    path: PathBuf,
}

impl FileSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OpenFileError> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let mut reader = BufReader::new(file);
        let meta = parse_header(&mut reader)?;
        // Position the reader at the first sample of the data chunk so
        // the very next `pull` reads from there.
        reader.seek(SeekFrom::Start(meta.data_offset))?;
        let total_frames = meta.frame_count();
        Ok(Self {
            reader,
            meta,
            cursor: 0,
            total_frames,
            path,
        })
    }

    fn frame_size_bytes(&self) -> u64 {
        u64::from(self.meta.channels) * 4
    }
}

impl PlaybackSource for FileSource {
    fn pull(&mut self, dst: &mut [f32]) -> usize {
        let ch = usize::from(self.meta.channels);
        debug_assert_eq!(dst.len() % ch, 0, "dst must be a whole number of frames");

        if self.cursor >= self.total_frames {
            return 0;
        }
        let max_frames_dst = dst.len() / ch;
        // total_frames - cursor fits in u64; saturating into usize is
        // safe because pull is bounded by `dst.len() / ch ≤ usize::MAX`
        // and we min the two below.
        let remaining = usize::try_from(self.total_frames - self.cursor).unwrap_or(usize::MAX);
        let frames = max_frames_dst.min(remaining);
        if frames == 0 {
            return 0;
        }

        // Decode interleaved 32-bit float LE one sample at a time.
        // BufReader amortizes the syscall cost; if a future profile
        // shows the per-sample loop matters, switch to a single
        // `read_exact` into a stack/heap byte buffer plus chunk
        // decode. For now: keep it obvious.
        let dst_slice = &mut dst[..frames * ch];
        let mut buf = [0u8; 4];
        let mut written = 0usize;
        for slot in dst_slice.iter_mut() {
            if self.reader.read_exact(&mut buf).is_err() {
                // Truncated file mid-stream — stop where we are.
                break;
            }
            *slot = f32::from_le_bytes(buf);
            written += 1;
        }
        let frames_written = written / ch;
        debug_assert_eq!(written % ch, 0, "partial frame read; file truncated mid-frame?");
        self.cursor += frames_written as u64;
        frames_written
    }

    fn seek(&mut self, frame: u64) -> Result<(), SeekError> {
        if frame > self.total_frames {
            return Err(SeekError::OutOfBounds {
                requested: frame,
                max: self.total_frames,
            });
        }
        let byte_offset = self.meta.data_offset + frame * self.frame_size_bytes();
        self.reader
            .seek(SeekFrom::Start(byte_offset))
            .map_err(|e| SeekError::Io(e.to_string()))?;
        self.cursor = frame;
        Ok(())
    }

    fn duration_frames(&self) -> Option<u64> {
        Some(self.total_frames)
    }

    fn sample_rate(&self) -> u32 {
        self.meta.sample_rate
    }

    fn channels(&self) -> u16 {
        self.meta.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use tempfile::NamedTempFile;

    fn write_hound_wav(path: &Path, sample_rate: u32, channels: u16, frames: usize) -> Vec<f32> {
        let spec = WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut w = WavWriter::create(path, spec).unwrap();
        let mut samples = Vec::with_capacity(frames * usize::from(channels));
        for i in 0..frames {
            for c in 0..channels {
                let v = (i as f32) * 0.001 + (c as f32) * 0.5;
                w.write_sample(v).unwrap();
                samples.push(v);
            }
        }
        w.finalize().unwrap();
        samples
    }

    #[test]
    fn opens_hound_written_stereo_and_pulls_all_frames() {
        let f = NamedTempFile::new().unwrap();
        let expected = write_hound_wav(f.path(), 48_000, 2, 100);
        let mut src = FileSource::open(f.path()).unwrap();
        assert_eq!(src.sample_rate(), 48_000);
        assert_eq!(src.channels(), 2);
        assert_eq!(src.duration_frames(), Some(100));

        let mut got = vec![0.0f32; 200];
        let frames = src.pull(&mut got);
        assert_eq!(frames, 100);
        assert_eq!(got, expected);

        // Second pull at EOF returns 0.
        let mut after = vec![0.0f32; 8];
        assert_eq!(src.pull(&mut after), 0);
    }

    #[test]
    fn pull_in_chunks_concatenates_correctly() {
        let f = NamedTempFile::new().unwrap();
        let expected = write_hound_wav(f.path(), 48_000, 2, 50);
        let mut src = FileSource::open(f.path()).unwrap();
        let mut acc = Vec::with_capacity(expected.len());
        let mut chunk = vec![0.0f32; 6]; // 3 stereo frames at a time
        loop {
            let frames = src.pull(&mut chunk);
            if frames == 0 {
                break;
            }
            acc.extend_from_slice(&chunk[..frames * 2]);
        }
        assert_eq!(acc, expected);
    }

    #[test]
    fn seek_to_zero_replays_from_start() {
        let f = NamedTempFile::new().unwrap();
        let expected = write_hound_wav(f.path(), 48_000, 2, 20);
        let mut src = FileSource::open(f.path()).unwrap();

        let mut buf = vec![0.0f32; 40];
        let _ = src.pull(&mut buf);
        src.seek(0).unwrap();
        let mut buf2 = vec![0.0f32; 40];
        let frames = src.pull(&mut buf2);
        assert_eq!(frames, 20);
        assert_eq!(buf2, expected);
    }

    #[test]
    fn seek_to_middle_resumes_correctly() {
        let f = NamedTempFile::new().unwrap();
        let expected = write_hound_wav(f.path(), 48_000, 2, 30);
        let mut src = FileSource::open(f.path()).unwrap();

        src.seek(10).unwrap();
        let mut buf = vec![0.0f32; 40]; // ask for 20 frames
        let frames = src.pull(&mut buf);
        assert_eq!(frames, 20); // only 20 left after frame 10
        // Frames 10..30 of `expected` should match.
        assert_eq!(buf[..40], expected[20..60]);
    }

    #[test]
    fn seek_past_end_is_out_of_bounds() {
        let f = NamedTempFile::new().unwrap();
        let _ = write_hound_wav(f.path(), 48_000, 2, 10);
        let mut src = FileSource::open(f.path()).unwrap();
        let err = src.seek(11).unwrap_err();
        assert_eq!(err, SeekError::OutOfBounds { requested: 11, max: 10 });
    }

    #[test]
    fn seek_to_exact_eof_is_allowed_and_pulls_zero() {
        let f = NamedTempFile::new().unwrap();
        let _ = write_hound_wav(f.path(), 48_000, 2, 10);
        let mut src = FileSource::open(f.path()).unwrap();
        src.seek(10).unwrap();
        let mut buf = vec![0.0f32; 8];
        assert_eq!(src.pull(&mut buf), 0);
    }

    #[test]
    fn open_rejects_nonexistent_file() {
        let err = FileSource::open("/tmp/does-not-exist-octave-test.wav").unwrap_err();
        assert!(matches!(err, OpenFileError::Io(_)));
    }

    #[test]
    fn open_rejects_non_wav_file() {
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), b"not a wav file at all").unwrap();
        let err = FileSource::open(f.path()).unwrap_err();
        assert!(matches!(err, OpenFileError::Parse(_)));
    }

    #[test]
    fn mono_file_round_trips() {
        let f = NamedTempFile::new().unwrap();
        let expected = write_hound_wav(f.path(), 44_100, 1, 17);
        let mut src = FileSource::open(f.path()).unwrap();
        assert_eq!(src.channels(), 1);
        assert_eq!(src.sample_rate(), 44_100);
        let mut buf = vec![0.0f32; 17];
        let frames = src.pull(&mut buf);
        assert_eq!(frames, 17);
        assert_eq!(buf, expected);
    }
}
