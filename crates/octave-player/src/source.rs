//! Source-agnostic frame supplier for the playback reader thread.
//!
//! The reader thread (added in a later commit) is unaware of where its
//! samples come from — it pulls through this trait. v1 ships two impls:
//! [`BufferSource`] (in-memory `Arc<[f32]>`) here, and `FileSource`
//! (32-bit float WAV / RF64) in a follow-up commit.
//!
//! See `docs/modules/playback-audio.md` §5.1.

use std::sync::Arc;

use thiserror::Error;

/// Errors a [`PlaybackSource`] may return when asked to seek.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SeekError {
    /// The source can not seek (streaming source with no random access).
    #[error("source is not seekable")]
    NotSeekable,
    /// The requested frame is beyond the source's end.
    #[error("seek out of bounds: requested {requested}, max {max}")]
    OutOfBounds {
        requested: u64,
        max: u64,
    },
    /// The underlying I/O failed (file source).
    #[error("source seek failed: {0}")]
    Io(String),
}

/// A frame supplier consumed by the reader thread.
///
/// All v1 impls produce **interleaved 32-bit float** at a fixed sample
/// rate and channel count declared at construction. The trait is
/// deliberately narrow: no concept of "playing", "paused", or
/// "transport" — those live in the playback handle.
pub trait PlaybackSource: Send {
    /// Pull up to `dst.len() / channels` frames into `dst` (interleaved).
    /// Returns the number of **frames** actually written. Returning 0
    /// signals end-of-source.
    ///
    /// `dst.len()` must be a whole multiple of `channels()`; the
    /// implementation may panic in debug builds if it isn't.
    fn pull(&mut self, dst: &mut [f32]) -> usize;

    /// Reposition the read cursor to the given frame index.
    /// `frame == duration_frames()` is allowed and means "seek to EOF".
    fn seek(&mut self, frame: u64) -> Result<(), SeekError>;

    /// Total frame count if known; `None` for unbounded streaming sources.
    fn duration_frames(&self) -> Option<u64>;

    /// Sample rate this source produces at, in Hz.
    fn sample_rate(&self) -> u32;

    /// Number of interleaved channels per frame.
    fn channels(&self) -> u16;
}

/// In-memory `Arc<[f32]>` source. The shared backing buffer lives at
/// least as long as the source — callers may drop their `Arc` clone
/// after handing one to `BufferSource::new` without freeing the audio.
///
/// Used by the future mix engine as one of its outputs and by tests
/// that need a deterministic, repeatable sample stream.
pub struct BufferSource {
    samples: Arc<[f32]>,
    sample_rate: u32,
    channels: u16,
    /// Frame index of the next frame to be returned by `pull`.
    cursor: u64,
}

impl BufferSource {
    /// `samples.len()` must be a whole multiple of `channels`. Returns
    /// `None` if the buffer is mis-aligned or `channels == 0`.
    pub fn new(samples: Arc<[f32]>, sample_rate: u32, channels: u16) -> Option<Self> {
        if channels == 0 {
            return None;
        }
        if samples.len() % usize::from(channels) != 0 {
            return None;
        }
        Some(Self {
            samples,
            sample_rate,
            channels,
            cursor: 0,
        })
    }

    fn total_frames(&self) -> u64 {
        // Safe: `new` enforces samples.len() % channels == 0, and
        // channels > 0; the division is exact.
        (self.samples.len() / usize::from(self.channels)) as u64
    }
}

impl PlaybackSource for BufferSource {
    fn pull(&mut self, dst: &mut [f32]) -> usize {
        let ch = usize::from(self.channels);
        debug_assert_eq!(dst.len() % ch, 0, "dst must be a whole number of frames");

        let total = self.total_frames();
        if self.cursor >= total {
            return 0;
        }

        let max_frames_dst = dst.len() / ch;
        // BufferSource is backed by Arc<[f32]> whose .len() is itself
        // usize, so total - cursor is bounded by usize::MAX by
        // construction. try_from + saturate is the explicit form.
        let remaining = usize::try_from(total - self.cursor).unwrap_or(usize::MAX);
        let frames = max_frames_dst.min(remaining);
        if frames == 0 {
            return 0;
        }

        // Same bound: cursor ≤ total ≤ usize::MAX (samples.len() / ch).
        let cursor_usize = usize::try_from(self.cursor).unwrap_or(usize::MAX);
        let start = cursor_usize * ch;
        let end = start + frames * ch;
        dst[..frames * ch].copy_from_slice(&self.samples[start..end]);
        self.cursor += frames as u64;
        frames
    }

    fn seek(&mut self, frame: u64) -> Result<(), SeekError> {
        let total = self.total_frames();
        if frame > total {
            return Err(SeekError::OutOfBounds { requested: frame, max: total });
        }
        self.cursor = frame;
        Ok(())
    }

    fn duration_frames(&self) -> Option<u64> {
        Some(self.total_frames())
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo_ramp(frames: usize) -> Arc<[f32]> {
        let mut v = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let f = i as f32;
            v.push(f);          // ch0
            v.push(-f);         // ch1
        }
        v.into()
    }

    #[test]
    fn new_rejects_misaligned_buffer() {
        // 5 samples in a stereo (2-channel) buffer is misaligned.
        let bad: Arc<[f32]> = vec![0.0; 5].into();
        assert!(BufferSource::new(bad, 48_000, 2).is_none());
    }

    #[test]
    fn new_rejects_zero_channels() {
        let buf: Arc<[f32]> = vec![0.0; 8].into();
        assert!(BufferSource::new(buf, 48_000, 0).is_none());
    }

    #[test]
    fn pull_returns_zero_at_eof() {
        let buf = stereo_ramp(4);
        let mut src = BufferSource::new(buf, 48_000, 2).unwrap();
        let mut dst = [0.0f32; 16];
        let got = src.pull(&mut dst);
        assert_eq!(got, 4);
        let got2 = src.pull(&mut dst);
        assert_eq!(got2, 0, "second pull at EOF returns 0 frames");
    }

    #[test]
    fn pull_clamps_to_remaining_when_dst_is_larger() {
        let buf = stereo_ramp(3);
        let mut src = BufferSource::new(buf, 48_000, 2).unwrap();
        let mut dst = [0.0f32; 100];
        let got = src.pull(&mut dst);
        assert_eq!(got, 3);
        // Frames written are correct interleaved values.
        assert_eq!(dst[..6], [0.0, 0.0, 1.0, -1.0, 2.0, -2.0]);
    }

    #[test]
    fn pull_clamps_to_dst_when_source_is_larger() {
        let buf = stereo_ramp(100);
        let mut src = BufferSource::new(buf, 48_000, 2).unwrap();
        let mut dst = [0.0f32; 8];   // 4 stereo frames
        let got = src.pull(&mut dst);
        assert_eq!(got, 4);
        // Cursor advanced exactly 4 frames; next pull returns the next 4.
        let mut dst2 = [0.0f32; 8];
        let got2 = src.pull(&mut dst2);
        assert_eq!(got2, 4);
        assert_eq!(dst2[..2], [4.0, -4.0]);
    }

    #[test]
    fn seek_to_zero_replays_from_start() {
        let buf = stereo_ramp(5);
        let mut src = BufferSource::new(buf, 48_000, 2).unwrap();
        let mut dst = [0.0f32; 10];
        src.pull(&mut dst);
        src.seek(0).unwrap();
        let mut dst2 = [0.0f32; 10];
        let got = src.pull(&mut dst2);
        assert_eq!(got, 5);
        assert_eq!(dst2, dst);
    }

    #[test]
    fn seek_past_end_is_out_of_bounds() {
        let buf = stereo_ramp(4);
        let mut src = BufferSource::new(buf, 48_000, 2).unwrap();
        let err = src.seek(5).unwrap_err();
        assert_eq!(err, SeekError::OutOfBounds { requested: 5, max: 4 });
    }

    #[test]
    fn seek_to_exact_eof_is_allowed_and_pulls_zero() {
        let buf = stereo_ramp(4);
        let mut src = BufferSource::new(buf, 48_000, 2).unwrap();
        src.seek(4).expect("seek to duration is allowed");
        let mut dst = [0.0f32; 8];
        assert_eq!(src.pull(&mut dst), 0);
    }

    #[test]
    fn duration_frames_matches_buffer_length() {
        let src = BufferSource::new(stereo_ramp(48_000), 48_000, 2).unwrap();
        assert_eq!(src.duration_frames(), Some(48_000));
        assert_eq!(src.sample_rate(), 48_000);
        assert_eq!(src.channels(), 2);
    }

    #[test]
    fn arc_clone_keeps_buffer_alive_after_caller_drops() {
        let original = stereo_ramp(4);
        let mut src = BufferSource::new(Arc::clone(&original), 48_000, 2).unwrap();
        drop(original); // caller releases their clone
        let mut dst = [0.0f32; 8];
        let got = src.pull(&mut dst);
        assert_eq!(got, 4);
        assert_eq!(dst, [0.0, 0.0, 1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
    }
}
