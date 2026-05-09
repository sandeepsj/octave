//! `octave-mcp` — Model Context Protocol server exposing Octave's typed
//! Rust APIs as tools that AI agents (Claude Desktop, Claude Code,
//! custom) can call.
//!
//! v0.1 ships the seven `recording_*` tools defined in
//! [`docs/modules/record-audio.md`](https://github.com/sandeepsj/octave/blob/main/docs/modules/record-audio.md)
//! §10. The plan for the layer itself lives in
//! [`docs/modules/mcp-layer.md`](https://github.com/sandeepsj/octave/blob/main/docs/modules/mcp-layer.md).
//!
//! See [`serve`] for the entry point. Library consumers run it from
//! their own tokio runtime; the `octave-mcp` binary creates a runtime
//! and calls into it.

mod audio_actor;
mod handler;
mod types;

use thiserror::Error;

pub use audio_actor::AudioActorHandle;
pub use handler::OctaveServer;

/// Top-level error from running the MCP server. Variants cover transport,
/// rmcp framework, and audio-thread spawn failures.
#[derive(Debug, Error)]
pub enum ServerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rmcp: {0}")]
    Rmcp(String),
    #[error("audio-management thread spawn failed: {0}")]
    AudioThreadSpawn(String),
}

/// Run the MCP server over stdio. Blocks until stdin EOF or fatal error.
///
/// Spawns the audio-management thread, builds the [`OctaveServer`]
/// (the rmcp tool router), and serves on `(tokio::io::stdin(), tokio::io::stdout())`
/// per the MCP stdio convention.
///
/// # Errors
/// Returns [`ServerError::AudioThreadSpawn`] if the audio thread can't
/// start, [`ServerError::Rmcp`] for protocol-level failures, and
/// [`ServerError::Io`] for transport failures.
pub async fn serve() -> Result<(), ServerError> {
    use rmcp::ServiceExt;

    let actor = AudioActorHandle::spawn()
        .map_err(|e| ServerError::AudioThreadSpawn(e.to_string()))?;
    let server = OctaveServer::new(actor);

    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let running = server
        .serve(transport)
        .await
        .map_err(|e| ServerError::Rmcp(e.to_string()))?;

    running
        .waiting()
        .await
        .map_err(|e| ServerError::Rmcp(e.to_string()))?;
    Ok(())
}
