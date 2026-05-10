//! Error variants per `docs/modules/record-audio.md` §9.3.

use std::path::PathBuf;

use octave_audio_devices::DeviceError;
use thiserror::Error;

use crate::{Capabilities, DeviceId, RecorderState, RecordingSpec};

/// Failures from [`crate::open`] and [`crate::device_capabilities`].
#[derive(Debug, Error)]
pub enum OpenError {
    #[error("device not found: {id:?}")]
    DeviceNotFound { id: DeviceId },

    #[error("requested format unsupported by device")]
    FormatUnsupported {
        requested: Box<RecordingSpec>,
        supported: Box<Capabilities>,
    },

    #[error("backend error: {0}")]
    BackendError(String),

    #[error("permission denied")]
    PermissionDenied,
}

impl From<DeviceError> for OpenError {
    fn from(e: DeviceError) -> Self {
        match e {
            DeviceError::DeviceNotFound { id } => OpenError::DeviceNotFound { id },
            DeviceError::BackendError(s) => OpenError::BackendError(s),
        }
    }
}

/// Failures from [`crate::RecordingHandle::arm`].
#[derive(Debug, Error)]
pub enum ArmError {
    #[error("not idle (current: {current:?})")]
    NotIdle { current: RecorderState },

    #[error("build_input_stream failed: {0}")]
    BuildStreamFailed(String),
}

/// Failures from [`crate::RecordingHandle::record`].
#[derive(Debug, Error)]
pub enum RecordError {
    #[error("not armed (current: {current:?})")]
    NotArmed { current: RecorderState },

    #[error("output path invalid: {0}")]
    OutputPathInvalid(PathBuf),

    #[error("permission denied: {0}")]
    PermissionDenied(PathBuf),

    #[error("disk full")]
    DiskFull,
}

/// Failures from [`crate::RecordingHandle::stop`].
#[derive(Debug, Error)]
pub enum StopError {
    #[error("not recording (current: {current:?})")]
    NotRecording { current: RecorderState },

    #[error("finalize failed: {0}")]
    FinalizeFailed(String),
}

/// Failures from [`crate::RecordingHandle::cancel`].
#[derive(Debug, Error)]
pub enum CancelError {
    #[error("not recording (current: {current:?})")]
    NotRecording { current: RecorderState },
}
