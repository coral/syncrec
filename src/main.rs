//! syncrec — an audio recorder whose timestamps come from NTP, not the OS clock.

// Do not pop a console window behind the UI on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> iced::Result {
    syncrec::ui::run()
}
