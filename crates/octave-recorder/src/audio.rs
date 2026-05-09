//! cpal stream construction, telemetry atomics, and `RecordingHandle`
//! state-machine wiring.
//!
//! Threading: the cpal stream's audio callback runs on the platform's
//! RT-priority thread. Inside it we call `assert_no_alloc::assert_no_alloc(|| …)`
//! around [`crate::rt::process_input_buffer`]; the test global allocator
//! (set in `lib.rs`) panics on any allocation that escapes RT discipline.
//! The cpal `Stream` itself is `!Send` on every backend, which propagates
//! to [`crate::RecordingHandle`] — it must be created, used, and dropped
//! on the same OS thread.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::SystemTime;

use cpal::traits::{DeviceTrait, StreamTrait};
use rtrb::Consumer;
use uuid::Uuid;

use crate::device::{capabilities_impl, find_device};
use crate::ring::{DEFAULT_HEADROOM_MS, build_ring};
use crate::rt::process_input_buffer;
use crate::state::RecorderState;
use crate::wav::WavWriter;
use crate::writer::{WriterOutcome, spawn_writer};
use crate::{
    ArmError, BufferSize, CancelError, OpenError, RecordError, RecordedClip, RecordingHandle,
    RecordingSpec, StopError,
};

/// Cross-thread audio telemetry. The RT thread writes; the API/UI threads read.
///
/// Per-channel atomics store `f32::to_bits()` so we can use `AtomicU32`
/// without locking. Single-writer (RT) discipline makes the running-peak
/// load-then-store race-free.
pub(crate) struct Telemetry {
    pub peak: Vec<AtomicU32>,
    pub running_peak: Vec<AtomicU32>,
    pub mean_square: Vec<AtomicU32>,
    pub xrun_count: AtomicU32,
    pub dropped_samples: AtomicU64,
}

impl Telemetry {
    pub fn new(channels: u16) -> Arc<Self> {
        let n = usize::from(channels);
        Arc::new(Self {
            peak: (0..n).map(|_| AtomicU32::new(0)).collect(),
            running_peak: (0..n).map(|_| AtomicU32::new(0)).collect(),
            mean_square: (0..n).map(|_| AtomicU32::new(0)).collect(),
            xrun_count: AtomicU32::new(0),
            dropped_samples: AtomicU64::new(0),
        })
    }

    pub fn peak_value(&self, channel: u16) -> f32 {
        f32::from_bits(self.peak[usize::from(channel)].load(Ordering::Relaxed))
    }

    pub fn running_peak_value(&self, channel: u16) -> f32 {
        f32::from_bits(self.running_peak[usize::from(channel)].load(Ordering::Relaxed))
    }

    pub fn mean_square_value(&self, channel: u16) -> f32 {
        f32::from_bits(self.mean_square[usize::from(channel)].load(Ordering::Relaxed))
    }

    pub fn reset_running_peaks(&self) {
        for ap in &self.running_peak {
            ap.store(0, Ordering::Relaxed);
        }
    }
}

/// Internal state owned by [`RecordingHandle`]. Not exposed.
pub(crate) struct Internals {
    pub state: RecorderState,
    pub sample_rate: u32,
    pub channels: u16,
    pub telemetry: Arc<Telemetry>,
    pub consumer: Option<Consumer<f32>>,
    pub stream: Option<cpal::Stream>,
    pub writer: Option<ActiveWriter>,
    pub started_at: Option<SystemTime>,
}

pub(crate) struct ActiveWriter {
    pub join: JoinHandle<std::io::Result<WriterOutcome>>,
    pub stop: Arc<AtomicBool>,
    pub cancel: Arc<AtomicBool>,
}

const FLOOR_DBFS: f32 = -180.0;
const FLOOR_LINEAR: f32 = 1e-9;

#[allow(clippy::cast_precision_loss)]
fn linear_to_dbfs(x: f32) -> f32 {
    if x <= FLOOR_LINEAR {
        FLOOR_DBFS
    } else {
        20.0 * x.log10()
    }
}

fn is_metering_state(state: RecorderState) -> bool {
    matches!(
        state,
        RecorderState::Armed
            | RecorderState::Recording
            | RecorderState::Stopping
            | RecorderState::Cancelling
    )
}

/// Public entry point: open a device, build the cpal stream (parked), and
/// return an [`Idle`](RecorderState::Idle) handle ready to be `arm`ed.
pub fn open(spec: RecordingSpec) -> Result<RecordingHandle, OpenError> {
    let device = find_device(&spec.device_id)?;
    let caps = capabilities_impl(&spec.device_id)?;

    if !caps.channels.contains(&spec.channels)
        || spec.sample_rate < caps.min_sample_rate
        || spec.sample_rate > caps.max_sample_rate
    {
        return Err(OpenError::FormatUnsupported {
            requested: Box::new(spec),
            supported: Box::new(caps),
        });
    }

    // Pick the f32-format config that matches the requested rate + channels.
    let supported = device
        .supported_input_configs()
        .map_err(|e| OpenError::BackendError(format!("supported_input_configs: {e}")))?;
    let chosen = supported
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32 && c.channels() == spec.channels)
        .find(|c| {
            c.min_sample_rate().0 <= spec.sample_rate && c.max_sample_rate().0 >= spec.sample_rate
        });
    if chosen.is_none() {
        return Err(OpenError::FormatUnsupported {
            requested: Box::new(spec),
            supported: Box::new(caps),
        });
    }

    let stream_config = cpal::StreamConfig {
        channels: spec.channels,
        sample_rate: cpal::SampleRate(spec.sample_rate),
        buffer_size: match spec.buffer_size {
            BufferSize::Default => cpal::BufferSize::Default,
            BufferSize::Fixed(n) => cpal::BufferSize::Fixed(n),
        },
    };

    let (mut producer, consumer) = build_ring(spec.sample_rate, spec.channels, DEFAULT_HEADROOM_MS);
    let telemetry = Telemetry::new(spec.channels);
    let telemetry_cb = telemetry.clone();
    let channels_cb = spec.channels;

    let stream = device
        .build_input_stream(
            &stream_config,
            move |samples: &[f32], _info: &cpal::InputCallbackInfo| {
                assert_no_alloc::assert_no_alloc(|| {
                    process_input_buffer(samples, channels_cb, &telemetry_cb, &mut producer);
                });
            },
            |err| {
                tracing::error!(?err, "cpal input stream error");
            },
            None,
        )
        .map_err(|e| OpenError::BackendError(format!("build_input_stream: {e}")))?;

    Ok(RecordingHandle {
        inner: Internals {
            state: RecorderState::Idle,
            sample_rate: spec.sample_rate,
            channels: spec.channels,
            telemetry,
            consumer: Some(consumer),
            stream: Some(stream),
            writer: None,
            started_at: None,
        },
    })
}

impl RecordingHandle {
    pub fn arm(&mut self) -> Result<(), ArmError> {
        if self.inner.state != RecorderState::Idle {
            return Err(ArmError::NotIdle {
                current: self.inner.state,
            });
        }
        let stream = self
            .inner
            .stream
            .as_ref()
            .expect("stream present in Idle");
        stream
            .play()
            .map_err(|e| ArmError::BuildStreamFailed(e.to_string()))?;
        self.inner.state = RecorderState::Armed;
        Ok(())
    }

    pub fn record(&mut self, output_path: &Path) -> Result<(), RecordError> {
        if self.inner.state != RecorderState::Armed {
            return Err(RecordError::NotArmed {
                current: self.inner.state,
            });
        }
        let writer = WavWriter::create(output_path, self.inner.sample_rate, self.inner.channels)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    RecordError::PermissionDenied(output_path.to_path_buf())
                }
                std::io::ErrorKind::StorageFull => RecordError::DiskFull,
                _ => RecordError::OutputPathInvalid(output_path.to_path_buf()),
            })?;

        let consumer = self.inner.consumer.take().ok_or(RecordError::NotArmed {
            current: self.inner.state,
        })?;
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let join = spawn_writer(
            consumer,
            writer,
            output_path.to_path_buf(),
            stop.clone(),
            cancel.clone(),
        );

        self.inner.telemetry.reset_running_peaks();
        self.inner.writer = Some(ActiveWriter { join, stop, cancel });
        self.inner.started_at = Some(SystemTime::now());
        self.inner.state = RecorderState::Recording;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<RecordedClip, StopError> {
        if self.inner.state != RecorderState::Recording {
            return Err(StopError::NotRecording {
                current: self.inner.state,
            });
        }
        self.inner.state = RecorderState::Stopping;

        let writer = self
            .inner
            .writer
            .take()
            .expect("writer present in Recording");
        writer.stop.store(true, Ordering::Release);
        let outcome = writer
            .join
            .join()
            .map_err(|_| StopError::FinalizeFailed("writer thread panicked".into()))?
            .map_err(|e| StopError::FinalizeFailed(e.to_string()))?;

        let fin = outcome
            .finalized
            .ok_or_else(|| StopError::FinalizeFailed("writer returned no clip".into()))?;

        let started_at = self.inner.started_at.take().unwrap_or_else(SystemTime::now);
        let mut peak_dbfs = Vec::with_capacity(usize::from(self.inner.channels));
        for c in 0..self.inner.channels {
            peak_dbfs.push(linear_to_dbfs(self.inner.telemetry.running_peak_value(c)));
        }

        #[allow(clippy::cast_precision_loss)]
        let duration_seconds = fin.frame_count as f64 / f64::from(self.inner.sample_rate);

        let clip = RecordedClip {
            path: outcome.path,
            uuid: Uuid::new_v4(),
            sample_rate: self.inner.sample_rate,
            channels: self.inner.channels,
            frame_count: fin.frame_count,
            duration_seconds,
            started_at,
            xrun_count: self.inner.telemetry.xrun_count.load(Ordering::Relaxed),
            dropped_samples: self.inner.telemetry.dropped_samples.load(Ordering::Relaxed),
            peak_dbfs,
        };
        // v0.1: stop is terminal (consumer wasn't returned); the handle
        // moves to Idle so users know `record()` would fail. close() is
        // the only legal next move. Multi-take support arrives when the
        // writer returns the consumer.
        self.inner.state = RecorderState::Idle;
        Ok(clip)
    }

    pub fn cancel(&mut self) -> Result<(), CancelError> {
        if self.inner.state != RecorderState::Recording {
            return Err(CancelError::NotRecording {
                current: self.inner.state,
            });
        }
        self.inner.state = RecorderState::Cancelling;

        let writer = self
            .inner
            .writer
            .take()
            .expect("writer present in Recording");
        writer.cancel.store(true, Ordering::Release);
        // Best-effort delete; we ignore join / io errors per plan §13.12.
        if let Ok(Ok(outcome)) = writer.join.join() {
            if outcome.cancelled {
                let _ = std::fs::remove_file(&outcome.path);
            }
        }
        self.inner.state = RecorderState::Idle;
        Ok(())
    }

    pub fn peak_dbfs(&self, channel: u16) -> f32 {
        if !is_metering_state(self.inner.state) || channel >= self.inner.channels {
            return f32::NEG_INFINITY;
        }
        linear_to_dbfs(self.inner.telemetry.peak_value(channel))
    }

    pub fn rms_dbfs(&self, channel: u16) -> f32 {
        if !is_metering_state(self.inner.state) || channel >= self.inner.channels {
            return f32::NEG_INFINITY;
        }
        let ms = self.inner.telemetry.mean_square_value(channel);
        linear_to_dbfs(ms.sqrt())
    }

    pub fn xrun_count(&self) -> u32 {
        self.inner.telemetry.xrun_count.load(Ordering::Relaxed)
    }

    pub fn dropped_samples(&self) -> u64 {
        self.inner.telemetry.dropped_samples.load(Ordering::Relaxed)
    }

    pub fn state(&self) -> RecorderState {
        self.inner.state
    }

    pub fn close(mut self) {
        // Drop stream first (pauses callback), then everything else falls.
        self.inner.stream.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbfs_floor_clamps_silence_to_minus_180() {
        assert_eq!(linear_to_dbfs(0.0), FLOOR_DBFS);
        assert_eq!(linear_to_dbfs(FLOOR_LINEAR / 2.0), FLOOR_DBFS);
    }

    #[test]
    fn dbfs_full_scale_is_zero() {
        assert!((linear_to_dbfs(1.0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn dbfs_half_scale_is_minus_six() {
        assert!((linear_to_dbfs(0.5) - (-6.0_f32)).abs() < 0.05);
    }

    #[test]
    fn telemetry_starts_at_zero() {
        let t = Telemetry::new(4);
        for c in 0..4 {
            assert_eq!(t.peak_value(c), 0.0);
            assert_eq!(t.running_peak_value(c), 0.0);
            assert_eq!(t.mean_square_value(c), 0.0);
        }
    }
}
