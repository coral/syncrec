//! Microphone permission, reduced to one shape the UI can actually reason about.
//!
//! The two platforms disagree about almost everything here. macOS has a real
//! four-state authorisation API and will prompt on request; Windows (for an
//! unpackaged Win32 desktop app) has no query and no request at all — the Settings
//! toggle is the entire mechanism, and the only way to learn you are blocked is to
//! open a stream and get silence or a failure. Papering over that difference with a
//! lowest-common-denominator boolean would force the UI to guess, so instead this
//! module keeps one honest enum with a `NotDetermined` *and* an `Unknown` state, and
//! lets each platform say which of them it can actually distinguish.
//!
//! The rule the rest of the app follows: never block on permission. Query is cheap
//! and synchronous, request is asynchronous and reports back over a channel, and a
//! denial discovered the hard way — a `cpal::Error` with
//! [`cpal::ErrorKind::PermissionDenied`] — folds back into the same enum through
//! [`from_cpal_error`]. One code path, three ways in.
//!
//! ## macOS: this only works inside a `.app` bundle
//!
//! `AVCaptureDevice`'s authorisation machinery reads `NSMicrophoneUsageDescription`
//! out of the main bundle's `Info.plist` to build the consent dialog. A bare binary
//! run from a terminal has no bundle and no usage string, so TCC has nothing to show
//! the user. The observed behaviour is not a clean error: depending on macOS version
//! the request either resolves to denied without ever showing a prompt, or the
//! process is killed outright for requesting a protected resource without a purpose
//! string. Either way the user sees nothing and concludes the recorder is broken.
//!
//! So: ship syncrec as a bundle with `NSMicrophoneUsageDescription` set, and treat
//! [`macos_appears_bundled`] as the early warning for developers running the raw
//! binary during `cargo run`.

use std::sync::mpsc::Sender;

use anyhow::Result;

/// Whether we are allowed to open the microphone — the one vocabulary the UI uses.
///
/// `NotDetermined` and `Unknown` are genuinely different and the distinction is load
/// bearing. `NotDetermined` means "asking will produce a prompt", so the UI can offer
/// a *Grant access* button that does something. `Unknown` means "this OS will not
/// tell us", so the only honest UI is to try and see — there is nothing to prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStatus {
    Granted,
    /// Refused, or refused on the user's behalf by policy. Only Settings can undo it.
    Denied,
    /// Never asked. A request will prompt.
    NotDetermined,
    /// The platform has no way to tell us. Attempt capture and infer from the result.
    Unknown,
}

impl PermissionStatus {
    pub fn label(self) -> &'static str {
        match self {
            PermissionStatus::Granted => "granted",
            PermissionStatus::Denied => "denied",
            PermissionStatus::NotDetermined => "not-determined",
            PermissionStatus::Unknown => "unknown",
        }
    }

    /// Whether opening a stream is worth attempting.
    ///
    /// Everything except an outright denial is: `NotDetermined` because the attempt
    /// itself triggers the prompt on macOS, and `Unknown` because on Windows the
    /// attempt *is* the query.
    pub fn can_attempt_capture(self) -> bool {
        !matches!(self, PermissionStatus::Denied)
    }

    /// Whether a prompt is available, i.e. whether [`request`] will do anything useful.
    pub fn can_prompt(self) -> bool {
        matches!(self, PermissionStatus::NotDetermined)
    }

    /// Whether the only way forward is the user visiting system settings.
    pub fn needs_settings_visit(self) -> bool {
        matches!(self, PermissionStatus::Denied)
    }

    /// What to put in front of the operator.
    pub fn advice(self) -> &'static str {
        match self {
            PermissionStatus::Granted => "Microphone access is granted.",
            PermissionStatus::Denied => {
                "Microphone access is blocked. Enable it in system settings, then restart syncrec."
            }
            PermissionStatus::NotDetermined => {
                "Microphone access has not been requested yet. Start a recording to be prompted."
            }
            PermissionStatus::Unknown => {
                "This system does not report microphone permission in advance; \
                 if capture fails, check the microphone privacy setting."
            }
        }
    }
}

/// Current status, without prompting and without opening a device.
///
/// Cheap enough to call on every UI frame if you like, though once per view refresh
/// is plenty.
pub fn status() -> PermissionStatus {
    platform::status()
}

/// Ask the OS to prompt the user, delivering the outcome over `tx`.
///
/// Returns as soon as the request is lodged. The answer arrives later — on macOS the
/// completion handler runs on an arbitrary dispatch queue, which is exactly why this
/// hands back over a channel rather than blocking: iced's runloop must keep turning
/// or the consent dialog itself will not draw.
///
/// The channel receives exactly one message, always, even on platforms that cannot
/// prompt at all (they answer immediately with whatever they do know). That keeps the
/// UI's "waiting for an answer" state from hanging forever on Windows.
///
/// `tx` may be dropped by the caller; the send is best-effort and a closed channel is
/// not an error.
pub fn request(tx: Sender<PermissionStatus>) -> Result<()> {
    platform::request(tx)
}

/// Open the system's microphone privacy settings.
///
/// The escape hatch for [`PermissionStatus::Denied`], which no API can reverse: once
/// a user has said no, only the user can say yes again, and only from Settings.
/// Best-effort — a failure here is a nuisance, not a recording failure.
pub fn open_settings() -> Result<()> {
    platform::open_settings()
}

/// Read a permission verdict out of a cpal failure.
///
/// On Windows this is the *only* way denial is ever discovered, since there is
/// nothing to query up front: the stream build comes back with
/// [`cpal::ErrorKind::PermissionDenied`] and that is the whole signal.
///
/// Returns `None` for every other kind of error, so callers can use it as a filter
/// without having to pre-classify: a missing device or an unsupported sample rate is
/// not a permission problem and must not be reported to the user as one.
pub fn from_cpal_error(err: &cpal::Error) -> Option<PermissionStatus> {
    match err.kind() {
        cpal::ErrorKind::PermissionDenied => Some(PermissionStatus::Denied),
        _ => None,
    }
}

/// Convenience predicate over [`from_cpal_error`].
pub fn is_permission_denied(err: &cpal::Error) -> bool {
    from_cpal_error(err) == Some(PermissionStatus::Denied)
}

/// Whether this process looks like it is running from inside a `.app` bundle.
///
/// A path heuristic (`.../Foo.app/Contents/MacOS/foo`) rather than a real bundle
/// lookup, which is all we need: the point is to warn a developer running the bare
/// binary that the permission prompt will not appear, not to be authoritative.
/// Always `false` off macOS.
pub fn macos_appears_bundled() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let path = exe.to_string_lossy();
    path.contains(".app/Contents/MacOS/")
}

#[cfg(target_os = "macos")]
mod platform {
    //! AVFoundation's `AVCaptureDevice` authorisation API.
    //!
    //! Audio and video share one mechanism here; passing `AVMediaTypeAudio` is
    //! documented as equivalent to `-[AVAudioSession requestRecordPermission:]`.
    //! Anything other than the audio or video media type throws an ObjC exception, so
    //! the constant is never derived from user input.
    //!
    //! `AVAuthorizationStatusRestricted` is folded into `Denied`. It means the user is
    //! not permitted to change the setting (MDM policy, parental controls), which from
    //! the recorder's point of view is a denial that cannot even be appealed — but the
    //! remedy we offer, "open Settings", is still the right next step, since that is
    //! where the restriction is visible.

    use super::PermissionStatus;
    use anyhow::{Result, anyhow};
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_av_foundation::{
        AVAuthorizationStatus, AVCaptureDevice, AVMediaType, AVMediaTypeAudio,
    };
    use std::sync::mpsc::Sender;

    /// The `AVMediaTypeAudio` constant, or `None` if AVFoundation somehow did not
    /// export it. The binding is `Option<&'static AVMediaType>` precisely because a
    /// framework constant can be absent on older systems.
    fn audio_media_type() -> Option<&'static AVMediaType> {
        // SAFETY: reading an immutable `NSString *` constant exported by AVFoundation.
        // It is initialised before any Rust code can run and is never mutated.
        unsafe { AVMediaTypeAudio }
    }

    pub fn status() -> PermissionStatus {
        let Some(media_type) = audio_media_type() else {
            return PermissionStatus::Unknown;
        };
        // SAFETY: `media_type` is AVMediaTypeAudio, one of the two values the method
        // accepts; any other would raise NSInvalidArgumentException. The call has no
        // thread affinity and no ownership implications.
        let raw = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };

        // `AVAuthorizationStatus` is a newtype over NSInteger rather than a Rust enum,
        // so compare against the named constants and leave a catch-all for a value
        // Apple adds later. Guessing at an unknown state would be worse than saying so.
        if raw == AVAuthorizationStatus::Authorized {
            PermissionStatus::Granted
        } else if raw == AVAuthorizationStatus::Denied || raw == AVAuthorizationStatus::Restricted {
            PermissionStatus::Denied
        } else if raw == AVAuthorizationStatus::NotDetermined {
            PermissionStatus::NotDetermined
        } else {
            PermissionStatus::Unknown
        }
    }

    pub fn request(tx: Sender<PermissionStatus>) -> Result<()> {
        // Already settled? Answer straight away rather than handing AVFoundation a
        // request it will resolve without any user interaction. Same channel contract
        // either way, so the caller cannot tell the difference and does not need to.
        let current = status();
        if current != PermissionStatus::NotDetermined {
            let _ = tx.send(current);
            return Ok(());
        }

        let media_type =
            audio_media_type().ok_or_else(|| anyhow!("AVMediaTypeAudio is unavailable"))?;

        let handler = RcBlock::new(move |granted: Bool| {
            let status = if granted.as_bool() {
                PermissionStatus::Granted
            } else {
                PermissionStatus::Denied
            };
            // Best-effort: the UI may have been torn down while the dialog was up.
            let _ = tx.send(status);
        });

        // SAFETY: `media_type` is AVMediaTypeAudio. The block is invoked at most once,
        // on an arbitrary dispatch queue; it owns its `Sender` outright and touches
        // nothing else, so there is no shared state to race on and no thread affinity
        // to violate. `RcBlock` is copied to the heap by AVFoundation and released
        // after the handler runs, so it outliving this stack frame is fine.
        unsafe {
            AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &handler);
        }
        Ok(())
    }

    pub fn open_settings() -> Result<()> {
        // The documented deep link for the Privacy & Security > Microphone pane. The
        // anchor name still says "preference"; it has survived the System Settings
        // rewrite and remains the supported form.
        const URL: &str =
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone";
        let ok = std::process::Command::new("/usr/bin/open")
            .arg(URL)
            .status()
            .map_err(|e| anyhow!("could not launch `open`: {e}"))?
            .success();
        if ok {
            Ok(())
        } else {
            Err(anyhow!("`open` refused the privacy settings URL"))
        }
    }
}

#[cfg(windows)]
mod platform {
    //! Windows has no microphone permission API for unpackaged desktop apps.
    //!
    //! This is worth stating plainly because it looks like an omission. The
    //! `Windows.Security.Authorization.AppCapabilityAccess` and
    //! `AppCapability.RequestAccessAsync` surfaces that do exist are for packaged
    //! (MSIX/UWP) apps with a declared capability. A plain Win32 executable has no
    //! package identity, so there is nothing to query and nothing to request: the
    //! Settings > Privacy & security > Microphone toggle is the entire gate.
    //!
    //! What actually happens when the toggle is off is worse than an error. Depending
    //! on the Windows build, `IAudioClient::Initialize` fails with `E_ACCESSDENIED`
    //! (which cpal surfaces as `ErrorKind::PermissionDenied`) *or* the stream opens
    //! normally and delivers digital silence forever. That second case is why the
    //! recorder also watches its input meters: a permission problem can be
    //! indistinguishable from a dead microphone at the API level.
    //!
    //! Hence `Unknown` up front, and [`super::from_cpal_error`] as the real detector.

    use super::PermissionStatus;
    use anyhow::{Result, anyhow};
    use std::os::windows::process::CommandExt;
    use std::sync::mpsc::Sender;

    /// Detach the helper from a console so launching Settings does not flash a window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub fn status() -> PermissionStatus {
        PermissionStatus::Unknown
    }

    pub fn request(tx: Sender<PermissionStatus>) -> Result<()> {
        // Nothing to ask and nobody to ask it of. Answer immediately so the UI's
        // pending state always resolves; the channel contract is the same everywhere.
        let _ = tx.send(PermissionStatus::Unknown);
        Ok(())
    }

    pub fn open_settings() -> Result<()> {
        // `ShellExecuteW` would be the tidier call, but it lives behind the windows
        // crate's `Win32_UI_WindowsAndMessaging` feature, which nothing in this
        // dependency graph enables. Shelling out to the shell's own URL handler gets
        // the identical result with no feature surface at all.
        let ok = std::process::Command::new("cmd")
            .args(["/C", "start", "", "ms-settings:privacy-microphone"])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|e| anyhow!("could not launch the settings URL handler: {e}"))?
            .success();
        if ok {
            Ok(())
        } else {
            Err(anyhow!("the shell refused ms-settings:privacy-microphone"))
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    //! No gatekeeper we know how to talk to.
    //!
    //! Reported as `Granted` rather than `Unknown` on purpose: on these systems access
    //! is governed by file permissions on the device node, which produces an ordinary
    //! I/O failure at open time and needs no permission UI. Claiming `Unknown` would
    //! make the UI display a warning about a problem that does not exist here.

    use super::PermissionStatus;
    use anyhow::{Result, anyhow};
    use std::sync::mpsc::Sender;

    pub fn status() -> PermissionStatus {
        PermissionStatus::Granted
    }

    pub fn request(tx: Sender<PermissionStatus>) -> Result<()> {
        let _ = tx.send(PermissionStatus::Granted);
        Ok(())
    }

    pub fn open_settings() -> Result<()> {
        Err(anyhow!(
            "no microphone privacy settings page is known for this platform"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Only an outright denial should stop us trying; the two "we don't know" states
    /// must not block capture, since on those platforms the attempt is the query.
    #[test]
    fn only_denied_blocks_a_capture_attempt() {
        assert!(PermissionStatus::Granted.can_attempt_capture());
        assert!(PermissionStatus::NotDetermined.can_attempt_capture());
        assert!(PermissionStatus::Unknown.can_attempt_capture());
        assert!(!PermissionStatus::Denied.can_attempt_capture());
    }

    /// A prompt is only ever available before the user has decided. Offering one for
    /// `Unknown` would put a button in the UI that provably cannot do anything.
    #[test]
    fn only_not_determined_can_be_prompted() {
        assert!(PermissionStatus::NotDetermined.can_prompt());
        assert!(!PermissionStatus::Granted.can_prompt());
        assert!(!PermissionStatus::Denied.can_prompt());
        assert!(!PermissionStatus::Unknown.can_prompt());
    }

    /// Settings is the remedy for denial and for nothing else.
    #[test]
    fn only_denied_sends_the_user_to_settings() {
        assert!(PermissionStatus::Denied.needs_settings_visit());
        assert!(!PermissionStatus::Granted.needs_settings_visit());
        assert!(!PermissionStatus::NotDetermined.needs_settings_visit());
        assert!(!PermissionStatus::Unknown.needs_settings_visit());
    }

    /// Every state must have its own label and its own advice, or the UI collapses
    /// distinctions the enum exists to preserve.
    #[test]
    fn every_status_has_a_distinct_label_and_advice() {
        let all = [
            PermissionStatus::Granted,
            PermissionStatus::Denied,
            PermissionStatus::NotDetermined,
            PermissionStatus::Unknown,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.label(), b.label());
                assert_ne!(a.advice(), b.advice());
            }
            assert!(!a.label().is_empty());
            assert!(!a.advice().is_empty());
        }
    }

    /// A cpal permission failure is the only thing that maps to `Denied`.
    #[test]
    fn cpal_permission_denied_maps_to_denied() {
        let err = cpal::Error::new(cpal::ErrorKind::PermissionDenied);
        assert_eq!(from_cpal_error(&err), Some(PermissionStatus::Denied));
        assert!(is_permission_denied(&err));
    }

    /// Unrelated cpal failures must not be reported to the user as permission
    /// problems: telling someone to visit Settings because their sample rate is
    /// unsupported sends them off to fix the wrong thing.
    #[test]
    fn other_cpal_errors_do_not_map_to_a_permission_verdict() {
        for kind in [
            cpal::ErrorKind::DeviceNotAvailable,
            cpal::ErrorKind::DeviceBusy,
            cpal::ErrorKind::UnsupportedConfig,
            cpal::ErrorKind::BackendError,
            cpal::ErrorKind::Other,
        ] {
            let err = cpal::Error::new(kind);
            assert_eq!(
                from_cpal_error(&err),
                None,
                "{kind:?} is not a permission error"
            );
            assert!(!is_permission_denied(&err));
        }
    }

    /// The mapping ignores the message and keys only off the kind, so a backend that
    /// attaches prose to a permission error still classifies correctly.
    #[test]
    fn cpal_error_mapping_ignores_the_message() {
        let err = cpal::Error::with_message(
            cpal::ErrorKind::PermissionDenied,
            "the microphone privacy setting is off",
        );
        assert_eq!(from_cpal_error(&err), Some(PermissionStatus::Denied));
    }

    /// Whatever the platform, `status()` must return promptly and without panicking.
    #[test]
    fn status_query_is_safe_to_call_anywhere() {
        let s = status();
        assert!(!s.label().is_empty());
    }

    /// The channel contract: a request always produces exactly one answer, even where
    /// the platform cannot prompt. A UI that waits for a message must never hang.
    #[test]
    fn request_always_delivers_exactly_one_answer() {
        // Guard, not laziness: on macOS an undecided status makes `request` hand the
        // job to AVFoundation, which puts a consent dialog in front of whoever is
        // running the test suite — and from an unbundled `cargo test` binary that is
        // at best a silent denial and at worst a killed process. So only exercise the
        // paths that resolve without user interaction.
        if cfg!(target_os = "macos") && status() == PermissionStatus::NotDetermined {
            return;
        }

        let (tx, rx) = mpsc::channel();
        request(tx).expect("lodging a permission request should not fail");
        let first = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("a request that needs no prompt must answer immediately");
        assert!(!first.label().is_empty());
        assert!(rx.try_recv().is_err(), "exactly one answer, not more");
    }

    /// The bundle heuristic is macOS-only and must be a flat `false` elsewhere rather
    /// than an accidental `true` from a path that happens to contain the substring.
    #[test]
    fn bundle_heuristic_is_false_off_macos() {
        if !cfg!(target_os = "macos") {
            assert!(!macos_appears_bundled());
        }
    }
}
