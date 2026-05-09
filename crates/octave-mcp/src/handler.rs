//! rmcp tool router — the MCP tool surface.
//!
//! Each `#[tool]` method below corresponds to one row in
//! [`docs/modules/mcp-layer.md`](../../../../docs/modules/mcp-layer.md)
//! §10.1. Read-only tools (`recording_list_devices`,
//! `recording_describe_device`) bypass the actor and call the recorder
//! directly. All seven session-aware tools route through the
//! [`AudioActorHandle`] so the `!Send` `RecordingHandle`s stay on one
//! thread (see audio_actor.rs).

use std::time::UNIX_EPOCH;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::audio_actor::{
    AudioActorHandle, Command, SessionError, StartReplyError, spec_from_args,
};
use crate::types::{
    CapabilitiesJson, CancelResult, DescribeDeviceArgs, DeviceInfoJson, LevelsResult,
    ListDevicesResult, RecordedClipJson, SessionArgs, StartArgs, StartResult, StatusResult,
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
    pub fn new(actor: AudioActorHandle) -> Self {
        Self {
            actor,
            tool_router: Self::tool_router(),
        }
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
            Err(e) => Err(ErrorData::invalid_params(
                format!("OpenError::{e:?}"),
                None,
            )),
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
}

#[tool_handler]
impl ServerHandler for OctaveServer {}

fn parse_session_id(s: &str) -> Result<Uuid, ErrorData> {
    Uuid::parse_str(s).map_err(|_| {
        ErrorData::invalid_params(format!("invalid session_id: {s} is not a UUID"), None)
    })
}
