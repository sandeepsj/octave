//! `octave-handler` — the intent + state process binary.
//!
//! Spawns an `octave-engine` child, exposes its own MCP server on
//! stdio. Frontends (Tauri UI, chat CLI, future voice) connect here.
//!
//! Phase 2a is plumbing only; the real surface lands in phase 2b.

use std::process::ExitCode;

use octave_handler::EngineCommand;

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

    let engine_cmd = EngineCommand::default_dev();
    if let Err(e) = octave_handler::serve(engine_cmd).await {
        tracing::error!(error = %e, "octave-handler exited with error");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
