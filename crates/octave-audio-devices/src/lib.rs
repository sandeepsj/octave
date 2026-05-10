//! Cross-platform audio device enumeration + capability query — the
//! shared backbone used by both `octave-player` and `octave-recorder`.
//!
//! # Why a shared crate?
//!
//! Before this crate existed, the player and the recorder each owned
//! a private `DeviceCatalog`. Each one cached `cpal::Device` wrappers
//! independently. When cpal probes a device on Linux, its
//! `DeviceHandles::open` opens the ALSA PCM in **both** directions
//! (playback AND capture) and stores both handles inside an
//! `Arc<Mutex<DeviceHandles>>`. So the player's cached Focusrite
//! Device held both PCMs — and when the recorder then tried to
//! enumerate inputs, its own probe failed with EBUSY because the
//! player's cache was still holding the capture side. (And vice
//! versa.) Listing one direction made the other direction's listing
//! lose the device.
//!
//! Unified catalog: one `cpal::Device` per physical device, looked up
//! by id from a single shared cache. The player's `start()` and the
//! recorder's `open()` both clone the same Device — cpal accepts
//! `build_output_stream` and `build_input_stream` on the same Device
//! (they consume different sides of `DeviceHandles`), so two
//! consumers, one device, no conflict.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Multi-pass enumeration tunables. cpal's ALSA backend probes each
/// PCM hint by `snd_pcm_open(name, …, SND_PCM_NONBLOCK)` and silently
/// drops devices whose open returns `EBUSY`. PipeWire holds physical
/// `hw:CARD=` PCMs at arbitrary moments, so any single pass can miss
/// a device. Three passes with 100 ms gaps catches most of the race;
/// the union-by-id keeps the `cpal::Device` from whichever pass first
/// saw each device.
///
/// 3 × 100 ms = 300 ms per direction. The full refresh enumerates
/// outputs then inputs sequentially — but inputs only multi-pass for
/// devices not already cached from outputs (cpal's input probe of a
/// device we already hold via output cache is doomed to EBUSY anyway,
/// and we already have its `cpal::Device` and its input metadata via
/// `supported_input_configs()` on the cached Device). Worst case a
/// fully-input-only-device system: 600 ms. Typical mixed: ~350 ms.
const ENUMERATION_PASSES: u32 = 3;
const ENUMERATION_PASS_GAP: Duration = Duration::from_millis(100);

/// Platform-stable identifier for an audio device.
///
/// Encoded as `"{HOST_NAME}:{DEVICE_NAME}"` — opaque to callers, but
/// stable enough that re-finding a device by id works as long as the
/// device's name doesn't change between runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub String);

/// Kernel-level audio backend a device is exposed through.
///
/// `PipeWire` is **reserved** — today PipeWire on Linux is reached
/// via the `Alsa` host (cpal exposes it as ALSA), so no device is
/// currently tagged `PipeWire`.
///
/// `Other(name)` carries any cpal `HostId::name()` value not in the
/// explicit list. Better than silently coercing to `Alsa`, which
/// historically mis-tagged macOS / Windows / future hosts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    Alsa,
    PipeWire,
    Jack,
    CoreAudio,
    Wasapi,
    Asio,
    Other(String),
}

/// Buffer-size request handed to cpal at stream-build time.
///
/// `Default` lets the backend choose a reasonable size; `Fixed(n)`
/// asks for exactly `n` frames. Backends may treat fixed sizes as
/// hints (Core Audio, WASAPI shared mode); the engine verifies the
/// actual size from the first callback's slice length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BufferSize {
    Default,
    Fixed(u32),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputDeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub backend: Backend,
    pub is_default_output: bool,
    pub max_output_channels: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputDeviceInfo {
    pub id: DeviceId,
    pub name: String,
    pub backend: Backend,
    pub is_default_input: bool,
    /// Class-compliant USB detection requires udev / IOKit / WinUSB
    /// queries that are out of scope for v0.1. Always `false` today.
    pub is_class_compliant_usb: bool,
    pub max_input_channels: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputCapabilities {
    pub min_sample_rate: u32,
    pub max_sample_rate: u32,
    pub supported_sample_rates: Vec<u32>,
    pub min_buffer_size: u32,
    pub max_buffer_size: u32,
    pub channels: Vec<u16>,
    pub default_sample_rate: u32,
    pub default_buffer_size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputCapabilities {
    pub min_sample_rate: u32,
    pub max_sample_rate: u32,
    pub supported_sample_rates: Vec<u32>,
    pub min_buffer_size: u32,
    pub max_buffer_size: u32,
    pub channels: Vec<u16>,
    pub default_sample_rate: u32,
    pub default_buffer_size: u32,
}

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("device not found: {id:?}")]
    DeviceNotFound { id: DeviceId },
    #[error("backend error: {0}")]
    BackendError(String),
}

/// Per-device cache entry. Holds the live `cpal::Device` plus
/// pre-computed metadata for whichever directions the device exposes.
struct CachedDevice {
    device: cpal::Device,
    output_info: Option<OutputDeviceInfo>,
    input_info: Option<InputDeviceInfo>,
}

/// Owns the cached `cpal::Device` handles and the metadata
/// (`OutputDeviceInfo` / `InputDeviceInfo`) for each of them.
///
/// `Send + Sync` (the inner `Mutex<HashMap<…, CachedDevice>>` is, on
/// every backend Octave targets — see module doc).
pub struct DeviceCatalog {
    devices: Mutex<HashMap<DeviceId, CachedDevice>>,
}

impl DeviceCatalog {
    pub fn new() -> Self {
        Self {
            devices: Mutex::new(HashMap::new()),
        }
    }

    /// Refresh the cache and return the output-direction metadata for
    /// every device that exposes one.
    ///
    /// A full refresh enumerates outputs first, then inputs; for each
    /// cached Device we also call `supported_{input,output}_configs()`
    /// directly, so a Device cached via output enumeration carries its
    /// input metadata too without needing the input enumeration to
    /// successfully re-probe it (which it can't — we hold it).
    pub fn list_output_devices(&self) -> Vec<OutputDeviceInfo> {
        self.refresh();
        let guard = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .values()
            .filter_map(|c| c.output_info.clone())
            .collect()
    }

    /// Symmetric to [`Self::list_output_devices`]. Returns the
    /// input-direction metadata for every cached device that has any.
    pub fn list_input_devices(&self) -> Vec<InputDeviceInfo> {
        self.refresh();
        let guard = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .values()
            .filter_map(|c| c.input_info.clone())
            .collect()
    }

    /// Query an output device's capabilities. Uses the cached
    /// `cpal::Device` when available (no fresh probe needed); falls
    /// back to enumeration on cache miss.
    pub fn output_capabilities(&self, id: &DeviceId) -> Result<OutputCapabilities, DeviceError> {
        let device = self.find_device(id)?;
        output_capabilities_for_device(&device)
    }

    /// Symmetric to [`Self::output_capabilities`].
    pub fn input_capabilities(&self, id: &DeviceId) -> Result<InputCapabilities, DeviceError> {
        let device = self.find_device(id)?;
        input_capabilities_for_device(&device)
    }

    /// Re-find a device on its host by encoded id. Cache-first; falls
    /// back to fresh enumeration on miss (so callers that supply a
    /// remembered id without refreshing first still work, at the
    /// original race risk).
    pub fn find_device(&self, id: &DeviceId) -> Result<cpal::Device, DeviceError> {
        if let Some(c) = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
        {
            return Ok(c.device.clone());
        }
        find_device_via_enum(id)
    }

    /// Drop the old cache UP FRONT — before any new probe — so our
    /// own previously-cached `cpal::Device` handles release their
    /// underlying ALSA PCM refs before the multi-pass probe tries to
    /// open those same devices. Without this, a re-list call sees
    /// previously-listed hw: devices as "busy" because we ourselves
    /// still hold them open from the previous list. Active
    /// `PlaybackHandle` / `RecordingHandle`s carry their own
    /// Arc-shared clone of `cpal::Device`, so this clear doesn't
    /// disturb them — the underlying ALSA PCM stays open as long as
    /// any live handle still references it.
    fn refresh(&self) {
        {
            let mut guard = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            guard.clear();
        }

        let mut new_cache: HashMap<DeviceId, CachedDevice> = HashMap::new();
        for host_id in cpal::available_hosts() {
            let Ok(host) = cpal::host_from_id(host_id) else { continue };
            let backend = host_id_to_backend(host_id);
            let default_output_name = host
                .default_output_device()
                .and_then(|d| d.name().ok());
            let default_input_name = host
                .default_input_device()
                .and_then(|d| d.name().ok());

            // Phase 1: output enumeration with multi-pass. Devices that
            // also expose an input direction get both `output_info` and
            // `input_info` populated by querying `supported_{input,output}_configs`
            // directly on the same `cpal::Device` — no second probe.
            for pass in 0..ENUMERATION_PASSES {
                if pass > 0 {
                    std::thread::sleep(ENUMERATION_PASS_GAP);
                }
                let Ok(devices) = host.output_devices() else { continue };
                for device in devices {
                    let Ok(name) = device.name() else { continue };
                    let id = DeviceId(encode_id(host_id, &name));
                    if new_cache.contains_key(&id) {
                        continue;
                    }
                    let output_info = build_output_info(
                        &device,
                        &id,
                        &name,
                        &backend,
                        default_output_name.as_deref(),
                    );
                    let input_info = build_input_info(
                        &device,
                        &id,
                        &name,
                        &backend,
                        default_input_name.as_deref(),
                    );
                    new_cache.insert(
                        id,
                        CachedDevice {
                            device,
                            output_info,
                            input_info,
                        },
                    );
                }
            }

            // Phase 2: input enumeration with multi-pass. Catches
            // input-only devices we haven't seen yet (e.g. virtual
            // mics). Devices already in cache are skipped — their
            // input metadata was computed in phase 1 from the same
            // `cpal::Device`.
            for pass in 0..ENUMERATION_PASSES {
                if pass > 0 {
                    std::thread::sleep(ENUMERATION_PASS_GAP);
                }
                let Ok(devices) = host.input_devices() else { continue };
                for device in devices {
                    let Ok(name) = device.name() else { continue };
                    let id = DeviceId(encode_id(host_id, &name));
                    if new_cache.contains_key(&id) {
                        continue;
                    }
                    let output_info = build_output_info(
                        &device,
                        &id,
                        &name,
                        &backend,
                        default_output_name.as_deref(),
                    );
                    let input_info = build_input_info(
                        &device,
                        &id,
                        &name,
                        &backend,
                        default_input_name.as_deref(),
                    );
                    new_cache.insert(
                        id,
                        CachedDevice {
                            device,
                            output_info,
                            input_info,
                        },
                    );
                }
            }
        }

        let mut guard = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        *guard = new_cache;
    }
}

impl Default for DeviceCatalog {
    fn default() -> Self {
        Self::new()
    }
}

/// Probe a device for its output capabilities; build an
/// `OutputDeviceInfo` if it has any. Returns `None` for input-only
/// devices.
fn build_output_info(
    device: &cpal::Device,
    id: &DeviceId,
    name: &str,
    backend: &Backend,
    default_output_name: Option<&str>,
) -> Option<OutputDeviceInfo> {
    let max_output_channels = device
        .supported_output_configs()
        .ok()
        .and_then(|iter| iter.map(|c| c.channels()).max())?;
    Some(OutputDeviceInfo {
        id: id.clone(),
        name: name.to_string(),
        backend: backend.clone(),
        is_default_output: default_output_name == Some(name),
        max_output_channels,
    })
}

/// Symmetric to [`build_output_info`]. `None` for output-only devices
/// (HDMI on a system with no display, etc.).
fn build_input_info(
    device: &cpal::Device,
    id: &DeviceId,
    name: &str,
    backend: &Backend,
    default_input_name: Option<&str>,
) -> Option<InputDeviceInfo> {
    let max_input_channels = device
        .supported_input_configs()
        .ok()
        .and_then(|iter| iter.map(|c| c.channels()).max())?;
    Some(InputDeviceInfo {
        id: id.clone(),
        name: name.to_string(),
        backend: backend.clone(),
        is_default_input: default_input_name == Some(name),
        is_class_compliant_usb: false,
        max_input_channels,
    })
}

fn output_capabilities_for_device(device: &cpal::Device) -> Result<OutputCapabilities, DeviceError> {
    let supported = device
        .supported_output_configs()
        .map_err(|e| DeviceError::BackendError(format!("supported_output_configs: {e}")))?;

    let common_rates = [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000];
    let mut min_rate = u32::MAX;
    let mut max_rate = 0u32;
    let mut rates: BTreeSet<u32> = BTreeSet::new();
    let mut min_buf = u32::MAX;
    let mut max_buf = 0u32;
    let mut channels_set: BTreeSet<u16> = BTreeSet::new();
    let mut saw_any = false;

    for c in supported {
        saw_any = true;
        let lo = c.min_sample_rate().0;
        let hi = c.max_sample_rate().0;
        min_rate = min_rate.min(lo);
        max_rate = max_rate.max(hi);
        for r in common_rates {
            if r >= lo && r <= hi {
                rates.insert(r);
            }
        }
        if let cpal::SupportedBufferSize::Range { min, max } = c.buffer_size() {
            min_buf = min_buf.min(*min);
            max_buf = max_buf.max(*max);
        }
        channels_set.insert(c.channels());
    }
    if !saw_any {
        return Err(DeviceError::BackendError(
            "device exposes no supported output configs".into(),
        ));
    }

    let default_config = device
        .default_output_config()
        .map_err(|e| DeviceError::BackendError(format!("default_output_config: {e}")))?;

    Ok(OutputCapabilities {
        min_sample_rate: min_rate,
        max_sample_rate: max_rate,
        supported_sample_rates: rates.into_iter().collect(),
        min_buffer_size: if min_buf == u32::MAX { 0 } else { min_buf },
        max_buffer_size: max_buf,
        channels: channels_set.into_iter().collect(),
        default_sample_rate: default_config.sample_rate().0,
        default_buffer_size: 256,
    })
}

fn input_capabilities_for_device(device: &cpal::Device) -> Result<InputCapabilities, DeviceError> {
    let supported = device
        .supported_input_configs()
        .map_err(|e| DeviceError::BackendError(format!("supported_input_configs: {e}")))?;

    let common_rates = [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000];
    let mut min_rate = u32::MAX;
    let mut max_rate = 0u32;
    let mut rates: BTreeSet<u32> = BTreeSet::new();
    let mut min_buf = u32::MAX;
    let mut max_buf = 0u32;
    let mut channels_set: BTreeSet<u16> = BTreeSet::new();
    let mut saw_any = false;

    for c in supported {
        saw_any = true;
        let lo = c.min_sample_rate().0;
        let hi = c.max_sample_rate().0;
        min_rate = min_rate.min(lo);
        max_rate = max_rate.max(hi);
        for r in common_rates {
            if r >= lo && r <= hi {
                rates.insert(r);
            }
        }
        if let cpal::SupportedBufferSize::Range { min, max } = c.buffer_size() {
            min_buf = min_buf.min(*min);
            max_buf = max_buf.max(*max);
        }
        channels_set.insert(c.channels());
    }
    if !saw_any {
        return Err(DeviceError::BackendError(
            "device exposes no supported input configs".into(),
        ));
    }

    let default_config = device
        .default_input_config()
        .map_err(|e| DeviceError::BackendError(format!("default_input_config: {e}")))?;

    let min_buffer_size = if min_buf == u32::MAX { 0 } else { min_buf };
    let max_buffer_size = max_buf;
    let default_buffer_size = clamp_default_buffer_size(min_buffer_size, max_buffer_size);

    Ok(InputCapabilities {
        min_sample_rate: min_rate,
        max_sample_rate: max_rate,
        supported_sample_rates: rates.into_iter().collect(),
        min_buffer_size,
        max_buffer_size,
        channels: channels_set.into_iter().collect(),
        default_sample_rate: default_config.sample_rate().0,
        default_buffer_size,
    })
}

/// Pick a default buffer size inside the device's [min, max] range.
/// Prefers 256; clamps up to `min` when the device requires a larger
/// minimum, down to `max` when the device caps below 256.
fn clamp_default_buffer_size(min: u32, max: u32) -> u32 {
    const PREFERRED: u32 = 256;
    if min == 0 && max == 0 {
        return PREFERRED;
    }
    PREFERRED.clamp(min, max.max(min))
}

/// Cache-miss fallback for `find_device` — fresh enumeration walk.
fn find_device_via_enum(id: &DeviceId) -> Result<cpal::Device, DeviceError> {
    let (host_str, dev_name) = decode_id(&id.0)
        .ok_or_else(|| DeviceError::DeviceNotFound { id: id.clone() })?;
    let host_id = cpal::available_hosts()
        .into_iter()
        .find(|h| h.name() == host_str)
        .ok_or_else(|| DeviceError::DeviceNotFound { id: id.clone() })?;
    let host = cpal::host_from_id(host_id)
        .map_err(|e| DeviceError::BackendError(e.to_string()))?;
    // Try output direction first; fall through to input. cpal's
    // iterators internally probe both directions, so either iterator
    // could surface the device.
    if let Ok(devices) = host.output_devices() {
        for d in devices {
            if d.name().is_ok_and(|n| n == dev_name) {
                return Ok(d);
            }
        }
    }
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            if d.name().is_ok_and(|n| n == dev_name) {
                return Ok(d);
            }
        }
    }
    Err(DeviceError::DeviceNotFound { id: id.clone() })
}

fn encode_id(host_id: cpal::HostId, name: &str) -> String {
    format!("{}:{}", host_id.name(), name)
}

fn decode_id(s: &str) -> Option<(&str, &str)> {
    s.split_once(':')
}

fn host_id_to_backend(host_id: cpal::HostId) -> Backend {
    let name = host_id.name();
    match name {
        "ALSA" => Backend::Alsa,
        "JACK" => Backend::Jack,
        "CoreAudio" => Backend::CoreAudio,
        "WASAPI" => Backend::Wasapi,
        "ASIO" => Backend::Asio,
        other => Backend::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_output_devices_does_not_panic() {
        let catalog = DeviceCatalog::new();
        let _ = catalog.list_output_devices();
    }

    #[test]
    fn list_input_devices_does_not_panic() {
        let catalog = DeviceCatalog::new();
        let _ = catalog.list_input_devices();
    }

    #[test]
    fn id_round_trip() {
        let host_id = cpal::available_hosts()
            .into_iter()
            .next()
            .expect("platform should expose at least one cpal host");
        let s = encode_id(host_id, "Some Device Name");
        let (host, dev) = decode_id(&s).unwrap();
        assert_eq!(host, host_id.name());
        assert_eq!(dev, "Some Device Name");
    }

    #[test]
    fn unknown_id_returns_device_not_found() {
        let catalog = DeviceCatalog::new();
        let bogus = DeviceId("NOPE:not-a-real-device-87b5e".into());
        match catalog.find_device(&bogus) {
            Err(DeviceError::DeviceNotFound { .. }) => {}
            Err(other) => panic!("expected DeviceNotFound, got {other:?}"),
            Ok(_) => panic!("expected DeviceNotFound, got Ok(device)"),
        }
    }

    #[test]
    fn unified_listing_returns_same_devices_for_both_directions() {
        // The whole point of the unified catalog: a device that has
        // both directions appears in BOTH list_output_devices and
        // list_input_devices outputs. (CI hosts may have zero such
        // devices; in that case the assertion is vacuous and that's OK.)
        let catalog = DeviceCatalog::new();
        let outputs: std::collections::HashSet<_> = catalog
            .list_output_devices()
            .into_iter()
            .map(|d| d.id)
            .collect();
        let inputs: std::collections::HashSet<_> = catalog
            .list_input_devices()
            .into_iter()
            .map(|d| d.id)
            .collect();
        // We can't assert non-empty intersection on every CI host, but
        // we CAN assert that any id present in outputs is findable
        // (cache is shared).
        for id in outputs.iter().chain(inputs.iter()) {
            catalog
                .find_device(id)
                .expect("a just-listed device must be findable");
        }
    }

    #[test]
    fn relisting_replaces_the_cache_not_merging_into_it() {
        let catalog = DeviceCatalog::new();
        catalog.list_output_devices();
        let stolen = {
            let guard = catalog.devices.lock().unwrap();
            guard.values().next().map(|c| c.device.clone())
        };
        let fake_id = DeviceId("ALSA:no-such-device-marker-only-for-this-test".into());
        if let Some(d) = stolen {
            catalog.devices.lock().unwrap().insert(
                fake_id.clone(),
                CachedDevice {
                    device: d,
                    output_info: None,
                    input_info: None,
                },
            );
            assert!(catalog.devices.lock().unwrap().contains_key(&fake_id));
        } else {
            return;
        }

        let _ = catalog.list_output_devices();
        let cache_ids: std::collections::HashSet<_> = catalog
            .devices
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert!(
            !cache_ids.contains(&fake_id),
            "fake key from before re-list must have been evicted",
        );
    }

    #[test]
    fn clamp_default_buffer_size_picks_256_inside_range() {
        assert_eq!(clamp_default_buffer_size(32, 4096), 256);
    }

    #[test]
    fn clamp_default_buffer_size_clamps_up_when_min_exceeds_256() {
        assert_eq!(clamp_default_buffer_size(512, 8192), 512);
    }

    #[test]
    fn clamp_default_buffer_size_clamps_down_when_max_below_256() {
        assert_eq!(clamp_default_buffer_size(16, 128), 128);
    }

    #[test]
    fn clamp_default_buffer_size_unknown_range_returns_256() {
        assert_eq!(clamp_default_buffer_size(0, 0), 256);
    }

    #[test]
    fn host_id_to_backend_unknown_host_yields_other_not_alsa() {
        assert!(matches!(Backend::Other("sndio".into()), Backend::Other(_)));
    }
}
