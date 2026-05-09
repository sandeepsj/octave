//! Tool argument and return types.
//!
//! These are thin wrappers over [`octave_recorder`]'s public types so
//! the MCP-facing JSON schemas stay flat and human-readable. We avoid
//! exposing rust enum-with-data shapes (`{"Fixed": 64}`) that would
//! confuse an agent's schema discovery.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use octave_recorder::{Backend, BufferSize, RecorderState};

/// Result of `recording_list_devices`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListDevicesResult {
    pub devices: Vec<DeviceInfoJson>,
}

/// Flattened, agent-friendly mirror of [`octave_recorder::DeviceInfo`].
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeviceInfoJson {
    /// Opaque, platform-stable identifier; pass to other tools.
    pub device_id: String,
    /// Human-readable name surfaced in UIs.
    pub name: String,
    /// Backend the device is exposed through.
    pub backend: BackendJson,
    /// `true` if this is the host's current default input device.
    pub is_default_input: bool,
    /// `true` if the device works against the generic UAC2 driver.
    pub is_class_compliant_usb: bool,
    /// Maximum input channel count the device advertises.
    pub max_input_channels: u16,
}

/// Stringified backend, friendlier in JSON than the rust enum.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackendJson {
    Alsa,
    #[serde(rename = "pipewire")]
    PipeWire,
    Jack,
    #[serde(rename = "coreaudio")]
    CoreAudio,
    Wasapi,
    Asio,
}

impl From<Backend> for BackendJson {
    fn from(b: Backend) -> Self {
        match b {
            Backend::Alsa => Self::Alsa,
            Backend::PipeWire => Self::PipeWire,
            Backend::Jack => Self::Jack,
            Backend::CoreAudio => Self::CoreAudio,
            Backend::Wasapi => Self::Wasapi,
            Backend::Asio => Self::Asio,
        }
    }
}

impl From<octave_recorder::DeviceInfo> for DeviceInfoJson {
    fn from(d: octave_recorder::DeviceInfo) -> Self {
        Self {
            device_id: d.id.0,
            name: d.name,
            backend: d.backend.into(),
            is_default_input: d.is_default_input,
            is_class_compliant_usb: d.is_class_compliant_usb,
            max_input_channels: d.max_input_channels,
        }
    }
}

/// Argument to `recording_describe_device`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DescribeDeviceArgs {
    /// Opaque device identifier returned from `recording_list_devices`.
    pub device_id: String,
}

/// Mirror of [`octave_recorder::Capabilities`] — already JSON-friendly,
/// re-derived here to keep the wire surface independent of the recorder.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CapabilitiesJson {
    pub min_sample_rate: u32,
    pub max_sample_rate: u32,
    pub supported_sample_rates: Vec<u32>,
    pub min_buffer_size: u32,
    pub max_buffer_size: u32,
    pub channels: Vec<u16>,
    pub default_sample_rate: u32,
    pub default_buffer_size: u32,
}

impl From<octave_recorder::Capabilities> for CapabilitiesJson {
    fn from(c: octave_recorder::Capabilities) -> Self {
        Self {
            min_sample_rate: c.min_sample_rate,
            max_sample_rate: c.max_sample_rate,
            supported_sample_rates: c.supported_sample_rates,
            min_buffer_size: c.min_buffer_size,
            max_buffer_size: c.max_buffer_size,
            channels: c.channels,
            default_sample_rate: c.default_sample_rate,
            default_buffer_size: c.default_buffer_size,
        }
    }
}

/// Buffer-size choice in agent-friendly tagged form, replacing the
/// recorder's `enum BufferSize { Default, Fixed(u32) }`.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BufferSizeJson {
    /// Let the backend pick.
    Default,
    /// Request a specific power-of-two-friendly size in samples.
    Fixed { samples: u32 },
}

impl From<BufferSizeJson> for BufferSize {
    fn from(b: BufferSizeJson) -> Self {
        match b {
            BufferSizeJson::Default => BufferSize::Default,
            BufferSizeJson::Fixed { samples } => BufferSize::Fixed(samples),
        }
    }
}

impl From<BufferSizeJson> for octave_player::BufferSize {
    fn from(b: BufferSizeJson) -> Self {
        match b {
            BufferSizeJson::Default => octave_player::BufferSize::Default,
            BufferSizeJson::Fixed { samples } => octave_player::BufferSize::Fixed(samples),
        }
    }
}

/// Argument to `recording_start`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct StartArgs {
    /// Opaque device identifier from `recording_list_devices`.
    pub device_id: String,
    /// Capture rate in Hz. Common: 48000 (default for production audio).
    pub sample_rate: u32,
    /// Buffer size in samples. `{"kind": "default"}` lets the OS choose.
    pub buffer_size: BufferSizeJson,
    /// Number of channels to capture. 2 for stereo, 1 for mono.
    pub channels: u16,
    /// Where to write the 32-bit float WAV. Overwritten if exists.
    pub output_path: PathBuf,
}

/// Result of `recording_start`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StartResult {
    /// Pass this to subsequent tools to refer to the running session.
    /// UUID v4, serialized as a string (e.g. `"7c9e...-..."`).
    pub session_id: String,
    /// Unix-epoch seconds when recording began. Use for elapsed math.
    pub started_at_unix_seconds: u64,
}

/// Argument shared by all session-bound tools.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SessionArgs {
    /// UUID v4 string returned from `recording_start`.
    pub session_id: String,
}

/// Result of `recording_stop` — a finalized recording.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RecordedClipJson {
    pub path: PathBuf,
    /// UUID v4 string assigned to this clip.
    pub clip_uuid: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_count: u64,
    pub duration_seconds: f64,
    pub started_at_unix_seconds: u64,
    pub xrun_count: u32,
    pub dropped_samples: u64,
    /// Per-channel peak in dBFS over the whole take.
    pub peak_dbfs: Vec<f32>,
}

/// Result of `recording_cancel`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CancelResult {
    /// The path the partial file was at; `deleted` reports whether
    /// removal succeeded (we don't fail the tool if delete couldn't run).
    pub path: PathBuf,
    pub deleted: bool,
}

/// Result of `recording_get_levels`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct LevelsResult {
    pub peak_dbfs: Vec<f32>,
    pub rms_dbfs: Vec<f32>,
}

/// Result of `recording_get_status`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusResult {
    pub state: RecorderStateJson,
    pub xrun_count: u32,
    pub dropped_samples: u64,
    pub elapsed_seconds: f64,
}

/// Wire form of [`octave_recorder::RecorderState`].
#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecorderStateJson {
    Idle,
    Opening,
    Armed,
    Recording,
    Stopping,
    Cancelling,
    Closed,
    Errored,
}

impl From<RecorderState> for RecorderStateJson {
    fn from(s: RecorderState) -> Self {
        match s {
            RecorderState::Idle => Self::Idle,
            RecorderState::Opening => Self::Opening,
            RecorderState::Armed => Self::Armed,
            RecorderState::Recording => Self::Recording,
            RecorderState::Stopping => Self::Stopping,
            RecorderState::Cancelling => Self::Cancelling,
            RecorderState::Closed => Self::Closed,
            RecorderState::Errored => Self::Errored,
        }
    }
}

// ============================================================================
//   Playback wire types — mirror of the recording set, for `playback_*` tools.
// ============================================================================

/// Result of `playback_list_output_devices`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListOutputDevicesResult {
    pub devices: Vec<OutputDeviceInfoJson>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OutputDeviceInfoJson {
    pub device_id: String,
    pub name: String,
    pub backend: BackendJson,
    pub is_default_output: bool,
    pub max_output_channels: u16,
}

impl From<octave_player::OutputDeviceInfo> for OutputDeviceInfoJson {
    fn from(d: octave_player::OutputDeviceInfo) -> Self {
        Self {
            device_id: d.id.0,
            name: d.name,
            backend: player_backend(d.backend),
            is_default_output: d.is_default_output,
            max_output_channels: d.max_output_channels,
        }
    }
}

fn player_backend(b: octave_player::Backend) -> BackendJson {
    match b {
        octave_player::Backend::Alsa => BackendJson::Alsa,
        octave_player::Backend::PipeWire => BackendJson::PipeWire,
        octave_player::Backend::Jack => BackendJson::Jack,
        octave_player::Backend::CoreAudio => BackendJson::CoreAudio,
        octave_player::Backend::Wasapi => BackendJson::Wasapi,
        octave_player::Backend::Asio => BackendJson::Asio,
    }
}

impl From<octave_player::OutputCapabilities> for CapabilitiesJson {
    fn from(c: octave_player::OutputCapabilities) -> Self {
        Self {
            min_sample_rate: c.min_sample_rate,
            max_sample_rate: c.max_sample_rate,
            supported_sample_rates: c.supported_sample_rates,
            min_buffer_size: c.min_buffer_size,
            max_buffer_size: c.max_buffer_size,
            channels: c.channels,
            default_sample_rate: c.default_sample_rate,
            default_buffer_size: c.default_buffer_size,
        }
    }
}

/// Source descriptor for `playback_start`. v0.1 supports `file` (path on
/// disk to a 32-bit float WAV / RF64) and `buffer` (inline f32 array,
/// capped at 100 MB by the MCP layer to avoid pathological JSON-RPC
/// payloads — agents wanting larger should write a temp file first).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlaybackSourceJson {
    File {
        path: PathBuf,
    },
    Buffer {
        /// Interleaved 32-bit float samples, channel-major within each
        /// frame. Length must be a multiple of `channels`.
        samples: Vec<f32>,
        sample_rate: u32,
        channels: u16,
    },
}

/// Argument to `playback_start`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PlaybackStartArgs {
    pub device_id: String,
    pub source: PlaybackSourceJson,
    pub buffer_size: BufferSizeJson,
}

/// Result of `playback_start`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlaybackStartResult {
    pub session_id: String,
    pub started_at_unix_seconds: u64,
    /// Total source duration in seconds, when known. `None` for unbounded
    /// sources (none in v0.1, but reserved for future streaming sources).
    pub duration_seconds: Option<f64>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Wire form of [`octave_player::PlaybackState`]. `Ended` is collapsed
/// into `Stopped` with a `reason: "eof"` field per resolved §13.6.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStateJson {
    Idle,
    Loading,
    Playing,
    Paused,
    Seeking,
    Stopped,
    Errored,
    Closed,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StoppedReasonJson {
    User,
    Eof,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlaybackStatusJson {
    pub state: PlaybackStateJson,
    /// Set when `state == "stopped"`; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_reason: Option<StoppedReasonJson>,
    pub position_frames: u64,
    pub position_seconds: f64,
    pub duration_frames: Option<u64>,
    pub duration_seconds: Option<f64>,
    pub sample_rate: u32,
    pub channels: u16,
    pub xrun_count: u32,
}

impl From<octave_player::PlaybackStatus> for PlaybackStatusJson {
    fn from(s: octave_player::PlaybackStatus) -> Self {
        let (state, stopped_reason) = match s.state {
            octave_player::PlaybackState::Idle => (PlaybackStateJson::Idle, None),
            octave_player::PlaybackState::Loading => (PlaybackStateJson::Loading, None),
            octave_player::PlaybackState::Playing => (PlaybackStateJson::Playing, None),
            octave_player::PlaybackState::Paused => (PlaybackStateJson::Paused, None),
            octave_player::PlaybackState::Seeking => (PlaybackStateJson::Seeking, None),
            octave_player::PlaybackState::Stopped => {
                (PlaybackStateJson::Stopped, Some(StoppedReasonJson::User))
            }
            octave_player::PlaybackState::Ended => {
                (PlaybackStateJson::Stopped, Some(StoppedReasonJson::Eof))
            }
            octave_player::PlaybackState::Errored => (PlaybackStateJson::Errored, None),
            octave_player::PlaybackState::Closed => (PlaybackStateJson::Closed, None),
        };
        Self {
            state,
            stopped_reason,
            position_frames: s.position_frames,
            position_seconds: s.position_seconds,
            duration_frames: s.duration_frames,
            duration_seconds: s.duration_seconds,
            sample_rate: s.sample_rate,
            channels: s.channels,
            xrun_count: s.xrun_count,
        }
    }
}

/// Argument to `playback_seek`. One of `position_seconds` /
/// `position_frames` is required; if both are given, frames win.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PlaybackSeekArgs {
    pub session_id: String,
    pub position_seconds: Option<f64>,
    pub position_frames: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlaybackSeekResult {
    pub position_seconds: f64,
    pub position_frames: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlaybackTransportResult {
    pub state: PlaybackStateJson,
    pub position_seconds: f64,
    pub position_frames: u64,
}
