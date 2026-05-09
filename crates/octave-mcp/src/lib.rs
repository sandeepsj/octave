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
/// # Tool filtering
/// Reads the `OCTAVE_MCP_TOOLS` environment variable as a
/// comma-separated allowlist of tool names. Empty or unset = all tools
/// enabled. Set = only listed tools are advertised by `tools/list` and
/// accepted by `call_tool`. Unknown names are ignored (logged as a
/// warning).
///
/// # Errors
/// Returns [`ServerError::AudioThreadSpawn`] if the audio thread can't
/// start, [`ServerError::Rmcp`] for protocol-level failures, and
/// [`ServerError::Io`] for transport failures.
pub async fn serve() -> Result<(), ServerError> {
    use rmcp::ServiceExt;

    let actor = AudioActorHandle::spawn()
        .map_err(|e| ServerError::AudioThreadSpawn(e.to_string()))?;

    let server = match parse_tools_env() {
        None => {
            tracing::info!(
                tools = ?OctaveServer::all_tool_names(),
                "OCTAVE_MCP_TOOLS unset; advertising all tools"
            );
            OctaveServer::new(actor)
        }
        Some(allowed) => {
            let known: std::collections::HashSet<String> =
                OctaveServer::all_tool_names().into_iter().collect();
            let unknown: Vec<&String> = allowed.iter().filter(|n| !known.contains(*n)).collect();
            if !unknown.is_empty() {
                tracing::warn!(?unknown, "OCTAVE_MCP_TOOLS contains unknown names (ignored)");
            }
            let (server, enabled) = OctaveServer::with_allowed_tools(actor, &allowed);
            tracing::info!(?enabled, "OCTAVE_MCP_TOOLS allowlist applied");
            server
        }
    };

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

/// Parse `OCTAVE_MCP_TOOLS` into an allowlist set. Returns `None` when
/// the variable is unset or empty/whitespace, in which case all tools
/// remain enabled.
fn parse_tools_env() -> Option<std::collections::HashSet<String>> {
    let raw = std::env::var("OCTAVE_MCP_TOOLS").ok()?;
    let set: std::collections::HashSet<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if set.is_empty() { None } else { Some(set) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tools_env_handles_unset_empty_and_whitespace() {
        // Use unique scopes per case so concurrent tests don't collide.
        // SAFETY: env var mutation in tests; serial within this fn.
        unsafe {
            std::env::remove_var("OCTAVE_MCP_TOOLS");
        }
        assert!(parse_tools_env().is_none(), "unset → None");

        unsafe {
            std::env::set_var("OCTAVE_MCP_TOOLS", "");
        }
        assert!(parse_tools_env().is_none(), "empty → None");

        unsafe {
            std::env::set_var("OCTAVE_MCP_TOOLS", "  ,  ,");
        }
        assert!(parse_tools_env().is_none(), "whitespace+commas → None");

        unsafe {
            std::env::set_var(
                "OCTAVE_MCP_TOOLS",
                " recording_start , recording_stop ,recording_start",
            );
        }
        let s = parse_tools_env().expect("Some");
        assert_eq!(s.len(), 2);
        assert!(s.contains("recording_start"));
        assert!(s.contains("recording_stop"));

        unsafe {
            std::env::remove_var("OCTAVE_MCP_TOOLS");
        }
    }
}
