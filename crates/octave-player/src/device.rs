//! Cross-platform output-device enumeration and capability query.
//!
//! Wraps cpal's `Host` / `Device` / `SupportedStreamConfigRange` API
//! into the typed surface `list_output_devices`, `output_device_capabilities`,
//! and `open` use. The `DeviceId` is encoded as `"{HOST_NAME}:{DEVICE_NAME}"`
//! — opaque to callers, but stable enough that re-finding a device by id
//! works as long as the name doesn't change between runs.
//!
//! Mirror of `octave-recorder`'s device.rs, with `input_*` swapped to
//! `output_*` throughout. See `docs/modules/playback-audio.md` §3.2.

use std::collections::BTreeSet;

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

/// Enumerate every output device on every cpal host the platform exposes.
pub(crate) fn list_output_devices_impl() -> Vec<OutputDeviceInfo> {
    let mut out = Vec::new();
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
            out.push(OutputDeviceInfo {
                id,
                name,
                backend,
                is_default_output,
                max_output_channels,
            });
        }
    }
    out
}

/// Query one output device's capabilities by id.
pub(crate) fn capabilities_impl(id: &DeviceId) -> Result<OutputCapabilities, DeviceError> {
    let device = find_device(id)?;
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

/// Re-find an output device on its host by encoded id.
pub(crate) fn find_device(id: &DeviceId) -> Result<cpal::Device, DeviceError> {
    let (host_str, dev_name) = decode_id(&id.0)
        .ok_or_else(|| DeviceError::DeviceNotFound { id: id.clone() })?;
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
        // CI machines may have zero output devices; we only assert no panic.
        let _ = list_output_devices_impl();
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
        let id = DeviceId("ALSA:absolutely-no-such-device-zzz".into());
        // cpal::Device doesn't impl Debug, so unwrap_err is unavailable.
        match find_device(&id) {
            Ok(_) => panic!("found a device that shouldn't exist"),
            Err(e) => assert!(matches!(e, DeviceError::DeviceNotFound { .. })),
        }
    }
}
