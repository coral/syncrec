//! Ethersync as a time reference: LAN timecode instead of a public NTP server.
//!
//! The two modes answer different questions. NTP answers "what time is it really?",
//! and every recorder on the job answers it separately, so two machines agree only
//! as well as their two independent network paths allow. Ethersync answers "what
//! time does the *leader* think it is?", which is a worse question to ask of the
//! universe and a much better one to ask of a rig: every follower is wrong by the
//! same amount, so the takes line up with each other exactly, which is the property
//! a multitrack shoot actually needs.
//!
//! Timecode is anchored to local time of day, the way production sound has always
//! done it. That choice is what lets everything downstream stay as it was: the
//! timeline resolves to a real UTC instant, so the drift log, the drift fit, the
//! safety gate and `bext` never learn that the clock changed underneath them.
//!
//! The leader's transport *is* the record state. Rolling means playing, stopping
//! means paused, and a follower watching the rate change is watching the record
//! button on another machine.

use std::net::SocketAddr;
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local, Utc};
use libethersync::{
    ConnectionState, Discovery, DiscoveryConfig, DiscoveredLeader, Engine, Event, FollowerConfig,
    FrameFormat, Leader, LeaderConfig, MonotonicClock, Position, Rate, Reading, TimecodeReader,
    TimecodeSnapshot, Trust,
};
use libethersync::SyncState as LinkSync;

use crate::clock::{RefStatus, Reference, TimecodeFormat, unix_nanos};

/// Nanoseconds in a day. Timecode wraps here and so does everything derived from it.
const DAY_NS: i128 = 86_400_000_000_000;

/// Which end of the rig this machine is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Role {
    /// This recorder drives the timecode and the record state for everyone else.
    #[default]
    Leader,
    /// This recorder takes both from the leader.
    Follower,
}

impl Role {
    pub const ALL: [Role; 2] = [Role::Leader, Role::Follower];

    pub fn label(self) -> &'static str {
        match self {
            Role::Leader => "Leader",
            Role::Follower => "Follower",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The frame rates ethersync accepts, as the operator thinks of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Fps {
    F23_976,
    F24,
    /// The default. Anything else on a job is a decision somebody made on purpose.
    #[default]
    F25,
    F29_97,
    F30,
    F47_952,
    F48,
    F50,
    F59_94,
    F60,
}

impl Fps {
    pub const ALL: [Fps; 10] = [
        Fps::F23_976,
        Fps::F24,
        Fps::F25,
        Fps::F29_97,
        Fps::F30,
        Fps::F47_952,
        Fps::F48,
        Fps::F50,
        Fps::F59_94,
        Fps::F60,
    ];

    fn ratio(self) -> (u32, u32) {
        match self {
            Fps::F23_976 => (24000, 1001),
            Fps::F24 => (24, 1),
            Fps::F25 => (25, 1),
            Fps::F29_97 => (30000, 1001),
            Fps::F30 => (30, 1),
            Fps::F47_952 => (48000, 1001),
            Fps::F48 => (48, 1),
            Fps::F50 => (50, 1),
            Fps::F59_94 => (60000, 1001),
            Fps::F60 => (60, 1),
        }
    }

    /// Whether this rate can carry drop-frame numbering at all. Only the 1000/1001
    /// rates that drift against the wall clock by 3.6 s an hour have anything to drop.
    pub fn supports_drop_frame(self) -> bool {
        matches!(self, Fps::F29_97 | Fps::F59_94)
    }

    pub fn format(self, drop_frame: bool) -> Result<FrameFormat> {
        let (n, d) = self.ratio();
        FrameFormat::new(n, d, drop_frame && self.supports_drop_frame())
            .map_err(|e| anyhow!("{n}/{d} is not a usable frame rate: {e}"))
    }
}

impl std::fmt::Display for Fps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Fps::F23_976 => "23.976",
            Fps::F24 => "24",
            Fps::F25 => "25",
            Fps::F29_97 => "29.97",
            Fps::F30 => "30",
            Fps::F47_952 => "47.952",
            Fps::F48 => "48",
            Fps::F50 => "50",
            Fps::F59_94 => "59.94",
            Fps::F60 => "60",
        })
    }
}

// ---------------------------------------------------------------------------
// Timecode as time of day
// ---------------------------------------------------------------------------

/// Real time since local midnight that a timecode position represents.
///
/// Deliberately *not* drop-frame aware, which looks like a bug and is not.
/// Drop-frame only renames labels — it skips two numbers a minute so that the
/// printed label tracks the wall clock. The underlying position still advances at
/// the true frame rate, and the leader anchors it by advancing from midnight in
/// real nanoseconds, so position over frame rate is the real offset into the day in
/// both numbering schemes.
pub fn tod_nanos(position: Position, format: FrameFormat) -> i128 {
    let num = format.numerator() as i128;
    let den = format.denominator() as i128;
    // Split the fixed-point position so the whole-frame term cannot overflow even
    // for an absurd leader position; each half is exact to under a nanosecond.
    let whole = position.frames as i128 * den * 1_000_000_000 / num;
    let frac = (position.subframe as i128 * den * 1_000_000_000) / (num << 32);
    (whole + frac).rem_euclid(DAY_NS)
}

/// The timecode position that represents a given real time since local midnight.
///
/// The inverse of [`tod_nanos`], and what the leader uses to anchor itself.
pub fn position_for_tod(tod_ns: i128, format: FrameFormat) -> Position {
    let ns = tod_ns.rem_euclid(DAY_NS).min(i64::MAX as i128) as i64;
    Position::ZERO.advance(ns, format, Rate::NORMAL)
}

/// Place a time of day on the calendar.
///
/// The OS clock chooses the date and nothing else. That is not a hole in the
/// premise of this program — the OS clock is untrusted for *time*, and picking
/// between yesterday, today and tomorrow needs it to be right to within hours.
pub fn unix_nanos_for_tod(tod_ns: i128, now_unix_nanos: i128) -> i128 {
    use chrono::{Datelike, TimeZone};

    let tod = tod_ns.rem_euclid(DAY_NS);
    let secs = (tod / 1_000_000_000) as u32;
    let nanos = (tod % 1_000_000_000) as u32;

    let now = DateTime::<Utc>::from_timestamp_nanos(
        now_unix_nanos.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
    )
    .with_timezone(&Local);
    let today = now.date_naive();

    [today.pred_opt(), Some(today), today.succ_opt()]
        .into_iter()
        .flatten()
        .filter_map(|d| {
            Local
                .with_ymd_and_hms(d.year(), d.month(), d.day(), secs / 3600, secs / 60 % 60, secs % 60)
                // A local time inside a spring-forward gap does not exist. Taking
                // the earliest representable instant keeps the day as a candidate
                // instead of silently dropping it.
                .earliest()
        })
        .filter_map(|dt| dt.timestamp_nanos_opt())
        .map(|n| n as i128 + nanos as i128)
        .min_by_key(|c| (c - now_unix_nanos).abs())
        .unwrap_or(now_unix_nanos)
}

// ---------------------------------------------------------------------------
// The reference
// ---------------------------------------------------------------------------

/// The `(instant, utc, time of day)` correspondence that fixes which calendar day
/// a take belongs to.
#[derive(Debug, Clone, Copy)]
struct DayAnchor {
    at: Instant,
    utc: i128,
    tod: i128,
}

/// An ethersync timeline behind [`Reference`], so a take cannot tell the difference.
pub struct TimecodeReference {
    reader: TimecodeReader,
    clock: MonotonicClock,
    role: Role,
    source: String,
    /// The published state as of the last refresh.
    ///
    /// Every mark in a writer pass is resolved against this one frozen copy, so a
    /// pass cannot straddle a transport change and stamp half its marks against a
    /// timeline the other half never saw.
    snapshot: Option<TimecodeSnapshot>,
    latest: Option<Reading>,
    anchor: Option<DayAnchor>,
}

/// Whether a reading can place a timestamp at all.
fn usable(role: Role, r: &Reading) -> bool {
    // A paused timeline cannot place anything, including marks captured while it
    // was still running. Once the leader pauses, its anchor is
    // `(pause_time, pause_position, PAUSED)` and evaluating *any* instant against
    // it returns the frozen position — there is no history to extrapolate back
    // through.
    //
    // This is not hypothetical. A follower notices the leader stop up to one tick
    // late, and the writer thread then drains and resolves whatever marks are
    // still in the ring. Without this check the last of them would be stamped with
    // the pause instant instead of its own, planting an outlier of up to a frame
    // at the far end of the drift fit's lever arm, which is the worst possible
    // place for one. Dropping that mark costs a second of observation span;
    // keeping it would bend the measured sample rate.
    //
    // Note this is about resolving timestamps, not about arming the recorder: an
    // idle leader is paused and is still perfectly ready to record, which is why
    // `status_of` reports it synced regardless.
    if r.rate == Rate::PAUSED {
        return false;
    }
    match role {
        // A leader generates the timeline from its own monotonic clock. There is
        // nothing to acquire and nothing to lose.
        Role::Leader => true,
        // Holdover counts: a follower that has locked and then lost the link is
        // still running the leader's timeline, just with widening uncertainty.
        // Refusing to timestamp there would throw away a take over a dropped
        // packet. The gate still sees that it was not synchronised.
        Role::Follower => matches!(
            r.status.synchronization,
            LinkSync::Synchronized | LinkSync::Holdover
        ),
    }
}

impl Reference for TimecodeReference {
    fn refresh(&mut self) {
        let snapshot = self.reader.snapshot();
        let now = Instant::now();
        let reading = self.clock.ns_at(now).map(|ns| snapshot.evaluate(ns));

        // Fix the calendar day once, the first time the timeline is worth reading.
        // Doing it here rather than lazily inside `utc_nanos` is what lets that
        // stay a pure function of the snapshot, and it pins the day to an instant
        // we chose rather than to whichever mark happened to resolve first.
        if self.anchor.is_none()
            && let Some(r) = reading
            && usable(self.role, &r)
        {
            let tod = tod_nanos(r.position, r.format);
            self.anchor = Some(DayAnchor {
                at: now,
                utc: unix_nanos_for_tod(tod, unix_nanos(SystemTime::now())),
                tod,
            });
        }

        self.latest = reading;
        self.snapshot = Some(snapshot);
    }

    fn utc_nanos(&self, at: Instant) -> Option<i128> {
        let snapshot = self.snapshot?;
        let anchor = self.anchor?;
        let reading = snapshot.evaluate(self.clock.ns_at(at)?);
        if !usable(self.role, &reading) {
            return None;
        }
        let tod = tod_nanos(reading.position, reading.format);

        // Midnight of the day this take belongs to, decided once. Adding the time
        // of day back on would wrap a take that crosses midnight straight back to
        // the previous morning, so pick the whole day that puts the result nearest
        // to where the monotonic clock says we should be. The local clock would
        // have to be out by twelve hours for that to choose wrong.
        let midnight = anchor.utc - anchor.tod;
        let expected = anchor.utc + signed_nanos(anchor.at, at);
        let raw = midnight + tod;
        let days = ((expected - raw) as f64 / DAY_NS as f64).round() as i128;
        Some(raw + days * DAY_NS)
    }

    fn status(&self) -> RefStatus {
        status_of(self.role, &self.source, self.latest.as_ref())
    }

    fn timecode_format(&self) -> Option<TimecodeFormat> {
        // A follower adopts whatever the leader is running, which is not
        // necessarily what this machine was configured for; report what is
        // actually on the wire rather than what we asked for.
        let format = self.latest.map(|r| r.format)?;
        Some(TimecodeFormat {
            numerator: format.numerator(),
            denominator: format.denominator(),
            drop_frame: format.drop_frame(),
        })
    }
}

/// The slate label a UTC instant carries in a given timecode format.
///
/// Timecode here is time of day, so this is just the local wall time of `t0`
/// rendered in frames — which is exactly what a sound report needs and what
/// `bext.TimeReference` cannot express.
pub fn label_at(unix_nanos: i128, tc: TimecodeFormat) -> Option<String> {
    let format = FrameFormat::new(tc.numerator, tc.denominator, tc.drop_frame).ok()?;
    let position = position_for_tod(tod_of(unix_nanos), format);
    Some(format.label(position).to_string())
}

/// Signed nanoseconds from `base` to `t`.
fn signed_nanos(base: Instant, t: Instant) -> i128 {
    if t >= base {
        t.duration_since(base).as_nanos() as i128
    } else {
        -(base.duration_since(t).as_nanos() as i128)
    }
}

/// What a reading says about its own trustworthiness.
fn status_of(role: Role, source: &str, reading: Option<&Reading>) -> RefStatus {
    match role {
        Role::Leader => RefStatus {
            kind: "ethersync",
            source: source.to_string(),
            // A leader is the reference. There is no exchange to wait for and no
            // offset to converge, so it is ready from the moment it exists; the
            // only error is in sampling the OS date at the top of the take.
            synced: true,
            label: match reading.map(|r| r.rate) {
                Some(rate) if rate != Rate::PAUSED => "rolling".into(),
                _ => "idle".into(),
            },
            samples: None,
            discarded: None,
            dispersion_s: None,
            slope_ppm: None,
        },
        Role::Follower => {
            let Some(r) = reading else {
                return RefStatus {
                    kind: "ethersync",
                    source: source.to_string(),
                    synced: false,
                    label: "connecting".into(),
                    samples: None,
                    discarded: None,
                    dispersion_s: None,
                    slope_ppm: None,
                };
            };
            let (synced, label) = match (r.status.connection, r.status.synchronization) {
                (_, LinkSync::Synchronized) => (true, "synced"),
                (_, LinkSync::Holdover) => (false, "holdover"),
                (ConnectionState::Connected, LinkSync::Acquiring) => (false, "acquiring"),
                (ConnectionState::Connecting, _) => (false, "connecting"),
                (ConnectionState::Shutdown, _) => (false, "shut down"),
                _ => (false, "disconnected"),
            };
            RefStatus {
                kind: "ethersync",
                source: source.to_string(),
                synced,
                label: label.into(),
                // The follower's own count of accepted clock exchanges in this
                // acquisition, so the safety gate can hold both time sources to
                // the same "N independent confirmations" standard rather than
                // letting one in on a single good sample. It resets when the
                // leader starts a new session, which is exactly when the
                // measurements behind it stop meaning anything.
                samples: Some(r.status.accepted_observations),
                discarded: Some(r.status.lost_packets),
                dispersion_s: Some(r.status.uncertainty_ns / 1.0e9),
                slope_ppm: Some(r.status.drift_ppm),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The link
// ---------------------------------------------------------------------------

/// How a link is connected to the rig.
enum Kind {
    Leader(Leader),
    Follower {
        follower: Box<libethersync::Follower>,
        address: SocketAddr,
    },
    /// A follower with nobody to follow yet, browsing for one.
    Browsing(Discovery),
}

/// One ethersync engine and whatever it is currently doing.
///
/// Held for as long as the mode is ethersync, not just while recording: followers
/// have to be watching the leader's transport in order to notice it roll, and a
/// leader has to be advertising in order to be found before the take starts.
pub struct Link {
    engine: Engine,
    kind: Kind,
    clock: MonotonicClock,
    format: FrameFormat,
    role: Role,
    /// A reader for the window, separate from the one a take owns.
    display: Option<TimecodeReader>,
    /// The most recent thing worth telling the operator.
    message: Option<String>,
}

impl Link {
    /// Become the leader, paused until the first take rolls.
    ///
    /// `advertise` publishes over mDNS so followers find this machine without
    /// being told an address. Off is for a network where multicast is unavailable
    /// or unwelcome, and for tests, which should not put a service on the LAN.
    pub fn leader(name: &str, port: u16, format: FrameFormat, advertise: bool) -> Result<Self> {
        let engine = Engine::new().context("starting the ethersync engine")?;
        let clock = engine.clock();
        let config = LeaderConfig {
            bind: SocketAddr::from(([0, 0, 0, 0], port)),
            name: name.chars().take(63).collect::<String>(),
            format,
            // Paused is the resting state: the transport is the record state, so a
            // leader that is not recording must not be running.
            rate: Rate::PAUSED,
            position: Position::ZERO,
            advertise,
            ..Default::default()
        };
        let leader = engine.leader(config).context("becoming the ethersync leader")?;
        let mut link = Self {
            engine,
            kind: Kind::Leader(leader),
            clock,
            format,
            role: Role::Leader,
            display: None,
            message: None,
        };
        link.display = link.reader().ok();
        Ok(link)
    }

    /// Follow a named leader.
    pub fn follower(address: SocketAddr, fingerprint: Option<&str>, format: FrameFormat) -> Result<Self> {
        let engine = Engine::new().context("starting the ethersync engine")?;
        let clock = engine.clock();
        let mut config = FollowerConfig::direct(address);
        config.fallback_format = format;
        if let Some(fp) = fingerprint {
            config.trust = Trust::Pinned(fp.to_string());
        }
        let follower = engine
            .follower(config)
            .with_context(|| format!("following the ethersync leader at {address}"))?;
        let mut link = Self {
            engine,
            kind: Kind::Follower {
                follower: Box::new(follower),
                address,
            },
            clock,
            format,
            role: Role::Follower,
            display: None,
            message: None,
        };
        link.display = link.reader().ok();
        Ok(link)
    }

    /// Browse for leaders without connecting to any of them yet.
    pub fn browsing(format: FrameFormat) -> Result<Self> {
        let engine = Engine::new().context("starting the ethersync engine")?;
        let clock = engine.clock();
        let discovery = engine
            .discovery(DiscoveryConfig::default())
            .context("browsing for ethersync leaders")?;
        Ok(Self {
            engine,
            kind: Kind::Browsing(discovery),
            clock,
            format,
            role: Role::Follower,
            display: None,
            message: None,
        })
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn format(&self) -> FrameFormat {
        self.format
    }

    /// Where this link says its timestamps come from, for the file.
    pub fn source(&self) -> String {
        match &self.kind {
            Kind::Leader(l) => format!("ethersync leader {}", l.info().address),
            Kind::Follower { address, .. } => format!("ethersync follower of {address}"),
            Kind::Browsing(_) => "ethersync (no leader)".into(),
        }
    }

    /// The certificate fingerprint a follower should be told to pin, when we are
    /// the leader. Followers on the same LAN get it from mDNS instead.
    pub fn fingerprint(&self) -> Option<&str> {
        match &self.kind {
            Kind::Leader(l) => Some(l.info().fingerprint.as_str()),
            _ => None,
        }
    }

    /// Addresses a follower could actually be pointed at, when we are the leader.
    ///
    /// The listener binds the IPv4 wildcard so one leader serves Ethernet and
    /// Wi-Fi at once, which makes its bind address useless to show an operator.
    /// This enumerates the interfaces instead. It is not a reader-path operation
    /// and it allocates, so call it when something changed, not every frame.
    pub fn endpoints(&self) -> Vec<SocketAddr> {
        let Kind::Leader(leader) = &self.kind else {
            return Vec::new();
        };
        let mut endpoints = leader.info().local_endpoints().unwrap_or_default();
        // Loopback is real but it is not what anyone is about to type into
        // another machine, and listing it first would be actively unhelpful.
        endpoints.sort_by_key(|a| a.ip().is_loopback());
        endpoints
    }

    pub fn address(&self) -> Option<SocketAddr> {
        match &self.kind {
            Kind::Leader(l) => Some(l.info().address),
            Kind::Follower { address, .. } => Some(*address),
            Kind::Browsing(_) => None,
        }
    }

    fn reader(&self) -> Result<TimecodeReader> {
        match &self.kind {
            Kind::Leader(l) => l.reader().context("opening a leader timecode reader"),
            Kind::Follower { follower, .. } => follower
                .reader()
                .context("opening a follower timecode reader"),
            Kind::Browsing(_) => Err(anyhow!("no leader selected yet")),
        }
    }

    /// A reference a take can be measured against. One per take.
    pub fn reference(&self) -> Result<TimecodeReference> {
        Ok(TimecodeReference {
            reader: self.reader()?,
            clock: self.clock,
            role: self.role,
            source: self.source(),
            snapshot: None,
            latest: None,
            anchor: None,
        })
    }

    /// The current reading, for the window.
    pub fn reading(&mut self) -> Option<Reading> {
        self.display.as_mut().map(|r| r.read())
    }

    pub fn status(&mut self) -> RefStatus {
        let source = self.source();
        let role = self.role;
        let reading = self.reading();
        status_of(role, &source, reading.as_ref())
    }

    /// Whether the transport is running, which is to say whether the rig is
    /// recording. `None` when there is nothing to read yet.
    pub fn rolling(&mut self) -> Option<bool> {
        self.reading().map(|r| r.rate != Rate::PAUSED)
    }

    /// Roll: anchor the timeline to the time of day and start it.
    ///
    /// The wall clock is sampled between two engine-clock reads so that the anchor
    /// carries the instant the time was actually taken, rather than the instant the
    /// command reached the worker. Queue delay would otherwise make every
    /// follower's timecode late by however long the leader was busy.
    pub fn roll(&mut self) -> Result<()> {
        let Kind::Leader(leader) = &self.kind else {
            return Ok(());
        };
        let before = self.engine.clock().now_ns();
        let wall = unix_nanos(SystemTime::now());
        let after = self.engine.clock().now_ns();
        let sampled_at = before + (after - before) / 2;

        let position = position_for_tod(tod_of(wall), self.format);
        leader
            .set_transport(position, Rate::NORMAL, Some(sampled_at))
            .context("rolling the ethersync transport")?;
        Ok(())
    }

    /// Stop the transport. Followers see the rate go to zero and finalise.
    pub fn halt(&mut self) -> Result<()> {
        let Kind::Leader(leader) = &self.kind else {
            return Ok(());
        };
        leader.pause().context("pausing the ethersync transport")?;
        Ok(())
    }

    /// Leaders discovered on the LAN. Empty unless this link is browsing.
    pub fn discovered(&mut self) -> Vec<DiscoveredLeader> {
        match &mut self.kind {
            Kind::Browsing(d) => d.poll(),
            _ => Vec::new(),
        }
    }

    /// Drain engine events, keeping the last one worth showing.
    pub fn poll_events(&mut self) {
        let mut latest = None;
        loop {
            let event = match &self.kind {
                Kind::Leader(l) => l.try_event(),
                Kind::Follower { follower, .. } => follower.try_event(),
                Kind::Browsing(_) => None,
            };
            let Some(event) = event else { break };
            match event {
                Event::Error(e) => latest = Some(e),
                Event::Connection(state) => latest = Some(format!("link {state:?}")),
                Event::SourceHealth(h) => latest = Some(format!("source {h:?}")),
                // Corrections and clock observations are the link working, not news.
                Event::Correction(_) | Event::ClockObservation(_) | Event::ProbeTiming { .. } => {}
            }
        }
        if latest.is_some() {
            self.message = latest;
        }
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// The timecode as it would be read off a slate, or `None` before first read.
    pub fn label(&mut self) -> Option<String> {
        self.reading().map(|r| r.label().to_string())
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        // The engine owns a worker thread and, as a leader, an mDNS registration.
        // Neither should outlive a mode switch.
        let _ = self.engine.shutdown();
    }
}

/// Time of day, in nanoseconds, of a unix instant, in the local zone.
fn tod_of(unix_nanos: i128) -> i128 {
    use chrono::Timelike;
    let local = DateTime::<Utc>::from_timestamp_nanos(
        unix_nanos.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
    )
    .with_timezone(&Local);
    local.num_seconds_from_midnight() as i128 * 1_000_000_000
        + local.nanosecond().min(999_999_999) as i128
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(n: u32, d: u32, drop: bool) -> FrameFormat {
        FrameFormat::new(n, d, drop).unwrap()
    }

    #[test]
    fn a_whole_hour_of_frames_is_a_whole_hour() {
        for (n, d) in [(25, 1), (30, 1), (24, 1), (50, 1)] {
            let f = fmt(n, d, false);
            let pos = Position::from_frames(3600 * n as i64);
            assert_eq!(tod_nanos(pos, f), 3_600_000_000_000, "{n}/{d}");
        }
    }

    #[test]
    fn fractional_rates_keep_real_time_not_frame_count() {
        // 30000/1001 runs slow: 30 frames take 1.001 s of real time, and it is real
        // time the file has to be stamped with.
        let f = fmt(30000, 1001, false);
        assert_eq!(tod_nanos(Position::from_frames(30), f), 1_001_000_000);
    }

    #[test]
    fn drop_frame_does_not_change_where_a_position_falls_in_the_day() {
        // Drop-frame renumbers labels; it does not bend the timeline. Both formats
        // must place the same position at the same real time, or a drop-frame rig
        // would stamp its files 3.6 s per hour away from a non-drop one.
        let plain = fmt(30000, 1001, false);
        let drop = fmt(30000, 1001, true);
        for frames in [0, 1, 1799, 1800, 107_892, 2_589_408] {
            let p = Position::from_frames(frames);
            assert_eq!(tod_nanos(p, plain), tod_nanos(p, drop), "at {frames}");
        }
    }

    #[test]
    fn subframes_survive_the_conversion() {
        let f = fmt(25, 1, false);
        // Half a frame at 25 fps is 20 ms.
        let half = Position {
            frames: 0,
            subframe: 1 << 31,
        };
        assert_eq!(tod_nanos(half, f), 20_000_000);
    }

    #[test]
    fn a_position_past_midnight_wraps_into_the_day() {
        let f = fmt(25, 1, false);
        let a_day = Position::from_frames(24 * 3600 * 25);
        assert_eq!(tod_nanos(a_day, f), 0);
        let past = Position::from_frames(24 * 3600 * 25 + 25);
        assert_eq!(tod_nanos(past, f), 1_000_000_000);
    }

    #[test]
    fn negative_positions_wrap_backwards_into_the_previous_day() {
        let f = fmt(25, 1, false);
        // One second before midnight is 23:59:59, not a negative time.
        assert_eq!(
            tod_nanos(Position::from_frames(-25), f),
            DAY_NS - 1_000_000_000
        );
    }

    #[test]
    fn position_and_time_of_day_round_trip() {
        for fps in Fps::ALL {
            let f = fps.format(false).unwrap();
            for tod in [
                0i128,
                1_000_000_000,
                3_600_000_000_000,
                45_296_000_000_000,
                DAY_NS - 1_000_000_000,
            ] {
                let back = tod_nanos(position_for_tod(tod, f), f);
                // Within one frame: a position cannot represent a time of day more
                // precisely than its own subframe resolution.
                let frame_ns = 1_000_000_000i128 * f.denominator() as i128 / f.numerator() as i128;
                assert!(
                    (back - tod).abs() < frame_ns,
                    "{fps} {tod}: got {back}, off by {}",
                    back - tod
                );
            }
        }
    }

    #[test]
    fn a_time_of_day_lands_on_the_nearest_day() {
        // Midday today, with the OS clock also saying midday today.
        let now = unix_nanos(SystemTime::now());
        let tod = tod_of(now);
        let placed = unix_nanos_for_tod(tod, now);
        assert!(
            (placed - now).abs() < 1_000_000_000,
            "same time of day must resolve to the same instant, off by {} ns",
            placed - now
        );
    }

    #[test]
    fn a_timecode_just_before_midnight_resolves_to_yesterday_just_after_it() {
        // The rollover case: the clock says 00:00:05, the timecode says 23:59:55.
        // The only sane reading is ten seconds ago, not almost a day from now.
        let now = unix_nanos(SystemTime::now());
        let today_tod = tod_of(now);
        let tod = (today_tod - 10_000_000_000).rem_euclid(DAY_NS);
        let placed = unix_nanos_for_tod(tod, now);
        assert!(
            (placed - (now - 10_000_000_000)).abs() < 1_000_000_000,
            "expected ten seconds ago, got {} s away",
            (placed - now) as f64 / 1e9
        );
    }

    #[test]
    fn drop_frame_is_refused_for_rates_that_have_nothing_to_drop() {
        // 25 fps does not drift against the wall clock, so drop-frame numbering is
        // meaningless there and ethersync rejects it. Asking for it must not fail
        // the whole configuration.
        assert!(!Fps::F25.supports_drop_frame());
        let f = Fps::F25.format(true).unwrap();
        assert!(!f.drop_frame());

        assert!(Fps::F29_97.supports_drop_frame());
        assert!(Fps::F29_97.format(true).unwrap().drop_frame());
    }

    #[test]
    fn every_offered_frame_rate_is_one_ethersync_accepts() {
        for fps in Fps::ALL {
            assert!(fps.format(false).is_ok(), "{fps}");
        }
    }
}
