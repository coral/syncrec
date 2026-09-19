//! Level metering, published from the realtime callback without locks.
//!
//! The audio thread must never block, so levels travel as plain atomics. Peak uses
//! `fetch_max` on the raw `f32` bit pattern, which is valid here because IEEE-754
//! bit patterns are monotonically ordered for non-negative floats and we only ever
//! store magnitudes.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Anything quieter than this reads as silence on the meter.
pub const FLOOR_DBFS: f32 = -72.0;

pub struct Meters {
    /// Largest magnitude seen since the UI last looked.
    peak: Vec<AtomicU32>,
    /// Mean square of the most recent callback buffer.
    mean_square: Vec<AtomicU32>,
    /// Sticky until the UI clears it.
    clipped: Vec<AtomicBool>,
    /// Callbacks whose audio we could not fit in the ring buffer.
    overruns: AtomicU64,
}

impl Meters {
    pub fn new(channels: usize) -> Self {
        Self {
            peak: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            mean_square: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            clipped: (0..channels).map(|_| AtomicBool::new(false)).collect(),
            overruns: AtomicU64::new(0),
        }
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

            // Monotone on non-negative floats, so max-of-bits is max-of-values.
            self.peak[ch].fetch_max(peak.to_bits(), Ordering::Relaxed);
            self.mean_square[ch].store(
                ((sum_sq / frames as f64) as f32).to_bits(),
                Ordering::Relaxed,
            );
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

    /// Current RMS per channel. Not reset by reading.
    pub fn rms(&self, out: &mut Vec<f32>) {
        out.clear();
        for m in &self.mean_square {
            out.push(f32::from_bits(m.load(Ordering::Relaxed)).max(0.0).sqrt());
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
        for m in &self.mean_square {
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
        let m = Meters::new(2);
        // Interleaved L,R. Left peaks at 0.5, right at -0.8 (magnitude 0.8).
        m.ingest(&[0.1, -0.2, 0.5, -0.8, -0.3, 0.4], 2);
        let mut peaks = Vec::new();
        m.take_peaks(&mut peaks);
        assert!((peaks[0] - 0.5).abs() < 1e-6, "{peaks:?}");
        assert!((peaks[1] - 0.8).abs() < 1e-6, "{peaks:?}");
    }

    #[test]
    fn peak_holds_across_buffers_until_read() {
        let m = Meters::new(1);
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

    #[test]
    fn rms_of_a_full_scale_square_is_one() {
        let m = Meters::new(1);
        m.ingest(&[1.0, -1.0, 1.0, -1.0], 1);
        let mut rms = Vec::new();
        m.rms(&mut rms);
        assert!((rms[0] - 1.0).abs() < 1e-6, "{rms:?}");
    }

    #[test]
    fn clip_is_sticky_until_cleared() {
        let m = Meters::new(1);
        m.ingest(&[1.0], 1);
        assert!(m.clipped(0));
        m.ingest(&[0.0], 1);
        assert!(m.clipped(0), "clip indicator must latch");
        m.clear_clip();
        assert!(!m.clipped(0));
    }

    #[test]
    fn just_below_full_scale_does_not_clip() {
        let m = Meters::new(1);
        m.ingest(&[0.999], 1);
        assert!(!m.clipped(0));
    }

    #[test]
    fn reset_clears_everything_a_take_should_not_inherit() {
        let m = Meters::new(1);
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
        let m = Meters::new(2);
        // Five samples across two channels: the trailing partial frame is ignored
        // rather than read out of bounds.
        m.ingest(&[0.1, 0.2, 0.3, 0.4, 0.5], 2);
        let mut peaks = Vec::new();
        m.take_peaks(&mut peaks);
        assert!((peaks[0] - 0.3).abs() < 1e-6, "{peaks:?}");
        assert!((peaks[1] - 0.4).abs() < 1e-6, "{peaks:?}");
    }
}
