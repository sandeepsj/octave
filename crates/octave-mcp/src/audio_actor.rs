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

use crate::types::{LevelsResult, RecordedClipJson, RecorderStateJson, StatusResult};

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

struct Session {
    handle: RecordingHandle,
    started_at: SystemTime,
    output_path: PathBuf,
    #[allow(dead_code)] // recorded for future per-session telemetry
    sample_rate: u32,
    channels: u16,
}

fn run_actor(rx: Receiver<Command>) {
    let mut sessions: HashMap<Uuid, Session> = HashMap::new();
    while let Ok(cmd) = rx.recv() {
        handle_command(cmd, &mut sessions);
    }
    // Channel closed → drain and close everything.
    for (_, mut s) in sessions.drain() {
        let _ = s.handle.cancel();
        s.handle.close();
    }
}

#[allow(clippy::cast_precision_loss)]
fn handle_command(cmd: Command, sessions: &mut HashMap<Uuid, Session>) {
    match cmd {
        Command::StartRecording {
            spec,
            output_path,
            reply,
        } => {
            let _ = reply.send(start_session(spec, &output_path, sessions));
        }
        Command::Stop { session_id, reply } => {
            let result = match sessions.remove(&session_id) {
                None => Err(SessionError::NotFound(session_id)),
                Some(mut s) => match s.handle.stop() {
                    Ok(clip) => {
                        s.handle.close();
                        Ok(clip_to_json(clip, s.started_at))
                    }
                    Err(StopError::NotRecording { current }) => {
                        // put it back so the agent can inspect / cancel
                        sessions.insert(session_id, s);
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
            let result = match sessions.remove(&session_id) {
                None => Err(SessionError::NotFound(session_id)),
                Some(mut s) => match s.handle.cancel() {
                    Ok(()) => {
                        s.handle.close();
                        // Echo the original path so the agent can verify
                        // *which* file was deleted. `cancel` deletes the
                        // partial WAV best-effort; we report success
                        // unconditionally because the file may not have
                        // been created yet (early failure) and the agent
                        // still needs the path for cleanup logging.
                        let path = s.output_path.clone();
                        let deleted = !path.exists();
                        Ok((path, deleted))
                    }
                    Err(CancelError::NotRecording { current }) => {
                        sessions.insert(session_id, s);
                        Err(SessionError::Cancel(format!("NotRecording {{ {current:?} }}")))
                    }
                },
            };
            let _ = reply.send(result);
        }
        Command::GetLevels { session_id, reply } => {
            let result = match sessions.get(&session_id) {
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
            let result = match sessions.get(&session_id) {
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
    }
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
