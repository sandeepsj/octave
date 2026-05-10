//! Device enumeration is now provided by the shared
//! [`octave-audio-devices`] crate. This module re-exports the relevant
//! types so existing `octave_player::*` paths keep compiling and so
//! call sites don't need to learn a third crate name.
//!
//! See `docs/modules/playback-audio.md` §3.3.1 for the rationale
//! behind the unified catalog (the previous per-engine catalogs
//! deadlocked each other on Linux when the same physical device
//! appeared in both directions — cpal's `DeviceHandles::open` opens
//! both PCMs during enumeration).

pub use octave_audio_devices::{DeviceCatalog, DeviceError};
