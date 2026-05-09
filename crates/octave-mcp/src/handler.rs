//! rmcp tool router — the MCP tool surface.
//!
//! Each `#[tool]` method below corresponds to one row in
//! [`docs/modules/mcp-layer.md`](../../../../docs/modules/mcp-layer.md)
//! §10.1. Read-only tools (`recording_list_devices`,
//! `recording_describe_device`) bypass the actor and call the recorder
//! directly. All seven session-aware tools route through the
//! [`AudioActorHandle`] so the `!Send` `RecordingHandle`s stay on one
//! thread (see audio_actor.rs).

use std::collections::HashSet;
use std::time::UNIX_EPOCH;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::audio_actor::{
    AudioActorHandle, Command, PlaybackSessionError, PlaybackStartError, SessionError,
    StartReplyError, spec_from_args,
};
use crate::types::{
    CapabilitiesJson, CancelResult, DescribeDeviceArgs, DeviceInfoJson, LevelsResult,
    ListDevicesResult, ListOutputDevicesResult, OutputDeviceInfoJson, PlaybackSeekArgs,
    PlaybackSeekResult, PlaybackStartArgs, PlaybackStartResult, PlaybackStatusJson,
    PlaybackTransportResult, RecordedClipJson, SessionArgs, StartArgs, StartResult, StatusResult,
};

/// The MCP server's stateful root. Holds the actor handle so each tool
/// invocation can reach the audio-management thread.
#[derive(Clone)]
pub struct OctaveServer {
    actor: AudioActorHandle,
    // Used by macro-generated `ServerHandler::list_tools` and `call_tool`.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl OctaveServer {
    /// All tools enabled.
    pub fn new(actor: AudioActorHandle) -> Self {
        Self {
            actor,
            tool_router: Self::tool_router(),
        }
    }

    /// Only tools whose names appear in `allowed` are advertised by
    /// `tools/list` and accepted by `call_tool`. Unknown names in the
    /// set are ignored. Returns the names of the tools that are actually
    /// enabled (intersection of `allowed` and the registered tools).
    pub fn with_allowed_tools(
        actor: AudioActorHandle,
        allowed: &HashSet<String>,
    ) -> (Self, Vec<String>) {
        let mut tool_router = Self::tool_router();
        let mut enabled = Vec::new();
        let names: Vec<String> = tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for name in &names {
            if allowed.contains(name) {
                enabled.push(name.clone());
            } else {
                tool_router.disable_route(name.clone());
            }
        }
        (Self { actor, tool_router }, enabled)
    }

    /// Names of every tool the server knows about, regardless of whether
    /// they are currently enabled. Useful for diagnostics and config
    /// validation.
    pub fn all_tool_names() -> Vec<String> {
        Self::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }
}

#[tool_router]
impl OctaveServer {
    #[tool(
        name = "recording_list_devices",
        description = "Enumerate every input device the host can see across all backends. Returns an array with each device's stable id, human name, backend, default-input flag, and channel count. Safe and read-only."
    )]
    async fn recording_list_devices(&self) -> Result<Json<ListDevicesResult>, ErrorData> {
        let devices = octave_recorder::list_devices()
            .into_iter()
            .map(DeviceInfoJson::from)
            .collect();
        Ok(Json(ListDevicesResult { devices }))
    }

    #[tool(
        name = "recording_describe_device",
        description = "Return supported sample rates, channel counts, and buffer sizes for one device. Use the device_id from recording_list_devices."
    )]
    async fn recording_describe_device(
        &self,
        Parameters(DescribeDeviceArgs { device_id }): Parameters<DescribeDeviceArgs>,
    ) -> Result<Json<CapabilitiesJson>, ErrorData> {
        let id = octave_recorder::DeviceId(device_id);
        match octave_recorder::device_capabilities(&id) {
            Ok(c) => Ok(Json(c.into())),
            Err(e) => Err(ErrorData::invalid_params(format_typed_error("OpenError", &e), None)),
        }
    }

    #[tool(
        name = "recording_start",
        description = "Open the named device, start the audio callback, and begin writing 32-bit float WAV to output_path. Returns a session_id you pass to subsequent tools. DESTRUCTIVE: overwrites output_path if it exists.",
        annotations(
            title = "Start a recording",
            destructive_hint = true
        )
    )]
    async fn recording_start(
        &self,
        Parameters(args): Parameters<StartArgs>,
    ) -> Result<Json<StartResult>, ErrorData> {
        let spec = spec_from_args(
            args.device_id,
            args.sample_rate,
            args.buffer_size.into(),
            args.channels,
        );
        let (tx, rx) = oneshot::channel();
        self.actor
            .send(Command::StartRecording {
                spec,
                output_path: args.output_path,
                reply: tx,
            })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let reply = rx
            .await
            .map_err(|_| ErrorData::internal_error("audio actor dropped reply", None))?;
        match reply {
            Ok(r) => Ok(Json(StartResult {
                session_id: r.session_id.to_string(),
                started_at_unix_seconds: r
                    .started_at
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            })),
            Err(StartReplyError::Open(s)) => Err(ErrorData::invalid_params(s, None)),
            Err(StartReplyError::Arm(s)) | Err(StartReplyError::Record(s)) => {
                Err(ErrorData::internal_error(s, None))
            }
            Err(StartReplyError::TooManySessions) => Err(ErrorData::invalid_request(
                "TooManySessions: 8 concurrent recording sessions max in v0.1",
                None,
            )),
        }
    }

    #[tool(
        name = "recording_stop",
        description = "Stop a recording cleanly. Drains the buffer, finalizes the WAV header, fsyncs, returns the clip metadata."
    )]
    async fn recording_stop(
        &self,
        Parameters(SessionArgs { session_id }): Parameters<SessionArgs>,
    ) -> Result<Json<RecordedClipJson>, ErrorData> {
        let session_id = parse_session_id(&session_id)?;
        let (tx, rx) = oneshot::channel();
        self.actor
            .send(Command::Stop {
                session_id,
                reply: tx,
            })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let reply = rx
            .await
            .map_err(|_| ErrorData::internal_error("audio actor dropped reply", None))?;
        match reply {
            Ok(c) => Ok(Json(c)),
            Err(SessionError::NotFound(id)) => Err(ErrorData::invalid_params(
                format!("session_not_found: {id}"),
                None,
            )),
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "recording_cancel",
        description = "Stop a recording and delete the partial file. Use when the recording is unwanted. DESTRUCTIVE: removes the output file.",
        annotations(destructive_hint = true)
    )]
    async fn recording_cancel(
        &self,
        Parameters(SessionArgs { session_id }): Parameters<SessionArgs>,
    ) -> Result<Json<CancelResult>, ErrorData> {
        let session_id = parse_session_id(&session_id)?;
        let (tx, rx) = oneshot::channel();
        self.actor
            .send(Command::Cancel {
                session_id,
                reply: tx,
            })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let reply = rx
            .await
            .map_err(|_| ErrorData::internal_error("audio actor dropped reply", None))?;
        match reply {
            Ok((path, deleted)) => Ok(Json(CancelResult { path, deleted })),
            Err(SessionError::NotFound(id)) => Err(ErrorData::invalid_params(
                format!("session_not_found: {id}"),
                None,
            )),
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "recording_get_levels",
        description = "Read current per-channel peak and RMS levels in dBFS. Safe to poll at meter rates (e.g., 30 Hz). Returns NEG_INFINITY before the meter is live."
    )]
    async fn recording_get_levels(
        &self,
        Parameters(SessionArgs { session_id }): Parameters<SessionArgs>,
    ) -> Result<Json<LevelsResult>, ErrorData> {
        let session_id = parse_session_id(&session_id)?;
        let (tx, rx) = oneshot::channel();
        self.actor
            .send(Command::GetLevels {
                session_id,
                reply: tx,
            })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let reply = rx
            .await
            .map_err(|_| ErrorData::internal_error("audio actor dropped reply", None))?;
        match reply {
            Ok(l) => Ok(Json(l)),
            Err(SessionError::NotFound(id)) => Err(ErrorData::invalid_params(
                format!("session_not_found: {id}"),
                None,
            )),
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "recording_get_status",
        description = "Return the recorder's state, xrun count, dropped-sample count, and elapsed seconds since recording_start."
    )]
    async fn recording_get_status(
        &self,
        Parameters(SessionArgs { session_id }): Parameters<SessionArgs>,
    ) -> Result<Json<StatusResult>, ErrorData> {
        let session_id = parse_session_id(&session_id)?;
        let (tx, rx) = oneshot::channel();
        self.actor
            .send(Command::GetStatus {
                session_id,
                reply: tx,
            })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let reply = rx
            .await
            .map_err(|_| ErrorData::internal_error("audio actor dropped reply", None))?;
        match reply {
            Ok(s) => Ok(Json(s)),
            Err(SessionError::NotFound(id)) => Err(ErrorData::invalid_params(
                format!("session_not_found: {id}"),
                None,
            )),
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    // ============================================================
    //   Playback tools
    // ============================================================

    #[tool(
        name = "playback_list_output_devices",
        description = "Enumerate every output device the host can see across all backends. Returns each device's stable id, name, backend, default-output flag, and channel count. Safe and read-only."
    )]
    async fn playback_list_output_devices(
        &self,
    ) -> Result<Json<ListOutputDevicesResult>, ErrorData> {
        let devices = octave_player::list_output_devices()
            .into_iter()
            .map(OutputDeviceInfoJson::from)
            .collect();
        Ok(Json(ListOutputDevicesResult { devices }))
    }

    #[tool(
        name = "playback_describe_device",
        description = "Return supported sample rates, channel counts, and buffer sizes for one output device. Use the device_id from playback_list_output_devices."
    )]
    async fn playback_describe_device(
        &self,
        Parameters(DescribeDeviceArgs { device_id }): Parameters<DescribeDeviceArgs>,
    ) -> Result<Json<CapabilitiesJson>, ErrorData> {
        let id = octave_player::DeviceId(device_id);
        match octave_player::output_device_capabilities(&id) {
            Ok(c) => Ok(Json(c.into())),
            Err(e) => Err(ErrorData::invalid_params(
                format_typed_error("DeviceError", &e),
                None,
            )),
        }
    }

    #[tool(
        name = "playback_start",
        description = "Open the named output device, load the source (file path or in-memory f32 buffer), and begin playback. Single playback session at a time. Returns a session_id for subsequent transport tools."
    )]
    async fn playback_start(
        &self,
        Parameters(args): Parameters<PlaybackStartArgs>,
    ) -> Result<Json<PlaybackStartResult>, ErrorData> {
        const MAX_BUFFER_SAMPLES: usize = 100 * 1024 * 1024 / 4; // 100 MB of f32

        let source = match args.source {
            crate::types::PlaybackSourceJson::File { path } => {
                octave_player::PlaybackSourceSpec::File { path }
            }
            crate::types::PlaybackSourceJson::Buffer {
                samples,
                sample_rate,
                channels,
            } => {
                if samples.len() > MAX_BUFFER_SAMPLES {
                    return Err(ErrorData::invalid_params(
                        format!(
                            "buffer source too large ({} samples > {} cap); use a file source",
                            samples.len(),
                            MAX_BUFFER_SAMPLES
                        ),
                        None,
                    ));
                }
                octave_player::PlaybackSourceSpec::Buffer {
                    samples: samples.into(),
                    sample_rate,
                    channels,
                }
            }
        };
        let spec = octave_player::PlaybackSpec {
            device_id: octave_player::DeviceId(args.device_id),
            source,
            buffer_size: args.buffer_size.into(),
        };

        let (tx, rx) = oneshot::channel();
        self.actor
            .send(Command::PlaybackStart { spec, reply: tx })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let reply = rx
            .await
            .map_err(|_| ErrorData::internal_error("audio actor dropped reply", None))?;
        match reply {
            Ok(r) => Ok(Json(PlaybackStartResult {
                session_id: r.session_id.to_string(),
                started_at_unix_seconds: r
                    .started_at
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                duration_seconds: r.duration_seconds,
                sample_rate: r.sample_rate,
                channels: r.channels,
            })),
            Err(PlaybackStartError::AlreadyPlaying { current_session }) => {
                Err(ErrorData::invalid_request(
                    format!("AlreadyPlaying: current_session={current_session}"),
                    None,
                ))
            }
            Err(PlaybackStartError::Start(s)) => Err(ErrorData::invalid_params(s, None)),
        }
    }

    #[tool(
        name = "playback_pause",
        description = "Pause the active playback session. State transitions to Paused; resume with playback_resume."
    )]
    async fn playback_pause(
        &self,
        Parameters(SessionArgs { session_id }): Parameters<SessionArgs>,
    ) -> Result<Json<PlaybackTransportResult>, ErrorData> {
        let session_id = parse_session_id(&session_id)?;
        let (tx, rx) = oneshot::channel();
        self.actor
            .send(Command::PlaybackPause { session_id, reply: tx })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        playback_transport_reply(rx).await
    }

    #[tool(
        name = "playback_resume",
        description = "Resume a paused playback session from its current position."
    )]
    async fn playback_resume(
        &self,
        Parameters(SessionArgs { session_id }): Parameters<SessionArgs>,
    ) -> Result<Json<PlaybackTransportResult>, ErrorData> {
        let session_id = parse_session_id(&session_id)?;
        let (tx, rx) = oneshot::channel();
        self.actor
            .send(Command::PlaybackResume { session_id, reply: tx })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        playback_transport_reply(rx).await
    }

    #[tool(
        name = "playback_stop",
        description = "Stop the active playback session. Drops the device, joins the reader thread, returns the final status."
    )]
    async fn playback_stop(
        &self,
        Parameters(SessionArgs { session_id }): Parameters<SessionArgs>,
    ) -> Result<Json<PlaybackStatusJson>, ErrorData> {
        let session_id = parse_session_id(&session_id)?;
        let (tx, rx) = oneshot::channel();
        self.actor
            .send(Command::PlaybackStop { session_id, reply: tx })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let reply = rx
            .await
            .map_err(|_| ErrorData::internal_error("audio actor dropped reply", None))?;
        match reply {
            Ok(s) => Ok(Json(s)),
            Err(PlaybackSessionError::NotFound(id)) => Err(ErrorData::invalid_params(
                format!("session_not_found: {id}"),
                None,
            )),
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "playback_seek",
        description = "Seek to a position in the source. Provide either position_seconds (f64) or position_frames (u64); if both are given, frames win. User-visible cost: one period of silence at the seek point (~1.3 ms at 48 kHz / 64 buffer)."
    )]
    async fn playback_seek(
        &self,
        Parameters(args): Parameters<PlaybackSeekArgs>,
    ) -> Result<Json<PlaybackSeekResult>, ErrorData> {
        let session_id = parse_session_id(&args.session_id)?;

        // Resolve the seek target. We need the session's sample_rate to
        // convert seconds → frames; ask the actor for status first.
        let target_frames = if let Some(f) = args.position_frames {
            f
        } else if let Some(secs) = args.position_seconds {
            let (st_tx, st_rx) = oneshot::channel();
            self.actor
                .send(Command::PlaybackGetStatus { session_id, reply: st_tx })
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            let status = st_rx
                .await
                .map_err(|_| ErrorData::internal_error("audio actor dropped reply", None))?
                .map_err(|e| match e {
                    PlaybackSessionError::NotFound(id) => ErrorData::invalid_params(
                        format!("session_not_found: {id}"),
                        None,
                    ),
                    other => ErrorData::internal_error(other.to_string(), None),
                })?;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let frames = (secs * f64::from(status.sample_rate)).max(0.0) as u64;
            frames
        } else {
            return Err(ErrorData::invalid_params(
                "playback_seek: provide position_seconds or position_frames",
                None,
            ));
        };

        let (tx, rx) = oneshot::channel();
        self.actor
            .send(Command::PlaybackSeek {
                session_id,
                target_frames,
                reply: tx,
            })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let reply = rx
            .await
            .map_err(|_| ErrorData::internal_error("audio actor dropped reply", None))?;
        match reply {
            Ok(r) => Ok(Json(r)),
            Err(PlaybackSessionError::NotFound(id)) => Err(ErrorData::invalid_params(
                format!("session_not_found: {id}"),
                None,
            )),
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "playback_get_status",
        description = "Return the playback session's state, position, duration, and xrun count. Safe to poll."
    )]
    async fn playback_get_status(
        &self,
        Parameters(SessionArgs { session_id }): Parameters<SessionArgs>,
    ) -> Result<Json<PlaybackStatusJson>, ErrorData> {
        let session_id = parse_session_id(&session_id)?;
        let (tx, rx) = oneshot::channel();
        self.actor
            .send(Command::PlaybackGetStatus { session_id, reply: tx })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let reply = rx
            .await
            .map_err(|_| ErrorData::internal_error("audio actor dropped reply", None))?;
        match reply {
            Ok(s) => Ok(Json(s)),
            Err(PlaybackSessionError::NotFound(id)) => Err(ErrorData::invalid_params(
                format!("session_not_found: {id}"),
                None,
            )),
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    #[tool(
        name = "playback_get_levels",
        description = "Read current per-channel peak and RMS output levels in dBFS. Safe to poll at meter rates (~30 Hz). Returns -180 dBFS before the first audio buffer is rendered."
    )]
    async fn playback_get_levels(
        &self,
        Parameters(SessionArgs { session_id }): Parameters<SessionArgs>,
    ) -> Result<Json<LevelsResult>, ErrorData> {
        let session_id = parse_session_id(&session_id)?;
        let (tx, rx) = oneshot::channel();
        self.actor
            .send(Command::PlaybackGetLevels { session_id, reply: tx })
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let reply = rx
            .await
            .map_err(|_| ErrorData::internal_error("audio actor dropped reply", None))?;
        match reply {
            Ok(l) => Ok(Json(l)),
            Err(PlaybackSessionError::NotFound(id)) => Err(ErrorData::invalid_params(
                format!("session_not_found: {id}"),
                None,
            )),
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }
}

async fn playback_transport_reply(
    rx: oneshot::Receiver<Result<PlaybackTransportResult, PlaybackSessionError>>,
) -> Result<Json<PlaybackTransportResult>, ErrorData> {
    let reply = rx
        .await
        .map_err(|_| ErrorData::internal_error("audio actor dropped reply", None))?;
    match reply {
        Ok(r) => Ok(Json(r)),
        Err(PlaybackSessionError::NotFound(id)) => Err(ErrorData::invalid_params(
            format!("session_not_found: {id}"),
            None,
        )),
        Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
    }
}

#[tool_handler]
impl ServerHandler for OctaveServer {}

fn parse_session_id(s: &str) -> Result<Uuid, ErrorData> {
    Uuid::parse_str(s).map_err(|_| {
        ErrorData::invalid_params(format!("invalid session_id: {s} is not a UUID"), None)
    })
}

/// Format an error enum variant as `EnumName::DebugBody` per
/// mcp-layer plan §10.3 wire-contract format. Uses Debug so the
/// variant name + structured fields land verbatim — agents pattern-
/// match on the variant prefix to recover from typed failures.
///
/// Used by every site in this module that converts a typed engine
/// error into the JSON-RPC `error.message` string. Centralised so
/// the format stays uniform: a future change here updates every
/// tool's error envelope at once.
fn format_typed_error<E: std::fmt::Debug>(prefix: &str, err: &E) -> String {
    format!("{prefix}::{err:?}")
}
