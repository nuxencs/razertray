use crate::config::PidCache;
use crate::hid::client::{self, PollBatch};
use crate::hid::scanner::scan_devices;
use crate::model::PollResult;
use anyhow::{Context, Result};
use hidapi::HidApi;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const INITIAL_POLL_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_REPLY_ALLOWANCE: Duration = Duration::from_secs(1);
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_SUPERVISED_WORKERS: usize = 4;
const OBSERVATION_TRANSIT_ALLOWANCE_MS: u64 = 10;
const FRAME_PREFIX: &str = "RAZERTRAY-HID:";

static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);
static RETIRED_WORKERS: LazyLock<Mutex<Vec<RetiredWorker>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static REAPER: LazyLock<()> = LazyLock::new(|| {
    let _ = thread::spawn(|| {
        loop {
            let completed = {
                let mut workers = RETIRED_WORKERS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut completed = Vec::new();
                let mut index = workers.len();
                while index > 0 {
                    index -= 1;
                    if matches!(workers[index].child.try_wait(), Ok(Some(_))) {
                        completed.push(workers.swap_remove(index));
                    }
                }
                completed
            };
            for mut worker in completed {
                if let Some(reader) = worker.reader.take() {
                    let _ = reader.join();
                }
            }
            thread::sleep(STATUS_POLL_INTERVAL);
        }
    });
});

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

#[derive(Debug, Deserialize, Serialize)]
enum WorkerFrame {
    PollPlan { timeout_ms: u64 },
    Reply(WorkerReply),
}

struct WorkerPermit;

impl WorkerPermit {
    fn acquire() -> Result<Self> {
        ACTIVE_WORKERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_SUPERVISED_WORKERS).then_some(active + 1)
            })
            .map_err(|_| {
                anyhow::anyhow!("HID worker capacity is retained by stalled operations")
            })?;
        Ok(Self)
    }
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        ACTIVE_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct RetiredWorker {
    child: Child,
    reader: Option<thread::JoinHandle<()>>,
    _permit: WorkerPermit,
}

struct OutputReader {
    receiver: mpsc::Receiver<std::result::Result<WorkerFrame, String>>,
    handle: thread::JoinHandle<()>,
}

struct SupervisedChild {
    child: Option<Child>,
    reader: Option<thread::JoinHandle<()>>,
    permit: Option<WorkerPermit>,
}

impl SupervisedChild {
    fn new(child: Child, permit: WorkerPermit) -> Self {
        Self {
            child: Some(child),
            reader: None,
            permit: Some(permit),
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("supervised child")
    }

    fn mark_exited(&mut self) {
        self.child.take();
        self.permit.take();
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if let (Some(child), Some(permit)) = (self.child.take(), self.permit.take()) {
            retire_worker(child, self.reader.take(), permit);
        }
    }
}

pub(crate) fn poll(cache: &mut PidCache) -> Result<PollBatch> {
    let reply = run_process(
        &WorkerRequest::Poll {
            cache: cache.clone(),
        },
        INITIAL_POLL_TIMEOUT,
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
    let permit = WorkerPermit::acquire()?;
    let input = serde_json::to_vec(request).context("failed encoding HID worker request")?;
    let executable = std::env::current_exe().context("failed resolving HID worker executable")?;
    let child = Command::new(executable)
        .arg("--hid-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed starting HID worker process")?;
    supervise_child(child, &input, timeout, permit)
}

fn supervise_child(
    child: Child,
    input: &[u8],
    initial_timeout: Duration,
    permit: WorkerPermit,
) -> Result<WorkerReply> {
    let mut worker = SupervisedChild::new(child, permit);
    let stdout = worker
        .child_mut()
        .stdout
        .take()
        .context("HID worker stdout was unavailable")?;
    let OutputReader { receiver, handle } = start_output_reader(stdout)?;
    worker.reader = Some(handle);
    let mut stdin = worker
        .child_mut()
        .stdin
        .take()
        .context("HID worker stdin was unavailable")?;
    stdin
        .write_all(input)
        .context("failed sending HID worker request")?;
    drop(stdin);

    let mut timeout = initial_timeout;
    let mut deadline = Instant::now() + timeout;
    let mut reply = None;
    loop {
        let now = Instant::now();
        let wait = STATUS_POLL_INTERVAL.min(deadline.saturating_duration_since(now));
        match receiver.recv_timeout(wait) {
            Ok(Ok(WorkerFrame::PollPlan { timeout_ms })) => {
                timeout = Duration::from_millis(timeout_ms);
                deadline = Instant::now()
                    .checked_add(timeout)
                    .context("HID worker returned an invalid poll deadline")?;
            }
            Ok(Ok(WorkerFrame::Reply(received))) => reply = Some(received),
            Ok(Err(error)) => anyhow::bail!("failed reading HID worker response: {error}"),
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
        }

        match worker.child_mut().try_wait() {
            Ok(Some(status)) => {
                worker.mark_exited();
                if let Some(handle) = worker.reader.take() {
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("HID worker output reader failed"))?;
                }
                for frame in receiver.try_iter() {
                    match frame {
                        Ok(WorkerFrame::PollPlan { .. }) => {}
                        Ok(WorkerFrame::Reply(received)) => reply = Some(received),
                        Err(error) => anyhow::bail!("failed reading HID worker response: {error}"),
                    }
                }
                if !status.success() {
                    anyhow::bail!("HID worker exited with {status}");
                }
                return reply.context("HID worker exited without a response");
            }
            Ok(None) => {}
            Err(error) => return Err(error).context("failed waiting for HID worker"),
        }

        if Instant::now() >= deadline {
            anyhow::bail!(
                "HID operation unavailable after timing out at {} ms",
                timeout.as_millis()
            );
        }
    }
}

fn start_output_reader(stdout: ChildStdout) -> Result<OutputReader> {
    let (sender, receiver) = mpsc::channel();
    let handle = thread::Builder::new()
        .name("razertray-hid-output".to_string())
        .spawn(move || read_output_frames(stdout, &sender))
        .context("failed starting HID worker output reader")?;
    Ok(OutputReader { receiver, handle })
}

fn read_output_frames(
    stdout: ChildStdout,
    sender: &mpsc::Sender<std::result::Result<WorkerFrame, String>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let Some(encoded) = line.strip_prefix(FRAME_PREFIX) else {
                    continue;
                };
                let frame = serde_json::from_str(encoded.trim_end())
                    .map_err(|error| format!("failed decoding HID worker frame: {error}"));
                if sender.send(frame).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = sender.send(Err(error.to_string()));
                break;
            }
        }
    }
}

fn retire_worker(mut child: Child, reader: Option<thread::JoinHandle<()>>, permit: WorkerPermit) {
    let _ = child.kill();
    RETIRED_WORKERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(RetiredWorker {
            child,
            reader,
            _permit: permit,
        });
    LazyLock::force(&REAPER);
}

pub(crate) fn run() -> Result<()> {
    let request: WorkerRequest =
        serde_json::from_reader(std::io::stdin().lock()).context("invalid HID worker request")?;
    let mut stdout = std::io::stdout().lock();
    let reply = match execute(request, &mut stdout) {
        Ok(reply) => reply,
        Err(error) => WorkerReply::Failure {
            message: format!("{error:#}"),
        },
    };
    write_frame(&mut stdout, &WorkerFrame::Reply(reply))?;
    Ok(())
}

fn execute<W: Write>(request: WorkerRequest, output: &mut W) -> Result<WorkerReply> {
    match request {
        WorkerRequest::Poll { mut cache } => {
            let api = HidApi::new().context("failed initializing HID access")?;
            let discovered = scan_devices(&api);
            let timeout = client::maximum_poll_duration(discovered.len())
                .saturating_add(POLL_REPLY_ALLOWANCE);
            write_frame(
                output,
                &WorkerFrame::PollPlan {
                    timeout_ms: timeout.as_millis().try_into().unwrap_or(u64::MAX),
                },
            )?;
            let batch = client::poll_discovered_devices(discovered, &mut cache);
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

fn write_frame<W: Write>(output: &mut W, frame: &WorkerFrame) -> Result<()> {
    output
        .write_all(FRAME_PREFIX.as_bytes())
        .context("failed writing HID worker frame prefix")?;
    serde_json::to_writer(&mut *output, frame).context("failed writing HID worker frame")?;
    output
        .write_all(b"\n")
        .context("failed completing HID worker frame")?;
    output.flush().context("failed flushing HID worker frame")
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
    use super::{
        INITIAL_POLL_TIMEOUT, STATUS_POLL_INTERVAL, WorkerFrame, WorkerPermit, WorkerReply,
        supervise_child, write_frame,
    };
    use std::process::{Child, Command, Stdio};
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

        let error = supervise_child(
            child,
            &[],
            timeout,
            WorkerPermit::acquire().expect("worker capacity"),
        )
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

    #[test]
    fn review_round_28_large_worker_response_does_not_fill_pipe() {
        let child = child_test("hid::worker::tests::review_round_28_large_output_child");
        let reply = supervise_child(
            child,
            &[],
            Duration::from_secs(2),
            WorkerPermit::acquire().expect("worker capacity"),
        )
        .expect("large reply should be drained while the child runs");

        let WorkerReply::Failure { message } = reply else {
            panic!("expected large failure reply")
        };
        assert_eq!(message.len(), 1_048_576);
    }

    #[test]
    #[ignore]
    fn review_round_28_large_output_child() {
        write_frame(
            &mut std::io::stdout().lock(),
            &WorkerFrame::Reply(WorkerReply::Failure {
                message: "x".repeat(1_048_576),
            }),
        )
        .expect("write large worker reply");
    }

    #[test]
    fn review_round_28_poll_plan_extends_initial_deadline() {
        let child = child_test("hid::worker::tests::review_round_28_planned_poll_child");
        let reply = supervise_child(
            child,
            &[],
            Duration::from_millis(20),
            WorkerPermit::acquire().expect("worker capacity"),
        )
        .expect("work-derived poll deadline should replace the enumeration deadline");

        assert!(matches!(reply, WorkerReply::Failure { message } if message == "planned"));
        assert_eq!(INITIAL_POLL_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    #[ignore]
    fn review_round_28_planned_poll_child() {
        let mut stdout = std::io::stdout().lock();
        write_frame(&mut stdout, &WorkerFrame::PollPlan { timeout_ms: 500 })
            .expect("write poll plan");
        std::thread::sleep(Duration::from_millis(80));
        write_frame(
            &mut stdout,
            &WorkerFrame::Reply(WorkerReply::Failure {
                message: "planned".to_string(),
            }),
        )
        .expect("write planned reply");
    }

    fn child_test(name: &str) -> Child {
        Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", name, "--ignored", "--nocapture"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start isolated child")
    }
}
