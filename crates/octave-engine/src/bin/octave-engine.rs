//! `octave-engine` — the audio engine process binary.
//!
//! Owns all cpal hardware + active streams. Speaks low-level MCP
//! (`output_*`, `input_*`, `stream_*` tools) over stdio. Designed to
//! be launched and restart-watched by `octave-supervisor`; the
//! handler is the only intended client.
//!
//! Direct external use (e.g. for debugging) is supported via stdio.
//! For Claude Desktop or other LAN agents, connect to the **handler**
//! instead — the engine's surface is `internal` tier and not meant
//! for end-agent consumption.
//!
//! ## Tool allowlist
//!
//! Set `OCTAVE_ENGINE_TOOLS` to a comma-separated list of tool names
//! to advertise only that subset. Unset or empty = all tools enabled.
//!
//! ```json
//! {
//!   "octave-engine": {
//!     "command": "octave-engine",
//!     "env": { "OCTAVE_ENGINE_TOOLS": "input_list,input_start,input_stop" }
//!   }
//! }
//! ```

use std::process::ExitCode;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    // Logs go to stderr — stdout is the MCP protocol channel.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(e) = octave_engine::serve().await {
        tracing::error!(error = %e, "octave-engine exited with error");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
