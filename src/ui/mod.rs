//! The iced front end.
//!
//! The interesting decision here is that the input stream runs continuously from
//! the moment a device is chosen, not from the moment the operator hits record.
//! You cannot set gain against a meter that only comes alive once you are already
//! recording, and handing the already-running stream to the writer means pressing
//! record does not reopen the device — so there is no glitch, and no risk of the
//! device being grabbed by something else in between.
//!
//! The window has two screens. The recording screen carries only what changes take
//! to take: the device, the meters, the folder, the transport. Everything that
//! belongs to the rig rather than the take — which clock to trust, the NTP server,
//! the frame rate, which machine leads — lives on the settings screen, where an
//! operator watching a meter cannot reach it by accident.
//!
//! In ethersync follower mode the record button is not a button. The leader's
//! transport is the record state for the whole rig, so this machine watches it and
//! rolls when it rolls; offering a local override would only ever produce a take
//! that does not line up with the others.

pub mod meter;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::widget::{
    Space, button, checkbox, column, container, pick_list, radio, row, rule, scrollable, text,
    text_input,
};
use iced::{Alignment, Border, Color, Element, Fill, Length, Subscription, Task, Theme};
use libethersync::DiscoveredLeader;

use crate::audio::capture::Capture;
use crate::audio::meters::Meters;
use crate::audio::session::{self, Recording};
use crate::audio::writer::{Sidecar, TakePaths};
use crate::audio::{self, DeviceChoice, Negotiated};
use crate::bwf::{Provenance, TimecodeStamp};
use crate::clock::{ClockModel, ClockSnapshot, NtpReference, RefStatus, Reference, sntp};
use crate::ethersync::{Fps, Link, Role};
use crate::finalize::{self, Outcome};
use crate::latency::{InputLatency, LatencyCorrection};
use crate::permission::{self, PermissionStatus};
use crate::settings::{Settings, TimeSource};

/// Meter refresh. Fast enough that a transient peak is never missed by the eye.
const TICK: Duration = Duration::from_millis(16);

pub fn run() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title(App::title)
        .subscription(App::subscription)
        .theme(App::theme)
        .window_size((760.0, 660.0))
        .run()
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    RefreshDevices,
    DeviceSelected(DeviceChoice),
    BrowseDir,
    DirPicked(Option<PathBuf>),
    BaseNameChanged(String),
    ToggleRecord,
    RequestPermission,
    OpenPrivacySettings,
    ClearClip,

    ShowSettings,
    ShowRecorder,
    SourceSelected(TimeSource),
    NtpServerChanged(String),
    NtpServerCommitted,
    TrimChanged(String),
    RoleSelected(Role),
    FpsSelected(Fps),
    DropFrameToggled(bool),
    LeaderNameChanged(String),
    LeaderPortChanged(String),
    FollowerAddressChanged(String),
    LinkSettingsCommitted,
    UseDiscovered(String),
}

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Record,
    Settings,
}

/// What the recorder is doing right now.
enum Stage {
    Idle,
    Recording {
        recording: Recording,
        /// The take's own meters. The monitor stream is gone while recording, so
        /// the UI reads levels from the capture the writer owns.
        meters: Arc<Meters>,
    },
    /// Resampling on a worker thread; the UI stays responsive.
    Finalizing { rx: Receiver<Result<Box<Outcome>, String>> },
}

/// A live input stream that is not (yet) being written anywhere.
struct Monitor {
    capture: Option<Capture>,
    meters: Arc<Meters>,
    negotiated: Negotiated,
}

pub struct App {
    devices: Vec<DeviceChoice>,
    selected: Option<DeviceChoice>,
    monitor: Option<Monitor>,

    output_dir: PathBuf,
    base_name: String,

    settings: Settings,
    /// Text mirrors for the numeric settings, so a half-typed value is not thrown
    /// away by a parse failure between keystrokes.
    trim_text: String,
    port_text: String,
    screen: Screen,

    clock: Arc<Mutex<ClockModel>>,
    poller: Option<sntp::Poller>,

    link: Option<Link>,
    /// Leaders seen on the LAN while browsing.
    discovered: Vec<DiscoveredLeader>,
    /// Addresses a follower could be pointed at, when we are the leader.
    /// Cached: enumerating interfaces is not something to do sixty times a second.
    endpoints: Vec<SocketAddr>,
    endpoints_checked: Instant,
    /// A discovered address that would not connect, so we stop hammering it.
    refused: Option<SocketAddr>,
    /// The leader's transport state as of the last tick, for edge detection.
    rolling: Option<bool>,
    /// The leader rolled again while we were still finalising the last take.
    pending_roll: bool,

    /// Refreshed once a tick so `view` never has to touch a reader.
    status: RefStatus,
    timecode: Option<String>,

    permission: PermissionStatus,
    permission_rx: Option<Receiver<PermissionStatus>>,

    stage: Stage,
    take: Option<TakePaths>,
    last_result: Option<String>,
    error: Option<String>,
    next_preview: String,

    // Reused every frame so the meter does not allocate at 60 Hz.
    peaks: Vec<f32>,
    rms: Vec<f32>,
    clips: Vec<bool>,
}

impl App {
    fn boot() -> (Self, Task<Message>) {
        let default_dir = dirs_audio_fallback();
        let clock = Arc::new(Mutex::new(ClockModel::new(Instant::now())));
        let settings = Settings::load();

        let mut app = Self {
            devices: Vec::new(),
            selected: None,
            monitor: None,
            output_dir: default_dir,
            base_name: "rec".into(),
            trim_text: format_trim(settings.trim_ms),
            port_text: settings.leader_port.to_string(),
            screen: Screen::Record,
            settings,
            clock,
            poller: None,
            link: None,
            discovered: Vec::new(),
            endpoints: Vec::new(),
            endpoints_checked: Instant::now(),
            refused: None,
            rolling: None,
            pending_roll: false,
            status: offline_status("starting"),
            timecode: None,
            permission: permission::status(),
            permission_rx: None,
            stage: Stage::Idle,
            take: None,
            last_result: None,
            error: None,
            next_preview: String::new(),
            peaks: Vec::new(),
            rms: Vec::new(),
            clips: Vec::new(),
        };
        app.refresh_preview();
        app.apply_time_source();

        // Ask for the microphone before touching any device, so the first thing the
        // operator sees is the system prompt rather than an inscrutable failure.
        let task = if app.permission.can_prompt() {
            Task::done(Message::RequestPermission)
        } else {
            Task::done(Message::RefreshDevices)
        };
        (app, task)
    }

    fn title(&self) -> String {
        match &self.stage {
            Stage::Recording { .. } => "syncrec — recording".into(),
            Stage::Finalizing { .. } => "syncrec — finalising".into(),
            Stage::Idle => "syncrec".into(),
        }
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }

    fn subscription(&self) -> Subscription<Message> {
        iced::time::every(TICK).map(|_| Message::Tick)
    }

    // -----------------------------------------------------------------------
    // The time reference
    // -----------------------------------------------------------------------

    fn clock_snapshot(&self) -> Option<ClockSnapshot> {
        self.clock.lock().ok().map(|m| m.snapshot())
    }

    /// Whether the clock is good enough for a take to be correctable.
    ///
    /// The same condition `finalize`'s safety gate applies, checked up front so the
    /// operator learns the take will not be corrected *before* recording it rather
    /// than afterwards.
    fn clock_ready(&self) -> bool {
        self.status.ready()
    }

    /// Whether this machine is driving the rig's transport.
    fn leading(&self) -> bool {
        self.settings.source == TimeSource::Ethersync
            && self.settings.role == Role::Leader
            && self.link.is_some()
    }

    /// Whether this machine's record button belongs to somebody else.
    fn slaved(&self) -> bool {
        self.settings.source == TimeSource::Ethersync && self.settings.role == Role::Follower
    }

    /// Build the reference this take will be measured against.
    fn reference(&self) -> anyhow::Result<Box<dyn Reference>> {
        match self.settings.source {
            TimeSource::Ntp => Ok(Box::new(NtpReference::new(
                self.settings.ntp_server.clone(),
                Arc::clone(&self.clock),
            ))),
            TimeSource::Ethersync => {
                let link = self
                    .link
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("no ethersync link"))?;
                Ok(Box::new(link.reference()?))
            }
        }
    }

    /// Tear down whichever reference is running and start the configured one.
    fn apply_time_source(&mut self) {
        self.poller = None;
        self.link = None;
        self.discovered.clear();
        self.endpoints.clear();
        self.refused = None;
        self.rolling = None;
        self.pending_roll = false;
        match self.settings.source {
            TimeSource::Ntp => self.restart_clock(),
            TimeSource::Ethersync => self.relink(),
        }
        self.refresh_status();
    }

    fn restart_clock(&mut self) {
        // Drop the old poller first so only one thread is ever polling.
        self.poller = None;
        self.clock = Arc::new(Mutex::new(ClockModel::new(Instant::now())));
        let server = self.settings.ntp_server.trim().to_string();
        if !server.is_empty() {
            self.poller = Some(sntp::Poller::spawn(server, Arc::clone(&self.clock)));
        }
    }

    /// Stand up the ethersync engine for the configured role.
    fn relink(&mut self) {
        // The old engine owns a worker thread, a UDP socket and possibly an mDNS
        // registration. Drop it before binding anything again.
        self.link = None;
        self.discovered.clear();
        self.endpoints.clear();
        self.refused = None;
        self.rolling = None;

        let format = match self.settings.fps.format(self.settings.drop_frame) {
            Ok(f) => f,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                return;
            }
        };
        let built = match self.settings.role {
            Role::Leader => Link::leader(
                &self.settings.leader_name,
                self.settings.leader_port,
                format,
                true,
            ),
            Role::Follower => match self.settings.follower_socket() {
                Some(address) => Link::follower(address, None, format),
                // No address typed: browse, and take the first leader that
                // answers. On a rig with one leader that is the right answer and
                // saves the operator typing an IP into a laptop in a field.
                None => Link::browsing(format),
            },
        };
        match built {
            Ok(link) => {
                self.endpoints = link.endpoints();
                self.link = Some(link);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Re-enumerate the leader's addresses, but only while anyone is looking.
    ///
    /// They change when a cable goes in, so a one-shot read at startup would go
    /// stale — and `local_endpoints` walks the OS interface table, which is not
    /// something to do on a 60 Hz redraw.
    fn refresh_endpoints(&mut self) {
        const EVERY: Duration = Duration::from_secs(1);
        if self.screen != Screen::Settings
            || self.settings.role != Role::Leader
            || self.endpoints_checked.elapsed() < EVERY
        {
            return;
        }
        self.endpoints_checked = Instant::now();
        if let Some(link) = &self.link {
            self.endpoints = link.endpoints();
        }
    }

    /// Re-read the reference. Once a tick, so `view` can stay immutable.
    fn refresh_status(&mut self) {
        match self.settings.source {
            TimeSource::Ntp => {
                self.status = match self.clock_snapshot() {
                    Some(snap) => NtpReference::status_of(&self.settings.ntp_server, &snap),
                    None => offline_status("clock unavailable"),
                };
                self.timecode = None;
            }
            TimeSource::Ethersync => match &mut self.link {
                Some(link) => {
                    self.status = link.status();
                    self.timecode = link.label();
                }
                None => {
                    self.status = offline_status("no link");
                    self.timecode = None;
                }
            },
        }
    }


    // -----------------------------------------------------------------------
    // Following the leader
    // -----------------------------------------------------------------------

    /// Service the link: drain its events, browse, and slave the transport.
    ///
    /// The link is taken out of `self` for the duration because reading a timecode
    /// reader needs `&mut`, and half of what we do with the answer needs `&mut
    /// self` as well.
    fn tick_link(&mut self) {
        let Some(mut link) = self.link.take() else {
            return;
        };
        link.poll_events();
        let browsing = link.address().is_none();
        if browsing {
            self.discovered = link.discovered();
        }
        let rolling = (!browsing && self.slaved())
            .then(|| link.rolling())
            .flatten();
        self.link = Some(link);

        if browsing {
            self.adopt_discovered_leader();
            return;
        }
        if let Some(rolling) = rolling {
            self.follow_transport(rolling);
        }
    }

    /// Connect to the first leader the LAN offers, if we are waiting for one.
    fn adopt_discovered_leader(&mut self) {
        if self.settings.follower_socket().is_some() {
            return;
        }
        let Some(leader) = self.discovered.first().cloned() else {
            return;
        };
        // Prefer IPv4: a link-local IPv6 address needs a scope id the operator
        // cannot see and cannot fix.
        let Some(address) = leader
            .addresses
            .iter()
            .find(|a| a.is_ipv4())
            .or_else(|| leader.addresses.first())
            .copied()
        else {
            return;
        };
        if self.refused == Some(address) {
            return;
        }
        let Ok(format) = self.settings.fps.format(self.settings.drop_frame) else {
            return;
        };

        self.link = None;
        // The fingerprint came from the same mDNS record as the address, so pinning
        // it is worth doing even though it is only as trustworthy as the LAN.
        match Link::follower(address, Some(&leader.fingerprint), format) {
            Ok(link) => {
                self.link = Some(link);
                self.rolling = None;
                self.error = None;
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                self.refused = Some(address);
                // Go back to browsing rather than sitting with no link at all.
                self.link = Link::browsing(format).ok();
            }
        }
    }

    /// Mirror the leader's transport onto this machine's recorder.
    fn follow_transport(&mut self, rolling: bool) {
        if self.rolling == Some(rolling) {
            return;
        }
        self.rolling = Some(rolling);
        match (&self.stage, rolling) {
            (Stage::Idle, true) => self.start_recording(),
            (Stage::Recording { .. }, false) => self.stop_recording(),
            // The leader stopped and rolled again before the resampler finished.
            // Remember it; the take starts the moment the thread comes back.
            (Stage::Finalizing { .. }, true) => self.pending_roll = true,
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Housekeeping
    // -----------------------------------------------------------------------

    fn trim_ms(&self) -> f64 {
        self.settings.trim_ms
    }

    fn latency(&self) -> LatencyCorrection {
        let platform = InputLatency::query(self.selected.as_ref().map(|d| d.name.as_str()));
        LatencyCorrection::new(platform, self.trim_ms())
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save() {
            self.error = Some(format!("could not save settings: {e}"));
        }
    }

    fn refresh_preview(&mut self) {
        let base = if self.base_name.trim().is_empty() {
            "rec"
        } else {
            self.base_name.trim()
        };
        let paths = crate::audio::writer::next_take(&self.output_dir, base);
        self.next_preview = paths
            .final_wav
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => self.on_tick(),
            Message::RefreshDevices => {
                match audio::input_devices() {
                    Ok(devices) => {
                        // Keep the current choice if it is still plugged in.
                        let keep = self
                            .selected
                            .as_ref()
                            .filter(|s| devices.iter().any(|d| d.id == s.id))
                            .cloned();
                        self.devices = devices;
                        let pick = keep.or_else(|| self.devices.first().cloned());
                        if pick != self.selected {
                            self.selected = pick;
                            self.open_monitor();
                        }
                    }
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
                Task::none()
            }
            Message::DeviceSelected(d) => {
                self.selected = Some(d);
                self.open_monitor();
                Task::none()
            }
            Message::BrowseDir => {
                let start = self.output_dir.clone();
                Task::perform(
                    async move {
                        rfd::AsyncFileDialog::new()
                            .set_directory(start)
                            .set_title("Choose where to record")
                            .pick_folder()
                            .await
                            .map(|h| h.path().to_path_buf())
                    },
                    Message::DirPicked,
                )
            }
            Message::DirPicked(Some(dir)) => {
                self.output_dir = dir;
                self.refresh_preview();
                Task::none()
            }
            Message::DirPicked(None) => Task::none(),
            Message::BaseNameChanged(s) => {
                // Keep the sequence name usable as a filename.
                self.base_name = s.replace(['/', '\\', ':'], "-");
                self.refresh_preview();
                Task::none()
            }
            Message::ToggleRecord => {
                self.toggle_record();
                Task::none()
            }
            Message::RequestPermission => {
                let (tx, rx) = channel();
                match permission::request(tx) {
                    Ok(()) => self.permission_rx = Some(rx),
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
                Task::none()
            }
            Message::OpenPrivacySettings => {
                if let Err(e) = permission::open_settings() {
                    self.error = Some(format!("{e:#}"));
                }
                Task::none()
            }
            Message::ClearClip => {
                if let Some(m) = &self.monitor {
                    m.meters.clear_clip();
                }
                Task::none()
            }

            Message::ShowSettings => {
                self.screen = Screen::Settings;
                Task::none()
            }
            Message::ShowRecorder => {
                self.screen = Screen::Record;
                self.save_settings();
                Task::none()
            }
            Message::SourceSelected(source) => {
                if self.settings.source != source {
                    self.settings.source = source;
                    self.apply_time_source();
                    self.save_settings();
                }
                Task::none()
            }
            Message::NtpServerChanged(s) => {
                self.settings.ntp_server = s;
                Task::none()
            }
            Message::NtpServerCommitted => {
                self.restart_clock();
                self.save_settings();
                Task::none()
            }
            Message::TrimChanged(s) => {
                if s.is_empty() || s == "-" {
                    self.trim_text = s;
                    self.settings.trim_ms = 0.0;
                } else if let Ok(v) = s.parse::<f64>() {
                    self.trim_text = s;
                    self.settings.trim_ms = v;
                }
                Task::none()
            }
            Message::RoleSelected(role) => {
                if self.settings.role != role {
                    self.settings.role = role;
                    self.relink();
                    self.save_settings();
                }
                Task::none()
            }
            Message::FpsSelected(fps) => {
                if self.settings.fps != fps {
                    self.settings.fps = fps;
                    if !fps.supports_drop_frame() {
                        self.settings.drop_frame = false;
                    }
                    self.relink();
                    self.save_settings();
                }
                Task::none()
            }
            Message::DropFrameToggled(on) => {
                self.settings.drop_frame = on;
                self.relink();
                self.save_settings();
                Task::none()
            }
            Message::LeaderNameChanged(s) => {
                self.settings.leader_name = s;
                Task::none()
            }
            Message::LeaderPortChanged(s) => {
                if s.is_empty() {
                    self.port_text = s;
                } else if let Ok(p) = s.parse::<u16>() {
                    self.port_text = s;
                    self.settings.leader_port = p;
                }
                Task::none()
            }
            Message::FollowerAddressChanged(s) => {
                self.settings.follower_address = s;
                Task::none()
            }
            Message::LinkSettingsCommitted => {
                self.relink();
                self.save_settings();
                Task::none()
            }
            Message::UseDiscovered(address) => {
                self.settings.follower_address = address;
                self.settings.role = Role::Follower;
                self.relink();
                self.save_settings();
                Task::none()
            }
        }
    }

    fn on_tick(&mut self) -> Task<Message> {
        if let Some(rx) = &self.permission_rx
            && let Ok(status) = rx.try_recv()
        {
            self.permission = status;
            self.permission_rx = None;
            if status.can_attempt_capture() {
                return Task::done(Message::RefreshDevices);
            }
        }

        if self.settings.source == TimeSource::Ethersync {
            self.tick_link();
            self.refresh_endpoints();
        }
        self.refresh_status();

        // Levels come from whichever stream is live: the monitor when idle, the
        // take's own capture while recording.
        let meters = match &self.stage {
            Stage::Recording { meters, .. } => Some(Arc::clone(meters)),
            _ => self.monitor.as_ref().map(|m| Arc::clone(&m.meters)),
        };
        if let Some(meters) = meters {
            meters.take_peaks(&mut self.peaks);
            meters.rms(&mut self.rms);
            self.clips.clear();
            for ch in 0..meters.channels() {
                self.clips.push(meters.clipped(ch));
            }
        }

        // Nothing drains the monitor's ring, so empty it here or the overrun
        // counter climbs against a take that has not started.
        if let Some(monitor) = &mut self.monitor
            && let Some(capture) = &mut monitor.capture
        {
            while capture.audio.pop().is_ok() {}
            while capture.marks.pop().is_ok() {}
        }

        if let Stage::Finalizing { rx } = &self.stage
            && let Ok(result) = rx.try_recv()
        {
            match result {
                Ok(outcome) => {
                    let name = self
                        .take
                        .as_ref()
                        .map(|t| file_name(&t.final_wav))
                        .unwrap_or_default();
                    self.last_result = Some(describe_outcome(&name, &outcome));
                }
                Err(e) => self.error = Some(e),
            }
            self.stage = Stage::Idle;
            self.refresh_preview();
            self.open_monitor();

            // The leader rolled again while this take was still resampling.
            if std::mem::take(&mut self.pending_roll) && self.rolling == Some(true) {
                self.start_recording();
            }
        }

        Task::none()
    }

    /// Open (or reopen) the always-on input stream for the selected device.
    fn open_monitor(&mut self) {
        self.monitor = None;
        let Some(choice) = self.selected.clone() else {
            return;
        };

        let result = audio::find_device(&choice)
            .and_then(|device| {
                let negotiated = audio::negotiate(&device)?;
                let capture = audio::capture::build(&device, &negotiated)?;
                capture.play()?;
                Ok((capture, negotiated))
            })
            .map_err(|e| {
                // On Windows a blocked microphone only shows up here, at open time.
                if let Some(cpal_err) = e.downcast_ref::<cpal::Error>()
                    && permission::is_permission_denied(cpal_err)
                {
                    self.permission = PermissionStatus::Denied;
                }
                format!("{e:#}")
            });

        match result {
            Ok((capture, negotiated)) => {
                self.monitor = Some(Monitor {
                    meters: Arc::clone(&capture.meters),
                    capture: Some(capture),
                    negotiated,
                });
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// The record button, which in leader mode is the whole rig's record button.
    fn toggle_record(&mut self) {
        match self.stage {
            Stage::Idle => {
                // Roll the transport *before* opening the stream. The timeline has
                // to already be running when the first sample lands, or sample zero
                // resolves against a paused clock and the whole file is stamped
                // wherever the leader happened to be parked.
                if self.leading()
                    && let Some(link) = &mut self.link
                    && let Err(e) = link.roll()
                {
                    self.error = Some(format!("{e:#}"));
                    return;
                }
                self.start_recording();
                // The capture failed to open. Put the rig back where it was rather
                // than leaving every follower recording a take this machine is not.
                if self.leading()
                    && !matches!(self.stage, Stage::Recording { .. })
                    && let Some(link) = &mut self.link
                {
                    let _ = link.halt();
                }
            }
            Stage::Recording { .. } => {
                // Stop and drain first. The writer resolves its last marks during
                // the drain, and it has to do that against a timeline that is still
                // running or the tail of the take lands on top of itself.
                self.stop_recording();
                if self.leading()
                    && let Some(link) = &mut self.link
                    && let Err(e) = link.halt()
                {
                    self.error = Some(format!("{e:#}"));
                }
            }
            Stage::Finalizing { .. } => {}
        }
    }

    fn start_recording(&mut self) {
        let Some(choice) = self.selected.clone() else {
            self.error = Some("no input device selected".into());
            return;
        };

        let base = if self.base_name.trim().is_empty() {
            "rec"
        } else {
            self.base_name.trim()
        };
        if let Err(e) = std::fs::create_dir_all(&self.output_dir) {
            self.error = Some(format!("cannot create {}: {e}", self.output_dir.display()));
            return;
        }
        let paths = crate::audio::writer::next_take(&self.output_dir, base);

        let reference = match self.reference() {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                return;
            }
        };

        // Open a brand new stream for the take rather than handing over the
        // monitoring one.
        //
        // The whole time base rests on "file sample 0 is the stream's first
        // captured frame", which is what makes the first callback's timestamp a
        // valid anchor. A monitoring stream has already been counting frames and
        // has already thrown frame 0 away, so reusing it offsets every sample index
        // by however long the operator spent setting gain and leaves the take with
        // no anchor at all. Reopening costs tens of milliseconds, once, and buys
        // back the invariant.
        self.monitor = None;

        let built = audio::find_device(&choice).and_then(|device| {
            let negotiated = audio::negotiate(&device)?;
            let capture = audio::capture::build(&device, &negotiated)?;
            Ok(capture)
        });
        let capture = match built {
            Ok(c) => c,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                self.open_monitor();
                return;
            }
        };

        let meters = Arc::clone(&capture.meters);
        let config = session::SessionConfig {
            paths: paths.clone(),
            device_name: choice.name.clone(),
            latency: self.latency(),
        };

        match session::start(capture, reference, config) {
            Ok(recording) => {
                self.take = Some(paths);
                self.last_result = None;
                self.error = None;
                self.stage = Stage::Recording { recording, meters };
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                self.open_monitor();
            }
        }
    }

    fn stop_recording(&mut self) {
        let Stage::Recording { recording, .. } = std::mem::replace(&mut self.stage, Stage::Idle)
        else {
            return;
        };

        let take = match recording.stop() {
            Ok(t) => t,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                self.open_monitor();
                return;
            }
        };

        let trim_ms = self.trim_ms();
        let (tx, rx) = channel();

        // Resampling a long take is slow; do it off the UI thread and report back
        // through the same tick that drives the meters.
        std::thread::Builder::new()
            .name("syncrec-finalize".into())
            .spawn(move || {
                let result = finalize_take(&take, trim_ms)
                    .map(Box::new)
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(result);
            })
            .ok();

        self.stage = Stage::Finalizing { rx };
        self.open_monitor();
    }

    // -----------------------------------------------------------------------
    // Views
    // -----------------------------------------------------------------------

    fn view(&self) -> Element<'_, Message> {
        match self.screen {
            Screen::Record => self.record_view(),
            Screen::Settings => self.settings_view(),
        }
    }

    fn record_view(&self) -> Element<'_, Message> {
        let mut body = column![self.header(), rule::horizontal(1)].spacing(12);

        if self.permission.needs_settings_visit() {
            body = body.push(self.permission_panel());
        }

        body = body
            .push(self.device_row())
            .push(self.meter_panel())
            .push(rule::horizontal(1))
            .push(self.destination_rows())
            .push(Space::new().height(Fill))
            .push(rule::horizontal(1))
            .push(self.transport());

        container(body.spacing(12))
            .padding(18)
            .width(Fill)
            .height(Fill)
            .into()
    }

    fn header(&self) -> Element<'_, Message> {
        let right: Element<'_, Message> = match self.settings.source {
            TimeSource::Ntp => self.ntp_indicator(),
            TimeSource::Ethersync => self.link_indicator(),
        };

        row![
            text("syncrec").size(22),
            Space::new().width(Fill),
            right,
            Space::new().width(Length::Fixed(12.0)),
            button(text("Settings").size(12)).on_press(Message::ShowSettings),
        ]
        .align_y(Alignment::Center)
        .into()
    }

    fn ntp_indicator(&self) -> Element<'_, Message> {
        let detail = match self.clock_snapshot() {
            Some(s) => {
                let disp = s
                    .dispersion()
                    .map(|d| format!("±{:.1} ms", d * 1e3))
                    .unwrap_or_else(|| "±?".into());
                let ppm = s
                    .fit()
                    .filter(|f| f.slope_trusted)
                    .map(|f| format!(" · {:+.1} ppm", f.ppm()))
                    .unwrap_or_default();
                let off = s
                    .system_clock_error()
                    .map(|e| format!(" · OS clock {:+.1} ms", e * 1e3))
                    .unwrap_or_default();
                format!("{disp}{ppm}{off} · {} polls", s.accepted)
            }
            None => "clock unavailable".into(),
        };

        column![
            row![
                status_dot(self.status.synced),
                text(format!("NTP {}", self.status.label)).size(13),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
            text(detail).size(11).color(DIM),
        ]
        .align_x(Alignment::End)
        .spacing(2)
        .into()
    }

    /// The ethersync corner: which end of the rig this is, and what it can see.
    ///
    /// The role lives here rather than in settings because it is the one link
    /// setting that changes on the day — a machine is promoted to leader because
    /// another one died, and that should not be four clicks deep.
    fn link_indicator(&self) -> Element<'_, Message> {
        let roles = row![
            radio("Leader", Role::Leader, Some(self.settings.role), Message::RoleSelected)
                .size(14)
                .text_size(13)
                .spacing(5),
            radio(
                "Follower",
                Role::Follower,
                Some(self.settings.role),
                Message::RoleSelected
            )
            .size(14)
            .text_size(13)
            .spacing(5),
        ]
        .spacing(14);

        let tc = self
            .timecode
            .clone()
            .unwrap_or_else(|| "--:--:--:--".into());

        let detail = match self.status.dispersion_s {
            Some(d) => format!("{} · ±{:.2} ms", self.status.source, d * 1e3),
            None => self.status.source.clone(),
        };

        column![
            roles,
            row![
                status_dot(self.status.synced),
                text(tc).size(15).font(iced::Font::MONOSPACE),
                text(format!("· {}", self.status.label)).size(12).color(DIM),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
            text(detail).size(11).color(DIM),
        ]
        .align_x(Alignment::End)
        .spacing(3)
        .into()
    }

    fn permission_panel(&self) -> Element<'_, Message> {
        container(
            column![
                text("Microphone access is blocked").size(14),
                text(self.permission.advice()).size(12).color(DIM),
                button(text("Open privacy settings").size(12))
                    .on_press(Message::OpenPrivacySettings),
            ]
            .spacing(6),
        )
        .padding(10)
        .width(Fill)
        .into()
    }

    fn device_row(&self) -> Element<'_, Message> {
        let format = match &self.monitor {
            Some(m) => {
                let rate = if m.negotiated.native_target_rate {
                    format!("{} Hz", m.negotiated.rate)
                } else {
                    format!("{} Hz → 48000 on save", m.negotiated.rate)
                };
                format!("{} ch · {rate}", m.negotiated.channels)
            }
            None => "no input".into(),
        };

        column![
            row![
                text("Input").size(13).width(Length::Fixed(LABEL_W)),
                pick_list(
                    self.devices.clone(),
                    self.selected.clone(),
                    Message::DeviceSelected
                )
                .placeholder("no input devices")
                .width(Fill),
                button(text("Rescan").size(12)).on_press(Message::RefreshDevices),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            row![
                Space::new().width(Length::Fixed(LABEL_W + 8.0)),
                text(format).size(11).color(DIM),
            ],
        ]
        .spacing(4)
        .into()
    }

    fn meter_panel(&self) -> Element<'_, Message> {
        let channels = self
            .monitor
            .as_ref()
            .map(|m| m.meters.channels())
            .unwrap_or(1);
        let data = meter::MeterData::new(&self.peaks, &self.rms, &self.clips);
        let any_clip = self.clips.iter().any(|c| *c);

        let mut stack = column![
            meter::meter(data)
                .width(Fill)
                .height(Length::Fixed(meter::preferred_height(channels)))
        ]
        .spacing(4);

        // The clip latch only appears once there is something to clear, rather than
        // occupying a permanently dead button.
        if any_clip {
            stack = stack.push(row![
                Space::new().width(Fill),
                button(text("Clear clip").size(11)).on_press(Message::ClearClip),
            ]);
        }
        stack.into()
    }

    fn destination_rows(&self) -> Element<'_, Message> {
        let dir = row![
            text(self.output_dir.display().to_string()).size(12),
            Space::new().width(Fill),
            button(text("Browse…").size(12)).on_press(Message::BrowseDir),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let name = row![
            text_input("rec", &self.base_name)
                .on_input(Message::BaseNameChanged)
                .size(13)
                .width(Length::Fixed(160.0)),
            text(format!("next: {}", self.next_preview))
                .size(12)
                .color(DIM),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        column![
            labelled("Folder", dir.into()),
            labelled("Name", name.into()),
        ]
        .spacing(8)
        .into()
    }

    fn transport(&self) -> Element<'_, Message> {
        let recording = matches!(self.stage, Stage::Recording { .. });
        let finalizing = matches!(self.stage, Stage::Finalizing { .. });
        let ready = self.clock_ready();

        let elapsed = match &self.stage {
            Stage::Recording { recording, .. } => hms(recording.progress.elapsed(recording.rate)),
            _ => "00:00:00".into(),
        };

        // Recording is never blocked on the clock: missing the moment is worse than
        // an uncorrected take. The button says what you will get instead.
        //
        // A follower is the exception, and not because of the clock: its transport
        // belongs to the leader, so the button is a lamp rather than a control.
        let (label, enabled) = if finalizing {
            ("Finalising…", false)
        } else if self.slaved() {
            match (recording, self.rolling) {
                (true, _) => ("Recording", false),
                (false, Some(_)) => ("Waiting for leader", false),
                (false, None) => ("No leader", false),
            }
        } else if recording {
            ("Stop", true)
        } else if self.monitor.is_none() {
            ("No input", false)
        } else if ready {
            ("Record", true)
        } else {
            ("Syncing…", true)
        };

        let record = button(text(label).size(15))
            .padding([11, 26])
            .style(record_style(ready || recording, enabled))
            .on_press_maybe(enabled.then_some(Message::ToggleRecord));

        // One short line, and it lives on its own row. Sharing a row with the
        // transport meant a long message shoved the record button off the window.
        let hint = (!recording && !finalizing)
            .then(|| self.status.waiting_for())
            .flatten();
        let status: Element<'_, Message> = match (&self.error, hint) {
            (Some(e), _) => text(e.as_str()).size(11).color(ERROR).into(),
            (None, Some(hint)) => text(hint).size(11).color(WARN).into(),
            _ => match &self.last_result {
                Some(msg) => text(msg.as_str()).size(11).color(DIM).into(),
                None => Space::new().height(Length::Fixed(13.0)).into(),
            },
        };

        column![
            status,
            row![
                Space::new().width(Fill),
                text(elapsed).size(30),
                Space::new().width(Length::Fixed(16.0)),
                record,
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(6)
        .into()
    }

    // -----------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------

    fn settings_view(&self) -> Element<'_, Message> {
        let header = row![
            text("Settings").size(22),
            Space::new().width(Fill),
            button(text("Done").size(12)).on_press(Message::ShowRecorder),
        ]
        .align_y(Alignment::Center);

        let body = column![
            self.source_section(),
            rule::horizontal(1),
            self.ntp_section(),
            rule::horizontal(1),
            self.ethersync_section(),
            rule::horizontal(1),
            self.input_section(),
        ]
        .spacing(16);

        container(
            column![header, rule::horizontal(1), scrollable(body).height(Fill)].spacing(12),
        )
        .padding(18)
        .width(Fill)
        .height(Fill)
        .into()
    }

    fn source_section(&self) -> Element<'_, Message> {
        let mut options = column![].spacing(6);
        for source in TimeSource::ALL {
            options = options.push(
                column![
                    radio(
                        source.label(),
                        source,
                        Some(self.settings.source),
                        Message::SourceSelected
                    )
                    .size(15)
                    .text_size(14),
                    row![
                        Space::new().width(Length::Fixed(24.0)),
                        text(source.blurb()).size(11).color(DIM),
                    ],
                ]
                .spacing(2),
            );
        }
        section("Time source", options.into())
    }

    fn ntp_section(&self) -> Element<'_, Message> {
        let row = row![
            text_input("pool.ntp.org", &self.settings.ntp_server)
                .on_input(Message::NtpServerChanged)
                .on_submit(Message::NtpServerCommitted)
                .size(13)
                .width(Length::Fixed(220.0)),
            button(text("Apply").size(12)).on_press(Message::NtpServerCommitted),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        section(
            "NTP server",
            column![
                row,
                text(
                    "Polled every 16 s. A server on the local network is worth an \
                     order of magnitude over a public pool."
                )
                .size(11)
                .color(DIM),
            ]
            .spacing(6)
            .into(),
        )
    }

    fn ethersync_section(&self) -> Element<'_, Message> {
        let roles = row![
            radio("Leader", Role::Leader, Some(self.settings.role), Message::RoleSelected)
                .size(15)
                .text_size(14),
            radio(
                "Follower",
                Role::Follower,
                Some(self.settings.role),
                Message::RoleSelected
            )
            .size(15)
            .text_size(14),
        ]
        .spacing(20);

        let rate = row![
            text("Frame rate").size(13).width(Length::Fixed(90.0)),
            pick_list(Fps::ALL.to_vec(), Some(self.settings.fps), Message::FpsSelected)
                .text_size(13)
                .width(Length::Fixed(110.0)),
            checkbox(self.settings.drop_frame)
                .label("Drop frame")
                .text_size(13)
                .size(15)
                .on_toggle_maybe(
                    self.settings
                        .fps
                        .supports_drop_frame()
                        .then_some(Message::DropFrameToggled)
                ),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        let body: Element<'_, Message> = match self.settings.role {
            Role::Leader => self.leader_settings(),
            Role::Follower => self.follower_settings(),
        };

        section(
            "Ethersync",
            column![
                roles,
                rate,
                text(
                    "Timecode is the time of day. The leader's transport is the \
                     record button for the whole rig."
                )
                .size(11)
                .color(DIM),
                rule::horizontal(1),
                body,
            ]
            .spacing(10)
            .into(),
        )
    }

    fn leader_settings(&self) -> Element<'_, Message> {
        let fields = row![
            text("Name").size(13).width(Length::Fixed(90.0)),
            text_input("syncrec", &self.settings.leader_name)
                .on_input(Message::LeaderNameChanged)
                .on_submit(Message::LinkSettingsCommitted)
                .size(13)
                .width(Length::Fixed(160.0)),
            text("Port").size(13),
            text_input("4443", &self.port_text)
                .on_input(Message::LeaderPortChanged)
                .on_submit(Message::LinkSettingsCommitted)
                .size(13)
                .width(Length::Fixed(70.0)),
            button(text("Apply").size(12)).on_press(Message::LinkSettingsCommitted),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        // The bound address is the IPv4 wildcard, so that one leader serves
        // Ethernet and Wi-Fi at once. `0.0.0.0:4443` is not something an operator
        // can type into another machine, so show the interfaces instead.
        let listening = match self.link.as_ref().and_then(|l| l.fingerprint()) {
            Some(fp) => format!("SHA-256 {fp}"),
            None => "not listening".into(),
        };
        let reachable = if self.endpoints.is_empty() {
            "no reachable address yet".to_string()
        } else {
            let list = self
                .endpoints
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join("  ");
            format!("followers can use  {list}")
        };

        column![
            fields,
            text(reachable).size(11),
            text(listening).size(11).color(DIM),
            text(
                "Advertised over mDNS, so followers on the same network find this \
                 machine without being told an address at all."
            )
            .size(11)
            .color(DIM),
        ]
        .spacing(6)
        .into()
    }

    fn follower_settings(&self) -> Element<'_, Message> {
        let fields = row![
            text("Leader").size(13).width(Length::Fixed(90.0)),
            text_input("(discover automatically)", &self.settings.follower_address)
                .on_input(Message::FollowerAddressChanged)
                .on_submit(Message::LinkSettingsCommitted)
                .size(13)
                .width(Length::Fixed(220.0)),
            button(text("Apply").size(12)).on_press(Message::LinkSettingsCommitted),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let mut found = column![].spacing(4);
        if self.settings.follower_socket().is_none() {
            if self.discovered.is_empty() {
                found = found.push(text("browsing for leaders…").size(11).color(DIM));
            }
            for leader in &self.discovered {
                let Some(address) = leader
                    .addresses
                    .iter()
                    .find(|a| a.is_ipv4())
                    .or_else(|| leader.addresses.first())
                else {
                    continue;
                };
                found = found.push(
                    row![
                        text(format!("{} · {address}", leader.name)).size(12),
                        Space::new().width(Fill),
                        button(text("Use").size(11))
                            .on_press(Message::UseDiscovered(address.to_string())),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center),
                );
            }
        }

        let connected = match self.link.as_ref().and_then(|l| l.address()) {
            Some(addr) => format!("locked to {addr} · {}", self.status.label),
            None => "no leader yet".into(),
        };

        column![
            fields,
            text(connected).size(11).color(DIM),
            found,
            text(
                "Leave the address empty to take the first leader discovered on \
                 the network. Recording follows the leader; this machine's record \
                 button is disabled."
            )
            .size(11)
            .color(DIM),
        ]
        .spacing(6)
        .into()
    }

    fn input_section(&self) -> Element<'_, Message> {
        let platform = InputLatency::query(self.selected.as_ref().map(|d| d.name.as_str()));
        let trim = row![
            text("Trim").size(13).width(Length::Fixed(90.0)),
            text_input("0.0", &self.trim_text)
                .on_input(Message::TrimChanged)
                .size(13)
                .width(Length::Fixed(70.0)),
            text("ms").size(12).color(DIM),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        section(
            "Input latency",
            column![
                trim,
                text(platform.describe()).size(11).color(DIM),
                text(
                    "Added to the platform figure and subtracted from every \
                     timestamp. Positive pulls the take earlier."
                )
                .size(11)
                .color(DIM),
            ]
            .spacing(6)
            .into(),
        )
    }
}

const LABEL_W: f32 = 64.0;

/// A titled block on the settings page.
fn section<'a>(title: &'a str, body: Element<'a, Message>) -> Element<'a, Message> {
    column![text(title).size(15), body].spacing(8).into()
}

/// Diameter of the sync indicator.
const DOT: f32 = 9.0;
const SYNC_OK: Color = Color::from_rgb(0.26, 0.78, 0.42);

/// A filled circle showing at a glance whether the clock can timestamp a take.
///
/// Green uses the same condition as the record button, so the two can never
/// disagree; anything short of a usable fix is amber rather than red, because an
/// unsynced clock is a "wait a moment", not a fault.
fn status_dot<'a>(ok: bool) -> Element<'a, Message> {
    let colour = if ok { SYNC_OK } else { WARN };
    container(Space::new().width(DOT).height(DOT))
        .style(move |_theme| container::Style {
            background: Some(colour.into()),
            border: Border {
                // A radius of half the box turns the square into a circle.
                radius: (DOT / 2.0).into(),
                ..Border::default()
            },
            ..container::Style::default()
        })
        .into()
}

/// Secondary text. Dimmer than the body so the numbers that matter stand out.
const DIM: Color = Color::from_rgb(0.62, 0.64, 0.68);
const WARN: Color = Color::from_rgb(0.90, 0.72, 0.30);
const ERROR: Color = Color::from_rgb(0.93, 0.44, 0.40);

/// Bright red once the clock can actually timestamp a take, muted before that.
///
/// Hardcoded rather than taken from the theme palette: a record button has to read
/// as *red* in any theme, and the operator needs to tell "armed" from "not yet"
/// across the room without reading the label.
fn record_style(ready: bool, enabled: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let base = if !enabled {
            Color::from_rgb(0.28, 0.28, 0.30)
        } else if ready {
            Color::from_rgb(0.86, 0.16, 0.16)
        } else {
            // Desaturated red: recognisably the record button, visibly not armed.
            Color::from_rgb(0.44, 0.31, 0.32)
        };
        let background = match status {
            button::Status::Hovered | button::Status::Pressed => lighten(base, 0.12),
            _ => base,
        };
        button::Style {
            background: Some(background.into()),
            text_color: if enabled {
                Color::WHITE
            } else {
                Color::from_rgb(0.6, 0.6, 0.62)
            },
            border: Border {
                radius: 6.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

fn lighten(c: Color, amount: f32) -> Color {
    Color {
        r: (c.r + amount).min(1.0),
        g: (c.g + amount).min(1.0),
        b: (c.b + amount).min(1.0),
        a: c.a,
    }
}

fn labelled<'a>(label: &'a str, content: Element<'a, Message>) -> Element<'a, Message> {
    row![
        text(label).size(13).width(Length::Fixed(LABEL_W)),
        content,
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

/// A reference that is not there yet. Never `synced`, so nothing downstream can
/// mistake "we have not started looking" for "the clock is fine".
fn offline_status(label: &str) -> RefStatus {
    RefStatus {
        kind: "clock",
        source: "no clock".into(),
        synced: false,
        label: label.into(),
        samples: None,
        discarded: None,
        dispersion_s: None,
        slope_ppm: None,
    }
}

fn format_trim(ms: f64) -> String {
    format!("{ms:.1}")
}

fn file_name(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn hms(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Where takes land by default.
fn dirs_audio_fallback() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join("Music").join("syncrec"))
        .unwrap_or_else(std::env::temp_dir)
}

/// Turn a finished take into the file that ships.
fn finalize_take(take: &session::TakeResult, trim_ms: f64) -> anyhow::Result<Outcome> {
    let sidecar: &Sidecar = &take.sidecar;
    let paths: &TakePaths = &take.paths;

    // Without an anchor we still have to write something, but it must be visibly
    // untrusted rather than quietly wrong.
    let (t0, sync_state) = match sidecar.t0_unix_nanos {
        Some(t0) => (t0, sidecar.sync_state.clone()),
        None => (
            crate::clock::unix_nanos(std::time::SystemTime::now())
                - (sidecar.raw_frames as i128 * 1_000_000_000
                    / sidecar.device_rate.max(1) as i128),
            "unsynced".to_string(),
        ),
    };

    // A timecode stamp only means anything when the anchor it labels is real, so
    // it rides on `t0` having resolved rather than on the reference having a rate.
    let timecode = sidecar
        .t0_unix_nanos
        .and(take.timecode)
        .zip(sidecar.start_timecode.clone())
        .map(|(format, start)| TimecodeStamp { format, start });

    let provenance = Provenance {
        t0_unix_nanos: t0,
        sample_rate: audio::TARGET_RATE,
        channels: sidecar.channels,
        bits_per_sample: 24,
        device_name: sidecar.device_name.clone(),
        device_rate: sidecar.device_rate,
        measured_rate: None,
        drift_ratio: None,
        resampled: false,
        clock_source: sidecar.clock_source.clone(),
        clock_dispersion_s: sidecar.clock_dispersion_s,
        sync_state,
        slope_ppm: sidecar.clock_slope_ppm,
        latency_offset_ms: trim_ms,
        timecode,
    };

    // Correction is not optional. Whether it is actually applied is the safety
    // gate's decision, made from the measurements, not a switch in the window.
    let outcome = finalize::finalize(
        &paths.raw,
        &paths.final_wav,
        &sidecar.observations,
        &take.status,
        &provenance,
    )?;

    // The Broadcast Wave file already carries t0, the measured rate, the drift
    // ratio, the clock source and the dispersion in its CodingHistory and iXML. The
    // sidecar adds only the raw per-observation rows the fit was derived from, so
    // on a clean take it is redundant and a successful take should leave exactly
    // one file. When the gate fails it is the evidence for why, and it stays put
    // alongside the scratch capture.
    if outcome.corrected() {
        let _ = std::fs::remove_file(&paths.sidecar);
    }

    Ok(outcome)
}

/// The post-take status line.
///
/// Deliberately terse. Why a take was not corrected is recorded in the file's
/// `CodingHistory` and in the sidecar; spelling every unmet gate condition out in
/// the window just buried the transport controls.
fn describe_outcome(file: &str, o: &Outcome) -> String {
    match (o.resampled, o.gate.short_reason()) {
        (true, _) => format!("saved {file}"),
        (false, Some(why)) => format!("saved {file} · {why}"),
        (false, None) => format!("saved {file} · uncorrected"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hms_formats_a_long_take() {
        assert_eq!(hms(Duration::from_secs(0)), "00:00:00");
        assert_eq!(hms(Duration::from_secs(59)), "00:00:59");
        assert_eq!(hms(Duration::from_secs(61)), "00:01:01");
        assert_eq!(hms(Duration::from_secs(3661)), "01:01:01");
        assert_eq!(hms(Duration::from_secs(36_000)), "10:00:00");
    }

    #[test]
    fn file_name_survives_a_bare_path() {
        assert_eq!(
            file_name(std::path::Path::new("/tmp/rec-3.wav")),
            "rec-3.wav"
        );
        assert_eq!(file_name(std::path::Path::new("")), "");
    }

    #[test]
    fn an_absent_reference_never_reads_as_ready() {
        let s = offline_status("no link");
        assert!(!s.synced);
        assert!(!s.ready());
    }
}
