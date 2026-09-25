# syncrec

An audio recorder that does not trust the operating system clock.

Every other recorder stamps its files from the system wall clock, which is stepped
and slewed by the machine's own time daemon and is routinely wrong by tens of
milliseconds in ways nothing on the machine can see. syncrec runs its own SNTP
client in-process, fits a linear model of the *monotonic* clock against it, anchors
once at the first captured sample, and then counts samples. It also measures what
sample rate the audio device is really running at over the take — 47999.4 Hz, not
the 48000 it claims — and resamples to exactly 48 kHz so the sample count is itself
a valid time base.

Output is Broadcast Wave with a correct `bext` chunk, plus a JSON sidecar carrying
the full audit trail.

Target accuracy: sub-millisecond over Ethernet, 5–20 ms over WiFi (bounded by
network path asymmetry, which no amount of software can observe).

## Two ways to know what time it is

Settings offers a choice of time source, and it is a choice between two different
questions.

**NTP** answers *what time is it really?* Every recorder answers it separately, so
two machines agree only as well as their two independent network paths allow. It
is the right answer for a lone recorder, and for anything that has to line up with
the rest of the world rather than with the machine next to it.

**Tidkod** answers *what time does the leader think it is?* — LAN timecode from
[tidkod](https://crates.io/crates/tidkod). That is a worse question to ask of
the universe and a much better one to ask of a rig: every follower is wrong by the
same amount, so the takes line up with each other *exactly*, which is the property
a multi-recorder shoot actually needs. On loopback the two ends agree to under
0.15 ms; see `tests/tidkod_link.rs`, which measures it.

Everything downstream of the capture path is indifferent to which one is running.
Both answer the same single question — *what UTC was this `Instant`?* — so the
drift log, the drift fit, the safety gate and the file writer never learn that the
clock changed underneath them.

## Tidkod: leader and follower

The role switch lives in the top right of the recording window, not four clicks
deep in settings, because a machine gets promoted to leader on the day — usually
because another one just died.

**Leader.** This machine drives the rig. Pressing record anchors the timeline to
the local time of day and starts it; pressing stop pauses it. The transport *is*
the record state, so there is no second channel to get out of step with the audio.
It advertises over mDNS, so followers find it without being told an address.

**Follower.** This machine's record button is a lamp, not a control. It watches
the leader's transport: when the leader rolls, it opens a new take; when the leader
stops, that take finalises exactly as if the button had been pressed here. Offering
a local override would only ever produce a take that does not line up with the
others. Leave the leader address empty and it takes the first leader the LAN
offers, pinning the certificate fingerprint from the same mDNS record.

Timecode is time of day, the way production sound has always done it. That is what
lets `bext.TimeReference`, `OriginationDate`, `OriginationTime` and `t0` all stay
honest: the timeline resolves to a real instant on the calendar rather than to an
offset from an arbitrary origin. A take crossing midnight is handled by picking the
whole day nearest to where the monotonic clock says we should be, so the label
wrapping at 24 hours does not drag the file back to yesterday morning.

The frame rate only ever affects the *label*. Drop-frame renumbering included:
position over frame rate is the real offset into the day in both numbering schemes,
so a 29.97 DF rig and a 25 fps rig stamp the same instant identically.

The one honesty cost is worth stating plainly. A leader has no NTP, so its idea of
the time of day comes from its own OS clock — the very clock this program exists
not to trust. It is used for one thing only, and to hours of precision: deciding
which second of which day the rig is anchored to. From that anchor on, every
follower is locked to the leader's *monotonic* clock, not to anybody's wall clock.
Absolute accuracy is the leader's OS clock; relative accuracy across the rig is
tidkod's, which is far better. Pick NTP if it is the first number you care about.

## Build and run

```sh
cargo run                 # dev, but see the macOS note below
cargo test                # unit + on-disk BWF round-trip tests
cargo clippy --all-targets
```

### macOS

The app **must** run from a bundle. macOS denies microphone access to a bare
binary with no `NSMicrophoneUsageDescription`, often silently.

```sh
cargo bundle              # produces target/debug/bundle/osx/syncrec.app
open target/debug/bundle/osx/syncrec.app
```

`bundle/Info.ext.plist` supplies the microphone usage string. It is a **bare
fragment** of key/value pairs, not a complete plist — cargo-bundle splices it
verbatim into the middle of the `<dict>` it generates, so wrapping it in
`<plist><dict>` produces an Info.plist macOS cannot parse.

### Windows

Windows has no permission-request API for unpackaged desktop apps; the Settings
toggle is the only gate, so the app detects denial at stream-open time and offers a
deep link to `ms-settings:privacy-microphone`.

There is no Windows machine in this project's loop, so the Windows paths are
type-checked rather than run:

```sh
rustup target add x86_64-pc-windows-msvc
cargo check --lib --target x86_64-pc-windows-msvc
```

## Verifying the clock without any audio

```sh
cargo run --example clock-probe -- time.apple.com 14
```

Polls one server every 16 s and prints each exchange plus the running fit. A real
run on WiFi looks like this — note the slope is withheld until the window can
actually support one:

```
  #   offset_ms   delay_ms     used         ppm   resid_us    disp_ms
  0       4.065      8.846    1/1           NaN        0.0      4.423
  4       4.621     12.369    5/5          0.98      762.7      5.055
 13       4.273      9.957    8/8        -16.28      726.6      4.928

slope      -16.280 ppm  (machine frequency error)
dispersion 4.928 ms
OS clock   +3.550 ms off true UTC
```

Cross-check the offset against `sntp -d <server>` on macOS or
`w32tm /stripchart` on Windows.

## Verifying the whole capture chain, headless

```sh
cargo run --example record-probe -- 25 time.apple.com
```

Runs SNTP → clock model → device → ring buffer → writer thread → WAV + sidecar and
reports the device's measured rate. NTP only; the tidkod path is covered by
`cargo test --test tidkod_link`, which stands a leader and a follower up on
loopback and checks that they agree about UTC and that the transport carries.

## What a take produces

A take that corrects cleanly leaves **one file**:

```
rec-1.wav        the Broadcast Wave file
```

`t0` to the nanosecond, the measured sample rate, the drift ratio, the clock
source, the sync state and the dispersion all travel inside it, in `bext`
`CodingHistory` and in iXML. A take stamped from timecode also carries its frame
rate, its drop-frame flag and its start timecode, which `bext.TimeReference` cannot
express because it counts samples. Nothing else is needed to interpret the take.

A take that could *not* be corrected keeps its evidence instead:

```
rec-1.wav                          written uncorrected, labelled with the device's
                                   real rate rather than 48000
rec-1.json                         every drift observation, the fit, the NTP log
.syncrec-scratch/rec-1.raw.wav     the untouched 32-bit float capture
```

So a clean folder means every take is corrected, and anything left behind is
something to look at. The scratch capture is float so the audio is quantised to 24
bits exactly once, after resampling, rather than twice.

## The safety gate

The corrected file replaces the original, so the correction has to be trustworthy
before the raw capture is deleted. All of these must hold:

- the time reference reported itself synced — `Synced` for the clock model,
  `Synchronized` for an tidkod follower
- at least 3 accepted clock exchanges, *when the reference counts exchanges at
  all*. An tidkod follower counts its own and is held to exactly the same
  three, so neither source gets in on a single lucky sample. An tidkod leader
  generates the timeline it is being judged against and has nobody to exchange
  with, so for a leader the criterion is skipped rather than satisfied with a
  made-up number. Being skipped is not a way past the gate: an unsynced reference
  still fails the condition above.
- at least 30 drift observations
- spanning at least 20 seconds
- `|drift|` under 200 ppm measured against the device's own nominal rate
- fit residual RMS under 5 ms

If any fails, the take is still written — but from unresampled audio, labelled with
the device's *actual* rate rather than 48000, and the scratch capture is kept. A
44.1 kHz file honestly labelled 44100 is recoverable; one labelled 48000 is a trap.
The UI shows which conditions failed.

This is why the record button reads **Syncing…** in muted red until the clock is
ready, and **Record** in bright red once a take started now could be corrected.
Recording is never actually blocked — missing the moment is worse than an
uncorrected take.

**The timestamp is not gated.** `t0`, `TimeReference`, `OriginationDate`/`Time` and
the measured rate are written on every take regardless of the outcome; the gate
decides only whether the audio is *resampled*. A take that fails the gate is still
correctly timestamped, and still records what rate the device was measured at — it
simply declines to apply a number it could not measure well enough to trust.

Short takes routinely fail the gate, and that is the right answer rather than a
fault: over 8 seconds the slope standard error is around 44 ppm, far noisier than
the ~30 ppm being corrected, while the total drift is only about 0.24 ms. There is
nothing worth correcting and no way to measure it if there were.

## Layout

```
src/clock/       the clock model: SNTP polling, the 8-sample fit, the audio-clock bridge
src/clock/reference.rs  the interface both time sources answer to
src/tidkod.rs LAN timecode: the engine, time-of-day conversion, leader and follower
src/settings.rs  the settings page's state, and where it lives between launches
src/audio/       device negotiation, the realtime capture callback, metering, the writer thread
src/bwf.rs       bext / iXML construction and the TimeReference maths
src/finalize.rs  drift fitting, resampling, the safety gate
src/latency.rs   platform input-latency correction
src/permission.rs microphone permission
src/ui/          the iced front end
```

Tidkod comes from crates.io:

```toml
tidkod = "0.1.1"
```
