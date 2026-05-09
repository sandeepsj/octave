import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/// Mirror of the Tauri command's return shape (defined in
/// app/src-tauri/src/lib.rs — keep in sync).
interface OutputDeviceInfo {
  device_id: string;
  name: string;
  backend: string;
  is_default_output: boolean;
  max_output_channels: number;
}

export default function App() {
  const [devices, setDevices] = useState<OutputDeviceInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

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

      {devices && devices.length === 0 && (
        <p className="mt-6 text-muted">No output devices found.</p>
      )}

      {devices && devices.length > 0 && (
        <ul className="mt-6 space-y-2">
          {devices.map((d) => (
            <li
              key={d.device_id}
              className="rounded-md bg-elevated border border-border px-4 py-3"
            >
              <div className="flex items-center gap-2">
                <span className="font-medium">{d.name}</span>
                {d.is_default_output && (
                  <span className="rounded bg-accent/20 px-1.5 py-0.5 text-xs font-medium text-accent">
                    DEFAULT
                  </span>
                )}
              </div>
              <div className="text-sm text-muted mt-0.5">
                {d.backend} · max {d.max_output_channels} ch
              </div>
            </li>
          ))}
        </ul>
      )}
    </main>
  );
}
