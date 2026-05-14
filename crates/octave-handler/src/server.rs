//! North-side rmcp server — the intent + state surface frontends use.
//!
//! Per system-architecture plan §4.3, the handler exposes a
//! high-level surface: `record_*`, `play_*`, `devices_*`, `state_*`,
//! `takes_*`. Each tool:
//!
//! 1. Acquires the AppState write lock.
//! 2. Validates the command against current state (state.rs validators).
//! 3. Issues the south-side engine call(s).
//! 4. Applies the resulting state mutation under the same lock.
//! 5. Returns the result to the frontend.
//!
//! Holding the write lock across the engine call serializes all
//! state-mutating commands. The engine itself serializes via its own
//! actor, so this matches the underlying concurrency model and avoids
//! TOCTOU races between validate and apply.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Json;
use rmcp::model::{CallToolResult, ErrorData};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;

use crate::engine_client::EngineClient;
use crate::state::{
    AppState, PlaybackSessionState, PlaybackSource, RecordingSessionState, TakeMetadata,
    ValidationError, new_session_id, now_unix_seconds,
};

#[derive(Clone)]
pub struct HandlerServer {
    engine: Arc<EngineClient>,
    state: Arc<RwLock<AppState>>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl HandlerServer {
    pub fn new(engine: EngineClient) -> Self {
        Self {
            engine: Arc::new(engine),
            state: Arc::new(RwLock::new(AppState::default())),
            tool_router: Self::tool_router(),
        }
    }

    pub fn all_tool_names() -> Vec<String> {
        Self::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }
}

// ============================================================
//  Wire shapes — intent tool args + results
// ============================================================

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DevicesListArgs {
    /// "input" or "output". Defaults to "output".
    #[serde(default = "default_direction_output")]
    pub direction: Direction,
}

fn default_direction_output() -> Direction {
    Direction::Output
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Input,
    Output,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DevicesSelectArgs {
    /// Set to null/absent to clear the selection (forces caller to pass
    /// device_id on each subsequent record_start / play_start).
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RecordStartArgs {
    /// Override the selected_input_device. None → use AppState's selection.
    pub device_id: Option<String>,
    /// True (default) folds capture R onto L on stop so a single mic
    /// into Input 1 of a 2-input interface plays through both ears.
    /// False keeps true stereo.
    #[serde(default = "default_true")]
    pub mono: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RecordStartResult {
    pub session_id: String,
    pub output_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SessionRefArgs {
    /// Optional — when None, refers to the (single) active session of
    /// that direction. Reject with `session_not_found` if non-None and
    /// doesn't match the active session.
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PlayStartArgs {
    pub source: PlaySourceArg,
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlaySourceArg {
    /// Replay a take from the handler's history.
    Take { take_id: String },
    /// Play an arbitrary WAV file by absolute path.
    File { path: PathBuf },
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlayStartResult {
    pub session_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PlaySeekArgs {
    pub session_id: Option<String>,
    pub position_seconds: f64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TakeIdArgs {
    pub take_id: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeleteTakeResult {
    pub deleted: bool,
    pub file_removed: bool,
}

// ============================================================
//  Tool implementations
// ============================================================

#[tool_router]
impl HandlerServer {
    // ---------- state ----------

    #[tool(
        name = "state_get",
        description = "Snapshot the handler's current AppState. Frontends call this on connect; phase 2c will add notifications/state_changed push so polling becomes optional."
    )]
    async fn state_get(&self) -> Result<Json<AppState>, ErrorData> {
        let state = self.state.read().await;
        Ok(Json(state.clone()))
    }

    // ---------- devices ----------

    #[tool(
        name = "devices_list",
        description = "List devices of the requested direction (input or output). Forwards to the engine's input_list / output_list."
    )]
    async fn devices_list(
        &self,
        rmcp::handler::server::wrapper::Parameters(args): rmcp::handler::server::wrapper::Parameters<DevicesListArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let engine_tool = match args.direction {
            Direction::Input => "input_list",
            Direction::Output => "output_list",
        };
        forward_engine_call(self.engine.call_tool(engine_tool, None).await)
    }

    #[tool(
        name = "devices_select_input",
        description = "Set the user's preferred input device. Use null device_id to clear. Returns the updated AppState."
    )]
    async fn devices_select_input(
        &self,
        rmcp::handler::server::wrapper::Parameters(args): rmcp::handler::server::wrapper::Parameters<DevicesSelectArgs>,
    ) -> Result<Json<AppState>, ErrorData> {
        let mut state = self.state.write().await;
        state.apply_select_input(args.device_id);
        Ok(Json(state.clone()))
    }

    #[tool(
        name = "devices_select_output",
        description = "Set the user's preferred output device. Use null device_id to clear. Returns the updated AppState."
    )]
    async fn devices_select_output(
        &self,
        rmcp::handler::server::wrapper::Parameters(args): rmcp::handler::server::wrapper::Parameters<DevicesSelectArgs>,
    ) -> Result<Json<AppState>, ErrorData> {
        let mut state = self.state.write().await;
        state.apply_select_output(args.device_id);
        Ok(Json(state.clone()))
    }

    // ---------- recording ----------

    #[tool(
        name = "record_start",
        description = "Begin recording from the selected (or override) input device. Captures stereo internally; if mono=true (default), the resulting WAV is post-processed on stop so both channels carry the mic signal. Returns a handler-side session_id and the output file path."
    )]
    async fn record_start(
        &self,
        rmcp::handler::server::wrapper::Parameters(args): rmcp::handler::server::wrapper::Parameters<RecordStartArgs>,
    ) -> Result<Json<RecordStartResult>, ErrorData> {
        let mut state = self.state.write().await;
        state.validate_record_start().map_err(validation_to_error)?;
        let device_id = state
            .resolve_input_device(args.device_id.as_deref())
            .map_err(validation_to_error)?;
        let output_path = generated_take_path();

        // Hardcoded for v1: 48 kHz / 2 channels / Default buffer. Mirrors
        // the Tauri shell's defaults; revisit when capability probing
        // gets wired in (probe via input_describe before input_start).
        let engine_args = json!({
            "device_id": device_id,
            "sample_rate": 48_000,
            "buffer_size": { "kind": "default" },
            "channels": 2,
            "output_path": output_path.to_string_lossy(),
        });
        let map = engine_args.as_object().cloned();
        let engine_result = self
            .engine
            .call_tool("input_start", map)
            .await
            .map_err(engine_call_error)?;
        let parsed: EngineStartResult = parse_engine_result(&engine_result)?;

        let session_id = new_session_id();
        let session = RecordingSessionState {
            session_id: session_id.clone(),
            engine_stream_id: parsed.stream_id,
            output_path: output_path.clone(),
            mono_fold: args.mono,
            started_at_unix_seconds: now_unix_seconds(),
        };
        state.apply_record_started(session);

        Ok(Json(RecordStartResult {
            session_id,
            output_path,
        }))
    }

    #[tool(
        name = "record_stop",
        description = "Stop the active recording. If the session was started with mono=true, the resulting stereo WAV's right channel is folded onto its left. Returns the take metadata."
    )]
    async fn record_stop(
        &self,
        rmcp::handler::server::wrapper::Parameters(args): rmcp::handler::server::wrapper::Parameters<SessionRefArgs>,
    ) -> Result<Json<TakeMetadata>, ErrorData> {
        let mut state = self.state.write().await;
        let rec = state
            .validate_record_stop(args.session_id.as_deref())
            .map_err(validation_to_error)?;
        let stream_id = rec.engine_stream_id.clone();
        let mono_fold = rec.mono_fold;

        let engine_args = json!({ "stream_id": stream_id });
        let result = self
            .engine
            .call_tool("input_stop", engine_args.as_object().cloned())
            .await
            .map_err(engine_call_error)?;
        let clip: EngineClipResult = parse_engine_result(&result)?;

        // Apply mono-fold post-process if requested. Same logic the
        // Tauri shell ran in app/src-tauri/src/app_actor.rs — moved
        // here because the engine doesn't know what "mono" means.
        let mut peak_dbfs = clip.peak_dbfs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let mono_folded = mono_fold && clip.channels == 2;
        if mono_folded {
            if let Err(e) = fold_stereo_to_mono_left(&clip.path) {
                tracing::warn!(
                    path = %clip.path.display(),
                    error = %e,
                    "mono-fold failed; clip stays stereo with right channel silent",
                );
            } else if peak_dbfs.is_finite() {
                // Right peak now mirrors left; max stays the same.
                // Nothing to update — peak across both channels is the
                // already-computed max.
            }
        }
        if !peak_dbfs.is_finite() {
            peak_dbfs = -180.0;
        }

        let take = TakeMetadata {
            take_id: new_session_id(), // mint a take id (UUID)
            path: clip.path,
            sample_rate: clip.sample_rate,
            channels: clip.channels,
            duration_seconds: clip.duration_seconds,
            peak_dbfs,
            recorded_at_unix_seconds: clip.started_at_unix_seconds,
            mono_folded,
        };
        state.apply_record_stopped(Some(take.clone()));
        Ok(Json(take))
    }

    // ---------- playback ----------

    #[tool(
        name = "play_start",
        description = "Play a take or arbitrary WAV through the selected (or override) output device. source is one of `{kind:\"take\", take_id}` or `{kind:\"file\", path}`. Returns a handler-side session_id."
    )]
    async fn play_start(
        &self,
        rmcp::handler::server::wrapper::Parameters(args): rmcp::handler::server::wrapper::Parameters<PlayStartArgs>,
    ) -> Result<Json<PlayStartResult>, ErrorData> {
        let mut state = self.state.write().await;
        state.validate_play_start().map_err(validation_to_error)?;
        let device_id = state
            .resolve_output_device(args.device_id.as_deref())
            .map_err(validation_to_error)?;

        let (source_path, source_marker) = match &args.source {
            PlaySourceArg::Take { take_id } => {
                let take = state.find_take(take_id).map_err(validation_to_error)?;
                (
                    take.path.clone(),
                    PlaybackSource::Take {
                        take_id: take_id.clone(),
                    },
                )
            }
            PlaySourceArg::File { path } => (
                path.clone(),
                PlaybackSource::File { path: path.clone() },
            ),
        };

        let engine_args = json!({
            "device_id": device_id,
            "source": { "kind": "file", "path": source_path.to_string_lossy() },
            "buffer_size": { "kind": "default" },
        });
        let result = self
            .engine
            .call_tool("output_start", engine_args.as_object().cloned())
            .await
            .map_err(engine_call_error)?;
        let parsed: EngineStartResult = parse_engine_result(&result)?;

        let session_id = new_session_id();
        let session = PlaybackSessionState {
            session_id: session_id.clone(),
            engine_stream_id: parsed.stream_id,
            source: source_marker,
            started_at_unix_seconds: now_unix_seconds(),
        };
        state.apply_play_started(session);
        Ok(Json(PlayStartResult { session_id }))
    }

    #[tool(name = "play_pause", description = "Pause the active playback.")]
    async fn play_pause(
        &self,
        rmcp::handler::server::wrapper::Parameters(args): rmcp::handler::server::wrapper::Parameters<SessionRefArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let stream_id = self.active_play_stream(args.session_id.as_deref()).await?;
        let engine_args = json!({ "stream_id": stream_id });
        forward_engine_call(
            self.engine
                .call_tool("output_pause", engine_args.as_object().cloned())
                .await,
        )
    }

    #[tool(name = "play_resume", description = "Resume the paused playback.")]
    async fn play_resume(
        &self,
        rmcp::handler::server::wrapper::Parameters(args): rmcp::handler::server::wrapper::Parameters<SessionRefArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let stream_id = self.active_play_stream(args.session_id.as_deref()).await?;
        let engine_args = json!({ "stream_id": stream_id });
        forward_engine_call(
            self.engine
                .call_tool("output_resume", engine_args.as_object().cloned())
                .await,
        )
    }

    #[tool(
        name = "play_stop",
        description = "Stop the active playback and clear the session."
    )]
    async fn play_stop(
        &self,
        rmcp::handler::server::wrapper::Parameters(args): rmcp::handler::server::wrapper::Parameters<SessionRefArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let stream_id = {
            let mut state = self.state.write().await;
            let pb = state
                .validate_play_active(args.session_id.as_deref())
                .map_err(validation_to_error)?;
            let id = pb.engine_stream_id.clone();
            // Apply state immediately — the engine call below is the
            // observable side effect, but the user's intent ("clear
            // the playback session") should not survive a failed stop.
            state.apply_play_stopped();
            id
        };
        let engine_args = json!({ "stream_id": stream_id });
        forward_engine_call(
            self.engine
                .call_tool("output_stop", engine_args.as_object().cloned())
                .await,
        )
    }

    #[tool(
        name = "play_seek",
        description = "Seek to position_seconds in the active playback."
    )]
    async fn play_seek(
        &self,
        rmcp::handler::server::wrapper::Parameters(args): rmcp::handler::server::wrapper::Parameters<PlaySeekArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let stream_id = self.active_play_stream(args.session_id.as_deref()).await?;
        let engine_args = json!({
            "stream_id": stream_id,
            "position_seconds": args.position_seconds,
        });
        forward_engine_call(
            self.engine
                .call_tool("output_seek", engine_args.as_object().cloned())
                .await,
        )
    }

    // ---------- takes ----------

    #[tool(
        name = "takes_delete",
        description = "Remove a take from the in-memory history and unlink its WAV from disk. Returns whether the deletion succeeded; file_removed is false if the disk delete failed (history entry is removed either way)."
    )]
    async fn takes_delete(
        &self,
        rmcp::handler::server::wrapper::Parameters(args): rmcp::handler::server::wrapper::Parameters<TakeIdArgs>,
    ) -> Result<Json<DeleteTakeResult>, ErrorData> {
        let mut state = self.state.write().await;
        let removed = state.apply_delete_take(&args.take_id);
        let (deleted, file_removed) = match removed {
            None => (false, false),
            Some(take) => {
                let removed_file = std::fs::remove_file(&take.path).is_ok();
                (true, removed_file)
            }
        };
        Ok(Json(DeleteTakeResult {
            deleted,
            file_removed,
        }))
    }

    // ---------- internal ----------

    /// Validate + extract the engine stream id for any play_* transport
    /// op. Held under read-lock briefly; engine call happens after.
    async fn active_play_stream(
        &self,
        session_id: Option<&str>,
    ) -> Result<String, ErrorData> {
        let state = self.state.read().await;
        let pb = state
            .validate_play_active(session_id)
            .map_err(validation_to_error)?;
        Ok(pb.engine_stream_id.clone())
    }
}

#[tool_handler]
impl ServerHandler for HandlerServer {}

// ============================================================
//  Helpers
// ============================================================

/// Map a state-machine validation error to an MCP error envelope.
/// Uses `invalid_request` so agents can pattern-match on the typed
/// `code` field embedded via serde.
fn validation_to_error(err: ValidationError) -> ErrorData {
    let payload = serde_json::to_value(&err).unwrap_or(serde_json::Value::Null);
    ErrorData::invalid_request(err.to_string(), Some(payload))
}

/// Map an rmcp `ServiceError` (south-side transport / protocol failure)
/// to a north-side ErrorData. Internal-error tier — frontends shouldn't
/// see these often; if they do, the handler-engine link is broken.
fn engine_call_error(err: rmcp::ServiceError) -> ErrorData {
    ErrorData::internal_error(
        format!("engine_call_failed: {err}"),
        Some(json!({ "engine_error": err.to_string() })),
    )
}

/// Forward an engine `CallToolResult` to the frontend, preserving
/// `is_error` and content. The handler doesn't try to re-shape — the
/// engine's wire contract is already agent-friendly.
fn forward_engine_call(
    result: Result<CallToolResult, rmcp::ServiceError>,
) -> Result<CallToolResult, ErrorData> {
    result.map_err(engine_call_error)
}

/// Decode a typed JSON body from the engine's `CallToolResult`. Engines
/// return their result via `structured_content` (the rmcp `Json<T>`
/// wrapper). is_error=true → propagates as a handler-side error.
fn parse_engine_result<T: for<'de> Deserialize<'de>>(
    result: &CallToolResult,
) -> Result<T, ErrorData> {
    if result.is_error.unwrap_or(false) {
        // The engine reports a tool-level failure. Surface its content
        // (typically a single text block) verbatim.
        let body = result
            .content
            .iter()
            .filter_map(|c| serde_json::to_value(c).ok())
            .collect::<Vec<_>>();
        return Err(ErrorData::invalid_request(
            "engine returned tool error",
            Some(json!({ "engine_content": body })),
        ));
    }
    let value = result
        .structured_content
        .clone()
        .ok_or_else(|| {
            ErrorData::internal_error(
                "engine response missing structured_content",
                None,
            )
        })?;
    serde_json::from_value(value).map_err(|e| {
        ErrorData::internal_error(format!("engine response decode: {e}"), None)
    })
}

#[derive(Deserialize)]
struct EngineStartResult {
    stream_id: String,
}

#[derive(Deserialize)]
struct EngineClipResult {
    path: PathBuf,
    sample_rate: u32,
    channels: u16,
    #[serde(default)]
    started_at_unix_seconds: u64,
    duration_seconds: f64,
    peak_dbfs: Vec<f32>,
}

/// Auto-generate a take path under the user's tmp dir. Mirrors the
/// Tauri shell's helper; deduped here because the handler is now the
/// owner of "where do takes go" once the Tauri app becomes a thin
/// MCP client (phase 4).
fn generated_take_path() -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("octave-take-{millis}.wav"))
}

/// Mono-fold a 32-bit-float stereo WAV in-place: copy each frame's
/// left sample into its right slot. Plays through stereo devices on
/// both ears even when only Input 1 of the interface had signal.
/// Lifted verbatim from app/src-tauri/src/app_actor.rs — same WAV
/// shape (RIFF, IEEE float, 2 channels), same scan-for-data-chunk
/// trick.
fn fold_stereo_to_mono_left(path: &std::path::Path) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    let mut bytes = std::fs::read(path)?;
    let Some(idx) = bytes.windows(4).position(|w| w == b"data") else {
        return Err(Error::new(ErrorKind::InvalidData, "no data chunk"));
    };
    let data_start = idx + 8;
    let frame_bytes = 8usize; // 2 channels × 4 bytes (f32)
    let mut i = data_start;
    while i + frame_bytes <= bytes.len() {
        bytes.copy_within(i..i + 4, i + 4);
        i += frame_bytes;
    }
    std::fs::write(path, bytes)
}
