//! Handler-side application state — the canonical truth that
//! frontends see. Per system-architecture plan §4.2.
//!
//! Held behind a single `RwLock` (in `server.rs`); writers serialize
//! through `apply_*` methods that bump the `revision` counter. Phase
//! 2c will broadcast `notifications/state_changed` whenever the
//! revision moves — the counter is the synchronization signal.
//!
//! Validation state machine (plan §7.4): `validate_*` methods check
//! whether a command is allowed given the current state. They return
//! a typed error variant on rejection. The handler's tool handlers
//! pair `validate_*` with `apply_*` while holding the write lock, so
//! the (check → engine call → apply) sequence is atomic from the
//! caller's perspective.

use std::path::PathBuf;
use std::time::SystemTime;

use schemars::JsonSchema;
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

/// Maximum take history kept in memory. Older takes evicted FIFO; the
/// underlying WAVs on disk survive (handler can re-import via the
/// `play_start { source: { path } }` path). 32 covers a typical
/// vocal-take session; raise when persistence lands (§13.8).
const MAX_TAKES: usize = 32;

/// Single-truth snapshot of the handler's view of the world.
///
/// `revision` increments on every mutation so phase 2c subscribers
/// can dedupe / order pushed snapshots. Frontends always see a
/// consistent state — never a partial transition — because every
/// `apply_*` method holds the same `RwLock` write guard for the
/// validate-then-mutate sequence.
#[derive(Debug, Clone, Serialize, JsonSchema, Default)]
pub struct AppState {
    pub revision: u64,
    pub playback: Option<PlaybackSessionState>,
    pub recording: Option<RecordingSessionState>,
    pub selected_output_device: Option<String>,
    pub selected_input_device: Option<String>,
    pub takes: Vec<TakeMetadata>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlaybackSessionState {
    /// Handler-minted (UUIDv4 string). Distinct from the engine's
    /// `stream_id` — the handler can recreate this if the engine
    /// disconnects and we restart the session.
    pub session_id: String,
    /// Engine's stream id; handler uses this to address the engine.
    pub engine_stream_id: String,
    /// What's playing — either a take from history or an arbitrary path.
    pub source: PlaybackSource,
    pub started_at_unix_seconds: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlaybackSource {
    /// Playing back a take from the handler's history.
    Take { take_id: String },
    /// Playing an arbitrary file path on disk.
    File { path: PathBuf },
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RecordingSessionState {
    pub session_id: String,
    pub engine_stream_id: String,
    pub output_path: PathBuf,
    /// Mono-fold mode — if true, on stop the handler post-processes
    /// the WAV to copy L into R so the take plays through both ears
    /// even when only Input 1 had signal.
    pub mono_fold: bool,
    pub started_at_unix_seconds: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TakeMetadata {
    pub take_id: String,
    pub path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub duration_seconds: f64,
    pub peak_dbfs: f32,
    pub recorded_at_unix_seconds: u64,
    pub mono_folded: bool,
}

// ============================================================
//  Errors — the validation state machine's reject reasons
// ============================================================

#[derive(Debug, Clone, Error, Serialize, JsonSchema)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ValidationError {
    #[error("a recording session is already active: {active_session}")]
    AlreadyRecording { active_session: String },
    #[error("no recording session is active")]
    NotRecording,
    #[error("a playback session is already active: {active_session}")]
    AlreadyPlaying { active_session: String },
    #[error("no playback session is active")]
    NotPlayable,
    /// Reserved for finer-grained checks when the handler caches engine
    /// state (today: handler can only check "session exists"; engine
    /// returns its own error if the actual state is Playing / Ended /
    /// Errored). Phase 2c's push notifications will let us narrow this.
    #[allow(dead_code)]
    #[error("playback is not paused (current state may be Playing or Stopped)")]
    NotResumable,
    #[allow(dead_code)]
    #[error("playback is not seekable in the current state")]
    NotSeekable,
    #[error("no input device selected and no override given")]
    NoInputDeviceSelected,
    #[error("no output device selected and no override given")]
    NoOutputDeviceSelected,
    #[error("take not found: {take_id}")]
    UnknownTake { take_id: String },
    /// Wrong session id — the requested session is not the active one.
    #[error("session_not_found: {session_id}")]
    SessionNotFound { session_id: String },
}

// ============================================================
//  AppState methods — validators + applicators
// ============================================================

impl AppState {
    /// Internal — bump revision before returning a mutated state.
    /// Every `apply_*` ends with this so subscribers see a fresh number.
    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn validate_record_start(&self) -> Result<(), ValidationError> {
        if let Some(rec) = &self.recording {
            return Err(ValidationError::AlreadyRecording {
                active_session: rec.session_id.clone(),
            });
        }
        Ok(())
    }

    pub fn validate_record_stop(
        &self,
        session_id: Option<&str>,
    ) -> Result<&RecordingSessionState, ValidationError> {
        let rec = self.recording.as_ref().ok_or(ValidationError::NotRecording)?;
        if let Some(sid) = session_id {
            if sid != rec.session_id {
                return Err(ValidationError::SessionNotFound {
                    session_id: sid.to_string(),
                });
            }
        }
        Ok(rec)
    }

    pub fn validate_play_start(&self) -> Result<(), ValidationError> {
        if let Some(pb) = &self.playback {
            return Err(ValidationError::AlreadyPlaying {
                active_session: pb.session_id.clone(),
            });
        }
        Ok(())
    }

    pub fn validate_play_active(
        &self,
        session_id: Option<&str>,
    ) -> Result<&PlaybackSessionState, ValidationError> {
        let pb = self.playback.as_ref().ok_or(ValidationError::NotPlayable)?;
        if let Some(sid) = session_id {
            if sid != pb.session_id {
                return Err(ValidationError::SessionNotFound {
                    session_id: sid.to_string(),
                });
            }
        }
        Ok(pb)
    }

    pub fn find_take(&self, take_id: &str) -> Result<&TakeMetadata, ValidationError> {
        self.takes
            .iter()
            .find(|t| t.take_id == take_id)
            .ok_or_else(|| ValidationError::UnknownTake {
                take_id: take_id.to_string(),
            })
    }

    /// Resolve the input device id to use for a recording, given an
    /// optional override. Returns the engine-facing device id string.
    pub fn resolve_input_device(
        &self,
        override_id: Option<&str>,
    ) -> Result<String, ValidationError> {
        if let Some(id) = override_id {
            return Ok(id.to_string());
        }
        self.selected_input_device
            .clone()
            .ok_or(ValidationError::NoInputDeviceSelected)
    }

    pub fn resolve_output_device(
        &self,
        override_id: Option<&str>,
    ) -> Result<String, ValidationError> {
        if let Some(id) = override_id {
            return Ok(id.to_string());
        }
        self.selected_output_device
            .clone()
            .ok_or(ValidationError::NoOutputDeviceSelected)
    }

    // -------- mutations --------

    pub fn apply_record_started(&mut self, session: RecordingSessionState) {
        self.recording = Some(session);
        self.bump();
    }

    /// Clear active recording; if `take` is provided, push to history.
    pub fn apply_record_stopped(&mut self, take: Option<TakeMetadata>) {
        self.recording = None;
        if let Some(t) = take {
            self.takes.push(t);
            // FIFO eviction — drop oldest until under cap.
            while self.takes.len() > MAX_TAKES {
                self.takes.remove(0);
            }
        }
        self.bump();
    }

    pub fn apply_play_started(&mut self, session: PlaybackSessionState) {
        self.playback = Some(session);
        self.bump();
    }

    pub fn apply_play_stopped(&mut self) {
        self.playback = None;
        self.bump();
    }

    pub fn apply_select_input(&mut self, device_id: Option<String>) {
        self.selected_input_device = device_id;
        self.bump();
    }

    pub fn apply_select_output(&mut self, device_id: Option<String>) {
        self.selected_output_device = device_id;
        self.bump();
    }

    pub fn apply_delete_take(&mut self, take_id: &str) -> Option<TakeMetadata> {
        let pos = self.takes.iter().position(|t| t.take_id == take_id)?;
        let take = self.takes.remove(pos);
        self.bump();
        Some(take)
    }
}

/// Helper: now as Unix seconds. Saturates to 0 on clock skew (impossible
/// on modern systems, but cheap to handle).
pub fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mint a fresh handler-side session id (UUIDv4 string).
pub fn new_session_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_recording() -> RecordingSessionState {
        RecordingSessionState {
            session_id: "rec-1".into(),
            engine_stream_id: "stream-1".into(),
            output_path: "/tmp/take.wav".into(),
            mono_fold: true,
            started_at_unix_seconds: 0,
        }
    }

    fn fake_playback() -> PlaybackSessionState {
        PlaybackSessionState {
            session_id: "play-1".into(),
            engine_stream_id: "stream-2".into(),
            source: PlaybackSource::File {
                path: "/tmp/take.wav".into(),
            },
            started_at_unix_seconds: 0,
        }
    }

    fn fake_take(take_id: &str) -> TakeMetadata {
        TakeMetadata {
            take_id: take_id.into(),
            path: format!("/tmp/{take_id}.wav").into(),
            sample_rate: 48_000,
            channels: 2,
            duration_seconds: 1.0,
            peak_dbfs: -10.0,
            recorded_at_unix_seconds: 0,
            mono_folded: true,
        }
    }

    #[test]
    fn record_start_rejected_when_already_recording() {
        let s = AppState {
            recording: Some(fake_recording()),
            ..AppState::default()
        };
        match s.validate_record_start() {
            Err(ValidationError::AlreadyRecording { active_session }) => {
                assert_eq!(active_session, "rec-1");
            }
            other => panic!("expected AlreadyRecording, got {other:?}"),
        }
    }

    #[test]
    fn record_stop_rejected_when_idle() {
        let s = AppState::default();
        match s.validate_record_stop(None) {
            Err(ValidationError::NotRecording) => {}
            other => panic!("expected NotRecording, got {other:?}"),
        }
    }

    #[test]
    fn record_stop_rejected_with_wrong_session_id() {
        let s = AppState {
            recording: Some(fake_recording()),
            ..AppState::default()
        };
        match s.validate_record_stop(Some("wrong-id")) {
            Err(ValidationError::SessionNotFound { session_id }) => {
                assert_eq!(session_id, "wrong-id");
            }
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    #[test]
    fn record_stop_accepts_correct_session_id_or_none() {
        let s = AppState {
            recording: Some(fake_recording()),
            ..AppState::default()
        };
        s.validate_record_stop(None).expect("None should match active");
        s.validate_record_stop(Some("rec-1"))
            .expect("matching id should succeed");
    }

    #[test]
    fn play_start_rejected_when_already_playing() {
        let s = AppState {
            playback: Some(fake_playback()),
            ..AppState::default()
        };
        match s.validate_play_start() {
            Err(ValidationError::AlreadyPlaying { active_session }) => {
                assert_eq!(active_session, "play-1");
            }
            other => panic!("expected AlreadyPlaying, got {other:?}"),
        }
    }

    #[test]
    fn play_active_validators_reject_when_idle() {
        let s = AppState::default();
        assert!(matches!(
            s.validate_play_active(None),
            Err(ValidationError::NotPlayable)
        ));
    }

    #[test]
    fn resolve_devices_uses_override_then_selected_then_errors() {
        let mut s = AppState::default();
        // No override, no selected → error.
        assert!(s.resolve_input_device(None).is_err());
        // Selected only → returns that.
        s.selected_input_device = Some("DEV-A".into());
        assert_eq!(s.resolve_input_device(None).unwrap(), "DEV-A");
        // Override wins over selected.
        assert_eq!(s.resolve_input_device(Some("DEV-B")).unwrap(), "DEV-B");
    }

    #[test]
    fn apply_record_started_stops_subsequent_starts() {
        let mut s = AppState::default();
        s.validate_record_start().expect("clean start ok");
        let r0 = s.revision;
        s.apply_record_started(fake_recording());
        assert!(s.revision > r0, "revision must bump");
        s.validate_record_start()
            .expect_err("second start must reject");
    }

    #[test]
    fn apply_record_stopped_pushes_take_and_caps_history() {
        let mut s = AppState::default();
        for i in 0..(MAX_TAKES + 5) {
            s.apply_record_started(fake_recording());
            s.apply_record_stopped(Some(fake_take(&format!("t-{i}"))));
        }
        assert_eq!(s.takes.len(), MAX_TAKES, "history must cap at MAX_TAKES");
        assert_eq!(
            s.takes.first().unwrap().take_id,
            format!("t-{}", 5),
            "FIFO eviction keeps newest at the back",
        );
    }

    #[test]
    fn apply_delete_take_removes_one_and_returns_it() {
        let mut s = AppState::default();
        s.takes.push(fake_take("a"));
        s.takes.push(fake_take("b"));
        let removed = s.apply_delete_take("a");
        assert!(removed.is_some());
        assert_eq!(s.takes.len(), 1);
        assert_eq!(s.takes[0].take_id, "b");
        // Deleting a missing id is a no-op (None) and doesn't bump revision.
        let r = s.revision;
        assert!(s.apply_delete_take("nope").is_none());
        assert_eq!(s.revision, r);
    }

    #[test]
    fn revision_increments_on_every_apply() {
        let mut s = AppState::default();
        let r0 = s.revision;
        s.apply_select_input(Some("X".into()));
        let r1 = s.revision;
        assert!(r1 > r0);
        s.apply_select_output(Some("Y".into()));
        let r2 = s.revision;
        assert!(r2 > r1);
    }
}
