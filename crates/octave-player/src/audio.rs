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
    /// Saved at `open()` so [`PlaybackHandle::pause`]'s
    /// verify-and-rebuild fallback (plan §5.7) can re-construct the
    /// pipeline on backends where `cpal::Stream::pause` silently
    /// no-ops (PipeWire ALSA bridge in particular).
    spec: PlaybackSpec,
    /// `true` once we've observed pause silently failing — `pause`
    /// dropped the stream + reader and `resume` must rebuild rather
    /// than calling `stream.play`.
    paused_via_drop: bool,
}

const PAUSE_VERIFY_WINDOW: std::time::Duration = std::time::Duration::from_millis(8);
/// Allow this many frames of advance during the verify window before
/// declaring pause a silent failure. One in-flight callback may finish
/// after `pause()` returns (~64-1024 frames depending on buffer size);
/// 4096 is a generous ceiling that still catches the "pause didn't
/// stop anything" case (which advances by tens of thousands of frames
/// in 8 ms at any sample rate).
const PAUSE_VERIFY_GRACE_FRAMES: u64 = 4_096;

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
    let (source, source_rate, source_channels, duration_frames) =
        construct_source(&spec.source)?;

    validate_source_against_device(&spec.device_id, source_rate, source_channels)?;

    let telemetry = Arc::new(Telemetry::new(source_channels));
    let signals = Arc::new(TransportSignals::new());

    let (stream, reader_join) = build_running_pipeline(
        &device,
        source,
        source_rate,
        source_channels,
        spec.buffer_size,
        Arc::clone(&telemetry),
        Arc::clone(&signals),
    )?;

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
            spec,
            paused_via_drop: false,
        },
    })
}

/// Construct the boxed PlaybackSource impl from the user-facing spec.
/// Returns `(source, sample_rate, channels, duration_frames)`.
fn construct_source(
    source_spec: &PlaybackSourceSpec,
) -> Result<(Box<dyn PlaybackSource>, u32, u16, Option<u64>), StartError> {
    Ok(match source_spec {
        PlaybackSourceSpec::File { path } => {
            let fs = FileSource::open(path)?;
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
            let bs = BufferSource::new(Arc::clone(samples), *sample_rate, *channels).ok_or_else(
                || {
                    StartError::SourceUnreadable(
                        "buffer source: samples.len() must be a whole multiple of channels and channels > 0".into(),
                    )
                },
            )?;
            let dur = bs.duration_frames();
            (Box::new(bs), *sample_rate, *channels, dur)
        }
    })
}

fn validate_source_against_device(
    device_id: &DeviceId,
    source_rate: u32,
    source_channels: u16,
) -> Result<(), StartError> {
    let caps = capabilities_impl(device_id)?;
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
    Ok(())
}

/// Build the cpal output stream + spawn the reader thread + start the
/// stream. Used both by initial `open` and by `resume`'s rebuild path.
/// Sleeps `PRE_ROLL_MS` before `stream.play()` so the very first
/// callback finds the ring primed.
fn build_running_pipeline(
    device: &cpal::Device,
    source: Box<dyn PlaybackSource>,
    sample_rate: u32,
    channels: u16,
    buffer_size: BufferSize,
    telemetry: Arc<Telemetry>,
    signals: Arc<TransportSignals>,
) -> Result<(cpal::Stream, JoinHandle<()>), StartError> {
    // Pick the f32 config that matches the source's rate + channels.
    let supported = device
        .supported_output_configs()
        .map_err(|e| StartError::BackendError(format!("supported_output_configs: {e}")))?;
    let chosen = supported
        .filter(|c| c.sample_format() == cpal::SampleFormat::F32 && c.channels() == channels)
        .find(|c| c.min_sample_rate().0 <= sample_rate && c.max_sample_rate().0 >= sample_rate)
        .ok_or_else(|| StartError::RateMismatch {
            source_rate: sample_rate,
            device_rate: 0,
        })?;

    if let cpal::SupportedBufferSize::Range { min, max } = chosen.buffer_size() {
        if let BufferSize::Fixed(n) = buffer_size {
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
        channels,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: match buffer_size {
            BufferSize::Default => cpal::BufferSize::Default,
            BufferSize::Fixed(n) => cpal::BufferSize::Fixed(n),
        },
    };

    let (producer, consumer) = build_ring(sample_rate, channels, DEFAULT_HEADROOM_MS);
    let telemetry_cb = Arc::clone(&telemetry);
    let signals_cb = Arc::clone(&signals);
    let mut consumer_cb = consumer;
    let channels_cb = channels;

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

    // Spawn the reader, then give it 50 ms to prime the ring before
    // `stream.play()` so the very first cpal callback finds samples
    // waiting (avoids the 1-period silence + cold-ring xrun).
    let reader_join = spawn_reader(source, producer, Arc::clone(&signals));
    std::thread::sleep(std::time::Duration::from_millis(50));

    stream
        .play()
        .map_err(|e| StartError::PlayStreamFailed(e.to_string()))?;

    Ok((stream, reader_join))
}

/// Tear down the active pipeline (stream + reader) without dropping
/// the shared state (telemetry, signals, spec). Used by the
/// pause-rebuild path. Resets the transient signal flags so the next
/// pipeline starts clean.
fn teardown_pipeline(
    stream: Option<cpal::Stream>,
    reader: Option<JoinHandle<()>>,
    signals: &TransportSignals,
) {
    // 1. Tell the reader to exit.
    signals.stop_request.store(true, Ordering::Release);
    // 2. Drop the stream. cpal's ALSA backend joins its internal poll
    // thread on drop.
    drop(stream);
    // 3. Join the reader.
    if let Some(h) = reader {
        let _ = h.join();
    }
    // 4. Reset transient signals so a future pipeline starts fresh.
    signals.stop_request.store(false, Ordering::Release);
    signals.flush_request.store(false, Ordering::Release);
    signals.seek_pending.store(false, Ordering::Release);
    signals.eof_seen.store(false, Ordering::Release);
    signals.playback_complete.store(false, Ordering::Release);
}

impl PlaybackHandle {
    /// Pause playback. State → [`PlaybackState::Paused`].
    ///
    /// First tries `cpal::Stream::pause`. After
    /// `PAUSE_VERIFY_WINDOW`, snapshots the position counter; if it
    /// advanced by more than `PAUSE_VERIFY_GRACE_FRAMES`, the backend
    /// silently no-opped the pause (PipeWire-bridged ALSA does this —
    /// plan §5.7 and recorder commit 978fa91). In that case we
    /// **drop the stream and the reader thread**; `resume` later
    /// rebuilds the entire pipeline and re-seeks the source to the
    /// observed position.
    pub fn pause(&mut self) -> Result<(), TransportError> {
        if self.inner.state != PlaybackState::Playing {
            return Err(TransportError::NotPlaying { current: self.inner.state });
        }
        let position_before = self.inner.telemetry.position_frames.load(Ordering::Acquire);
        if let Some(stream) = self.inner.stream.as_ref() {
            stream
                .pause()
                .map_err(|e| TransportError::BackendFailed(e.to_string()))?;
        }
        std::thread::sleep(PAUSE_VERIFY_WINDOW);
        let position_after = self.inner.telemetry.position_frames.load(Ordering::Acquire);
        let delta = position_after.saturating_sub(position_before);

        if delta > PAUSE_VERIFY_GRACE_FRAMES {
            // pause silently no-opped. Tear down the pipeline; resume
            // rebuilds from spec.
            tracing::warn!(
                delta_frames = delta,
                "cpal pause silently no-opped (backend reports can_pause=false), \
                 dropping stream + reader; resume will rebuild"
            );
            let stream = self.inner.stream.take();
            let reader = self.inner.reader_join.take();
            teardown_pipeline(stream, reader, &self.inner.signals);
            // Snapshot position (where we *actually* heard up to,
            // including frames that played after pause() returned).
            // The new pipeline will resume from here.
            self.inner.telemetry.set_position_frames(position_after);
            self.inner.paused_via_drop = true;
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

    /// Resume from Paused. If pause was the simple path, just
    /// `stream.play()`. If pause's verify failed and we tore down,
    /// rebuild the whole pipeline (re-construct the source from the
    /// saved spec, seek to the saved position, build a fresh stream
    /// + ring + reader). Cost: ~50 ms for the rebuild path's pre-roll.
    pub fn resume(&mut self) -> Result<(), TransportError> {
        if self.inner.state != PlaybackState::Paused {
            return Err(TransportError::NotPaused { current: self.inner.state });
        }
        if !self.inner.paused_via_drop {
            // Simple path — stream is alive, just play.
            if let Some(stream) = self.inner.stream.as_ref() {
                stream
                    .play()
                    .map_err(|e| TransportError::BackendFailed(e.to_string()))?;
            }
            self.inner.state = PlaybackState::Playing;
            return Ok(());
        }

        // Rebuild path.
        let device = find_device(&self.inner.spec.device_id)
            .map_err(|e| TransportError::BackendFailed(format!("{e}")))?;
        let (mut source, source_rate, source_channels, _dur) =
            construct_source(&self.inner.spec.source)
                .map_err(|e| TransportError::BackendFailed(format!("{e}")))?;
        let resume_at = self.inner.telemetry.position_frames.load(Ordering::Acquire);
        // Seek the source to where we stopped hearing audio.
        if let Err(e) = source.seek(resume_at) {
            tracing::warn!(?e, frame = resume_at, "rebuild seek failed; resuming from 0");
            let _ = source.seek(0);
            self.inner.telemetry.set_position_frames(0);
        }
        let (stream, reader_join) = build_running_pipeline(
            &device,
            source,
            source_rate,
            source_channels,
            self.inner.spec.buffer_size,
            Arc::clone(&self.inner.telemetry),
            Arc::clone(&self.inner.signals),
        )
        .map_err(|e| TransportError::BackendFailed(format!("{e}")))?;
        self.inner.stream = Some(stream);
        self.inner.reader_join = Some(reader_join);
        self.inner.paused_via_drop = false;
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

