//! Headless end-to-end capture check.
//!
//! Usage: record-probe [seconds] [ntp-server]
//!
//! Exercises the whole chain with no UI: SNTP -> clock model -> device -> ring
//! buffer -> writer thread -> raw WAV + sidecar, then reports what the device's
//! true sample rate turned out to be.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use syncrec::audio::{self, session};
use syncrec::clock::{ClockModel, NtpReference, SyncState, sntp};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let server = args.next().unwrap_or_else(|| "time.apple.com".into());

    let base = Instant::now();
    let clock = Arc::new(Mutex::new(ClockModel::new(base)));
    let _poller = sntp::Poller::spawn(server.clone(), Arc::clone(&clock));

    // The clock normally has a full window by the time anyone hits record; here we
    // wait for a first fix so t0 resolves immediately rather than being parked.
    print!("waiting for first NTP fix from {server} ");
    for _ in 0..40 {
        if clock.lock().unwrap().state() != SyncState::Unsynced {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
        print!(".");
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
    println!();

    let devices = audio::input_devices()?;
    if devices.is_empty() {
        anyhow::bail!("no input devices found");
    }
    println!("input devices:");
    for d in &devices {
        println!("  - {d}");
    }
    let choice = devices
        .iter()
        .find(|d| d.is_default)
        .unwrap_or(&devices[0])
        .clone();

    let device = audio::find_device(&choice)?;
    let negotiated = audio::negotiate(&device)?;
    println!(
        "\nusing    {choice}\nformat   {} ch @ {} Hz, {:?}{}",
        negotiated.channels,
        negotiated.rate,
        negotiated.sample_format,
        if negotiated.native_target_rate {
            ""
        } else {
            "  (device refused 48 kHz; will be resampled)"
        }
    );

    let capture = audio::capture::build(&device, &negotiated).context("building capture")?;
    println!(
        "bridge   +/- {:?} clock correlation",
        capture.bridge.uncertainty()
    );

    let dir = std::env::temp_dir().join("syncrec-probe");
    std::fs::create_dir_all(&dir)?;
    let paths = audio::writer::next_take(&dir, "probe");
    println!("writing  {}", paths.raw.display());

    let recording = session::start(
        capture,
        Box::new(NtpReference::new(server.clone(), Arc::clone(&clock))),
        session::SessionConfig {
            paths,
            device_name: choice.name.clone(),
            latency: syncrec::latency::LatencyCorrection::platform_only(
                syncrec::latency::InputLatency::query(Some(&choice.name)),
            ),
        },
    )?;

    println!("\nrecording {seconds}s...");
    let rate = recording.rate;
    for _ in 0..seconds {
        std::thread::sleep(Duration::from_secs(1));
        let p = &recording.progress;
        print!(
            "\r  {:>6.2}s  frames {:>9}  marks {:>4}  overruns {}",
            p.elapsed(rate).as_secs_f64(),
            p.frames(),
            p.observations(),
            p.overruns()
        );
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
    println!();

    let take = recording.stop()?;
    let s = &take.sidecar;

    println!("\n--- take ---");
    println!("raw frames   {}", s.raw_frames);
    println!("overruns     {}", s.overruns);
    println!("sync state   {}", s.sync_state);
    println!("observations {}", s.observations.len());
    println!("marks lost   {}", s.marks_abandoned);
    if let Some(e) = &s.stream_error {
        println!("stream error {e}");
    }
    match s.t0_unix_nanos {
        Some(t0) => println!("t0           {}", syncrec::bwf::iso8601_nanos(t0)),
        None => println!("t0           UNRESOLVED (clock never synced)"),
    }
    if let Some(d) = s.clock_dispersion_s {
        println!("dispersion   {:.3} ms", d * 1e3);
    }

    // Invariants the whole time base depends on. A take whose marks do not start
    // at sample 0, or run past the end of the file, has lost its anchor — that
    // happened for real when the capture stream was shared with a monitoring pass
    // and kept counting frames between takes.
    let mut bad = false;
    match s.observations.first() {
        Some(first) if first.sample_index == 0 => {}
        Some(first) => {
            println!(
                "\nFAIL: first observation is at sample {}, not 0 — the take has no anchor",
                first.sample_index
            );
            bad = true;
        }
        None => println!("\nWARN: no drift observations resolved"),
    }
    if let Some(last) = s.observations.last()
        && last.sample_index > s.raw_frames
    {
        println!(
            "FAIL: observation at sample {} is past the end of a {}-frame file",
            last.sample_index, s.raw_frames
        );
        bad = true;
    }
    if s.t0_unix_nanos.is_none() && s.sync_state != "unsynced" {
        println!("FAIL: clock was {} yet t0 never resolved", s.sync_state);
        bad = true;
    }
    if !bad && !s.observations.is_empty() {
        println!("\ninvariants OK: marks start at sample 0 and stay inside the file");
    }

    // A crude two-point rate estimate. The real fit lives in finalize.
    if let (Some(first), Some(last)) = (s.observations.first(), s.observations.last()) {
        let dn = last.sample_index as f64 - first.sample_index as f64;
        let dt = (last.utc_unix_nanos - first.utc_unix_nanos) as f64 / 1.0e9;
        if dt > 0.0 && dn > 0.0 {
            let measured = dn / dt;
            println!(
                "\nmeasured rate {:.3} Hz over {:.1}s  ({:+.1} ppm vs nominal {})",
                measured,
                dt,
                (measured / s.device_rate as f64 - 1.0) * 1e6,
                s.device_rate
            );
        }
    }
    // Finalise exactly as the app does: fit the drift, run the safety gate, and
    // only resample and delete the scratch capture if the gate is satisfied.
    println!("\n--- finalize ---");
    let provenance = syncrec::bwf::Provenance {
        t0_unix_nanos: s.t0_unix_nanos.unwrap_or(0),
        sample_rate: audio::TARGET_RATE,
        channels: s.channels,
        bits_per_sample: 24,
        device_name: s.device_name.clone(),
        device_rate: s.device_rate,
        measured_rate: None,
        drift_ratio: None,
        resampled: false,
        clock_source: s.clock_source.clone(),
        clock_dispersion_s: s.clock_dispersion_s,
        sync_state: s.sync_state.clone(),
        slope_ppm: s.clock_slope_ppm,
        latency_offset_ms: 0.0,
        timecode: None,
        session_id: s.session_id.clone(),
    };
    let outcome = syncrec::finalize::finalize(
        &take.paths.raw,
        &take.paths.final_wav,
        &s.observations,
        &take.status,
        &provenance,
    )?;

    println!("gate passed   {}", outcome.gate.passed());
    if let Some(reason) = outcome.gate.reason() {
        println!("gate reason   {reason}");
    }
    if let Some(f) = &outcome.fit {
        println!(
            "fit           {:.4} Hz measured ({:+.2} ppm), ratio {:.9}",
            f.measured_rate,
            f.crystal_ppm(s.device_rate),
            f.drift_ratio
        );
        println!(
            "              {} points over {:.1}s, residual {:.1} us",
            f.points,
            f.span_seconds,
            f.residual_rms_s * 1e6
        );
    }
    println!("resampled     {}", outcome.resampled);
    println!("raw deleted   {}", outcome.raw_deleted);
    println!("frames out    {}", outcome.frames_written);
    println!("\nfiles in {}", take.paths.final_wav.parent().unwrap().display());
    if bad {
        anyhow::bail!("time-base invariants violated");
    }
    Ok(())
}
