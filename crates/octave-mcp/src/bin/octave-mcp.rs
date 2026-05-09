//! `octave-mcp` — Model Context Protocol server binary.
//!
//! Configure your agent to launch this binary over stdio. Example for
//! Claude Desktop's `mcpServers` config:
//!
//! ```json
//! {
//!   "octave": {
//!     "command": "octave-mcp"
//!   }
//! }
//! ```
//!
//! ## Tool allowlist
//!
//! Set `OCTAVE_MCP_TOOLS` to a comma-separated list of tool names to
//! advertise only that subset (saves agent context budget). Unset or
//! empty = all tools enabled.
//!
//! ```json
//! {
//!   "octave": {
//!     "command": "octave-mcp",
//!     "env": { "OCTAVE_MCP_TOOLS": "recording_list_devices,recording_start,recording_stop" }
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

    if let Err(e) = octave_mcp::serve().await {
        tracing::error!(error = %e, "octave-mcp exited with error");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
