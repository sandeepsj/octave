//! Public typed surface — the shapes the API and MCP layer hand back.
//!
//! Several types here intentionally duplicate `octave-recorder`'s
//! equivalents (`Backend`, `DeviceId`, `BufferSize`). When a third
//! consumer (mix-engine, editor) needs the same types we'll extract
//! a shared `octave-audio-types` crate; until then duplication beats
//! the cross-crate dependency.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Platform-stable identifier for an audio device. Encoded as
/// `"{HOST_NAME}:{DEVICE_NAME}"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

/// The kernel-level audio backend a device is exposed through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    Alsa,
    PipeWire,
    Jack,
    CoreAudio,
    Wasapi,
    Asio,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputDeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub backend: Backend,
    pub is_default_output: bool,
    pub max_output_channels: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputCapabilities {
    pub min_sample_rate: u32,
    pub max_sample_rate: u32,
    pub supported_sample_rates: Vec<u32>,
    pub min_buffer_size: u32,
    pub max_buffer_size: u32,
    pub channels: Vec<u16>,
    pub default_sample_rate: u32,
    pub default_buffer_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BufferSize {
    Default,
    Fixed(u32),
}

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
