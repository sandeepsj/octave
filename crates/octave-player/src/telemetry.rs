//! Atomic telemetry shared between the audio callback (writer) and the
//! API / UI layer (readers).
//!
//! Every field is single-writer (the audio thread) / many-reader (the
//! handle's accessor methods); load/store ordering is `Relaxed` for
//! values that are independently meaningful, `Release` / `Acquire` for
//! the position counter where a non-RT consumer wants to read coherent
//! "position at this point in time".
//!
//! See `docs/modules/playback-audio.md` §5.3 (peak), §5.4 (RMS),
//! §5.8 (position), §7.2 (synchronisation primitives).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Per-session shared atomic state. Constructed once on `start`,
/// referenced by both the audio callback and the handle.
pub(crate) struct Telemetry {
    #[allow(dead_code)] // surfaced once running-peak-per-take is wired through the API
    pub channels: u16,

    // ---- meter (one slot per channel) ----
    /// Peak (max abs) over the **last** callback's buffer, bit-cast f32.
    pub peak: Vec<AtomicU32>,
    /// Mean-square over the **last** callback's buffer, bit-cast f32.
    pub mean_square: Vec<AtomicU32>,
    /// Peak (max abs) over the entire take so far, bit-cast f32.
    pub running_peak: Vec<AtomicU32>,

    // ---- counters ----
    /// Number of callbacks that found the ring empty (under-runs).
    pub xrun_count: AtomicU32,
    /// Frame index of the next sample to be played from the source.
    pub position_frames: AtomicU64,
}

impl Telemetry {
    pub fn new(channels: u16) -> Self {
        let n = usize::from(channels);
        Self {
            channels,
            peak: (0..n).map(|_| AtomicU32::new(0)).collect(),
            mean_square: (0..n).map(|_| AtomicU32::new(0)).collect(),
            running_peak: (0..n).map(|_| AtomicU32::new(0)).collect(),
            xrun_count: AtomicU32::new(0),
            position_frames: AtomicU64::new(0),
        }
    }

    /// Last-buffer peak as a linear value in `[0, 1]`. dBFS conversion
    /// happens off the audio thread.
    pub fn peak_value(&self, channel: u16) -> f32 {
        let i = usize::from(channel);
        f32::from_bits(self.peak[i].load(Ordering::Relaxed))
    }

    /// Last-buffer mean-square as a linear value in `[0, 1]`.
    pub fn mean_square_value(&self, channel: u16) -> f32 {
        let i = usize::from(channel);
        f32::from_bits(self.mean_square[i].load(Ordering::Relaxed))
    }

    /// Take-so-far peak as a linear value in `[0, 1]`.
    #[allow(dead_code)] // surfaced via PlaybackHandle once take-peak reporting lands
    pub fn running_peak_value(&self, channel: u16) -> f32 {
        let i = usize::from(channel);
        f32::from_bits(self.running_peak[i].load(Ordering::Relaxed))
    }

    /// Reset per-take running peaks. Called on `start` (and on `seek`
    /// to provide a fresh peak for the new region).
    #[allow(dead_code)] // wired in once running-peak reset on seek lands
    pub fn reset_running_peaks(&self) {
        for ap in &self.running_peak {
            ap.store(0u32, Ordering::Relaxed);
        }
    }

    /// Reset position to a specific frame (used by seek-flush handshake).
    pub fn set_position_frames(&self, frame: u64) {
        self.position_frames.store(frame, Ordering::Release);
    }
}
