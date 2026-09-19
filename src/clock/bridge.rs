//! Bridging the audio device clock to `std::time::Instant`.
//!
//! cpal hands callbacks a `StreamInstant`, which on CoreAudio comes from
//! `mach_absolute_time()` and on WASAPI from `QueryPerformanceCounter()` — the same
//! hardware sources `Instant` is built on, but with a different origin and no public
//! way to read `Instant`'s raw value.
//!
//! cpal 0.18 added `StreamTrait::now()`, which samples the stream's clock from any
//! thread. That gives us the missing correlation: read both clocks back to back,
//! many times, and keep the pair whose two reads were closest together. The residual
//! error is bounded by that gap, which lands well under a microsecond.

use std::time::{Duration, Instant};

use cpal::StreamInstant;
use cpal::traits::StreamTrait;

/// How many correlation attempts to make. Cheap — two clock reads each.
const ATTEMPTS: usize = 64;

#[derive(Debug, Clone, Copy)]
pub struct Bridge {
    /// The stream clock reading at the reference moment, in nanoseconds.
    stream_nanos: u128,
    /// The monotonic clock reading at the same moment.
    instant: Instant,
    /// Half the read-to-read gap of the winning pair: our error bound.
    uncertainty: Duration,
}

impl Bridge {
    /// Correlate the two clocks by racing them against each other.
    pub fn measure<S: StreamTrait>(stream: &S) -> Self {
        let mut best: Option<Bridge> = None;

        for _ in 0..ATTEMPTS {
            let before = Instant::now();
            let s = stream.now();
            let after = Instant::now();

            let gap = after.duration_since(before);
            let candidate = Bridge {
                stream_nanos: s.as_nanos(),
                // The stream read happened somewhere in `before..after`; the midpoint
                // is the best single guess and halves the worst-case error.
                instant: before + gap / 2,
                uncertainty: gap / 2,
            };

            if best.is_none_or(|b| candidate.uncertainty < b.uncertainty) {
                best = Some(candidate);
            }
        }

        best.expect("ATTEMPTS is nonzero")
    }

    /// Map a timestamp from an audio callback onto the monotonic clock.
    pub fn to_instant(&self, s: StreamInstant) -> Instant {
        let s = s.as_nanos();
        if s >= self.stream_nanos {
            let d = nanos_to_duration(s - self.stream_nanos);
            self.instant.checked_add(d).unwrap_or(self.instant)
        } else {
            let d = nanos_to_duration(self.stream_nanos - s);
            self.instant.checked_sub(d).unwrap_or(self.instant)
        }
    }

    /// How far off this correlation could be.
    pub fn uncertainty(&self) -> Duration {
        self.uncertainty
    }
}

fn nanos_to_duration(n: u128) -> Duration {
    Duration::new((n / 1_000_000_000) as u64, (n % 1_000_000_000) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in stream whose clock is `Instant` shifted by a known origin, so we
    /// can assert the bridge actually removes that shift.
    struct FakeStream {
        base: Instant,
        origin_offset: Duration,
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
            let elapsed = Instant::now().duration_since(self.base);
            let total = elapsed + self.origin_offset;
            StreamInstant::new(total.as_secs(), total.subsec_nanos())
        }
    }

    #[test]
    fn removes_a_known_origin_shift() {
        let base = Instant::now();
        let stream = FakeStream {
            base,
            origin_offset: Duration::from_secs(9_000),
        };

        let bridge = Bridge::measure(&stream);

        // A stream timestamp 2 s after the correlation should map to an Instant 2 s
        // after the correlation, with the 9000 s origin shift removed entirely.
        let s = StreamInstant::from_nanos((bridge.stream_nanos + 2_000_000_000) as u64);
        let mapped = bridge.to_instant(s);
        let expected = bridge.instant + Duration::from_secs(2);

        let err = if mapped > expected {
            mapped - expected
        } else {
            expected - mapped
        };
        assert!(err < Duration::from_micros(1), "error {err:?}");
    }

    #[test]
    fn maps_timestamps_before_the_correlation() {
        let base = Instant::now();
        let stream = FakeStream {
            base,
            origin_offset: Duration::from_secs(9_000),
        };
        let bridge = Bridge::measure(&stream);

        let s = StreamInstant::from_nanos((bridge.stream_nanos - 500_000_000) as u64);
        let mapped = bridge.to_instant(s);
        assert!(mapped < bridge.instant);
        let delta = bridge.instant - mapped;
        assert!(
            delta > Duration::from_millis(499) && delta < Duration::from_millis(501),
            "delta {delta:?}"
        );
    }

    #[test]
    fn correlation_is_tight() {
        let base = Instant::now();
        let stream = FakeStream {
            base,
            origin_offset: Duration::from_secs(1),
        };
        let bridge = Bridge::measure(&stream);
        // The plan's bar: well under 100 us.
        assert!(
            bridge.uncertainty() < Duration::from_micros(100),
            "uncertainty {:?}",
            bridge.uncertainty()
        );
    }
}
