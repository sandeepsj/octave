/**
 * Octave Chat — standalone CLI.
 *
 * Connects to the `octave-mcp` server and lets you drive Octave's
 * audio engine through Claude Haiku 4.5 via the Vercel AI SDK.
 *
 * Transport selection (env `OCTAVE_MCP`):
 *   stdio (default)  — spawn `cargo run -q -p octave-mcp` from the
 *                      sibling repo and pipe stdio to the MCP client.
 *                      Works for local dev; needs no network setup.
 *   <url>            — connect via SSE over HTTP to a running
 *                      octave-mcp instance (LAN-accessible). HTTP
 *                      transport on octave-mcp is the next cycle step;
 *                      this branch is wired so the swap is one env
 *                      var, not a code change.
 *
 * Required env: `ANTHROPIC_API_KEY=sk-ant-…`.
 *
 * Run:
 *   ANTHROPIC_API_KEY=sk-ant-… pnpm start
 */

import { createInterface } from "node:readline/promises";
import { stdin as input, stdout as output } from "node:process";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

import { createAnthropic } from "@ai-sdk/anthropic";
import { generateText, jsonSchema, stepCountIs, tool, type ModelMessage } from "ai";

import { Client as McpClient } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { SSEClientTransport } from "@modelcontextprotocol/sdk/client/sse.js";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";

const ANTHROPIC_API_KEY = process.env.ANTHROPIC_API_KEY;
if (!ANTHROPIC_API_KEY) {
  console.error(
    "ANTHROPIC_API_KEY not set. Run with:\n  ANTHROPIC_API_KEY=sk-ant-… pnpm start",
  );
  process.exit(1);
}

const SYSTEM_PROMPT = `You are Octave's CLI assistant. You have tools to enumerate audio devices and to record / play audio on the user's machine through the octave-mcp server.

Defaults the user expects:
- Recording defaults to mono unless they explicitly ask for stereo (their interface is typically a single-mic Focusrite Solo).
- On Linux, the device id "ALSA:default" routes through the system audio (PipeWire) — safe pick when the user doesn't name a specific device.
- After recording_stop returns the WAV path, feed that path straight into playback_start to play the take back.

Be concise. Confirm what you did with one short sentence. Don't explain tool calls unless asked.`;

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");

function buildTransport(): { transport: Transport; describe: string } {
  const url = process.env.OCTAVE_MCP;
  if (url && url !== "stdio") {
    return {
      transport: new SSEClientTransport(new URL(url)),
      describe: `SSE → ${url}`,
    };
  }
  // stdio: spawn `cargo run -q -p octave-mcp` from the repo root.
  // cargo handles incremental rebuild; first launch may take a few
  // seconds, subsequent launches are near-instant.
  return {
    transport: new StdioClientTransport({
      command: "cargo",
      args: ["run", "-q", "-p", "octave-mcp"],
      cwd: REPO_ROOT,
    }),
    describe: `stdio → cargo run -q -p octave-mcp (cwd ${REPO_ROOT})`,
  };
}

/**
 * Wrap an MCP server's tools as AI SDK `tool()` definitions. The MCP
 * client gives us a list of `{ name, description, inputSchema }` (with
 * inputSchema as JSON Schema); the SDK's `tool({ inputSchema, execute })`
 * accepts JSON Schema via the `jsonSchema()` helper. Each `execute`
 * round-trips through the MCP client's `callTool` so the agent's tool
 * call lands on the actual octave-mcp server.
 */
async function buildToolset(mcp: McpClient) {
  const { tools: mcpTools } = await mcp.listTools();
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const out: Record<string, any> = {};
  for (const t of mcpTools) {
    out[t.name] = tool({
      description: t.description ?? "",
      inputSchema: jsonSchema(t.inputSchema as Record<string, unknown>),
      execute: async (args: unknown) => {
        const result = await mcp.callTool({
          name: t.name,
          arguments: args as Record<string, unknown>,
        });
        return result;
      },
    });
  }
  return out;
}

async function main() {
  const { transport, describe } = buildTransport();
  console.log(`octave-chat: connecting via ${describe}`);

  const mcp = new McpClient({ name: "octave-chat", version: "0.1.0" });
  await mcp.connect(transport);

  const tools = await buildToolset(mcp);
  const toolNames = Object.keys(tools);
  console.log(
    `octave-chat: ${toolNames.length} tools loaded (${toolNames.join(", ")})\n`,
  );

  const anthropic = createAnthropic({ apiKey: ANTHROPIC_API_KEY });

  const rl = createInterface({ input, output });
  const history: ModelMessage[] = [];

  const cleanup = async () => {
    try {
      await mcp.close();
    } catch {
      /* best-effort */
    }
    rl.close();
  };
  process.on("SIGINT", () => {
    console.log("\nbye");
    void cleanup().then(() => process.exit(0));
  });

  // REPL: read line → call agent → print → loop. `quit` / Ctrl-D exit.
  while (true) {
    let prompt: string;
    try {
      prompt = await rl.question("you> ");
    } catch {
      // readline rejects on Ctrl-D / EOF.
      break;
    }
    if (!prompt.trim()) continue;
    if (prompt.trim() === "quit") break;

    history.push({ role: "user", content: prompt });
    try {
      const result = await generateText({
        model: anthropic("claude-haiku-4-5"),
        system: SYSTEM_PROMPT,
        messages: history,
        tools,
        // Bounded loop. list → record → wait → stop → play is at most
        // ~5 tool calls; 8 leaves headroom without runaway.
        stopWhen: stepCountIs(8),
      });
      // Persist the assistant + tool messages so the next turn sees the
      // context the agent just acted on.
      history.push(...result.response.messages);
      console.log(`octave> ${result.text || "(no text — see tool calls)"}\n`);
    } catch (e) {
      console.error(`octave> error: ${e}\n`);
    }
  }

  await cleanup();
}

await main();
