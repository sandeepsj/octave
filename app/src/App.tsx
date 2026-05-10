import { useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/// Mirror of the Tauri command's return shape (defined in
/// app/src-tauri/src/lib.rs — keep in sync).
interface OutputDeviceInfo {
  device_id: string;
  name: string;
  /// Linux-only: the human-readable name from /proc/asound/cards
  /// (e.g. "Focusrite Scarlett Solo USB"). Other platforms always
  /// null — Core Audio / WASAPI hand the friendly name in `name`.
  friendly_name: string | null;
  backend: string;
  is_default_output: boolean;
  max_output_channels: number;
}

interface PlaybackStartResult {
  duration_seconds: number | null;
  sample_rate: number;
  channels: number;
}

interface PlaybackStopResult {
  state: string;
  position_seconds: number;
}

/// Linux ALSA exposes ~15 PCM names per card (raw `hw:`, format-
/// converting `plughw:`, software mixer `dmix:`, channel-map
/// `surround{40,51,71}:`, `hdmi:`, `iec958:`, `dsnoop:`, etc.). For
/// the v0.1 device picker we keep only:
///   - `default` (the system default sink)
///   - `pipewire` (when present — same on most modern Linux setups)
///   - one entry per physical card (`hw:CARD=X,DEV=0`)
///   - everything from non-Alsa backends (CoreAudio / WASAPI / ASIO
///     don't have this plug-layer noise)
///
/// Advanced users see the full list via the "Show all" toggle.
const ESSENTIAL_ALSA_NAMES = new Set(["default", "pipewire"]);
const HARDWARE_ALSA_RE = /^hw:CARD=[^,]+,DEV=0$/;

function isEssential(d: OutputDeviceInfo): boolean {
  if (d.backend !== "Alsa") return true;
  if (ESSENTIAL_ALSA_NAMES.has(d.name)) return true;
  if (HARDWARE_ALSA_RE.test(d.name)) return true;
  return false;
}

/// Pretty-print a raw ALSA device name. cpal hands us the kernel's
/// PCM identifier (`hw:CARD=Generic_1,DEV=0`); the Tauri command
/// enriches with `friendly_name` from /proc/asound/cards when
/// available. We prefer the friendly name; otherwise fall back to
/// extracting the card slug. Non-Alsa backends pass through untouched.
function prettify(d: OutputDeviceInfo): string {
  if (d.backend !== "Alsa") return d.name;
  if (d.name === "default") return "System default";
  if (d.name === "pipewire") return "PipeWire";
  if (d.friendly_name) return d.friendly_name;
  const m = d.name.match(/^hw:CARD=([^,]+),DEV=\d+$/);
  if (m) return `${m[1]} (hardware)`;
  return d.name;
}

export default function App() {
  const [devices, setDevices] = useState<OutputDeviceInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [showAll, setShowAll] = useState(false);

  // Playback affordance state. We don't poll the engine for state
  // changes (the file may end on its own without us hearing about it);
  // the UI tracks "what we last asked for". A natural-end means Play
  // again will succeed; the AlreadyPlaying guard catches the case
  // where the stream is still live.
  const [sourcePath, setSourcePath] = useState("");
  const [selectedDeviceId, setSelectedDeviceId] = useState<string | null>(null);
  const [playInfo, setPlayInfo] = useState<PlaybackStartResult | null>(null);
  const [playError, setPlayError] = useState<string | null>(null);
  const [playBusy, setPlayBusy] = useState(false);

  async function handleListDevices() {
    setLoading(true);
    setError(null);
    try {
      const result = await invoke<OutputDeviceInfo[]>("list_output_devices");
      setDevices(result);
      // Pre-select the system default so the user can hit Play with
      // zero clicks. They can override by clicking another row.
      const def = result.find((d) => d.is_default_output);
      if (def && !selectedDeviceId) setSelectedDeviceId(def.device_id);
    } catch (e) {
      setError(String(e));
      setDevices(null);
    } finally {
      setLoading(false);
    }
  }

  async function handlePlay() {
    if (!selectedDeviceId || !sourcePath.trim()) return;
    setPlayBusy(true);
    setPlayError(null);
    try {
      const result = await invoke<PlaybackStartResult>("playback_start", {
        deviceId: selectedDeviceId,
        sourcePath: sourcePath.trim(),
      });
      setPlayInfo(result);
    } catch (e) {
      setPlayError(String(e));
      setPlayInfo(null);
    } finally {
      setPlayBusy(false);
    }
  }

  async function handleStop() {
    setPlayBusy(true);
    setPlayError(null);
    try {
      const result = await invoke<PlaybackStopResult>("playback_stop");
      setPlayInfo(null);
      // Surface where playback actually stopped (useful if the user
      // hit Stop before the file ended).
      setPlayError(`Stopped at ${result.position_seconds.toFixed(2)}s (${result.state})`);
    } catch (e) {
      setPlayError(String(e));
    } finally {
      setPlayBusy(false);
    }
  }

  const visible = useMemo(() => {
    if (!devices) return null;
    return showAll ? devices : devices.filter(isEssential);
  }, [devices, showAll]);

  const hiddenCount = devices && visible ? devices.length - visible.length : 0;
  const canPlay = !playBusy && !playInfo && !!selectedDeviceId && !!sourcePath.trim();

  return (
    <main className="min-h-screen p-8 max-w-2xl mx-auto">
      <header className="mb-8">
        <h1 className="text-2xl font-semibold tracking-tight">Octave</h1>
        <p className="text-muted text-sm mt-1">v0.1 — scaffold</p>
      </header>

      <button
        type="button"
        onClick={handleListDevices}
        disabled={loading}
        className="rounded-md bg-accent px-4 py-2 text-base font-medium text-black hover:bg-accent-hover disabled:opacity-50 transition"
      >
        {loading ? "Loading…" : "List Output Devices"}
      </button>

      {error && (
        <pre className="mt-4 text-red-400 text-sm whitespace-pre-wrap">{error}</pre>
      )}

      {visible && visible.length === 0 && (
        <p className="mt-6 text-muted">No output devices found.</p>
      )}

      {visible && visible.length > 0 && (
        <>
          <ul className="mt-6 space-y-2">
            {visible.map((d) => {
              const isSelected = d.device_id === selectedDeviceId;
              return (
                <li key={d.device_id}>
                  <button
                    type="button"
                    onClick={() => setSelectedDeviceId(d.device_id)}
                    className={`w-full text-left rounded-md border px-4 py-3 transition ${
                      isSelected
                        ? "bg-elevated border-accent"
                        : "bg-elevated border-border hover:border-muted"
                    }`}
                  >
                    <div className="flex items-center gap-2">
                      <span className="font-medium">{prettify(d)}</span>
                      {d.is_default_output && (
                        <span className="rounded bg-accent/20 px-1.5 py-0.5 text-xs font-medium text-accent">
                          DEFAULT
                        </span>
                      )}
                      {isSelected && (
                        <span className="ml-auto text-xs font-medium text-accent">
                          SELECTED
                        </span>
                      )}
                    </div>
                    <div className="text-sm text-muted mt-0.5 font-mono">
                      {d.backend.toLowerCase()} · max {d.max_output_channels} ch · {d.name}
                    </div>
                  </button>
                </li>
              );
            })}
          </ul>

          {!showAll && hiddenCount > 0 && (
            <button
              type="button"
              onClick={() => setShowAll(true)}
              className="mt-4 text-sm text-muted hover:text-fg underline-offset-2 hover:underline"
            >
              Show {hiddenCount} more (ALSA plug devices)
            </button>
          )}
          {showAll && (
            <button
              type="button"
              onClick={() => setShowAll(false)}
              className="mt-4 text-sm text-muted hover:text-fg underline-offset-2 hover:underline"
            >
              Hide ALSA plug devices
            </button>
          )}
        </>
      )}

      {selectedDeviceId && (
        <section className="mt-10 border-t border-border pt-8">
          <h2 className="text-lg font-semibold mb-3">Play a WAV file</h2>
          <div className="flex gap-2">
            <input
              type="text"
              value={sourcePath}
              onChange={(e) => setSourcePath(e.target.value)}
              placeholder="/absolute/path/to/audio.wav"
              spellCheck={false}
              className="flex-1 rounded-md bg-elevated border border-border px-3 py-2 font-mono text-sm placeholder:text-muted focus:border-accent focus:outline-none"
            />
            {!playInfo ? (
              <button
                type="button"
                onClick={handlePlay}
                disabled={!canPlay}
                className="rounded-md bg-accent px-4 py-2 text-base font-medium text-black hover:bg-accent-hover disabled:opacity-50 transition"
              >
                {playBusy ? "Starting…" : "Play"}
              </button>
            ) : (
              <button
                type="button"
                onClick={handleStop}
                disabled={playBusy}
                className="rounded-md bg-elevated border border-accent px-4 py-2 text-base font-medium text-accent hover:bg-accent hover:text-black disabled:opacity-50 transition"
              >
                {playBusy ? "Stopping…" : "Stop"}
              </button>
            )}
          </div>

          {playInfo && (
            <p className="mt-3 text-sm text-muted">
              Playing — {playInfo.sample_rate} Hz · {playInfo.channels} ch
              {playInfo.duration_seconds !== null &&
                ` · ${playInfo.duration_seconds.toFixed(2)}s`}
            </p>
          )}
          {playError && (
            <pre className="mt-3 text-sm text-red-400 whitespace-pre-wrap">{playError}</pre>
          )}
        </section>
      )}
    </main>
  );
}
