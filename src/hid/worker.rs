use crate::config::PidCache;
use crate::hid::client::{self, PollBatch};
use crate::hid::scanner::scan_devices;
use crate::model::PollResult;
use anyhow::{Context, Result};
use hidapi::HidApi;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fmt;
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
const MAX_SCAN_WORKERS: usize = 2;
const MAX_REQUEST_WORKERS: usize = 4;
const OBSERVATION_TRANSIT_ALLOWANCE_MS: u64 = 10;
const FRAME_PREFIX: &str = "RAZERTRAY-HID:";
const PROCESS_TREE_ENV: &str = "RAZERTRAY_HID_PROCESS_TREE";

static ACTIVE_SCAN_WORKERS: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_REQUEST_WORKERS: AtomicUsize = AtomicUsize::new(0);
static QUARANTINED_REQUESTS: LazyLock<Mutex<BTreeSet<FeatureRequestKey>>> =
    LazyLock::new(|| Mutex::new(BTreeSet::new()));
static NEW_STALLED_REQUESTS: LazyLock<Mutex<BTreeSet<FeatureRequestKey>>> =
    LazyLock::new(|| Mutex::new(BTreeSet::new()));
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
                    if workers[index].is_complete() {
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
        quarantined_requests: Vec<FeatureRequestKey>,
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
        stalled_requests: Vec<FeatureRequestKey>,
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

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct FeatureRequestKey {
    path: Vec<u8>,
    request: Vec<u8>,
}

impl FeatureRequestKey {
    fn new(path: &[u8], request: &[u8]) -> Self {
        Self {
            path: path.to_vec(),
            request: request.to_vec(),
        }
    }
}

#[derive(Debug)]
struct WorkerTimeout {
    timeout: Duration,
}

impl fmt::Display for WorkerTimeout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "HID operation unavailable after timing out at {} ms",
            self.timeout.as_millis()
        )
    }
}

impl std::error::Error for WorkerTimeout {}

#[derive(Clone, Copy, Debug)]
enum WorkerCapacity {
    Scan,
    Request,
}

impl WorkerCapacity {
    fn state(self) -> (&'static AtomicUsize, usize, &'static str) {
        match self {
            Self::Scan => (
                &ACTIVE_SCAN_WORKERS,
                MAX_SCAN_WORKERS,
                "HID scan capacity is retained by stalled scans",
            ),
            Self::Request => (
                &ACTIVE_REQUEST_WORKERS,
                MAX_REQUEST_WORKERS,
                "HID request capacity is retained by stalled operations",
            ),
        }
    }
}

#[derive(Debug)]
struct WorkerPermit {
    capacity: WorkerCapacity,
}

impl WorkerPermit {
    fn acquire(capacity: WorkerCapacity) -> Result<Self> {
        let (active_workers, maximum_workers, exhausted_message) = capacity.state();
        active_workers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < maximum_workers).then_some(active + 1)
            })
            .map_err(|_| anyhow::anyhow!(exhausted_message))?;
        Ok(Self { capacity })
    }
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        self.capacity.state().0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct RetiredWorker {
    child: Child,
    reader: Option<thread::JoinHandle<()>>,
    permit: Option<WorkerPermit>,
    process_tree: Option<process_tree::Owned>,
}

impl RetiredWorker {
    fn is_complete(&mut self) -> bool {
        let child_exited = matches!(self.child.try_wait(), Ok(Some(_)));
        let Some(tree) = &self.process_tree else {
            return child_exited;
        };
        match process_tree::is_empty(tree) {
            Ok(true) => child_exited,
            Ok(false) | Err(_) => {
                let _ = process_tree::terminate(tree);
                false
            }
        }
    }
}

struct OutputReader {
    receiver: mpsc::Receiver<std::result::Result<WorkerFrame, String>>,
    handle: thread::JoinHandle<()>,
}

struct SupervisedChild {
    child: Option<Child>,
    reader: Option<thread::JoinHandle<()>>,
    permit: Option<WorkerPermit>,
    tree: Option<process_tree::Owned>,
    retain_locally: bool,
}

impl SupervisedChild {
    fn new(child: Child, permit: Option<WorkerPermit>, retain_locally: bool) -> Self {
        Self {
            child: Some(child),
            reader: None,
            permit,
            tree: None,
            retain_locally,
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("supervised child")
    }

    fn mark_exited(&mut self) {
        let child = self.child.take().expect("supervised child");
        if self.retain_locally
            && self
                .tree
                .as_ref()
                .is_some_and(|tree| !matches!(process_tree::is_empty(tree), Ok(true)))
        {
            retain_worker(child, None, self.permit.take(), self.tree.take(), false);
        }
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            if self.retain_locally {
                retire_worker(
                    child,
                    self.reader.take(),
                    self.permit.take(),
                    self.tree.take(),
                );
            } else {
                let mut child = child;
                let _ = child.kill();
            }
        }
    }
}

pub(crate) fn poll(cache: &mut PidCache) -> Result<PollBatch> {
    let reply = run_process(
        &WorkerRequest::Poll {
            cache: cache.clone(),
            quarantined_requests: quarantined_requests(),
        },
        INITIAL_POLL_TIMEOUT,
    )
    .context("bounded HID scan failed")?;
    let (mut result, returned_cache, cache_changed, observation_ages_ms, stalled_requests) =
        match reply {
            WorkerReply::Poll {
                result,
                cache,
                cache_changed,
                observation_ages_ms,
                stalled_requests,
            } => (
                result,
                cache,
                cache_changed,
                observation_ages_ms,
                stalled_requests,
            ),
            reply => return worker_failure(reply, "poll"),
        };
    remember_stalled_requests(stalled_requests);
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
    let request_key = FeatureRequestKey::new(path, request);
    ensure_feature_request_available(&request_key)?;
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
    .map_err(|error| {
        record_stalled_request_from_error(&request_key, &error);
        error
    })
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

fn quarantined_requests() -> Vec<FeatureRequestKey> {
    QUARANTINED_REQUESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

fn replace_quarantined_requests(requests: Vec<FeatureRequestKey>) {
    *QUARANTINED_REQUESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = requests.into_iter().collect();
    NEW_STALLED_REQUESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

fn remember_stalled_requests(requests: impl IntoIterator<Item = FeatureRequestKey>) {
    QUARANTINED_REQUESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend(requests);
}

fn record_stalled_request(request: FeatureRequestKey) {
    remember_stalled_requests([request.clone()]);
    NEW_STALLED_REQUESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(request);
}

fn record_stalled_request_from_error(request: &FeatureRequestKey, error: &anyhow::Error) {
    if error.is::<WorkerTimeout>() {
        record_stalled_request(request.clone());
    }
}

fn ensure_feature_request_available(request: &FeatureRequestKey) -> Result<()> {
    if QUARANTINED_REQUESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(request)
    {
        anyhow::bail!(
            "HID feature request unavailable after a stalled operation; request quarantined"
        );
    }
    Ok(())
}

fn take_new_stalled_requests() -> Vec<FeatureRequestKey> {
    std::mem::take(
        &mut *NEW_STALLED_REQUESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_iter()
    .collect()
}

fn run_process(request: &WorkerRequest, timeout: Duration) -> Result<WorkerReply> {
    let inherited_tree = uses_inherited_process_tree(request);
    let permit = worker_permit(request, inherited_tree)?;
    let input = serde_json::to_vec(request).context("failed encoding HID worker request")?;
    let executable = std::env::current_exe().context("failed resolving HID worker executable")?;
    let mut command = Command::new(executable);
    command
        .arg("--hid-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if matches!(request, WorkerRequest::Poll { .. }) {
        command.env(PROCESS_TREE_ENV, "1");
    }
    let child = command
        .spawn()
        .context("failed starting HID worker process")?;
    let mut worker = SupervisedChild::new(child, permit, !inherited_tree);
    if matches!(request, WorkerRequest::Poll { .. }) {
        worker.tree = process_tree::assign(worker.child_mut())?;
    }
    supervise_child(worker, &input, timeout)
}

fn worker_permit(request: &WorkerRequest, inherited_tree: bool) -> Result<Option<WorkerPermit>> {
    let capacity = match request {
        WorkerRequest::Poll { .. } => Some(WorkerCapacity::Scan),
        WorkerRequest::Feature { .. } if !inherited_tree => Some(WorkerCapacity::Request),
        WorkerRequest::Feature { .. } => None,
    };
    capacity.map(WorkerPermit::acquire).transpose()
}

fn supervise_child(
    mut worker: SupervisedChild,
    input: &[u8],
    initial_timeout: Duration,
) -> Result<WorkerReply> {
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
            return Err(anyhow::Error::new(WorkerTimeout { timeout }));
        }
    }
}

fn uses_inherited_process_tree(request: &WorkerRequest) -> bool {
    #[cfg(windows)]
    return matches!(request, WorkerRequest::Feature { .. })
        && std::env::var_os(PROCESS_TREE_ENV).is_some();
    #[cfg(not(windows))]
    {
        let _ = request;
        false
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

fn retire_worker(
    child: Child,
    reader: Option<thread::JoinHandle<()>>,
    permit: Option<WorkerPermit>,
    tree: Option<process_tree::Owned>,
) {
    retain_worker(child, reader, permit, tree, true);
}

fn retain_worker(
    mut child: Child,
    reader: Option<thread::JoinHandle<()>>,
    permit: Option<WorkerPermit>,
    tree: Option<process_tree::Owned>,
    terminate: bool,
) {
    if let Some(tree) = &tree {
        if terminate {
            let _ = process_tree::terminate(tree);
        }
    }
    if terminate {
        let _ = child.kill();
    }
    RETIRED_WORKERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(RetiredWorker {
            child,
            reader,
            permit,
            process_tree: tree,
        });
    LazyLock::force(&REAPER);
}

pub(crate) fn shutdown(timeout: Duration) {
    process_tree::shutdown(timeout);
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
        WorkerRequest::Poll {
            mut cache,
            quarantined_requests,
        } => {
            replace_quarantined_requests(quarantined_requests);
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
                stalled_requests: take_new_stalled_requests(),
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

#[cfg(windows)]
mod process_tree {
    use super::{Child, Duration, Instant, LazyLock, Mutex, Result, STATUS_POLL_INTERVAL};
    use anyhow::Context;
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use std::sync::{Arc, Weak};
    use std::thread;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    };

    pub(super) type Owned = Arc<Job>;

    pub(super) struct Job {
        handle: HANDLE,
    }

    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    static ACTIVE_JOBS: LazyLock<Mutex<Vec<Weak<Job>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

    pub(super) fn assign(child: &Child) -> Result<Option<Owned>> {
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed creating HID process tree");
        }
        let job = Arc::new(Job { handle });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const c_void,
                std::mem::size_of_val(&limits)
                    .try_into()
                    .unwrap_or(u32::MAX),
            )
        };
        if configured == 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed configuring HID process tree");
        }
        let assigned = unsafe { AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) };
        if assigned == 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed assigning HID worker to process tree");
        }
        let mut active = ACTIVE_JOBS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.retain(|job| job.strong_count() > 0);
        active.push(Arc::downgrade(&job));
        Ok(Some(job))
    }

    pub(super) fn is_empty(job: &Owned) -> Result<bool> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let queried = unsafe {
            QueryInformationJobObject(
                job.handle,
                JobObjectBasicAccountingInformation,
                &mut accounting as *mut _ as *mut c_void,
                std::mem::size_of_val(&accounting)
                    .try_into()
                    .unwrap_or(u32::MAX),
                ptr::null_mut(),
            )
        };
        if queried == 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed querying HID process tree");
        }
        Ok(accounting.ActiveProcesses == 0)
    }

    pub(super) fn terminate(job: &Owned) -> Result<()> {
        let terminated = unsafe { TerminateJobObject(job.handle, 124) };
        if terminated == 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed terminating HID process tree");
        }
        Ok(())
    }

    pub(super) fn shutdown(timeout: Duration) {
        let jobs = {
            let mut active = ACTIVE_JOBS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let jobs = active.iter().filter_map(Weak::upgrade).collect::<Vec<_>>();
            active.retain(|job| job.strong_count() > 0);
            jobs
        };
        let deadline = Instant::now() + timeout;
        loop {
            let active = jobs.iter().any(|job| match is_empty(job) {
                Ok(true) => false,
                Ok(false) | Err(_) => {
                    let _ = terminate(job);
                    true
                }
            });
            if !active || Instant::now() >= deadline {
                return;
            }
            thread::sleep(STATUS_POLL_INTERVAL);
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(not(windows))]
mod process_tree {
    use super::{Child, Duration, Result};

    pub(super) type Owned = ();

    pub(super) fn assign(_child: &Child) -> Result<Option<Owned>> {
        Ok(None)
    }

    pub(super) fn is_empty(_job: &Owned) -> Result<bool> {
        Ok(true)
    }

    pub(super) fn terminate(_job: &Owned) -> Result<()> {
        Ok(())
    }

    pub(super) fn shutdown(_timeout: Duration) {}
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::RetiredWorker;
    use super::{
        FeatureRequestKey, INITIAL_POLL_TIMEOUT, STATUS_POLL_INTERVAL, WorkerCapacity, WorkerFrame,
        WorkerPermit, WorkerReply, WorkerRequest, WorkerTimeout, ensure_feature_request_available,
        record_stalled_request_from_error, supervise_child, worker_permit, write_frame,
    };
    #[cfg(windows)]
    use std::io::{Read, Write};
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
            super::SupervisedChild::new(
                child,
                Some(WorkerPermit::acquire(WorkerCapacity::Request).expect("worker capacity")),
                true,
            ),
            &[],
            timeout,
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
            super::SupervisedChild::new(
                child,
                Some(WorkerPermit::acquire(WorkerCapacity::Request).expect("worker capacity")),
                true,
            ),
            &[],
            Duration::from_secs(2),
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
            super::SupervisedChild::new(
                child,
                Some(WorkerPermit::acquire(WorkerCapacity::Request).expect("worker capacity")),
                true,
            ),
            &[],
            Duration::from_millis(20),
        )
        .expect("work-derived poll deadline should replace the enumeration deadline");

        assert!(matches!(reply, WorkerReply::Failure { message } if message == "planned"));
        assert_eq!(INITIAL_POLL_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn review_round_32_stalled_request_preserves_transaction_fallbacks() {
        let path = b"review-round-32-stalled-path";
        let mut first_request = [0; 91];
        first_request[2] = 0x1F;
        let first_request = FeatureRequestKey::new(path, &first_request);
        record_stalled_request_from_error(
            &first_request,
            &anyhow::Error::new(WorkerTimeout {
                timeout: Duration::from_millis(20),
            }),
        );

        let error = ensure_feature_request_available(&first_request)
            .expect_err("stalled request must remain quarantined");
        assert!(format!("{error:#}").contains("quarantined"));
        for transaction_id in [0x3F, 0xFF] {
            let mut fallback_request = [0; 91];
            fallback_request[2] = transaction_id;
            ensure_feature_request_available(&FeatureRequestKey::new(path, &fallback_request))
                .expect("later transaction must retain an isolated attempt");
        }
    }

    #[test]
    fn review_round_33_scan_capacity_is_bounded_with_recovery_reserved() {
        let poll = WorkerRequest::Poll {
            cache: crate::config::PidCache::default(),
            quarantined_requests: Vec::new(),
        };
        let feature = WorkerRequest::Feature {
            path: Vec::new(),
            request: Vec::new(),
            response: Vec::new(),
            response_wait_ms: 0,
            operation_timeout_ms: 0,
        };

        let primary_scan = worker_permit(&poll, false)
            .expect("primary scan admission")
            .expect("primary scan permit");
        let recovery_scan = worker_permit(&poll, false)
            .expect("recovery scan admission")
            .expect("recovery scan permit");
        let error = worker_permit(&poll, false).expect_err("scan capacity must remain bounded");

        assert!(format!("{error:#}").contains("scan capacity"));
        assert!(
            worker_permit(&feature, false)
                .expect("request admission")
                .is_some()
        );
        drop(recovery_scan);
        assert!(
            worker_permit(&poll, false)
                .expect("replacement recovery scan admission")
                .is_some()
        );
        drop(primary_scan);
    }

    #[cfg(windows)]
    #[test]
    fn review_round_29_process_tree_shutdown_terminates_worker() {
        let mut child = child_test("hid::worker::tests::review_round_27_timeout_child");
        let tree = super::process_tree::assign(&child)
            .expect("create process tree")
            .expect("Windows process tree");
        let started = Instant::now();

        assert!(!super::process_tree::is_empty(&tree).expect("query active worker"));
        super::process_tree::shutdown(Duration::from_millis(100));

        assert!(started.elapsed() < Duration::from_millis(250));
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if child.try_wait().expect("wait for worker").is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker process survived shutdown"
            );
            std::thread::sleep(STATUS_POLL_INTERVAL);
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if super::process_tree::is_empty(&tree).expect("query terminated worker") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "process tree retained active workers"
            );
            std::thread::sleep(STATUS_POLL_INTERVAL);
        }
    }

    #[cfg(windows)]
    #[test]
    fn review_round_30_retired_worker_waits_for_descendants() {
        let mut child = child_test("hid::worker::tests::review_round_30_descendant_parent");
        let tree = super::process_tree::assign(&child)
            .expect("create process tree")
            .expect("Windows process tree");
        child
            .stdin
            .take()
            .expect("parent stdin")
            .write_all(&[1])
            .expect("release parent");
        child.wait().expect("parent exits");
        let mut retired = RetiredWorker {
            child,
            reader: None,
            permit: Some(WorkerPermit::acquire(WorkerCapacity::Request).expect("worker capacity")),
            process_tree: Some(tree),
        };

        assert!(!retired.is_complete());
        let tree = retired
            .process_tree
            .as_ref()
            .expect("retained process tree");
        super::process_tree::terminate(tree).expect("terminate descendants");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !retired.is_complete() {
            assert!(Instant::now() < deadline, "descendant survived termination");
            std::thread::sleep(STATUS_POLL_INTERVAL);
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn review_round_30_descendant_parent() {
        let mut token = [0];
        std::io::stdin()
            .read_exact(&mut token)
            .expect("wait for process-tree assignment");
        Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "hid::worker::tests::review_round_27_timeout_child",
                "--ignored",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start descendant");
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
