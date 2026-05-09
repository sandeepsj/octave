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
// (AtomicBool used inside Telemetry::non_finite_seen below.)
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
    /// Set once by the RT path the first time a sample arrives that is
    /// not finite (NaN / Inf). The RT path replaces the offending
    /// sample with 0.0 in both the meter and the ring; this flag lets
    /// the writer / UI surface the fact that something downstream of
    /// the analog input produced bad floats.
    pub non_finite_seen: AtomicBool,
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
            non_finite_seen: AtomicBool::new(false),
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
    /// The path the caller asked us to record to. Held here so
    /// `cancel()` can attempt deletion even if the writer thread
    /// panicked before producing a `WriterOutcome` — without this
    /// the partial file would leak silently on writer panic.
    pub output_path: std::path::PathBuf,
}

const FLOOR_DBFS: f32 = -180.0;
const FLOOR_LINEAR: f32 = 1e-9;

/// Plan §5.5 universal period bounds (independent of device caps).
/// Below 0.5 ms even modern hosts xrun; above 100 ms latency is too
/// high to be useful for monitoring.
const MIN_PERIOD_MS: f32 = 0.5;
const MAX_PERIOD_MS: f32 = 100.0;

#[allow(clippy::cast_precision_loss)]
fn period_within_plan_bounds(buffer_size_samples: u32, sample_rate: u32) -> bool {
    if sample_rate == 0 {
        return false;
    }
    let period_ms = (buffer_size_samples as f32 / sample_rate as f32) * 1000.0;
    (MIN_PERIOD_MS..=MAX_PERIOD_MS).contains(&period_ms)
}

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

    // v0.1 WAV writer ships the plain (non-EXTENSIBLE) header, which
    // RIFF restricts to ≤ 2 channels. Reject ≥ 3 here at the public
    // entry point — without this, record() reaches WavWriter::create
    // and panics on a perfectly valid 4-channel device spec. Plan §4.5
    // EXTENSIBLE form lands when multi-channel is needed.
    if spec.channels > 2 {
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

    // Validate buffer_size:
    //   1. Against the device's reported [min, max] range (caps).
    //   2. Against plan §5.5's universal period bounds: 0.5 ms ≤ period ≤ 100 ms.
    // BufferSize::Default opts out of (1) — we let cpal pick — but we still
    // reject the rare device whose own caps fall entirely outside §5.5.
    if let BufferSize::Fixed(n) = spec.buffer_size {
        if n < caps.min_buffer_size || n > caps.max_buffer_size {
            return Err(OpenError::FormatUnsupported {
                requested: Box::new(spec),
                supported: Box::new(caps),
            });
        }
        if !period_within_plan_bounds(n, spec.sample_rate) {
            return Err(OpenError::FormatUnsupported {
                requested: Box::new(spec),
                supported: Box::new(caps),
            });
        }
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
            .map_err(|e| {
                // Log the underlying io::Error before collapsing it
                // into a RecordError variant — without this the
                // operator only sees "OutputPathInvalid" with no clue
                // whether it was NotFound, IsADirectory, or something
                // else. Plan §8 wants discoverable failure modes.
                tracing::warn!(
                    err = %e,
                    err_kind = ?e.kind(),
                    path = %output_path.display(),
                    "WavWriter::create failed"
                );
                match e.kind() {
                    std::io::ErrorKind::PermissionDenied => {
                        RecordError::PermissionDenied(output_path.to_path_buf())
                    }
                    std::io::ErrorKind::StorageFull => RecordError::DiskFull,
                    _ => RecordError::OutputPathInvalid(output_path.to_path_buf()),
                }
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
        self.inner.writer = Some(ActiveWriter {
            join,
            stop,
            cancel,
            output_path: output_path.to_path_buf(),
        });
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

        // Pause first so the audio thread stops feeding the ring; otherwise
        // the writer races a producer that never quits and `stop()` runs
        // unbounded. Best-effort — if the backend refuses pause, the writer
        // will still drain whatever's left and finalize.
        if let Some(stream) = self.inner.stream.as_ref() {
            if let Err(e) = stream.pause() {
                tracing::warn!(?e, "cpal pause failed; proceeding with finalize");
            }
        }

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

        let started_at = self
            .inner
            .started_at
            .take()
            .expect("started_at present in Recording");
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

        // Pause first to stop new samples — otherwise the writer's cancel
        // check races a producer that never quits.
        if let Some(stream) = self.inner.stream.as_ref() {
            if let Err(e) = stream.pause() {
                tracing::warn!(?e, "cpal pause failed; proceeding with cancel");
            }
        }

        let writer = self
            .inner
            .writer
            .take()
            .expect("writer present in Recording");
        writer.cancel.store(true, Ordering::Release);

        // The intent is "remove the partial file the user said they
        // didn't want". Three failure paths to make sure we ALWAYS
        // attempt the deletion, not just on the happy join path:
        //
        //  - writer.join() returns Err  → thread panicked. Outcome lost.
        //  - writer.join() returns Ok(Err(io)) → writer hit an io::Error.
        //  - writer.join() returns Ok(Ok(outcome)) → normal cancel return.
        //
        // The original output_path is held in ActiveWriter so we can
        // remove it even when WriterOutcome is unavailable. We log
        // the panic / io error via tracing rather than swallowing —
        // plan §8 wants the user-visible failure modes discoverable.
        let cancel_path = writer.output_path;
        match writer.join.join() {
            Ok(Ok(_outcome)) => {
                // Normal cancel: the writer already left the file
                // (or didn't, if cancel beat the open). Best-effort delete.
                if let Err(e) = std::fs::remove_file(&cancel_path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(err = %e, path = %cancel_path.display(),
                                       "cancel: file delete failed");
                    }
                }
            }
            Ok(Err(io_err)) => {
                tracing::warn!(err = %io_err, path = %cancel_path.display(),
                               "cancel: writer thread returned io error; attempting delete anyway");
                let _ = std::fs::remove_file(&cancel_path);
            }
            Err(panic_payload) => {
                tracing::warn!(?panic_payload, path = %cancel_path.display(),
                               "cancel: writer thread panicked; attempting delete anyway");
                let _ = std::fs::remove_file(&cancel_path);
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

    // ---------- §5.5 buffer-size period bounds ----------

    #[test]
    fn period_bounds_accept_typical_buffer_sizes() {
        // 64 samples @ 48 kHz = 1.33 ms — well inside [0.5, 100] ms.
        assert!(period_within_plan_bounds(64, 48_000));
        // 1024 samples @ 48 kHz = 21.3 ms — top of "monitorable" warn band, still in.
        assert!(period_within_plan_bounds(1024, 48_000));
        // 32 samples @ 192 kHz = 0.167 ms — below 0.5 ms floor.
        assert!(!period_within_plan_bounds(32, 192_000));
        // 4800 samples @ 48 kHz = 100 ms — at the ceiling, accepted.
        assert!(period_within_plan_bounds(4800, 48_000));
        // 4801 samples @ 48 kHz = 100.02 ms — above ceiling.
        assert!(!period_within_plan_bounds(4801, 48_000));
    }

    #[test]
    fn period_bounds_reject_zero_sample_rate() {
        // Defensive: division by 0 must not panic.
        assert!(!period_within_plan_bounds(64, 0));
    }
}
