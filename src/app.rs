use crate::config::{self, AppConfig, Loaded, PidCache};
use crate::hid::client;
use anyhow::{Context, Result};
use hidapi::HidApi;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriter;

const LOG_FILES_TO_KEEP: usize = 3;
const MAX_LOG_FILE_BYTES: u64 = 1_048_576;

/// Prints one battery reading per connected Razer device, then returns.
///
/// # Errors
///
/// Returns an error when hidapi cannot start or the pid cache cannot be saved.
/// Devices that fail to answer are listed on stderr and are not an error.
pub fn run_once() -> Result<()> {
    let loaded = load_config();
    if let Some(problem) = &loaded.problem {
        eprintln!("warning: {problem:#}");
    }
    let mut cache = load_pid_cache();
    let api = HidApi::new().context("failed to initialize hidapi")?;

    let result = client::poll_devices(&api, &mut cache);
    cache.save()?;

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
        tracing::warn!("{problem:#}");
    }
    loaded.value
}

/// Loads the config and starts logging, then logs any config problem.
///
/// Never fails: a broken config must not stop the tray app, which has no
/// console to report the error on.
fn load_config() -> Loaded<AppConfig> {
    let loaded = AppConfig::load();
    init_logging(&loaded.value);
    if let Some(problem) = &loaded.problem {
        tracing::warn!("{problem:#}");
    }
    loaded
}

fn init_logging(cfg: &AppConfig) {
    let (filter, filter_err) = match EnvFilter::try_new(&cfg.log_level) {
        Ok(filter) => (filter, None),
        Err(err) => (EnvFilter::new("info"), Some(err)),
    };
    let log_path = config::log_path();
    if let Some(parent) = log_path.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        eprintln!("failed creating log dir {}: {err}", parent.display());
    }

    let can_open_log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .is_ok();

    if can_open_log {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_thread_ids(true)
            .with_ansi(false)
            .with_writer(LogFileWriter {
                path: log_path.clone(),
            })
            .try_init();
        tracing::info!("logging to {}", log_path.display());
    } else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_thread_ids(true)
            .try_init();
        tracing::warn!(
            "failed to open log file {}, using stderr",
            log_path.display()
        );
    }

    if let Some(err) = filter_err {
        tracing::warn!(
            "invalid log_level {:?}, using \"info\": {err}",
            cfg.log_level
        );
    }
}

#[derive(Clone, Debug)]
struct LogFileWriter {
    path: PathBuf,
}

impl<'a> MakeWriter<'a> for LogFileWriter {
    type Writer = Box<dyn Write + Send + 'a>;

    fn make_writer(&'a self) -> Self::Writer {
        let _ = maybe_rotate_logs_by_size(&self.path, MAX_LOG_FILE_BYTES, LOG_FILES_TO_KEEP);
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => Box::new(file),
            Err(_) => Box::new(io::sink()),
        }
    }
}

fn maybe_rotate_logs_by_size(base_path: &Path, max_bytes: u64, keep_total: usize) -> Result<()> {
    let size = match fs::metadata(base_path) {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed reading metadata for {}", base_path.display()));
        }
    };

    if size < max_bytes {
        return Ok(());
    }

    rotate_log_files(base_path, keep_total)
}

fn rotate_log_files(base_path: &Path, keep_total: usize) -> Result<()> {
    if keep_total <= 1 {
        return Ok(());
    }

    let archive_max = keep_total - 1;
    if archive_max >= 1 {
        let oldest = rotated_log_path(base_path, archive_max)?;
        if oldest.exists() {
            fs::remove_file(&oldest)
                .with_context(|| format!("failed removing {}", oldest.display()))?;
        }

        for idx in (1..archive_max).rev() {
            let src = rotated_log_path(base_path, idx)?;
            if src.exists() {
                let dst = rotated_log_path(base_path, idx + 1)?;
                fs::rename(&src, &dst).with_context(|| {
                    format!("failed rotating {} to {}", src.display(), dst.display())
                })?;
            }
        }
    }

    if base_path.exists() {
        let dst = rotated_log_path(base_path, 1)?;
        fs::rename(base_path, &dst).with_context(|| {
            format!(
                "failed rotating {} to {}",
                base_path.display(),
                dst.display()
            )
        })?;
    }

    Ok(())
}

fn rotated_log_path(base_path: &Path, index: usize) -> Result<PathBuf> {
    let parent = base_path
        .parent()
        .with_context(|| format!("missing parent directory for {}", base_path.display()))?;
    let file_name = base_path
        .file_name()
        .with_context(|| format!("missing file name for {}", base_path.display()))?
        .to_string_lossy();
    Ok(parent.join(format!("{file_name}.{index}")))
}
