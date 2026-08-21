use crate::config::{self, AppConfig, PidCacheLoadState};
use crate::hid::{client::PollBatch, worker};
use crate::model::{PollError, PollResult};
use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriter;

const LOG_FILES_TO_KEEP: usize = 3;
const MAX_LOG_FILE_BYTES: u64 = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnceOutput {
    Human,
    Json,
    Diagnose,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnceStatus {
    Success,
    NoDevice,
    PartialFailure,
}

#[doc(hidden)]
pub fn run_hid_worker() -> Result<()> {
    worker::run()
}

pub fn run_once(output: OnceOutput) -> Result<OnceStatus> {
    let loaded = config::load_or_create_config()?;
    let cfg = loaded.config;
    init_logging(&cfg);
    if let Some(recovery) = loaded.recovery {
        eprintln!("Warning: {}", recovery.diagnostic_message());
    }

    let config::PidCacheLoad {
        mut cache,
        state: cache_state,
    } = config::load_pid_cache_for_polling();
    let batch = poll_batch_or_diagnostic(worker::poll(&mut cache));
    let mut result = batch.result;
    finish_cache_poll(&mut result, batch.cache_changed, cache_state, || {
        config::save_pid_cache(&cache)
    });

    if output == OnceOutput::Json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if result.devices.is_empty() {
        if result.errors.is_empty() {
            println!("No supported Razer devices found.");
        } else {
            println!("No Razer battery reading was available.");
        }
    } else {
        for dev in &result.devices {
            let charging = match dev.charge_state {
                crate::model::ChargeState::Charging => "charging",
                crate::model::ChargeState::NotCharging => "not-charging",
                crate::model::ChargeState::Unavailable => "charging-unknown",
                crate::model::ChargeState::Unsupported => "charging-unsupported",
            };
            println!(
                "{} pid=0x{:04X} battery={}% {}{}",
                dev.display_name,
                dev.pid,
                dev.battery_percent,
                charging,
                if output == OnceOutput::Diagnose {
                    format!(
                        " key={} raw={} transaction-cache={:?}",
                        dev.device_key,
                        dev.battery_raw,
                        cache.get(dev.pid)
                    )
                } else {
                    String::new()
                }
            );
        }
    }

    if output != OnceOutput::Json && !result.errors.is_empty() {
        write_poll_errors(io::stderr().lock(), &result.errors, output)?;
    }

    Ok(once_status(&result))
}

fn record_cache_save_result(result: &mut PollResult, save_result: Result<()>) {
    let Err(error) = save_result else {
        return;
    };
    let message = format!("failed saving PID cache: {error:#}");
    result.errors.push(PollError::pid_cache(message));
}

fn finish_cache_poll(
    result: &mut PollResult,
    cache_changed: bool,
    load_state: PidCacheLoadState,
    save: impl FnOnce() -> Result<()>,
) {
    match load_state {
        PidCacheLoadState::Available if cache_changed => record_cache_save_result(result, save()),
        PidCacheLoadState::Available => {}
        PidCacheLoadState::Unavailable(diagnostic) => result.errors.push(diagnostic),
    }
}

fn write_poll_errors(
    mut writer: impl Write,
    errors: &[PollError],
    output: OnceOutput,
) -> io::Result<()> {
    writeln!(writer, "Errors:")?;
    for error in errors {
        if output == OnceOutput::Diagnose {
            writeln!(
                writer,
                "- {} ({:04X}) key={} scope={} kind={}: {}",
                error.display_name,
                error.pid,
                error.device_key,
                error.scope,
                error.kind,
                error.message
            )?;
        } else {
            writeln!(
                writer,
                "- {} ({:04X}): {}",
                error.display_name, error.pid, error.message
            )?;
        }
    }
    Ok(())
}

fn once_status(result: &crate::model::PollResult) -> OnceStatus {
    if !result.errors.is_empty() {
        OnceStatus::PartialFailure
    } else if result.devices.is_empty() {
        OnceStatus::NoDevice
    } else {
        OnceStatus::Success
    }
}

fn poll_batch_or_diagnostic(result: Result<PollBatch>) -> PollBatch {
    result.unwrap_or_else(|error| PollBatch {
        result: PollResult {
            devices: Vec::new(),
            errors: vec![PollError::subsystem(format!("{error:#}"))],
        },
        cache_changed: false,
    })
}

#[cfg(target_os = "windows")]
pub fn run_tray() -> Result<()> {
    let Some(_instance_guard) = crate::single_instance::acquire()? else {
        return Ok(());
    };
    let loaded = config::load_or_create_config()?;
    init_logging(&loaded.config);
    crate::tray::run_tray_app(loaded.config, loaded.recovery)
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

#[cfg(test)]
mod tests {
    use super::{
        OnceOutput, OnceStatus, finish_cache_poll, once_status, poll_batch_or_diagnostic,
        record_cache_save_result, write_poll_errors,
    };
    use crate::config::PidCacheLoadState;
    use crate::model::{
        BatteryState, ChargeState, PollError, PollErrorKind, PollErrorScope, PollResult,
    };
    use std::cell::Cell;

    #[test]
    fn unsupported_device_is_a_failure_not_no_device() {
        let result = PollResult {
            devices: Vec::new(),
            errors: vec![PollError {
                device_key: "mouse".to_string(),
                display_name: "Mouse".to_string(),
                pid: 1,
                scope: PollErrorScope::Device,
                component: None,
                kind: PollErrorKind::Unsupported,
                message: "battery status is not supported".to_string(),
            }],
        };

        assert_eq!(once_status(&result), OnceStatus::PartialFailure);
        assert_eq!(once_status(&PollResult::default()), OnceStatus::NoDevice);
    }

    #[test]
    fn review_hid_initialization_failure_has_typed_json_result() {
        let batch = poll_batch_or_diagnostic(Err(anyhow::anyhow!(
            "failed to initialize hidapi: access denied"
        )));

        assert!(!batch.cache_changed);
        assert_eq!(once_status(&batch.result), OnceStatus::PartialFailure);
        let json = serde_json::to_value(batch.result).expect("serialize poll result");
        assert_eq!(json["devices"], serde_json::json!([]));
        assert_eq!(json["errors"][0]["scope"], "subsystem");
        assert_eq!(json["errors"][0]["component"], "hid");
        assert_eq!(json["errors"][0]["kind"], "access-denied");
    }

    #[test]
    fn review_cache_save_failure_preserves_poll_result() {
        let mut result = PollResult {
            devices: vec![BatteryState {
                device_key: "mouse".to_string(),
                display_name: "Mouse".to_string(),
                pid: 1,
                battery_raw: 128,
                battery_percent: 50,
                charge_state: ChargeState::NotCharging,
                observed_at: None,
            }],
            errors: Vec::new(),
        };

        record_cache_save_result(
            &mut result,
            Err(anyhow::anyhow!("permission denied while replacing cache")),
        );

        assert_eq!(once_status(&result), OnceStatus::PartialFailure);
        let json = serde_json::to_value(result).expect("serialize poll result");
        assert_eq!(json["devices"][0]["device_key"], "mouse");
        assert_eq!(json["devices"][0]["battery_percent"], 50);
        assert_eq!(json["errors"][0]["scope"], "subsystem");
        assert_eq!(json["errors"][0]["component"], "pid-cache");
        assert_eq!(json["errors"][0]["kind"], "access-denied");
        assert_eq!(json["errors"][0]["display_name"], "PID cache");
    }

    #[test]
    fn review_round_21_failed_cache_load_blocks_persistence() {
        let mut result = PollResult::default();
        let save_attempted = Cell::new(false);

        finish_cache_poll(
            &mut result,
            true,
            PidCacheLoadState::Unavailable(PollError::pid_cache(
                "PID cache unavailable: permission denied",
            )),
            || {
                save_attempted.set(true);
                Ok(())
            },
        );

        assert!(!save_attempted.get());
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].display_name, "PID cache");
        assert_eq!(result.errors[0].kind, PollErrorKind::AccessDenied);
    }

    #[test]
    fn review_diagnose_error_output_includes_typed_fields_only_in_diagnostic_mode() {
        let error = PollError {
            device_key: "mouse".to_string(),
            display_name: "Mouse".to_string(),
            pid: 1,
            scope: PollErrorScope::Device,
            component: None,
            kind: PollErrorKind::AccessDenied,
            message: "interface access denied".to_string(),
        };
        let mut diagnostic = Vec::new();
        let mut human = Vec::new();

        write_poll_errors(
            &mut diagnostic,
            std::slice::from_ref(&error),
            OnceOutput::Diagnose,
        )
        .expect("write diagnostic output");
        write_poll_errors(&mut human, &[error], OnceOutput::Human).expect("write human output");

        assert_eq!(
            String::from_utf8(diagnostic).expect("diagnostic output is UTF-8"),
            "Errors:\n- Mouse (0001) key=mouse scope=device kind=access-denied: interface access denied\n"
        );
        assert_eq!(
            String::from_utf8(human).expect("human output is UTF-8"),
            "Errors:\n- Mouse (0001): interface access denied\n"
        );
    }
}
