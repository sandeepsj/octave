//! Recorder state machine per `docs/modules/record-audio.md` §11.3.

use serde::{Deserialize, Serialize};

/// All states a [`crate::RecordingHandle`] can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecorderState {
    /// Handle exists, no stream built, no meter live.
    Idle,
    /// `open()` is running on a worker thread.
    Opening,
    /// Stream is live, meter is reading, but nothing is being written to disk.
    Armed,
    /// Stream is live and frames are being written to the WAV file.
    Recording,
    /// `stop()` is draining the ring and finalizing the file.
    Stopping,
    /// `cancel()` is closing and deleting the partial file.
    Cancelling,
    /// Resources released; handle is consumed.
    Closed,
    /// A non-recoverable failure occurred; handle should be closed.
    Errored,
}
