//! Cross-thread signaling primitives that co-ordinate the API thread,
//! the reader thread, and the audio (RT) thread.
//!
//! Distinct from [`crate::telemetry::Telemetry`] (which is meter +
//! position state). The signals here are short-lived booleans that
//! coordinate handshakes — seek-flush, EOF observation, terminal
//! states.
//!
//! See `docs/modules/playback-audio.md` §5.6 (seek-flush handshake),
//! §5.8 (EOF detection), §7.2 (synchronisation primitives).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Shared atomic flags between API / reader / audio threads.
///
/// All fields are single-writer; the `who writes` and `who reads`
/// columns are documented per field. Atomic ordering is `Acquire` /
/// `Release` on flags that gate observable side effects (seek and
/// flush); `Relaxed` is sufficient for the terminal flags (`eof_seen`,
/// `playback_complete`, `stop_request`) since they're checked once
/// per loop iteration and a one-iteration lag is acceptable.
pub(crate) struct TransportSignals {
    /// Frame index requested by the most recent seek call.
    /// **Writer:** API. **Readers:** reader thread (after observing
    /// `seek_pending`).
    pub seek_target: AtomicU64,

    /// API has requested a seek. Reader observes, kicks off the flush
    /// handshake, and clears once the source has been repositioned.
    /// **Writer:** API (set), reader (clear). **Reader:** reader thread.
    pub seek_pending: AtomicBool,

    /// Reader has asked the audio callback to drop the ring contents
    /// and reset position. Audio observes, performs the flush, clears.
    /// **Writer:** reader (set), audio (clear). **Reader:** audio thread.
    pub flush_request: AtomicBool,

    /// Reader has observed `source.pull` return 0 (source EOF).
    /// **Writer:** reader. **Readers:** audio thread, API.
    pub eof_seen: AtomicBool,

    /// Audio callback has played out the last samples after EOF and
    /// the ring is empty. The actor reads this to transition state to
    /// `Ended`.
    /// **Writer:** audio. **Reader:** API / actor.
    pub playback_complete: AtomicBool,

    /// API has requested a graceful stop. Reader observes and exits;
    /// audio callback observes and silences.
    /// **Writer:** API. **Readers:** reader, audio.
    pub stop_request: AtomicBool,
}

impl TransportSignals {
    pub fn new() -> Self {
        Self {
            seek_target: AtomicU64::new(0),
            seek_pending: AtomicBool::new(false),
            flush_request: AtomicBool::new(false),
            eof_seen: AtomicBool::new(false),
            playback_complete: AtomicBool::new(false),
            stop_request: AtomicBool::new(false),
        }
    }

    /// API → reader: kick off a seek to `frame`.
    #[allow(dead_code)] // wired when PlaybackHandle::seek lands
    pub fn request_seek(&self, frame: u64) {
        self.seek_target.store(frame, Ordering::Release);
        self.seek_pending.store(true, Ordering::Release);
    }
}
