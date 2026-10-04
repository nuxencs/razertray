//! Windows tray app that shows the battery level of wireless Razer mice.
//!
//! The binary calls [`run_tray`] for the tray app and [`run_once`] for the
//! `--once` command-line check. Everything else is internal.

/// Name used for the app data folder, log file, Run key and toast sender.
const APP_ID: &str = "razertray";

mod app;
mod config;
mod device_map;
mod hid;
mod model;
#[cfg_attr(
    not(windows),
    expect(
        dead_code,
        reason = "tray mode runs only on Windows; it still compiles elsewhere so its tests run"
    )
)]
mod tray;

pub use app::{run_once, run_tray};
