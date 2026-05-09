//! Octave's audio input layer.
//!
//! Opens an input device, captures frames in real-time, and writes them to
//! disk as 32-bit float WAV (auto-promoting to RF64 past ~3.5 GB), without a
//! single dropout, allocation, or lock on the audio thread.
//!
//! The full design — stack walk from hardware to MCP, performance budgets,
//! state machine, failure modes, acceptance criteria — lives in
//! [`docs/modules/record-audio.md`](https://github.com/sandeepsj/octave/blob/main/docs/modules/record-audio.md).
//!
//! This crate is currently **v0.1 scaffold**: the public API surface from
//! §9 of the plan compiles, but operations panic with `unimplemented!()`.
//! Implementation lands incrementally, RT path last.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod error;
mod state;

// `wav` is wired into `RecordingHandle::record` / `stop` in a follow-up turn;
// until then its public-by-crate items look dead to the lib build.
#[allow(dead_code)]
mod wav;

pub use error::{ArmError, CancelError, OpenError, RecordError, StopError};
pub use state::RecorderState;

/// Platform-stable identifier for an audio device.
///
/// Stable across program runs *on the same machine* as long as the device
/// is enumerated through the same backend. Not intended to be parsed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

/// The kernel-level audio backend a device is exposed through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    /// Linux ALSA (kernel) — the floor `cpal` falls back to.
    Alsa,
    /// Linux PipeWire — the modern default on Fedora / Ubuntu 22.04+ / Arch.
    PipeWire,
    /// Linux JACK (or PipeWire's JACK-compatible API) — pro low-latency path.
    Jack,
    /// macOS Core Audio HAL.
    CoreAudio,
    /// Windows WASAPI (default on Windows).
    Wasapi,
    /// Windows ASIO (requires Steinberg SDK at compile time).
    Asio,
}

/// Discovered information about an enumerable input device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Stable identifier — pass to [`device_capabilities`] or [`RecordingSpec::device_id`].
    pub id: DeviceId,
    /// Human-readable name surfaced in the UI device picker.
    pub name: String,
    /// Which backend the device speaks through.
    pub backend: Backend,
    /// `true` if this is the host's current default input device.
    pub is_default_input: bool,
    /// `true` if the device works against the generic UAC2 driver (no vendor driver).
    pub is_class_compliant_usb: bool,
    /// Device-reported maximum input channel count.
    pub max_input_channels: u16,
}

/// What a device can do — the result of asking it for its capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub min_sample_rate: u32,
    pub max_sample_rate: u32,
    /// Discrete sample-rate set when the backend reports it; empty if only a range is known.
    pub supported_sample_rates: Vec<u32>,
    pub min_buffer_size: u32,
    pub max_buffer_size: u32,
    /// Channel counts the device will accept. Typically `[1, 2]` for a Scarlett 2i2.
    pub channels: Vec<u16>,
    pub default_sample_rate: u32,
    pub default_buffer_size: u32,
}

/// Buffer-size choice when opening an input stream.
///
/// `Default` lets the backend pick (often 256–1024 samples on Linux PipeWire).
/// `Fixed(n)` requests `n` samples per period — the backend may round, so
/// the recorder validates the actual size from the first callback's slice
/// length (see record-audio plan §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BufferSize {
    Default,
    Fixed(u32),
}

/// All inputs [`open`] needs to bring up an input stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingSpec {
    pub device_id: DeviceId,
    pub sample_rate: u32,
    pub buffer_size: BufferSize,
    /// Number of channels to capture. Channels `[0..channels)` of the device are recorded.
    pub channels: u16,
}

/// A finalized recording — the artifact returned from [`RecordingHandle::stop`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedClip {
    pub path: PathBuf,
    pub uuid: Uuid,
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_count: u64,
    pub duration_seconds: f64,
    pub started_at: SystemTime,
    pub xrun_count: u32,
    pub dropped_samples: u64,
    /// Peak (in dBFS) reached on each channel over the entire take.
    pub peak_dbfs: Vec<f32>,
}

/// Opaque session handle. Obtained from [`open`].
///
/// Drives the recorder state machine: `Idle → Armed → Recording → Armed → Closed`.
/// See [`docs/modules/record-audio.md` §11.3](https://github.com/sandeepsj/octave/blob/main/docs/modules/record-audio.md) for the full diagram.
pub struct RecordingHandle {
    _private: (),
}

/// Enumerate every input device the host knows about, across every backend
/// the platform exposes. Cheap; safe to call from a UI render loop.
pub fn list_devices() -> Vec<DeviceInfo> {
    unimplemented!("octave-recorder v0.1 scaffold — see docs/modules/record-audio.md §3.3");
}

/// Ask one device about its supported sample rates, buffer sizes, and channel counts.
pub fn device_capabilities(_id: &DeviceId) -> Result<Capabilities, OpenError> {
    unimplemented!("octave-recorder v0.1 scaffold — see docs/modules/record-audio.md §9.2");
}

/// Open a device with a specific [`RecordingSpec`]. Returns a handle in [`RecorderState::Idle`].
///
/// The underlying `cpal::Stream` is built but not started — call [`RecordingHandle::arm`]
/// to begin the level meter, and [`RecordingHandle::record`] to start writing to disk.
pub fn open(_spec: RecordingSpec) -> Result<RecordingHandle, OpenError> {
    unimplemented!("octave-recorder v0.1 scaffold — see docs/modules/record-audio.md §3.4");
}

impl RecordingHandle {
    /// Arm the device: build the input stream, start the audio callback, light up the level meter.
    /// State must be [`RecorderState::Idle`].
    pub fn arm(&mut self) -> Result<(), ArmError> {
        unimplemented!("octave-recorder v0.1 scaffold");
    }

    /// Begin writing captured frames to `output_path` as 32-bit float WAV.
    /// State must be [`RecorderState::Armed`].
    pub fn record(&mut self, _output_path: &Path) -> Result<(), RecordError> {
        unimplemented!("octave-recorder v0.1 scaffold");
    }

    /// Stop recording cleanly: drain the ring, finalize the WAV header, fsync, return the clip.
    /// State transitions [`RecorderState::Recording`] → [`RecorderState::Stopping`] → [`RecorderState::Armed`].
    pub fn stop(&mut self) -> Result<RecordedClip, StopError> {
        unimplemented!("octave-recorder v0.1 scaffold");
    }

    /// Cancel: stop and **delete** the partial file. State returns to [`RecorderState::Armed`].
    pub fn cancel(&mut self) -> Result<(), CancelError> {
        unimplemented!("octave-recorder v0.1 scaffold");
    }

    /// Last-buffer peak, in dBFS, for the given channel. `f32::NEG_INFINITY` if not yet armed.
    pub fn peak_dbfs(&self, _channel: u16) -> f32 {
        f32::NEG_INFINITY
    }

    /// Last-buffer RMS, in dBFS, for the given channel. `f32::NEG_INFINITY` if not yet armed.
    pub fn rms_dbfs(&self, _channel: u16) -> f32 {
        f32::NEG_INFINITY
    }

    /// Cumulative xrun (over-run) count since [`open`].
    pub fn xrun_count(&self) -> u32 {
        0
    }

    /// Cumulative samples dropped because the writer thread couldn't keep up.
    /// **A non-zero value here means audible data loss** — see record-audio plan §8.
    pub fn dropped_samples(&self) -> u64 {
        0
    }

    /// Current recorder state.
    pub fn state(&self) -> RecorderState {
        RecorderState::Idle
    }

    /// Tear down the stream, join the writer thread, release the device. Consumes the handle.
    pub fn close(self) {}
}
