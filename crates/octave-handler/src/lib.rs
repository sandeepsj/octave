//! `octave-handler` — Octave's intent + state layer.
//!
//! Speaks MCP both ways: a north-side **server** that frontends
//! (Tauri UI, chat CLI, future voice) connect to, and a south-side
//! **client** that talks to a child `octave-engine` process. See
//! [`docs/modules/system-architecture.md`](https://github.com/sandeepsj/octave/blob/main/docs/modules/system-architecture.md).
//!
//! # Phases
//!
//! - **2a (this commit):** plumbing only — handler spawns engine,
//!   exposes a tiny passthrough surface (`state_get`,
//!   `devices_list_inputs`, `devices_list_outputs`) to prove the wire
//!   loop end-to-end. No state, no validation rules, no notifications.
//! - **2b:** add the AppState struct + validation state machine + the
//!   high-level intent surface (`record_start`, `play_pause`, etc.).
//! - **2c:** add `notifications/state_changed` push so frontends stay
//!   in sync without polling.
//!
//! See [`serve`] for the entry point.

mod engine_client;
mod server;
mod state;

use thiserror::Error;

pub use engine_client::{EngineClient, EngineCommand};
pub use server::HandlerServer;

/// Top-level error from running the handler.
#[derive(Debug, Error)]
pub enum HandlerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rmcp: {0}")]
    Rmcp(String),
    #[error("engine spawn failed: {0}")]
    EngineSpawn(String),
}

/// Run the handler. Spawns the engine child via `engine_cmd`, connects
/// the MCP client, then serves the handler's MCP server on stdio.
/// Blocks until stdin EOF or fatal error.
///
/// The caller chooses how the engine is launched — for dev, that's
/// usually `cargo run -q -p octave-engine` from the repo root. The
/// future supervisor will pass an absolute binary path.
pub async fn serve(engine_cmd: EngineCommand) -> Result<(), HandlerError> {
    use rmcp::ServiceExt;

    let engine = EngineClient::spawn(engine_cmd).await?;
    let server = HandlerServer::new(engine);

    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let running = server
        .serve(transport)
        .await
        .map_err(|e| HandlerError::Rmcp(e.to_string()))?;
    running
        .waiting()
        .await
        .map_err(|e| HandlerError::Rmcp(e.to_string()))?;
    Ok(())
}
