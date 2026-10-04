//! Entry points: the `--once` command-line check and the tray app.

use crate::config::{AppConfig, Loaded, PidCache};
use crate::hid::client;
use crate::logging;
use anyhow::{Context, Result};
use hidapi::HidApi;
use tracing::{Level, event};

/// Prints one battery reading per connected Razer device, then returns.
///
/// # Errors
///
/// Returns an error when hidapi cannot start or a new pid cache entry cannot be saved.
/// Devices that fail to answer are listed on stderr and are not an error.
pub fn run_once() -> Result<()> {
    let loaded = load_config();
    if let Some(problem) = &loaded.problem {
        eprintln!("warning: {problem:#}");
    }
    let mut cache = load_pid_cache();
    let api = HidApi::new().context("failed to initialize hidapi")?;

    let before = cache.clone();
    let result = client::poll_devices(&api, &mut cache);
    if cache != before {
        cache.save()?;
    }

    if result.devices.is_empty() {
        println!("No supported Razer devices found.");
    }
    for dev in &result.devices {
        let charging = if dev.charging {
            "charging"
        } else {
            "not-charging"
        };
        println!(
            "{} pid=0x{:04X} battery={} {charging}",
            dev.name, dev.pid, dev.percent
        );
    }

    if !result.failures.is_empty() {
        eprintln!("Errors:");
        for failure in &result.failures {
            eprintln!("- {failure}");
        }
    }

    Ok(())
}

/// Runs the tray app until the user picks "Exit". Windows only.
///
/// # Errors
///
/// Returns an error when the tray icon or menu cannot be created, and always
/// on other platforms.
#[cfg(target_os = "windows")]
pub fn run_tray() -> Result<()> {
    crate::tray::run(load_config().value)
}

/// Runs the tray app until the user picks "Exit". Windows only.
///
/// # Errors
///
/// Always returns an error on this platform.
#[cfg(not(target_os = "windows"))]
pub fn run_tray() -> Result<()> {
    anyhow::bail!("tray mode is only supported on Windows")
}

/// Loads the pid cache. A problem with the file is logged; the cache is rebuilt by probing.
pub(crate) fn load_pid_cache() -> PidCache {
    let loaded = PidCache::load();
    if let Some(problem) = &loaded.problem {
        event!(
            name: "pid_cache.load.failure",
            Level::WARN,
            exception.message = %format_args!("{problem:#}"),
            "pid cache not usable: {{exception.message}}"
        );
    }
    loaded.value
}

/// Loads the config and starts logging, then logs any config problem.
///
/// Never fails: a broken config must not stop the tray app, which has no
/// console to report the error on.
fn load_config() -> Loaded<AppConfig> {
    let loaded = AppConfig::load();
    logging::init(&loaded.value);
    if let Some(problem) = &loaded.problem {
        event!(
            name: "config.load.failure",
            Level::WARN,
            exception.message = %format_args!("{problem:#}"),
            "config not usable: {{exception.message}}",
        );
    }
    loaded
}
