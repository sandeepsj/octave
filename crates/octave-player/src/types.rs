//! Public typed surface — the playback-engine-specific shapes.
//!
//! Cross-engine types (`DeviceId`, `Backend`, `BufferSize`,
//! `OutputDeviceInfo`, `OutputCapabilities`) live in the shared
//! `octave-audio-devices` crate and are re-exported here for caller
//! ergonomics — `octave_player::DeviceId` keeps working without
//! callers needing to learn a third crate name.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub use octave_audio_devices::{
    Backend, BufferSize, DeviceId, OutputCapabilities, OutputDeviceInfo,
};

/// Source the playback engine should pull from.
#[derive(Debug, Clone)]
pub enum PlaybackSourceSpec {
    File {
        path: PathBuf,
    },
    Buffer {
        samples: Arc<[f32]>,
        sample_rate: u32,
        channels: u16,
    },
}

#[derive(Debug, Clone)]
pub struct PlaybackSpec {
    pub device_id: DeviceId,
    pub source: PlaybackSourceSpec,
    pub buffer_size: BufferSize,
}

/// Engine-level playback state.
///
/// `Ended` is distinct from `Stopped`: the former means the source
/// ran out and the audio thread has played out the last samples; the
/// latter means the user asked the player to stop. Both terminal
/// states drop the device on `close()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackState {
    Idle,
    Loading,
    Playing,
    Paused,
    Stopped,
    Ended,
    Errored,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackStatus {
    pub state: PlaybackState,
    pub position_frames: u64,
    pub position_seconds: f64,
    pub duration_frames: Option<u64>,
    pub duration_seconds: Option<f64>,
    pub sample_rate: u32,
    pub channels: u16,
    pub xrun_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackLevels {
    pub peak_dbfs: Vec<f32>,
    pub rms_dbfs: Vec<f32>,
}
