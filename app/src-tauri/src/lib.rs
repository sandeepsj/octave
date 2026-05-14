//! Tauri shell — wires `octave_player` and `octave_recorder` into the
//! React UI via `#[tauri::command]` handlers.
//!
//! Engine surface mirrored to the UI (and, separately, to the agent
//! via `octave-engine`):
//!
//! - `list_output_devices` / `list_input_devices` — enumerate devices.
//! - `playback_start` / `playback_pause` / `playback_resume` /
//!   `playback_stop` / `playback_status` — file → device playback +
//!   transport. Backed by [`app_actor`] because `cpal::Stream` is
//!   `!Send` and `tauri::State` requires `Send + Sync`.

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

/// Symmetric to `OutputDeviceInfo` for the recorder side. Both
/// engines re-export the same `Backend` enum from
/// `octave-audio-devices`, so the wire shape (stringified backend,
/// friendly name resolved on Linux) is identical across input and
/// output `list_*_devices` responses.
///
/// Keep this in sync with `InputDeviceInfo` in `app/src/App.tsx`.
#[derive(Serialize)]
struct InputDeviceInfo {
    device_id: String,
    name: String,
    friendly_name: Option<String>,
    backend: String,
    is_default_input: bool,
    max_input_channels: u16,
}

#[tauri::command]
fn list_input_devices(actor: tauri::State<'_, AppActorHandle>) -> Vec<InputDeviceInfo> {
    let alsa_long_names = read_alsa_card_long_names();
    actor
        .catalog()
        .list_input_devices()
        .into_iter()
        .map(|d| InputDeviceInfo {
            friendly_name: alsa_friendly_name(&d.name, &alsa_long_names),
            device_id: d.id.0,
            name: d.name,
            backend: format!("{:?}", d.backend),
            is_default_input: d.is_default_input,
            max_input_channels: d.max_input_channels,
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

/// Wire shape for `playback_stop` / `playback_pause` / `playback_resume`
/// / `playback_status` — engine state name (`"Playing"`, `"Paused"`,
/// `"Stopped"`, `"Ended"`, etc.) plus the live position. UI uses these
/// to drive the Pause/Resume/Stop button states and the position
/// display ("0:42 / 1:30").
///
/// `duration_seconds` is `None` for endless sources and for the
/// post-stop snapshot when the engine no longer holds a duration.
#[derive(Serialize)]
struct PlaybackStatusResult {
    state: String,
    position_seconds: f64,
    duration_seconds: Option<f64>,
}

impl From<octave_player::PlaybackStatus> for PlaybackStatusResult {
    fn from(s: octave_player::PlaybackStatus) -> Self {
        Self {
            // Debug-format the enum — same convention as `backend` in
            // `OutputDeviceInfo`. Pattern-matchable on the JS side.
            state: format!("{:?}", s.state),
            position_seconds: s.position_seconds,
            duration_seconds: s.duration_seconds,
        }
    }
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
async fn playback_pause(
    actor: tauri::State<'_, AppActorHandle>,
) -> Result<PlaybackStatusResult, String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    actor
        .send(Command::Pause { reply: reply_tx })
        .map_err(|e| format!("{e}"))?;
    let status = reply_rx
        .await
        .map_err(|_| "audio thread dropped reply".to_string())?
        .map_err(|e| format!("{e}"))?;
    Ok(status.into())
}

#[tauri::command]
async fn playback_resume(
    actor: tauri::State<'_, AppActorHandle>,
) -> Result<PlaybackStatusResult, String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    actor
        .send(Command::Resume { reply: reply_tx })
        .map_err(|e| format!("{e}"))?;
    let status = reply_rx
        .await
        .map_err(|_| "audio thread dropped reply".to_string())?
        .map_err(|e| format!("{e}"))?;
    Ok(status.into())
}

#[tauri::command]
async fn playback_stop(
    actor: tauri::State<'_, AppActorHandle>,
) -> Result<PlaybackStatusResult, String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    actor
        .send(Command::Stop { reply: reply_tx })
        .map_err(|e| format!("{e}"))?;
    let status = reply_rx
        .await
        .map_err(|_| "audio thread dropped reply".to_string())?
        .map_err(|e| format!("{e}"))?;
    Ok(status.into())
}

/// Cheap snapshot. Returns `None` when nothing is playing — the UI
/// uses that to clean up its polling tick + button state.
#[tauri::command]
async fn playback_status(
    actor: tauri::State<'_, AppActorHandle>,
) -> Result<Option<PlaybackStatusResult>, String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    actor
        .send(Command::Status { reply: reply_tx })
        .map_err(|e| format!("{e}"))?;
    let snapshot = reply_rx
        .await
        .map_err(|_| "audio thread dropped reply".to_string())?;
    Ok(snapshot.map(Into::into))
}

/// Wire shape returned to the React side when recording starts.
/// Includes the auto-generated output path so the UI can paste it
/// straight into the Play section after Stop — closing the
/// record-then-play loop without a save dialog.
#[derive(Serialize)]
struct RecordingStartResult {
    output_path: String,
    sample_rate: u32,
    channels: u16,
}

/// Wire shape for `recording_stop`. Mirrors `octave_recorder::RecordedClip`
/// — the UI surfaces `output_path` (auto-pasted into Play) plus a
/// summary line ("Recorded 4.32s · 48000 Hz · peak -3.2 dBFS").
#[derive(Serialize)]
struct RecordedClipResult {
    output_path: String,
    sample_rate: u32,
    channels: u16,
    frame_count: u64,
    duration_seconds: f64,
    xrun_count: u32,
    /// Maximum peak across all channels in dBFS, or `null` if no
    /// channels (recorder always returns at least one in practice).
    peak_dbfs: Option<f32>,
}

impl From<octave_recorder::RecordedClip> for RecordedClipResult {
    fn from(c: octave_recorder::RecordedClip) -> Self {
        let peak_dbfs = c
            .peak_dbfs
            .iter()
            .copied()
            .fold(None, |acc, x| Some(acc.map_or(x, |a: f32| a.max(x))));
        Self {
            output_path: c.path.to_string_lossy().into_owned(),
            sample_rate: c.sample_rate,
            channels: c.channels,
            frame_count: c.frame_count,
            duration_seconds: c.duration_seconds,
            xrun_count: c.xrun_count,
            peak_dbfs,
        }
    }
}

#[derive(Serialize)]
struct RecordingStatusResult {
    state: String,
    elapsed_seconds: f64,
    xrun_count: u32,
}

/// Auto-generate a take path under the user's tmp dir. Format:
/// `octave-take-<unix-epoch-millis>.wav`. Sortable, collision-free,
/// zero formatting deps. Less human-readable than YYYYMMDD-HHMMSS
/// would be, but the user mostly identifies takes by their position
/// in the file picker (most recent at the bottom). `tmp` is the right
/// v0.1 default: auto-reclaimed on reboot, no clutter; move to a
/// permanent location later via the OS file manager.
fn generated_take_path() -> PathBuf {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("octave-take-{millis}.wav"))
}

/// Open a recording session. `mono` is the channel-mode flag: `true`
/// post-processes the resulting WAV to fold its right channel onto
/// its left, so a single-mic capture (Input 1 only on a 2-input
/// interface like the Focusrite Solo) plays through both ears on
/// stereo devices. `false` keeps the raw L/R capture.
#[tauri::command]
async fn recording_start(
    actor: tauri::State<'_, AppActorHandle>,
    device_id: String,
    mono: bool,
) -> Result<RecordingStartResult, String> {
    use octave_recorder::{BufferSize, DeviceId, RecordingSpec};

    // Probe capabilities to pick a working (sample_rate, channels) the
    // device actually supports. Mirror of the recorder's record-demo
    // logic — prefer 48 kHz stereo, fall back to whatever the device
    // reports.
    let id = DeviceId(device_id);
    let caps = actor
        .catalog()
        .input_capabilities(&id)
        .map_err(|e| format!("input_capabilities: {e}"))?;
    let sample_rate = if caps.supported_sample_rates.contains(&48_000) {
        48_000
    } else {
        caps.default_sample_rate
    };
    let channels: u16 = if caps.channels.contains(&2) { 2 } else { 1 };

    let output_path = generated_take_path();
    let spec = RecordingSpec {
        device_id: id,
        sample_rate,
        buffer_size: BufferSize::Default,
        channels,
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    actor
        .send(Command::StartRecording {
            spec,
            output_path,
            fold_to_mono: mono,
            reply: reply_tx,
        })
        .map_err(|e| format!("{e}"))?;
    let result = reply_rx
        .await
        .map_err(|_| "audio thread dropped reply".to_string())?
        .map_err(|e| format!("{e}"))?;
    Ok(RecordingStartResult {
        output_path: result.output_path.to_string_lossy().into_owned(),
        sample_rate: result.sample_rate,
        channels: result.channels,
    })
}

#[tauri::command]
async fn recording_stop(
    actor: tauri::State<'_, AppActorHandle>,
) -> Result<RecordedClipResult, String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    actor
        .send(Command::StopRecording { reply: reply_tx })
        .map_err(|e| format!("{e}"))?;
    let clip = reply_rx
        .await
        .map_err(|_| "audio thread dropped reply".to_string())?
        .map_err(|e| format!("{e}"))?;
    Ok(clip.into())
}

#[tauri::command]
async fn recording_status(
    actor: tauri::State<'_, AppActorHandle>,
) -> Result<Option<RecordingStatusResult>, String> {
    let (reply_tx, reply_rx) = oneshot::channel();
    actor
        .send(Command::RecordingStatus { reply: reply_tx })
        .map_err(|e| format!("{e}"))?;
    let snapshot = reply_rx
        .await
        .map_err(|_| "audio thread dropped reply".to_string())?;
    Ok(snapshot.map(|s| RecordingStatusResult {
        state: s.state,
        elapsed_seconds: s.elapsed_seconds,
        xrun_count: s.xrun_count,
    }))
}

pub fn run() {
    let actor = AppActorHandle::spawn().expect("failed to spawn audio actor thread");
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(actor)
        .invoke_handler(tauri::generate_handler![
            list_output_devices,
            list_input_devices,
            playback_start,
            playback_pause,
            playback_resume,
            playback_stop,
            playback_status,
            recording_start,
            recording_stop,
            recording_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running octave-app");
}
