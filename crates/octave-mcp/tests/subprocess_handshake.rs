//! Subprocess integration test for the `octave-mcp` binary.
//!
//! Spawns the built binary, drives it over stdio with raw JSON-RPC,
//! and verifies the MCP handshake + tools/list + an error path.
//! Covers `mcp-layer` plan §12.3 ("subprocess JSON-RPC handshake")
//! and §14 acceptance line "Subprocess integration test passes in CI".

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Path to the built binary. cargo test sets `CARGO_BIN_EXE_octave-mcp`
/// to the absolute path of the binary it just built — the canonical
/// way to invoke a binary crate from its own integration test.
const BIN_ENV: &str = "CARGO_BIN_EXE_octave-mcp";

fn binary_path() -> String {
    std::env::var(BIN_ENV)
        .unwrap_or_else(|_| panic!("{BIN_ENV} must be set by cargo test"))
}

fn send_line(stdin: &mut impl Write, line: &str) {
    writeln!(stdin, "{line}").expect("write to mcp stdin");
    stdin.flush().expect("flush mcp stdin");
}

fn read_line_with_deadline(reader: &mut BufReader<impl std::io::Read>, deadline: Instant) -> String {
    if Instant::now() > deadline {
        panic!("read deadline already passed");
    }
    let mut buf = String::new();
    let n = reader.read_line(&mut buf).expect("read mcp stdout");
    if n == 0 {
        panic!("unexpected EOF from mcp stdout");
    }
    buf
}

#[test]
fn subprocess_handshake_lists_all_tools_and_rejects_unknown() {
    let mut child = Command::new(binary_path())
        // Quiet stderr so test output stays clean.
        .env("RUST_LOG", "warn")
        // Make sure no developer-shell allowlist leaks in — we want
        // to assert the full 16-tool default surface.
        .env_remove("OCTAVE_MCP_TOOLS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn octave-mcp binary");

    let mut stdin = child.stdin.take().expect("stdin pipe");
    let stdout = child.stdout.take().expect("stdout pipe");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(10);

    // 1. initialize → expect server capabilities response.
    send_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"subprocess-test","version":"0"}}}"#,
    );
    let init_resp = read_line_with_deadline(&mut reader, deadline);
    assert!(init_resp.contains(r#""id":1"#), "initialize response: {init_resp}");
    assert!(
        init_resp.contains("serverInfo") || init_resp.contains("capabilities"),
        "initialize response missing server info: {init_resp}",
    );

    // 2. notifications/initialized (no response expected).
    send_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );

    // 3. tools/list → expect all 16 tools.
    send_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let tools_resp = read_line_with_deadline(&mut reader, deadline);
    assert!(tools_resp.contains(r#""id":2"#), "tools/list response: {tools_resp}");
    for expected in [
        "recording_list_devices",
        "recording_describe_device",
        "recording_start",
        "recording_stop",
        "recording_cancel",
        "recording_get_levels",
        "recording_get_status",
        "playback_list_output_devices",
        "playback_describe_device",
        "playback_start",
        "playback_pause",
        "playback_resume",
        "playback_stop",
        "playback_seek",
        "playback_get_status",
        "playback_get_levels",
    ] {
        assert!(
            tools_resp.contains(&format!("\"{expected}\"")),
            "tools/list missing tool {expected}",
        );
    }

    // 4. tools/call with an unknown tool → error response.
    send_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"definitely_not_a_real_tool","arguments":{}}}"#,
    );
    let unknown_resp = read_line_with_deadline(&mut reader, deadline);
    assert!(
        unknown_resp.contains(r#""id":3"#),
        "unknown-tool response: {unknown_resp}",
    );
    assert!(
        unknown_resp.contains("error") || unknown_resp.contains("not found"),
        "unknown-tool response should be an error: {unknown_resp}",
    );

    // 5. Close stdin → server should EOF gracefully (we don't assert
    // an exit code; some MCP transports return non-zero on EOF and
    // both behaviours are acceptable for "handshake completed").
    drop(stdin);
    let _ = child.wait().expect("wait for mcp child");
}
