//! MCP client wrapper around the spawned `octave-engine` child.
//!
//! rmcp gives us `TokioChildProcess` (a transport that spawns and
//! pipes stdio to a child process) and `ServiceExt::serve` (which
//! wraps `()` as a `ClientHandler` we don't need callbacks from). We
//! glue them together and expose `call_tool` for our north-side tool
//! handlers to forward into.

use std::path::PathBuf;

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::{ServiceError, ServiceExt};

use crate::HandlerError;

/// How to launch the engine child.
///
/// Defaults via `EngineCommand::default_dev()` to `cargo run -q -p
/// octave-engine` with `cwd = std::env::current_dir()` — works when
/// the handler is invoked from anywhere in the workspace. Production
/// (supervisor-managed) invocations override `program` to an
/// absolute binary path so cargo isn't on the runtime path.
#[derive(Debug, Clone)]
pub struct EngineCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl EngineCommand {
    /// Default for `cargo run` from the workspace root. Honors env
    /// overrides so power users can swap the binary without code edits:
    ///   `OCTAVE_ENGINE_PROGRAM` — replaces "cargo".
    ///   `OCTAVE_ENGINE_ARGS`    — comma-separated, replaces the cargo args.
    ///   `OCTAVE_ENGINE_CWD`     — overrides the working directory.
    pub fn default_dev() -> Self {
        let program = std::env::var("OCTAVE_ENGINE_PROGRAM").unwrap_or_else(|_| "cargo".into());
        let args = std::env::var("OCTAVE_ENGINE_ARGS")
            .ok()
            .map(|s| s.split(',').map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_else(|| {
                vec!["run".into(), "-q".into(), "-p".into(), "octave-engine".into()]
            });
        let cwd = std::env::var("OCTAVE_ENGINE_CWD").ok().map(PathBuf::from);
        Self { program, args, cwd }
    }
}

/// Live MCP client connected to the spawned engine child. Holds the
/// `RunningService` so the connection stays alive for the handler's
/// lifetime; drops it on `Drop` which closes the transport and reaps
/// the child.
pub struct EngineClient {
    // Held for lifetime; we read tool replies via `peer`.
    _running: RunningService<RoleClient, ()>,
    peer: Peer<RoleClient>,
}

impl EngineClient {
    /// Spawn the engine and connect. Returns once the MCP `initialize`
    /// handshake has completed; ready for `call_tool`.
    pub async fn spawn(cmd: EngineCommand) -> Result<Self, HandlerError> {
        tracing::info!(
            program = %cmd.program,
            args = ?cmd.args,
            cwd = ?cmd.cwd,
            "spawning octave-engine child",
        );
        let mut command = tokio::process::Command::new(&cmd.program);
        command.args(&cmd.args);
        if let Some(ref cwd) = cmd.cwd {
            command.current_dir(cwd);
        }
        let transport = TokioChildProcess::new(command)
            .map_err(|e| HandlerError::EngineSpawn(format!("TokioChildProcess::new: {e}")))?;
        let running = ()
            .serve(transport)
            .await
            .map_err(|e| HandlerError::EngineSpawn(format!("rmcp client serve: {e}")))?;
        let peer = running.peer().clone();
        Ok(Self {
            _running: running,
            peer,
        })
    }

    /// Forward a tool call through to the engine. The handler's
    /// north-side tool handlers wrap this — phase 2a is pure
    /// passthrough; phase 2b will gate calls behind the validation
    /// state machine.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<CallToolResult, ServiceError> {
        let mut params = CallToolRequestParams::new(name.to_string());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        self.peer.call_tool(params).await
    }
}
