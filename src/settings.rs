//! Operator settings, and where they live between launches.
//!
//! These are the choices that belong to the rig rather than to the take: which
//! clock to trust, which frame rate the job is shot at, which machine is the
//! leader. They are deliberately not on the recording window — an operator looking
//! at a meter should not be one stray click away from changing the time source
//! mid-shoot — and they are deliberately persisted, because a rig is configured
//! once and then used all day.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ethersync::{Fps, Role};

/// Where a take's timestamps come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TimeSource {
    /// Our own SNTP client, fitted against the monotonic clock.
    #[default]
    Ntp,
    /// A timecode timeline shared with the rest of the rig over the LAN.
    Ethersync,
}

impl TimeSource {
    pub const ALL: [TimeSource; 2] = [TimeSource::Ntp, TimeSource::Ethersync];

    pub fn label(self) -> &'static str {
        match self {
            TimeSource::Ntp => "NTP",
            TimeSource::Ethersync => "Ethersync",
        }
    }

    /// One line on what choosing this actually buys.
    pub fn blurb(self) -> &'static str {
        match self {
            TimeSource::Ntp => {
                "Absolute UTC from a time server. Each recorder is independently \
                 right, to within its own network path."
            }
            TimeSource::Ethersync => {
                "LAN timecode from a leader. Recorders agree with each other \
                 exactly, and the leader's transport drives everyone's record button."
            }
        }
    }
}

/// Everything on the settings page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub source: TimeSource,

    /// The NTP server to poll. Only consulted in [`TimeSource::Ntp`].
    pub ntp_server: String,

    /// Input latency still to remove beyond the platform figure, in milliseconds.
    /// Signed: a manual trim may legitimately over-correct the platform's number.
    pub trim_ms: f64,

    pub role: Role,
    pub fps: Fps,
    pub drop_frame: bool,

    /// How this machine advertises itself when it is the leader.
    pub leader_name: String,
    pub leader_port: u16,

    /// `host:port` of the leader to follow. Empty means take the first leader
    /// discovered on the LAN, which is the right answer on a rig with one.
    pub follower_address: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            source: TimeSource::Ntp,
            ntp_server: "pool.ntp.org".into(),
            trim_ms: 0.0,
            role: Role::Leader,
            fps: Fps::default(),
            drop_frame: false,
            leader_name: "syncrec".into(),
            leader_port: 4443,
            follower_address: String::new(),
        }
    }
}

impl Settings {
    /// Load the saved settings, or the defaults.
    ///
    /// A corrupt or unreadable file is not an error worth stopping for: the
    /// defaults record perfectly well, and refusing to launch because a
    /// preferences file has a stray brace in it would be absurd.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = Self::path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    fn path() -> Option<PathBuf> {
        let dir = if cfg!(windows) {
            std::env::var_os("APPDATA").map(PathBuf::from)
        } else if cfg!(target_os = "macos") {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|h| h.join("Library").join("Application Support"))
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .map(|h| h.join(".config"))
                })
        }?;
        Some(dir.join("syncrec").join("settings.json"))
    }

    /// The address to follow, when one has been typed. `None` asks for discovery.
    pub fn follower_socket(&self) -> Option<std::net::SocketAddr> {
        let text = self.follower_address.trim();
        if text.is_empty() {
            return None;
        }
        use std::net::ToSocketAddrs;
        text.to_socket_addrs().ok()?.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_record_against_ntp_as_before() {
        let s = Settings::default();
        assert_eq!(s.source, TimeSource::Ntp);
        assert_eq!(s.ntp_server, "pool.ntp.org");
        assert_eq!(s.trim_ms, 0.0);
    }

    #[test]
    fn settings_round_trip_through_json() {
        let s = Settings {
            source: TimeSource::Ethersync,
            role: Role::Follower,
            fps: Fps::F29_97,
            drop_frame: true,
            leader_port: 5000,
            follower_address: "10.0.0.4:4443".into(),
            trim_ms: -1.5,
            ..Settings::default()
        };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn a_file_from_an_older_build_keeps_its_known_fields() {
        // `#[serde(default)]` is what makes adding a setting a non-event for
        // anyone who already has a preferences file.
        let old = r#"{"source":"Ethersync","ntp_server":"time.apple.com"}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.source, TimeSource::Ethersync);
        assert_eq!(s.ntp_server, "time.apple.com");
        assert_eq!(s.fps, Fps::default(), "an absent setting takes its default");
    }

    #[test]
    fn nonsense_in_the_file_does_not_stop_the_recorder() {
        let s: Settings = serde_json::from_str("not json").unwrap_or_default();
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn an_empty_follower_address_asks_for_discovery() {
        let mut s = Settings::default();
        assert_eq!(s.follower_socket(), None);
        s.follower_address = "   ".into();
        assert_eq!(s.follower_socket(), None);
        s.follower_address = "127.0.0.1:4443".into();
        assert_eq!(
            s.follower_socket(),
            Some("127.0.0.1:4443".parse().unwrap())
        );
    }

    #[test]
    fn an_unparseable_address_is_not_a_crash() {
        let s = Settings {
            follower_address: "this is not an address".into(),
            ..Settings::default()
        };
        assert_eq!(s.follower_socket(), None);
    }
}
