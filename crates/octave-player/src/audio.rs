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
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, StreamTrait};
use thiserror::Error;
use uuid::Uuid;

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

/// Errors returned by [`start`].
#[derive(Debug, Error)]
pub enum StartError {
    #[error("device not found: {0:?}")]
    DeviceNotFound(DeviceId),
    #[error("backend error: {0}")]
    BackendError(String),
    #[error("source unreadable: {0}")]
    SourceUnreadable(String),
    #[error("device does not support {requested} Hz; supported: {supported:?}")]
    RateUnsupportedByDevice { requested: u32, supported: Vec<u32> },
    #[error("device does not support {requested} channels; supported: {supported:?}")]
    ChannelsUnsupportedByDevice { requested: u16, supported: Vec<u16> },
    #[error("buffer size {requested} out of range [{min}, {max}]")]
    BufferSizeOutOfRange { requested: u32, min: u32, max: u32 },
    #[error("device exposes no f32 stream config matching {sample_rate} Hz / {channels} ch")]
    NoMatchingStreamConfig { sample_rate: u32, channels: u16 },
    #[error("failed to build cpal output stream: {0}")]
    BuildStreamFailed(String),
    #[error("failed to start cpal output stream: {0}")]
    PlayStreamFailed(String),
    #[error("a playback session is already active: {current_session}")]
    AlreadyPlaying { current_session: Uuid },
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
    /// Session UUID issued by `start`. Held by the session-lock entry
    /// (see `acquire_session`) and released by `close` / `Drop`. Used
    /// in `StartError::AlreadyPlaying { current_session }` when a
    /// second concurrent `start` is attempted.
    session_id: Uuid,
    sample_rate: u32,
    channels: u16,
    duration_frames: Option<u64>,
    telemetry: Arc<Telemetry>,
    signals: Arc<TransportSignals>,
    /// `!Send` — keep the handle on the thread that built the stream.
    stream: Option<cpal::Stream>,
    reader_join: Option<JoinHandle<()>>,
    /// Saved at `start()` so [`PlaybackHandle::pause`]'s
    /// verify-and-rebuild fallback (plan §5.7) can re-construct the
    /// pipeline on backends where `cpal::Stream::pause` silently
    /// no-ops (PipeWire ALSA bridge in particular).
    spec: PlaybackSpec,
    /// `true` once we've observed pause silently failing — `pause`
    /// dropped the stream + reader and `resume` must rebuild rather
    /// than calling `stream.play`.
    paused_via_drop: bool,
}

/// Process-global single-session enforcer. Plan §3.7 / §13.3 require
/// at most one active playback session at a time, with a check at
/// both the engine and actor layers (defence in depth). The actor
/// layer's check lives in `octave-mcp::audio_actor`; this is the
/// engine-layer counterpart.
fn session_lock() -> &'static Mutex<Option<Uuid>> {
    static LOCK: OnceLock<Mutex<Option<Uuid>>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(None))
}

/// Try to claim the global playback slot. Returns the new session UUID
/// on success, or `AlreadyPlaying { current_session }` when another
/// session already owns the slot.
fn acquire_session() -> Result<Uuid, StartError> {
    let mut guard = session_lock()
        .lock()
        .expect("playback session mutex poisoned");
    if let Some(current) = *guard {
        return Err(StartError::AlreadyPlaying {
            current_session: current,
        });
    }
    let id = Uuid::new_v4();
    *guard = Some(id);
    Ok(id)
}

/// Release the global playback slot — called by `close` and by `Drop`
/// for safety. Idempotent: releasing an already-empty slot is a no-op
/// (covers Drop-after-close).
fn release_session(expected: Uuid) {
    let mut guard = session_lock()
        .lock()
        .expect("playback session mutex poisoned");
    if let Some(current) = *guard {
        if current == expected {
            *guard = None;
        } else {
            // Different session somehow — don't clobber. Log and bail.
            tracing::warn!(
                expected = %expected,
                current = %current,
                "release_session: slot owned by a different session; skipping release"
            );
        }
    }
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
///
/// Plan §3.7 / §13.3 single-session enforcement: a process-global
/// slot is claimed at the top of this call and released by
/// [`PlaybackHandle::close`] (or its `Drop`). A second concurrent
/// caller gets [`StartError::AlreadyPlaying { current_session }`].
pub fn start(spec: PlaybackSpec) -> Result<PlaybackHandle, StartError> {
    // Claim the slot before spending any I/O / cpal cycles. Released
    // below on every error path and by close()/Drop on success.
    let session_id = acquire_session()?;

    // From here on, any early return must release the slot.
    let result = (|| -> Result<PlaybackHandle, StartError> {
        let device = find_device(&spec.device_id)?;
        let ConstructedSource {
            source,
            sample_rate: source_rate,
            channels: source_channels,
            duration_frames,
        } = construct_source(&spec.source)?;

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
                session_id,
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
    })();

    if result.is_err() {
        release_session(session_id);
    }
    result
}

/// Constructed-source bundle returned by [`construct_source`].
/// Replaces the previous 4-tuple to keep clippy's `type_complexity`
/// happy and to give the fields names at every call site.
pub(crate) struct ConstructedSource {
    pub source: Box<dyn PlaybackSource>,
    pub sample_rate: u32,
    pub channels: u16,
    pub duration_frames: Option<u64>,
}

/// Construct the boxed PlaybackSource impl from the user-facing spec.
fn construct_source(source_spec: &PlaybackSourceSpec) -> Result<ConstructedSource, StartError> {
    Ok(match source_spec {
        PlaybackSourceSpec::File { path } => {
            let fs = FileSource::open(path)?;
            let sample_rate = fs.sample_rate();
            let channels = fs.channels();
            let duration_frames = fs.duration_frames();
            ConstructedSource {
                source: Box::new(fs),
                sample_rate,
                channels,
                duration_frames,
            }
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
            let duration_frames = bs.duration_frames();
            ConstructedSource {
                source: Box::new(bs),
                sample_rate: *sample_rate,
                channels: *channels,
                duration_frames,
            }
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
        .ok_or(StartError::NoMatchingStreamConfig {
            sample_rate,
            channels,
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
        // Reset the per-take running peak so the meter doesn't carry
        // a clip from before the seek point into the new region —
        // plan §5.8 / glossary describe a fresh per-region peak.
        // Non-RT thread; allocation-safe.
        self.inner.telemetry.reset_running_peaks();
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
        let ConstructedSource {
            mut source,
            sample_rate: source_rate,
            channels: source_channels,
            duration_frames: _,
        } = construct_source(&self.inner.spec.source)
            .map_err(|e| TransportError::BackendFailed(format!("{e}")))?;
        let resume_at = self.inner.telemetry.position_frames.load(Ordering::Acquire);
        // Seek the source to where we stopped hearing audio. If the
        // source can't seek that far (e.g., file truncated since the
        // pause), fall back to the start. If even seek(0) fails, the
        // source is in an unusable state — return BackendFailed
        // rather than silently playing from an unknown offset.
        if let Err(e) = source.seek(resume_at) {
            tracing::warn!(?e, frame = resume_at, "rebuild seek failed; resuming from 0");
            if let Err(zero_err) = source.seek(0) {
                return Err(TransportError::BackendFailed(format!(
                    "rebuild seek({resume_at}) failed and fallback seek(0) also failed: {zero_err}"
                )));
            }
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

    /// Per-channel running peak (max abs since the last seek or take
    /// start) in dBFS. The recorder's analogue is `RecordedClip.peak_dbfs`;
    /// for playback the take-peak surfaces live so the UI can show
    /// "loudest sample heard so far". Reset by `seek`.
    pub fn peak_take_dbfs(&self, channel: u16) -> f32 {
        if channel >= self.inner.channels {
            return f32::NEG_INFINITY;
        }
        linear_to_dbfs(self.inner.telemetry.running_peak_value(channel))
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
        // u64 frame index → f64 seconds: lossy in the abstract (u64 is
        // 64-bit, f64 mantissa is 52-bit), exact for any real session
        // since 2^52 frames @ 192 kHz is ≈ 743 years of audio.
        #[allow(clippy::cast_precision_loss)]
        let pos_secs = pos as f64 / f64::from(self.inner.sample_rate);
        #[allow(clippy::cast_precision_loss)]
        let dur_secs = self
            .inner
            .duration_frames
            .map(|d| d as f64 / f64::from(self.inner.sample_rate));
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
        // Use the lazy state() so a session that finished naturally
        // (playback_complete is set, inner.state still Playing) is
        // recognised as Ended and skips the stop() call. Otherwise
        // close() would re-invoke stop()'s pause + drain on a
        // finished stream.
        let observed = self.state();
        if !matches!(
            observed,
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
        // Release the global session slot so the next start() succeeds.
        release_session(self.inner.session_id);
    }

    /// Engine-issued session UUID. Same value the
    /// [`StartError::AlreadyPlaying`] reports when a second
    /// concurrent caller hits the slot. Surfaced for the MCP layer
    /// (which currently mints its own; future refactor consolidates).
    pub fn session_id(&self) -> Uuid {
        self.inner.session_id
    }
}

impl Drop for PlaybackHandle {
    fn drop(&mut self) {
        // Safety net for callers that forget close(): release the
        // session slot. close() also releases — release_session is
        // idempotent (the second call sees an empty slot and no-ops).
        release_session(self.inner.session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only constructor — build a `PlaybackHandle` in an
    /// arbitrary state without actually opening a cpal device. Used
    /// to exercise the read-only / state-error paths that don't
    /// touch the audio pipeline.
    fn handle_for_test(state: PlaybackState, channels: u16, sample_rate: u32) -> PlaybackHandle {
        // Don't go through acquire_session (we'd contend with parallel
        // tests); mint a UUID directly. The Drop impl will call
        // release_session, which idempotently no-ops on an empty slot.
        let signals = Arc::new(TransportSignals::new());
        let telemetry = Arc::new(Telemetry::new(channels));
        PlaybackHandle {
            inner: Internals {
                state,
                session_id: Uuid::new_v4(),
                sample_rate,
                channels,
                duration_frames: Some(48_000),
                telemetry,
                signals,
                stream: None,
                reader_join: None,
                spec: PlaybackSpec {
                    device_id: DeviceId("ALSA:test".into()),
                    source: PlaybackSourceSpec::Buffer {
                        samples: Arc::from(vec![0.0_f32; (channels as usize) * 4]),
                        sample_rate,
                        channels,
                    },
                    buffer_size: BufferSize::Default,
                },
                paused_via_drop: false,
            },
        }
    }

    // ---------- linear_to_dbfs ----------

    #[test]
    fn linear_to_dbfs_floor_clamps_zero_and_subnormal_to_minus_180() {
        assert_eq!(linear_to_dbfs(0.0), FLOOR_DBFS);
        assert_eq!(linear_to_dbfs(FLOOR_LINEAR / 2.0), FLOOR_DBFS);
    }

    #[test]
    fn linear_to_dbfs_unity_is_zero() {
        assert!((linear_to_dbfs(1.0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn linear_to_dbfs_half_is_minus_six() {
        // 20 * log10(0.5) ≈ -6.0206
        assert!((linear_to_dbfs(0.5) - (-6.0)).abs() < 0.05);
    }

    // ---------- per-channel level accessors ----------

    #[test]
    fn peak_dbfs_out_of_range_channel_returns_neg_infinity() {
        let h = handle_for_test(PlaybackState::Playing, 2, 48_000);
        assert_eq!(h.peak_dbfs(0), FLOOR_DBFS); // valid channel, no audio
        assert_eq!(h.peak_dbfs(1), FLOOR_DBFS);
        assert_eq!(h.peak_dbfs(2), f32::NEG_INFINITY); // ch >= channels
        assert_eq!(h.peak_dbfs(99), f32::NEG_INFINITY);
    }

    #[test]
    fn rms_dbfs_out_of_range_channel_returns_neg_infinity() {
        let h = handle_for_test(PlaybackState::Playing, 2, 48_000);
        assert_eq!(h.rms_dbfs(2), f32::NEG_INFINITY);
    }

    #[test]
    fn peak_take_dbfs_out_of_range_channel_returns_neg_infinity() {
        let h = handle_for_test(PlaybackState::Playing, 1, 48_000);
        assert_eq!(h.peak_take_dbfs(1), f32::NEG_INFINITY);
    }

    // ---------- lazy Playing → Ended transition ----------

    #[test]
    fn state_lazily_reports_ended_when_playback_complete_is_set() {
        let h = handle_for_test(PlaybackState::Playing, 2, 48_000);
        assert_eq!(h.state(), PlaybackState::Playing);
        // Audio thread (simulated) signals EOF + drained.
        h.inner
            .signals
            .playback_complete
            .store(true, Ordering::Release);
        assert_eq!(h.state(), PlaybackState::Ended);
    }

    #[test]
    fn state_does_not_promote_to_ended_from_paused() {
        // playback_complete should only flip Playing → Ended; from
        // Paused (or any other state) the inner state wins.
        let h = handle_for_test(PlaybackState::Paused, 2, 48_000);
        h.inner
            .signals
            .playback_complete
            .store(true, Ordering::Release);
        assert_eq!(h.state(), PlaybackState::Paused);
    }

    #[test]
    fn status_carries_lazy_ended_transition() {
        let h = handle_for_test(PlaybackState::Playing, 2, 48_000);
        h.inner
            .signals
            .playback_complete
            .store(true, Ordering::Release);
        let s = h.status();
        assert_eq!(s.state, PlaybackState::Ended);
    }

    // ---------- seek state-error paths ----------

    #[test]
    fn seek_returns_not_seekable_in_terminal_states() {
        for bad_state in [
            PlaybackState::Idle,
            PlaybackState::Stopped,
            PlaybackState::Errored,
            PlaybackState::Closed,
            PlaybackState::Loading,
        ] {
            let mut h = handle_for_test(bad_state, 2, 48_000);
            let Err(SeekError::NotSeekable { current }) = h.seek(0) else {
                panic!("expected NotSeekable for state {bad_state:?}");
            };
            assert_eq!(current, bad_state);
        }
    }

    #[test]
    fn seek_returns_out_of_bounds_when_frame_exceeds_duration() {
        // handle_for_test sets duration_frames = Some(48_000).
        let mut h = handle_for_test(PlaybackState::Playing, 2, 48_000);
        let Err(SeekError::OutOfBounds { requested, max }) = h.seek(48_001) else {
            panic!("expected OutOfBounds for frame > duration");
        };
        assert_eq!(requested, 48_001);
        assert_eq!(max, 48_000);
    }

    #[test]
    fn seek_to_exact_duration_is_allowed() {
        let mut h = handle_for_test(PlaybackState::Playing, 2, 48_000);
        // duration is 48_000; seek to 48_000 (one-past-end → EOF) is OK.
        h.seek(48_000).expect("seek to duration is allowed");
    }

    #[test]
    fn seek_resets_running_peaks() {
        let h = handle_for_test(PlaybackState::Playing, 1, 48_000);
        // Pre-load a running peak as if the audio thread had recorded it.
        h.inner.telemetry.running_peak[0].store(0.5_f32.to_bits(), Ordering::Relaxed);
        assert!((h.inner.telemetry.running_peak_value(0) - 0.5).abs() < 1e-6);

        let mut h = h;
        h.seek(0).expect("valid seek");
        assert_eq!(h.inner.telemetry.running_peak_value(0), 0.0);
    }

    // ---------- close() idempotency / lazy state ----------

    #[test]
    fn close_after_natural_end_does_not_panic() {
        // Ensures close() takes the lazy state() path (Ended) rather
        // than calling stop() on a finished stream. We can't directly
        // observe whether stop ran without a real cpal device, but we
        // verify it doesn't panic and post-state is Closed semantics
        // (handle is consumed, no double-release on the global slot).
        // We deliberately do NOT touch acquire_session here so the
        // test doesn't race the session_lock test.
        let h = handle_for_test(PlaybackState::Playing, 2, 48_000);
        h.inner
            .signals
            .playback_complete
            .store(true, Ordering::Release);
        h.close();
    }

    // ---------- session lock semantics ----------
    //
    // The session lock is process-global, so tests that touch it
    // can race under cargo's default parallel runner. Merged into
    // one sequential test (with a serializing Mutex inside) so the
    // assertions stay deterministic without `--test-threads=1` or
    // adding the serial_test dep.
    #[test]
    fn session_lock_semantics_acquire_block_release_reuse() {
        // Serialize against any other accidental session_lock test.
        use std::sync::Mutex;
        static SERIALIZE: Mutex<()> = Mutex::new(());
        let _guard = SERIALIZE.lock().unwrap_or_else(|p| p.into_inner());

        // 1. First acquire wins.
        let first = acquire_session().expect("first acquire wins");

        // 2. Second acquire blocks with AlreadyPlaying { current_session: first }.
        let err = acquire_session().expect_err("second acquire blocks");
        match err {
            StartError::AlreadyPlaying { current_session } => {
                assert_eq!(current_session, first);
            }
            other => panic!("expected AlreadyPlaying, got {other:?}"),
        }

        // 3. Imposter release does not clobber the slot.
        let imposter = Uuid::new_v4();
        release_session(imposter); // warn + skip
        let still_blocked = acquire_session().expect_err("slot still owned by `first`");
        assert!(matches!(still_blocked, StartError::AlreadyPlaying { .. }));

        // 4. Owner release frees the slot.
        release_session(first);

        // 5. Next acquire succeeds.
        let second = acquire_session().expect("slot free after owner release");
        release_session(second);
    }
}

