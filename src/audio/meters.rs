//! Level metering, published from the realtime callback without locks.
//!
//! The audio thread must never block, so levels travel as plain atomics. Peak uses
//! `fetch_max` on the raw `f32` bit pattern, which is valid here because IEEE-754
//! bit patterns are monotonically ordered for non-negative floats and we only ever
//! store magnitudes.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Anything quieter than this reads as silence on the meter.
pub const FLOOR_DBFS: f32 = -72.0;

// Meter ballistics.
//
// A single callback buffer is about ten milliseconds of audio, so displaying its
// mean square raw is an instantaneous reading, not a meter — it flickers at the
// callback rate and is unreadable. Real meters integrate.
//
// The smoothing runs on the RMS *amplitude*, not on power. Smoothing power and
// taking the square root afterwards halves the decay rate in dB, which leaves a
// visible tail long after the sound has stopped: at a 300 ms power constant, three
// full seconds of silence still reads -43 dBFS. Amplitude-domain smoothing decays
// linearly in dB, which is how every meter behaves and how a reader expects it to.
//
// The asymmetry is deliberate and is what a PPM does: rise quickly so a transient
// is actually visible, fall slowly so the bar can be read. A symmetric average fast
// enough to catch transients is still too jittery to look at.
/// Time constant while the level is rising.
pub const ATTACK_TAU_S: f32 = 0.030;
/// Time constant while the level is falling. Reaches the meter floor from full
/// scale in a little over a second.
pub const RELEASE_TAU_S: f32 = 0.150;

/// Fast attack, slow release. Checked at compile time so the two cannot be
/// reordered by a well-meaning edit.
const _: () = assert!(RELEASE_TAU_S > ATTACK_TAU_S);

pub struct Meters {
    /// Largest magnitude seen since the UI last looked.
    peak: Vec<AtomicU32>,
    /// Integrated RMS amplitude, with the ballistics above applied.
    rms: Vec<AtomicU32>,
    /// Sticky until the UI clears it.
    clipped: Vec<AtomicBool>,
    /// Callbacks whose audio we could not fit in the ring buffer.
    overruns: AtomicU64,
    /// Needed to turn a block's frame count into a duration, so the ballistics are
    /// independent of the device's buffer size.
    sample_rate: u32,
}

impl Meters {
    pub fn new(channels: usize, sample_rate: u32) -> Self {
        Self {
            peak: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            rms: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            clipped: (0..channels).map(|_| AtomicBool::new(false)).collect(),
            overruns: AtomicU64::new(0),
            sample_rate: sample_rate.max(1),
        }
    }

    /// Smoothing factor for a block of `block_secs` at time constant `tau`.
    ///
    /// Derived from the block's duration in audio time rather than assumed per
    /// callback, so the ballistics are identical whether the device hands us 64
    /// frames or 2048.
    fn alpha(block_secs: f32, tau: f32) -> f32 {
        if tau <= 0.0 {
            return 1.0;
        }
        1.0 - (-block_secs / tau).exp()
    }

    pub fn channels(&self) -> usize {
        self.peak.len()
    }

    /// Fold one interleaved buffer into the meters. Called from the audio thread.
    pub fn ingest(&self, interleaved: &[f32], channels: usize) {
        if channels == 0 || interleaved.is_empty() {
            return;
        }
        let frames = interleaved.len() / channels;
        if frames == 0 {
            return;
        }

        // One exp per direction per buffer rather than per channel; the block
        // duration is the same for every channel.
        let block_secs = frames as f32 / self.sample_rate as f32;
        let attack = Self::alpha(block_secs, ATTACK_TAU_S);
        let release = Self::alpha(block_secs, RELEASE_TAU_S);

        for ch in 0..channels.min(self.peak.len()) {
            let mut peak = 0.0f32;
            let mut sum_sq = 0.0f64;
            for f in 0..frames {
                let s = interleaved[f * channels + ch];
                let mag = s.abs();
                if mag > peak {
                    peak = mag;
                }
                sum_sq += (s as f64) * (s as f64);
            }

            // Peak keeps an instantaneous attack: the whole point of a peak
            // indicator is that nothing is allowed to slip past it.
            // Monotone on non-negative floats, so max-of-bits is max-of-values.
            self.peak[ch].fetch_max(peak.to_bits(), Ordering::Relaxed);

            // The RMS bar is integrated. Only the audio thread writes this, so a
            // relaxed read-modify-write needs no synchronisation.
            let block_rms = ((sum_sq / frames as f64).max(0.0) as f32).sqrt();
            let previous = f32::from_bits(self.rms[ch].load(Ordering::Relaxed));
            let a = if block_rms > previous { attack } else { release };
            let smoothed = previous + a * (block_rms - previous);
            self.rms[ch].store(smoothed.max(0.0).to_bits(), Ordering::Relaxed);

            if peak >= 1.0 {
                self.clipped[ch].store(true, Ordering::Relaxed);
            }
        }
    }

    /// Read and reset the peak for each channel. Called from the UI thread.
    pub fn take_peaks(&self, out: &mut Vec<f32>) {
        out.clear();
        for p in &self.peak {
            out.push(f32::from_bits(p.swap(0, Ordering::Relaxed)));
        }
    }

    /// Current integrated RMS per channel. Not reset by reading.
    pub fn rms(&self, out: &mut Vec<f32>) {
        out.clear();
        for m in &self.rms {
            out.push(f32::from_bits(m.load(Ordering::Relaxed)).max(0.0));
        }
    }

    pub fn clipped(&self, ch: usize) -> bool {
        self.clipped
            .get(ch)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    pub fn clear_clip(&self) {
        for c in &self.clipped {
            c.store(false, Ordering::Relaxed);
        }
    }

    /// Clear peaks, clip latches and the overrun count.
    ///
    /// Called when a take begins: the meters run continuously so the operator can
    /// set gain before recording, and counters accumulated while merely monitoring
    /// would otherwise be reported against the take.
    pub fn reset(&self) {
        for p in &self.peak {
            p.store(0, Ordering::Relaxed);
        }
        for m in &self.rms {
            m.store(0, Ordering::Relaxed);
        }
        self.clear_clip();
        self.overruns.store(0, Ordering::Relaxed);
    }

    pub fn note_overrun(&self) {
        self.overruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::Relaxed)
    }
}

/// Linear magnitude to dBFS, floored so the meter has a bottom.
pub fn to_dbfs(mag: f32) -> f32 {
    if mag <= 0.0 {
        return FLOOR_DBFS;
    }
    (20.0 * mag.log10()).max(FLOOR_DBFS)
}

/// dBFS to a 0..1 position on the meter.
pub fn dbfs_to_fraction(db: f32) -> f32 {
    ((db - FLOOR_DBFS) / -FLOOR_DBFS).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_is_the_max_magnitude_across_the_buffer() {
        let m = Meters::new(2, 48_000);
        // Interleaved L,R. Left peaks at 0.5, right at -0.8 (magnitude 0.8).
        m.ingest(&[0.1, -0.2, 0.5, -0.8, -0.3, 0.4], 2);
        let mut peaks = Vec::new();
        m.take_peaks(&mut peaks);
        assert!((peaks[0] - 0.5).abs() < 1e-6, "{peaks:?}");
        assert!((peaks[1] - 0.8).abs() < 1e-6, "{peaks:?}");
    }

    #[test]
    fn peak_holds_across_buffers_until_read() {
        let m = Meters::new(1, 48_000);
        m.ingest(&[0.9], 1);
        m.ingest(&[0.1], 1);
        let mut peaks = Vec::new();
        m.take_peaks(&mut peaks);
        assert!(
            (peaks[0] - 0.9).abs() < 1e-6,
            "peak must survive a quiet buffer"
        );
        // Reading resets it.
        m.take_peaks(&mut peaks);
        assert_eq!(peaks[0], 0.0);
    }

    /// Feed `secs` of a constant-magnitude signal in `block` sized chunks.
    fn feed(m: &Meters, magnitude: f32, secs: f32, block: usize) {
        let buf: Vec<f32> = (0..block)
            .map(|i| if i % 2 == 0 { magnitude } else { -magnitude })
            .collect();
        let blocks = (secs * 48_000.0 / block as f32).round() as usize;
        for _ in 0..blocks {
            m.ingest(&buf, 1);
        }
    }

    fn rms_of(m: &Meters) -> f32 {
        let mut v = Vec::new();
        m.rms(&mut v);
        v[0]
    }

    #[test]
    fn rms_of_a_full_scale_square_settles_at_one() {
        let m = Meters::new(1, 48_000);
        // Well past the release time constant, so the integrator has converged.
        feed(&m, 1.0, 2.0, 512);
        assert!((rms_of(&m) - 1.0).abs() < 1e-3, "{}", rms_of(&m));
    }

    #[test]
    fn a_single_buffer_does_not_slam_the_bar_to_full_scale() {
        // The bug this fixes: one ~10 ms buffer used to set the displayed level
        // outright, so the meter flickered at the callback rate.
        let m = Meters::new(1, 48_000);
        m.ingest(&[1.0; 512], 1);
        let after_one = rms_of(&m);
        assert!(
            after_one < 0.7,
            "one buffer should move the bar part way, not all the way: {after_one}"
        );
        assert!(after_one > 0.0, "it should still move");
    }

    #[test]
    fn the_bar_rises_at_the_attack_time_constant() {
        let m = Meters::new(1, 48_000);
        // One time constant of a step reaches 1-1/e of the way, in amplitude.
        feed(&m, 1.0, ATTACK_TAU_S, 64);
        let expected = 1.0 - (-1.0f32).exp();
        assert!(
            (rms_of(&m) - expected).abs() < 0.02,
            "{} vs {expected}",
            rms_of(&m)
        );
    }

    #[test]
    fn the_tail_decays_linearly_in_db_not_as_a_square_root() {
        // Regression: smoothing power instead of amplitude halved the decay rate in
        // dB, so a second of silence after a loud passage still showed a bar.
        let m = Meters::new(1, 48_000);
        feed(&m, 1.0, 1.0, 512);
        feed(&m, 0.0, RELEASE_TAU_S, 64);
        // One time constant of decay is 1/e in amplitude, i.e. -8.7 dB.
        let db = to_dbfs(rms_of(&m));
        assert!(
            (db + 8.686).abs() < 0.5,
            "one release constant should be -8.7 dB, got {db}"
        );
    }

    #[test]
    fn the_bar_reaches_the_floor_within_about_a_second_of_silence() {
        let m = Meters::new(1, 48_000);
        feed(&m, 1.0, 1.0, 512);
        feed(&m, 0.0, 1.5, 512);
        assert_eq!(
            to_dbfs(rms_of(&m)),
            FLOOR_DBFS,
            "a meter that hangs after the sound stops reads as broken"
        );
    }

    #[test]
    fn it_falls_more_slowly_than_it_rises() {
        let rise = Meters::new(1, 48_000);
        feed(&rise, 1.0, 0.05, 64);
        let risen = rms_of(&rise);

        let fall = Meters::new(1, 48_000);
        feed(&fall, 1.0, 2.0, 64);
        feed(&fall, 0.0, 0.05, 64);
        let fallen_from_full = rms_of(&fall);

        // In 50 ms it climbs most of the way up but has barely started coming down.
        assert!(risen > 0.7, "attack too slow: {risen}");
        assert!(fallen_from_full > 0.6, "release too fast: {fallen_from_full}");
    }

    #[test]
    fn ballistics_do_not_depend_on_the_devices_buffer_size() {
        // The whole reason alpha is derived from block duration. A device handing
        // us 64-frame buffers must meter identically to one handing us 2048.
        let small = Meters::new(1, 48_000);
        let large = Meters::new(1, 48_000);
        feed(&small, 0.5, 0.25, 64);
        feed(&large, 0.5, 0.25, 2048);
        let (a, b) = (rms_of(&small), rms_of(&large));
        assert!((a - b).abs() < 0.01, "64-frame {a} vs 2048-frame {b}");
    }

    #[test]
    fn silence_eventually_reads_as_silence() {
        let m = Meters::new(1, 48_000);
        feed(&m, 1.0, 1.0, 512);
        feed(&m, 0.0, 3.0, 512);
        assert!(rms_of(&m) < 1e-4, "{}", rms_of(&m));
    }

    #[test]
    fn clip_is_sticky_until_cleared() {
        let m = Meters::new(1, 48_000);
        m.ingest(&[1.0], 1);
        assert!(m.clipped(0));
        m.ingest(&[0.0], 1);
        assert!(m.clipped(0), "clip indicator must latch");
        m.clear_clip();
        assert!(!m.clipped(0));
    }

    #[test]
    fn just_below_full_scale_does_not_clip() {
        let m = Meters::new(1, 48_000);
        m.ingest(&[0.999], 1);
        assert!(!m.clipped(0));
    }

    #[test]
    fn reset_clears_everything_a_take_should_not_inherit() {
        let m = Meters::new(1, 48_000);
        m.ingest(&[1.0], 1);
        m.note_overrun();
        assert!(m.clipped(0));
        assert_eq!(m.overruns(), 1);

        m.reset();

        assert!(!m.clipped(0), "a clip while monitoring is not a clip in the take");
        assert_eq!(m.overruns(), 0);
        let mut peaks = Vec::new();
        m.take_peaks(&mut peaks);
        assert_eq!(peaks[0], 0.0);
    }

    #[test]
    fn dbfs_landmarks() {
        assert!((to_dbfs(1.0) - 0.0).abs() < 1e-4);
        assert!((to_dbfs(0.5) + 6.0206).abs() < 1e-3);
        assert_eq!(to_dbfs(0.0), FLOOR_DBFS);
        // Below the floor clamps rather than running to -inf.
        assert_eq!(to_dbfs(1e-12), FLOOR_DBFS);
    }

    #[test]
    fn meter_fraction_spans_floor_to_full_scale() {
        assert_eq!(dbfs_to_fraction(FLOOR_DBFS), 0.0);
        assert_eq!(dbfs_to_fraction(0.0), 1.0);
        assert!((dbfs_to_fraction(FLOOR_DBFS / 2.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn ingest_tolerates_a_ragged_buffer() {
        let m = Meters::new(2, 48_000);
        // Five samples across two channels: the trailing partial frame is ignored
        // rather than read out of bounds.
        m.ingest(&[0.1, 0.2, 0.3, 0.4, 0.5], 2);
        let mut peaks = Vec::new();
        m.take_peaks(&mut peaks);
        assert!((peaks[0] - 0.3).abs() < 1e-6, "{peaks:?}");
        assert!((peaks[1] - 0.4).abs() < 1e-6, "{peaks:?}");
    }
}
