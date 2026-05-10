import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

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

/// Mirror of `PlaybackStatusResult` in app/src-tauri/src/lib.rs.
/// `state` is the engine's `PlaybackState` Debug-formatted —
/// "Playing", "Paused", "Stopped", "Ended", "Errored", "Closed".
interface PlaybackStatus {
  state: string;
  position_seconds: number;
  duration_seconds: number | null;
}

/// Format seconds as "M:SS". Negative / NaN → "0:00".
function formatTime(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "0:00";
  const m = Math.floor(seconds / 60);
  const s = Math.floor(seconds % 60);
  return `${m}:${s.toString().padStart(2, "0")}`;
}

const POLL_INTERVAL_MS = 200;

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

  // Playback affordance state. `playInfo` is the start-time snapshot
  // (sample rate / channels / total duration); `playStatus` is the
  // live snapshot from the engine, refreshed on a 200 ms tick while
  // a session is active. We need both: playInfo carries the duration
  // (status sometimes loses it on terminal states), playStatus carries
  // the live position + state for the buttons.
  const [sourcePath, setSourcePath] = useState("");
  const [selectedDeviceId, setSelectedDeviceId] = useState<string | null>(null);
  const [playInfo, setPlayInfo] = useState<PlaybackStartResult | null>(null);
  const [playStatus, setPlayStatus] = useState<PlaybackStatus | null>(null);
  const [playError, setPlayError] = useState<string | null>(null);
  const [playBusy, setPlayBusy] = useState(false);
  const pollTimer = useRef<number | null>(null);

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

  async function handleChooseFile() {
    // Multi-platform native picker. WAV-only filter; users on Linux see
    // hidden files toggled off by default (Tauri delegates to the OS
    // dialog). `null` means the user cancelled — leave the existing
    // selection unchanged. A genuine OS-level failure (xdg-portal not
    // running, permission denied, etc.) throws and is surfaced into
    // the same error region as playback errors — matching the
    // try/catch convention used by handleListDevices / handlePlay /
    // handleStop above.
    try {
      const picked = await openDialog({
        multiple: false,
        directory: false,
        title: "Open WAV file",
        filters: [{ name: "WAV audio", extensions: ["wav"] }],
      });
      if (typeof picked === "string") {
        setSourcePath(picked);
        // Cancelling the picker shouldn't clear a stale "Stopped at …" line,
        // but choosing a new file should — we're starting a new playback intent.
        setPlayError(null);
      }
    } catch (e) {
      setPlayError(`file picker failed: ${e}`);
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
      // Seed status from the start-time snapshot so the UI doesn't
      // flash "0:00 / —" before the first poll fires.
      setPlayStatus({
        state: "Playing",
        position_seconds: 0,
        duration_seconds: result.duration_seconds,
      });
    } catch (e) {
      setPlayError(String(e));
      setPlayInfo(null);
    } finally {
      setPlayBusy(false);
    }
  }

  async function handlePause() {
    setPlayBusy(true);
    setPlayError(null);
    try {
      const status = await invoke<PlaybackStatus>("playback_pause");
      setPlayStatus(status);
    } catch (e) {
      setPlayError(String(e));
    } finally {
      setPlayBusy(false);
    }
  }

  async function handleResume() {
    setPlayBusy(true);
    setPlayError(null);
    try {
      const status = await invoke<PlaybackStatus>("playback_resume");
      setPlayStatus(status);
    } catch (e) {
      setPlayError(String(e));
    } finally {
      setPlayBusy(false);
    }
  }

  async function handleStop() {
    setPlayBusy(true);
    setPlayError(null);
    try {
      const result = await invoke<PlaybackStatus>("playback_stop");
      setPlayInfo(null);
      setPlayStatus(null);
      // Surface where playback actually stopped (useful if the user
      // hit Stop before the file ended).
      setPlayError(`Stopped at ${formatTime(result.position_seconds)} (${result.state})`);
    } catch (e) {
      setPlayError(String(e));
    } finally {
      setPlayBusy(false);
    }
  }

  // Poll the engine while a session is active. The actor returns
  // `None` once the handle is gone (after Stop, or after a future
  // auto-cleanup on engine error) — that's our cue to tear the timer
  // down. Natural EOF doesn't drop the handle (engine moves to
  // `Ended`); the UI surfaces the Ended state and lets the user hit
  // Stop to reclaim the slot.
  useEffect(() => {
    if (!playInfo) {
      if (pollTimer.current !== null) {
        window.clearInterval(pollTimer.current);
        pollTimer.current = null;
      }
      return;
    }
    const tick = async () => {
      try {
        const snap = await invoke<PlaybackStatus | null>("playback_status");
        if (snap === null) {
          // Actor lost the handle (e.g., engine errored). Reflect that
          // in the UI by clearing playInfo, which trips this effect's
          // teardown branch on the next render.
          setPlayInfo(null);
          setPlayStatus(null);
          return;
        }
        setPlayStatus(snap);
      } catch {
        // Transient channel hiccup — ignore one tick; if it persists,
        // the user can hit Stop manually.
      }
    };
    pollTimer.current = window.setInterval(tick, POLL_INTERVAL_MS);
    return () => {
      if (pollTimer.current !== null) {
        window.clearInterval(pollTimer.current);
        pollTimer.current = null;
      }
    };
  }, [playInfo]);

  const visible = useMemo(() => {
    if (!devices) return null;
    return showAll ? devices : devices.filter(isEssential);
  }, [devices, showAll]);

  const hiddenCount = devices && visible ? devices.length - visible.length : 0;
  const canPlay = !playBusy && !playInfo && !!selectedDeviceId && !!sourcePath.trim();
  const isPaused = playStatus?.state === "Paused";
  const isEnded = playStatus?.state === "Ended";

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
            <button
              type="button"
              onClick={handleChooseFile}
              disabled={playBusy || !!playInfo}
              className="rounded-md bg-elevated border border-border px-3 py-2 text-sm hover:border-muted disabled:opacity-50 transition"
            >
              {sourcePath ? "Change file…" : "Open WAV…"}
            </button>
            <div
              className="flex-1 rounded-md bg-elevated border border-border px-3 py-2 font-mono text-sm text-muted truncate"
              title={sourcePath || ""}
            >
              {sourcePath || "no file selected"}
            </div>
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
              <>
                {/* Pause/Resume hidden once the engine reports `Ended`
                    — natural EOF means the audio thread is done; only
                    Stop is meaningful from there (it releases the
                    session slot so Play can re-arm). */}
                {!isEnded &&
                  (isPaused ? (
                    <button
                      type="button"
                      onClick={handleResume}
                      disabled={playBusy}
                      className="rounded-md bg-elevated border border-accent px-4 py-2 text-base font-medium text-accent hover:bg-accent hover:text-black disabled:opacity-50 transition"
                    >
                      {playBusy ? "Resuming…" : "Resume"}
                    </button>
                  ) : (
                    <button
                      type="button"
                      onClick={handlePause}
                      disabled={playBusy}
                      className="rounded-md bg-elevated border border-border px-4 py-2 text-base font-medium hover:border-muted disabled:opacity-50 transition"
                    >
                      {playBusy ? "Pausing…" : "Pause"}
                    </button>
                  ))}
                <button
                  type="button"
                  onClick={handleStop}
                  disabled={playBusy}
                  className="rounded-md bg-elevated border border-accent px-4 py-2 text-base font-medium text-accent hover:bg-accent hover:text-black disabled:opacity-50 transition"
                >
                  {playBusy ? "Stopping…" : "Stop"}
                </button>
              </>
            )}
          </div>

          {playInfo && playStatus && (
            <div className="mt-3 flex items-center justify-between text-sm">
              <span className="text-muted">
                {playStatus.state} — {playInfo.sample_rate} Hz · {playInfo.channels} ch
              </span>
              <span className="font-mono tabular-nums text-fg">
                {formatTime(playStatus.position_seconds)}
                {" / "}
                {playInfo.duration_seconds !== null
                  ? formatTime(playInfo.duration_seconds)
                  : "—:—"}
              </span>
            </div>
          )}
          {playError && (
            <pre className="mt-3 text-sm text-red-400 whitespace-pre-wrap">{playError}</pre>
          )}
        </section>
      )}
    </main>
  );
}
