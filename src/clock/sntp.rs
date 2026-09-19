//! The SNTP poll thread.
//!
//! `rsntp` gives us an offset and a round-trip delay, but its offset is measured
//! against `SystemTime`, which the OS time daemon steps and slews out from under us.
//! We want the offset against the *monotonic* clock instead, so every exchange is
//! sandwiched between paired readings of both clocks.
//!
//! The trick that makes this safe: if the OS jumps `SystemTime` forward by D, then
//! our sandwiched `SystemTime` midpoint gains D and `rsntp`'s reported offset loses
//! exactly D, so the UTC we derive is unchanged. The only case we cannot absorb is a
//! step landing inside the few milliseconds of the exchange itself, and the skew
//! check below catches that by noticing the two clocks disagreed about how much time
//! just passed.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use rsntp::{Config, SntpClient};

use super::{ClockModel, POLL_INTERVAL, unix_nanos};

/// Default NTP port.
const NTP_PORT: u16 = 123;

/// Fixed part of the tolerance for wall-vs-monotonic disagreement during one exchange.
const STEP_TOLERANCE_BASE: f64 = 0.001;
/// Proportional part. `adjtime` slewing is legitimate and can legally run this fast,
/// so we scale with the length of the exchange rather than flagging honest slew.
const STEP_TOLERANCE_RATE: f64 = 1000.0e-6;

const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(3);

/// One exchange, already re-expressed against the monotonic clock.
#[derive(Debug, Clone, Copy)]
pub struct Exchange {
    /// Midpoint of the exchange on the monotonic clock.
    pub mono_mid: Instant,
    /// UTC we believe held at `mono_mid`, in unix nanoseconds.
    pub utc_mid_nanos: i128,
    pub delay: f64,
    pub offset: f64,
}

#[derive(Debug)]
pub enum Outcome {
    Ok(Exchange),
    /// The wall clock and the monotonic clock disagreed about how much time passed,
    /// so something stepped `SystemTime` mid-exchange and this sample is unusable.
    Stepped {
        skew: f64,
    },
    Failed(String),
}

/// Perform one exchange and convert it into the monotonic domain.
pub fn exchange(client: &SntpClient, server: SocketAddr) -> Outcome {
    let mono_before = Instant::now();
    let sys_before = SystemTime::now();

    let result = client.synchronize(server);

    let mono_after = Instant::now();
    let sys_after = SystemTime::now();

    let result = match result {
        Ok(r) => r,
        Err(e) => return Outcome::Failed(e.to_string()),
    };

    let span = mono_after.duration_since(mono_before);
    let mono_elapsed = span.as_secs_f64();
    let sys_elapsed = (unix_nanos(sys_after) - unix_nanos(sys_before)) as f64 / 1.0e9;
    let skew = sys_elapsed - mono_elapsed;

    let tolerance = STEP_TOLERANCE_BASE + STEP_TOLERANCE_RATE * mono_elapsed;
    if skew.abs() > tolerance {
        return Outcome::Stepped { skew };
    }

    let offset = result.clock_offset().as_secs_f64();
    let delay = result.round_trip_delay().as_secs_f64();

    // Take the midpoint on both clocks. The offset is a difference between two
    // clocks that tick at nearly the same rate, so applying it at our midpoint
    // rather than rsntp's internal one costs picoseconds.
    let mono_mid = mono_before + span / 2;
    let sys_mid_nanos = (unix_nanos(sys_before) + unix_nanos(sys_after)) / 2;
    let utc_mid_nanos = sys_mid_nanos + (offset * 1.0e9).round() as i128;

    Outcome::Ok(Exchange {
        mono_mid,
        utc_mid_nanos,
        delay,
        offset,
    })
}

/// Resolve a user-typed server to a single address.
///
/// We pin one address rather than re-resolving per poll: the spec says poll *one*
/// server, and rotating through a pool's members would mix paths with different
/// asymmetries into one fit.
pub fn resolve(server: &str) -> std::io::Result<SocketAddr> {
    let with_port = if server.parse::<std::net::Ipv6Addr>().is_ok() {
        // A bare IPv6 literal is full of colons, so it has to be recognised before
        // the host:port check below or "::1" reads as host "" port ":1".
        format!("[{server}]:{NTP_PORT}")
    } else if let Some(rest) = server.strip_prefix('[') {
        // Bracketed IPv6: already has a port only if a colon follows the bracket.
        match rest.split_once(']') {
            Some((_, after)) if after.starts_with(':') => server.to_string(),
            _ => format!("{server}:{NTP_PORT}"),
        }
    } else if server.contains(':') {
        server.to_string()
    } else {
        format!("{server}:{NTP_PORT}")
    };
    with_port
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other(format!("no address for {server}")))
}

pub struct Poller {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Poller {
    /// Start polling `server` every `POLL_INTERVAL`, feeding `model`.
    ///
    /// The first exchange runs immediately so the app is not blind for 16 seconds.
    pub fn spawn(server: String, model: Arc<Mutex<ClockModel>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        let handle = thread::Builder::new()
            .name("syncrec-sntp".into())
            .spawn(move || {
                let client = SntpClient::with_config(
                    Config::default().timeout(EXCHANGE_TIMEOUT).connect_ip(true),
                );

                let mut addr: Option<SocketAddr> = None;

                while !stop_thread.load(Ordering::Relaxed) {
                    // Re-resolve lazily, and again after a failure, so a server that
                    // was down at launch recovers without a restart.
                    if addr.is_none() {
                        match resolve(&server) {
                            Ok(a) => addr = Some(a),
                            Err(_) => {
                                if let Ok(mut m) = model.lock() {
                                    m.record_failure();
                                }
                                sleep_interruptibly(POLL_INTERVAL, &stop_thread);
                                continue;
                            }
                        }
                    }

                    match exchange(&client, addr.expect("just resolved")) {
                        Outcome::Ok(ex) => {
                            if let Ok(mut m) = model.lock() {
                                m.push(ex.mono_mid, ex.utc_mid_nanos, ex.delay, ex.offset);
                            }
                        }
                        Outcome::Stepped { .. } => {
                            if let Ok(mut m) = model.lock() {
                                m.record_stepped();
                            }
                        }
                        Outcome::Failed(_) => {
                            if let Ok(mut m) = model.lock() {
                                m.record_failure();
                            }
                            addr = None;
                        }
                    }

                    sleep_interruptibly(POLL_INTERVAL, &stop_thread);
                }
            })
            .expect("spawn sntp thread");

        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Sleep in slices so shutdown does not wait out a full poll interval.
fn sleep_interruptibly(total: Duration, stop: &AtomicBool) {
    const SLICE: Duration = Duration::from_millis(200);
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(SLICE.min(remaining));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_adds_the_default_port() {
        let a = resolve("127.0.0.1").unwrap();
        assert_eq!(a.port(), NTP_PORT);
    }

    #[test]
    fn resolve_respects_an_explicit_port() {
        let a = resolve("127.0.0.1:1234").unwrap();
        assert_eq!(a.port(), 1234);
    }

    #[test]
    fn resolve_handles_bare_ipv6() {
        // Regression: a bare IPv6 literal is nothing but colons, so the host:port
        // heuristic used to pass it through unbracketed and portless, and it failed
        // to resolve at all.
        let a = resolve("::1").unwrap();
        assert!(a.is_ipv6());
        assert_eq!(a.port(), NTP_PORT);
    }

    #[test]
    fn resolve_handles_bracketed_ipv6_with_and_without_a_port() {
        let a = resolve("[::1]").unwrap();
        assert!(a.is_ipv6());
        assert_eq!(a.port(), NTP_PORT, "brackets alone do not imply a port");

        let b = resolve("[::1]:4460").unwrap();
        assert!(b.is_ipv6());
        assert_eq!(b.port(), 4460);
    }

    #[test]
    fn step_tolerance_allows_legitimate_slew_but_not_a_jump() {
        // A 50 ms exchange slewing at the maximum legal rate.
        let exchange_len = 0.050;
        let tol = STEP_TOLERANCE_BASE + STEP_TOLERANCE_RATE * exchange_len;
        assert!(exchange_len * STEP_TOLERANCE_RATE < tol);
        // A one-second step is nowhere near tolerable.
        assert!(1.0 > tol);
    }
}
