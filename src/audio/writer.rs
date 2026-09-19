//! Draining captured audio to disk, and turning device timestamps into UTC.
//!
//! Two jobs live here. The dull one is emptying the ring buffer into a WAV file
//! fast enough that the audio thread never overruns. The interesting one is the
//! drift log: every time mark the callback dropped gets resolved from the device
//! clock, through the monotonic clock, to UTC, and the resulting
//! `(sample_index, utc)` pairs are what later reveal that the device is really
//! running at 47999.4 Hz rather than the 48000 it claims.
//!
//! Resolution happens here rather than at the end of the take on purpose. The clock
//! model is a fit over a sliding eight-sample window, so it describes the recent
//! past well and the distant past badly. Converting a mark while its window is still
//! current uses the locally valid line instead of extrapolating one backwards across
//! the whole recording.
//!
//! Marks taken before the clock has synced cannot be resolved at all, so they are
//! parked and retried. That matters most for the very first mark, which is the
//! recording's anchor.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::capture::TimeMark;
use crate::clock::ClockSnapshot;
use crate::clock::bridge::Bridge;
use crate::latency::LatencyCorrection;

/// Ceiling on parked marks, so a take that never syncs cannot grow without bound.
/// At one mark per second this is over an hour of waiting.
const MAX_PENDING: usize = 4096;

/// One resolved observation: this sample was captured at this UTC instant.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DriftObservation {
    pub sample_index: u64,
    pub utc_unix_nanos: i128,
}

/// Turns raw device time marks into UTC observations.
///
/// Pure logic, no IO, so the time chain can be tested without a sound card.
pub struct DriftLog {
    bridge: Bridge,
    /// Input latency still to be removed, beyond whatever cpal already handled.
    ///
    /// Signed, because a manual trim can legitimately over-correct the platform
    /// figure and push a timestamp later rather than earlier. Squeezing this into a
    /// `Duration` would silently swallow that case.
    latency: LatencyCorrection,
    resolved: Vec<DriftObservation>,
    /// Marks whose UTC is not yet knowable because the clock had not synced.
    pending: Vec<(u64, Instant)>,
    /// Marks dropped because they were parked for too long.
    abandoned: u64,
}

impl DriftLog {
    pub fn new(bridge: Bridge, latency: LatencyCorrection) -> Self {
        Self {
            bridge,
            latency,
            resolved: Vec::new(),
            pending: Vec::new(),
            abandoned: 0,
        }
    }

    /// Map a device timestamp onto the monotonic clock.
    ///
    /// Latency is applied afterwards, in the UTC domain, rather than here. Both are
    /// linear shifts so the two differ only by the clock's frequency correction
    /// acting over the latency itself — some tens of nanoseconds — and applying it
    /// later means there is exactly one place that knows the sign convention.
    fn mark_instant(&self, mark: TimeMark) -> Instant {
        self.bridge
            .to_instant(cpal::StreamInstant::from_nanos(mark.capture_nanos as u64))
    }

    /// Feed one mark. It is resolved immediately if the clock can place it.
    pub fn observe(&mut self, mark: TimeMark, clock: &ClockSnapshot) {
        let at = self.mark_instant(mark);
        match clock.utc_nanos_at(at) {
            Some(utc) => self.resolved.push(DriftObservation {
                sample_index: mark.frame_index,
                utc_unix_nanos: self.latency.apply(utc),
            }),
            None => {
                if self.pending.len() < MAX_PENDING {
                    self.pending.push((mark.frame_index, at));
                } else {
                    self.abandoned += 1;
                }
            }
        }
    }

    /// Retry everything parked. Call whenever the clock may have gained a sample.
    ///
    /// Resolved marks are appended in sample order so the drift fit sees a tidy
    /// series regardless of how long they waited.
    pub fn retry_pending(&mut self, clock: &ClockSnapshot) {
        if self.pending.is_empty() {
            return;
        }
        let mut still_pending = Vec::new();
        let mut freshly_resolved = Vec::new();
        for (index, at) in self.pending.drain(..) {
            match clock.utc_nanos_at(at) {
                Some(utc) => freshly_resolved.push(DriftObservation {
                    sample_index: index,
                    utc_unix_nanos: self.latency.apply(utc),
                }),
                None => still_pending.push((index, at)),
            }
        }
        self.pending = still_pending;
        self.resolved.extend(freshly_resolved);
        self.resolved.sort_by_key(|o| o.sample_index);
    }

    pub fn observations(&self) -> &[DriftObservation] {
        &self.resolved
    }

    /// UTC of the first captured sample: the recording's anchor.
    ///
    /// This is the observation at sample zero specifically, not merely the earliest
    /// one we managed to resolve — using a later mark as the anchor would silently
    /// shift the whole file.
    pub fn t0_unix_nanos(&self) -> Option<i128> {
        self.resolved
            .iter()
            .find(|o| o.sample_index == 0)
            .map(|o| o.utc_unix_nanos)
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn abandoned_count(&self) -> u64 {
        self.abandoned
    }
}

/// Subdirectory holding live scratch captures.
///
/// The scratch file is an implementation detail that only survives when the
/// correction could not be verified, so it lives in a dotted subdirectory rather
/// than beside the deliverable. Three files per take in one flat folder made it
/// genuinely unclear which one was the Broadcast Wave file.
pub const SCRATCH_DIR: &str = ".syncrec-scratch";

/// Where the files of a take live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakePaths {
    /// Written live at the device's own rate, inside [`SCRATCH_DIR`].
    /// Deleted once a corrected file exists.
    pub raw: PathBuf,
    /// The Broadcast Wave file that ships.
    pub final_wav: PathBuf,
    /// The audit trail.
    pub sidecar: PathBuf,
}

/// Work out the next take in a `base-N` sequence.
///
/// Scans for existing takes and continues past the highest, so numbering survives
/// restarts and does not clobber yesterday's work. A name is considered taken if
/// *any* of its three files exists.
pub fn next_take(dir: &Path, base: &str) -> TakePaths {
    // Scan the take folder and the scratch folder together: an abandoned scratch
    // capture still reserves its number, or finalising a later take would overwrite
    // the evidence from an earlier failed one.
    let scan = |d: PathBuf| {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| take_number(&e.file_name().to_string_lossy(), base))
            .max()
            .unwrap_or(0)
    };
    let highest = scan(dir.to_path_buf()).max(scan(dir.join(SCRATCH_DIR)));

    let mut n = highest + 1;
    loop {
        let paths = take_paths(dir, base, n);
        if !paths.raw.exists() && !paths.final_wav.exists() && !paths.sidecar.exists() {
            return paths;
        }
        n += 1;
    }
}

fn take_paths(dir: &Path, base: &str, n: u32) -> TakePaths {
    TakePaths {
        raw: dir.join(SCRATCH_DIR).join(format!("{base}-{n}.raw.wav")),
        final_wav: dir.join(format!("{base}-{n}.wav")),
        sidecar: dir.join(format!("{base}-{n}.json")),
    }
}

/// Extract `N` from `base-N.wav`, `base-N.raw.wav` or `base-N.json`.
fn take_number(file_name: &str, base: &str) -> Option<u32> {
    let rest = file_name.strip_prefix(base)?.strip_prefix('-')?;
    let digits = rest
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|s| !s.is_empty())?;
    // Reject `rec-1extra.wav`: the digits must be the whole stem segment.
    let after = &rest[digits.len()..];
    if !matches!(after, ".wav" | ".raw.wav" | ".json") {
        return None;
    }
    digits.parse().ok()
}

/// The audit trail written alongside every take.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sidecar {
    pub t0_unix_nanos: Option<i128>,
    pub device_name: String,
    pub device_rate: u32,
    pub channels: u16,
    pub raw_frames: u64,
    pub ntp_server: String,
    pub sync_state: String,
    pub ntp_dispersion_s: Option<f64>,
    pub clock_slope_ppm: Option<f64>,
    pub clock_samples_accepted: u64,
    pub clock_samples_rejected: u64,
    pub latency_trim_ms: f64,
    pub overruns: u64,
    pub marks_abandoned: u64,
    /// First error the audio backend reported, if any.
    #[serde(default)]
    pub stream_error: Option<String>,
    pub observations: Vec<DriftObservation>,
}

impl Sidecar {
    pub fn write(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).context("serialising sidecar")?;
        std::fs::write(path, json).with_context(|| format!("writing sidecar {}", path.display()))
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading sidecar {}", path.display()))?;
        serde_json::from_str(&text).context("parsing sidecar")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ClockModel;
    use crate::latency::InputLatency;
    use std::time::Duration;
    use cpal::StreamInstant;
    use cpal::traits::StreamTrait;

    /// A stream whose clock is `Instant` shifted by a fixed origin, so the bridge
    /// has something real to correlate against.
    struct FakeStream {
        base: Instant,
        origin: Duration,
    }

    impl StreamTrait for FakeStream {
        fn play(&self) -> Result<(), cpal::Error> {
            Ok(())
        }
        fn pause(&self) -> Result<(), cpal::Error> {
            Ok(())
        }
        fn buffer_size(&self) -> Result<cpal::FrameCount, cpal::Error> {
            Ok(512)
        }
        fn now(&self) -> StreamInstant {
            let t = Instant::now().duration_since(self.base) + self.origin;
            StreamInstant::new(t.as_secs(), t.subsec_nanos())
        }
    }

    /// A bridge plus the stream-clock reading that corresponds to "now".
    fn rig() -> (Bridge, u128, Instant) {
        let base = Instant::now();
        let stream = FakeStream {
            base,
            origin: Duration::from_secs(5_000),
        };
        let bridge = Bridge::measure(&stream);
        let now_stream = stream.now().as_nanos();
        (bridge, now_stream, Instant::now())
    }

    /// A clock synced enough to resolve timestamps, anchored at `base`.
    fn synced_clock(base: Instant) -> ClockSnapshot {
        let mut m = ClockModel::new(base);
        for k in 0..8 {
            let x = k as f64 * 16.0;
            m.push(
                base + Duration::from_secs_f64(x),
                1_000_000_000_000i128 + (x * 1.0e9) as i128,
                0.01,
                0.0,
            );
        }
        m.snapshot()
    }

    fn mark(frame: u64, nanos: u128) -> TimeMark {
        TimeMark {
            frame_index: frame,
            capture_nanos: nanos,
        }
    }

    #[test]
    fn a_resolvable_mark_becomes_an_observation() {
        let (bridge, now_stream, now) = rig();
        let clock = synced_clock(now);
        let mut log = DriftLog::new(
            bridge,
            LatencyCorrection::platform_only(InputLatency::already_corrected()),
        );
        log.observe(mark(0, now_stream), &clock);
        assert_eq!(log.observations().len(), 1);
        assert_eq!(log.pending_count(), 0);
        assert_eq!(log.observations()[0].sample_index, 0);
    }

    #[test]
    fn marks_taken_before_sync_are_parked_not_discarded() {
        let (bridge, now_stream, now) = rig();
        let unsynced = ClockModel::new(now).snapshot();
        let mut log = DriftLog::new(
            bridge,
            LatencyCorrection::platform_only(InputLatency::already_corrected()),
        );

        log.observe(mark(0, now_stream), &unsynced);
        log.observe(mark(48_000, now_stream + 1_000_000_000), &unsynced);
        assert_eq!(log.observations().len(), 0);
        assert_eq!(
            log.pending_count(),
            2,
            "marks must survive being unresolvable"
        );

        // Once the clock catches up, the parked marks become usable.
        log.retry_pending(&synced_clock(now));
        assert_eq!(log.pending_count(), 0);
        assert_eq!(log.observations().len(), 2);
    }

    #[test]
    fn retried_marks_come_back_in_sample_order() {
        let (bridge, now_stream, now) = rig();
        let unsynced = ClockModel::new(now).snapshot();
        let mut log = DriftLog::new(
            bridge,
            LatencyCorrection::platform_only(InputLatency::already_corrected()),
        );
        let clock = synced_clock(now);

        // A resolvable mark first, then two parked ones from earlier in the take.
        log.observe(mark(96_000, now_stream + 2_000_000_000), &clock);
        log.observe(mark(0, now_stream), &unsynced);
        log.observe(mark(48_000, now_stream + 1_000_000_000), &unsynced);
        log.retry_pending(&clock);

        let idx: Vec<u64> = log.observations().iter().map(|o| o.sample_index).collect();
        assert_eq!(idx, vec![0, 48_000, 96_000]);
    }

    #[test]
    fn the_anchor_is_sample_zero_and_nothing_else() {
        let (bridge, now_stream, now) = rig();
        let clock = synced_clock(now);
        let mut log = DriftLog::new(
            bridge,
            LatencyCorrection::platform_only(InputLatency::already_corrected()),
        );

        // Only a later mark resolved: there is no anchor, and we must not pretend
        // the earliest available observation is one.
        log.observe(mark(48_000, now_stream), &clock);
        assert!(log.t0_unix_nanos().is_none());

        log.observe(mark(0, now_stream), &clock);
        assert!(log.t0_unix_nanos().is_some());
    }

    #[test]
    fn latency_pulls_the_timestamp_earlier() {
        let (bridge, now_stream, now) = rig();
        let clock = synced_clock(now);

        let mut plain = DriftLog::new(
            bridge,
            LatencyCorrection::platform_only(InputLatency::already_corrected()),
        );
        let mut trimmed = DriftLog::new(
            bridge,
            LatencyCorrection::new(InputLatency::measured(Duration::from_millis(10)), 0.0),
        );
        plain.observe(mark(0, now_stream), &clock);
        trimmed.observe(mark(0, now_stream), &clock);

        let delta =
            plain.observations()[0].utc_unix_nanos - trimmed.observations()[0].utc_unix_nanos;
        // The sound hit the mic 10 ms before the buffer was handed to us.
        assert!(
            (delta - 10_000_000).abs() < 100_000,
            "expected ~10 ms earlier, got {delta} ns"
        );
    }

    #[test]
    fn parked_marks_are_capped_rather_than_growing_forever() {
        let (bridge, now_stream, now) = rig();
        let unsynced = ClockModel::new(now).snapshot();
        let mut log = DriftLog::new(
            bridge,
            LatencyCorrection::platform_only(InputLatency::already_corrected()),
        );
        for i in 0..(MAX_PENDING as u64 + 50) {
            log.observe(mark(i, now_stream + i as u128), &unsynced);
        }
        assert_eq!(log.pending_count(), MAX_PENDING);
        assert_eq!(log.abandoned_count(), 50);
    }

    #[test]
    fn take_numbering_starts_at_one_in_an_empty_directory() {
        let dir = std::env::temp_dir().join(format!("syncrec-t{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let t = next_take(&dir, "rec");
        assert!(t.final_wav.ends_with("rec-1.wav"), "{t:?}");
        assert!(t.sidecar.ends_with("rec-1.json"), "{t:?}");
        // The deliverable sits in the chosen folder; the scratch capture does not.
        assert_eq!(t.final_wav.parent().unwrap(), dir);
        assert_eq!(t.sidecar.parent().unwrap(), dir);
        assert!(t.raw.ends_with("rec-1.raw.wav"), "{t:?}");
        assert_eq!(t.raw.parent().unwrap(), dir.join(SCRATCH_DIR));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn take_numbering_continues_past_the_highest_existing() {
        let dir = std::env::temp_dir().join(format!("syncrec-h{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A gap in the sequence must not be reused; we continue past the maximum.
        std::fs::write(dir.join("rec-1.wav"), b"").unwrap();
        std::fs::write(dir.join("rec-7.wav"), b"").unwrap();
        let t = next_take(&dir, "rec");
        assert!(t.final_wav.ends_with("rec-8.wav"), "{t:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_abandoned_raw_file_still_reserves_its_number() {
        let dir = std::env::temp_dir().join(format!("syncrec-r{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A take that crashed mid-write leaves only a scratch file behind. Reusing
        // that number would overwrite the evidence.
        std::fs::create_dir_all(dir.join(SCRATCH_DIR)).unwrap();
        std::fs::write(dir.join(SCRATCH_DIR).join("rec-3.raw.wav"), b"").unwrap();
        let t = next_take(&dir, "rec");
        assert!(t.final_wav.ends_with("rec-4.wav"), "{t:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn other_peoples_files_do_not_shift_the_sequence() {
        let dir = std::env::temp_dir().join(format!("syncrec-o{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "take-9.wav",
            "rec.wav",
            "rec-.wav",
            "rec-2x.wav",
            "recording-5.wav",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let t = next_take(&dir, "rec");
        assert!(t.final_wav.ends_with("rec-1.wav"), "{t:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn take_number_parses_only_exact_matches() {
        assert_eq!(take_number("rec-1.wav", "rec"), Some(1));
        assert_eq!(take_number("rec-42.raw.wav", "rec"), Some(42));
        assert_eq!(take_number("rec-7.json", "rec"), Some(7));
        assert_eq!(take_number("rec-1extra.wav", "rec"), None);
        assert_eq!(take_number("rec-.wav", "rec"), None);
        assert_eq!(take_number("recording-1.wav", "rec"), None);
        assert_eq!(take_number("other-1.wav", "rec"), None);
    }

    #[test]
    fn sidecar_round_trips_through_json() {
        let dir = std::env::temp_dir().join(format!("syncrec-s{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rec-1.json");
        let s = Sidecar {
            t0_unix_nanos: Some(1_800_000_000_123_456_789),
            device_name: "Scarlett 2i2".into(),
            device_rate: 48_000,
            channels: 2,
            raw_frames: 2_880_000,
            ntp_server: "time.apple.com".into(),
            sync_state: "synced".into(),
            ntp_dispersion_s: Some(0.004928),
            clock_slope_ppm: Some(-16.28),
            clock_samples_accepted: 14,
            clock_samples_rejected: 0,
            latency_trim_ms: 0.0,
            overruns: 0,
            marks_abandoned: 0,
            stream_error: None,
            observations: vec![DriftObservation {
                sample_index: 0,
                utc_unix_nanos: 1_800_000_000_123_456_789,
            }],
        };
        s.write(&path).unwrap();
        let back = Sidecar::read(&path).unwrap();
        // i128 nanos must survive JSON without losing the low digits.
        assert_eq!(back.t0_unix_nanos, s.t0_unix_nanos);
        assert_eq!(
            back.observations[0].utc_unix_nanos,
            1_800_000_000_123_456_789
        );
        assert_eq!(back.device_name, "Scarlett 2i2");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
