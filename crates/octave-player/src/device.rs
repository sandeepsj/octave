//! Cross-platform output-device enumeration and capability query.
//!
//! Wraps cpal's `Host` / `Device` / `SupportedStreamConfigRange` API
//! into the typed surface used by [`DeviceCatalog`] (which also owns
//! the cache that makes [`DeviceCatalog::start`] survive the PipeWire
//! ALSA enumerate-race).
//! The `DeviceId` is encoded as `"{HOST_NAME}:{DEVICE_NAME}"` — opaque
//! to callers, but stable enough that re-finding a device by id works
//! as long as the name doesn't change between runs.
//!
//! Mirror of `octave-recorder`'s device.rs, with `input_*` swapped to
//! `output_*` throughout. See `docs/modules/playback-audio.md` §3.2 +
//! §3.3.1 (the device-handle cache that defeats the PipeWire ALSA
//! enumerate-race).

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use cpal::traits::{DeviceTrait, HostTrait};
use thiserror::Error;

use crate::types::{Backend, DeviceId, OutputCapabilities, OutputDeviceInfo};

/// Errors returned when looking up a device or its capabilities.
#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("device not found: {id:?}")]
    DeviceNotFound { id: DeviceId },
    #[error("backend error: {0}")]
    BackendError(String),
}

/// Owns the cached `cpal::Device` handles from the most recent
/// `list_output_devices()` call. See module docs / plan §3.3.1 for
/// rationale: cpal's ALSA backend probes devices by `snd_pcm_open`,
/// which loses to PipeWire's exclusive grab — by retaining the
/// `cpal::Device` we keep the underlying ALSA PCM open and `start()`
/// can re-find the device by id without re-enumerating.
///
/// `Send + Sync` (the inner `Mutex<HashMap<…, cpal::Device>>` is, on
/// every backend Octave targets — see plan §3.3.1).
///
/// Cheap to construct (`new()` allocates an empty `HashMap` behind a
/// `Mutex`); cheap to keep alive (one `cpal::Device` per output device
/// the platform exposes — small handles).
pub struct DeviceCatalog {
    devices: Mutex<HashMap<DeviceId, cpal::Device>>,
}

impl DeviceCatalog {
    pub fn new() -> Self {
        Self {
            devices: Mutex::new(HashMap::new()),
        }
    }

    /// Enumerate every output device on every cpal host the platform
    /// exposes. Replaces the cache with a fresh map of cpal::Device
    /// handles; entries that disappeared since the previous call are
    /// dropped (releasing their underlying ALSA refs). Subsequent
    /// `start()` calls will use these cached handles.
    pub fn list_output_devices(&self) -> Vec<OutputDeviceInfo> {
        let mut out = Vec::new();
        let mut new_cache: HashMap<DeviceId, cpal::Device> = HashMap::new();
        for host_id in cpal::available_hosts() {
            let Ok(host) = cpal::host_from_id(host_id) else { continue };
            let backend = host_id_to_backend(host_id);
            let default_name = host
                .default_output_device()
                .and_then(|d| d.name().ok());
            let Ok(devices) = host.output_devices() else { continue };
            for device in devices {
                let Ok(name) = device.name() else { continue };
                let max_output_channels = device
                    .supported_output_configs()
                    .ok()
                    .and_then(|iter| iter.map(|c| c.channels()).max())
                    .unwrap_or(0);
                let id = DeviceId(encode_id(host_id, &name));
                let is_default_output = default_name.as_deref() == Some(name.as_str());
                new_cache.insert(id.clone(), device);
                out.push(OutputDeviceInfo {
                    id,
                    name,
                    backend,
                    is_default_output,
                    max_output_channels,
                });
            }
        }
        // Mutex poisoning here would mean a previous holder panicked
        // mid-update; recover by overwriting the inner map with the
        // fresh enumeration.
        let mut guard = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        *guard = new_cache;
        out
    }

    /// Query one output device's capabilities by id. Cache hit when
    /// the id was returned by a recent `list_output_devices()` on this
    /// catalog; cache miss falls through to fresh enumeration.
    pub fn output_device_capabilities(
        &self,
        id: &DeviceId,
    ) -> Result<OutputCapabilities, DeviceError> {
        let device = self.find_device(id)?;
        capabilities_for_device(&device)
    }

    /// Re-find an output device on its host by encoded id. Cache-first
    /// (see plan §3.3.1); enumerate on miss so callers that supply a
    /// remembered id without listing first still work.
    pub(crate) fn find_device(&self, id: &DeviceId) -> Result<cpal::Device, DeviceError> {
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

fn capabilities_for_device(device: &cpal::Device) -> Result<OutputCapabilities, DeviceError> {
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

fn find_device_via_enum(id: &DeviceId) -> Result<cpal::Device, DeviceError> {
    let (host_str, dev_name) =
        decode_id(&id.0).ok_or_else(|| DeviceError::DeviceNotFound { id: id.clone() })?;
    let host_id = cpal::available_hosts()
        .into_iter()
        .find(|h| h.name() == host_str)
        .ok_or_else(|| DeviceError::DeviceNotFound { id: id.clone() })?;
    let host = cpal::host_from_id(host_id)
        .map_err(|e| DeviceError::BackendError(e.to_string()))?;
    host.output_devices()
        .map_err(|e| DeviceError::BackendError(e.to_string()))?
        .find(|d| d.name().is_ok_and(|n| n == dev_name))
        .ok_or_else(|| DeviceError::DeviceNotFound { id: id.clone() })
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
        // The recorder grew Backend::Other(String) to surface unknown
        // hosts (commit b58bfec); the player's Backend hasn't yet —
        // tracked as future-cleanup. Until then log the host so the
        // mis-tag is visible in the operator's tracing output rather
        // than silent.
        unknown => {
            tracing::warn!(host = unknown, "unknown cpal host name; tagging as Alsa (Backend::Other not yet on player)");
            Backend::Alsa
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_output_devices_does_not_panic() {
        let catalog = DeviceCatalog::new();
        // CI machines may have zero output devices; we only assert no panic.
        let _ = catalog.list_output_devices();
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
        let id = DeviceId("ALSA:absolutely-no-such-device-zzz".into());
        match catalog.find_device(&id) {
            Ok(_) => panic!("found a device that shouldn't exist"),
            Err(e) => assert!(matches!(e, DeviceError::DeviceNotFound { .. })),
        }
    }

    #[test]
    fn list_then_find_via_cache_succeeds() {
        // The whole point of the cache: any device that came back from
        // `list_output_devices` must be re-findable without enumeration
        // (which would race with PipeWire's exclusive grab on Linux).
        // We can't directly assert "didn't enumerate" from outside —
        // but we CAN assert the basic round-trip works.
        let catalog = DeviceCatalog::new();
        let listed = catalog.list_output_devices();
        for info in &listed {
            catalog
                .find_device(&info.id)
                .expect("a just-listed device must be findable");
        }
    }

    #[test]
    fn relisting_replaces_the_cache_not_merging_into_it() {
        // Inject a fake `DeviceId` into the cache by abusing the
        // module-private `devices` field. We can't manufacture a real
        // `cpal::Device`, but we CAN smuggle one in by stealing one
        // from a real listing under a fake key. After a second
        // `list_output_devices` call the fake key must be gone — that
        // proves `list_output_devices` REPLACES the cache rather than
        // merging into it (a merge would silently leak handles for
        // devices that disappeared between calls).
        let catalog = DeviceCatalog::new();
        catalog.list_output_devices();
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
            // CI host with zero devices — nothing to steal, can't run
            // the eviction half. The first half (assert presence) is
            // what we'd assert; skipping the rest is honest.
            return;
        }

        // Re-list: cache must lose the fake, regain only what cpal
        // currently returns.
        let second = catalog.list_output_devices();
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
}
