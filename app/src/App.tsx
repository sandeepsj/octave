import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

/// Fields shared between input and output device info — used by the
/// `isEssential` filter and the `prettify` rewriter. Both Tauri
/// commands (`list_output_devices`, `list_input_devices`) return
/// objects compatible with this shape.
interface DeviceCommon {
  name: string;
  /// Linux-only: the human-readable name from /proc/asound/cards
  /// (e.g. "Focusrite Scarlett Solo USB"). Other platforms always
  /// null — Core Audio / WASAPI hand the friendly name in `name`.
  friendly_name: string | null;
  backend: string;
}

/// Mirror of the Tauri command's return shape (defined in
/// app/src-tauri/src/lib.rs — keep in sync).
interface OutputDeviceInfo extends DeviceCommon {
  device_id: string;
  is_default_output: boolean;
  max_output_channels: number;
}

interface InputDeviceInfo extends DeviceCommon {
  device_id: string;
  is_default_input: boolean;
  max_input_channels: number;
}

interface PlaybackStartResult {
  duration_seconds: number | null;
  sample_rate: number;
  channels: number;
}

interface RecordingStartResult {
  output_path: string;
  sample_rate: number;
  channels: number;
}

interface RecordedClip {
  output_path: string;
  sample_rate: number;
  channels: number;
  frame_count: number;
  duration_seconds: number;
  xrun_count: number;
  peak_dbfs: number | null;
}

interface RecordingStatus {
  state: string;
  elapsed_seconds: number;
  xrun_count: number;
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

function isEssential(d: DeviceCommon): boolean {
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
function prettify(d: DeviceCommon): string {
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

  // Input-side device list — fetched separately because the user may
  // want to record without listing outputs first. State mirror of the
  // output-list state above.
  const [inputs, setInputs] = useState<InputDeviceInfo[] | null>(null);
  const [inputError, setInputError] = useState<string | null>(null);
  const [inputLoading, setInputLoading] = useState(false);
  const [showAllInputs, setShowAllInputs] = useState(false);
  const [selectedInputDeviceId, setSelectedInputDeviceId] = useState<string | null>(null);

  // Recording session state. `recInfo` is the start-time snapshot
  // (sample rate / channels / output path); `recStatus` is the live
  // engine snapshot from the 200 ms polling tick. `recordedClip` is
  // the final summary surfaced after Stop, with the auto-paste hook
  // into the Play section.
  const [recInfo, setRecInfo] = useState<RecordingStartResult | null>(null);
  const [recStatus, setRecStatus] = useState<RecordingStatus | null>(null);
  const [recordedClip, setRecordedClip] = useState<RecordedClip | null>(null);
  const [recError, setRecError] = useState<string | null>(null);
  const [recBusy, setRecBusy] = useState(false);
  // Default Mono — covers the common "one mic into Input 1 of a
  // 2-input interface" case. The recorder always opens whatever the
  // device exposes (typically stereo); when Mono is selected, the
  // post-stop WAV is folded so R = L for both-ear playback.
  const [recordMode, setRecordMode] = useState<"mono" | "stereo">("mono");
  const recPollTimer = useRef<number | null>(null);

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

  async function handleListInputs() {
    setInputLoading(true);
    setInputError(null);
    try {
      const result = await invoke<InputDeviceInfo[]>("list_input_devices");
      setInputs(result);
      const def = result.find((d) => d.is_default_input);
      if (def && !selectedInputDeviceId) setSelectedInputDeviceId(def.device_id);
    } catch (e) {
      setInputError(String(e));
      setInputs(null);
    } finally {
      setInputLoading(false);
    }
  }

  async function handleRecord() {
    if (!selectedInputDeviceId) return;
    setRecBusy(true);
    setRecError(null);
    setRecordedClip(null);
    try {
      const result = await invoke<RecordingStartResult>("recording_start", {
        deviceId: selectedInputDeviceId,
        mono: recordMode === "mono",
      });
      setRecInfo(result);
      setRecStatus({ state: "Recording", elapsed_seconds: 0, xrun_count: 0 });
    } catch (e) {
      setRecError(String(e));
      setRecInfo(null);
    } finally {
      setRecBusy(false);
    }
  }

  async function handleStopRecording() {
    setRecBusy(true);
    setRecError(null);
    try {
      const clip = await invoke<RecordedClip>("recording_stop");
      setRecInfo(null);
      setRecStatus(null);
      setRecordedClip(clip);
      // Close the loop: the just-recorded WAV becomes the next thing
      // the user can play. Pre-fills the Play row's path so a single
      // click on Play sends the take to the selected output device.
      setSourcePath(clip.output_path);
    } catch (e) {
      setRecError(String(e));
    } finally {
      setRecBusy(false);
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

  // Symmetric polling for the recorder side — see playback's effect
  // above for the contract. Returns-null teardown matches.
  useEffect(() => {
    if (!recInfo) {
      if (recPollTimer.current !== null) {
        window.clearInterval(recPollTimer.current);
        recPollTimer.current = null;
      }
      return;
    }
    const tick = async () => {
      try {
        const snap = await invoke<RecordingStatus | null>("recording_status");
        if (snap === null) {
          setRecInfo(null);
          setRecStatus(null);
          return;
        }
        setRecStatus(snap);
      } catch {
        // Transient; ignore one tick.
      }
    };
    recPollTimer.current = window.setInterval(tick, POLL_INTERVAL_MS);
    return () => {
      if (recPollTimer.current !== null) {
        window.clearInterval(recPollTimer.current);
        recPollTimer.current = null;
      }
    };
  }, [recInfo]);

  const visible = useMemo(() => {
    if (!devices) return null;
    return showAll ? devices : devices.filter(isEssential);
  }, [devices, showAll]);

  const visibleInputs = useMemo(() => {
    if (!inputs) return null;
    return showAllInputs ? inputs : inputs.filter(isEssential);
  }, [inputs, showAllInputs]);

  const hiddenCount = devices && visible ? devices.length - visible.length : 0;
  const hiddenInputCount =
    inputs && visibleInputs ? inputs.length - visibleInputs.length : 0;
  const canPlay = !playBusy && !playInfo && !!selectedDeviceId && !!sourcePath.trim();
  const isPaused = playStatus?.state === "Paused";
  const isEnded = playStatus?.state === "Ended";

  return (
    <main className="min-h-screen p-8 max-w-2xl mx-auto">
      <header className="mb-8 flex items-start justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Octave</h1>
          <p className="text-muted text-sm mt-1">v0.1 — scaffold</p>
        </div>
        <button
          type="button"
          onClick={() => {
            invoke("open_chat_window").catch((e) =>
              setError(`open_chat_window: ${e}`),
            );
          }}
          className="rounded-md bg-elevated border border-border px-3 py-2 text-sm hover:border-accent transition"
          title="Open the in-app AI chat (Haiku 4.5 with tool access)"
        >
          💬 Chat
        </button>
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

      <section className="mt-12 border-t border-border pt-8">
        <h2 className="text-lg font-semibold mb-3">Inputs</h2>
        <button
          type="button"
          onClick={handleListInputs}
          disabled={inputLoading}
          className="rounded-md bg-accent px-4 py-2 text-base font-medium text-black hover:bg-accent-hover disabled:opacity-50 transition"
        >
          {inputLoading ? "Loading…" : "List Input Devices"}
        </button>

        {inputError && (
          <pre className="mt-4 text-red-400 text-sm whitespace-pre-wrap">{inputError}</pre>
        )}

        {visibleInputs && visibleInputs.length === 0 && (
          <p className="mt-6 text-muted">No input devices found.</p>
        )}

        {visibleInputs && visibleInputs.length > 0 && (
          <>
            <ul className="mt-6 space-y-2">
              {visibleInputs.map((d) => {
                const isSelected = d.device_id === selectedInputDeviceId;
                return (
                  <li key={d.device_id}>
                    <button
                      type="button"
                      onClick={() => setSelectedInputDeviceId(d.device_id)}
                      className={`w-full text-left rounded-md border px-4 py-3 transition ${
                        isSelected
                          ? "bg-elevated border-accent"
                          : "bg-elevated border-border hover:border-muted"
                      }`}
                    >
                      <div className="flex items-center gap-2">
                        <span className="font-medium">{prettify(d)}</span>
                        {d.is_default_input && (
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
                        {d.backend.toLowerCase()} · max {d.max_input_channels} ch · {d.name}
                      </div>
                    </button>
                  </li>
                );
              })}
            </ul>

            {!showAllInputs && hiddenInputCount > 0 && (
              <button
                type="button"
                onClick={() => setShowAllInputs(true)}
                className="mt-4 text-sm text-muted hover:text-fg underline-offset-2 hover:underline"
              >
                Show {hiddenInputCount} more (ALSA plug devices)
              </button>
            )}
            {showAllInputs && (
              <button
                type="button"
                onClick={() => setShowAllInputs(false)}
                className="mt-4 text-sm text-muted hover:text-fg underline-offset-2 hover:underline"
              >
                Hide ALSA plug devices
              </button>
            )}
          </>
        )}

        {selectedInputDeviceId && (
          <div className="mt-6">
            <div className="flex items-center gap-3">
              {/* Mono / Stereo segmented control. Hidden mid-recording
                  so the user can't change capture intent partway. */}
              {!recInfo && (
                <div className="inline-flex rounded-md border border-border overflow-hidden text-sm">
                  <button
                    type="button"
                    onClick={() => setRecordMode("mono")}
                    disabled={recBusy}
                    className={`px-3 py-2 transition ${
                      recordMode === "mono"
                        ? "bg-accent text-black font-medium"
                        : "bg-elevated text-muted hover:text-fg"
                    }`}
                    title="Single mic into Input 1 — fold capture so both ears hear it on playback."
                  >
                    Mono
                  </button>
                  <button
                    type="button"
                    onClick={() => setRecordMode("stereo")}
                    disabled={recBusy}
                    className={`px-3 py-2 transition border-l border-border ${
                      recordMode === "stereo"
                        ? "bg-accent text-black font-medium"
                        : "bg-elevated text-muted hover:text-fg"
                    }`}
                    title="True stereo — Input 1 → L, Input 2 → R."
                  >
                    Stereo
                  </button>
                </div>
              )}
              {!recInfo ? (
                <button
                  type="button"
                  onClick={handleRecord}
                  disabled={recBusy}
                  className="rounded-md bg-red-600 px-4 py-2 text-base font-medium text-white hover:bg-red-500 disabled:opacity-50 transition"
                >
                  {recBusy ? "Starting…" : "● Record"}
                </button>
              ) : (
                <button
                  type="button"
                  onClick={handleStopRecording}
                  disabled={recBusy}
                  className="rounded-md bg-elevated border border-red-500 px-4 py-2 text-base font-medium text-red-400 hover:bg-red-600 hover:text-white disabled:opacity-50 transition"
                >
                  {recBusy ? "Stopping…" : "■ Stop"}
                </button>
              )}
              {recInfo && recStatus && (
                <span className="font-mono tabular-nums text-fg">
                  ● {formatTime(recStatus.elapsed_seconds)}
                  {recStatus.xrun_count > 0 && (
                    <span className="ml-3 text-yellow-400">
                      xruns: {recStatus.xrun_count}
                    </span>
                  )}
                </span>
              )}
            </div>
            {recInfo && (
              <p className="mt-2 text-sm text-muted">
                {recInfo.sample_rate} Hz · {recInfo.channels} ch → {recInfo.output_path}
              </p>
            )}
            {recordedClip && (
              <div className="mt-3 rounded-md bg-elevated border border-border px-4 py-3 text-sm">
                <div className="flex items-baseline justify-between">
                  <span className="text-fg">
                    Recorded {formatTime(recordedClip.duration_seconds)} ·{" "}
                    {recordedClip.sample_rate} Hz · {recordedClip.channels} ch
                  </span>
                  {recordedClip.peak_dbfs !== null && (
                    <span className="text-muted font-mono">
                      peak {recordedClip.peak_dbfs.toFixed(1)} dBFS
                    </span>
                  )}
                </div>
                <div className="mt-1 text-muted font-mono text-xs truncate" title={recordedClip.output_path}>
                  {recordedClip.output_path}
                </div>
                <div className="mt-2 text-xs text-accent">
                  ↑ Auto-loaded into Play — pick an output device and hit Play.
                </div>
              </div>
            )}
            {recError && (
              <pre className="mt-3 text-sm text-red-400 whitespace-pre-wrap">{recError}</pre>
            )}
          </div>
        )}
      </section>
    </main>
  );
}
