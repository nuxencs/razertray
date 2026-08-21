use crate::config::PidCache;
use crate::hid::client::{self, PollBatch};
use crate::model::PollResult;
use anyhow::{Context, Result};
use hidapi::HidApi;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const POLL_TIMEOUT: Duration = Duration::from_secs(30);
const REAP_TIMEOUT: Duration = Duration::from_millis(250);
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_REAPERS: usize = 4;
const OBSERVATION_TRANSIT_ALLOWANCE_MS: u64 = 10;

static ACTIVE_REAPERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Deserialize, Serialize)]
enum WorkerRequest {
    Poll {
        cache: PidCache,
    },
    Feature {
        path: Vec<u8>,
        request: Vec<u8>,
        response: Vec<u8>,
        response_wait_ms: u64,
        operation_timeout_ms: u64,
    },
}

#[derive(Debug, Deserialize, Serialize)]
enum WorkerReply {
    Poll {
        result: PollResult,
        cache: PidCache,
        cache_changed: bool,
        observation_ages_ms: Vec<Option<u64>>,
    },
    Feature {
        count: usize,
        response: Vec<u8>,
    },
    Failure {
        message: String,
    },
}

pub(crate) fn poll(cache: &mut PidCache) -> Result<PollBatch> {
    let reply = run_process(
        &WorkerRequest::Poll {
            cache: cache.clone(),
        },
        POLL_TIMEOUT,
    )
    .context("bounded HID scan failed")?;
    let (mut result, returned_cache, cache_changed, observation_ages_ms) = match reply {
        WorkerReply::Poll {
            result,
            cache,
            cache_changed,
            observation_ages_ms,
        } => (result, cache, cache_changed, observation_ages_ms),
        reply => return worker_failure(reply, "poll"),
    };
    if observation_ages_ms.len() != result.devices.len() {
        anyhow::bail!(
            "HID worker returned {} observation ages for {} devices",
            observation_ages_ms.len(),
            result.devices.len()
        );
    }
    let received_at = Instant::now();
    for (device, age_ms) in result.devices.iter_mut().zip(observation_ages_ms) {
        device.observed_at = age_ms.map(|age_ms| {
            received_at
                .checked_sub(Duration::from_millis(
                    age_ms.saturating_add(OBSERVATION_TRANSIT_ALLOWANCE_MS),
                ))
                .unwrap_or(received_at)
        });
    }
    *cache = returned_cache;
    Ok(PollBatch {
        result,
        cache_changed,
    })
}

pub(crate) fn exchange_feature(
    path: &[u8],
    request: &[u8],
    response: &mut [u8],
    response_wait: Duration,
    timeout: Duration,
) -> Result<usize> {
    let reply = run_process(
        &WorkerRequest::Feature {
            path: path.to_vec(),
            request: request.to_vec(),
            response: response.to_vec(),
            response_wait_ms: response_wait.as_millis().try_into().unwrap_or(u64::MAX),
            operation_timeout_ms: timeout.as_millis().try_into().unwrap_or(u64::MAX),
        },
        timeout,
    )
    .context("HID feature operation unavailable")?;
    let (count, received) = match reply {
        WorkerReply::Feature { count, response } => (count, response),
        reply => return worker_failure(reply, "feature"),
    };
    if received.len() != response.len() {
        anyhow::bail!(
            "HID worker returned {} bytes of storage, expected {}",
            received.len(),
            response.len()
        );
    }
    response.copy_from_slice(&received);
    Ok(count)
}

fn worker_failure<T>(reply: WorkerReply, expected: &str) -> Result<T> {
    match reply {
        WorkerReply::Failure { message } => Err(anyhow::Error::msg(message)),
        _ => anyhow::bail!("HID worker returned an unexpected reply for {expected}"),
    }
}

fn run_process(request: &WorkerRequest, timeout: Duration) -> Result<WorkerReply> {
    let deadline = Instant::now() + timeout;
    let input = serde_json::to_vec(request).context("failed encoding HID worker request")?;
    let executable = std::env::current_exe().context("failed resolving HID worker executable")?;
    let child = Command::new(executable)
        .arg("--hid-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed starting HID worker process")?;
    let output = collect_child_output(child, &input, deadline, timeout)?;
    serde_json::from_slice(&output).context("failed decoding HID worker response")
}

fn collect_child_output(
    mut child: Child,
    input: &[u8],
    deadline: Instant,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut stdin = child
        .stdin
        .take()
        .context("HID worker stdin was unavailable")?;
    stdin
        .write_all(input)
        .context("failed sending HID worker request")?;
    drop(stdin);

    loop {
        if let Some(status) = child.try_wait().context("failed waiting for HID worker")? {
            let mut output = Vec::new();
            child
                .stdout
                .take()
                .context("HID worker stdout was unavailable")?
                .read_to_end(&mut output)
                .context("failed reading HID worker response")?;
            if !status.success() {
                anyhow::bail!("HID worker exited with {status}");
            }
            return Ok(output);
        }

        let now = Instant::now();
        if now >= deadline {
            terminate_and_reap(child);
            anyhow::bail!(
                "HID operation unavailable after timing out at {} ms",
                timeout.as_millis()
            );
        }
        thread::sleep(STATUS_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

fn terminate_and_reap(mut child: Child) {
    let _ = child.kill();
    if ACTIVE_REAPERS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < MAX_REAPERS).then_some(active + 1)
        })
        .is_err()
    {
        return;
    }
    thread::spawn(move || {
        let deadline = Instant::now() + REAP_TIMEOUT;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => thread::sleep(STATUS_POLL_INTERVAL),
            }
        }
        ACTIVE_REAPERS.fetch_sub(1, Ordering::AcqRel);
    });
}

pub(crate) fn run() -> Result<()> {
    let request: WorkerRequest =
        serde_json::from_reader(std::io::stdin().lock()).context("invalid HID worker request")?;
    let reply = match execute(request) {
        Ok(reply) => reply,
        Err(error) => WorkerReply::Failure {
            message: format!("{error:#}"),
        },
    };
    serde_json::to_writer(std::io::stdout().lock(), &reply)
        .context("failed writing HID worker response")?;
    Ok(())
}

fn execute(request: WorkerRequest) -> Result<WorkerReply> {
    match request {
        WorkerRequest::Poll { mut cache } => {
            let api = HidApi::new().context("failed initializing HID access")?;
            let batch = client::poll_devices(&api, &mut cache);
            let encoded_at = Instant::now();
            let observation_ages_ms = batch
                .result
                .devices
                .iter()
                .map(|device| {
                    device.observed_at.map(|observed_at| {
                        encoded_at
                            .saturating_duration_since(observed_at)
                            .as_millis()
                            .try_into()
                            .unwrap_or(u64::MAX)
                    })
                })
                .collect();
            Ok(WorkerReply::Poll {
                result: batch.result,
                cache,
                cache_changed: batch.cache_changed,
                observation_ages_ms,
            })
        }
        WorkerRequest::Feature {
            path,
            request,
            mut response,
            response_wait_ms,
            operation_timeout_ms,
        } => {
            let (completed_tx, completed_rx) = mpsc::channel();
            thread::spawn(move || {
                if matches!(
                    completed_rx.recv_timeout(Duration::from_millis(operation_timeout_ms)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    terminate_current_process();
                }
            });
            HidApi::disable_device_discovery();
            let path = CString::new(path).context("invalid HID interface path")?;
            let api = HidApi::new().context("failed initializing HID access")?;
            let device = api
                .open_path(path.as_c_str())
                .context("could not open HID interface")?;
            device
                .send_feature_report(&request)
                .context("send_feature_report failed")?;
            thread::sleep(Duration::from_millis(response_wait_ms));
            let count = device
                .get_feature_report(&mut response)
                .context("get_feature_report failed")?;
            let _ = completed_tx.send(());
            Ok(WorkerReply::Feature { count, response })
        }
    }
}

fn terminate_current_process() -> ! {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Threading::TerminateProcess(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            124,
        );
    }
    std::process::abort()
}

#[cfg(test)]
mod tests {
    use super::{STATUS_POLL_INTERVAL, collect_child_output};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn review_round_27_timeout_returns_before_slow_child_exits() {
        let child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "hid::worker::tests::review_round_27_timeout_child",
                "--ignored",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start isolated child");
        let started = Instant::now();
        let timeout = Duration::from_millis(20);

        let error = collect_child_output(child, &[], started + timeout, timeout)
            .expect_err("slow isolated operation should time out");

        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(format!("{error:#}").contains("timing out"));
        assert_eq!(STATUS_POLL_INTERVAL, Duration::from_millis(5));
    }

    #[test]
    #[ignore]
    fn review_round_27_timeout_child() {
        std::thread::sleep(Duration::from_secs(5));
    }
}
