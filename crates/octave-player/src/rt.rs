//! The real-time-safe output-buffer fill function.
//!
//! Pure function. No I/O, no logging, no `tracing`, no allocations,
//! no mutex/lock acquisition, no syscalls. Stack-only locals, atomic
//! stores to `Telemetry`, and a single `rtrb` `read_chunk` (or all-zero
//! fill on under-run).
//!
//! Lives in its own module so it can be tested end-to-end without
//! cpal, by passing in a hand-built `Telemetry` and a fresh ring's
//! `Consumer` after pre-populating the producer side. The real cpal
//! callback (in a follow-up `audio.rs`) wraps a single call to
//! [`process_output_buffer`] in `assert_no_alloc::assert_no_alloc(|| …)`.
//!
//! See `docs/modules/playback-audio.md` §3.4 (RT side), §5.3, §5.4.

use std::sync::atomic::Ordering;

use rtrb::Consumer;

use crate::signals::TransportSignals;
use crate::telemetry::Telemetry;

/// One audio-callback's worth of work.
///
/// Behaviour:
/// 1. If `signals.stop_request` is set, silence `out`, store silence
///    in the meter, and return early. (No position advance, no xrun
///    counted — stop is graceful.)
/// 2. If `signals.flush_request` is set, drop everything currently in
///    the ring, silence `out`, set `position_frames =
///    signals.seek_target`, clear `flush_request`, and return. The
///    one-period silence is the user-visible cost of the seek.
/// 3. Otherwise, drain up to `out.len()` interleaved samples from the
///    ring, rounded down to a whole number of frames; copy into `out`;
///    silence-fill any shortfall and bump `xrun_count`.
/// 4. Per-channel peak / mean-square / running peak updated.
/// 5. `telemetry.position_frames` advances by frames actually drained.
/// 6. If `signals.eof_seen` is set and the ring is empty after the
///    drain, set `signals.playback_complete` so the actor can
///    transition state to `Ended`.
pub(crate) fn process_output_buffer(
    out: &mut [f32],
    channels: u16,
    telemetry: &Telemetry,
    consumer: &mut Consumer<f32>,
    signals: &TransportSignals,
) {
    if out.is_empty() {
        return;
    }
    let ch = usize::from(channels);
    debug_assert_eq!(out.len() % ch, 0, "out must be a whole number of frames");

    // ---------- stop handshake (silence + early return) ----------
    if signals.stop_request.load(Ordering::Acquire) {
        for slot in out.iter_mut() {
            *slot = 0.0;
        }
        zero_meter(telemetry);
        return;
    }

    // ---------- flush handshake (drop ring, reset position, silence) ----------
    if signals.flush_request.load(Ordering::Acquire) {
        let avail = consumer.slots();
        if avail > 0 {
            if let Ok(chunk) = consumer.read_chunk(avail) {
                chunk.commit_all();
            }
        }
        for slot in out.iter_mut() {
            *slot = 0.0;
        }
        let target = signals.seek_target.load(Ordering::Acquire);
        telemetry.set_position_frames(target);
        zero_meter(telemetry);
        signals.flush_request.store(false, Ordering::Release);
        return;
    }

    // ---------- ring drain: wait-free, silence on shortfall ----------
    let want = out.len();
    let available = consumer.slots();
    // Round down to whole frames so we never split a frame across the
    // silence boundary.
    let take_samples = (available.min(want) / ch) * ch;

    if take_samples > 0 {
        // `read_chunk` succeeds because we asked for ≤ available.
        match consumer.read_chunk(take_samples) {
            Ok(chunk) => {
                let (s1, s2) = chunk.as_slices();
                let n1 = s1.len();
                out[..n1].copy_from_slice(s1);
                if !s2.is_empty() {
                    out[n1..n1 + s2.len()].copy_from_slice(s2);
                }
                chunk.commit_all();
            }
            Err(_) => {
                // Should be impossible given the `min(available)` clamp,
                // but if it happens, silence everything and count the xrun.
                for slot in out.iter_mut() {
                    *slot = 0.0;
                }
                telemetry.xrun_count.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    }

    // Silence the remainder of `out`. Count the under-run as an xrun
    // only if we're still in normal playback — after EOF the silence
    // is the natural tail of the source, not starvation.
    if take_samples < want {
        for slot in &mut out[take_samples..] {
            *slot = 0.0;
        }
        if !signals.eof_seen.load(Ordering::Acquire) {
            telemetry.xrun_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    // ---------- meter: stack-only ----------
    let n_frames = out.len() / ch;
    for c in 0..ch {
        let mut peak = 0.0_f32;
        let mut sq = 0.0_f32;
        let mut i = c;
        while i < out.len() {
            let s = out[i];
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

    // ---------- position counter ----------
    let frames_drained = take_samples / ch;
    telemetry
        .position_frames
        .fetch_add(frames_drained as u64, Ordering::Release);

    // ---------- EOF transition ----------
    // The reader has stopped pulling from the source; once the ring
    // is empty after this callback's drain, there's nothing left to
    // play and the actor should transition to `Ended`.
    if signals.eof_seen.load(Ordering::Acquire) && consumer.slots() == 0 {
        signals.playback_complete.store(true, Ordering::Release);
    }
}

/// Park the meter at silence — used when `stop` or `flush` short-
/// circuits the normal path so the UI doesn't see the prior buffer's
/// peak frozen for one cycle.
fn zero_meter(telemetry: &Telemetry) {
    let zero_bits = 0.0_f32.to_bits();
    for ap in telemetry.peak.iter() {
        ap.store(zero_bits, Ordering::Relaxed);
    }
    for ap in telemetry.mean_square.iter() {
        ap.store(zero_bits, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ring::build_ring;

    /// Push `samples` into the producer end. Test ring is sized larger
    /// than any individual push so this never errors.
    fn push_all(producer: &mut rtrb::Producer<f32>, samples: &[f32]) {
        for s in samples {
            producer.push(*s).expect("test ring big enough");
        }
    }

    #[test]
    fn empty_out_is_a_no_op() {
        let telemetry = Telemetry::new(2);
        let signals = TransportSignals::new();
        let (_p, mut c) = build_ring(48_000, 2, 200);
        process_output_buffer(&mut [], 2, &telemetry, &mut c, &signals);
        assert_eq!(telemetry.peak_value(0), 0.0);
        assert_eq!(telemetry.position_frames.load(Ordering::Relaxed), 0);
        assert_eq!(telemetry.xrun_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn happy_path_drains_full_buffer_and_updates_meter() {
        let telemetry = Telemetry::new(2);
        let signals = TransportSignals::new();
        let (mut p, mut c) = build_ring(48_000, 2, 200);

        // 8 frames of stereo: ch0 saturates at 1.0, ch1 sits at 0.5.
        let frames: Vec<f32> = (0..8).flat_map(|_| [1.0_f32, 0.5_f32]).collect();
        push_all(&mut p, &frames);

        let mut out = vec![0.0_f32; 16]; // 8 frames * 2 ch
        process_output_buffer(&mut out, 2, &telemetry, &mut c, &signals);

        assert_eq!(out, frames, "out matches what was in the ring");
        assert_eq!(telemetry.position_frames.load(Ordering::Relaxed), 8);
        assert_eq!(telemetry.xrun_count.load(Ordering::Relaxed), 0);
        // ch0 peak 1.0, ms 1.0; ch1 peak 0.5, ms 0.25
        assert!((telemetry.peak_value(0) - 1.0).abs() < 1e-6);
        assert!((telemetry.peak_value(1) - 0.5).abs() < 1e-6);
        assert!((telemetry.mean_square_value(0) - 1.0).abs() < 1e-6);
        assert!((telemetry.mean_square_value(1) - 0.25).abs() < 1e-6);
        // Running peak holds the same values.
        assert!((telemetry.running_peak_value(0) - 1.0).abs() < 1e-6);
        assert!((telemetry.running_peak_value(1) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn under_run_writes_silence_and_bumps_xrun() {
        let telemetry = Telemetry::new(1);
        let signals = TransportSignals::new();
        let (mut p, mut c) = build_ring(48_000, 1, 200);

        // Only 4 frames available; callback wants 8.
        push_all(&mut p, &[0.7_f32; 4]);
        let mut out = vec![1.0_f32; 8];
        process_output_buffer(&mut out, 1, &telemetry, &mut c, &signals);

        // First 4 came from ring; last 4 are silence.
        assert_eq!(&out[..4], &[0.7, 0.7, 0.7, 0.7]);
        assert_eq!(&out[4..], &[0.0, 0.0, 0.0, 0.0]);
        assert_eq!(telemetry.xrun_count.load(Ordering::Relaxed), 1);
        // Position only advanced by the 4 frames actually drained.
        assert_eq!(telemetry.position_frames.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn total_under_run_silences_everything() {
        let telemetry = Telemetry::new(2);
        let signals = TransportSignals::new();
        let (_p, mut c) = build_ring(48_000, 2, 200);

        let mut out = vec![1.0_f32; 16];
        process_output_buffer(&mut out, 2, &telemetry, &mut c, &signals);

        assert!(out.iter().all(|s| *s == 0.0));
        assert_eq!(telemetry.xrun_count.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.position_frames.load(Ordering::Relaxed), 0);
        // Meter sees silence: peak == 0
        assert_eq!(telemetry.peak_value(0), 0.0);
        assert_eq!(telemetry.peak_value(1), 0.0);
    }

    #[test]
    fn position_advances_across_callbacks() {
        let telemetry = Telemetry::new(2);
        let signals = TransportSignals::new();
        let (mut p, mut c) = build_ring(48_000, 2, 200);

        let frames: Vec<f32> = vec![0.0; 32]; // 16 frames stereo
        push_all(&mut p, &frames);

        let mut out = vec![0.0_f32; 16]; // 8 frames stereo
        process_output_buffer(&mut out, 2, &telemetry, &mut c, &signals);
        process_output_buffer(&mut out, 2, &telemetry, &mut c, &signals);
        assert_eq!(telemetry.position_frames.load(Ordering::Relaxed), 16);
    }

    #[test]
    fn running_peak_holds_max_across_buffers() {
        let telemetry = Telemetry::new(1);
        let signals = TransportSignals::new();
        let (mut p, mut c) = build_ring(48_000, 1, 200);

        push_all(&mut p, &[0.3_f32; 8]);
        let mut out = vec![0.0_f32; 8];
        process_output_buffer(&mut out, 1, &telemetry, &mut c, &signals);
        assert!((telemetry.running_peak_value(0) - 0.3).abs() < 1e-6);

        // Quieter buffer; running peak holds.
        push_all(&mut p, &[0.1_f32; 8]);
        process_output_buffer(&mut out, 1, &telemetry, &mut c, &signals);
        assert!((telemetry.running_peak_value(0) - 0.3).abs() < 1e-6);
        assert!((telemetry.peak_value(0) - 0.1).abs() < 1e-6); // last-buffer peak DID drop

        // Louder buffer; running peak rises.
        push_all(&mut p, &[0.7_f32; 8]);
        process_output_buffer(&mut out, 1, &telemetry, &mut c, &signals);
        assert!((telemetry.running_peak_value(0) - 0.7).abs() < 1e-6);
    }

    #[test]
    fn samples_drained_in_order_across_a_ring_wrap() {
        let telemetry = Telemetry::new(1);
        // Small ring (mono, 100 ms at 48 kHz = 4800 slots) — enough to wrap.
        let signals = TransportSignals::new();
        let (mut p, mut c) = build_ring(48_000, 1, 100);

        // Push 100 samples, drain 60, push 80 more (forces a wrap).
        let first: Vec<f32> = (0..100).map(|i| i as f32).collect();
        push_all(&mut p, &first);

        let mut out = vec![0.0_f32; 60];
        process_output_buffer(&mut out, 1, &telemetry, &mut c, &signals);
        assert_eq!(out[..60], first[..60]);

        let second: Vec<f32> = (100..180).map(|i| i as f32).collect();
        push_all(&mut p, &second);

        let mut out2 = vec![0.0_f32; 120]; // ask for 120; ring has 40 + 80 = 120
        process_output_buffer(&mut out2, 1, &telemetry, &mut c, &signals);
        // Samples 60..180 in order.
        let expected: Vec<f32> = (60..180).map(|i| i as f32).collect();
        assert_eq!(out2, expected);
    }

    #[test]
    fn position_can_be_reset_for_seek() {
        let telemetry = Telemetry::new(2);
        let signals = TransportSignals::new();
        let (mut p, mut c) = build_ring(48_000, 2, 200);

        push_all(&mut p, &[0.0_f32; 16]); // 8 stereo frames
        let mut out = vec![0.0_f32; 16];
        process_output_buffer(&mut out, 2, &telemetry, &mut c, &signals);
        assert_eq!(telemetry.position_frames.load(Ordering::Relaxed), 8);

        // Simulate a seek to frame 1000.
        telemetry.set_position_frames(1000);
        assert_eq!(telemetry.position_frames.load(Ordering::Acquire), 1000);

        // Next callback advances from 1000.
        push_all(&mut p, &[0.0_f32; 16]);
        process_output_buffer(&mut out, 2, &telemetry, &mut c, &signals);
        assert_eq!(telemetry.position_frames.load(Ordering::Acquire), 1008);
    }

    #[test]
    fn flush_request_drops_ring_resets_position_and_silences_out() {
        let telemetry = Telemetry::new(2);
        let signals = TransportSignals::new();
        let (mut p, mut c) = build_ring(48_000, 2, 200);

        // Fill ring with audio that should NOT be played past the seek.
        push_all(&mut p, &[0.9_f32; 32]);

        // UI -> reader -> signals: target = 12345, flush_request = true.
        signals.seek_target.store(12_345, Ordering::Release);
        signals.flush_request.store(true, Ordering::Release);

        let mut out = vec![0.42_f32; 16]; // sentinel non-zero
        process_output_buffer(&mut out, 2, &telemetry, &mut c, &signals);

        assert!(out.iter().all(|s| *s == 0.0), "out is silenced");
        assert_eq!(c.slots(), 0, "ring drained");
        assert_eq!(
            telemetry.position_frames.load(Ordering::Acquire),
            12_345,
            "position jumped to seek target"
        );
        assert!(
            !signals.flush_request.load(Ordering::Acquire),
            "flush flag cleared so reader can call source.seek and resume"
        );
        // No xrun on a flush — it's a deliberate silence.
        assert_eq!(telemetry.xrun_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn stop_request_silences_out_and_returns_early() {
        let telemetry = Telemetry::new(2);
        let signals = TransportSignals::new();
        let (mut p, mut c) = build_ring(48_000, 2, 200);

        push_all(&mut p, &[0.5_f32; 32]);
        signals.stop_request.store(true, Ordering::Release);

        let mut out = vec![0.42_f32; 16];
        process_output_buffer(&mut out, 2, &telemetry, &mut c, &signals);

        assert!(out.iter().all(|s| *s == 0.0));
        // Ring untouched — stop is the actor's job to drain on shutdown.
        assert_eq!(c.slots(), 32);
        assert_eq!(telemetry.position_frames.load(Ordering::Relaxed), 0);
        assert_eq!(telemetry.xrun_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn eof_seen_plus_empty_ring_signals_playback_complete() {
        let telemetry = Telemetry::new(1);
        let signals = TransportSignals::new();
        let (mut p, mut c) = build_ring(48_000, 1, 200);

        push_all(&mut p, &[0.1_f32; 4]);
        signals.eof_seen.store(true, Ordering::Release);

        let mut out = vec![0.0_f32; 4];
        process_output_buffer(&mut out, 1, &telemetry, &mut c, &signals);

        assert!(signals.playback_complete.load(Ordering::Acquire));
    }

    #[test]
    fn eof_seen_with_remaining_frames_does_not_complete_yet() {
        let telemetry = Telemetry::new(1);
        let signals = TransportSignals::new();
        let (mut p, mut c) = build_ring(48_000, 1, 200);

        push_all(&mut p, &[0.1_f32; 8]);
        signals.eof_seen.store(true, Ordering::Release);

        // Drain 4 of 8 — ring still has 4.
        let mut out = vec![0.0_f32; 4];
        process_output_buffer(&mut out, 1, &telemetry, &mut c, &signals);
        assert!(!signals.playback_complete.load(Ordering::Acquire));

        // Drain the rest — now complete.
        process_output_buffer(&mut out, 1, &telemetry, &mut c, &signals);
        assert!(signals.playback_complete.load(Ordering::Acquire));
    }

    #[test]
    fn flush_takes_precedence_over_normal_drain_even_with_data_available() {
        let telemetry = Telemetry::new(1);
        let signals = TransportSignals::new();
        let (mut p, mut c) = build_ring(48_000, 1, 200);

        // Plenty of "stale" data in the ring.
        push_all(&mut p, &[0.99_f32; 64]);
        signals.flush_request.store(true, Ordering::Release);
        signals.seek_target.store(0, Ordering::Release);

        let mut out = vec![0.42_f32; 16];
        process_output_buffer(&mut out, 1, &telemetry, &mut c, &signals);

        assert!(out.iter().all(|s| *s == 0.0), "stale samples never reached out");
        assert_eq!(c.slots(), 0);
    }

    #[test]
    fn stop_takes_precedence_over_flush() {
        // If both are set in the same callback, stop wins (graceful exit).
        let telemetry = Telemetry::new(1);
        let signals = TransportSignals::new();
        let (mut p, mut c) = build_ring(48_000, 1, 200);

        push_all(&mut p, &[0.1_f32; 16]);
        signals.flush_request.store(true, Ordering::Release);
        signals.stop_request.store(true, Ordering::Release);

        let mut out = vec![0.0_f32; 8];
        process_output_buffer(&mut out, 1, &telemetry, &mut c, &signals);

        assert!(out.iter().all(|s| *s == 0.0));
        // Ring untouched (stop didn't drain), flush flag still set
        // (will be cleared next callback when stop_request is gone, or
        // never if stop is permanent).
        assert_eq!(c.slots(), 16);
        assert!(signals.flush_request.load(Ordering::Acquire));
    }
}
