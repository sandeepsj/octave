//! The audio-management thread — owns every active `RecordingHandle`.
//!
//! `cpal::Stream` is `!Send` on every backend, so `RecordingHandle`
//! cannot live behind `Arc<Mutex<>>` shared with tokio tasks. We adopt
//! the actor pattern: one OS thread holds the
//! `HashMap<Uuid, RecordingHandle>`; async tool tasks send [`Command`]s
//! through a bounded `crossbeam_channel` and `await` a `tokio::sync::oneshot`
//! reply.
//!
//! See [`docs/modules/mcp-layer.md`](../../../../docs/modules/mcp-layer.md)
//! §5.1 for the design and §7 for the threading model.

use std::collections::HashMap;
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::SystemTime;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use tokio::sync::oneshot;
use uuid::Uuid;

use octave_recorder::{
    ArmError, BufferSize, CancelError, DeviceId, OpenError, RecordError, RecordingHandle,
    RecordingSpec, StopError,
};

use octave_player::{
    self as player, PlaybackHandle, PlaybackSpec,
};

use crate::types::{
    LevelsResult, PlaybackSeekResult, PlaybackStatusJson, PlaybackTransportResult,
    RecordedClipJson, RecorderStateJson, StatusResult,
};

const COMMAND_QUEUE: usize = 64;
const MAX_SESSIONS: usize = 8;

/// Handle held by tool methods. Cheap to clone (`Sender` is `Arc` inside).
#[derive(Clone)]
pub struct AudioActorHandle {
    tx: Sender<Command>,
    _join: std::sync::Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
}

impl AudioActorHandle {
    pub fn spawn() -> std::io::Result<Self> {
        let (tx, rx) = bounded::<Command>(COMMAND_QUEUE);
        let join = thread::Builder::new()
            .name("octave-mcp-audio".into())
            .spawn(move || run_actor(rx))?;
        Ok(Self {
            tx,
            _join: std::sync::Arc::new(std::sync::Mutex::new(Some(join))),
        })
    }

    /// Send a command; non-blocking. Errors if the channel is full or
    /// the actor thread has died.
    pub fn send(&self, cmd: Command) -> Result<(), ActorError> {
        match self.tx.try_send(cmd) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(ActorError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(ActorError::Gone),
        }
    }
}

/// Reasons the actor channel can fail.
#[derive(Debug, thiserror::Error)]
pub enum ActorError {
    #[error("audio-management thread is gone")]
    Gone,
    #[error("audio-command queue is full")]
    QueueFull,
    #[error("audio-management thread dropped the reply channel")]
    ReplyLost,
}

/// Commands the actor accepts. Each carries a oneshot to reply on.
pub enum Command {
    // ---------- recording ----------
    /// Open + Arm + Record collapsed into one atomic step (rolls back on failure).
    StartRecording {
        spec: RecordingSpec,
        output_path: PathBuf,
        reply: oneshot::Sender<Result<StartReply, StartReplyError>>,
    },
    Stop {
        session_id: Uuid,
        reply: oneshot::Sender<Result<RecordedClipJson, SessionError>>,
    },
    Cancel {
        session_id: Uuid,
        reply: oneshot::Sender<Result<(PathBuf, bool), SessionError>>,
    },
    GetLevels {
        session_id: Uuid,
        reply: oneshot::Sender<Result<LevelsResult, SessionError>>,
    },
    GetStatus {
        session_id: Uuid,
        reply: oneshot::Sender<Result<StatusResult, SessionError>>,
    },

    // ---------- playback ----------
    PlaybackStart {
        spec: PlaybackSpec,
        reply: oneshot::Sender<Result<PlaybackStartReply, PlaybackStartError>>,
    },
    PlaybackPause {
        session_id: Uuid,
        reply: oneshot::Sender<Result<PlaybackTransportResult, PlaybackSessionError>>,
    },
    PlaybackResume {
        session_id: Uuid,
        reply: oneshot::Sender<Result<PlaybackTransportResult, PlaybackSessionError>>,
    },
    PlaybackStop {
        session_id: Uuid,
        reply: oneshot::Sender<Result<PlaybackStatusJson, PlaybackSessionError>>,
    },
    PlaybackSeek {
        session_id: Uuid,
        target_frames: u64,
        reply: oneshot::Sender<Result<PlaybackSeekResult, PlaybackSessionError>>,
    },
    PlaybackGetStatus {
        session_id: Uuid,
        reply: oneshot::Sender<Result<PlaybackStatusJson, PlaybackSessionError>>,
    },
    PlaybackGetLevels {
        session_id: Uuid,
        reply: oneshot::Sender<Result<LevelsResult, PlaybackSessionError>>,
    },
}

#[derive(Debug)]
pub struct StartReply {
    pub session_id: Uuid,
    pub started_at: SystemTime,
}

#[derive(Debug, thiserror::Error)]
pub enum StartReplyError {
    #[error("OpenError::{0}")]
    Open(String),
    #[error("ArmError::{0}")]
    Arm(String),
    #[error("RecordError::{0}")]
    Record(String),
    #[error("session limit reached ({MAX_SESSIONS})")]
    TooManySessions,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session_not_found: {0}")]
    NotFound(Uuid),
    #[error("StopError::{0}")]
    Stop(String),
    #[error("CancelError::{0}")]
    Cancel(String),
}

#[derive(Debug)]
pub struct PlaybackStartReply {
    pub session_id: Uuid,
    pub started_at: SystemTime,
    pub duration_seconds: Option<f64>,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum PlaybackStartError {
    #[error("StartError::{0}")]
    Start(String),
    #[error("a playback session is already active: {current_session}")]
    AlreadyPlaying { current_session: Uuid },
}

#[derive(Debug, thiserror::Error)]
pub enum PlaybackSessionError {
    #[error("session_not_found: {0}")]
    NotFound(Uuid),
    #[error("transport: {0}")]
    Transport(String),
    #[error("seek: {0}")]
    Seek(String),
}

struct Session {
    handle: RecordingHandle,
    started_at: SystemTime,
    output_path: PathBuf,
    #[allow(dead_code)] // recorded for future per-session telemetry
    sample_rate: u32,
    channels: u16,
}

struct PlaybackSession {
    handle: PlaybackHandle,
    session_id: Uuid,
    #[allow(dead_code)] // future per-session telemetry
    started_at: SystemTime,
}

#[derive(Default)]
struct ActorState {
    recording: HashMap<Uuid, Session>,
    playback: Option<PlaybackSession>,
}

fn run_actor(rx: Receiver<Command>) {
    let mut state = ActorState::default();
    while let Ok(cmd) = rx.recv() {
        handle_command(cmd, &mut state);
    }
    // Channel closed → drain and close everything.
    for (_, mut s) in state.recording.drain() {
        let _ = s.handle.cancel();
        s.handle.close();
    }
    if let Some(ps) = state.playback.take() {
        ps.handle.close();
    }
}

#[allow(clippy::cast_precision_loss)]
fn handle_command(cmd: Command, state: &mut ActorState) {
    match cmd {
        Command::StartRecording {
            spec,
            output_path,
            reply,
        } => {
            let _ = reply.send(start_session(spec, &output_path, &mut state.recording));
        }
        Command::Stop { session_id, reply } => {
            let result = match state.recording.remove(&session_id) {
                None => Err(SessionError::NotFound(session_id)),
                Some(mut s) => match s.handle.stop() {
                    Ok(clip) => {
                        s.handle.close();
                        Ok(clip_to_json(clip, s.started_at))
                    }
                    Err(StopError::NotRecording { current }) => {
                        state.recording.insert(session_id, s);
                        Err(SessionError::Stop(format!("NotRecording {{ {current:?} }}")))
                    }
                    Err(StopError::FinalizeFailed(msg)) => {
                        s.handle.close();
                        Err(SessionError::Stop(format!("FinalizeFailed({msg})")))
                    }
                },
            };
            let _ = reply.send(result);
        }
        Command::Cancel { session_id, reply } => {
            let result = match state.recording.remove(&session_id) {
                None => Err(SessionError::NotFound(session_id)),
                Some(mut s) => match s.handle.cancel() {
                    Ok(()) => {
                        s.handle.close();
                        let path = s.output_path.clone();
                        let deleted = !path.exists();
                        Ok((path, deleted))
                    }
                    Err(CancelError::NotRecording { current }) => {
                        state.recording.insert(session_id, s);
                        Err(SessionError::Cancel(format!("NotRecording {{ {current:?} }}")))
                    }
                },
            };
            let _ = reply.send(result);
        }
        Command::GetLevels { session_id, reply } => {
            let result = match state.recording.get(&session_id) {
                None => Err(SessionError::NotFound(session_id)),
                Some(s) => {
                    let mut peak = Vec::with_capacity(s.channels as usize);
                    let mut rms = Vec::with_capacity(s.channels as usize);
                    for c in 0..s.channels {
                        peak.push(s.handle.peak_dbfs(c));
                        rms.push(s.handle.rms_dbfs(c));
                    }
                    Ok(LevelsResult {
                        peak_dbfs: peak,
                        rms_dbfs: rms,
                    })
                }
            };
            let _ = reply.send(result);
        }
        Command::GetStatus { session_id, reply } => {
            let result = match state.recording.get(&session_id) {
                None => Err(SessionError::NotFound(session_id)),
                Some(s) => {
                    let elapsed_seconds = SystemTime::now()
                        .duration_since(s.started_at)
                        .map(|d| d.as_secs_f64())
                        .unwrap_or(0.0);
                    Ok(StatusResult {
                        state: RecorderStateJson::from(s.handle.state()),
                        xrun_count: s.handle.xrun_count(),
                        dropped_samples: s.handle.dropped_samples(),
                        elapsed_seconds,
                    })
                }
            };
            let _ = reply.send(result);
        }

        // ============================================================
        //   Playback handlers
        // ============================================================
        Command::PlaybackStart { spec, reply } => {
            let result = start_playback(spec, &mut state.playback);
            let _ = reply.send(result);
        }
        Command::PlaybackPause { session_id, reply } => {
            let result = with_playback_session(&mut state.playback, session_id, |ps| {
                ps.handle.pause().map_err(|e| {
                    PlaybackSessionError::Transport(format!("{e}"))
                })?;
                Ok(playback_transport(ps))
            });
            let _ = reply.send(result);
        }
        Command::PlaybackResume { session_id, reply } => {
            let result = with_playback_session(&mut state.playback, session_id, |ps| {
                ps.handle.resume().map_err(|e| {
                    PlaybackSessionError::Transport(format!("{e}"))
                })?;
                Ok(playback_transport(ps))
            });
            let _ = reply.send(result);
        }
        Command::PlaybackStop { session_id, reply } => {
            // Take the session: stop is terminal.
            let result = match state.playback.take() {
                None => Err(PlaybackSessionError::NotFound(session_id)),
                Some(ps) if ps.session_id != session_id => {
                    let other_id = ps.session_id;
                    state.playback = Some(ps);
                    Err(PlaybackSessionError::NotFound(other_id))
                }
                Some(mut ps) => match ps.handle.stop() {
                    Ok(status) => {
                        ps.handle.close();
                        Ok(PlaybackStatusJson::from(status))
                    }
                    Err(e) => {
                        let msg = format!("{e}");
                        ps.handle.close();
                        Err(PlaybackSessionError::Transport(msg))
                    }
                },
            };
            let _ = reply.send(result);
        }
        Command::PlaybackSeek {
            session_id,
            target_frames,
            reply,
        } => {
            let result = with_playback_session(&mut state.playback, session_id, |ps| {
                ps.handle.seek(target_frames).map_err(|e| {
                    PlaybackSessionError::Seek(format!("{e}"))
                })?;
                // Return the requested target rather than a fresh
                // status() snapshot — seek is async (the audio thread
                // hasn't acked the flush yet), so the snapshot would
                // still reflect the pre-seek position. Agents expect
                // "you asked for frame N, you're at frame N now."
                let sr = f64::from(ps.handle.sample_rate());
                #[allow(clippy::cast_precision_loss)]
                let secs = target_frames as f64 / sr;
                Ok(PlaybackSeekResult {
                    position_frames: target_frames,
                    position_seconds: secs,
                })
            });
            let _ = reply.send(result);
        }
        Command::PlaybackGetStatus { session_id, reply } => {
            let result = with_playback_session(&mut state.playback, session_id, |ps| {
                Ok(PlaybackStatusJson::from(ps.handle.status()))
            });
            let _ = reply.send(result);
        }
        Command::PlaybackGetLevels { session_id, reply } => {
            let result = with_playback_session(&mut state.playback, session_id, |ps| {
                let lv = ps.handle.levels();
                Ok(LevelsResult {
                    peak_dbfs: lv.peak_dbfs,
                    rms_dbfs: lv.rms_dbfs,
                })
            });
            let _ = reply.send(result);
        }
    }
}

fn with_playback_session<R>(
    playback: &mut Option<PlaybackSession>,
    session_id: Uuid,
    f: impl FnOnce(&mut PlaybackSession) -> Result<R, PlaybackSessionError>,
) -> Result<R, PlaybackSessionError> {
    match playback.as_mut() {
        None => Err(PlaybackSessionError::NotFound(session_id)),
        Some(ps) if ps.session_id != session_id => {
            Err(PlaybackSessionError::NotFound(session_id))
        }
        Some(ps) => f(ps),
    }
}

fn playback_transport(ps: &PlaybackSession) -> PlaybackTransportResult {
    let st = ps.handle.status();
    PlaybackTransportResult {
        state: PlaybackStatusJson::from(st.clone()).state,
        position_seconds: st.position_seconds,
        position_frames: st.position_frames,
    }
}

fn start_playback(
    spec: PlaybackSpec,
    playback: &mut Option<PlaybackSession>,
) -> Result<PlaybackStartReply, PlaybackStartError> {
    if let Some(ps) = playback.as_ref() {
        return Err(PlaybackStartError::AlreadyPlaying {
            current_session: ps.session_id,
        });
    }
    let handle = player::open(spec).map_err(|e| PlaybackStartError::Start(format!("{e}")))?;
    let session_id = Uuid::new_v4();
    let started_at = SystemTime::now();
    let status = handle.status();
    let reply = PlaybackStartReply {
        session_id,
        started_at,
        duration_seconds: status.duration_seconds,
        sample_rate: status.sample_rate,
        channels: status.channels,
    };
    *playback = Some(PlaybackSession {
        handle,
        session_id,
        started_at,
    });
    Ok(reply)
}

fn start_session(
    spec: RecordingSpec,
    output_path: &std::path::Path,
    sessions: &mut HashMap<Uuid, Session>,
) -> Result<StartReply, StartReplyError> {
    if sessions.len() >= MAX_SESSIONS {
        return Err(StartReplyError::TooManySessions);
    }
    let mut handle =
        octave_recorder::open(spec.clone()).map_err(|e| StartReplyError::Open(open_err_str(&e)))?;
    if let Err(e) = handle.arm() {
        handle.close();
        return Err(StartReplyError::Arm(arm_err_str(&e)));
    }
    if let Err(e) = handle.record(output_path) {
        // Recorder doesn't expose disarm; close discards the stream.
        handle.close();
        return Err(StartReplyError::Record(record_err_str(&e)));
    }
    let id = Uuid::new_v4();
    let started_at = SystemTime::now();
    sessions.insert(
        id,
        Session {
            handle,
            started_at,
            output_path: output_path.to_path_buf(),
            sample_rate: spec.sample_rate,
            channels: spec.channels,
        },
    );
    Ok(StartReply {
        session_id: id,
        started_at,
    })
}

fn clip_to_json(clip: octave_recorder::RecordedClip, started_at: SystemTime) -> RecordedClipJson {
    RecordedClipJson {
        path: clip.path,
        clip_uuid: clip.uuid.to_string(),
        sample_rate: clip.sample_rate,
        channels: clip.channels,
        frame_count: clip.frame_count,
        duration_seconds: clip.duration_seconds,
        started_at_unix_seconds: started_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        xrun_count: clip.xrun_count,
        dropped_samples: clip.dropped_samples,
        peak_dbfs: clip.peak_dbfs,
    }
}

fn open_err_str(e: &OpenError) -> String {
    match e {
        OpenError::DeviceNotFound { id: DeviceId(s) } => format!("DeviceNotFound({s})"),
        OpenError::FormatUnsupported { .. } => "FormatUnsupported".into(),
        OpenError::BackendError(m) => format!("BackendError({m})"),
        OpenError::PermissionDenied => "PermissionDenied".into(),
    }
}
fn arm_err_str(e: &ArmError) -> String {
    match e {
        ArmError::NotIdle { current } => format!("NotIdle {{ {current:?} }}"),
        ArmError::BuildStreamFailed(m) => format!("BuildStreamFailed({m})"),
    }
}
fn record_err_str(e: &RecordError) -> String {
    match e {
        RecordError::NotArmed { current } => format!("NotArmed {{ {current:?} }}"),
        RecordError::OutputPathInvalid(p) => format!("OutputPathInvalid({})", p.display()),
        RecordError::PermissionDenied(p) => format!("PermissionDenied({})", p.display()),
        RecordError::DiskFull => "DiskFull".into(),
    }
}

/// Convenience: build a [`RecordingSpec`] from agent-flat args.
pub(crate) fn spec_from_args(
    device_id: String,
    sample_rate: u32,
    buffer_size: BufferSize,
    channels: u16,
) -> RecordingSpec {
    RecordingSpec {
        device_id: DeviceId(device_id),
        sample_rate,
        buffer_size,
        channels,
    }
}
