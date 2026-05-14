# octave-chat

Standalone CLI agent for Octave. Connects to the `octave-engine` server
and lets you drive Octave's audio engine through Claude Haiku 4.5.

## Setup

```sh
pnpm install
```

Set your Anthropic API key:

```sh
echo 'ANTHROPIC_API_KEY=sk-ant-…' > .env
```

(or `export ANTHROPIC_API_KEY=…` in your shell)

## Run

```sh
ANTHROPIC_API_KEY=sk-ant-… pnpm start
```

By default the chat spawns `cargo run -q -p octave-engine` from the
repo root and pipes stdio to the MCP client — works for local dev
with no network setup. First launch builds the binary (~30s);
subsequent launches are near-instant.

## LAN mode (when octave-engine gains HTTP/SSE transport)

Once `octave-engine` exposes an HTTP/SSE endpoint:

```sh
OCTAVE_MCP=http://192.168.1.42:8000/sse \
ANTHROPIC_API_KEY=sk-ant-… \
pnpm start
```

The chat connects to the running server over the LAN. No code change
in this project — just the env var.

## Try it

```
you> what input devices do I have?
octave> Focusrite Scarlett Solo (USB), HD-Audio Generic mic, system default.

you> record me for 3 seconds, then play it back through the system default
octave> Recorded 3.0s to /tmp/octave-take-1778…wav and played it back.
```

The agent has tool access to all `output_*` and `input_*` operations
(see the engine's MCP tool surface). Type `quit` or hit Ctrl-D to exit.
