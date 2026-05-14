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
//! Working end-to-end: device enumeration + capability query, file
//! source (WAV / RF64), in-memory buffer source, RT output callback
//! with seek/EOF handshake atomics, reader thread, [`DeviceCatalog::start`] /
//! [`PlaybackHandle::pause`] / [`PlaybackHandle::resume`] (with the
//! verify-and-rebuild fallback for the cpal pause silent-no-op trap)
//! / [`PlaybackHandle::seek`] / [`PlaybackHandle::stop`] /
//! [`PlaybackHandle::status`] / [`PlaybackHandle::levels`] /
//! [`PlaybackHandle::close`]. Working example binary at
//! `examples/play-demo.rs`. The MCP tool surface (`output_*` tools)
//! lives in the separate `octave-engine` crate.

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

pub use audio::{
    PlaybackHandle, SeekError as PlaybackSeekError, StartError, StopError, TransportError, start,
};
pub use device::{DeviceCatalog, DeviceError};
pub use file_source::{FileSource, OpenFileError};
pub use source::{BufferSource, PlaybackSource, SeekError};
pub use types::{
    Backend, BufferSize, DeviceId, OutputCapabilities, OutputDeviceInfo, PlaybackLevels,
    PlaybackSourceSpec, PlaybackSpec, PlaybackState, PlaybackStatus,
};
pub use wav::ParseError;
