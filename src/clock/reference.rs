//! The time reference a take is measured against, whichever kind it is.
//!
//! Until now there was only one: the SNTP-fitted model in this module's parent.
//! Ethersync adds a second, where UTC comes from a timecode timeline shared over
//! the LAN rather than from a public NTP server. Everything downstream of the
//! capture path — the drift log, the drift fit, the safety gate, `bext` — only ever
//! needed one thing from the clock, "what UTC was this `Instant`?", so that is the
//! whole of the interface and both sources fit behind it unchanged.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::{ClockModel, ClockSnapshot, SyncState};

/// The timecode format a reference stamps in, when it stamps timecode at all.
///
/// Kept as plain numbers rather than an ethersync `FrameFormat` so that the file
/// writer and the session do not have to know a timecode library exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimecodeFormat {
    pub numerator: u32,
    pub denominator: u32,
    pub drop_frame: bool,
}

impl std::fmt::Display for TimecodeFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fps = self.numerator as f64 / self.denominator as f64;
        if self.denominator == 1 {
            write!(f, "{}", self.numerator)?;
        } else {
            write!(f, "{fps:.3}")?;
        }
        f.write_str(if self.drop_frame { " DF" } else { " NDF" })
    }
}

/// What a reference says about itself: enough for the safety gate and the file.
#[derive(Debug, Clone, PartialEq)]
pub struct RefStatus {
    /// Which kind of reference this is, for a status line with no room for the
    /// whole of `source`.
    pub kind: &'static str,
    /// Where the timestamps came from, as it should read in the file:
    /// `pool.ntp.org`, `ethersync leader`, `ethersync follower 10.0.0.4:4443`.
    pub source: String,
    /// Whether the reference is good enough to have timestamped this take.
    pub synced: bool,
    /// One word for the window and the file: `synced`, `coarse`, `holdover`.
    pub label: String,
    /// Independent exchanges behind `synced`, when the reference counts such a
    /// thing. `None` means it does not — a leader is its own reference and has
    /// nothing to exchange with — and the gate then skips that criterion rather
    /// than inventing a number to satisfy it.
    pub samples: Option<u64>,
    /// Exchanges the reference threw away, when it counts such a thing: NTP
    /// samples rejected for excessive delay, or packets the link lost.
    pub discarded: Option<u64>,
    /// Best estimate of the timestamp error, in seconds.
    pub dispersion_s: Option<f64>,
    /// Frequency error of the local clock against the reference, in ppm, when it
    /// was measured well enough to believe.
    pub slope_ppm: Option<f64>,
}

impl RefStatus {
    /// Whether a take started now could be corrected. The record button and the
    /// gate both ask this, so they can never disagree about what "ready" means.
    pub fn ready(&self) -> bool {
        self.synced && self.samples.is_none_or(|n| n >= crate::finalize::MIN_CLOCK_SAMPLES)
    }

    /// How many more exchanges are needed, or `None` once there is nothing to wait
    /// for.
    pub fn waiting_for(&self) -> Option<String> {
        if self.ready() {
            return None;
        }
        match self.samples {
            Some(n) if n < crate::finalize::MIN_CLOCK_SAMPLES => Some(format!(
                "waiting for {} {}/{}",
                self.kind,
                n,
                crate::finalize::MIN_CLOCK_SAMPLES
            )),
            _ => Some(format!("{} is {}", self.kind, self.label)),
        }
    }
}

/// Resolves a monotonic instant to UTC, whatever is doing the resolving.
///
/// Split deliberately into a mutable refresh and an immutable read. A reference is
/// a *frozen copy* of whatever its source last published, and it stays frozen
/// until [`refresh`](Reference::refresh) is called again. That is what lets every
/// mark in a writer pass be resolved against one consistent state instead of
/// straddling a change that landed halfway through the loop — and it means a
/// caller that forgets to refresh reads stale data rather than torn data.
pub trait Reference: Send {
    /// Pick up whatever the source has published since the last call.
    ///
    /// Must be called at least once before [`utc_nanos`](Reference::utc_nanos) can
    /// resolve anything, and once per writer pass thereafter.
    fn refresh(&mut self);

    /// UTC in unix nanoseconds at `at`, or `None` while the reference cannot say.
    ///
    /// Returning `None` is not a failure: the caller parks the mark and retries it,
    /// which is exactly what happens to the anchor of a take started before the
    /// clock has a fix.
    fn utc_nanos(&self, at: Instant) -> Option<i128>;

    fn status(&self) -> RefStatus;

    /// The timecode format this reference stamps in. `None` for a clock that only
    /// knows UTC, which is every NTP reference: there is no frame rate to report.
    fn timecode_format(&self) -> Option<TimecodeFormat> {
        None
    }
}

/// The SNTP-fitted clock, behind the common interface.
pub struct NtpReference {
    server: String,
    /// Absent for a frozen reference, which is what tests and the headless probes
    /// want: a fixed model that cannot move underneath an assertion.
    model: Option<Arc<Mutex<ClockModel>>>,
    snapshot: ClockSnapshot,
}

impl NtpReference {
    pub fn new(server: String, model: Arc<Mutex<ClockModel>>) -> Self {
        let snapshot = model
            .lock()
            .map(|m| m.snapshot())
            .unwrap_or_else(|_| ClockModel::new(Instant::now()).snapshot());
        Self {
            server,
            model: Some(model),
            snapshot,
        }
    }

    /// A reference pinned to one snapshot, for tests and offline finalising.
    pub fn fixed(server: String, snapshot: ClockSnapshot) -> Self {
        Self {
            server,
            model: None,
            snapshot,
        }
    }

    pub fn snapshot(&self) -> ClockSnapshot {
        self.snapshot
    }

    /// The status a snapshot implies, without needing a whole reference.
    pub fn status_of(server: &str, snap: &ClockSnapshot) -> RefStatus {
        RefStatus {
            kind: "NTP",
            source: server.to_string(),
            synced: snap.state == SyncState::Synced,
            label: snap.state.label().to_string(),
            samples: Some(snap.accepted),
            discarded: Some(snap.rejected),
            dispersion_s: snap.dispersion(),
            slope_ppm: snap.fit().filter(|f| f.slope_trusted).map(|f| f.ppm()),
        }
    }
}

impl Reference for NtpReference {
    fn refresh(&mut self) {
        if let Some(model) = &self.model
            && let Ok(m) = model.lock()
        {
            self.snapshot = m.snapshot();
        }
    }

    fn utc_nanos(&self, at: Instant) -> Option<i128> {
        self.snapshot.utc_nanos_at(at)
    }

    fn status(&self) -> RefStatus {
        Self::status_of(&self.server, &self.snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn synced(base: Instant) -> ClockSnapshot {
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

    #[test]
    fn a_frozen_reference_resolves_like_its_snapshot() {
        let base = Instant::now();
        let snap = synced(base);
        let r = NtpReference::fixed("time.apple.com".into(), snap);
        assert_eq!(r.utc_nanos(base), snap.utc_nanos_at(base));
        assert!(r.status().synced);
        assert_eq!(r.status().source, "time.apple.com");
    }

    #[test]
    fn an_unsynced_reference_declines_rather_than_guesses() {
        let base = Instant::now();
        let r = NtpReference::fixed("pool.ntp.org".into(), ClockModel::new(base).snapshot());
        assert!(r.utc_nanos(base).is_none());
        assert!(!r.status().synced);
        assert!(!r.status().ready());
    }

    #[test]
    fn readiness_needs_the_exchanges_as_well_as_the_state() {
        let base = Instant::now();
        let mut m = ClockModel::new(base);
        // One exchange is enough to have a fit, and nowhere near enough to trust it.
        m.push(base, 1_000_000_000_000, 0.01, 0.0);
        let status = NtpReference::status_of("s", &m.snapshot());
        assert!(!status.ready(), "one exchange must not read as ready");
        assert!(status.waiting_for().is_some());
    }

    #[test]
    fn a_reference_that_counts_nothing_is_ready_once_it_is_synced() {
        // The leader case: nothing to exchange with, so the exchange count must not
        // be able to hold it back.
        let status = RefStatus {
            kind: "ethersync",
            source: "ethersync leader".into(),
            synced: true,
            label: "leader".into(),
            samples: None,
            discarded: None,
            dispersion_s: None,
            slope_ppm: None,
        };
        assert!(status.ready());
        assert_eq!(status.waiting_for(), None);
    }

    #[test]
    fn refresh_picks_up_what_the_poller_has_learned() {
        let base = Instant::now();
        let model = Arc::new(Mutex::new(ClockModel::new(base)));
        let mut r = NtpReference::new("s".into(), Arc::clone(&model));
        assert!(r.utc_nanos(base).is_none());

        *model.lock().unwrap() = {
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
            m
        };
        assert!(
            r.utc_nanos(base).is_none(),
            "a reference must not see a change it has not refreshed for"
        );
        r.refresh();
        assert!(r.utc_nanos(base).is_some());
    }
}
