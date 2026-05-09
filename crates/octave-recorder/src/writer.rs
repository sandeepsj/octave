//! Writer-thread loop: drain the rtrb ring into the WAV file, off the RT path.
//!
//! See `docs/modules/record-audio.md` §3.5 (writer loop) and §7
//! (concurrency model). The writer is a normal-priority thread — it may
//! allocate, lock, and call `write` / `fsync`. It must never touch the
//! audio thread's atomics in a way that would block the audio thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rtrb::Consumer;

use crate::wav::{FinalizedWav, WavWriter};

/// How many samples to drain in one `write_all` call. Tuned to be a few
/// audio buffers worth so we don't burn CPU on tiny chunks but also
/// don't starve when the producer is bursty.
const DRAIN_BLOCK: usize = 4_096;

/// How long to park when the ring is empty and nothing is signaled.
const PARK_DURATION: Duration = Duration::from_millis(2);

#[derive(Debug)]
pub(crate) struct WriterOutcome {
    /// `Some` on a clean stop; `None` is the canonical cancel signal —
    /// the audio.rs cancel path uses `finalized.is_none()` as its
    /// proof the writer observed the cancel flag and bailed out
    /// without finalizing.
    pub finalized: Option<FinalizedWav>,
    pub path: PathBuf,
}

/// Spawn the writer thread. Owns the ring's consumer end and the WAV
/// file. `stop` and `cancel` are signaled by the API thread and read by
/// the writer with `Ordering::Acquire`.
pub(crate) fn spawn_writer(
    consumer: Consumer<f32>,
    writer: WavWriter,
    path: PathBuf,
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
) -> JoinHandle<std::io::Result<WriterOutcome>> {
    thread::Builder::new()
        .name("octave-recorder-writer".into())
        .spawn(move || writer_loop(consumer, writer, path, stop, cancel))
        .expect("writer thread spawn must succeed")
}

fn writer_loop(
    mut consumer: Consumer<f32>,
    mut writer: WavWriter,
    path: PathBuf,
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
) -> std::io::Result<WriterOutcome> {
    let ch = usize::from(writer.channels());

    loop {
        if cancel.load(Ordering::Acquire) {
            // Drop the writer (closes the file); caller deletes the path.
            drop(writer);
            return Ok(WriterOutcome {
                finalized: None,
                path,
            });
        }

        // Round down to a whole-frame boundary; partial frames wait.
        let available = consumer.slots();
        let n = (available.min(DRAIN_BLOCK) / ch) * ch;

        if n > 0 {
            let chunk = consumer
                .read_chunk(n)
                .map_err(|e| std::io::Error::other(format!("rtrb read_chunk: {e:?}")))?;
            let (s1, s2) = chunk.as_slices();
            writer.write_frames(s1)?;
            if !s2.is_empty() {
                writer.write_frames(s2)?;
            }
            chunk.commit_all();
        } else if stop.load(Ordering::Acquire) {
            let fin = writer.finalize()?;
            return Ok(WriterOutcome {
                finalized: Some(fin),
                path,
            });
        } else {
            thread::sleep(PARK_DURATION);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::tempdir;

    use crate::ring::build_ring;
    use crate::test_support::sine_stereo;

    fn push_all(producer: &mut rtrb::Producer<f32>, samples: &[f32]) {
        for s in samples {
            // In tests the ring is sized larger than the push, so push() never errors.
            producer.push(*s).expect("ring full in test");
        }
    }

    #[test]
    fn writer_drains_ring_and_finalizes_on_stop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("happy.wav");

        let (mut producer, consumer) = build_ring(48_000, 2, 200);
        let writer = WavWriter::create(&path, 48_000, 2).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let h = spawn_writer(consumer, writer, path.clone(), stop.clone(), cancel.clone());

        let frames = sine_stereo(500, 48_000, 220.0);
        push_all(&mut producer, &frames);

        stop.store(true, Ordering::Release);
        let outcome = h.join().expect("writer panicked").expect("writer io error");

        assert_eq!(outcome.path, path);
        let fin = outcome.finalized.expect("finalized clip on stop");
        assert_eq!(fin.frame_count, 500);
        assert!(!fin.promoted_to_rf64);

        let mut reader = hound::WavReader::open(&path).unwrap();
        let read: Vec<f32> = reader.samples::<f32>().map(Result::unwrap).collect();
        assert_eq!(read.len(), frames.len());
        for (i, (got, want)) in read.iter().zip(frames.iter()).enumerate() {
            assert_eq!(got.to_bits(), want.to_bits(), "sample {i}");
        }
    }

    #[test]
    fn writer_returns_cancelled_outcome_without_finalizing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cancelled.wav");

        let (mut producer, consumer) = build_ring(48_000, 2, 200);
        let writer = WavWriter::create(&path, 48_000, 2).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let h = spawn_writer(consumer, writer, path.clone(), stop.clone(), cancel.clone());

        push_all(&mut producer, &sine_stereo(200, 48_000, 220.0));
        cancel.store(true, Ordering::Release);
        let outcome = h.join().expect("writer panicked").expect("writer io error");

        // Cancel signal: finalized is None (the canonical "writer
        // observed cancel and bailed without finalizing" proof).
        assert!(outcome.finalized.is_none());
        // File exists but is unfinalized; caller is responsible for deleting.
        assert!(path.exists());
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn writer_handles_ring_wrap_over_a_long_recording() {
        // Headroom of 50 ms = 4 800 frames stereo. Push 12 000 frames in
        // chunks with sleeps so the writer drains and the ring wraps.
        let dir = tempdir().unwrap();
        let path = dir.path().join("wrap.wav");

        let (mut producer, consumer) = build_ring(48_000, 2, 50);
        let writer = WavWriter::create(&path, 48_000, 2).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let h = spawn_writer(consumer, writer, path.clone(), stop.clone(), cancel.clone());

        let frames = sine_stereo(12_000, 48_000, 440.0);
        for chunk in frames.chunks(2_000) {
            push_all(&mut producer, chunk);
            // Give the writer time to drain so we don't wedge on a full ring.
            thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Release);
        let outcome = h.join().expect("writer panicked").expect("writer io error");

        let fin = outcome.finalized.expect("finalized clip");
        assert_eq!(fin.frame_count, 12_000);

        let mut reader = hound::WavReader::open(&path).unwrap();
        let read: Vec<f32> = reader.samples::<f32>().map(Result::unwrap).collect();
        assert_eq!(read.len(), frames.len());
        for (i, (got, want)) in read.iter().zip(frames.iter()).enumerate() {
            assert_eq!(got.to_bits(), want.to_bits(), "sample {i}");
        }
    }
}
