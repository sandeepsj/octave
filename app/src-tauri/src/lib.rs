//! Tauri shell — wires `octave_player` into the React UI via
//! `#[tauri::command]` handlers.
//!
//! v0.1 surface: one command, `list_output_devices`, mirroring the
//! recorder's MCP tool surface (the agent calls `playback_list_output_devices`
//! over stdio; the UI calls this over Tauri IPC). Same engine
//! function, two facades — per the project architecture memory.

use serde::Serialize;

/// Wire shape returned to the React side. Mirrors
/// [`octave_player::OutputDeviceInfo`] but flattens the `DeviceId`
/// newtype and stringifies the `Backend` enum so the JSON shape is
/// predictable for TypeScript consumers.
///
/// Keep this in sync with `OutputDeviceInfo` in `app/src/App.tsx`.
#[derive(Serialize)]
struct OutputDeviceInfo {
    device_id: String,
    name: String,
    backend: String,
    is_default_output: bool,
    max_output_channels: u16,
}

#[tauri::command]
fn list_output_devices() -> Vec<OutputDeviceInfo> {
    octave_player::list_output_devices()
        .into_iter()
        .map(|d| OutputDeviceInfo {
            device_id: d.id.0,
            name: d.name,
            // Backend is `pub enum { Alsa, PipeWire, Jack, CoreAudio,
            // Wasapi, Asio }` — Debug formatting is exact, stable, and
            // pattern-matchable on the JS side.
            backend: format!("{:?}", d.backend),
            is_default_output: d.is_default_output,
            max_output_channels: d.max_output_channels,
        })
        .collect()
}

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![list_output_devices])
        .run(tauri::generate_context!())
        .expect("error while running octave-app");
}
