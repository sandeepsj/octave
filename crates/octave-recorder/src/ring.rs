//! SPSC ring sizing for the audio→writer handoff.
//!
//! Wraps `rtrb::RingBuffer<f32>` so the rest of the crate doesn't depend
//! on `rtrb` directly. Capacity is sized in *samples* (interleaved frames
//! flattened); see `docs/modules/record-audio.md` §5.4.

use rtrb::{Consumer, Producer, RingBuffer};

/// Default ring headroom: 1 second of audio. The writer thread parks for
/// ≤ 2 ms between drains, so this tolerates ~500× the typical drain
/// interval before the audio thread must drop a buffer.
pub(crate) const DEFAULT_HEADROOM_MS: u32 = 1_000;

/// Build the SPSC ring used by the audio thread (producer) and the
/// writer thread (consumer). Capacity in samples is
/// `sample_rate × headroom_ms / 1000 × channels`, always a whole multiple
/// of `channels` so reads/writes can stay frame-aligned.
pub(crate) fn build_ring(
    sample_rate: u32,
    channels: u16,
    headroom_ms: u32,
) -> (Producer<f32>, Consumer<f32>) {
    let frames = u64::from(sample_rate) * u64::from(headroom_ms) / 1_000;
    let samples = frames * u64::from(channels);
    let capacity = usize::try_from(samples).expect("ring capacity fits usize");
    RingBuffer::new(capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_ring_has_writable_capacity_and_no_readable_data() {
        let (p, c) = build_ring(48_000, 2, DEFAULT_HEADROOM_MS);
        assert_eq!(p.slots(), 96_000); // 48 000 × 2
        assert_eq!(c.slots(), 0);
    }

    #[test]
    fn capacity_scales_with_headroom() {
        let (p, _) = build_ring(48_000, 2, 100);
        assert_eq!(p.slots(), 9_600); // 100 ms of stereo @ 48 kHz
    }

    #[test]
    fn capacity_is_whole_frames() {
        for ch in 1..=2u16 {
            for rate in [44_100, 48_000, 88_200, 96_000, 176_400, 192_000] {
                let (p, _) = build_ring(rate, ch, DEFAULT_HEADROOM_MS);
                assert_eq!(p.slots() % usize::from(ch), 0, "rate={rate}, ch={ch}");
            }
        }
    }
}
