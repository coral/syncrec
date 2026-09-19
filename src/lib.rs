//! syncrec — an audio recorder whose timestamps come from NTP rather than the OS clock.

pub mod audio;
pub mod bwf;
pub mod clock;
pub mod finalize;
pub mod latency;
pub mod permission;
pub mod ui;
