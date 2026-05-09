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

  async function handleListDevices() {
    setLoading(true);
    setError(null);
    try {
      const result = await invoke<OutputDeviceInfo[]>("list_output_devices");
      setDevices(result);
    } catch (e) {
      setError(String(e));
      setDevices(null);
    } finally {
      setLoading(false);
    }
  }

  const visible = useMemo(() => {
    if (!devices) return null;
    return showAll ? devices : devices.filter(isEssential);
  }, [devices, showAll]);

  const hiddenCount = devices && visible ? devices.length - visible.length : 0;

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
            {visible.map((d) => (
              <li
                key={d.device_id}
                className="rounded-md bg-elevated border border-border px-4 py-3"
              >
                <div className="flex items-center gap-2">
                  <span className="font-medium">{prettify(d)}</span>
                  {d.is_default_output && (
                    <span className="rounded bg-accent/20 px-1.5 py-0.5 text-xs font-medium text-accent">
                      DEFAULT
                    </span>
                  )}
                </div>
                <div className="text-sm text-muted mt-0.5 font-mono">
                  {d.backend.toLowerCase()} · max {d.max_output_channels} ch · {d.name}
                </div>
              </li>
            ))}
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
    </main>
  );
}
