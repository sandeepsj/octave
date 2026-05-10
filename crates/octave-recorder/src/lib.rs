//! Octave's audio input layer.
//!
//! Opens an input device, captures frames in real-time, and writes them to
//! disk as 32-bit float WAV (auto-promoting to RF64 past ~3.5 GB), without a
//! single dropout, allocation, or lock on the audio thread.
//!
//! See [`docs/modules/record-audio.md`](https://github.com/sandeepsj/octave/blob/main/docs/modules/record-audio.md)
//! for the full design — stack walk from hardware to MCP, performance budgets,
//! state machine, failure modes, acceptance criteria.

#![cfg_attr(test, allow(clippy::float_cmp, clippy::cast_precision_loss))]

// Test-only: route allocations through `assert_no_alloc::AllocDisabler` so
// any heap allocation reachable from `assert_no_alloc(|| …)` panics. The
// real-time audio callback is wrapped in such a block (see `audio.rs`).
// Library consumers don't see this — `cfg(test)` is only true for our own
// test binary.
// `assert_no_alloc::AllocDisabler` is gated to debug builds in the
// crate (default features `disable_release` strips it in release).
// Mirror the gate here so `cargo test --release` compiles.
#[cfg(all(test, debug_assertions))]
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod audio;
mod device;
mod error;
mod ring;
mod rt;
mod state;
mod wav;
mod writer;

#[cfg(test)]
mod test_support;

pub use device::DeviceCatalog;
pub use error::{ArmError, CancelError, OpenError, RecordError, StopError};
pub use state::RecorderState;

/// Platform-stable identifier for an audio device.
///
/// Encoded as `"{HOST_NAME}:{DEVICE_NAME}"` — opaque to callers, but
/// stable enough that re-finding a device by id works as long as the
/// device's name doesn't change between runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

/// The kernel-level audio backend a device is exposed through.
///
/// `PipeWire` is **reserved** for future direct-PipeWire integrations;
/// today PipeWire on Linux is reached via the `Alsa` host (cpal exposes
/// it as ALSA), so `host_id_to_backend` never produces this variant.
///
/// `Other(name)` is returned for any cpal `HostId::name()` value not
/// in the explicit list — better than silently coercing to `Alsa`,
/// which used to mis-tag macOS / Windows / future hosts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    Alsa,
    PipeWire,
    Jack,
    CoreAudio,
    Wasapi,
    Asio,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub backend: Backend,
    pub is_default_input: bool,
    pub is_class_compliant_usb: bool,
    pub max_input_channels: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingSpec {
    pub device_id: DeviceId,
    pub sample_rate: u32,
    pub buffer_size: BufferSize,
    pub channels: u16,
}

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
    pub peak_dbfs: Vec<f32>,
}

/// Opaque session handle returned from [`open`]. `!Send` (cpal's `Stream`
/// is `!Send` on every backend) — keep it on the OS thread that opened it.
pub struct RecordingHandle {
    inner: audio::Internals,
}

// `list_devices`, `device_capabilities`, and `open` are methods on
// `DeviceCatalog` (see device.rs). Holding the catalog across list +
// open is what defeats the cpal-on-ALSA enumerate-race that
// PipeWire's exclusive PCM grab triggers — see plan §3.3.1.

// `RecordingHandle` methods (`arm`, `record`, `stop`, `cancel`,
// `peak_dbfs`, `rms_dbfs`, `xrun_count`, `dropped_samples`, `state`,
// `close`) are implemented in `audio.rs` alongside `Internals` and the
// cpal stream.
