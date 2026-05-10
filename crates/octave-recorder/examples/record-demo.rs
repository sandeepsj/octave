//! Tiny end-to-end demo for `octave-recorder`.
//!
//! Records from the default input device for N seconds at 48 kHz stereo
//! (mono if the device only supports 1 channel), printing a live peak
//! meter, and writes a 32-bit float WAV to the path you give it.
//!
//! Run:
//!
//! ```sh
//! cargo run --example record-demo --release -- /tmp/take.wav 5
//! aplay /tmp/take.wav     # or open in Audacity / mpv / VLC
//! ```

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use octave_recorder::{BufferSize, DeviceCatalog, RecordingSpec};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: record-demo <output.wav> [duration_seconds]");
        return ExitCode::from(1);
    }
    let output_path = PathBuf::from(&args[1]);
    let duration_s: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5.0);

    // One catalog for the lifetime of the demo. `list_devices`
    // populates the device-handle cache; `open` reuses it (plan
    // §3.3.1) so the demo doesn't lose to PipeWire's ALSA exclusive-
    // grab race.
    let catalog = DeviceCatalog::new();
    let devices = catalog.list_devices();
    if devices.is_empty() {
        eprintln!("no input devices found");
        return ExitCode::from(2);
    }

    println!("input devices:");
    for (i, d) in devices.iter().enumerate() {
        let marker = if d.is_default_input { "*" } else { " " };
        println!(
            "  {marker} [{i}] {}  ({:?}, max {} ch)",
            d.name, d.backend, d.max_input_channels,
        );
    }

    let default = devices
        .iter()
        .find(|d| d.is_default_input)
        .unwrap_or(&devices[0]);
    println!("\nopening default: {}", default.name);

    let caps = match catalog.device_capabilities(&default.id) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("device_capabilities failed: {e}");
            return ExitCode::from(3);
        }
    };
    println!("  rates: {:?} Hz", caps.supported_sample_rates);
    println!("  channels: {:?}", caps.channels);

    let sample_rate = if caps.supported_sample_rates.contains(&48_000) {
        48_000
    } else {
        caps.default_sample_rate
    };
    let channels: u16 = if caps.channels.contains(&2) { 2 } else { 1 };

    let spec = RecordingSpec {
        device_id: default.id.clone(),
        sample_rate,
        buffer_size: BufferSize::Default,
        channels,
    };
    println!("  using: {sample_rate} Hz, {channels} ch, default buffer\n");

    let mut handle = match catalog.open(spec) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("open failed: {e}");
            return ExitCode::from(4);
        }
    };
    if let Err(e) = handle.arm() {
        eprintln!("arm failed: {e}");
        return ExitCode::from(5);
    }
    if let Err(e) = handle.record(&output_path) {
        eprintln!("record failed: {e}");
        return ExitCode::from(6);
    }

    println!(
        "recording {duration_s:.1} s to {} (Ctrl+C to abort)\n",
        output_path.display(),
    );

    let start = Instant::now();
    let dur = Duration::from_secs_f32(duration_s);
    while start.elapsed() < dur {
        let p0 = handle.peak_dbfs(0);
        let p1 = if channels >= 2 {
            handle.peak_dbfs(1)
        } else {
            f32::NEG_INFINITY
        };
        let line = format_meter_line(p0, p1, channels);
        eprint!("\r{line}");
        thread::sleep(Duration::from_millis(50));
    }
    eprintln!();

    let clip = match handle.stop() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\nstop failed: {e}");
            return ExitCode::from(7);
        }
    };
    handle.close();

    println!("\nrecording complete:");
    println!("  path:            {}", clip.path.display());
    println!("  duration:        {:.2} s", clip.duration_seconds);
    println!("  frames:          {}", clip.frame_count);
    println!("  xrun count:      {}", clip.xrun_count);
    println!("  dropped samples: {}", clip.dropped_samples);
    for (c, p) in clip.peak_dbfs.iter().enumerate() {
        println!("  ch{c} peak (take):  {p:6.1} dBFS");
    }
    if clip.dropped_samples > 0 {
        println!(
            "\nwarning: {} samples were dropped (writer too slow or ring undersized)",
            clip.dropped_samples,
        );
    }
    ExitCode::SUCCESS
}

const METER_WIDTH: usize = 30;

fn format_meter_line(p0: f32, p1: f32, channels: u16) -> String {
    let bar = |db: f32| -> String {
        let clamped = if db.is_finite() {
            db.clamp(-60.0, 0.0)
        } else {
            -60.0
        };
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
        )]
        let n = ((clamped + 60.0) / 60.0 * METER_WIDTH as f32) as usize;
        let mut s = String::with_capacity(METER_WIDTH);
        for _ in 0..n {
            s.push('=');
        }
        for _ in n..METER_WIDTH {
            s.push(' ');
        }
        s
    };
    if channels >= 2 {
        format!(
            " ch0 [{}] {:6.1}   ch1 [{}] {:6.1}",
            bar(p0),
            p0,
            bar(p1),
            p1,
        )
    } else {
        format!(" ch0 [{}] {:6.1}", bar(p0), p0)
    }
}
