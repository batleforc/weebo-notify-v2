//! Desktop entry point.
//!
//! Everything the app actually does lives in `lib.rs` — this file exists only
//! because Tauri's desktop target needs a `fn main`, where mobile targets call
//! `lib.rs::run` directly via `tauri::mobile_entry_point` (see `lib.rs`'s module
//! doc for why the crate is split this way, and what is and is not implemented
//! for mobile as of this split — M6, design §10bis.4, §14.4 D33).

// Windows: no console window behind the app in a release build.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    weebo_notify_desktop::run();
}
