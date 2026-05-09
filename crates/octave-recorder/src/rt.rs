//! The real-time-safe input-buffer processing function.
//!
//! Pure function. No I/O, no logging, no `tracing`, no allocations, no
//! mutex/lock acquisition, no syscalls. Stack-only locals, atomic stores
//! to `Telemetry`, and a single `rtrb` push (or a counter bump on full).
//!
//! Lives in its own module so it can be tested end-to-end without cpal,
//! by passing in a hand-built `Telemetry` and a fresh ring's `Producer`.
//! The cpal callback closure (in `audio.rs`) wraps a single call to
//! [`process_input_buffer`] in `assert_no_alloc::assert_no_alloc(|| …)`,
//! which fires under the test global allocator (see `lib.rs`).
//!
//! See `docs/modules/record-audio.md` §3.4 (RT path) and §5 (algorithms).

use std::sync::atomic::Ordering;

use rtrb::Producer;

use crate::audio::Telemetry;

/// One audio-callback's worth of work.
///
/// 1. Per-channel peak (max abs) and mean-square — stack locals only.
///    Non-finite samples (NaN / Inf, e.g. from a buggy driver or an
///    upstream DSP block) are replaced with 0.0 before they touch
///    the meter; `telemetry.non_finite_seen` is latched so the
///    writer/UI can surface the event.
/// 2. Atomic stores: last-buffer peak, last-buffer mean-square,
///    running peak (max over the take so far).
/// 3. Push the (sanitized) interleaved slice into the ring; on full,
///    bump the `dropped_samples` counter. Never blocks.
pub(crate) fn process_input_buffer(
    samples: &[f32],
    channels: u16,
    telemetry: &Telemetry,
    producer: &mut Producer<f32>,
) {
    if samples.is_empty() {
        return;
    }
    let ch = usize::from(channels);
    debug_assert_eq!(samples.len() % ch, 0, "samples must be whole frames");
    let n_frames = samples.len() / ch;
    if n_frames == 0 {
        return;
    }

    // ---------- meter computations: stack-only, NaN/Inf-sanitized ----------
    let mut non_finite_in_buffer = false;
    for c in 0..ch {
        let mut peak = 0.0_f32;
        let mut sq = 0.0_f32;
        let mut i = c;
        while i < samples.len() {
            let raw = samples[i];
            let s = if raw.is_finite() {
                raw
            } else {
                non_finite_in_buffer = true;
                0.0
            };
            let a = s.abs();
            if a > peak {
                peak = a;
            }
            sq += s * s;
            i += ch;
        }
        #[allow(clippy::cast_precision_loss)]
        let ms = sq / n_frames as f32;

        telemetry.peak[c].store(peak.to_bits(), Ordering::Relaxed);
        telemetry.mean_square[c].store(ms.to_bits(), Ordering::Relaxed);

        // Running peak: single-writer (RT) so load-then-store is race-free.
        let prev = f32::from_bits(telemetry.running_peak[c].load(Ordering::Relaxed));
        if peak > prev {
            telemetry.running_peak[c].store(peak.to_bits(), Ordering::Relaxed);
        }
    }

    // ---------- ring push: wait-free, sanitized on the way in ----------
    //
    // If the meter pass saw clean input (the common case), `copy_nonoverlapping`
    // is the fast path. Otherwise we walk sample-by-sample and replace the
    // offenders, so the WAV file never carries NaN/Inf even though the meter
    // already counted them as 0.
    match producer.write_chunk_uninit(samples.len()) {
        Ok(mut chunk) => {
            let (s1, s2) = chunk.as_mut_slices();
            let n1 = s1.len();
            unsafe {
                if non_finite_in_buffer {
                    // Sanitizing copy: write each sample explicitly.
                    // The `is_finite` branch should auto-vectorize on
                    // x86_64 / aarch64. Cost is paid only when a
                    // non-finite sample was already detected above.
                    let s1_ptr = s1.as_mut_ptr().cast::<f32>();
                    let s2_ptr = s2.as_mut_ptr().cast::<f32>();
                    for (i, &raw) in samples.iter().enumerate() {
                        let s = if raw.is_finite() { raw } else { 0.0 };
                        if i < n1 {
                            // SAFETY: i < n1 and s1_ptr is valid for n1 writes.
                            *s1_ptr.add(i) = s;
                        } else {
                            // SAFETY: i - n1 < samples.len() - n1 == s2.len().
                            *s2_ptr.add(i - n1) = s;
                        }
                    }
                } else {
                    // Fast path: bulk memcpy.
                    // SAFETY: `MaybeUninit<f32>` has the same layout as `f32`;
                    // the two chunks together cover exactly `samples.len()`
                    // contiguous slots; we initialize every slot before commit.
                    let src = samples.as_ptr();
                    std::ptr::copy_nonoverlapping(src, s1.as_mut_ptr().cast::<f32>(), n1);
                    if n1 < samples.len() {
                        std::ptr::copy_nonoverlapping(
                            src.add(n1),
                            s2.as_mut_ptr().cast::<f32>(),
                            samples.len() - n1,
                        );
                    }
                }
                chunk.commit_all();
            }
        }
        Err(_) => {
            telemetry
                .dropped_samples
                .fetch_add(samples.len() as u64, Ordering::Relaxed);
        }
    }

    // Latch the non-finite flag last — the meter and ring writes are
    // already complete, so anyone observing the flag sees consistent
    // state (sanitized peaks + sanitized ring + flag set).
    if non_finite_in_buffer {
        telemetry.non_finite_seen.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::audio::Telemetry;
    use crate::ring::build_ring;

    #[test]
    fn peak_and_rms_per_channel_are_independent() {
        // Two channels: ch0 is a full-scale sine at +1/-1; ch1 is constant 0.5.
        let frames: Vec<f32> = (0..256)
            .flat_map(|i| {
                let s = if i % 2 == 0 { 1.0 } else { -1.0 };
                [s, 0.5_f32]
            })
            .collect();

        let telemetry = Telemetry::new(2);
        let (mut producer, _consumer) = build_ring(48_000, 2, 200);

        process_input_buffer(&frames, 2, &telemetry, &mut producer);

        // ch0: peak 1.0, ms 1.0
        assert!((telemetry.peak_value(0) - 1.0).abs() < 1e-6);
        assert!((telemetry.mean_square_value(0) - 1.0).abs() < 1e-6);
        // ch1: peak 0.5, ms 0.25
        assert!((telemetry.peak_value(1) - 0.5).abs() < 1e-6);
        assert!((telemetry.mean_square_value(1) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn running_peak_accumulates_across_buffers() {
        let telemetry = Telemetry::new(1);
        let (mut producer, _consumer) = build_ring(48_000, 1, 200);

        process_input_buffer(&vec![0.3_f32; 64], 1, &telemetry, &mut producer);
        assert!((telemetry.running_peak_value(0) - 0.3).abs() < 1e-6);

        // Second buffer's peak is lower; running_peak holds.
        process_input_buffer(&vec![0.1_f32; 64], 1, &telemetry, &mut producer);
        assert!((telemetry.running_peak_value(0) - 0.3).abs() < 1e-6);
        // last-buffer peak DOES drop to 0.1
        assert!((telemetry.peak_value(0) - 0.1).abs() < 1e-6);

        // Third buffer's peak is higher; running_peak rises.
        process_input_buffer(&vec![0.7_f32; 64], 1, &telemetry, &mut producer);
        assert!((telemetry.running_peak_value(0) - 0.7).abs() < 1e-6);
    }

    #[test]
    fn samples_pushed_through_ring_in_order() {
        let telemetry = Telemetry::new(1);
        let (mut producer, mut consumer) = build_ring(48_000, 1, 200);

        let buf: Vec<f32> = (0..64).map(|i| i as f32 * 0.01).collect();
        process_input_buffer(&buf, 1, &telemetry, &mut producer);

        let chunk = consumer.read_chunk(64).unwrap();
        let (s1, s2) = chunk.as_slices();
        let mut got = Vec::with_capacity(64);
        got.extend_from_slice(s1);
        got.extend_from_slice(s2);
        chunk.commit_all();

        for (i, (g, w)) in got.iter().zip(buf.iter()).enumerate() {
            assert_eq!(g.to_bits(), w.to_bits(), "sample {i}");
        }
    }

    #[test]
    fn ring_full_increments_dropped_samples_and_does_not_panic() {
        let telemetry = Telemetry::new(1);
        // Smallest ring that holds at least one buffer: cap = 100 samples.
        let (mut producer, _consumer) = rtrb::RingBuffer::<f32>::new(100);

        // First push fills the ring (100 samples fit).
        let buf = vec![0.5_f32; 100];
        process_input_buffer(&buf, 1, &telemetry, &mut producer);
        assert_eq!(telemetry.dropped_samples.load(Ordering::Relaxed), 0);

        // Second push has no room; whole buffer dropped.
        process_input_buffer(&buf, 1, &telemetry, &mut producer);
        assert_eq!(telemetry.dropped_samples.load(Ordering::Relaxed), 100);

        // Third push: still full.
        process_input_buffer(&buf, 1, &telemetry, &mut producer);
        assert_eq!(telemetry.dropped_samples.load(Ordering::Relaxed), 200);
    }

    #[test]
    fn empty_buffer_is_a_no_op() {
        let telemetry = Telemetry::new(2);
        let (mut producer, _consumer) = build_ring(48_000, 2, 200);
        process_input_buffer(&[], 2, &telemetry, &mut producer);
        assert_eq!(telemetry.peak_value(0), 0.0);
        assert_eq!(telemetry.peak_value(1), 0.0);
        assert_eq!(telemetry.dropped_samples.load(Ordering::Relaxed), 0);
    }

    // ---------- NaN / Inf sanitization (plan §5.7, §8) ----------

    #[test]
    fn nan_input_replaced_with_zero_in_meter_and_ring() {
        let telemetry = Telemetry::new(1);
        let (mut producer, mut consumer) = build_ring(48_000, 1, 200);

        // 4 samples: 0.5, NaN, -0.25, NaN. After sanitization the
        // meter and ring see 0.5, 0.0, -0.25, 0.0.
        let buf = [0.5_f32, f32::NAN, -0.25, f32::NAN];
        process_input_buffer(&buf, 1, &telemetry, &mut producer);

        // Meter: peak = 0.5 (no NaN propagation), ms = (0.25 + 0 + 0.0625 + 0)/4 = 0.078125.
        assert!((telemetry.peak_value(0) - 0.5).abs() < 1e-6);
        assert!((telemetry.mean_square_value(0) - 0.078_125).abs() < 1e-6);

        // Latched flag.
        assert!(telemetry.non_finite_seen.load(Ordering::Acquire));

        // Ring: NaNs replaced with 0.0 on the way in.
        let chunk = consumer.read_chunk(4).unwrap();
        let (s1, s2) = chunk.as_slices();
        let mut got = Vec::with_capacity(4);
        got.extend_from_slice(s1);
        got.extend_from_slice(s2);
        chunk.commit_all();
        assert_eq!(got, vec![0.5, 0.0, -0.25, 0.0]);
    }

    #[test]
    fn inf_input_replaced_with_zero_in_meter_and_ring() {
        let telemetry = Telemetry::new(1);
        let (mut producer, mut consumer) = build_ring(48_000, 1, 200);

        let buf = [f32::INFINITY, 0.3, f32::NEG_INFINITY, -0.7];
        process_input_buffer(&buf, 1, &telemetry, &mut producer);

        // Meter: peak = 0.7, ms = (0 + 0.09 + 0 + 0.49)/4 = 0.145.
        assert!((telemetry.peak_value(0) - 0.7).abs() < 1e-6);
        assert!((telemetry.mean_square_value(0) - 0.145).abs() < 1e-6);
        assert!(telemetry.non_finite_seen.load(Ordering::Acquire));

        let chunk = consumer.read_chunk(4).unwrap();
        let (s1, s2) = chunk.as_slices();
        let mut got = Vec::with_capacity(4);
        got.extend_from_slice(s1);
        got.extend_from_slice(s2);
        chunk.commit_all();
        assert_eq!(got, vec![0.0, 0.3, 0.0, -0.7]);
    }

    #[test]
    fn clean_input_does_not_set_non_finite_flag_or_pay_sanitize_cost() {
        let telemetry = Telemetry::new(2);
        let (mut producer, mut consumer) = build_ring(48_000, 2, 200);

        let buf: Vec<f32> = (0..16).map(|i| (i as f32) * 0.01).collect();
        process_input_buffer(&buf, 2, &telemetry, &mut producer);

        assert!(!telemetry.non_finite_seen.load(Ordering::Acquire));

        let chunk = consumer.read_chunk(16).unwrap();
        let (s1, s2) = chunk.as_slices();
        let mut got = Vec::with_capacity(16);
        got.extend_from_slice(s1);
        got.extend_from_slice(s2);
        chunk.commit_all();
        assert_eq!(got, buf);
    }

    #[test]
    fn nan_flag_is_latched_across_buffers() {
        let telemetry = Telemetry::new(1);
        let (mut producer, mut consumer) = build_ring(48_000, 1, 200);

        // Buffer 1: clean.
        process_input_buffer(&[0.1_f32; 4], 1, &telemetry, &mut producer);
        assert!(!telemetry.non_finite_seen.load(Ordering::Acquire));
        consumer.read_chunk(4).unwrap().commit_all();

        // Buffer 2: introduces NaN.
        process_input_buffer(&[0.1, f32::NAN, 0.2, 0.3], 1, &telemetry, &mut producer);
        assert!(telemetry.non_finite_seen.load(Ordering::Acquire));
        consumer.read_chunk(4).unwrap().commit_all();

        // Buffer 3: clean again — flag stays set (latched).
        process_input_buffer(&[0.1_f32; 4], 1, &telemetry, &mut producer);
        assert!(telemetry.non_finite_seen.load(Ordering::Acquire));
    }
}
