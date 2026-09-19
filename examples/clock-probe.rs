//! Headless verification of the clock model.
//!
//! Usage: clock-probe [server] [rounds]
//!
//! Polls one server and prints each exchange plus the running fit, so the clock can
//! be sanity-checked (and compared against `sntp -d` / `w32tm /stripchart`) before
//! any audio code exists.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use syncrec::clock::sntp::{self, Outcome};
use syncrec::clock::{ClockModel, POLL_INTERVAL};

fn main() {
    let mut args = std::env::args().skip(1);
    let server = args.next().unwrap_or_else(|| "pool.ntp.org".into());
    let rounds: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(12);

    let addr = match sntp::resolve(&server) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("cannot resolve {server}: {e}");
            std::process::exit(1);
        }
    };
    println!("server   {server} -> {addr}");
    println!("interval {}s, {rounds} rounds\n", POLL_INTERVAL.as_secs());

    let base = Instant::now();
    let model = Arc::new(Mutex::new(ClockModel::new(base)));
    let client = rsntp::SntpClient::with_config(
        rsntp::Config::default()
            .timeout(Duration::from_secs(3))
            .connect_ip(true),
    );

    println!(
        "{:>3}  {:>10}  {:>9}  {:>7}  {:>10}  {:>9}  {:>9}",
        "#", "offset_ms", "delay_ms", "used", "ppm", "resid_us", "disp_ms"
    );

    for i in 0..rounds {
        if i > 0 {
            std::thread::sleep(POLL_INTERVAL);
        }

        match sntp::exchange(&client, addr) {
            Outcome::Ok(ex) => {
                let mut m = model.lock().unwrap();
                m.push(ex.mono_mid, ex.utc_mid_nanos, ex.delay, ex.offset);
                let snap = m.snapshot();
                let fit = snap.fit().expect("just pushed");
                println!(
                    "{:>3}  {:>10.3}  {:>9.3}  {:>3}/{:<3}  {:>10.2}  {:>9.1}  {:>9.3}",
                    i,
                    ex.offset * 1e3,
                    ex.delay * 1e3,
                    fit.used,
                    fit.total,
                    if fit.slope_trusted {
                        fit.ppm()
                    } else {
                        f64::NAN
                    },
                    fit.residual_rms * 1e6,
                    snap.dispersion().unwrap_or(f64::NAN) * 1e3,
                );
            }
            Outcome::Stepped { skew } => {
                model.lock().unwrap().record_stepped();
                println!(
                    "{i:>3}  rejected: system clock stepped {:.3} ms mid-exchange",
                    skew * 1e3
                );
            }
            Outcome::Failed(e) => {
                model.lock().unwrap().record_failure();
                println!("{i:>3}  failed: {e}");
            }
        }
    }

    let m = model.lock().unwrap();
    let snap = m.snapshot();
    println!("\n--- final ---");
    println!("state      {}", snap.state.label());
    println!(
        "accepted   {}  rejected(step) {}  failed {}",
        snap.accepted, snap.rejected, snap.failed
    );
    if let Some(fit) = snap.fit() {
        if fit.slope_trusted {
            println!(
                "slope      {:+.3} ppm  (machine frequency error)",
                fit.ppm()
            );
        } else {
            println!(
                "slope      not trusted ({} samples over {:.0}s) -- offset-only model",
                fit.used, fit.span
            );
        }
        println!(
            "residual   {:.1} us rms over {} of {} samples",
            fit.residual_rms * 1e6,
            fit.used,
            fit.total
        );
        println!("min delay  {:.3} ms", fit.min_delay * 1e3);
        println!("dispersion {:.3} ms", snap.dispersion().unwrap() * 1e3);
    }
    if let Some(err) = snap.system_clock_error() {
        println!("OS clock   {:+.3} ms off true UTC", err * 1e3);
    }
}
