import { useEffect, useRef, useState } from "react";
import { createAnthropic } from "@ai-sdk/anthropic";
import { generateText, stepCountIs, type ModelMessage } from "ai";

import { octaveTools } from "./octaveTools";

/// MVP Anthropic provider configured for direct browser use. Tauri
/// runs in a webview with origin `tauri://localhost` (or the dev
/// port), and Anthropic's API normally rejects browser Origins; the
/// `anthropic-dangerous-direct-browser-access` header opts past that.
/// The API key ships in the bundle — fine for a local Tauri app, NEVER
/// for a public web app. Production would route through a backend.
const anthropic = createAnthropic({
  apiKey: import.meta.env.VITE_ANTHROPIC_API_KEY,
  headers: { "anthropic-dangerous-direct-browser-access": "true" },
});

const SYSTEM_PROMPT = `You are Octave's in-app assistant. The user's audio interface is connected via the Tauri shell; use the available tools to enumerate devices and to record / play audio on the user's behalf.

When the user asks you to record, default to mono unless they explicitly ask for stereo. When you record, stop_recording returns the WAV path — feed that path straight into playback_start to play the take back. For Linux, "ALSA:default" is a safe default device id that routes through the system audio (PipeWire).

Be concise. Confirm what you did with one short sentence.`;

interface UiMessage {
  role: "user" | "assistant";
  text: string;
}

export default function Chat() {
  const [history, setHistory] = useState<ModelMessage[]>([]);
  const [ui, setUi] = useState<UiMessage[]>([]);
  const [input, setInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const scrollerRef = useRef<HTMLDivElement | null>(null);

  // Pin scroll to the bottom whenever a message lands. Same pattern
  // every chat UI uses — read scrollHeight after layout, jump there.
  useEffect(() => {
    const el = scrollerRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [ui]);

  async function send() {
    const prompt = input.trim();
    if (!prompt || busy) return;
    setUi((prev) => [...prev, { role: "user", text: prompt }]);
    setInput("");
    setBusy(true);
    setError(null);
    try {
      const messages: ModelMessage[] = [
        ...history,
        { role: "user", content: prompt },
      ];
      const result = await generateText({
        model: anthropic("claude-haiku-4-5"),
        system: SYSTEM_PROMPT,
        messages,
        tools: octaveTools,
        // Cap the agentic loop at 8 steps — well above any single user
        // intent (list → start → stop is 3) and bounded enough that a
        // confused model can't grind through tool calls forever.
        stopWhen: stepCountIs(8),
      });
      // Persist the full turn (assistant + tool messages) so the next
      // user message sees the same context the agent just saw.
      setHistory([...messages, ...result.response.messages]);
      setUi((prev) => [
        ...prev,
        { role: "assistant", text: result.text || "(no text — see tool calls)" },
      ]);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="min-h-screen flex flex-col p-6 max-w-3xl mx-auto">
      <header className="mb-4">
        <h1 className="text-xl font-semibold tracking-tight">Octave Chat</h1>
        <p className="text-muted text-sm mt-1">
          Haiku 4.5 with tool access to recording + playback. Try: "record me
          for 5 seconds and play it back".
        </p>
      </header>

      <div
        ref={scrollerRef}
        className="flex-1 overflow-y-auto rounded-md bg-elevated border border-border p-4 space-y-3 min-h-[400px]"
      >
        {ui.length === 0 && (
          <p className="text-muted text-sm">
            No messages yet. Type something below.
          </p>
        )}
        {ui.map((m, i) => (
          <div
            key={i}
            className={`text-sm whitespace-pre-wrap ${
              m.role === "user" ? "text-fg" : "text-fg/90"
            }`}
          >
            <span
              className={`inline-block text-xs px-1.5 py-0.5 rounded mr-2 align-top mt-1 ${
                m.role === "user"
                  ? "bg-accent/20 text-accent"
                  : "bg-border text-muted"
              }`}
            >
              {m.role}
            </span>
            <span>{m.text}</span>
          </div>
        ))}
        {busy && (
          <div className="text-sm text-muted">
            <span className="inline-block text-xs px-1.5 py-0.5 rounded mr-2 align-top mt-1 bg-border text-muted">
              assistant
            </span>
            thinking…
          </div>
        )}
      </div>

      {error && (
        <pre className="mt-3 text-sm text-red-400 whitespace-pre-wrap">
          {error}
        </pre>
      )}

      <div className="mt-4 flex gap-2">
        <input
          type="text"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              send();
            }
          }}
          placeholder="Ask Octave to record, list devices, play a file…"
          disabled={busy}
          className="flex-1 rounded-md bg-elevated border border-border px-3 py-2 text-sm focus:border-accent focus:outline-none disabled:opacity-50"
        />
        <button
          type="button"
          onClick={send}
          disabled={busy || !input.trim()}
          className="rounded-md bg-accent px-4 py-2 text-sm font-medium text-black hover:bg-accent-hover disabled:opacity-50 transition"
        >
          {busy ? "…" : "Send"}
        </button>
      </div>
    </main>
  );
}
