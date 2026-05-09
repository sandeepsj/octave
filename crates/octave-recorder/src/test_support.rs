//! Test helpers shared across module-level `#[cfg(test)] mod tests`.

#![cfg(test)]

/// Generate `frames` interleaved stereo samples of a sine at `freq_hz`,
/// with channel 1 inverted relative to channel 0 (so we can spot a swap).
#[allow(clippy::cast_precision_loss)]
pub(crate) fn sine_stereo(frames: usize, sample_rate: u32, freq_hz: f32) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| {
            let t = i as f32 / sample_rate as f32;
            let s = (2.0 * std::f32::consts::PI * freq_hz * t).sin();
            [s, -s]
        })
        .collect()
}
