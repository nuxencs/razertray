use crate::config::{self, AppConfig};
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

pub fn run_once() -> Result<()> {
    let cfg = config::load_or_create_config()?;
    init_logging(&cfg);

    let mut cache = config::load_or_create_pid_cache()?;
    let api = HidApi::new().context("failed to initialize hidapi")?;

    let result = client::poll_devices(&api, &mut cache);
    config::save_pid_cache(&cache)?;

    if result.devices.is_empty() {
        println!("No supported Razer devices found.");
    } else {
        for dev in &result.devices {
            let charging = if dev.is_charging {
                "charging"
            } else {
                "not-charging"
            };
            println!(
                "{} pid=0x{:04X} battery={}% {}",
                dev.display_name, dev.pid, dev.battery_percent, charging
            );
        }
    }

    if !result.errors.is_empty() {
        eprintln!("Errors:");
        for err in result.errors {
            eprintln!("- {err}");
        }
    }

    Ok(())
}

#[cfg(target_os = "windows")]
pub fn run_tray() -> Result<()> {
    let cfg = config::load_or_create_config()?;
    init_logging(&cfg);
    crate::tray::run_tray_app(cfg)
}

#[cfg(not(target_os = "windows"))]
pub fn run_tray() -> Result<()> {
    anyhow::bail!("tray mode is only supported on Windows")
}

fn init_logging(cfg: &AppConfig) {
    let filter =
        EnvFilter::try_new(cfg.log_level.clone()).unwrap_or_else(|_| EnvFilter::new("info"));
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
