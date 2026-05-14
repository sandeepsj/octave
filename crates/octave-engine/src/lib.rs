//! `octave-engine` — Octave's audio engine process.
//!
//! Owns all `cpal::Device` handles and active streams. Speaks
//! low-level MCP — `output_*` / `input_*` / `stream_*` tools (see
//! `OctaveServer::all_tool_names`). The handler process is the
//! intended client; external agents (Claude Desktop) should connect
//! to the **handler** instead, not the engine. See
//! [`docs/modules/system-architecture.md`](https://github.com/sandeepsj/octave/blob/main/docs/modules/system-architecture.md).
//!
//! See [`serve`] for the entry point. Library consumers run it from
//! their own tokio runtime; the `octave-engine` binary creates a
//! runtime and calls into it.

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
/// Reads the `OCTAVE_ENGINE_TOOLS` environment variable as a
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
                "OCTAVE_ENGINE_TOOLS unset; advertising all tools"
            );
            OctaveServer::new(actor)
        }
        Some(allowed) => {
            let known: std::collections::HashSet<String> =
                OctaveServer::all_tool_names().into_iter().collect();
            let unknown: Vec<&String> = allowed.iter().filter(|n| !known.contains(*n)).collect();
            if !unknown.is_empty() {
                tracing::warn!(?unknown, "OCTAVE_ENGINE_TOOLS contains unknown names (ignored)");
            }
            let (server, enabled) = OctaveServer::with_allowed_tools(actor, &allowed);
            tracing::info!(?enabled, "OCTAVE_ENGINE_TOOLS allowlist applied");
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

/// Parse `OCTAVE_ENGINE_TOOLS` into an allowlist set. Returns `None`
/// when the variable is unset or empty/whitespace, in which case all
/// tools remain enabled.
fn parse_tools_env() -> Option<std::collections::HashSet<String>> {
    let raw = std::env::var("OCTAVE_ENGINE_TOOLS").ok()?;
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
            std::env::remove_var("OCTAVE_ENGINE_TOOLS");
        }
        assert!(parse_tools_env().is_none(), "unset → None");

        unsafe {
            std::env::set_var("OCTAVE_ENGINE_TOOLS", "");
        }
        assert!(parse_tools_env().is_none(), "empty → None");

        unsafe {
            std::env::set_var("OCTAVE_ENGINE_TOOLS", "  ,  ,");
        }
        assert!(parse_tools_env().is_none(), "whitespace+commas → None");

        unsafe {
            std::env::set_var(
                "OCTAVE_ENGINE_TOOLS",
                " input_start , input_stop ,input_start",
            );
        }
        let s = parse_tools_env().expect("Some");
        assert_eq!(s.len(), 2);
        assert!(s.contains("input_start"));
        assert!(s.contains("input_stop"));

        unsafe {
            std::env::remove_var("OCTAVE_ENGINE_TOOLS");
        }
    }

    /// System-architecture plan §4.3 single-source-of-truth for the
    /// tool set. Asserting the exact list catches accidental adds /
    /// drops / renames at compile-time of the test.
    #[test]
    fn all_tool_names_matches_published_set_of_16() {
        let mut got = OctaveServer::all_tool_names();
        got.sort();
        let expected = [
            "input_cancel",
            "input_describe",
            "input_levels",
            "input_list",
            "input_start",
            "input_status",
            "input_stop",
            "output_describe",
            "output_levels",
            "output_list",
            "output_pause",
            "output_resume",
            "output_seek",
            "output_start",
            "output_status",
            "output_stop",
        ];
        assert_eq!(got.len(), 16, "expected 16 tools, got {}: {got:?}", got.len());
        assert_eq!(got.iter().map(String::as_str).collect::<Vec<_>>(), expected);
    }
}
