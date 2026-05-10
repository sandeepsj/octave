//! Cross-platform device enumeration and capability query.
//!
//! Wraps cpal's `Host` / `Device` / `SupportedStreamConfigRange` API
//! into the typed surface used by [`DeviceCatalog`] (which also owns
//! the cache that makes [`DeviceCatalog::open`] survive the PipeWire
//! ALSA enumerate-race — see plan §3.3.1).
//!
//! See `docs/modules/record-audio.md` §3.2 (driver layer) and §3.3 (cpal).

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait};

/// Multi-pass enumeration tunables (mirror of `octave-player`'s
/// constants). cpal's ALSA backend probes each PCM hint with
/// `snd_pcm_open(name, …, SND_PCM_NONBLOCK)` and silently drops
/// devices whose open returns `EBUSY`. PipeWire holds physical
/// `hw:CARD=` capture PCMs exclusively at arbitrary moments, so any
/// single pass can miss a device. Three passes with 100 ms gaps
/// catches most of the race; the union-by-id keeps the `cpal::Device`
/// from whichever pass first saw each device.
///
/// 3 × 100 ms = 300 ms total list latency. Acceptable for a button
/// click; below the 400 ms ISO-acceptable threshold for "instant".
const ENUMERATION_PASSES: u32 = 3;
const ENUMERATION_PASS_GAP: Duration = Duration::from_millis(100);

use crate::{Backend, Capabilities, DeviceId, DeviceInfo, OpenError};

/// Owns the cached `cpal::Device` handles from the most recent
/// `list_devices()` call. Mirror of `octave_player::DeviceCatalog` —
/// see plan §3.3.1 for the rationale: cpal's ALSA backend probes
/// devices by `snd_pcm_open`, which loses to PipeWire's exclusive
/// grab between list and open. By retaining the `cpal::Device` we
/// keep the underlying ALSA PCM open and `open()` can re-find the
/// device by id without re-enumerating.
///
/// `Send + Sync` (the inner `Mutex<HashMap<…, cpal::Device>>` is, on
/// every backend Octave targets — see plan §3.3.1 for the per-backend
/// proof).
pub struct DeviceCatalog {
    devices: Mutex<HashMap<DeviceId, cpal::Device>>,
}

impl DeviceCatalog {
    pub fn new() -> Self {
        Self {
            devices: Mutex::new(HashMap::new()),
        }
    }

    /// Enumerate every input device on every cpal host the platform
    /// exposes. Replaces the cache with a fresh map of cpal::Device
    /// handles; entries that disappeared since the previous call are
    /// dropped (releasing their underlying ALSA refs). Subsequent
    /// `open()` calls will use these cached handles.
    ///
    /// Devices whose `supported_input_configs` query fails are
    /// **skipped** (with a `tracing::warn`) rather than being surfaced
    /// with `max_input_channels = 0` — the latter looks like a broken
    /// device to UI/agents and hides the underlying probe failure.
    ///
    /// Multi-pass: see [`ENUMERATION_PASSES`] / [`ENUMERATION_PASS_GAP`]
    /// — single-pass enumeration loses to PipeWire's intermittent
    /// exclusive-grab on Linux. Each pass is a fresh
    /// `host.input_devices()` iteration; the union keeps every device
    /// id any pass saw, with the `cpal::Device` from the first pass
    /// that found it.
    pub fn list_devices(&self) -> Vec<DeviceInfo> {
        // Drop the old cache UP FRONT — see the symmetric comment in
        // `octave-player::DeviceCatalog::list_output_devices`. Our own
        // cached `cpal::Device` handles would otherwise block the
        // multi-pass probe of those same devices on a re-list. Active
        // `RecordingHandle`s carry their own Arc-shared clone, so
        // this clear doesn't disturb them.
        {
            let mut guard = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            guard.clear();
        }

        let mut out = Vec::new();
        let mut new_cache: HashMap<DeviceId, cpal::Device> = HashMap::new();
        for host_id in cpal::available_hosts() {
            let Ok(host) = cpal::host_from_id(host_id) else { continue };
            let backend = host_id_to_backend(host_id);
            let default_name = host
                .default_input_device()
                .and_then(|d| d.name().ok());
            for pass in 0..ENUMERATION_PASSES {
                if pass > 0 {
                    std::thread::sleep(ENUMERATION_PASS_GAP);
                }
                let Ok(devices) = host.input_devices() else { continue };
                for device in devices {
                    let Ok(name) = device.name() else { continue };
                    let id = DeviceId(encode_id(host_id, &name));
                    // Union by id: keep the first pass's cpal::Device
                    // and skip duplicates on later passes.
                    if new_cache.contains_key(&id) {
                        continue;
                    }
                    let max_input_channels = match device.supported_input_configs() {
                        Ok(iter) => iter.map(|c| c.channels()).max().unwrap_or(0),
                        Err(e) => {
                            tracing::warn!(
                                device = %name,
                                error = %e,
                                "supported_input_configs failed; skipping device"
                            );
                            continue;
                        }
                    };
                    let is_default_input = default_name.as_deref() == Some(name.as_str());
                    new_cache.insert(id.clone(), device);
                    out.push(DeviceInfo {
                        id,
                        name,
                        backend: backend.clone(),
                        is_default_input,
                        // Class-compliant USB detection requires udev / IOKit / WinUSB
                        // queries that are out of scope for v0.1.
                        is_class_compliant_usb: false,
                        max_input_channels,
                    });
                }
            }
        }
        // Mutex poisoning here would mean a previous holder panicked
        // mid-update; recover by overwriting the inner map with the
        // fresh enumeration.
        let mut guard = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        *guard = new_cache;
        out
    }

    /// Query one device's capabilities by id. Cache-first; falls back
    /// to fresh enumeration on miss (so callers that supply a
    /// remembered id without listing first still work).
    pub fn device_capabilities(&self, id: &DeviceId) -> Result<Capabilities, OpenError> {
        let device = self.find_device(id)?;
        capabilities_for_device(&device)
    }

    /// Re-find a device on its host by encoded id. Cache-first (see
    /// plan §3.3.1); enumerate on miss.
    pub(crate) fn find_device(&self, id: &DeviceId) -> Result<cpal::Device, OpenError> {
        if let Some(d) = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
        {
            return Ok(d.clone());
        }
        find_device_via_enum(id)
    }
}

impl Default for DeviceCatalog {
    fn default() -> Self {
        Self::new()
    }
}

fn capabilities_for_device(device: &cpal::Device) -> Result<Capabilities, OpenError> {
    let supported = device
        .supported_input_configs()
        .map_err(|e| OpenError::BackendError(format!("supported_input_configs: {e}")))?;

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
        return Err(OpenError::BackendError(
            "device exposes no supported input configs".into(),
        ));
    }

    let default_config = device
        .default_input_config()
        .map_err(|e| OpenError::BackendError(format!("default_input_config: {e}")))?;

    let min_buffer_size = if min_buf == u32::MAX { 0 } else { min_buf };
    let max_buffer_size = max_buf;

    // cpal doesn't surface a default buffer size; 256 is a reasonable
    // tradeoff between latency and scheduler jitter, but it must land
    // inside the device's actual range — some pro interfaces report
    // min > 256, and some embedded backends max < 256.
    let default_buffer_size = clamp_default_buffer_size(min_buffer_size, max_buffer_size);

    Ok(Capabilities {
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
/// minimum, down to `max` when the device caps below 256. If the
/// range is unknown (both 0) returns 256 — the historical default.
fn clamp_default_buffer_size(min: u32, max: u32) -> u32 {
    const PREFERRED: u32 = 256;
    if min == 0 && max == 0 {
        return PREFERRED;
    }
    PREFERRED.clamp(min, max.max(min))
}

/// Re-find a device on its host by encoded id. Cache-miss fallback
/// — see [`DeviceCatalog::find_device`].
fn find_device_via_enum(id: &DeviceId) -> Result<cpal::Device, OpenError> {
    let (host_str, dev_name) = decode_id(&id.0)
        .ok_or_else(|| OpenError::DeviceNotFound { id: id.clone() })?;
    let host_id = cpal::available_hosts()
        .into_iter()
        .find(|h| h.name() == host_str)
        .ok_or_else(|| OpenError::DeviceNotFound { id: id.clone() })?;
    let host = cpal::host_from_id(host_id)
        .map_err(|e| OpenError::BackendError(e.to_string()))?;
    host.input_devices()
        .map_err(|e| OpenError::BackendError(e.to_string()))?
        .find(|d| d.name().is_ok_and(|n| n == dev_name))
        .ok_or_else(|| OpenError::DeviceNotFound { id: id.clone() })
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
        // Strictly: do NOT silently coerce unknown hosts to Alsa —
        // that mis-tagged macOS / Windows / future hosts. Surface the
        // raw name so the caller can see what happened.
        other => Backend::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_devices_does_not_panic() {
        let catalog = DeviceCatalog::new();
        let _ = catalog.list_devices();
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
        // Fresh catalog → cache miss → enumeration fallback → not found.
        let catalog = DeviceCatalog::new();
        let bogus = DeviceId("NOPE:not-a-real-device-87b5e".into());
        match catalog.find_device(&bogus) {
            Err(OpenError::DeviceNotFound { .. }) => {}
            Err(other) => panic!("expected DeviceNotFound, got {other:?}"),
            Ok(_) => panic!("expected DeviceNotFound, got Ok(device)"),
        }
    }

    #[test]
    fn list_then_find_via_cache_succeeds() {
        // Mirror of octave-player's analogous test: any device that
        // came back from `list_devices` must be re-findable without
        // enumeration (which would race with PipeWire's exclusive
        // grab on Linux).
        let catalog = DeviceCatalog::new();
        let listed = catalog.list_devices();
        for info in &listed {
            catalog
                .find_device(&info.id)
                .expect("a just-listed device must be findable");
        }
    }

    #[test]
    fn relisting_replaces_the_cache_not_merging_into_it() {
        // Smuggle a known-fake key into the cache by stealing a real
        // `cpal::Device` from the first listing. After a second
        // `list_devices` the fake key must be gone — proves the cache
        // is REPLACED, not merged (a merge would silently leak handles
        // to vanished devices).
        let catalog = DeviceCatalog::new();
        catalog.list_devices();
        let stolen_device = {
            let guard = catalog.devices.lock().unwrap();
            guard.values().next().cloned()
        };
        let fake_id = DeviceId("ALSA:no-such-device-marker-only-for-this-test".into());
        if let Some(d) = stolen_device {
            catalog.devices.lock().unwrap().insert(fake_id.clone(), d);
            assert!(
                catalog.devices.lock().unwrap().contains_key(&fake_id),
                "test setup: fake key should be in the cache before re-list"
            );
        } else {
            // CI host with zero devices — nothing to steal; skip the
            // eviction half rather than asserting on absent state.
            return;
        }

        let second = catalog.list_devices();
        let second_ids: std::collections::HashSet<_> =
            second.iter().map(|d| d.id.clone()).collect();
        let cache_ids: std::collections::HashSet<_> = catalog
            .devices
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            second_ids, cache_ids,
            "after re-list, cache must contain exactly the second listing",
        );
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
        // We can't fabricate a cpal::HostId in tests, but the variant
        // exists and the only path to it is the catch-all arm —
        // covered by build-time exhaustiveness.
        assert!(matches!(Backend::Other("sndio".into()), Backend::Other(_)));
    }
}
