//! Cross-platform device enumeration and capability query.
//!
//! Wraps cpal's `Host` / `Device` / `SupportedStreamConfigRange` API
//! into the typed surface the public `list_devices`, `device_capabilities`,
//! and `open` use. The `DeviceId` is encoded as `"{HOST_NAME}:{DEVICE_NAME}"`
//! — opaque to callers, but stable enough that re-finding a device by id
//! works as long as the name doesn't change between runs.
//!
//! See `docs/modules/record-audio.md` §3.2 (driver layer) and §3.3 (cpal).

use std::collections::BTreeSet;

use cpal::traits::{DeviceTrait, HostTrait};

use crate::{Backend, Capabilities, DeviceId, DeviceInfo, OpenError};

/// Enumerate every input device on every cpal host the platform exposes.
pub(crate) fn list_devices_impl() -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    for host_id in cpal::available_hosts() {
        let Ok(host) = cpal::host_from_id(host_id) else { continue };
        let backend = host_id_to_backend(host_id);
        let default_name = host
            .default_input_device()
            .and_then(|d| d.name().ok());
        let Ok(devices) = host.input_devices() else { continue };
        for device in devices {
            let Ok(name) = device.name() else { continue };
            let max_input_channels = device
                .supported_input_configs()
                .ok()
                .and_then(|iter| iter.map(|c| c.channels()).max())
                .unwrap_or(0);
            let id = DeviceId(encode_id(host_id, &name));
            let is_default_input = default_name.as_deref() == Some(name.as_str());
            out.push(DeviceInfo {
                id,
                name,
                backend,
                is_default_input,
                // Class-compliant USB detection requires udev / IOKit / WinUSB
                // queries that are out of scope for v0.1.
                is_class_compliant_usb: false,
                max_input_channels,
            });
        }
    }
    out
}

/// Query one device's capabilities by id.
pub(crate) fn capabilities_impl(id: &DeviceId) -> Result<Capabilities, OpenError> {
    let device = find_device(id)?;
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

    Ok(Capabilities {
        min_sample_rate: min_rate,
        max_sample_rate: max_rate,
        supported_sample_rates: rates.into_iter().collect(),
        min_buffer_size: if min_buf == u32::MAX { 0 } else { min_buf },
        max_buffer_size: max_buf,
        channels: channels_set.into_iter().collect(),
        default_sample_rate: default_config.sample_rate().0,
        // cpal doesn't surface a default buffer size; 256 is a reasonable
        // tradeoff between latency and OS scheduling jitter.
        default_buffer_size: 256,
    })
}

/// Re-find a device on its host by encoded id.
pub(crate) fn find_device(id: &DeviceId) -> Result<cpal::Device, OpenError> {
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
    match host_id.name() {
        "ALSA" => Backend::Alsa,
        "JACK" => Backend::Jack,
        "CoreAudio" => Backend::CoreAudio,
        "WASAPI" => Backend::Wasapi,
        "ASIO" => Backend::Asio,
        _ => Backend::Alsa,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_devices_does_not_panic() {
        // CI machines may have zero audio devices; we only assert no panic.
        let _ = list_devices_impl();
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
        let bogus = DeviceId("NOPE:not-a-real-device-87b5e".into());
        match find_device(&bogus) {
            Err(OpenError::DeviceNotFound { .. }) => {}
            Err(other) => panic!("expected DeviceNotFound, got {other:?}"),
            Ok(_) => panic!("expected DeviceNotFound, got Ok(device)"),
        }
    }
}
