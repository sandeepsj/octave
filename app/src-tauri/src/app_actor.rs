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

use octave_player::{
    DeviceCatalog as PlayerCatalog, PlaybackHandle, PlaybackSpec, PlaybackStatus,
};
use octave_recorder::DeviceCatalog as RecorderCatalog;

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
    /// Output-device catalog shared between the synchronous
    /// `list_output_devices` Tauri command and the actor thread that
    /// calls `playback.start(...)`. Holding the same catalog across
    /// list and start is what defeats the PipeWire ALSA enumerate-race
    /// (plan §3.3.1) — `start` reuses the cpal::Device the listing
    /// already opened.
    playback: Arc<PlayerCatalog>,
    /// Symmetric input-device catalog for the recorder side. Same
    /// rationale as `playback`. Currently only feeds the read-only
    /// `list_input_devices` Tauri command — the actor doesn't yet
    /// own a recording session (record affordance is the next cycle
    /// step), but the catalog lives here so the future `record_*`
    /// commands and the listing share the same cached handles.
    recorder: Arc<RecorderCatalog>,
}

impl AppActorHandle {
    pub fn spawn() -> std::io::Result<Self> {
        let (tx, rx) = bounded::<Command>(COMMAND_QUEUE);
        let playback = Arc::new(PlayerCatalog::new());
        let recorder = Arc::new(RecorderCatalog::new());
        let actor_playback = Arc::clone(&playback);
        let join = thread::Builder::new()
            .name("octave-app-audio".into())
            .spawn(move || run_actor(rx, actor_playback))?;
        Ok(Self {
            tx: Some(tx),
            join: Arc::new(std::sync::Mutex::new(Some(join))),
            playback,
            recorder,
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

    /// Borrow the actor's playback (output) catalog for the read-only
    /// `list_output_devices` Tauri command. `Send + Sync`, safe to
    /// share across concurrent commands.
    pub fn playback_catalog(&self) -> &Arc<PlayerCatalog> {
        &self.playback
    }

    /// Borrow the actor's recorder (input) catalog for the read-only
    /// `list_input_devices` Tauri command.
    pub fn recorder_catalog(&self) -> &Arc<RecorderCatalog> {
        &self.recorder
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

fn run_actor(rx: Receiver<Command>, catalog: Arc<PlayerCatalog>) {
    let mut active: Option<PlaybackHandle> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::Start { spec, reply } => {
                if active.is_some() {
                    let _ = reply.send(Err(PlaybackStartError::AlreadyPlaying));
                    continue;
                }
                match catalog.start(spec) {
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
        }
    }
    // Channel closed (last AppActorHandle dropped) — close any active
    // session before we exit.
    if let Some(h) = active.take() {
        h.close();
    }
}
