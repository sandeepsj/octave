//! Tiny end-to-end demo for `octave-player`.
//!
//! Plays a 32-bit float WAV (recorder-written or compatible) through
//! the system default output device. Prints a live peak meter and a
//! position counter; reports stats when playback ends or the user
//! Ctrl-C's. Mirror of `octave-recorder`'s `record-demo`.
//!
//! Run:
//!
//! ```sh
//! cargo run --release --example play-demo -- /tmp/take.wav
//! ```

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use octave_player::{
    BufferSize, PlaybackSourceSpec, PlaybackSpec, PlaybackState, list_output_devices,
    output_device_capabilities, open,
};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: play-demo <input.wav>");
        return ExitCode::from(1);
    }
    let input_path = PathBuf::from(&args[1]);

    let devices = list_output_devices();
    if devices.is_empty() {
        eprintln!("no output devices found");
        return ExitCode::from(2);
    }

    println!("output devices:");
    for (i, d) in devices.iter().enumerate() {
        let marker = if d.is_default_output { "*" } else { " " };
        println!(
            "  {marker} [{i}] {}  ({:?}, max {} ch)",
            d.name, d.backend, d.max_output_channels,
        );
    }

    let default = devices
        .iter()
        .find(|d| d.is_default_output)
        .unwrap_or(&devices[0]);
    println!("\nopening default: {}", default.name);

    let caps = match output_device_capabilities(&default.id) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("output_device_capabilities failed: {e}");
            return ExitCode::from(3);
        }
    };
    println!("  rates: {:?} Hz", caps.supported_sample_rates);
    println!("  channels: {:?}\n", caps.channels);

    let spec = PlaybackSpec {
        device_id: default.id.clone(),
        source: PlaybackSourceSpec::File { path: input_path.clone() },
        buffer_size: BufferSize::Default,
    };

    let mut handle = match open(spec) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("open failed: {e}");
            return ExitCode::from(4);
        }
    };

    let sr = handle.sample_rate();
    let ch = handle.channels();
    let dur_frames = handle.status().duration_frames.unwrap_or(0);
    // Frame counts and percentages — exact for any real session
    // (2^52 frames @ 192 kHz ≈ 743 years).
    #[allow(clippy::cast_precision_loss)]
    let dur_secs = dur_frames as f64 / f64::from(sr);
    println!(
        "playing {} ({:.2} s, {sr} Hz, {ch} ch)\n",
        input_path.display(),
        dur_secs,
    );

    // Poll the meter at ~30 Hz until playback completes (or fails).
    loop {
        let status = handle.status();
        let p0 = handle.peak_dbfs(0);
        let p1 = if ch >= 2 {
            handle.peak_dbfs(1)
        } else {
            f32::NEG_INFINITY
        };
        #[allow(clippy::cast_precision_loss)]
        let pos_pct = if dur_frames > 0 {
            100.0 * (status.position_frames as f64 / dur_frames as f64)
        } else {
            0.0
        };
        let line = format_meter_line(p0, p1, ch, status.position_seconds, pos_pct);
        eprint!("\r{line}");

        match status.state {
            PlaybackState::Ended | PlaybackState::Stopped | PlaybackState::Closed
            | PlaybackState::Errored => break,
            _ => {}
        }
        thread::sleep(Duration::from_millis(33));
    }
    eprintln!();

    let final_status = match handle.stop() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("\nstop failed: {e}");
            return ExitCode::from(5);
        }
    };
    handle.close();

    println!("\nplayback complete:");
    println!("  position:        {:.2} s", final_status.position_seconds);
    println!("  duration:        {:.2} s", final_status.duration_seconds.unwrap_or(0.0));
    println!("  xrun count:      {}", final_status.xrun_count);
    println!("  state:           {:?}", final_status.state);
    if final_status.xrun_count > 0 {
        println!(
            "\nwarning: {} under-runs (audio thread didn't get samples in time)",
            final_status.xrun_count
        );
    }
    ExitCode::SUCCESS
}

const METER_WIDTH: usize = 30;

fn format_meter_line(p0: f32, p1: f32, channels: u16, secs: f64, pct: f64) -> String {
    let bar = |db: f32| -> String {
        let clamped = if db.is_finite() { db.clamp(-60.0, 0.0) } else { -60.0 };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        let n = ((clamped + 60.0) / 60.0 * METER_WIDTH as f32) as usize;
        let mut s = String::with_capacity(METER_WIDTH);
        for _ in 0..n { s.push('='); }
        for _ in n..METER_WIDTH { s.push(' '); }
        s
    };
    if channels >= 2 {
        format!(
            " {:6.2}s ({:5.1}%)  ch0 [{}] {:6.1}   ch1 [{}] {:6.1}",
            secs, pct, bar(p0), p0, bar(p1), p1,
        )
    } else {
        format!(" {:6.2}s ({:5.1}%)  ch0 [{}] {:6.1}", secs, pct, bar(p0), p0)
    }
}
