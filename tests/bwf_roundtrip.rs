//! Writes real BWF files to disk and reads them back with bwavfile's own parser.
//!
//! The unit tests in `src/bwf.rs` check our arithmetic against itself. These check
//! that the bytes we emit are actually a Broadcast Wave file, which is the claim
//! that matters to anyone downstream.

use std::path::PathBuf;

use bwavfile::{WaveReader, WaveWriter};
use chrono::{Local, TimeZone};
use syncrec::bwf::{self, Provenance};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("syncrec-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// A known local wall-clock time as unix nanoseconds.
fn at_local(h: u32, m: u32, s: u32, nanos: u32) -> i128 {
    Local
        .with_ymd_and_hms(2026, 6, 15, h, m, s)
        .single()
        .expect("unambiguous local time")
        .timestamp() as i128
        * 1_000_000_000
        + nanos as i128
}

fn provenance(t0: i128, channels: u16) -> Provenance {
    Provenance {
        t0_unix_nanos: t0,
        sample_rate: 48_000,
        channels,
        bits_per_sample: 24,
        device_name: "Scarlett 2i2 USB".into(),
        device_rate: 48_000,
        measured_rate: Some(47_999.4),
        drift_ratio: Some(48_000.0 / 47_999.4),
        resampled: true,
        clock_source: "time.apple.com".into(),
        clock_dispersion_s: Some(0.004928),
        sync_state: "synced".into(),
        slope_ppm: Some(-16.28),
        latency_offset_ms: 0.0,
        timecode: None,
        session_id: None,
    }
}

/// Write a finished-style 24-bit BWF with full metadata.
fn write_take(path: &PathBuf, p: &Provenance, frames: &[f32]) {
    let mut w = WaveWriter::create(path, bwf::wave_fmt_pcm24(p.sample_rate, p.channels)).unwrap();
    // Both must precede the data chunk.
    w.write_broadcast_metadata(&bwf::bext(p)).unwrap();
    w.write_ixml(bwf::ixml(p).as_bytes()).unwrap();
    let mut fw = w.audio_frame_writer().unwrap();
    // Every 24-bit write must go through the clamp; see bwf::clamp_for_pcm24.
    let mut safe = frames.to_vec();
    bwf::clamp_buffer_for_pcm24(&mut safe);
    fw.write_frames(&safe).unwrap();
    fw.end().unwrap();
}

#[test]
fn a_written_take_validates_as_broadcast_wave() {
    let path = scratch("validates.wav");
    let p = provenance(at_local(14, 30, 0, 0), 2);
    write_take(&path, &p, &[0.0f32; 480]);

    let mut r = WaveReader::open(&path).unwrap();
    r.validate_readable().expect("must be a readable wave file");
    r.validate_broadcast_wave()
        .expect("must satisfy bwavfile's own BWF validation");
}

#[test]
fn the_format_chunk_says_what_we_asked_for() {
    let path = scratch("fmt.wav");
    let p = provenance(at_local(9, 0, 0, 0), 2);
    write_take(&path, &p, &[0.0f32; 480]);

    let mut r = WaveReader::open(&path).unwrap();
    let fmt = r.format().unwrap();
    assert_eq!(fmt.sample_rate, 48_000);
    assert_eq!(fmt.channel_count, 2);
    assert_eq!(fmt.bits_per_sample, 24);
    assert_eq!(fmt.block_alignment, 6);
    assert_eq!(
        r.frame_length().unwrap(),
        240,
        "480 samples over 2 channels"
    );
}

#[test]
fn time_reference_survives_the_round_trip_and_decodes_to_the_right_wall_time() {
    let path = scratch("tref.wav");
    // 14:30:00.25 local.
    let t0 = at_local(14, 30, 0, 250_000_000);
    let p = provenance(t0, 2);
    write_take(&path, &p, &[0.0f32; 480]);

    let mut r = WaveReader::open(&path).unwrap();
    let bext = r
        .broadcast_extension()
        .unwrap()
        .expect("bext must be present");

    let expected = (14 * 3600 + 30 * 60) * 48_000 + 12_000;
    assert_eq!(bext.time_reference, expected);
    // And it is the value our own maths produces, read back through the parser.
    assert_eq!(bext.time_reference, bwf::time_reference(t0, 48_000));

    // Decoding it back to a wall time must land where we started.
    let secs = bext.time_reference as f64 / 48_000.0;
    assert!((secs - (14.0 * 3600.0 + 30.0 * 60.0 + 0.25)).abs() < 1e-6);
}

#[test]
fn origination_date_and_time_survive_the_round_trip() {
    let path = scratch("date.wav");
    let t0 = at_local(7, 8, 9, 0);
    write_take(&path, &provenance(t0, 2), &[0.0f32; 96]);

    let mut r = WaveReader::open(&path).unwrap();
    let bext = r.broadcast_extension().unwrap().unwrap();
    assert_eq!(bext.origination_date, "2026-06-15");
    assert_eq!(bext.origination_time, "07:08:09");
}

#[test]
fn coding_history_carries_the_drift_numbers_into_the_file() {
    let path = scratch("history.wav");
    let p = provenance(at_local(12, 0, 0, 0), 2);
    write_take(&path, &p, &[0.0f32; 96]);

    let mut r = WaveReader::open(&path).unwrap();
    let bext = r.broadcast_extension().unwrap().unwrap();
    assert!(
        bext.coding_history
            .starts_with("A=PCM,F=48000,W=24,M=stereo")
    );
    for needle in [
        "drift_ratio:1.000012500",
        "clock_dispersion_ms:4.928",
        "resampled:yes",
    ] {
        assert!(
            bext.coding_history.contains(needle),
            "missing {needle} in:\n{}",
            bext.coding_history
        );
    }
}

#[test]
fn ixml_survives_the_round_trip_as_well_formed_xml() {
    let path = scratch("ixml.wav");
    let p = provenance(at_local(16, 45, 30, 0), 2);
    write_take(&path, &p, &[0.0f32; 96]);

    let mut r = WaveReader::open(&path).unwrap();
    let mut buf = Vec::new();
    let n = r.read_ixml(&mut buf).unwrap();
    assert!(n > 0, "iXML chunk must be present");

    let text = String::from_utf8(buf[..n].to_vec()).unwrap();
    assert!(text.contains("<BWFXML>"));
    assert!(text.contains("</BWFXML>"));
    assert!(
        text.contains("<DRIFT_RATIO>1.000012500</DRIFT_RATIO>"),
        "{text}"
    );
    assert!(
        text.contains("<CLOCK_SYNC_STATE>synced</CLOCK_SYNC_STATE>"),
        "{text}"
    );
}

#[test]
fn audio_survives_24_bit_quantisation_within_a_lsb() {
    let path = scratch("audio.wav");
    let p = provenance(at_local(10, 0, 0, 0), 1);

    // A ramp plus the extremes, so quantisation error is visible if it is wrong.
    let written: Vec<f32> = (0..512).map(|i| (i as f32 / 511.0) * 2.0 - 1.0).collect();
    write_take(&path, &p, &written);

    let r = WaveReader::open(&path).unwrap();
    let mut fr = r.audio_frame_reader().unwrap();
    let mut back = vec![0.0f32; written.len()];
    let frames = fr.read_frames(&mut back).unwrap();
    assert_eq!(frames as usize, written.len());

    // One 24-bit LSB is 2^-23; allow a couple for rounding.
    let tol = 3.0 / 8_388_608.0;
    for (i, (a, b)) in written.iter().zip(back.iter()).enumerate() {
        assert!((a - b).abs() <= tol, "sample {i}: wrote {a}, read {b}");
    }
}

#[test]
fn full_scale_does_not_wrap_to_negative() {
    // Regression test for a real defect: the float-to-24-bit conversion wraps
    // modulo, so +1.0 was being written to disk as -1.0. Anything at or above full
    // scale must clip to full scale, never flip polarity.
    let path = scratch("fullscale.wav");
    let p = provenance(at_local(10, 0, 0, 0), 1);
    let hot: Vec<f32> = vec![1.0, 1.5, 2.0, 0.999, -1.0, -1.5, -2.0, 0.0];
    write_take(&path, &p, &hot);

    let r = WaveReader::open(&path).unwrap();
    let mut fr = r.audio_frame_reader().unwrap();
    let mut back = vec![0.0f32; hot.len()];
    fr.read_frames(&mut back).unwrap();

    for (i, (a, b)) in hot.iter().zip(back.iter()).enumerate() {
        assert_eq!(
            a.signum() as i32 * (a.abs() > 0.0) as i32,
            b.signum() as i32 * (b.abs() > 0.0) as i32,
            "sample {i}: wrote {a}, read {b} -- polarity flipped"
        );
        assert!(b.abs() <= 1.0, "sample {i} read back out of range: {b}");
    }
    // The hot samples must sit at full scale, not somewhere near zero.
    assert!(
        back[0] > 0.999,
        "+1.0 should clip to full scale, got {}",
        back[0]
    );
    assert!(
        back[1] > 0.999,
        "+1.5 should clip to full scale, got {}",
        back[1]
    );
    assert!(
        back[2] > 0.999,
        "+2.0 should clip to full scale, got {}",
        back[2]
    );
    assert!(
        back[5] < -0.999,
        "-1.5 should clip to full scale, got {}",
        back[5]
    );
}

#[test]
fn the_float_scratch_format_round_trips_bit_exactly() {
    // The live capture file is float precisely so it loses nothing before the
    // single quantisation that happens when the finished take is written.
    let path = scratch("scratch.wav");
    let written: Vec<f32> = vec![0.0, 1.0, -1.0, 0.1234567, -0.7654321, 1e-8, -1e-8, 0.5];

    let w = WaveWriter::create(&path, bwf::wave_fmt_f32(48_000, 1)).unwrap();
    let mut fw = w.audio_frame_writer().unwrap();
    fw.write_frames(&written).unwrap();
    fw.end().unwrap();

    let mut r = WaveReader::open(&path).unwrap();
    assert_eq!(r.format().unwrap().bits_per_sample, 32);
    let mut fr = r.audio_frame_reader().unwrap();
    let mut back = vec![0.0f32; written.len()];
    fr.read_frames(&mut back).unwrap();
    assert_eq!(back, written, "the scratch file must be lossless");
}

#[test]
fn an_eight_channel_take_is_still_a_valid_bwf() {
    let path = scratch("eight.wav");
    let p = provenance(at_local(11, 11, 11, 0), 8);
    write_take(&path, &p, &[0.25f32; 8 * 100]);

    let mut r = WaveReader::open(&path).unwrap();
    r.validate_broadcast_wave()
        .expect("8ch must still validate");
    let fmt = r.format().unwrap();
    assert_eq!(fmt.channel_count, 8);
    assert_eq!(fmt.block_alignment, 24);
    assert_eq!(r.frame_length().unwrap(), 100);
    assert!(
        r.broadcast_extension().unwrap().is_some(),
        "bext must survive the extended fmt record"
    );
}

#[test]
fn an_unsynced_take_is_labelled_as_such_in_the_file() {
    let path = scratch("unsynced.wav");
    let mut p = provenance(at_local(3, 0, 0, 0), 2);
    p.sync_state = "unsynced".into();
    p.clock_dispersion_s = None;
    p.measured_rate = None;
    p.drift_ratio = None;
    p.resampled = false;
    write_take(&path, &p, &[0.0f32; 96]);

    let mut r = WaveReader::open(&path).unwrap();
    let bext = r.broadcast_extension().unwrap().unwrap();
    // Someone opening this file must be able to tell the timestamp is not trusted.
    assert!(bext.coding_history.contains("clock_sync:unsynced"));
    assert!(bext.coding_history.contains("clock_dispersion_ms:unknown"));
    assert!(bext.coding_history.contains("resampled:no"));
}
