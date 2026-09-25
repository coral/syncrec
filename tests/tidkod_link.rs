//! Two tidkod links on loopback, exercised the way a rig uses them.
//!
//! The unit tests in `src/tidkod.rs` check the timecode arithmetic against
//! itself. These check the claim that actually matters: that a follower's idea of
//! UTC agrees with the leader's, that the leader's transport reaches the follower,
//! and that both of them hand `session` a reference it can timestamp a take with.
//!
//! No mDNS. The leader is told not to advertise and the follower is given the
//! address directly, so running the suite does not publish a service onto whatever
//! network the machine happens to be on.

use std::time::{Duration, Instant};

use syncrec::clock::Reference;
use syncrec::tidkod::{Fps, Link};

/// Long enough for a loopback QUIC handshake and a few clock probes, short enough
/// that a genuinely broken link fails the suite rather than hanging it.
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);
const STEP: Duration = Duration::from_millis(20);

fn rig() -> (Link, Link) {
    let format = Fps::F25.format(false).expect("25 fps");
    let leader = Link::leader("syncrec-test", 0, format, false).expect("leader");

    let bound = leader.address().expect("leader is listening");
    let fingerprint = leader.fingerprint().expect("leader has a certificate").to_string();
    // The leader binds the IPv4 wildcard so it can serve Ethernet and Wi-Fi at
    // once; a follower has to be pointed at a real address.
    let address = format!("127.0.0.1:{}", bound.port()).parse().expect("loopback");

    let follower = Link::follower(address, Some(&fingerprint), format).expect("follower");
    (leader, follower)
}

/// Spin until `f` holds, or fail with what was actually seen.
fn until(what: &str, link: &mut Link, mut f: impl FnMut(&mut Link) -> bool) {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    while Instant::now() < deadline {
        link.poll_events();
        if f(link) {
            return;
        }
        std::thread::sleep(STEP);
    }
    panic!(
        "timed out waiting for {what}: status {:?}, last event {:?}",
        link.status(),
        link.message()
    );
}

#[test]
fn a_follower_locks_to_a_leader_and_agrees_with_it_about_utc() {
    let (mut leader, mut follower) = rig();

    // Roll first: the transport has to be running for either end to have a time
    // of day to agree about, which is the same reason the record button rolls the
    // leader before it opens the stream.
    leader.roll().expect("rolling the leader");
    until("the follower to synchronise", &mut follower, |l| {
        l.status().synced
    });

    let mut here = leader.reference().expect("leader reference");
    let mut there = follower.reference().expect("follower reference");
    here.refresh();
    there.refresh();

    // Both references have to be asked about the *same* instant. Asking each about
    // "now" would measure how long the first call took, not how well they agree.
    let at = Instant::now();
    let ours = here.utc_nanos(at).expect("leader resolves an instant");
    let theirs = there.utc_nanos(at).expect("follower resolves an instant");

    let delta_ms = (theirs - ours) as f64 / 1.0e6;
    eprintln!("leader/follower agreement: {delta_ms:+.4} ms");
    assert!(
        delta_ms.abs() < 5.0,
        "leader and follower disagree by {delta_ms:.3} ms on the same instant"
    );

    // And the shared answer has to be a real time of day, not an offset from some
    // arbitrary origin: the whole reason for anchoring to the wall clock is that
    // `bext` needs a date and a time, and a free-running counter has neither.
    let now = syncrec::clock::unix_nanos(std::time::SystemTime::now());
    let from_os_ms = (ours - now) as f64 / 1.0e6;
    assert!(
        from_os_ms.abs() < 1_000.0,
        "timecode is {from_os_ms:.1} ms from the wall clock; it is supposed to be \
         the time of day"
    );
}

#[test]
fn the_leaders_transport_is_the_followers_record_button() {
    let (mut leader, mut follower) = rig();

    until("the follower to connect", &mut follower, |l| {
        l.rolling().is_some()
    });
    assert_eq!(
        follower.rolling(),
        Some(false),
        "a leader that has not rolled must not have the rig recording"
    );

    leader.roll().expect("rolling");
    until("the follower to see the roll", &mut follower, |l| {
        l.rolling() == Some(true)
    });

    leader.halt().expect("halting");
    until("the follower to see the stop", &mut follower, |l| {
        l.rolling() == Some(false)
    });
}

#[test]
fn every_roll_is_a_new_session_that_both_ends_stamp() {
    let (mut leader, mut follower) = rig();

    let session_of_a_roll = |leader: &mut Link, follower: &mut Link| {
        leader.roll().expect("rolling");
        // Synced, not just rolling: a follower's take anchors, and so learns its
        // session, on the first reading it can actually timestamp against.
        until("the follower to synchronise on the roll", follower, |l| {
            l.rolling() == Some(true) && l.status().synced
        });
        let mut here = leader.reference().expect("leader reference");
        let mut there = follower.reference().expect("follower reference");
        here.refresh();
        there.refresh();
        let ours = here.session_id().expect("the leader stamps its session");
        let theirs = there.session_id().expect("the follower stamps the leader's session");
        assert_eq!(ours, theirs, "both ends of one roll must share a session");
        leader.halt().expect("halting");
        until("the follower to see the stop", follower, |l| {
            l.rolling() == Some(false)
        });
        ours
    };

    let first = session_of_a_roll(&mut leader, &mut follower);
    let second = session_of_a_roll(&mut leader, &mut follower);
    assert_ne!(first, second, "each recording must get its own session");
}

#[test]
fn a_paused_timeline_stamps_nothing_at_either_end() {
    // The tail-of-take case. A follower notices the leader stop up to one tick
    // late, and its writer thread then drains the ring and resolves whatever marks
    // are still in it. A paused timeline answers every instant with the position
    // it froze at, so a mark resolved here would be stamped with the stop time
    // rather than its own — an outlier at the very end of the drift fit's lever
    // arm. Both ends must decline instead.
    let (mut leader, mut follower) = rig();

    leader.roll().expect("rolling");
    until("the follower to synchronise", &mut follower, |l| {
        l.status().synced
    });

    let mut here = leader.reference().expect("leader reference");
    let mut there = follower.reference().expect("follower reference");
    here.refresh();
    there.refresh();
    assert!(here.utc_nanos(Instant::now()).is_some());
    assert!(there.utc_nanos(Instant::now()).is_some());

    leader.halt().expect("halting");
    until("the follower to see the stop", &mut follower, |l| {
        l.rolling() == Some(false)
    });

    // A reference is a frozen copy until it is refreshed, which is the whole
    // point: a writer pass resolves every mark against one consistent state. So
    // the pause is not visible here until we ask for it — and the writer thread
    // asks at the top of every pass, before it resolves anything.
    let at = Instant::now();
    assert!(
        here.utc_nanos(at).is_some(),
        "an unrefreshed reference must keep answering from the state it captured"
    );

    here.refresh();
    there.refresh();
    let at = Instant::now();
    assert_eq!(
        here.utc_nanos(at),
        None,
        "a paused leader must not stamp the frozen position onto a new mark"
    );
    assert_eq!(there.utc_nanos(at), None, "nor may a follower of one");
}

#[test]
fn a_follower_reports_the_exchanges_behind_its_lock() {
    // The safety gate will not resample a take destructively until the reference
    // has several independent confirmations behind it — three accepted SNTP
    // exchanges, for the clock model. Tidkod now reports the same quantity, so
    // both sources are held to one standard instead of a follower getting in on
    // whatever `Synchronized` happens to mean this week.
    let (mut leader, mut follower) = rig();
    leader.roll().expect("rolling");

    until("the follower to be gate-ready", &mut follower, |l| {
        l.status().ready()
    });

    let status = follower.status();
    let counted = status.samples.expect("a follower counts its exchanges");
    assert!(
        counted >= syncrec::finalize::MIN_CLOCK_SAMPLES,
        "ready on only {counted} exchanges"
    );

    // And a leader still counts nothing, because there is nobody to exchange
    // with. Reporting the zero it publishes would lock it out of the gate forever.
    assert_eq!(leader.status().samples, None);
    assert!(leader.status().ready());
}

#[test]
fn a_follower_with_nobody_to_follow_refuses_to_timestamp_anything() {
    // The failure that matters most: an unlocked follower must park its marks, not
    // stamp them with whatever its own fallback timeline says. A take that came
    // out plausibly wrong would be worse than one that came out visibly unsynced.
    let format = Fps::F25.format(false).expect("25 fps");
    // Nothing is listening here, so the follower can connect to nothing.
    let follower = Link::follower("127.0.0.1:1".parse().unwrap(), None, format)
        .expect("a follower may be created before its leader exists");

    let mut reference = follower.reference().expect("reference");
    reference.refresh();
    assert_eq!(reference.utc_nanos(Instant::now()), None);

    let status = reference.status();
    assert!(!status.synced);
    assert!(!status.ready(), "an unlocked follower must never read as ready");
}

#[test]
fn a_leader_is_ready_the_moment_it_exists() {
    // A leader has nobody to exchange with and nothing to converge on, so unlike
    // an NTP clock it must not make the operator wait before arming.
    let format = Fps::F25.format(false).expect("25 fps");
    let mut leader = Link::leader("syncrec-test", 0, format, false).expect("leader");

    let status = leader.status();
    assert!(status.ready(), "{status:?}");
    assert!(
        status.samples.is_none(),
        "a leader counts no exchanges, and must not pretend to"
    );

    // Rolling anchors it to the wall clock, and only then is there a timecode.
    leader.roll().expect("rolling");
    std::thread::sleep(Duration::from_millis(50));
    let label = leader.label().expect("a rolling leader has a timecode");
    assert_ne!(label, "00:00:00:00", "the timecode should be the time of day");
}
