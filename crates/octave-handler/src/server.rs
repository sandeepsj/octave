//! North-side rmcp server — the surface frontends connect to.
//!
//! Phase 2a ships three passthrough tools to prove the wire loop:
//!
//! - `state_get`             — placeholder; returns `{"phase": "2a", ...}`.
//! - `devices_list_inputs`   — proxies to engine's `input_list`.
//! - `devices_list_outputs`  — proxies to engine's `output_list`.
//!
//! Phase 2b will replace these with the real `record_*`, `play_*`,
//! `state_*`, `devices_*` surface backed by AppState + validation.
//! Phase 2c adds push notifications.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Json;
use rmcp::model::{CallToolResult, ErrorData};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::json;

use crate::engine_client::EngineClient;

/// Phase 2a placeholder for AppState. Phase 2b replaces with the real
/// struct from system-architecture §4.2.
#[derive(Debug, Clone, Serialize, JsonSchema)]
struct StatePlaceholder {
    phase: String,
    note: String,
}

#[derive(Clone)]
pub struct HandlerServer {
    engine: Arc<EngineClient>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl HandlerServer {
    pub fn new(engine: EngineClient) -> Self {
        Self {
            engine: Arc::new(engine),
            tool_router: Self::tool_router(),
        }
    }

    /// Names of every tool the handler exposes. Useful for diagnostics
    /// and for integration tests.
    pub fn all_tool_names() -> Vec<String> {
        Self::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }
}

#[tool_router]
impl HandlerServer {
    #[tool(
        name = "state_get",
        description = "Return the handler's app-state snapshot. Phase 2a placeholder; phase 2b returns the real AppState (current playback / recording sessions, take history, device selection) per system-architecture §4.2."
    )]
    async fn state_get(&self) -> Result<Json<StatePlaceholder>, ErrorData> {
        Ok(Json(StatePlaceholder {
            phase: "2a".into(),
            note: "AppState lands in phase 2b. This handler currently passes through to the engine."
                .into(),
        }))
    }

    #[tool(
        name = "devices_list_inputs",
        description = "List input (recording) devices the engine can see. Forwarded to the engine's input_list tool."
    )]
    async fn devices_list_inputs(&self) -> Result<CallToolResult, ErrorData> {
        forward(self.engine.call_tool("input_list", None).await)
    }

    #[tool(
        name = "devices_list_outputs",
        description = "List output (playback) devices the engine can see. Forwarded to the engine's output_list tool."
    )]
    async fn devices_list_outputs(&self) -> Result<CallToolResult, ErrorData> {
        forward(self.engine.call_tool("output_list", None).await)
    }
}

#[tool_handler]
impl ServerHandler for HandlerServer {}

/// Adapt an engine-side rmcp `ServiceError` into a north-side
/// `ErrorData` for the calling frontend. We surface the engine's
/// error message verbatim — it's already shaped per the engine's
/// wire contract (e.g. `"DeviceError::DeviceNotFound(..)"`).
fn forward(result: Result<CallToolResult, rmcp::ServiceError>) -> Result<CallToolResult, ErrorData> {
    result.map_err(|e| {
        ErrorData::internal_error(
            format!("engine_call_failed: {e}"),
            Some(json!({ "engine_error": e.to_string() })),
        )
    })
}
