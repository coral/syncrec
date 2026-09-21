//! Our own model of the machine's clock, fitted against NTP.
//!
//! The OS wall clock is stepped and slewed by the system time daemon, so we never
//! read it as truth and never try to correct it. Instead we keep a linear model of
//! the *monotonic* clock relative to UTC: the intercept is our offset, the slope is
//! the machine's frequency error. Callers ask "what UTC was this `Instant`?" and get
//! an answer that does not jump when the system clock does.

pub mod bridge;
pub mod reference;
pub mod sntp;

pub use reference::{NtpReference, RefStatus, Reference, TimecodeFormat};

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How many recent exchanges the fit considers. Per spec.
pub const WINDOW: usize = 8;
/// Samples whose round-trip delay exceeds this multiple of the window minimum are
/// assumed to have hit a congested or asymmetric path and are dropped.
pub const DELAY_REJECT_FACTOR: f64 = 2.0;
/// Accepted samples needed before we call ourselves synced.
pub const SYNCED_THRESHOLD: usize = 3;

// A slope fitted from too few samples over too short a span is not a measurement of
// the crystal, it is a measurement of the network's jitter. Observed in practice: a
// 3-sample, 32-second window produced +105 ppm, which would inject milliseconds of
// error when extrapolated. Unless all three gates below pass we report offset only.
/// Minimum survivors before a slope is believed at all.
pub const MIN_SLOPE_SAMPLES: usize = 4;
/// Minimum span in seconds the survivors must cover.
pub const MIN_SLOPE_SPAN: f64 = 60.0;
/// Consumer crystals live inside roughly +/-100 ppm. Beyond this the line is chasing
/// noise, not frequency.
pub const MAX_PLAUSIBLE_PPM: f64 = 200.0;

pub fn unix_nanos(t: SystemTime) -> i128 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

/// One usable SNTP exchange, already re-expressed against the monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct ClockSample {
    /// Seconds from `base_mono` to the midpoint of the exchange.
    pub x: f64,
    /// Correction in seconds: true UTC minus what our uncorrected model predicted.
    pub y: f64,
    /// Round-trip delay in seconds. Drives the rejection filter.
    pub delay: f64,
    /// Raw offset the server implied, in seconds. Logged, not fitted.
    pub offset: f64,
    /// Wall time of the exchange. For the sidecar log only.
    pub at_unix_nanos: i128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    /// No usable exchange yet. Timestamps are not trustworthy.
    Unsynced,
    /// Some samples, but too few to trust the fit.
    Coarse,
    Synced,
}

impl SyncState {
    pub fn label(self) -> &'static str {
        match self {
            SyncState::Unsynced => "unsynced",
            SyncState::Coarse => "coarse",
            SyncState::Synced => "synced",
        }
    }
}

/// The fitted line through the surviving samples.
#[derive(Debug, Clone, Copy)]
pub struct Fit {
    /// Intercept: offset in seconds at `base_mono`.
    pub a: f64,
    /// Slope: fractional frequency error of the monotonic clock (s/s).
    ///
    /// Zero when the window could not support a trustworthy slope; check
    /// `slope_trusted` to tell "measured as zero" from "declined to guess".
    pub b: f64,
    /// Whether the slope gates passed and `b` is a real estimate.
    pub slope_trusted: bool,
    pub residual_rms: f64,
    pub min_delay: f64,
    pub used: usize,
    pub total: usize,
    /// Seconds spanned by the surviving samples.
    pub span: f64,
}

impl Fit {
    /// Slope expressed the way clock people read it.
    pub fn ppm(&self) -> f64 {
        self.b * 1.0e6
    }
}

/// An immutable view of the model, cheap to copy out from under the lock.
#[derive(Debug, Clone, Copy)]
pub struct ClockSnapshot {
    base_mono: Instant,
    base_utc_nanos: i128,
    fit: Option<Fit>,
    pub state: SyncState,
    pub accepted: u64,
    pub rejected: u64,
    pub failed: u64,
}

impl ClockSnapshot {
    /// UTC, as unix nanoseconds, at the given monotonic instant.
    ///
    /// Returns `None` while unsynced rather than handing back a plausible-looking lie.
    pub fn utc_nanos_at(&self, mono: Instant) -> Option<i128> {
        let fit = self.fit?;
        if self.state == SyncState::Unsynced {
            return None;
        }
        // Signed elapsed: marks can predate the first accepted exchange.
        let (dx_nanos, dx_secs) = signed_elapsed(self.base_mono, mono);
        let correction = fit.a + fit.b * dx_secs;
        Some(self.base_utc_nanos + dx_nanos + (correction * 1.0e9).round() as i128)
    }

    /// Best estimate of how wrong we are, in seconds.
    ///
    /// Fit scatter plus half the minimum observed round trip: even a perfect fit
    /// cannot see path asymmetry, and half the fastest round trip bounds it.
    pub fn dispersion(&self) -> Option<f64> {
        let fit = self.fit?;
        Some(fit.residual_rms + fit.min_delay / 2.0)
    }

    pub fn fit(&self) -> Option<Fit> {
        self.fit
    }

    /// How wrong the OS wall clock is right now, in seconds (positive = OS is slow).
    ///
    /// Note this is *not* the fit's intercept. We pin the model's origin to the first
    /// accepted exchange so the fitted correction stays a small, well-conditioned
    /// number; the absolute offset lives in `base_utc_nanos`. This recovers it.
    pub fn system_clock_error(&self) -> Option<f64> {
        let mono = Instant::now();
        let sys = unix_nanos(SystemTime::now());
        let ours = self.utc_nanos_at(mono)?;
        Some((ours - sys) as f64 / 1.0e9)
    }
}

/// Elapsed time from `base` to `t`, signed, as (nanos, seconds).
fn signed_elapsed(base: Instant, t: Instant) -> (i128, f64) {
    if t >= base {
        let d = t.duration_since(base);
        (d.as_nanos() as i128, d.as_secs_f64())
    } else {
        let d = base.duration_since(t);
        (-(d.as_nanos() as i128), -d.as_secs_f64())
    }
}

pub struct ClockModel {
    base_mono: Instant,
    /// UTC at `base_mono`, fixed once from the first accepted sample so that the
    /// fitted correction stays a small number near zero.
    base_utc_nanos: Option<i128>,
    window: VecDeque<ClockSample>,
    log: Vec<ClockSample>,
    fit: Option<Fit>,
    accepted: u64,
    rejected: u64,
    failed: u64,
}

impl ClockModel {
    pub fn new(base_mono: Instant) -> Self {
        Self {
            base_mono,
            base_utc_nanos: None,
            window: VecDeque::with_capacity(WINDOW),
            log: Vec::new(),
            fit: None,
            accepted: 0,
            rejected: 0,
            failed: 0,
        }
    }

    pub fn base_mono(&self) -> Instant {
        self.base_mono
    }

    /// Record an exchange that failed outright (timeout, refused, kiss-o'-death).
    pub fn record_failure(&mut self) {
        self.failed += 1;
    }

    /// Record an exchange discarded because the system clock stepped during it.
    pub fn record_stepped(&mut self) {
        self.rejected += 1;
    }

    /// Feed one good exchange.
    ///
    /// `mono_mid` is the midpoint of the request/reply pair on the monotonic clock,
    /// `utc_mid_nanos` the UTC we believe held at that instant.
    pub fn push(&mut self, mono_mid: Instant, utc_mid_nanos: i128, delay: f64, offset: f64) {
        let (dx_nanos, x) = signed_elapsed(self.base_mono, mono_mid);

        // Pin the origin to the first sample we accept, so `y` is a small correction
        // rather than a billion-second absolute.
        let base = *self.base_utc_nanos.get_or_insert(utc_mid_nanos - dx_nanos);

        let y = (utc_mid_nanos - (base + dx_nanos)) as f64 / 1.0e9;

        let sample = ClockSample {
            x,
            y,
            delay,
            offset,
            at_unix_nanos: utc_mid_nanos,
        };

        if self.window.len() == WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(sample);
        self.log.push(sample);
        self.accepted += 1;
        self.refit();
    }

    /// Drop the samples that took too long, then least-squares the rest.
    fn refit(&mut self) {
        let total = self.window.len();
        if total == 0 {
            self.fit = None;
            return;
        }

        let min_delay = self
            .window
            .iter()
            .map(|s| s.delay)
            .fold(f64::INFINITY, f64::min);
        let cutoff = min_delay * DELAY_REJECT_FACTOR;

        let kept: Vec<&ClockSample> = self.window.iter().filter(|s| s.delay <= cutoff).collect();

        let n = kept.len();
        if n == 0 {
            self.fit = None;
            return;
        }

        let x_min = kept.iter().map(|s| s.x).fold(f64::INFINITY, f64::min);
        let x_max = kept.iter().map(|s| s.x).fold(f64::NEG_INFINITY, f64::max);
        let span = x_max - x_min;

        let inv = 1.0 / n as f64;
        let mean_y = kept.iter().map(|s| s.y).sum::<f64>() * inv;

        // Only attempt a slope when the window can actually support one.
        let mut slope_trusted = n >= MIN_SLOPE_SAMPLES && span >= MIN_SLOPE_SPAN;

        let (a, b) = if !slope_trusted {
            (mean_y, 0.0)
        } else {
            let mx = kept.iter().map(|s| s.x).sum::<f64>() * inv;
            let mut sxx = 0.0;
            let mut sxy = 0.0;
            for s in &kept {
                let dx = s.x - mx;
                sxx += dx * dx;
                sxy += dx * (s.y - mean_y);
            }
            if sxx <= f64::EPSILON {
                slope_trusted = false;
                (mean_y, 0.0)
            } else {
                let b = sxy / sxx;
                // Last gate: a physically impossible slope means we fitted jitter.
                if (b * 1.0e6).abs() > MAX_PLAUSIBLE_PPM {
                    slope_trusted = false;
                    (mean_y, 0.0)
                } else {
                    (mean_y - b * mx, b)
                }
            }
        };

        let sse: f64 = kept
            .iter()
            .map(|s| {
                let r = s.y - (a + b * s.x);
                r * r
            })
            .sum();

        // Divide by the degrees of freedom, not n: with 3 points and 2 fitted
        // parameters an n-denominator flatters the fit and understates dispersion.
        let dof = n.saturating_sub(if slope_trusted { 2 } else { 1 }).max(1);

        self.fit = Some(Fit {
            a,
            b,
            slope_trusted,
            residual_rms: (sse / dof as f64).sqrt(),
            min_delay,
            used: n,
            total,
            span,
        });
    }

    pub fn state(&self) -> SyncState {
        match (self.fit.is_some(), self.accepted as usize) {
            (false, _) | (_, 0) => SyncState::Unsynced,
            (true, n) if n >= SYNCED_THRESHOLD => SyncState::Synced,
            _ => SyncState::Coarse,
        }
    }

    pub fn snapshot(&self) -> ClockSnapshot {
        ClockSnapshot {
            base_mono: self.base_mono,
            base_utc_nanos: self.base_utc_nanos.unwrap_or(0),
            fit: self.fit,
            state: self.state(),
            accepted: self.accepted,
            rejected: self.rejected,
            failed: self.failed,
        }
    }

    /// Every accepted sample, for the audit sidecar.
    pub fn log(&self) -> &[ClockSample] {
        &self.log
    }
}

/// How long we wait between exchanges. Per spec.
pub const POLL_INTERVAL: Duration = Duration::from_secs(16);

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> (ClockModel, Instant) {
        let base = Instant::now();
        (ClockModel::new(base), base)
    }

    #[test]
    fn unsynced_refuses_to_guess() {
        let (m, base) = model();
        assert_eq!(m.state(), SyncState::Unsynced);
        assert!(m.snapshot().utc_nanos_at(base).is_none());
    }

    #[test]
    fn single_sample_gives_offset_but_no_slope() {
        let (mut m, base) = model();
        m.push(base, 1_000_000_000_000, 0.01, 0.0);
        let fit = m.fit.unwrap();
        assert_eq!(fit.b, 0.0);
        assert_eq!(fit.used, 1);
    }

    #[test]
    fn recovers_a_known_slope_and_offset() {
        // Machine runs 20 ppm fast; true UTC therefore lags our raw monotonic
        // reading by a growing amount. Offset at origin is +5 ms.
        let (mut m, base) = model();
        let ppm = 20.0e-6;
        let offset0 = 0.005;
        for k in 0..WINDOW {
            let x = k as f64 * 16.0;
            let correction = offset0 + ppm * x;
            let mono = base + Duration::from_secs_f64(x);
            let utc = 1_000_000_000_000i128 + (x * 1.0e9) as i128 + (correction * 1.0e9) as i128;
            m.push(mono, utc, 0.01, correction);
        }
        let fit = m.fit.unwrap();
        assert!((fit.ppm() - 20.0).abs() < 1e-3, "ppm {}", fit.ppm());
        assert_eq!(fit.used, WINDOW);
        assert_eq!(m.state(), SyncState::Synced);

        // The origin is pinned to the first accepted sample, so the intercept is a
        // correction relative to it, not the absolute offset. The absolute offset
        // is carried in base_utc_nanos; what must hold is that the model reproduces
        // every sample it was given.
        assert!(fit.a.abs() < 1e-9, "intercept should be ~0, got {}", fit.a);
        let snap = m.snapshot();
        for k in 0..WINDOW {
            let x = k as f64 * 16.0;
            let want =
                1_000_000_000_000i128 + (x * 1.0e9) as i128 + ((offset0 + ppm * x) * 1.0e9) as i128;
            let got = snap
                .utc_nanos_at(base + Duration::from_secs_f64(x))
                .unwrap();
            assert!((got - want).abs() < 1_000, "k={k} got {got} want {want}");
        }
    }

    #[test]
    fn slow_samples_are_dropped() {
        let (mut m, base) = model();
        for k in 0..WINDOW {
            let x = k as f64 * 16.0;
            let mono = base + Duration::from_secs_f64(x);
            // One sample takes 10x the others and carries a 50 ms error with it.
            let (delay, err) = if k == 4 { (0.10, 0.050) } else { (0.01, 0.0) };
            let utc = 1_000_000_000_000i128 + (x * 1.0e9) as i128 + (err * 1.0e9) as i128;
            m.push(mono, utc, delay, err);
        }
        let fit = m.fit.unwrap();
        assert_eq!(fit.total, WINDOW);
        assert_eq!(
            fit.used,
            WINDOW - 1,
            "the congested sample should be rejected"
        );
        // With the outlier gone the fit should be flat and clean.
        assert!(fit.ppm().abs() < 1e-3, "ppm {}", fit.ppm());
        assert!(fit.residual_rms < 1e-9);
    }

    #[test]
    fn refuses_an_implausible_slope_from_a_short_noisy_window() {
        // Reproduces what a real 3-sample probe run produced: a late sample carrying
        // several ms of asymmetry, over a span far too short to determine frequency.
        let (mut m, base) = model();
        for (k, (delay, err)) in [(0.0099, 0.0), (0.0085, 0.0), (0.0164, 0.0035)]
            .into_iter()
            .enumerate()
        {
            let x = k as f64 * 16.0;
            let mono = base + Duration::from_secs_f64(x);
            let utc = 1_000_000_000_000i128 + (x * 1.0e9) as i128 + (err * 1.0e9) as i128;
            m.push(mono, utc, delay, err);
        }
        let fit = m.fit.unwrap();
        assert!(
            !fit.slope_trusted,
            "3 samples over 32 s cannot determine a slope"
        );
        assert_eq!(fit.b, 0.0);
        // Falls back to offset-only, which is the mean of the survivors.
        assert!(fit.a.abs() < 0.01);
    }

    #[test]
    fn refuses_a_slope_that_no_crystal_could_have() {
        let (mut m, base) = model();
        // Enough samples and span to pass the first two gates, but the data implies
        // 1000 ppm, which is an order of magnitude beyond any real oscillator.
        for k in 0..WINDOW {
            let x = k as f64 * 16.0;
            let mono = base + Duration::from_secs_f64(x);
            let utc = 1_000_000_000_000i128 + (x * 1.0e9) as i128 + (1000.0e-6 * x * 1.0e9) as i128;
            m.push(mono, utc, 0.01, 0.0);
        }
        let fit = m.fit.unwrap();
        assert!(!fit.slope_trusted, "ppm was {}", fit.ppm());
        assert_eq!(fit.b, 0.0);
    }

    #[test]
    fn a_plausible_slope_over_a_long_span_is_trusted() {
        let (mut m, base) = model();
        for k in 0..WINDOW {
            let x = k as f64 * 16.0;
            let mono = base + Duration::from_secs_f64(x);
            let utc = 1_000_000_000_000i128 + (x * 1.0e9) as i128 + (30.0e-6 * x * 1.0e9) as i128;
            m.push(mono, utc, 0.01, 0.0);
        }
        let fit = m.fit.unwrap();
        assert!(fit.slope_trusted);
        assert!((fit.ppm() - 30.0).abs() < 1e-3);
        assert!(fit.span >= MIN_SLOPE_SPAN);
    }

    #[test]
    fn window_holds_only_the_most_recent() {
        let (mut m, base) = model();
        for k in 0..(WINDOW * 3) {
            let mono = base + Duration::from_secs_f64(k as f64 * 16.0);
            m.push(
                mono,
                1_000_000_000_000 + (k as i128 * 16_000_000_000),
                0.01,
                0.0,
            );
        }
        assert_eq!(m.window.len(), WINDOW);
        assert_eq!(m.accepted, (WINDOW * 3) as u64);
        assert_eq!(m.log().len(), WINDOW * 3);
    }

    #[test]
    fn extrapolates_through_the_fitted_line() {
        let (mut m, base) = model();
        let ppm = 20.0e-6;
        for k in 0..WINDOW {
            let x = k as f64 * 16.0;
            let mono = base + Duration::from_secs_f64(x);
            let utc = 1_000_000_000_000i128 + (x * 1.0e9) as i128 + (ppm * x * 1.0e9) as i128;
            m.push(mono, utc, 0.01, 0.0);
        }
        let snap = m.snapshot();
        // 100 s past the origin the model should have accumulated 100*20ppm = 2 ms.
        let probe = base + Duration::from_secs(100);
        let got = snap.utc_nanos_at(probe).unwrap();
        let want = 1_000_000_000_000i128 + 100_000_000_000 + 2_000_000;
        assert!((got - want).abs() < 10_000, "got {got} want {want}");
    }

    #[test]
    fn marks_before_the_origin_resolve_backwards() {
        let (mut m, base) = model();
        let later = base + Duration::from_secs(60);
        m.push(later, 1_000_000_000_000, 0.01, 0.0);
        m.push(
            later + Duration::from_secs(16),
            1_016_000_000_000,
            0.01,
            0.0,
        );
        let snap = m.snapshot();
        let got = snap.utc_nanos_at(base).unwrap();
        // base is 60 s before the first exchange.
        assert!((got - (1_000_000_000_000i128 - 60_000_000_000)).abs() < 10_000);
    }
}
