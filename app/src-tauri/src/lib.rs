//! Tauri shell — wires `octave_player` into the React UI via
//! `#[tauri::command]` handlers.
//!
//! Engine surface mirrored to the UI (and, separately, to the agent
//! via `octave-mcp`):
//!
//! - `list_output_devices` — enumerate output devices.
//! - `playback_start` / `playback_stop` — open a WAV file and play it
//!   through a chosen device; stop it. Backed by [`app_actor`] because
//!   `cpal::Stream` is `!Send` and `tauri::State` requires `Send + Sync`.

mod app_actor;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;
use tokio::sync::oneshot;

use app_actor::{AppActorHandle, Command};

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
fn list_output_devices(actor: tauri::State<'_, AppActorHandle>) -> Vec<OutputDeviceInfo> {
    let alsa_long_names = read_alsa_card_long_names();
    actor
        .catalog()
        .list_output_devices()
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

/// Wire shape for `playback_start` — what we hand the React side
/// after the engine has accepted the open + the audio stream is live.
/// `duration_seconds` is `None` for endless sources (in v0.1 only the
/// `Buffer` source variant is endless; `File` always reports a
/// duration).
#[derive(Serialize)]
struct PlaybackStartResult {
    duration_seconds: Option<f64>,
    sample_rate: u32,
    channels: u16,
}

/// Wire shape for `playback_stop` — final position the audio thread
/// reached, plus the engine state name (`"Stopped"`, `"Ended"`, etc.).
#[derive(Serialize)]
struct PlaybackStopResult {
    state: String,
    position_seconds: f64,
}

#[tauri::command]
async fn playback_start(
    actor: tauri::State<'_, AppActorHandle>,
    device_id: String,
    source_path: String,
) -> Result<PlaybackStartResult, String> {
    use octave_player::{BufferSize, DeviceId, PlaybackSourceSpec, PlaybackSpec};

    let spec = PlaybackSpec {
        device_id: DeviceId(device_id),
        source: PlaybackSourceSpec::File {
            path: PathBuf::from(source_path),
        },
        // The engine picks a sensible buffer for the device — UI doesn't
        // expose tuning yet; agents can dial it via the MCP surface.
        buffer_size: BufferSize::Default,
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    actor
        .send(Command::Start {
            spec,
            reply: reply_tx,
        })
        .map_err(|e| format!("{e}"))?;
    let result = reply_rx.await.map_err(|_| "audio thread dropped reply".to_string())?;
    let r = result.map_err(|e| format!("{e}"))?;
    Ok(PlaybackStartResult {
        duration_seconds: r.duration_seconds,
        sample_rate: r.sample_rate,
        channels: r.channels,
    })
}

#[tauri::command]
async fn playback_stop(
    actor: tauri::State<'_, AppActorHandle>,
) -> Result<PlaybackStopResult, String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    actor
        .send(Command::Stop { reply: reply_tx })
        .map_err(|e| format!("{e}"))?;
    let status = reply_rx
        .await
        .map_err(|_| "audio thread dropped reply".to_string())?
        .map_err(|e| format!("{e}"))?;
    Ok(PlaybackStopResult {
        // Debug-format the enum — same convention as `backend` in
        // `OutputDeviceInfo`. Pattern-matchable on the JS side.
        state: format!("{:?}", status.state),
        position_seconds: status.position_seconds,
    })
}

pub fn run() {
    let actor = AppActorHandle::spawn().expect("failed to spawn audio actor thread");
    tauri::Builder::default()
        .manage(actor)
        .invoke_handler(tauri::generate_handler![
            list_output_devices,
            playback_start,
            playback_stop,
        ])
        .run(tauri::generate_context!())
        .expect("error while running octave-app");
}
