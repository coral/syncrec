//! Broadcast Wave metadata: the `bext` chunk, iXML, and the timecode maths.
//!
//! `bext`'s numeric timestamp field, `TimeReference`, counts samples since *local*
//! midnight and carries no date, which is why `OriginationDate` and
//! `OriginationTime` have to be filled in alongside it. Everything here is derived
//! from `t0` — the moment the first sample hit the converter — so it is all known
//! before the first frame is written.

use bwavfile::{
    Bext, WAVE_TAG_EXTENDED, WAVE_TAG_FLOAT, WAVE_TAG_PCM, WAVE_UUID_FLOAT, WAVE_UUID_PCM, WaveFmt,
    WaveFmtExtended,
};
use chrono::{DateTime, Datelike, Local, Timelike, Utc};

use crate::clock::TimecodeFormat;

/// The largest `f32` that survives conversion to 24-bit integer PCM.
///
/// 24-bit samples run to `2^23 - 1` on the positive side but `-2^23` on the
/// negative, so +1.0 has no representation while -1.0 does.
pub const PCM24_MAX: f32 = 1.0 - 1.0 / 8_388_608.0;
/// The most negative value, which *is* exactly representable.
pub const PCM24_MIN: f32 = -1.0;

/// Clamp a float sample into the range 24-bit PCM can actually hold.
///
/// This is not a nicety. The float-to-24-bit conversion in our WAV writer wraps
/// modulo rather than saturating, so without this a sample of +1.0 is written as
/// -1.0, +1.5 as -0.5 and +2.0 as 0.0 — a full-scale polarity flip, which is about
/// the worst artefact a recorder can produce. cpal hands us `f32` that routinely
/// exceeds +/-1.0 on a hot input or an intersample peak, so this is the common case,
/// not the exotic one. Hard-clipping at full scale is what every other recorder
/// does and what an engineer expects to hear; wrapping is not.
#[inline]
pub fn clamp_for_pcm24(sample: f32) -> f32 {
    // NaN would otherwise sail through `min`/`max` and convert unpredictably.
    if sample.is_nan() {
        return 0.0;
    }
    sample.clamp(PCM24_MIN, PCM24_MAX)
}

/// Clamp a whole interleaved buffer in place, ready for a 24-bit write.
pub fn clamp_buffer_for_pcm24(buffer: &mut [f32]) {
    for s in buffer {
        *s = clamp_for_pcm24(*s);
    }
}

/// Build a `fmt` chunk.
///
/// Above two channels WAVE requires the extended form. We declare a channel mask of
/// zero, which means "discrete, unassigned": a field recorder's inputs are numbered
/// jacks, not a 5.1 layout, and claiming a speaker layout we do not have would make
/// downstream tools route the audio somewhere surprising.
fn wave_fmt(sample_rate: u32, channels: u16, bits: u16, float: bool) -> WaveFmt {
    let container_bits = bits.div_ceil(8) * 8;
    let bytes_per_sample = container_bits / 8;
    let block_alignment = bytes_per_sample * channels;

    let basic_tag = if float { WAVE_TAG_FLOAT } else { WAVE_TAG_PCM };
    // The extended record is required for >2 channels, and also whenever the valid
    // bit count differs from the container size.
    let needs_extended = channels > 2 || container_bits != bits;

    let (tag, extended_format) = if needs_extended {
        (
            WAVE_TAG_EXTENDED,
            Some(WaveFmtExtended {
                valid_bits_per_sample: bits,
                channel_mask: 0,
                type_guid: if float {
                    WAVE_UUID_FLOAT
                } else {
                    WAVE_UUID_PCM
                },
            }),
        )
    } else {
        (basic_tag, None)
    };

    WaveFmt {
        tag,
        channel_count: channels,
        sample_rate,
        bytes_per_second: block_alignment as u32 * sample_rate,
        block_alignment,
        bits_per_sample: container_bits,
        extended_format,
    }
}

/// The format the finished file ships in: 24-bit integer PCM.
pub fn wave_fmt_pcm24(sample_rate: u32, channels: u16) -> WaveFmt {
    wave_fmt(sample_rate, channels, 24, false)
}

/// The format of the live scratch capture: 32-bit float.
///
/// The capture path is `f32` end to end, so writing the intermediate as float keeps
/// it bit-exact. Quantising to 24 bits here and again after resampling would round
/// the audio twice for no benefit, and this file is deleted once the corrected take
/// exists anyway.
pub fn wave_fmt_f32(sample_rate: u32, channels: u16) -> WaveFmt {
    wave_fmt(sample_rate, channels, 32, true)
}

/// The timecode a take was stamped against, when it came from a timecode source.
///
/// `bext.TimeReference` counts samples and so cannot carry a frame rate, which is
/// the first thing anyone conforming a multicam shoot asks for. It goes in iXML and
/// in `CodingHistory` instead.
#[derive(Debug, Clone, PartialEq)]
pub struct TimecodeStamp {
    pub format: TimecodeFormat,
    /// The label at `t0`, as it reads on a slate.
    pub start: String,
}

/// What we know about how a take was made. Travels into `bext`, iXML and the sidecar.
#[derive(Debug, Clone)]
pub struct Provenance {
    /// When the first sample was captured, in unix nanoseconds, latency-corrected.
    pub t0_unix_nanos: i128,
    /// Sample rate written into `fmt`, i.e. the rate of the file as it ships.
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub device_name: String,
    /// The nominal rate the device was asked to run at.
    pub device_rate: u32,
    /// The rate the device actually ran at, measured over the take.
    pub measured_rate: Option<f64>,
    /// `sample_rate / measured_rate`. 1.0 means no correction was needed.
    pub drift_ratio: Option<f64>,
    /// Whether the audio was actually resampled by that ratio.
    pub resampled: bool,
    /// What the timestamps were measured against: an NTP server, or the ethersync
    /// leader this machine was locked to.
    pub clock_source: String,
    /// Best estimate of our timestamp error, in seconds.
    pub clock_dispersion_s: Option<f64>,
    pub sync_state: String,
    /// Machine frequency error from the clock fit, if it was trustworthy.
    pub slope_ppm: Option<f64>,
    /// Manual latency trim the operator dialled in, in milliseconds.
    pub latency_offset_ms: f64,
    /// The timecode at `t0`, when the clock was a timecode source.
    pub timecode: Option<TimecodeStamp>,
}

/// Samples since local midnight at `t0` — the value `bext.TimeReference` wants.
///
/// Expressed in the file's own sample rate, so it stays exact for a resampled file.
pub fn time_reference(t0_unix_nanos: i128, sample_rate: u32) -> u64 {
    let local = to_local(t0_unix_nanos);
    let secs = local.num_seconds_from_midnight() as f64;
    // `nanosecond()` runs past 1e9 inside a leap second; clamp so we cannot overshoot.
    let sub = (local.nanosecond().min(999_999_999) as f64) / 1.0e9;
    ((secs + sub) * sample_rate as f64).round().max(0.0) as u64
}

/// `bext` has no date field, so the date has to ride along as text.
pub fn origination_date_time(t0_unix_nanos: i128) -> (String, String) {
    let local = to_local(t0_unix_nanos);
    (
        format!(
            "{:04}-{:02}-{:02}",
            local.year(),
            local.month(),
            local.day()
        ),
        format!(
            "{:02}:{:02}:{:02}",
            local.hour(),
            local.minute(),
            local.second()
        ),
    )
}

fn to_local(unix_nanos: i128) -> DateTime<Local> {
    let clamped = unix_nanos.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    DateTime::<Utc>::from_timestamp_nanos(clamped).with_timezone(&Local)
}

fn channel_mode(channels: u16) -> &'static str {
    match channels {
        1 => "mono",
        2 => "stereo",
        _ => "multichannel",
    }
}

/// The `CodingHistory` string, so the numbers travel with the file.
///
/// Starts with the EBU R98 `A=/F=/W=/M=/T=` form that other tools parse, then
/// appends our own key=value fields after it.
pub fn coding_history(p: &Provenance) -> String {
    let mut s = format!(
        "A=PCM,F={},W={},M={},T=syncrec",
        p.sample_rate,
        p.bits_per_sample,
        channel_mode(p.channels)
    );
    s.push_str(&format!("\r\nT=device:{}", p.device_name));
    s.push_str(&format!("\r\nT=device_rate:{}", p.device_rate));
    if let Some(r) = p.measured_rate {
        s.push_str(&format!("\r\nT=measured_rate:{r:.4}"));
    }
    if let Some(r) = p.drift_ratio {
        s.push_str(&format!("\r\nT=drift_ratio:{r:.9}"));
    }
    s.push_str(&format!(
        "\r\nT=resampled:{}",
        if p.resampled { "yes" } else { "no" }
    ));
    s.push_str(&format!("\r\nT=clock_source:{}", p.clock_source));
    s.push_str(&format!("\r\nT=clock_sync:{}", p.sync_state));
    if let Some(d) = p.clock_dispersion_s {
        s.push_str(&format!("\r\nT=clock_dispersion_ms:{:.3}", d * 1.0e3));
    } else {
        s.push_str("\r\nT=clock_dispersion_ms:unknown");
    }
    if let Some(ppm) = p.slope_ppm {
        s.push_str(&format!("\r\nT=clock_slope_ppm:{ppm:.3}"));
    }
    if let Some(tc) = &p.timecode {
        s.push_str(&format!("\r\nT=timecode_rate:{}", tc.format));
        s.push_str(&format!("\r\nT=start_timecode:{}", tc.start));
    }
    s.push_str(&format!("\r\nT=latency_trim_ms:{:.3}", p.latency_offset_ms));
    s.push_str(&format!("\r\nT=t0_utc:{}", iso8601_nanos(p.t0_unix_nanos)));
    s
}

/// `t0` to nanosecond precision, which neither `bext` field can express.
pub fn iso8601_nanos(unix_nanos: i128) -> String {
    let clamped = unix_nanos.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    DateTime::<Utc>::from_timestamp_nanos(clamped)
        .format("%Y-%m-%dT%H:%M:%S%.9fZ")
        .to_string()
}

/// The same facts as structured XML, for tools that read iXML.
pub fn ixml(p: &Provenance) -> String {
    let (date, time) = origination_date_time(p.t0_unix_nanos);
    let opt = |v: Option<f64>, prec: usize| match v {
        Some(x) => format!("{x:.*}", prec),
        None => String::new(),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<BWFXML>
  <IXML_VERSION>1.61</IXML_VERSION>
  <PROJECT>syncrec</PROJECT>
  <NOTE>{note}</NOTE>
  <SPEED>
    <MASTER_SPEED>{rate}/1</MASTER_SPEED>
    <TIMECODE_RATE>{tc_rate}</TIMECODE_RATE>
    <TIMECODE_FLAG>{tc_flag}</TIMECODE_FLAG>
    <FILE_SAMPLE_RATE>{rate}</FILE_SAMPLE_RATE>
  </SPEED>
  <SYNCREC>
    <T0_UTC>{t0}</T0_UTC>
    <ORIGINATION_DATE>{date}</ORIGINATION_DATE>
    <ORIGINATION_TIME>{time}</ORIGINATION_TIME>
    <TIME_REFERENCE>{tref}</TIME_REFERENCE>
    <DEVICE>{device}</DEVICE>
    <DEVICE_NOMINAL_RATE>{device_rate}</DEVICE_NOMINAL_RATE>
    <MEASURED_RATE>{measured}</MEASURED_RATE>
    <DRIFT_RATIO>{ratio}</DRIFT_RATIO>
    <RESAMPLED>{resampled}</RESAMPLED>
    <CLOCK_SOURCE>{server}</CLOCK_SOURCE>
    <CLOCK_SYNC_STATE>{sync}</CLOCK_SYNC_STATE>
    <CLOCK_DISPERSION_MS>{disp}</CLOCK_DISPERSION_MS>
    <CLOCK_SLOPE_PPM>{ppm}</CLOCK_SLOPE_PPM>
    <START_TIMECODE>{start_tc}</START_TIMECODE>
    <LATENCY_TRIM_MS>{trim:.3}</LATENCY_TRIM_MS>
  </SYNCREC>
</BWFXML>
"#,
        rate = p.sample_rate,
        note = match &p.timecode {
            Some(_) => "Timestamps derived from LAN timecode (ethersync), not the OS wall clock.",
            None => "Timestamps derived from in-process SNTP against a monotonic clock, not the OS wall clock.",
        },
        // iXML wants the *timecode* rate here. Without a timecode source there is
        // no frame rate to report, and the sample rate is the only honest stand-in
        // for a file whose timestamps are counted in samples.
        tc_rate = match &p.timecode {
            Some(tc) => format!("{}/{}", tc.format.numerator, tc.format.denominator),
            None => format!("{}/1", p.sample_rate),
        },
        tc_flag = match &p.timecode {
            Some(tc) if tc.format.drop_frame => "DF",
            _ => "NDF",
        },
        start_tc = p.timecode.as_ref().map(|tc| tc.start.as_str()).unwrap_or_default(),
        t0 = iso8601_nanos(p.t0_unix_nanos),
        date = date,
        time = time,
        tref = time_reference(p.t0_unix_nanos, p.sample_rate),
        device = xml_escape(&p.device_name),
        device_rate = p.device_rate,
        measured = opt(p.measured_rate, 4),
        ratio = opt(p.drift_ratio, 9),
        resampled = if p.resampled { "true" } else { "false" },
        server = xml_escape(&p.clock_source),
        sync = xml_escape(&p.sync_state),
        disp = opt(p.clock_dispersion_s.map(|d| d * 1.0e3), 3),
        ppm = opt(p.slope_ppm, 3),
        trim = p.latency_offset_ms,
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Build the `bext` chunk.
///
/// Version 0: we have no UMID or loudness metadata to offer, and claiming a higher
/// version would promise fields we would then have to leave empty.
pub fn bext(p: &Provenance) -> Bext {
    let (origination_date, origination_time) = origination_date_time(p.t0_unix_nanos);
    Bext {
        description: truncate(
            &format!(
                "syncrec {} @ {} | t0={} | disp={}",
                channel_mode(p.channels),
                p.sample_rate,
                iso8601_nanos(p.t0_unix_nanos),
                match p.clock_dispersion_s {
                    Some(d) => format!("{:.3}ms", d * 1.0e3),
                    None => "unknown".into(),
                }
            ),
            256,
        ),
        originator: truncate("syncrec", 32),
        originator_reference: truncate(&p.sync_state, 32),
        origination_date,
        origination_time,
        time_reference: time_reference(p.t0_unix_nanos, p.sample_rate),
        version: 0,
        umid: None,
        loudness_value: None,
        loudness_range: None,
        max_true_peak_level: None,
        max_momentary_loudness: None,
        max_short_term_loudness: None,
        coding_history: coding_history(p),
    }
}

/// `bext` string fields are fixed-width ASCII; overrunning them corrupts the chunk.
fn truncate(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| c.is_ascii() && !c.is_control())
        .take(max)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn provenance() -> Provenance {
        Provenance {
            t0_unix_nanos: 0,
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 24,
            device_name: "Scarlett 2i2".into(),
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
        }
    }

    /// A known local wall-clock time, expressed as unix nanos.
    fn at_local(h: u32, m: u32, s: u32, nanos: u32) -> i128 {
        let dt = Local
            .with_ymd_and_hms(2026, 3, 14, h, m, s)
            .single()
            .expect("unambiguous local time");
        dt.timestamp() as i128 * 1_000_000_000 + nanos as i128
    }

    #[test]
    fn time_reference_counts_samples_from_local_midnight() {
        // 01:00:00 local is 3600 s in.
        let t = at_local(1, 0, 0, 0);
        assert_eq!(time_reference(t, 48_000), 3600 * 48_000);
    }

    #[test]
    fn time_reference_is_exactly_zero_at_local_midnight() {
        let t = at_local(0, 0, 0, 0);
        assert_eq!(time_reference(t, 48_000), 0);
    }

    #[test]
    fn time_reference_carries_sub_second_precision() {
        // Half a second past 10:00:00 is half a sample-rate's worth of samples.
        let t = at_local(10, 0, 0, 500_000_000);
        let want = 10 * 3600 * 48_000 + 24_000;
        assert_eq!(time_reference(t, 48_000), want);
    }

    #[test]
    fn time_reference_follows_the_files_own_rate() {
        let t = at_local(1, 0, 0, 0);
        assert_eq!(time_reference(t, 44_100), 3600 * 44_100);
        assert_eq!(time_reference(t, 96_000), 3600 * 96_000);
    }

    #[test]
    fn time_reference_just_before_midnight_stays_inside_the_day() {
        let t = at_local(23, 59, 59, 0);
        let tref = time_reference(t, 48_000);
        assert!(
            tref < 24 * 3600 * 48_000,
            "must not roll past a day: {tref}"
        );
        assert_eq!(tref, 86_399 * 48_000);
    }

    #[test]
    fn origination_fields_are_the_fixed_widths_bext_requires() {
        let (d, t) = origination_date_time(at_local(9, 5, 3, 0));
        assert_eq!(d.len(), 10, "OriginationDate is a 10-char field");
        assert_eq!(t.len(), 8, "OriginationTime is an 8-char field");
        assert_eq!(d, "2026-03-14");
        assert_eq!(t, "09:05:03");
    }

    #[test]
    fn coding_history_starts_with_the_parseable_ebu_form() {
        let h = coding_history(&provenance());
        assert!(
            h.starts_with("A=PCM,F=48000,W=24,M=stereo,T=syncrec"),
            "{h}"
        );
    }

    #[test]
    fn coding_history_carries_the_numbers_that_matter() {
        let h = coding_history(&provenance());
        for needle in [
            "drift_ratio:1.000012500",
            "measured_rate:47999.4000",
            "resampled:yes",
            "clock_dispersion_ms:4.928",
            "clock_sync:synced",
            "clock_source:time.apple.com",
            "clock_slope_ppm:-16.280",
        ] {
            assert!(h.contains(needle), "missing {needle} in:\n{h}");
        }
    }

    #[test]
    fn unsynced_takes_say_so_rather_than_inventing_a_dispersion() {
        let mut p = provenance();
        p.clock_dispersion_s = None;
        p.sync_state = "unsynced".into();
        let h = coding_history(&p);
        assert!(h.contains("clock_dispersion_ms:unknown"), "{h}");
        assert!(h.contains("clock_sync:unsynced"), "{h}");
    }

    #[test]
    fn a_timecode_take_carries_its_frame_rate_and_start() {
        let p = Provenance {
            clock_source: "ethersync follower of 10.0.0.4:4443".into(),
            timecode: Some(TimecodeStamp {
                format: TimecodeFormat {
                    numerator: 30000,
                    denominator: 1001,
                    drop_frame: true,
                },
                start: "10:31:07;12".into(),
            }),
            ..provenance()
        };
        let h = coding_history(&p);
        assert!(h.contains("T=start_timecode:10:31:07;12"), "{h}");
        assert!(h.contains("T=timecode_rate:29.970 DF"), "{h}");
        assert!(h.contains("T=clock_source:ethersync follower of"), "{h}");

        let x = ixml(&p);
        // Sample rate is not a frame rate. A conform that reads 48000/1 here puts
        // the take four hundred hours away from where it belongs.
        assert!(x.contains("<TIMECODE_RATE>30000/1001</TIMECODE_RATE>"), "{x}");
        assert!(x.contains("<TIMECODE_FLAG>DF</TIMECODE_FLAG>"), "{x}");
        assert!(x.contains("<START_TIMECODE>10:31:07;12</START_TIMECODE>"), "{x}");
    }

    #[test]
    fn an_ntp_take_claims_no_timecode_rate_it_does_not_have() {
        let x = ixml(&provenance());
        assert!(x.contains("<TIMECODE_FLAG>NDF</TIMECODE_FLAG>"), "{x}");
        assert!(x.contains("<START_TIMECODE></START_TIMECODE>"), "{x}");
    }

    #[test]
    fn bext_string_fields_respect_their_limits() {
        let mut p = provenance();
        p.device_name = "x".repeat(500);
        let b = bext(&p);
        assert!(b.description.len() <= 256);
        assert!(b.originator.len() <= 32);
        assert!(b.originator_reference.len() <= 32);
    }

    #[test]
    fn non_ascii_device_names_do_not_corrupt_bext() {
        let mut p = provenance();
        p.device_name = "Røde NT-USB — café".into();
        let b = bext(&p);
        assert!(b.description.is_ascii(), "bext fields must stay ASCII");
    }

    #[test]
    fn ixml_escapes_device_names_containing_markup() {
        let mut p = provenance();
        p.device_name = "A & B <mic>".into();
        let x = ixml(&p);
        assert!(x.contains("A &amp; B &lt;mic&gt;"), "{x}");
        assert!(!x.contains("<mic>"));
    }

    #[test]
    fn ixml_agrees_with_bext_on_the_timestamp() {
        let p = Provenance {
            t0_unix_nanos: at_local(13, 37, 42, 123_456_789),
            ..provenance()
        };
        let x = ixml(&p);
        let b = bext(&p);
        assert!(x.contains(&format!(
            "<TIME_REFERENCE>{}</TIME_REFERENCE>",
            b.time_reference
        )));
        assert!(x.contains(&format!(
            "<ORIGINATION_DATE>{}</ORIGINATION_DATE>",
            b.origination_date
        )));
    }

    #[test]
    fn iso8601_keeps_nanoseconds() {
        // 1 ns past the epoch.
        assert_eq!(iso8601_nanos(1), "1970-01-01T00:00:00.000000001Z");
    }

    #[test]
    fn full_scale_is_clamped_below_the_wrap_point() {
        // The exact failure observed against the real writer: +1.0 became -1.0.
        assert!(clamp_for_pcm24(1.0) < 1.0);
        assert_eq!(clamp_for_pcm24(1.0), PCM24_MAX);
        assert_eq!(clamp_for_pcm24(1.5), PCM24_MAX);
        assert_eq!(clamp_for_pcm24(f32::INFINITY), PCM24_MAX);
    }

    #[test]
    fn negative_full_scale_is_kept_because_it_is_representable() {
        // -1.0 maps exactly to -2^23 and must not be needlessly attenuated.
        assert_eq!(clamp_for_pcm24(-1.0), -1.0);
        assert_eq!(clamp_for_pcm24(-1.5), -1.0);
        assert_eq!(clamp_for_pcm24(f32::NEG_INFINITY), -1.0);
    }

    #[test]
    fn ordinary_samples_pass_through_untouched() {
        for v in [0.0, 0.5, -0.5, 0.25, -0.999, 1e-9] {
            assert_eq!(
                clamp_for_pcm24(v),
                v,
                "clamping must be transparent below FS"
            );
        }
    }

    #[test]
    fn nan_becomes_silence_rather_than_noise() {
        assert_eq!(clamp_for_pcm24(f32::NAN), 0.0);
    }

    #[test]
    fn the_clamp_ceiling_is_one_lsb_below_unity() {
        assert!((PCM24_MAX - (1.0 - 2.0f32.powi(-23))).abs() < f32::EPSILON);
    }

    #[test]
    fn buffer_clamping_covers_every_sample() {
        let mut buf = [1.0, -1.5, 0.5, f32::NAN, 2.0];
        clamp_buffer_for_pcm24(&mut buf);
        assert_eq!(buf, [PCM24_MAX, -1.0, 0.5, 0.0, PCM24_MAX]);
    }

    #[test]
    fn pcm24_fmt_has_consistent_derived_fields() {
        let f = wave_fmt_pcm24(48_000, 2);
        assert_eq!(
            f.tag, WAVE_TAG_PCM,
            "stereo 24-bit needs no extended record"
        );
        assert_eq!(f.bits_per_sample, 24);
        assert_eq!(f.block_alignment, 6, "3 bytes x 2 channels");
        assert_eq!(f.bytes_per_second, 6 * 48_000);
        assert!(f.extended_format.is_none());
    }

    #[test]
    fn float_fmt_declares_ieee_float() {
        let f = wave_fmt_f32(48_000, 2);
        assert_eq!(f.tag, WAVE_TAG_FLOAT);
        assert_eq!(f.bits_per_sample, 32);
        assert_eq!(f.block_alignment, 8);
    }

    #[test]
    fn more_than_two_channels_uses_the_extended_record() {
        let f = wave_fmt_pcm24(48_000, 8);
        assert_eq!(f.tag, WAVE_TAG_EXTENDED);
        let ext = f
            .extended_format
            .expect("8 channels requires the extended form");
        assert_eq!(ext.valid_bits_per_sample, 24);
        assert_eq!(ext.type_guid, WAVE_UUID_PCM);
        assert_eq!(ext.channel_mask, 0, "discrete inputs, not a speaker layout");
        assert_eq!(f.block_alignment, 24);
    }

    #[test]
    fn eight_channel_float_declares_the_float_guid() {
        let f = wave_fmt_f32(96_000, 8);
        assert_eq!(f.extended_format.unwrap().type_guid, WAVE_UUID_FLOAT);
        assert_eq!(f.bytes_per_second, 32 * 96_000);
    }

    #[test]
    fn mono_stays_in_the_simple_form() {
        let f = wave_fmt_pcm24(44_100, 1);
        assert_eq!(f.tag, WAVE_TAG_PCM);
        assert_eq!(f.block_alignment, 3);
        assert!(f.extended_format.is_none());
    }

    #[test]
    fn version_zero_promises_no_fields_we_cannot_fill() {
        let b = bext(&provenance());
        assert_eq!(b.version, 0);
        assert!(b.umid.is_none());
        assert!(b.loudness_value.is_none());
    }
}
