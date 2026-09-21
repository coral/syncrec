//! syncrec — an audio recorder whose timestamps come from a clock it can vouch for:
//! its own SNTP model, or an ethersync timecode leader on the LAN. Never the OS.

pub mod audio;
pub mod bwf;
pub mod clock;
pub mod ethersync;
pub mod finalize;
pub mod latency;
pub mod permission;
pub mod settings;
pub mod ui;
