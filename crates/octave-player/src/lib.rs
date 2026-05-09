//! Octave's audio output layer.
//!
//! Opens an audio output device, pulls frames from a 32-bit float WAV
//! file or an in-memory `Arc<[f32]>` buffer, and pushes them to the
//! device — sample-accurate, dropout-free, and with the same RT
//! discipline as `octave-recorder`.
//!
//! See [`docs/modules/playback-audio.md`](https://github.com/sandeepsj/octave/blob/main/docs/modules/playback-audio.md)
//! for the full design — stack walk from hardware to MCP, transport
//! semantics, performance budgets, state machine, failure modes,
//! acceptance criteria.
//!
//! # Status (v0.1)
//!
//! Working: device enumeration + capability query, file source (WAV /
//! RF64), in-memory buffer source, RT output callback with seek/EOF
//! handshake atomics, reader thread, [`open`] +
//! [`PlaybackHandle::stop`] / [`PlaybackHandle::status`] /
//! [`PlaybackHandle::levels`] / [`PlaybackHandle::close`]. Working
//! example binary at `examples/play-demo.rs`.
//!
//! Not yet wired through `PlaybackHandle`: pause / resume (signals
//! exist; API methods land next), seek (handshake works at the RT
//! and reader layers; API method lands next), MCP tool surface.

#![cfg_attr(test, allow(clippy::float_cmp, clippy::cast_precision_loss))]

mod audio;
mod device;
mod file_source;
mod reader;
mod ring;
mod rt;
mod signals;
mod source;
mod telemetry;
mod types;
mod wav;

pub use audio::{PlaybackHandle, StartError, StopError, open, list_output_devices, output_device_capabilities};
pub use device::DeviceError;
pub use file_source::{FileSource, OpenFileError};
pub use source::{BufferSource, PlaybackSource, SeekError};
pub use types::{
    Backend, BufferSize, DeviceId, OutputCapabilities, OutputDeviceInfo, PlaybackLevels,
    PlaybackSourceSpec, PlaybackSpec, PlaybackState, PlaybackStatus,
};
pub use wav::ParseError;
