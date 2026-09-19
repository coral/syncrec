//! Turning "when the callback fired" into "when the sound hit the microphone".
//!
//! syncrec timestamps the *first* captured sample and then counts samples, so the
//! whole file's accuracy rests on that one number. The device hands us a buffer at
//! some instant T, but the air pressure that became those samples arrived earlier:
//! preamp and converter group delay, the driver's ring buffer, and the safety margin
//! the host insists on before it will read the buffer at all. On a USB interface that
//! total is routinely 5-15 ms. Our target is sub-millisecond, so not subtracting it
//! misses the budget by an order of magnitude all on its own.
//!
//! The awkward part is that cpal has *already* done some of this work, and how much
//! depends entirely on the backend. This module therefore does not report "the
//! device's latency"; it reports only the **additional** correction still owed on
//! top of `InputCallbackInfo::timestamp().capture`. Getting that distinction wrong in
//! either direction is a real error: subtract nothing and we are late, subtract twice
//! and we are early by the same amount.
//!
//! Per-backend state of play, verified against the cpal 0.18.2 sources:
//!
//! * **macOS / CoreAudio** — already corrected, so we owe nothing. See the comment on
//!   the macOS `platform` module below; it matters enough to spell out there.
//! * **Windows / WASAPI** — not corrected at all. `input_timestamp()` in
//!   `host/wasapi/stream.rs` converts the raw QPC position out of
//!   `IAudioCaptureClient::GetBuffer` and stops. The `stream_latency` the stream
//!   holds (from `IAudioClient::GetStreamLatency`) is consumed only by
//!   `output_timestamp`. So on Windows we have to go ask WASAPI ourselves.
//! * **Everything else** — we have not measured it, so we claim nothing and say so.
//!
//! A latency query must never be able to stop a recording. Every failure path here
//! degrades to zero and records that the number was *assumed* rather than measured,
//! which is what [`LatencySource`] is for: a take whose timestamps are uncorrected
//! should say so in its sidecar rather than quietly look like a corrected one.

use std::time::Duration;

/// Bound on the operator's manual trim, in milliseconds.
///
/// Not a physical limit — it is a typo guard. Real rigs land inside a few tens of
/// milliseconds; a stray keystroke that turns `5` into `5000000` should not be able
/// to produce a timestamp that is wrong by a lifetime.
pub const MAX_TRIM_MS: f64 = 10_000.0;

/// Where a latency figure came from, so a reader can tell measurement from guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencySource {
    /// The backend corrected the capture timestamp itself and left us nothing to do.
    /// The accompanying duration is zero, and that zero is *right*, not a fallback.
    AlreadyCorrected,
    /// We asked the OS and it answered.
    Measured,
    /// We could not find out. The duration is zero because zero is the only
    /// defensible default, but the timestamp is uncorrected and should be labelled so.
    Unavailable,
}

impl LatencySource {
    pub fn label(self) -> &'static str {
        match self {
            LatencySource::AlreadyCorrected => "already-corrected",
            LatencySource::Measured => "measured",
            LatencySource::Unavailable => "unavailable",
        }
    }

    /// Whether the zero-or-not figure beside this tag reflects reality.
    ///
    /// `AlreadyCorrected` counts as trustworthy: it is a positive statement that the
    /// remaining correction is genuinely nil, not an admission of ignorance.
    pub fn is_trustworthy(self) -> bool {
        matches!(
            self,
            LatencySource::AlreadyCorrected | LatencySource::Measured
        )
    }
}

/// The additional input latency owed on top of cpal's `capture` timestamp.
///
/// Always non-negative: a platform cannot tell us that sound arrived *after* the
/// buffer did. Signed adjustment is the operator's job, via the manual trim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputLatency {
    /// How much further back in time the first sample actually happened.
    pub extra: Duration,
    pub source: LatencySource,
}

impl InputLatency {
    /// The backend already did the subtraction for us.
    pub const fn already_corrected() -> Self {
        Self {
            extra: Duration::ZERO,
            source: LatencySource::AlreadyCorrected,
        }
    }

    /// We asked and got an answer.
    pub const fn measured(extra: Duration) -> Self {
        Self {
            extra,
            source: LatencySource::Measured,
        }
    }

    /// We could not find out; assume nothing and say so.
    pub const fn unavailable() -> Self {
        Self {
            extra: Duration::ZERO,
            source: LatencySource::Unavailable,
        }
    }

    /// Ask the platform how much correction is still owed for an input device.
    ///
    /// `device_name` should be the name cpal reported for the device being recorded.
    /// It is used on Windows to query the right endpoint rather than whichever one
    /// happens to be the system default; pass `None` to mean "the default input".
    ///
    /// Infallible by construction. Anything that goes wrong becomes
    /// [`LatencySource::Unavailable`], because a latency query is never a good enough
    /// reason to refuse to record.
    pub fn query(device_name: Option<&str>) -> Self {
        platform::query(device_name)
    }

    pub fn millis(&self) -> f64 {
        self.extra.as_secs_f64() * 1.0e3
    }

    /// One-line summary for the UI and the sidecar log.
    pub fn describe(&self) -> String {
        match self.source {
            LatencySource::AlreadyCorrected => {
                "0.00 ms (backend already corrected the timestamp)".to_string()
            }
            LatencySource::Measured => format!("{:.2} ms (measured)", self.millis()),
            LatencySource::Unavailable => {
                "0.00 ms (not available on this platform; timestamp is uncorrected)".to_string()
            }
        }
    }
}

/// The platform figure plus whatever the operator dialled in by hand.
///
/// The manual trim exists because no API knows about the analogue half of the chain:
/// mic preamp, a long cable, an outboard converter, or simply a device whose driver
/// lies. Operators measure it with a loopback click and type in the difference, so it
/// has to be allowed to go negative — "you are subtracting too much" is just as
/// legitimate a correction as "you are subtracting too little".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencyCorrection {
    pub platform: InputLatency,
    /// Sanitised: finite, and clamped to +/-[`MAX_TRIM_MS`].
    trim_ms: f64,
}

impl LatencyCorrection {
    /// `trim_ms` comes straight off a UI text field, so it is treated as hostile:
    /// NaN and the infinities become zero, and the magnitude is clamped.
    pub fn new(platform: InputLatency, trim_ms: f64) -> Self {
        let trim_ms = if trim_ms.is_finite() {
            trim_ms.clamp(-MAX_TRIM_MS, MAX_TRIM_MS)
        } else {
            0.0
        };
        Self { platform, trim_ms }
    }

    /// Platform figure only, no operator trim.
    pub fn platform_only(platform: InputLatency) -> Self {
        Self::new(platform, 0.0)
    }

    /// The trim actually in force, after sanitising — which is not necessarily the
    /// number the operator typed, so the UI should be able to read it back.
    pub fn trim_ms(&self) -> f64 {
        self.trim_ms
    }

    /// Total correction in nanoseconds, **signed**.
    ///
    /// Positive means "the sound happened this long before the reported capture
    /// time", i.e. subtract it. Negative is meaningful and is deliberately not
    /// clamped to zero: if the operator has measured that we over-correct, refusing
    /// to let them say so would just bake in a known error. Callers must therefore
    /// treat this as a signed shift, not a `Duration`.
    pub fn total_nanos(&self) -> i64 {
        let platform = self.platform.extra.as_nanos().min(i64::MAX as u128) as i64;
        let trim = (self.trim_ms * 1.0e6).round();
        // The trim is clamped to +/-10 s, so this cast cannot lose anything.
        platform.saturating_add(trim as i64)
    }

    pub fn total_seconds(&self) -> f64 {
        self.total_nanos() as f64 / 1.0e9
    }

    pub fn total_millis(&self) -> f64 {
        self.total_nanos() as f64 / 1.0e6
    }

    /// Apply the correction to a capture timestamp expressed as unix nanoseconds.
    ///
    /// Subtraction, because the correction says how much *earlier* the sound was.
    /// Done in `i128` to match the rest of the codebase's timestamp type and so a
    /// pre-epoch or otherwise silly input cannot wrap.
    pub fn apply(&self, capture_unix_nanos: i128) -> i128 {
        capture_unix_nanos - self.total_nanos() as i128
    }

    /// Whether the platform half of this figure is a measurement rather than a shrug.
    ///
    /// The trim is always the operator's own claim, so it says nothing either way.
    pub fn platform_is_trustworthy(&self) -> bool {
        self.platform.source.is_trustworthy()
    }

    /// One line for the sidecar: both halves, kept separate on purpose so a future
    /// reader can tell which part the machine supplied and which part a human did.
    pub fn describe(&self) -> String {
        format!(
            "total {:.3} ms = platform {} + manual {:+.3} ms",
            self.total_millis(),
            self.platform.describe(),
            self.trim_ms,
        )
    }
}

#[cfg(target_os = "macos")]
mod platform {
    //! CoreAudio: deliberately a no-op. Read this before "fixing" it.
    //!
    //! cpal already subtracts input latency on this backend. In
    //! `src/host/coreaudio/macos/device.rs` the input callback computes
    //!
    //! ```text
    //! latency_frames = device_buffer_frames + kAudioDevicePropertyLatency
    //!                                       + kAudioDevicePropertySafetyOffset
    //! capture        = callback_host_time - frames_to_duration(latency_frames, rate)
    //! ```
    //!
    //! (the two device properties come from `get_device_extra_latency_frames`), so the
    //! `capture` timestamp handed to our data callback has already been walked back
    //! past the buffer, the device's reported latency and the safety offset.
    //!
    //! Querying those same properties here and subtracting them again would double the
    //! correction and put every timestamp several milliseconds *early* — the exact
    //! failure this module exists to prevent, and a silent one, because the output
    //! still looks entirely plausible. Hence zero, tagged `AlreadyCorrected` rather
    //! than `Unavailable` so the sidecar records "nothing left to do" and not "we did
    //! not look".
    //!
    //! What CoreAudio still cannot see is the analogue tail — preamp, cable, an
    //! outboard converter. That is what the operator's manual trim is for.

    use super::InputLatency;

    pub fn query(_device_name: Option<&str>) -> InputLatency {
        InputLatency::already_corrected()
    }
}

#[cfg(windows)]
mod platform {
    //! WASAPI: cpal leaves input timestamps uncorrected, so we ask the endpoint.
    //!
    //! What `IAudioClient::GetStreamLatency` actually reports in shared mode is the
    //! delay through the *audio engine* path — the engine's periodic buffering between
    //! the endpoint and our client. It is the largest single term we can get at, and
    //! it is the one cpal already applies on the render side, so applying it on the
    //! capture side restores symmetry.
    //!
    //! It is emphatically not the whole story. It excludes the driver's own buffering
    //! below the engine, the converter's group delay, and everything analogue. Treat
    //! the result as a floor on the true latency, not the true latency; the residual
    //! is what the operator's manual trim is there to absorb.
    //!
    //! `GetStreamLatency` is only valid on an initialised client, so we stand up a
    //! throwaway shared-mode client on the endpoint purely to read the number and drop
    //! it again. Shared mode does not lock the device, so this is safe to do alongside
    //! (or before) cpal's own stream.

    use super::InputLatency;
    use std::time::Duration;
    use windows::Win32::Devices::Properties::DEVPKEY_Device_FriendlyName;
    use windows::Win32::Foundation::{PROPERTYKEY, RPC_E_CHANGED_MODE, S_FALSE, S_OK};
    use windows::Win32::Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE, IAudioClient, IMMDevice,
        IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eConsole,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize, STGM_READ,
    };

    pub fn query(device_name: Option<&str>) -> InputLatency {
        // `_com` is bound first so it is dropped *last*: every COM pointer created
        // inside `measure` must be released before the apartment is torn down.
        let _com = ComApartment::enter();
        match unsafe { measure(device_name) } {
            Ok(extra) => InputLatency::measured(extra),
            // Deliberately swallowed. A device that is busy, unplugged mid-query, or
            // blocked by the microphone privacy setting must not stop the recording;
            // it just means the timestamp goes out labelled uncorrected.
            Err(_) => InputLatency::unavailable(),
        }
    }

    /// Ensures COM is usable on this thread, and balances the call if — and only if —
    /// it was ours to balance.
    struct ComApartment {
        must_uninitialise: bool,
    }

    impl ComApartment {
        fn enter() -> Self {
            // SAFETY: plain COM initialisation; no reserved parameter is passed.
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            // Three outcomes matter, and only one of them is a real failure:
            //   S_OK               we initialised the apartment; we must balance it.
            //   S_FALSE            already initialised on this thread in a compatible
            //                      mode. Still counts as a successful call and the
            //                      reference must still be balanced.
            //   RPC_E_CHANGED_MODE the thread is already an STA — which is exactly what
            //                      a winit/iced UI thread looks like. COM is perfectly
            //                      usable; we simply did not take a reference, so we
            //                      must NOT call CoUninitialize.
            let must_uninitialise = hr == S_OK || hr == S_FALSE;
            debug_assert!(
                must_uninitialise || hr == RPC_E_CHANGED_MODE,
                "unexpected CoInitializeEx result"
            );
            Self { must_uninitialise }
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            if self.must_uninitialise {
                // SAFETY: balances exactly one successful CoInitializeEx on this thread.
                unsafe { CoUninitialize() };
            }
        }
    }

    /// # Safety
    ///
    /// Calls COM. The caller must have an initialised apartment on this thread, and
    /// must not let the returned value outlive it (it does not: we return a plain
    /// `Duration`).
    unsafe fn measure(device_name: Option<&str>) -> windows::core::Result<Duration> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;

            let device = match device_name {
                Some(name) => find_capture_endpoint(&enumerator, name)?,
                None => enumerator.GetDefaultAudioEndpoint(eCapture, eConsole)?,
            };

            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;

            // Both periods are REFERENCE_TIMEs: 100-nanosecond units, not nanoseconds.
            // The default period is our fallback if GetStreamLatency reports nothing
            // useful, because in shared mode the engine period *is* the dominant term.
            let mut default_period: i64 = 0;
            let mut minimum_period: i64 = 0;
            client.GetDevicePeriod(Some(&mut default_period), Some(&mut minimum_period))?;

            // GetStreamLatency returns AUDCLNT_E_NOT_INITIALIZED on a bare client, so
            // initialise a shared-mode one in the engine's own mix format. Zero
            // buffer duration and zero periodicity mean "use the engine defaults",
            // which is what we want: we are measuring the default path, not a custom
            // one. No stream is ever started.
            let mix_format = client.GetMixFormat()?;
            let initialised =
                client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 0, 0, mix_format, None);
            // GetMixFormat allocates with CoTaskMemAlloc and hands us ownership, so
            // free it whether or not Initialize succeeded.
            CoTaskMemFree(Some(mix_format as *const core::ffi::c_void));
            initialised?;

            let latency_100ns = client.GetStreamLatency()?;
            let ticks = if latency_100ns > 0 {
                latency_100ns
            } else {
                default_period
            };
            Ok(reference_time_to_duration(ticks))
        }
    }

    /// REFERENCE_TIME is a signed count of 100-nanosecond ticks.
    ///
    /// Negative is nonsense here and is floored to zero rather than wrapped: a
    /// negative latency would mean the sound arrived after the buffer.
    fn reference_time_to_duration(ticks: i64) -> Duration {
        Duration::from_nanos((ticks.max(0) as u64).saturating_mul(100))
    }

    /// Find the capture endpoint cpal is calling `name`.
    ///
    /// cpal names WASAPI devices from `DEVPKEY_Device_FriendlyName`, so matching on
    /// that string lines us up with the device actually being recorded. If no endpoint
    /// matches we return the error rather than silently measuring the system default:
    /// a confident number for the wrong device is worse than an honest "unknown".
    ///
    /// # Safety
    ///
    /// Calls COM; requires an initialised apartment.
    unsafe fn find_capture_endpoint(
        enumerator: &IMMDeviceEnumerator,
        name: &str,
    ) -> windows::core::Result<IMMDevice> {
        unsafe {
            let collection = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
            let count = collection.GetCount()?;
            for index in 0..count {
                let device = collection.Item(index)?;
                if endpoint_friendly_name(&device).as_deref() == Some(name) {
                    return Ok(device);
                }
            }
            Err(windows::core::Error::new(
                windows::Win32::Foundation::E_INVALIDARG,
                "no active capture endpoint matched the requested device name",
            ))
        }
    }

    /// # Safety
    ///
    /// Calls COM; requires an initialised apartment.
    unsafe fn endpoint_friendly_name(device: &IMMDevice) -> Option<String> {
        unsafe {
            let store = device.OpenPropertyStore(STGM_READ).ok()?;
            // DEVPROPKEY and PROPERTYKEY are the same {GUID, u32} shape; the property
            // store API is typed in terms of the latter. cpal does the same cast.
            let key = &DEVPKEY_Device_FriendlyName as *const _ as *const PROPERTYKEY;
            let value = store.GetValue(key).ok()?;
            let name = value.to_string();
            if name.is_empty() { None } else { Some(name) }
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    //! Nothing measured, nothing claimed.
    //!
    //! ALSA and PulseAudio can both report a delay, but syncrec has no tested Linux
    //! path and an untested correction is worse than a labelled absence: a wrong
    //! subtraction is invisible in the output, whereas `Unavailable` at least tells
    //! the operator to reach for the manual trim.

    use super::InputLatency;

    pub fn query(_device_name: Option<&str>) -> InputLatency {
        InputLatency::unavailable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Platform figure alone, no trim, is passed through unchanged.
    #[test]
    fn platform_latency_with_no_trim_is_the_platform_figure() {
        let c =
            LatencyCorrection::platform_only(InputLatency::measured(Duration::from_micros(7_500)));
        assert_eq!(c.total_nanos(), 7_500_000);
        assert!((c.total_millis() - 7.5).abs() < 1e-9);
    }

    /// A positive trim adds to the platform figure; both mean "earlier".
    #[test]
    fn positive_trim_adds_to_the_platform_figure() {
        let c = LatencyCorrection::new(InputLatency::measured(Duration::from_millis(10)), 2.5);
        assert_eq!(c.total_nanos(), 12_500_000);
    }

    /// A negative trim subtracts, because over-correction is a real failure mode the
    /// operator must be able to undo.
    #[test]
    fn negative_trim_reduces_the_platform_figure() {
        let c = LatencyCorrection::new(InputLatency::measured(Duration::from_millis(10)), -4.0);
        assert_eq!(c.total_nanos(), 6_000_000);
    }

    /// A trim more negative than the platform figure yields a negative total rather
    /// than being clamped at zero: "you are subtracting too much" must be expressible.
    #[test]
    fn trim_more_negative_than_platform_latency_gives_a_negative_total() {
        let c = LatencyCorrection::new(InputLatency::measured(Duration::from_millis(3)), -8.0);
        assert_eq!(c.total_nanos(), -5_000_000);
        assert!(c.total_seconds() < 0.0);
    }

    /// A negative total moves the timestamp *later*, which is the whole point of
    /// allowing it.
    #[test]
    fn negative_total_shifts_the_timestamp_forward_in_time() {
        let c = LatencyCorrection::new(InputLatency::unavailable(), -5.0);
        let t0: i128 = 1_700_000_000_000_000_000;
        assert_eq!(c.apply(t0), t0 + 5_000_000);
    }

    /// A positive total moves the timestamp earlier, since the sound predates the buffer.
    #[test]
    fn positive_total_shifts_the_timestamp_backward_in_time() {
        let c = LatencyCorrection::new(InputLatency::measured(Duration::from_millis(12)), 0.0);
        let t0: i128 = 1_700_000_000_000_000_000;
        assert_eq!(c.apply(t0), t0 - 12_000_000);
    }

    /// Applying a zero correction is exactly the identity, not merely close to it.
    #[test]
    fn zero_correction_leaves_the_timestamp_bit_identical() {
        let c = LatencyCorrection::platform_only(InputLatency::already_corrected());
        let t0: i128 = -12_345;
        assert_eq!(c.apply(t0), t0);
        assert_eq!(c.total_nanos(), 0);
    }

    /// A NaN from a half-typed UI field must not poison the timestamp.
    #[test]
    fn non_finite_trim_is_treated_as_zero() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let c = LatencyCorrection::new(InputLatency::measured(Duration::from_millis(5)), bad);
            assert_eq!(c.trim_ms(), 0.0, "trim {bad} should sanitise to zero");
            assert_eq!(c.total_nanos(), 5_000_000);
        }
    }

    /// An absurd trim is clamped rather than accepted, in both directions.
    #[test]
    fn trim_is_clamped_to_the_typo_guard_in_both_directions() {
        let high = LatencyCorrection::new(InputLatency::unavailable(), 5.0e9);
        assert_eq!(high.trim_ms(), MAX_TRIM_MS);
        let low = LatencyCorrection::new(InputLatency::unavailable(), -5.0e9);
        assert_eq!(low.trim_ms(), -MAX_TRIM_MS);
    }

    /// The clamped extremes still convert to a sane signed nanosecond count.
    #[test]
    fn clamped_trim_converts_without_overflow() {
        let c = LatencyCorrection::new(InputLatency::unavailable(), -MAX_TRIM_MS);
        assert_eq!(c.total_nanos(), -10_000_000_000);
    }

    /// `AlreadyCorrected` is a claim about the world; `Unavailable` is an admission.
    /// Both carry zero, so only the tag can tell them apart.
    #[test]
    fn already_corrected_is_trustworthy_but_unavailable_is_not() {
        assert_eq!(InputLatency::already_corrected().extra, Duration::ZERO);
        assert_eq!(InputLatency::unavailable().extra, Duration::ZERO);
        assert!(LatencySource::AlreadyCorrected.is_trustworthy());
        assert!(LatencySource::Measured.is_trustworthy());
        assert!(!LatencySource::Unavailable.is_trustworthy());
    }

    /// The two zero-valued sources must not describe themselves identically, or the
    /// sidecar loses the distinction the tag exists to preserve.
    #[test]
    fn zero_sources_describe_themselves_distinguishably() {
        let corrected = InputLatency::already_corrected().describe();
        let unavailable = InputLatency::unavailable().describe();
        assert_ne!(corrected, unavailable);
        assert!(corrected.contains("already corrected"));
        assert!(unavailable.contains("uncorrected"));
    }

    /// Rounding is to the nearest nanosecond, so sub-nanosecond trim fractions do not
    /// silently truncate toward zero.
    #[test]
    fn trim_rounds_to_nearest_nanosecond() {
        let c = LatencyCorrection::new(InputLatency::unavailable(), 0.000_000_6);
        assert_eq!(c.total_nanos(), 1);
        let c = LatencyCorrection::new(InputLatency::unavailable(), -0.000_000_6);
        assert_eq!(c.total_nanos(), -1);
    }

    /// Querying must never panic and never take an unbounded amount of trust: on any
    /// platform the result is one of the three defined sources.
    #[test]
    fn query_always_returns_a_well_formed_result() {
        let l = InputLatency::query(Some("a device that does not exist"));
        assert!(matches!(
            l.source,
            LatencySource::AlreadyCorrected | LatencySource::Measured | LatencySource::Unavailable
        ));
    }

    /// On macOS the answer must stay zero and stay tagged `AlreadyCorrected`. If this
    /// ever fails, someone has added a second subtraction on top of cpal's.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reports_no_additional_correction_because_cpal_already_applied_it() {
        let l = InputLatency::query(None);
        assert_eq!(l.extra, Duration::ZERO);
        assert_eq!(l.source, LatencySource::AlreadyCorrected);
    }
}
