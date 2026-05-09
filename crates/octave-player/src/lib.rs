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
//! Scaffolding commit. The current crate exposes the
//! [`PlaybackSource`] trait and its in-memory [`BufferSource`] impl.
//! File source, RT path (cpal output stream + ring), reader thread,
//! seek handshake, pause/rebuild fallback, and MCP exposure land in
//! follow-up commits.

#![cfg_attr(test, allow(clippy::float_cmp, clippy::cast_precision_loss))]

mod file_source;
mod ring;
mod source;
mod wav;

pub use file_source::{FileSource, OpenFileError};
pub use source::{BufferSource, PlaybackSource, SeekError};
pub use wav::ParseError;
