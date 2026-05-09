//! The reader thread: pulls frames from a [`PlaybackSource`] and
//! pushes them into the SPSC ring the audio callback drains.
//!
//! Owns the [`PlaybackSource`] trait object, the producer end of the
//! ring, and a clone of the [`TransportSignals`]. Lives at normal OS
//! priority — never touches the audio thread, may allocate / lock /
//! syscall freely.
//!
//! See `docs/modules/playback-audio.md` §3.5 (reader side), §5.6
//! (seek-flush handshake), §5.8 (EOF detection), §7.4 (cancellation).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rtrb::Producer;

use crate::signals::TransportSignals;
use crate::source::PlaybackSource;

/// How many frames to pull per source call. Tuned to amortize the
/// pull / push overhead while keeping ring residency low so seek
/// latency (§6.2) stays under budget.
const READ_BLOCK_FRAMES: usize = 4_096;

/// How long the reader sleeps when the ring is full or it's idling
/// after EOF / pre-seek. Short enough to react to seek/stop signals
/// within the §6.2 50 ms budget.
pub(crate) const PARK_DURATION: Duration = Duration::from_millis(1);

/// Per-thread state the reader carries across iterations. Exposed so
/// `reader_step` can be called repeatedly from a test without
/// spawning an OS thread.
#[derive(Default)]
pub(crate) struct ReaderState {
    /// True while the reader has set `flush_request` and is waiting
    /// for the audio callback to clear it. Once cleared, the reader
    /// performs the actual `source.seek` and resumes.
    waiting_for_audio_ack: bool,
}

impl ReaderState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// One iteration's worth of work for the reader. Returns whether the
/// caller should continue, sleep briefly, or exit.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReaderAction {
    /// Make progress on the next iteration with no delay.
    Continue,
    /// Ring is full, EOF is set, or we're waiting for the audio thread
    /// to ack a flush — sleep `PARK_DURATION` then loop.
    Sleep,
    /// `stop_request` was set; the reader has dropped its source and
    /// the loop should exit.
    Exit,
}

/// Single iteration of the reader's main loop.
///
/// Pure (apart from pushing into the ring and reading from the
/// source) — only atomic loads/stores on `signals`. Test by calling
/// repeatedly with a hand-built `BufferSource` and a fresh ring.
pub(crate) fn reader_step<S: PlaybackSource + ?Sized>(
    state: &mut ReaderState,
    source: &mut S,
    producer: &mut Producer<f32>,
    signals: &TransportSignals,
    scratch: &mut [f32],
) -> ReaderAction {
    if signals.stop_request.load(Ordering::Acquire) {
        return ReaderAction::Exit;
    }

    // ---------- seek handshake ----------
    if signals.seek_pending.load(Ordering::Acquire) {
        if !state.waiting_for_audio_ack {
            // Fresh seek — kick the audio thread to drain the ring
            // and jump position. Reset EOF so playback can continue
            // from the new position.
            signals.eof_seen.store(false, Ordering::Release);
            signals.playback_complete.store(false, Ordering::Release);
            signals.flush_request.store(true, Ordering::Release);
            state.waiting_for_audio_ack = true;
            return ReaderAction::Sleep;
        }
        if signals.flush_request.load(Ordering::Acquire) {
            // Audio hasn't acked yet — keep waiting.
            return ReaderAction::Sleep;
        }
        // Audio acked. Reposition the source and clear seek_pending.
        // Source seek failures are best-effort: if the underlying
        // file disappeared or seeking hit an I/O error, we treat it
        // as EOF for this take. The user-visible signal is the same
        // as a normal EOF (state → Ended).
        let target = signals.seek_target.load(Ordering::Acquire);
        if source.seek(target).is_err() {
            signals.eof_seen.store(true, Ordering::Release);
        }
        signals.seek_pending.store(false, Ordering::Release);
        state.waiting_for_audio_ack = false;
        return ReaderAction::Continue;
    }

    // EOF: reader is done pulling, just idle until stop or seek.
    if signals.eof_seen.load(Ordering::Acquire) {
        return ReaderAction::Sleep;
    }

    // ---------- normal pull ----------
    let ch = usize::from(source.channels());
    let pull_capacity = (scratch.len() / ch) * ch;
    if pull_capacity == 0 {
        return ReaderAction::Sleep;
    }
    let ring_writable = (producer.slots() / ch) * ch;
    if ring_writable == 0 {
        return ReaderAction::Sleep;
    }
    let want_samples = pull_capacity.min(ring_writable);
    let want_frames = want_samples / ch;

    let frames_pulled = source.pull(&mut scratch[..want_samples]);
    if frames_pulled == 0 {
        signals.eof_seen.store(true, Ordering::Release);
        return ReaderAction::Sleep;
    }

    // Push into ring. The ring has room for `want_samples` and we
    // pulled ≤ want_frames frames, so the chunk fits.
    let pushed_samples = frames_pulled * ch;
    match producer.write_chunk_uninit(pushed_samples) {
        Ok(mut chunk) => {
            let (s1, s2) = chunk.as_mut_slices();
            let n1 = s1.len();
            // SAFETY: `MaybeUninit<f32>` has the same layout as `f32`;
            // the two slices together cover exactly `pushed_samples`
            // contiguous slots, and we initialize every slot before
            // commit. Mirror of the recorder's RT push.
            unsafe {
                let src = scratch.as_ptr();
                std::ptr::copy_nonoverlapping(src, s1.as_mut_ptr().cast::<f32>(), n1);
                if n1 < pushed_samples {
                    std::ptr::copy_nonoverlapping(
                        src.add(n1),
                        s2.as_mut_ptr().cast::<f32>(),
                        pushed_samples - n1,
                    );
                }
                chunk.commit_all();
            }
        }
        Err(_) => {
            // Shouldn't happen given the slots() check above.
            return ReaderAction::Sleep;
        }
    }

    // Hit EOF mid-pull (source returned fewer frames than requested).
    if frames_pulled < want_frames {
        signals.eof_seen.store(true, Ordering::Release);
    }

    ReaderAction::Continue
}

/// Spawn the reader thread. The returned [`JoinHandle`] drops the
/// source and exits cleanly once `signals.stop_request` is set.
pub(crate) fn spawn_reader<S>(
    mut source: Box<S>,
    mut producer: Producer<f32>,
    signals: Arc<TransportSignals>,
) -> JoinHandle<()>
where
    S: PlaybackSource + ?Sized + 'static,
{
    thread::Builder::new()
        .name("octave-playback-reader".into())
        .spawn(move || {
            // Heap-alloc the scratch once — non-RT thread, allocation
            // is fine. Sized to one READ_BLOCK at the source's channel
            // count.
            let ch = usize::from(source.channels());
            let scratch_samples = READ_BLOCK_FRAMES * ch;
            let mut scratch = vec![0.0_f32; scratch_samples];

            let mut state = ReaderState::new();
            loop {
                match reader_step(
                    &mut state,
                    &mut *source,
                    &mut producer,
                    &signals,
                    &mut scratch,
                ) {
                    ReaderAction::Continue => {}
                    ReaderAction::Sleep => thread::sleep(PARK_DURATION),
                    ReaderAction::Exit => return,
                }
            }
        })
        .expect("OS refused to spawn reader thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ring::build_ring;
    use crate::source::BufferSource;

    fn ramp_source(frames: usize, sample_rate: u32) -> BufferSource {
        let mut v = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            v.push(i as f32);
            v.push(-(i as f32));
        }
        BufferSource::new(Arc::from(v), sample_rate, 2).unwrap()
    }

    fn fresh() -> (
        ReaderState,
        TransportSignals,
        Vec<f32>,
        rtrb::Producer<f32>,
        rtrb::Consumer<f32>,
    ) {
        let signals = TransportSignals::new();
        let (p, c) = build_ring(48_000, 2, 200);
        (
            ReaderState::new(),
            signals,
            vec![0.0_f32; READ_BLOCK_FRAMES * 2],
            p,
            c,
        )
    }

    #[test]
    fn step_pushes_source_samples_into_ring() {
        let mut src = ramp_source(8, 48_000);
        let (mut state, signals, mut scratch, mut p, mut c) = fresh();

        let action = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
        // Source had 8 frames; READ_BLOCK is much larger; pull returns 8 < want
        // → eof_seen flips to true on this same iteration.
        assert_eq!(action, ReaderAction::Continue);
        assert_eq!(c.slots(), 16);
        assert!(signals.eof_seen.load(Ordering::Acquire));

        let chunk = c.read_chunk(16).unwrap();
        let (s1, s2) = chunk.as_slices();
        let mut got = Vec::new();
        got.extend_from_slice(s1);
        got.extend_from_slice(s2);
        chunk.commit_all();
        for (i, pair) in got.chunks(2).enumerate() {
            assert_eq!(pair[0], i as f32);
            assert_eq!(pair[1], -(i as f32));
        }
    }

    #[test]
    fn step_returns_exit_on_stop_request() {
        let mut src = ramp_source(8, 48_000);
        let (mut state, signals, mut scratch, mut p, _c) = fresh();
        signals.stop_request.store(true, Ordering::Release);
        let action = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
        assert_eq!(action, ReaderAction::Exit);
    }

    #[test]
    fn step_after_eof_idles() {
        let mut src = ramp_source(4, 48_000);
        let (mut state, signals, mut scratch, mut p, _c) = fresh();

        // Pull all 4 frames + flip eof.
        let _ = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
        assert!(signals.eof_seen.load(Ordering::Acquire));

        // Subsequent step is Sleep, no further pull.
        let writable_before = p.slots();
        let action = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
        assert_eq!(action, ReaderAction::Sleep);
        assert_eq!(p.slots(), writable_before);
    }

    #[test]
    fn full_seek_handshake_repositions_source() {
        let mut src = ramp_source(20, 48_000);
        let (mut state, signals, mut scratch, mut p, mut c) = fresh();

        // Pull some samples first (8 frames push).
        let _ = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
        assert_eq!(c.slots(), 40); // 20 frames * 2 ch

        // Drain partially so the ring has stale samples in it.
        let drain = c.read_chunk(8).unwrap();
        drain.commit_all();

        // UI requests seek to frame 5.
        signals.request_seek(5);

        // Step 1: reader observes seek_pending, kicks flush_request, sleeps.
        let action = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
        assert_eq!(action, ReaderAction::Sleep);
        assert!(signals.flush_request.load(Ordering::Acquire));
        assert!(state.waiting_for_audio_ack);

        // Step 2: still waiting (flush still set) → Sleep.
        let action = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
        assert_eq!(action, ReaderAction::Sleep);
        assert!(state.waiting_for_audio_ack);

        // Audio acks (clears flush_request).
        signals.flush_request.store(false, Ordering::Release);

        // Step 3: reader observes ack, calls source.seek(5), clears
        // seek_pending, returns Continue.
        let action = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
        assert_eq!(action, ReaderAction::Continue);
        assert!(!signals.seek_pending.load(Ordering::Acquire));
        assert!(!state.waiting_for_audio_ack);

        // Drain the now-stale ring contents (left over from the first pull).
        let stale = c.slots();
        if stale > 0 {
            let chunk = c.read_chunk(stale).unwrap();
            chunk.commit_all();
        }

        // Step 4: source is now at frame 5; pulls remaining 15 frames into ring.
        let _ = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
        // Verify the next frame in ring is frame 5 (ramp value 5).
        let chunk = c.read_chunk(2).unwrap();
        let (s, _) = chunk.as_slices();
        assert_eq!(s[0], 5.0);
        assert_eq!(s[1], -5.0);
        chunk.commit_all();
    }

    #[test]
    fn seek_resets_eof_so_playback_can_resume_from_new_position() {
        let mut src = ramp_source(4, 48_000);
        let (mut state, signals, mut scratch, mut p, _c) = fresh();

        // Drive to EOF.
        let _ = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
        assert!(signals.eof_seen.load(Ordering::Acquire));

        // Seek to start.
        signals.request_seek(0);
        let _ = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch); // kick flush
        assert!(!signals.eof_seen.load(Ordering::Acquire), "seek clears EOF");
        assert!(!signals.playback_complete.load(Ordering::Acquire), "seek clears complete");
    }

    #[test]
    fn seek_to_out_of_bounds_marks_source_as_eof() {
        let mut src = ramp_source(4, 48_000);
        let (mut state, signals, mut scratch, mut p, _c) = fresh();

        signals.request_seek(100); // way past end (only 4 frames exist)
        // Step 1 kicks flush.
        let _ = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
        // Audio acks.
        signals.flush_request.store(false, Ordering::Release);
        // Step 2: reader calls source.seek(100), gets OutOfBounds, sets EOF.
        let _ = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
        assert!(signals.eof_seen.load(Ordering::Acquire));
        assert!(!signals.seek_pending.load(Ordering::Acquire));
    }

    #[test]
    fn full_ring_returns_sleep_without_pull() {
        let mut src = ramp_source(10_000, 48_000);
        let signals = TransportSignals::new();
        let mut state = ReaderState::new();
        let (mut p, _c) = build_ring(48_000, 2, 100); // 100 ms = 4800 samples
        let mut scratch = vec![0.0_f32; READ_BLOCK_FRAMES * 2];

        // Pump until ring fills.
        loop {
            let action = reader_step(&mut state, &mut src, &mut p, &signals, &mut scratch);
            if action == ReaderAction::Sleep {
                break;
            }
            assert_eq!(action, ReaderAction::Continue);
        }
        assert_eq!(p.slots(), 0);
    }

    #[test]
    fn spawn_reader_drains_source_and_exits_on_stop() {
        let src = Box::new(ramp_source(100, 48_000));
        let signals = Arc::new(TransportSignals::new());
        let (p, mut c) = build_ring(48_000, 2, 200);

        let h = spawn_reader(src, p, Arc::clone(&signals));

        // Wait until the reader has signalled EOF or pushed all 200 samples.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while c.slots() < 200 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(c.slots(), 200, "reader pushed all 100 stereo frames");

        signals.stop_request.store(true, Ordering::Release);
        h.join().expect("reader thread panicked");
    }
}
