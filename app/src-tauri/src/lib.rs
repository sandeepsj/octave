//! Tauri shell — wires `octave_player` into the React UI via
//! `#[tauri::command]` handlers.
//!
//! v0.1 surface: one command, `list_output_devices`, mirroring the
//! recorder's MCP tool surface (the agent calls `playback_list_output_devices`
//! over stdio; the UI calls this over Tauri IPC). Same engine
//! function, two facades — per the project architecture memory.

use std::collections::HashMap;

use serde::Serialize;

/// Wire shape returned to the React side. Mirrors
/// [`octave_player::OutputDeviceInfo`] but flattens the `DeviceId`
/// newtype and stringifies the `Backend` enum so the JSON shape is
/// predictable for TypeScript consumers.
///
/// `friendly_name` is the human-readable name from
/// `/proc/asound/cards` on Linux ("Focusrite Scarlett Solo USB"),
/// `None` when we couldn't resolve it. Other platforms always None
/// for now — Core Audio / WASAPI hand us the friendly name in
/// `name` directly.
///
/// Keep this in sync with `OutputDeviceInfo` in `app/src/App.tsx`.
#[derive(Serialize)]
struct OutputDeviceInfo {
    device_id: String,
    name: String,
    friendly_name: Option<String>,
    backend: String,
    is_default_output: bool,
    max_output_channels: u16,
}

#[tauri::command]
fn list_output_devices() -> Vec<OutputDeviceInfo> {
    let alsa_long_names = read_alsa_card_long_names();
    octave_player::list_output_devices()
        .into_iter()
        .map(|d| OutputDeviceInfo {
            friendly_name: alsa_friendly_name(&d.name, &alsa_long_names),
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

/// Read `/proc/asound/cards` and extract `short_name -> long_name`
/// for each card. Format (kernel-controlled, stable for ~20 years):
///
/// ```text
///  2 [USB            ]: USB-Audio - Scarlett Solo USB
///                       Focusrite Scarlett Solo USB at usb-...
/// ```
///
/// We want `USB -> "Focusrite Scarlett Solo USB"` (the second line,
/// before the "at <location>" suffix). The first line's "Scarlett
/// Solo USB" is also useful but the second line carries the
/// manufacturer string from the USB descriptor — what other Linux
/// audio apps show.
///
/// Empty map on non-Linux or when `/proc/asound/cards` is missing.
fn read_alsa_card_long_names() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(contents) = std::fs::read_to_string("/proc/asound/cards") else {
        return map;
    };

    let mut lines = contents.lines();
    while let Some(header) = lines.next() {
        // " 2 [USB            ]: USB-Audio - Scarlett Solo USB"
        let Some(open) = header.find('[') else { continue };
        let Some(close) = header.find(']') else { continue };
        if close <= open + 1 {
            continue;
        }
        let short_name = header[open + 1..close].trim().to_string();

        // Long name lives on the next line, before " at <location>".
        let Some(detail) = lines.next() else { continue };
        let detail = detail.trim();
        let long_name = detail
            .rsplit_once(" at ")
            .map(|(name, _)| name.trim())
            .unwrap_or(detail);
        if !short_name.is_empty() && !long_name.is_empty() {
            map.insert(short_name, long_name.to_string());
        }
    }
    map
}

/// Resolve the human-readable name for one cpal-returned ALSA
/// device name. We only enrich the `hw:CARD=X,DEV=Y` shape — the
/// short-name `X` slot maps into `read_alsa_card_long_names`.
/// `default`, `pipewire`, plug-layer names (`dmix`, `surround*`,
/// etc.) keep their raw form.
fn alsa_friendly_name(name: &str, cards: &HashMap<String, String>) -> Option<String> {
    let prefix = name.find("CARD=").map(|i| i + "CARD=".len())?;
    let after = &name[prefix..];
    let card = after.split(',').next()?;
    cards.get(card).cloned()
}

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![list_output_devices])
        .run(tauri::generate_context!())
        .expect("error while running octave-app");
}
