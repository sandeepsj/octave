//! Vercel AI SDK `tool()` definitions that wrap our existing Tauri
//! commands. The agent (Haiku via @ai-sdk/anthropic) sees these as its
//! action surface — same names + semantics as the MCP server tools we
//! ship for external agents (Claude Desktop, etc.), just bridged
//! through Tauri IPC instead of an MCP transport.
//!
//! Future cycle step will swap this hand-rolled bridge for a real
//! MCP client connecting to the `octave-mcp` binary, so external and
//! internal agents share the exact same wire surface.

import { invoke } from "@tauri-apps/api/core";
import { tool } from "ai";
import { z } from "zod";

export const octaveTools = {
  list_output_devices: tool({
    description:
      "List output (speaker / headphone / interface) devices the system can play through. Returns each device's id, friendly name, backend, default-output flag, and channel count.",
    inputSchema: z.object({}),
    execute: async () => invoke("list_output_devices"),
  }),

  list_input_devices: tool({
    description:
      "List input (microphone / line-in / interface) devices the system can record from.",
    inputSchema: z.object({}),
    execute: async () => invoke("list_input_devices"),
  }),

  playback_start: tool({
    description:
      "Play a 32-bit-float WAV file through an output device. The device_id must come from list_output_devices. Use 'ALSA:default' on Linux to route through the system default sink.",
    inputSchema: z.object({
      device_id: z.string(),
      source_path: z.string().describe("Absolute path to the WAV file."),
    }),
    execute: async ({ device_id, source_path }) =>
      invoke("playback_start", { deviceId: device_id, sourcePath: source_path }),
  }),

  playback_stop: tool({
    description: "Stop the currently-playing file. Returns the position reached.",
    inputSchema: z.object({}),
    execute: async () => invoke("playback_stop"),
  }),

  playback_status: tool({
    description:
      "Snapshot the live playback state. Returns null if nothing is playing; otherwise state name (Playing / Paused / Ended), position seconds, and total duration seconds.",
    inputSchema: z.object({}),
    execute: async () => invoke("playback_status"),
  }),

  playback_pause: tool({
    description: "Pause the current playback (resumable).",
    inputSchema: z.object({}),
    execute: async () => invoke("playback_pause"),
  }),

  playback_resume: tool({
    description: "Resume a paused playback.",
    inputSchema: z.object({}),
    execute: async () => invoke("playback_resume"),
  }),

  recording_start: tool({
    description:
      "Start a recording from an input device. The device_id must come from list_input_devices. Set mono=true (default) for the common single-mic case — the resulting stereo WAV's right channel is folded onto the left so playback reaches both ears. Set mono=false for true stereo capture (Input 1 → L, Input 2 → R).",
    inputSchema: z.object({
      device_id: z.string(),
      mono: z.boolean().describe("True for single-mic capture; false for true stereo."),
    }),
    execute: async ({ device_id, mono }) =>
      invoke("recording_start", { deviceId: device_id, mono }),
  }),

  recording_stop: tool({
    description:
      "Stop the active recording. Returns the saved WAV's path, sample rate, channels, frame_count, duration_seconds, xrun_count, and peak dBFS. The path can be fed straight into playback_start.",
    inputSchema: z.object({}),
    execute: async () => invoke("recording_stop"),
  }),

  recording_status: tool({
    description:
      "Snapshot the live recording state. Returns null if nothing is recording; otherwise elapsed seconds and xrun count.",
    inputSchema: z.object({}),
    execute: async () => invoke("recording_status"),
  }),
};
