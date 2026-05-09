//! cpal output stream construction, telemetry wiring, and
//! [`PlaybackHandle`] state-machine.
//!
//! Threading: the cpal stream's audio callback runs on the platform's
//! RT-priority thread. Inside it we wrap the
//! [`crate::rt::process_output_buffer`] call in
//! `assert_no_alloc::assert_no_alloc(|| …)` — debug builds enforce
//! zero RT allocations. The cpal `Stream` is `!Send` on every backend
//! which propagates to [`PlaybackHandle`]: created, used, and dropped
//! on the same OS thread.
//!
//! See `docs/modules/playback-audio.md` §3.4 (RT side) and §3.5
//! (reader side) for the full design.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, StreamTrait};
use thiserror::Error;

use crate::device::{DeviceError, capabilities_impl, find_device};
use crate::file_source::{FileSource, OpenFileError};
use crate::reader::spawn_reader;
use crate::ring::{DEFAULT_HEADROOM_MS, build_ring};
use crate::rt::process_output_buffer;
use crate::signals::TransportSignals;
use crate::source::{BufferSource, PlaybackSource};
use crate::telemetry::Telemetry;
use crate::types::{
    BufferSize, DeviceId, OutputCapabilities, OutputDeviceInfo, PlaybackLevels, PlaybackSourceSpec,
    PlaybackSpec, PlaybackState, PlaybackStatus,
};

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

/// Errors returned by [`open`].
#[derive(Debug, Error)]
pub enum StartError {
    #[error("device not found: {0:?}")]
    DeviceNotFound(DeviceId),
    #[error("backend error: {0}")]
    BackendError(String),
    #[error("source unreadable: {0}")]
    SourceUnreadable(String),
    #[error("file format mismatch — source {source_rate} Hz, device {device_rate} Hz")]
    RateMismatch { source_rate: u32, device_rate: u32 },
    #[error("channel mismatch — source {source_channels} ch, device {device_channels} ch")]
    ChannelMismatch { source_channels: u16, device_channels: u16 },
    #[error("device does not support {requested} Hz; supported: {supported:?}")]
    RateUnsupportedByDevice { requested: u32, supported: Vec<u32> },
    #[error("device does not support {requested} channels; supported: {supported:?}")]
    ChannelsUnsupportedByDevice { requested: u16, supported: Vec<u16> },
    #[error("buffer size {requested} out of range [{min}, {max}]")]
    BufferSizeOutOfRange { requested: u32, min: u32, max: u32 },
    #[error("failed to build cpal output stream: {0}")]
    BuildStreamFailed(String),
    #[error("failed to start cpal output stream: {0}")]
    PlayStreamFailed(String),
}

impl From<DeviceError> for StartError {
    fn from(e: DeviceError) -> Self {
        match e {
            DeviceError::DeviceNotFound { id } => StartError::DeviceNotFound(id),
            DeviceError::BackendError(s) => StartError::BackendError(s),
        }
    }
}

impl From<OpenFileError> for StartError {
    fn from(e: OpenFileError) -> Self {
        StartError::SourceUnreadable(e.to_string())
    }
}

/// Errors returned by [`PlaybackHandle::stop`].
#[derive(Debug, Error)]
pub enum StopError {
    #[error("not active (current state: {current:?})")]
    NotActive { current: PlaybackState },
}

/// Errors returned by [`PlaybackHandle::pause`] and [`PlaybackHandle::resume`].
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("not playing (current state: {current:?})")]
    NotPlaying { current: PlaybackState },
    #[error("not paused (current state: {current:?})")]
    NotPaused { current: PlaybackState },
    #[error("backend pause/play failed: {0}")]
    BackendFailed(String),
}

/// Errors returned by [`PlaybackHandle::seek`].
#[derive(Debug, Error)]
pub enum SeekError {
    #[error("not seekable (current state: {current:?})")]
    NotSeekable { current: PlaybackState },
    #[error("seek out of bounds: requested frame {requested}, source has {max} frames")]
    OutOfBounds { requested: u64, max: u64 },
}

/// Internal state held by [`PlaybackHandle`]. Not exposed.
struct Internals {
    state: PlaybackState,
    sample_rate: u32,
    channels: u16,
    duration_frames: Option<u64>,
    telemetry: Arc<Telemetry>,
    signals: Arc<TransportSignals>,
    /// `!Send` — keep the handle on the thread that built the stream.
    stream: Option<cpal::Stream>,
    reader_join: Option<JoinHandle<()>>,
}

/// Opaque handle returned by [`open`]. `!Send` (cpal's `Stream` is
/// `!Send` on every backend) — keep it on the thread that opened it.
pub struct PlaybackHandle {
    inner: Internals,
}

/// Public entry point: enumerate every output device the platform sees.
pub fn list_output_devices() -> Vec<OutputDeviceInfo> {
    crate::device::list_output_devices_impl()
}

/// Public entry point: ask one output device about its supported
/// sample rates, buffer sizes, and channel counts.
pub fn output_device_capabilities(id: &DeviceId) -> Result<OutputCapabilities, DeviceError> {
    crate::device::capabilities_impl(id)
}

/// Public entry point: open the output device, load the source, build
/// and start the cpal output stream, spawn the reader thread. On
/// success the returned [`PlaybackHandle`] is in [`PlaybackState::Playing`].
pub fn open(spec: PlaybackSpec) -> Result<PlaybackHandle, StartError> {
    let device = find_device(&spec.device_id)?;
    let caps = capabilities_impl(&spec.device_id)?;

    // Load the source first so we can validate rate/channels BEFORE
    // building the cpal stream.
    let (source, source_rate, source_channels, duration_frames): (
        Box<dyn PlaybackSource>,
        u32,
        u16,
        Option<u64>,
    ) = match spec.source {
        PlaybackSourceSpec::File { path } => {
            let fs = FileSource::open(&path)?;
            let sr = fs.sample_rate();
            let ch = fs.channels();
            let dur = fs.duration_frames();
            (Box::new(fs), sr, ch, dur)
        }
        PlaybackSourceSpec::Buffer {
            samples,
            sample_rate,
            channels,
        } => {
            let bs = BufferSource::new(samples, sample_rate, channels).ok_or_else(|| {
                StartError::SourceUnreadable(
                    "buffer source: samples.len() must be a whole multiple of channels and channels > 0".into(),
                )
            })?;
            let dur = bs.duration_frames();
            (Box::new(bs), sample_rate, channels, dur)
        }
    };

    if !caps.supported_sample_rates.contains(&source_rate) {
        return Err(StartError::RateUnsupportedByDevice {
            requested: source_rate,
            supported: caps.supported_sample_rates,
        });
    }
    if !caps.channels.contains(&source_channels) {
        return Err(StartError::ChannelsUnsupportedByDevice {
            requested: source_channels,
            supported: caps.channels,
        });
    }

    // Pick the f32 config that matches the source's rate + channels.
    let supported = device
        .supported_output_configs()
        .map_err(|e| StartError::BackendError(format!("supported_output_configs: {e}")))?;
    let chosen = supported
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32 && c.channels() == source_channels)
        .find(|c| {
            c.min_sample_rate().0 <= source_rate && c.max_sample_rate().0 >= source_rate
        })
        .ok_or_else(|| StartError::RateMismatch {
            source_rate,
            device_rate: caps.default_sample_rate,
        })?;

    // Validate buffer size against the chosen config's range.
    if let cpal::SupportedBufferSize::Range { min, max } = chosen.buffer_size() {
        if let BufferSize::Fixed(n) = spec.buffer_size {
            if n < *min || n > *max {
                return Err(StartError::BufferSizeOutOfRange {
                    requested: n,
                    min: *min,
                    max: *max,
                });
            }
        }
    }

    let stream_config = cpal::StreamConfig {
        channels: source_channels,
        sample_rate: cpal::SampleRate(source_rate),
        buffer_size: match spec.buffer_size {
            BufferSize::Default => cpal::BufferSize::Default,
            BufferSize::Fixed(n) => cpal::BufferSize::Fixed(n),
        },
    };

    let (producer, consumer) =
        build_ring(source_rate, source_channels, DEFAULT_HEADROOM_MS);
    let telemetry = Arc::new(Telemetry::new(source_channels));
    let signals = Arc::new(TransportSignals::new());

    let telemetry_cb = Arc::clone(&telemetry);
    let signals_cb = Arc::clone(&signals);
    let mut consumer_cb = consumer;
    let channels_cb = source_channels;

    let stream = device
        .build_output_stream(
            &stream_config,
            move |out: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                assert_no_alloc::assert_no_alloc(|| {
                    process_output_buffer(
                        out,
                        channels_cb,
                        &telemetry_cb,
                        &mut consumer_cb,
                        &signals_cb,
                    );
                });
            },
            |err| {
                tracing::error!(?err, "cpal output stream error");
            },
            None,
        )
        .map_err(|e| StartError::BuildStreamFailed(e.to_string()))?;

    // Spawn the reader before starting the stream so the ring has
    // some pre-roll before the device starts pulling.
    let reader_join = spawn_reader(source, producer, Arc::clone(&signals));

    stream
        .play()
        .map_err(|e| StartError::PlayStreamFailed(e.to_string()))?;

    Ok(PlaybackHandle {
        inner: Internals {
            state: PlaybackState::Playing,
            sample_rate: source_rate,
            channels: source_channels,
            duration_frames,
            telemetry,
            signals,
            stream: Some(stream),
            reader_join: Some(reader_join),
        },
    })
}

impl PlaybackHandle {
    /// Pause playback. State transitions to [`PlaybackState::Paused`].
    /// On backends where `cpal::Stream::pause` honours the request
    /// (most), the audio callback halts immediately. On backends
    /// where it silently no-ops (some PipeWire-bridged ALSA devices —
    /// see plan §5.7 and the recorder's `978fa91`), audio keeps
    /// playing. v0.1 ships the simple path; a verify-and-rebuild
    /// fallback that drops + re-opens the stream lands as a
    /// follow-up commit if real backend behaviour requires it.
    pub fn pause(&mut self) -> Result<(), TransportError> {
        if self.inner.state != PlaybackState::Playing {
            return Err(TransportError::NotPlaying { current: self.inner.state });
        }
        if let Some(stream) = self.inner.stream.as_ref() {
            stream
                .pause()
                .map_err(|e| TransportError::BackendFailed(e.to_string()))?;
        }
        self.inner.state = PlaybackState::Paused;
        Ok(())
    }

    /// Seek to `frame`. Allowed from `Playing`, `Paused`, or `Ended`
    /// (in which case the seek implicitly transitions back toward
    /// `Playing` as the audio thread plays out the new region).
    /// Out-of-range frames return [`SeekError::OutOfBounds`].
    ///
    /// Triggers the seek-flush handshake (plan §5.6): the reader
    /// signals the audio callback to drain the ring + jump the
    /// position counter, then re-positions the source. User-visible
    /// effect is one period (~1.3 ms at 48 kHz / 64 buffer) of
    /// silence at the seek point.
    pub fn seek(&mut self, frame: u64) -> Result<(), SeekError> {
        if matches!(
            self.state(),
            PlaybackState::Idle
                | PlaybackState::Closed
                | PlaybackState::Errored
                | PlaybackState::Stopped
                | PlaybackState::Loading
        ) {
            return Err(SeekError::NotSeekable { current: self.state() });
        }
        if let Some(dur) = self.inner.duration_frames {
            if frame > dur {
                return Err(SeekError::OutOfBounds { requested: frame, max: dur });
            }
        }
        self.inner.signals.request_seek(frame);
        Ok(())
    }

    /// Resume from Paused. Position carries over; reader was idle
    /// while paused (ring full or empty depending on timing).
    pub fn resume(&mut self) -> Result<(), TransportError> {
        if self.inner.state != PlaybackState::Paused {
            return Err(TransportError::NotPaused { current: self.inner.state });
        }
        if let Some(stream) = self.inner.stream.as_ref() {
            stream
                .play()
                .map_err(|e| TransportError::BackendFailed(e.to_string()))?;
        }
        self.inner.state = PlaybackState::Playing;
        Ok(())
    }

    /// Stop playback immediately. Drops the stream, signals the
    /// reader thread to exit, joins it. Idempotent — safe to call
    /// from any non-Closed state.
    pub fn stop(&mut self) -> Result<PlaybackStatus, StopError> {
        // stop() is also legal from Paused (and from the lazy Ended,
        // which we observe via state()).
        let observed = self.state();
        if matches!(
            observed,
            PlaybackState::Idle | PlaybackState::Closed | PlaybackState::Errored
        ) {
            return Err(StopError::NotActive { current: observed });
        }

        // Tell both the audio callback and the reader to exit.
        self.inner.signals.stop_request.store(true, Ordering::Release);

        // Drop the stream (joins the cpal poll thread on backends where
        // that thread exists; pause() first to be friendly).
        if let Some(stream) = self.inner.stream.as_ref() {
            // Best-effort pause — see plan §5.7 on cpal's pause trap.
            // We follow up with drop unconditionally.
            let _ = stream.pause();
        }
        self.inner.stream.take();

        // Reader will observe stop_request and exit; join it.
        if let Some(handle) = self.inner.reader_join.take() {
            // If the reader panicked we don't want to propagate from stop().
            let _ = handle.join();
        }

        self.inner.state = PlaybackState::Stopped;
        Ok(self.status())
    }

    /// Read the playback position in frames. Cheap (single atomic load).
    pub fn position_frames(&self) -> u64 {
        self.inner
            .telemetry
            .position_frames
            .load(Ordering::Acquire)
    }

    /// Per-channel last-buffer peak, in dBFS. Returns `f32::NEG_INFINITY`
    /// for an out-of-range channel index.
    pub fn peak_dbfs(&self, channel: u16) -> f32 {
        if channel >= self.inner.channels {
            return f32::NEG_INFINITY;
        }
        linear_to_dbfs(self.inner.telemetry.peak_value(channel))
    }

    /// Per-channel last-buffer RMS, in dBFS.
    pub fn rms_dbfs(&self, channel: u16) -> f32 {
        if channel >= self.inner.channels {
            return f32::NEG_INFINITY;
        }
        let ms = self.inner.telemetry.mean_square_value(channel);
        linear_to_dbfs(ms.sqrt())
    }

    /// All-channel levels in one snapshot.
    pub fn levels(&self) -> PlaybackLevels {
        let n = usize::from(self.inner.channels);
        let mut peak = Vec::with_capacity(n);
        let mut rms = Vec::with_capacity(n);
        for c in 0..self.inner.channels {
            peak.push(self.peak_dbfs(c));
            rms.push(self.rms_dbfs(c));
        }
        PlaybackLevels { peak_dbfs: peak, rms_dbfs: rms }
    }

    /// Cumulative under-runs since `open`.
    pub fn xrun_count(&self) -> u32 {
        self.inner.telemetry.xrun_count.load(Ordering::Relaxed)
    }

    /// Current state. Includes the lazy `Playing → Ended` transition
    /// when the audio callback has signalled `playback_complete`.
    pub fn state(&self) -> PlaybackState {
        if self.inner.state == PlaybackState::Playing
            && self.inner.signals.playback_complete.load(Ordering::Acquire)
        {
            PlaybackState::Ended
        } else {
            self.inner.state
        }
    }

    /// Combined snapshot — state, position, duration, xruns.
    pub fn status(&self) -> PlaybackStatus {
        let pos = self.position_frames();
        let pos_secs = pos as f64 / self.inner.sample_rate as f64;
        let dur_secs = self
            .inner
            .duration_frames
            .map(|d| d as f64 / self.inner.sample_rate as f64);
        // Resolve the lazy "playing → ended" transition for callers.
        let state = if self.inner.state == PlaybackState::Playing
            && self.inner.signals.playback_complete.load(Ordering::Acquire)
        {
            PlaybackState::Ended
        } else {
            self.inner.state
        };
        PlaybackStatus {
            state,
            position_frames: pos,
            position_seconds: pos_secs,
            duration_frames: self.inner.duration_frames,
            duration_seconds: dur_secs,
            sample_rate: self.inner.sample_rate,
            channels: self.inner.channels,
            xrun_count: self.xrun_count(),
        }
    }

    /// Source sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.inner.sample_rate
    }

    /// Channel count.
    pub fn channels(&self) -> u16 {
        self.inner.channels
    }

    /// Tear everything down. Idempotent. Consumes the handle.
    pub fn close(mut self) {
        // Make sure stop ran (no-op if already stopped).
        if !matches!(
            self.inner.state,
            PlaybackState::Stopped | PlaybackState::Closed | PlaybackState::Errored | PlaybackState::Ended
        ) {
            let _ = self.stop();
        }
        // Drop stream first (pauses callback thread); then signals;
        // then join reader if still around.
        self.inner.stream.take();
        if let Some(h) = self.inner.reader_join.take() {
            self.inner.signals.stop_request.store(true, Ordering::Release);
            let _ = h.join();
        }
        self.inner.state = PlaybackState::Closed;
    }
}

