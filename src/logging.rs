//! Logging to `razertray.log` in the app data folder, with size-based rotation.

use crate::config::{self, AppConfig};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use tracing::{Level, event};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Rotate when the log reaches this size. The README documents "about 1 MiB".
const MAX_LOG_BYTES: u64 = 1024 * 1024;
/// Files kept: the live log plus `.1` and `.2`. The README documents three files.
const LOG_FILES_TO_KEEP: usize = 3;

/// Starts logging to the log file, or to stderr when the file cannot be opened.
pub(crate) fn init(cfg: &AppConfig) {
    let (filter, filter_err) = match EnvFilter::try_new(&cfg.log_level) {
        Ok(filter) => (filter, None),
        Err(err) => (EnvFilter::new("info"), Some(err)),
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(true);

    match LogFile::open(config::log_path(), MAX_LOG_BYTES, LOG_FILES_TO_KEEP) {
        Ok(log_file) => {
            let _ = builder.with_ansi(false).with_writer(log_file).try_init();
        }
        Err(err) => {
            let _ = builder.try_init();
            event!(
                name: "log.open.failure",
                Level::WARN,
                exception.message = %format_args!("{err:#}"),
                "cannot open the log file, logging to stderr: {{exception.message}}",
            );
        }
    }

    event!(
        name: "app.start",
        Level::INFO,
        service.version = env!("CARGO_PKG_VERSION"),
        "razertray {{service.version}} started"
    );

    if let Some(err) = filter_err {
        event!(
            name: "log.filter.invalid",
            Level::WARN,
            config.log_level = %cfg.log_level,
            exception.message = %format_args!("{err:#}"),
            "invalid log_level {{config.log_level}}, using \"info\": {{exception.message}}",
        );
    }
}

/// An append-only log file that rotates itself past a size limit.
///
/// The file stays open between events. It is closed before rotation,
/// because Windows cannot rename an open file.
#[derive(Debug)]
struct LogFile {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    state: Mutex<OpenLog>,
}

#[derive(Debug)]
struct OpenLog {
    /// `None` after a failed reopen; the next write tries again.
    file: Option<File>,
    len: u64,
    /// Size at which the next rotation is due.
    rotate_at: u64,
}

impl LogFile {
    fn open(path: PathBuf, max_bytes: u64, keep: usize) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let (file, len) = open_append(&path)?;
        Ok(Self {
            path,
            max_bytes,
            keep,
            state: Mutex::new(OpenLog {
                file: Some(file),
                len,
                rotate_at: max_bytes,
            }),
        })
    }

    fn rotate(&self, state: &mut OpenLog) {
        state.file = None;
        // Rotation fails when another process (a `--once` run) holds the
        // file open. Then keep appending and try again one size step later.
        // Errors cannot be logged here: this is the logger.
        if rotate_files(&self.path, self.keep).is_ok() {
            state.rotate_at = self.max_bytes;
        } else {
            state.rotate_at = state.len + self.max_bytes;
        }
        if let Ok((file, len)) = open_append(&self.path) {
            state.file = Some(file);
            state.len = len;
        }
    }
}

impl Write for &LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.len >= state.rotate_at {
            self.rotate(&mut state);
        }
        let file = if let Some(file) = state.file.take() {
            file
        } else {
            let (file, len) = open_append(&self.path)?;
            state.len = len;
            file
        };
        let written = (&file).write(buf);
        state.file = Some(file);
        let written = written?;
        state.len += u64::try_from(written).unwrap_or(u64::MAX);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.file.as_mut().map_or(Ok(()), File::flush)
    }
}

impl<'a> MakeWriter<'a> for LogFile {
    type Writer = &'a LogFile;

    fn make_writer(&'a self) -> Self::Writer {
        self
    }
}

fn open_append(path: &Path) -> io::Result<(File, u64)> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let len = file.metadata()?.len();
    Ok((file, len))
}

/// Shifts `log` to `log.1`, `log.1` to `log.2` and so on, keeping `keep`
/// files in total including the live one.
fn rotate_files(path: &Path, keep: usize) -> io::Result<()> {
    if keep <= 1 {
        return Ok(());
    }
    let oldest = rotated_path(path, keep - 1);
    match fs::remove_file(&oldest) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    for index in (1..keep - 1).rev() {
        let src = rotated_path(path, index);
        if src.exists() {
            fs::rename(&src, rotated_path(path, index + 1))?;
        }
    }
    fs::rename(path, rotated_path(path, 1))
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::{LogFile, rotated_path};
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("read log")
    }

    #[test]
    fn rotates_past_the_limit_and_keeps_newest_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("test.log");
        let log = LogFile::open(path.clone(), 100, 3).expect("open log");

        for line in 0..10 {
            (&log)
                .write_all(format!("line {line:02} {}\n", "x".repeat(41)).as_bytes())
                .expect("write");
        }

        // Two 50-byte lines fill a file; the last 6 lines survive in 3 files.
        assert!(read(&path).starts_with("line 08"), "{}", read(&path));
        assert!(read(&rotated_path(&path, 1)).starts_with("line 06"));
        assert!(read(&rotated_path(&path, 2)).starts_with("line 04"));
        assert!(!rotated_path(&path, 3).exists());
    }

    #[test]
    fn existing_large_file_rotates_on_first_write() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("test.log");
        fs::write(&path, "old\n".repeat(50)).expect("seed log");

        let log = LogFile::open(path.clone(), 100, 3).expect("open log");
        (&log).write_all(b"new\n").expect("write");

        assert_eq!(fs::read_to_string(&path).expect("read"), "new\n");
        assert!(rotated_path(&path, 1).exists());
    }
}
