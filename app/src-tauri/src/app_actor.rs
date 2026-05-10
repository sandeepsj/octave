//! Tauri-side audio actor — owns the `!Send` [`PlaybackHandle`] on a
//! dedicated OS thread.
//!
//! `cpal::Stream` is `!Send` on every backend, so the engine's
//! `PlaybackHandle` cannot live in `tauri::State` (which must be
//! `Send + Sync`). Mirrors `octave-mcp::audio_actor`'s playback half:
//! one OS thread holds `Option<PlaybackHandle>`, async Tauri commands
//! send [`Command`]s through a bounded crossbeam channel and `await`
//! a `tokio::sync::oneshot` reply.
//!
//! Single active session — the engine itself doesn't enforce this for
//! the UI facade (only for MCP), but the v0.1 affordance is one-file
//! play/stop, so the actor matches: `start` while something is already
//! playing returns [`PlaybackStartError::AlreadyPlaying`].

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use tokio::sync::oneshot;

use std::path::PathBuf;
use std::time::Instant;

// One shared catalog for both directions — see plan §3.3.1. Before
// the unification the player and recorder each owned a private
// `DeviceCatalog`, and the player's cached `cpal::Device` for a
// shared device blocked the recorder's input probe (and vice versa)
// because cpal's `DeviceHandles::open` opens both PCMs during
// enumeration.
use octave_audio_devices::DeviceCatalog;
use octave_player::{PlaybackHandle, PlaybackSpec, PlaybackStatus};
use octave_recorder::{RecordedClip, RecordingHandle, RecordingSpec};

/// Bounded so a runaway producer can't grow the queue without limit.
/// 16 is plenty: the UI sends one command per click, the actor
/// processes each in microseconds (the engine call is the slow part,
/// but it's serialised through this single thread anyway).
const COMMAND_QUEUE: usize = 16;

#[derive(Clone)]
pub struct AppActorHandle {
    /// `Option` so `Drop` can `take` and close the channel before
    /// joining the thread — same trick as `octave-mcp::audio_actor`.
    /// Without it the actor's `rx.recv()` would never see the close,
    /// and `handle.join()` would deadlock.
    tx: Option<Sender<Command>>,
    join: Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
    /// Single shared device catalog. Read-only Tauri commands
    /// (`list_output_devices`, `list_input_devices`) and the actor
    /// thread (`playback_start`, `recording_start`) all route through
    /// this Arc — same cache, no two `cpal::Device` wrappers fighting
    /// over the same physical device's PCMs.
    catalog: Arc<DeviceCatalog>,
}

impl AppActorHandle {
    pub fn spawn() -> std::io::Result<Self> {
        let (tx, rx) = bounded::<Command>(COMMAND_QUEUE);
        let catalog = Arc::new(DeviceCatalog::new());
        let actor_catalog = Arc::clone(&catalog);
        let join = thread::Builder::new()
            .name("octave-app-audio".into())
            .spawn(move || run_actor(rx, actor_catalog))?;
        Ok(Self {
            tx: Some(tx),
            join: Arc::new(std::sync::Mutex::new(Some(join))),
            catalog,
        })
    }

    pub fn send(&self, cmd: Command) -> Result<(), ActorError> {
        let tx = self.tx.as_ref().ok_or(ActorError::Gone)?;
        match tx.try_send(cmd) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(ActorError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(ActorError::Gone),
        }
    }

    /// Borrow the actor's shared device catalog for the read-only
    /// `list_output_devices` / `list_input_devices` Tauri commands.
    /// `Send + Sync`, safe to share across concurrent commands.
    pub fn catalog(&self) -> &Arc<DeviceCatalog> {
        &self.catalog
    }
}

impl Drop for AppActorHandle {
    fn drop(&mut self) {
        let _ = self.tx.take();
        if Arc::strong_count(&self.join) > 1 {
            return;
        }
        if let Ok(mut guard) = self.join.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ActorError {
    #[error("audio thread is gone")]
    Gone,
    #[error("audio command queue is full")]
    QueueFull,
}

pub enum Command {
    Start {
        spec: PlaybackSpec,
        reply: oneshot::Sender<Result<PlaybackStartReply, PlaybackStartError>>,
    },
    Pause {
        reply: oneshot::Sender<Result<PlaybackStatus, PlaybackTransportError>>,
    },
    Resume {
        reply: oneshot::Sender<Result<PlaybackStatus, PlaybackTransportError>>,
    },
    Stop {
        reply: oneshot::Sender<Result<PlaybackStatus, PlaybackStopError>>,
    },
    /// Cheap status snapshot — UI polls this while playing to update
    /// the position display and to notice natural EOF (engine
    /// transitions to `Ended` without us asking).
    Status {
        reply: oneshot::Sender<Option<PlaybackStatus>>,
    },

    // ============================================================
    //  Recording
    // ============================================================
    /// Open + arm + record collapsed into one atomic action (rolls
    /// back on failure — same shape as `octave-mcp::audio_actor`'s
    /// `start_session` helper). Returns the output path and started-
    /// at instant; the UI uses both to display "recording → /tmp/…
    /// 0:03". `fold_to_mono` is honoured at Stop time (see
    /// [`RecordingSession::fold_to_mono`]).
    StartRecording {
        spec: RecordingSpec,
        output_path: PathBuf,
        fold_to_mono: bool,
        reply: oneshot::Sender<Result<RecordingStartReply, RecordingStartError>>,
    },
    StopRecording {
        reply: oneshot::Sender<Result<RecordedClip, RecordingStopError>>,
    },
    /// Cheap status snapshot for the UI's elapsed-time tick.
    /// Returns `None` when nothing is recording.
    RecordingStatus {
        reply: oneshot::Sender<Option<RecordingStatusSnapshot>>,
    },
}

#[derive(Debug)]
pub struct PlaybackStartReply {
    pub duration_seconds: Option<f64>,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum PlaybackStartError {
    #[error("StartError::{0}")]
    Start(String),
    #[error("a playback session is already active — stop it first")]
    AlreadyPlaying,
}

#[derive(Debug, thiserror::Error)]
pub enum PlaybackStopError {
    #[error("nothing is currently playing")]
    NotPlaying,
    #[error("StopError::{0}")]
    Stop(String),
}

#[derive(Debug, thiserror::Error)]
pub enum PlaybackTransportError {
    #[error("nothing is currently playing")]
    NotPlaying,
    #[error("TransportError::{0}")]
    Transport(String),
}

#[derive(Debug)]
pub struct RecordingStartReply {
    pub output_path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum RecordingStartError {
    #[error("a recording session is already active — stop it first")]
    AlreadyRecording,
    #[error("OpenError::{0}")]
    Open(String),
    #[error("ArmError::{0}")]
    Arm(String),
    #[error("RecordError::{0}")]
    Record(String),
}

#[derive(Debug, thiserror::Error)]
pub enum RecordingStopError {
    #[error("nothing is currently recording")]
    NotRecording,
    #[error("StopError::{0}")]
    Stop(String),
}

/// What the UI polls while a recording is active. State name +
/// elapsed seconds since the StartRecording command was processed.
/// Engine xrun count surfaced for "recording is glitching" feedback.
#[derive(Debug)]
pub struct RecordingStatusSnapshot {
    pub state: String,
    pub elapsed_seconds: f64,
    pub xrun_count: u32,
}

/// Internal — paired with the live RecordingHandle on the actor thread.
/// `fold_to_mono` flips the post-stop WAV from "L=mic, R=silent" to
/// "L=mic, R=mic" by overwriting each frame's right sample with the
/// left. Captures the user's "I plug one mic into Input 1 of a 2-in
/// interface" case — the most common setup. Engine still records
/// stereo because cpal doesn't expose a 1-channel mode for most
/// devices (e.g. Focusrite Solo reports channels: [2] only).
struct RecordingSession {
    handle: RecordingHandle,
    output_path: PathBuf,
    started_at: Instant,
    fold_to_mono: bool,
}

fn run_actor(rx: Receiver<Command>, catalog: Arc<DeviceCatalog>) {
    let mut active: Option<PlaybackHandle> = None;
    let mut recording: Option<RecordingSession> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::Start { spec, reply } => {
                if active.is_some() {
                    let _ = reply.send(Err(PlaybackStartError::AlreadyPlaying));
                    continue;
                }
                match octave_player::start(&catalog, spec) {
                    Ok(handle) => {
                        let status = handle.status();
                        active = Some(handle);
                        let _ = reply.send(Ok(PlaybackStartReply {
                            duration_seconds: status.duration_seconds,
                            sample_rate: status.sample_rate,
                            channels: status.channels,
                        }));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(PlaybackStartError::Start(format!("{e}"))));
                    }
                }
            }
            Command::Pause { reply } => {
                let result = match active.as_mut() {
                    None => Err(PlaybackTransportError::NotPlaying),
                    Some(handle) => match handle.pause() {
                        Ok(()) => Ok(handle.status()),
                        Err(e) => Err(PlaybackTransportError::Transport(format!("{e}"))),
                    },
                };
                let _ = reply.send(result);
            }
            Command::Resume { reply } => {
                let result = match active.as_mut() {
                    None => Err(PlaybackTransportError::NotPlaying),
                    Some(handle) => match handle.resume() {
                        Ok(()) => Ok(handle.status()),
                        Err(e) => Err(PlaybackTransportError::Transport(format!("{e}"))),
                    },
                };
                let _ = reply.send(result);
            }
            Command::Stop { reply } => {
                let result = match active.take() {
                    None => Err(PlaybackStopError::NotPlaying),
                    Some(mut handle) => match handle.stop() {
                        Ok(status) => {
                            handle.close();
                            Ok(status)
                        }
                        Err(e) => {
                            // close() consumes self, so we must close before
                            // returning even on the error path — otherwise the
                            // PlaybackHandle leaks its reader thread.
                            let msg = format!("{e}");
                            handle.close();
                            Err(PlaybackStopError::Stop(msg))
                        }
                    },
                };
                let _ = reply.send(result);
            }
            Command::Status { reply } => {
                let snapshot = active.as_ref().map(|h| h.status());
                let _ = reply.send(snapshot);
            }

            // ============================================================
            //  Recording
            // ============================================================
            Command::StartRecording {
                spec,
                output_path,
                fold_to_mono,
                reply,
            } => {
                if recording.is_some() {
                    let _ = reply.send(Err(RecordingStartError::AlreadyRecording));
                    continue;
                }
                let result = start_recording(spec, output_path, fold_to_mono, &catalog);
                match result {
                    Ok((session, sample_rate, channels)) => {
                        let path = session.output_path.clone();
                        recording = Some(session);
                        let _ = reply.send(Ok(RecordingStartReply {
                            output_path: path,
                            sample_rate,
                            channels,
                        }));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Command::StopRecording { reply } => {
                let result = match recording.take() {
                    None => Err(RecordingStopError::NotRecording),
                    Some(mut session) => match session.handle.stop() {
                        Ok(mut clip) => {
                            session.handle.close();
                            // Apply mono-fold AFTER the writer has
                            // finalised the WAV (handle.stop() flushes
                            // and closes the writer thread). Fold is a
                            // no-op when channel count != 2 — covers
                            // future devices that expose true 1-ch
                            // capture and where we'd already have a
                            // mono file.
                            if session.fold_to_mono && clip.channels == 2 {
                                if let Err(e) = fold_stereo_to_mono_left(&clip.path) {
                                    eprintln!(
                                        "warn: mono-fold post-process failed for {}: {} \
                                         — clip stays stereo with right channel silent",
                                        clip.path.display(),
                                        e,
                                    );
                                } else {
                                    // Update peak_dbfs's right-channel
                                    // entry to mirror left so the UI
                                    // doesn't show a dead R meter.
                                    if let Some(left) = clip.peak_dbfs.first().copied() {
                                        if let Some(right) = clip.peak_dbfs.get_mut(1) {
                                            *right = left;
                                        }
                                    }
                                }
                            }
                            Ok(clip)
                        }
                        Err(e) => {
                            let msg = format!("{e}");
                            session.handle.close();
                            Err(RecordingStopError::Stop(msg))
                        }
                    },
                };
                let _ = reply.send(result);
            }
            Command::RecordingStatus { reply } => {
                let snapshot = recording.as_ref().map(|s| RecordingStatusSnapshot {
                    state: format!("{:?}", s.handle.state()),
                    elapsed_seconds: s.started_at.elapsed().as_secs_f64(),
                    xrun_count: s.handle.xrun_count(),
                });
                let _ = reply.send(snapshot);
            }
        }
    }
    // Channel closed (last AppActorHandle dropped) — close any active
    // session before we exit.
    if let Some(h) = active.take() {
        h.close();
    }
    if let Some(mut s) = recording.take() {
        // Best-effort cancel; if the writer was mid-flush we accept the
        // interruption rather than blocking the actor's exit on an FS
        // operation that may itself be hanging.
        let _ = s.handle.cancel();
        s.handle.close();
    }
}

/// Open + arm + record on the recorder catalog, rolling back on
/// failure (close the handle on any error so we don't leak the
/// reader thread / cpal stream). Mirror of
/// `octave-mcp::audio_actor::start_session` for the playback-side
/// single-session app actor.
fn start_recording(
    spec: RecordingSpec,
    output_path: PathBuf,
    fold_to_mono: bool,
    catalog: &DeviceCatalog,
) -> Result<(RecordingSession, u32, u16), RecordingStartError> {
    let sample_rate = spec.sample_rate;
    let channels = spec.channels;
    let mut handle = octave_recorder::open(catalog, spec)
        .map_err(|e| RecordingStartError::Open(format!("{e}")))?;
    if let Err(e) = handle.arm() {
        handle.close();
        return Err(RecordingStartError::Arm(format!("{e}")));
    }
    if let Err(e) = handle.record(&output_path) {
        handle.close();
        return Err(RecordingStartError::Record(format!("{e}")));
    }
    Ok((
        RecordingSession {
            handle,
            output_path,
            started_at: Instant::now(),
            fold_to_mono,
        },
        sample_rate,
        channels,
    ))
}

/// Mono-fold a 32-bit-float stereo WAV in-place: copy each frame's
/// left sample into its right slot. Plays correctly through stereo
/// devices on both ears even when only Input 1 of the interface had
/// signal during capture.
///
/// Format assumptions: the file was written by `octave-recorder`
/// (RIFF/RF64, IEEE-float 32-bit, 2 channels, interleaved L/R). We
/// scan for the `data` chunk header rather than hard-coding a header
/// length so format-tweaks in hound don't break this.
fn fold_stereo_to_mono_left(path: &std::path::Path) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    let mut bytes = std::fs::read(path)?;
    let Some(idx) = bytes.windows(4).position(|w| w == b"data") else {
        return Err(Error::new(ErrorKind::InvalidData, "no data chunk"));
    };
    // After "data" + 4-byte size = data chunk payload.
    let data_start = idx + 8;
    let frame_bytes = 8usize; // 2 channels × 4 bytes (f32)
    let mut i = data_start;
    while i + frame_bytes <= bytes.len() {
        // copy_within is safe for non-overlapping slices in the same
        // Vec; bytes[i..i+4] (left) → bytes[i+4..i+8] (right).
        bytes.copy_within(i..i + 4, i + 4);
        i += frame_bytes;
    }
    std::fs::write(path, bytes)
}
