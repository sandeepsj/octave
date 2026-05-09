---
name: octave-test
description: Verification rules for every Octave step. Specifies which facade(s) test the change (UI / agent / direct MCP probe / unit-only), defines the manual-probe templates, and gates the cycle from advancing to the reviewer if tests don't pass. Invoked from octave-cycle step 4.
---

# Octave testing rules

## Coverage matrix — what tests what

| Change touches | Required tests |
|---|---|
| Pure RT-path Rust function (e.g., `process_output_buffer`) | unit tests in same `#[cfg(test)]` module, including under-run / wrap / EOF / handshake cases |
| Public Rust API (handle method, free function) | unit tests + integration test that drives the API end-to-end without the audio device (e.g., via `BufferSource`) |
| `tauri::command` wrapper | unit test that calls the command via `mockito` / direct invocation; **manual UI probe** as part of handoff |
| MCP tool | direct stdio probe script in `agent/probes/<tool>.mjs` that initializes, calls, asserts, returns 0/1 exit |
| UI affordance | manual probe script that the user follows in the handoff message; one Vitest / Playwright test if the affordance has logic worth automating |
| Both facades (typical) | unit + MCP probe + UI manual probe — three independent paths to the same engine call |

## Self-test sequence (step 4 of the cycle)

Before invoking the reviewer:

```
1. cargo test --workspace          # 0 failures, 0 ignored without reason
2. cargo clippy --workspace -- -D warnings   # clean
3. (if Tauri/UI changed) pnpm test  # 0 failures
4. Manual probe of each facade I changed:
   - MCP changed     → run agent/probes/<topic>.mjs, exit 0
   - tauri::command  → cargo test -p octave-app, OR open dev app
   - UI affordance   → open dev app, perform the click
5. Document the actual probe runs in the commit message footer:
   Verified: <three-line summary, one per facade>
```

If **any** test fails, fix and re-run before reviewer. Don't run reviewer
on red.

## MCP probe template (Node.js)

`agent/probes/_template.mjs` (the canonical shape — copy and edit per
tool):

```js
#!/usr/bin/env node
// Direct stdio probe of an octave-mcp tool. Exits 0 on pass.
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";

const BIN = process.env.OCTAVE_MCP_BIN
  ?? "/media/extra/Developer/octave/target/release/octave-mcp";

const p = spawn(BIN, [], { stdio: ["pipe", "pipe", "pipe"] });
let buf = "";
p.stdout.on("data", chunk => { buf += chunk; });

const send = (msg) => p.stdin.write(JSON.stringify(msg) + "\n");
const recv = () => new Promise(resolve => {
  const tick = () => {
    const i = buf.indexOf("\n");
    if (i >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      resolve(JSON.parse(line));
    } else {
      setTimeout(tick, 5);
    }
  };
  tick();
});

send({ jsonrpc: "2.0", id: 1, method: "initialize",
  params: { protocolVersion: "2024-11-05", capabilities: {},
            clientInfo: { name: "probe", version: "0" } }});
await recv();
send({ jsonrpc: "2.0", method: "notifications/initialized" });

// === probe-specific call here ===
send({ jsonrpc: "2.0", id: 2, method: "tools/call",
       params: { name: "<tool_name>", arguments: { /* … */ } }});
const resp = await recv();
const body = JSON.parse(resp.result.content[0].text);
console.log(JSON.stringify(body, null, 2));

// === probe-specific assertion here ===
if (!body.expected_field) { console.error("FAIL: expected_field missing"); process.exit(1); }

p.kill();
console.log("PASS");
```

## UI probe template

For each step, the UI probe is one path through the app. Document in the
handoff message in the form:

```
UI: open the app → <click 1> → <click 2> → expect <observable outcome>
```

If the path needs > 3 clicks, the affordance is too big — split into
two steps.

## "Manually verified" — what counts

A self-test is "manually verified" when:

- For MCP: probe script ran, exit 0, output looks right (logged in commit).
- For UI: I personally opened `pnpm tauri dev`, performed the path, and
  observed the expected outcome (logged in commit, can include a
  screenshot path for non-trivial UI).
- For pure Rust: `cargo test` ran in the relevant package; pass/fail
  visible.

"Cargo test passed" is NOT enough by itself for changes that affect a
facade. The facade probe is mandatory.

## Forbidden moves

- Marking a facade-changing step "tested" without exercising the facade.
- Hand-waving with "this should work" — run the probe.
- Using the agent test harness (which uses real LLM calls) for
  deterministic assertions; agent probes are for end-to-end "the AI
  can find and call the tool". Use direct stdio probes for behavioural
  assertions.
- Skipping the user's manual test (step 8 of the cycle) — your "tested"
  is preliminary; their "tested" is final.
