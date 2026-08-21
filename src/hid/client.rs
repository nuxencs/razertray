use crate::config::PidCache;
use crate::device_map;
use crate::hid::protocol::{
    FEATURE_REPORT_LENGTH, RazerReport, STATUS_BUSY, STATUS_FAILURE, STATUS_NO_RESPONSE,
    STATUS_NOT_SUPPORTED, STATUS_SUCCESSFUL, build_battery_request, build_charging_request,
    expected_response_matches, feature_report_payload,
};
use crate::hid::scanner::{DiscoveredDevice, InterfaceGrouping, scan_devices};
use crate::model::{
    BatteryState, ChargeState, PollError, PollErrorKind, PollErrorScope, PollResult,
};
use anyhow::{Context, Result};
use hidapi::{HidApi, HidDevice};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_CANDIDATES_PER_DEVICE: usize = 4;
const MAX_RETRIES: usize = 5;
const DEVICE_POLL_BUDGET: Duration = Duration::from_secs(8);
const FEATURE_IO_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_BLOCKED_FEATURE_OPERATIONS: usize = MAX_CANDIDATES_PER_DEVICE;
const SEND_DELAY: Duration = Duration::from_millis(60);
const RETRY_DELAY: Duration = Duration::from_millis(400);

static ACTIVE_FEATURE_OPERATIONS: AtomicUsize = AtomicUsize::new(0);

pub struct PollBatch {
    pub result: PollResult,
    pub cache_changed: bool,
}

#[derive(Debug)]
enum QueryFailure {
    Unsupported {
        evidence: UnsupportedEvidence,
        auxiliary: Vec<anyhow::Error>,
    },
    Failed {
        errors: Vec<anyhow::Error>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnsupportedEvidence {
    Conclusive,
    Partial,
}

#[derive(Debug)]
struct ProbeSuccess {
    candidate_index: usize,
    transaction_id: u8,
    report: RazerReport,
}

pub fn poll_devices(api: &HidApi, pid_cache: &mut PidCache) -> PollBatch {
    let discovered = scan_devices(api);
    let mut result = PollResult::default();
    let mut cache_changed = false;

    for device in discovered {
        let query = query_device(api, &device, pid_cache, &mut cache_changed);
        record_query_result(&mut result, &device, query);
    }

    result.sort_devices();
    PollBatch {
        result,
        cache_changed,
    }
}

fn record_query_result(
    result: &mut PollResult,
    device: &DiscoveredDevice,
    query: std::result::Result<(BatteryState, Vec<PollError>), QueryFailure>,
) {
    let ambiguous = device.interface_grouping.is_ambiguous();
    if ambiguous {
        result.errors.push(PollError {
            device_key: device.key.clone(),
            display_name: display_name(device),
            pid: device.pid,
            scope: PollErrorScope::Device,
            component: None,
            kind: PollErrorKind::AmbiguousIdentity,
            message: "serialless interfaces cannot be assigned to one physical device".to_string(),
        });
    }
    match query {
        Ok((state, warnings)) if !ambiguous => {
            result.devices.push(state);
            result.errors.extend(warnings);
        }
        Ok((_, warnings)) => result.errors.extend(warnings),
        Err(QueryFailure::Unsupported {
            mut evidence,
            auxiliary,
        }) => {
            if ambiguous {
                evidence = UnsupportedEvidence::Partial;
            }
            result.errors.push(PollError {
                device_key: device.key.clone(),
                display_name: display_name(device),
                pid: device.pid,
                scope: PollErrorScope::Device,
                component: None,
                kind: match evidence {
                    UnsupportedEvidence::Conclusive => PollErrorKind::Unsupported,
                    UnsupportedEvidence::Partial => PollErrorKind::PartialUnsupported,
                },
                message: match evidence {
                    UnsupportedEvidence::Conclusive => {
                        "battery status is not supported by this device".to_string()
                    }
                    UnsupportedEvidence::Partial => {
                        "one or more battery probes reported unsupported status".to_string()
                    }
                },
            });
            result
                .errors
                .extend(auxiliary.into_iter().map(|err| poll_error(device, err)));
        }
        Err(QueryFailure::Failed { errors }) => result
            .errors
            .extend(errors.into_iter().map(|error| poll_error(device, error))),
    }
}

fn query_device(
    api: &HidApi,
    device: &DiscoveredDevice,
    pid_cache: &mut PidCache,
    cache_changed: &mut bool,
) -> std::result::Result<(BatteryState, Vec<PollError>), QueryFailure> {
    let known = device_map::known_device_support(device.pid);
    let display_name = display_name(device);
    let (candidates, truncation_warning) = candidate_probe_plan(&device.candidates);
    let mut failures: Vec<_> = truncation_warning
        .into_iter()
        .map(anyhow::Error::msg)
        .collect();
    let overall_deadline = Instant::now() + DEVICE_POLL_BUDGET;
    let mut transports = Vec::new();
    for candidate in candidates {
        if Instant::now() >= overall_deadline {
            failures.push(anyhow::anyhow!(
                "poll time budget exhausted while opening interfaces"
            ));
            break;
        }
        match api.open_path(candidate.path.as_c_str()) {
            Ok(handle) => match HidTransport::new(handle) {
                Ok(transport) => transports.push((candidate.interface_number, transport)),
                Err(err) => failures.push(anyhow::anyhow!(
                    "interface {} transport unavailable: {err:#}",
                    candidate.interface_number
                )),
            },
            Err(err) => failures.push(anyhow::anyhow!(
                "interface {} could not be opened: {err}",
                candidate.interface_number
            )),
        }
    }

    if transports.is_empty() {
        if pid_cache.get(device.pid).is_some() {
            *cache_changed |= pid_cache.remove(device.pid);
        }
        return Err(no_open_transport_failure(failures));
    }

    let cached = pid_cache.get(device.pid);
    let transaction_ids = battery_transaction_ids(cached, known);
    let battery_probe = probe_request_with(
        &transports,
        &transaction_ids,
        build_battery_request,
        response_wait(device.pid),
        overall_deadline,
    );

    let ProbeSuccess {
        candidate_index,
        transaction_id,
        report: battery_report,
    } = match battery_probe {
        Ok(found) => found,
        Err(failure) => {
            if cached.is_some() {
                *cache_changed |= pid_cache.remove(device.pid);
            }
            return Err(merge_query_failure(failure, failures));
        }
    };
    let observed_at = Instant::now();

    let generated = known.map(|support| support.transaction_id);
    *cache_changed |=
        update_cache_after_success(pid_cache, device.pid, cached, generated, transaction_id);
    let battery_raw = battery_report.arguments[1];
    let battery_percent = scale_percent(battery_raw);
    let mut warnings = device_transport_warnings(device, failures);
    let (charge_state, charge_warnings) =
        if known.is_some_and(|support| !support.supports_charging_status) {
            (ChargeState::Unsupported, Vec::new())
        } else {
            prioritize_probe_candidate(&mut transports, candidate_index);
            let charge_plan = charge_probe_plan(&transports, device.interface_grouping);
            let charge_probe = probe_request_with(
                charge_plan.candidates,
                &[transaction_id],
                build_charging_request,
                response_wait(device.pid),
                overall_deadline,
            )
            .map_err(|failure| mark_unsupported_incomplete(failure, charge_plan.omitted));
            charge_query_result(device, charge_probe)
        };
    warnings.extend(charge_warnings);

    Ok((
        BatteryState {
            device_key: device.key.clone(),
            display_name,
            pid: device.pid,
            battery_raw,
            battery_percent,
            charge_state,
            observed_at: Some(observed_at),
        },
        warnings,
    ))
}

fn device_transport_warnings(
    device: &DiscoveredDevice,
    failures: Vec<anyhow::Error>,
) -> Vec<PollError> {
    failures
        .into_iter()
        .map(|error| poll_error(device, error))
        .collect()
}

fn no_open_transport_failure(mut failures: Vec<anyhow::Error>) -> QueryFailure {
    if failures.is_empty() {
        failures.push(anyhow::anyhow!(
            "candidate interfaces unavailable for battery query"
        ));
    }
    QueryFailure::Failed { errors: failures }
}

fn charge_query_result(
    device: &DiscoveredDevice,
    query: std::result::Result<ProbeSuccess, QueryFailure>,
) -> (ChargeState, Vec<PollError>) {
    match query {
        Ok(success) if success.report.arguments[1] > 0 => (ChargeState::Charging, Vec::new()),
        Ok(_) => (ChargeState::NotCharging, Vec::new()),
        Err(QueryFailure::Unsupported {
            evidence: UnsupportedEvidence::Conclusive,
            auxiliary,
        }) if auxiliary.is_empty() => (ChargeState::Unsupported, Vec::new()),
        Err(QueryFailure::Unsupported { auxiliary, .. }) => {
            let mut warnings = vec![PollError {
                device_key: device.key.clone(),
                display_name: display_name(device),
                pid: device.pid,
                scope: PollErrorScope::ChargeState,
                component: None,
                kind: PollErrorKind::PartialUnsupported,
                message: "one or more charging-status probes reported unsupported status"
                    .to_string(),
            }];
            warnings.extend(auxiliary.into_iter().map(|error| {
                contextual_poll_error(
                    device,
                    PollErrorScope::ChargeState,
                    error,
                    "charging status probe incomplete",
                )
            }));
            (ChargeState::Unavailable, warnings)
        }
        Err(QueryFailure::Failed { errors }) => {
            let warnings = errors
                .into_iter()
                .map(|error| {
                    contextual_poll_error(
                        device,
                        PollErrorScope::ChargeState,
                        error,
                        "charging status unavailable",
                    )
                })
                .collect();
            (ChargeState::Unavailable, warnings)
        }
    }
}

fn merge_query_failure(failure: QueryFailure, mut auxiliary: Vec<anyhow::Error>) -> QueryFailure {
    match failure {
        QueryFailure::Unsupported {
            mut evidence,
            auxiliary: errors,
        } => {
            if !auxiliary.is_empty() {
                evidence = UnsupportedEvidence::Partial;
            }
            auxiliary.extend(errors);
            QueryFailure::Unsupported {
                evidence,
                auxiliary,
            }
        }
        QueryFailure::Failed { errors } => {
            auxiliary.extend(errors);
            QueryFailure::Failed { errors: auxiliary }
        }
    }
}

fn candidate_probe_plan<T>(candidates: &[T]) -> (&[T], Option<String>) {
    let attempted = candidates.len().min(MAX_CANDIDATES_PER_DEVICE);
    let omitted = candidates.len() - attempted;
    let warning = (omitted > 0)
        .then(|| format!("{omitted} candidate interface(s) skipped by the bounded probe limit"));
    (&candidates[..attempted], warning)
}

fn prioritize_probe_candidate<T>(candidates: &mut [T], candidate_index: usize) {
    if candidate_index < candidates.len() {
        candidates[..=candidate_index].rotate_right(1);
    }
}

struct ChargeProbePlan<'a, T> {
    candidates: &'a [T],
    omitted: bool,
}

fn charge_probe_plan<T>(
    candidates: &[T],
    interface_grouping: InterfaceGrouping,
) -> ChargeProbePlan<'_, T> {
    let count = interface_grouping.charge_candidate_count(candidates.len());
    ChargeProbePlan {
        candidates: &candidates[..count],
        omitted: count < candidates.len(),
    }
}

fn mark_unsupported_incomplete(failure: QueryFailure, incomplete: bool) -> QueryFailure {
    match failure {
        QueryFailure::Unsupported {
            evidence: UnsupportedEvidence::Conclusive,
            auxiliary,
        } if incomplete => QueryFailure::Unsupported {
            evidence: UnsupportedEvidence::Partial,
            auxiliary,
        },
        failure => failure,
    }
}

fn battery_transaction_ids(
    cached: Option<u8>,
    known: Option<&device_map::DeviceSupport>,
) -> Vec<u8> {
    let mut transaction_ids = Vec::with_capacity(5);
    if let Some(tx) = cached {
        transaction_ids.push(tx);
    }
    if let Some(support) = known
        && !transaction_ids.contains(&support.transaction_id)
    {
        transaction_ids.push(support.transaction_id);
    }
    for tx in [0x1F_u8, 0x3F_u8, 0xFF_u8] {
        if !transaction_ids.contains(&tx) {
            transaction_ids.push(tx);
        }
    }
    transaction_ids
}

enum ProbeState {
    Pending,
    Retry(anyhow::Error),
    Failed(anyhow::Error),
    Unsupported,
}

impl ProbeState {
    fn is_active(&self) -> bool {
        matches!(self, Self::Pending | Self::Retry(_))
    }
}

fn probe_request_with<T: FeatureTransport, F: Fn(u8) -> RazerReport>(
    candidates: &[(i32, T)],
    transaction_ids: &[u8],
    build_request: F,
    response_wait: Duration,
    deadline: Instant,
) -> std::result::Result<ProbeSuccess, QueryFailure> {
    let target_count = candidates.len() * transaction_ids.len();
    let mut states: Vec<_> = (0..target_count).map(|_| ProbeState::Pending).collect();
    let mut budget_exhausted = false;

    'rounds: for round in 0..MAX_RETRIES {
        for (transaction_index, transaction_id) in transaction_ids.iter().copied().enumerate() {
            for (candidate_index, (_, transport)) in candidates.iter().enumerate() {
                let target_index = transaction_index * candidates.len() + candidate_index;
                if !states[target_index].is_active() {
                    continue;
                }

                let now = Instant::now();
                if now >= deadline || deadline.saturating_duration_since(now) < response_wait {
                    budget_exhausted = true;
                    break 'rounds;
                }

                let request = build_request(transaction_id);
                let operation_timeout =
                    FEATURE_IO_TIMEOUT.min(deadline.saturating_duration_since(now));
                states[target_index] = match attempt_request_with(
                    transport,
                    &request,
                    response_wait,
                    operation_timeout,
                ) {
                    RequestAttempt::Success(report) => {
                        return Ok(ProbeSuccess {
                            candidate_index,
                            transaction_id,
                            report,
                        });
                    }
                    RequestAttempt::Retry(err) => ProbeState::Retry(err),
                    RequestAttempt::Failed(err) => ProbeState::Failed(err),
                    RequestAttempt::Unsupported => ProbeState::Unsupported,
                };
            }
        }

        if round + 1 >= MAX_RETRIES || !states.iter().any(ProbeState::is_active) {
            break;
        }
        if let Some((_, transport)) = candidates.first() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            transport.pause(RETRY_DELAY.min(remaining));
        }
    }

    let mut failures = Vec::new();
    for (transaction_index, transaction_id) in transaction_ids.iter().copied().enumerate() {
        for (candidate_index, (interface_number, _)) in candidates.iter().enumerate() {
            let target_index = transaction_index * candidates.len() + candidate_index;
            let error = match &states[target_index] {
                ProbeState::Retry(err) | ProbeState::Failed(err) => Some(err),
                ProbeState::Pending | ProbeState::Unsupported => None,
            };
            if let Some(err) = error {
                failures.push(anyhow::anyhow!(
                    "interface {interface_number}, tx 0x{transaction_id:02X}: {}",
                    format_error_chain(err)
                ));
            }
        }
    }
    if budget_exhausted {
        failures.push(anyhow::anyhow!("poll time budget exhausted"));
    }
    let unsupported_observed = states
        .iter()
        .any(|state| matches!(state, ProbeState::Unsupported));
    if unsupported_observed {
        let evidence = if target_count > 0
            && states
                .iter()
                .all(|state| matches!(state, ProbeState::Unsupported))
        {
            UnsupportedEvidence::Conclusive
        } else {
            UnsupportedEvidence::Partial
        };
        return Err(QueryFailure::Unsupported {
            evidence,
            auxiliary: failures,
        });
    }
    if failures.is_empty() {
        failures.push(anyhow::anyhow!(
            "candidate interfaces unavailable for battery query"
        ));
    }
    Err(QueryFailure::Failed { errors: failures })
}

fn update_cache_after_success(
    pid_cache: &mut PidCache,
    pid: u16,
    cached: Option<u8>,
    generated: Option<u8>,
    successful: u8,
) -> bool {
    if cached == Some(successful) {
        false
    } else if generated == Some(successful) {
        pid_cache.remove(pid)
    } else {
        pid_cache.set(pid, successful)
    }
}

trait FeatureTransport {
    fn exchange(
        &self,
        request: &[u8],
        response: &mut [u8],
        response_wait: Duration,
        operation_timeout: Duration,
    ) -> Result<usize>;

    fn pause(&self, duration: Duration);
}

trait BlockingFeatureDevice: Send + 'static {
    fn exchange_blocking(
        &self,
        request: &[u8],
        response: &mut [u8],
        response_wait: Duration,
    ) -> Result<usize>;
}

impl BlockingFeatureDevice for HidDevice {
    fn exchange_blocking(
        &self,
        request: &[u8],
        response: &mut [u8],
        response_wait: Duration,
    ) -> Result<usize> {
        self.send_feature_report(request)
            .context("send_feature_report failed")?;
        thread::sleep(response_wait);
        self.get_feature_report(response)
            .context("get_feature_report failed")
    }
}

struct FeatureCommand {
    request: Vec<u8>,
    response: Vec<u8>,
    response_wait: Duration,
    reply: mpsc::Sender<Result<(usize, Vec<u8>)>>,
}

struct FeatureOperationPermit;

impl FeatureOperationPermit {
    fn acquire() -> Option<Self> {
        ACTIVE_FEATURE_OPERATIONS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_BLOCKED_FEATURE_OPERATIONS).then_some(active + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for FeatureOperationPermit {
    fn drop(&mut self) {
        ACTIVE_FEATURE_OPERATIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct HidTransport {
    commands: mpsc::Sender<FeatureCommand>,
    timed_out: AtomicBool,
}

impl HidTransport {
    fn new<D: BlockingFeatureDevice>(device: D) -> Result<Self> {
        let (commands, receiver) = mpsc::channel::<FeatureCommand>();
        thread::Builder::new()
            .name("razertray-hid-feature".to_string())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    let result = match FeatureOperationPermit::acquire() {
                        Some(_permit) => {
                            let mut response = command.response;
                            device
                                .exchange_blocking(
                                    &command.request,
                                    &mut response,
                                    command.response_wait,
                                )
                                .map(|count| (count, response))
                        }
                        None => Err(anyhow::anyhow!(
                            "HID feature operation unavailable because earlier operations remain blocked"
                        )),
                    };
                    let _ = command.reply.send(result);
                }
            })
            .context("failed spawning HID feature worker")?;
        Ok(Self {
            commands,
            timed_out: AtomicBool::new(false),
        })
    }
}

impl FeatureTransport for HidTransport {
    fn exchange(
        &self,
        request: &[u8],
        response: &mut [u8],
        response_wait: Duration,
        operation_timeout: Duration,
    ) -> Result<usize> {
        if self.timed_out.load(Ordering::Acquire) {
            anyhow::bail!("HID feature transport unavailable after a timed-out operation");
        }
        let (reply, receiver) = mpsc::channel();
        self.commands
            .send(FeatureCommand {
                request: request.to_vec(),
                response: response.to_vec(),
                response_wait,
                reply,
            })
            .context("HID feature worker unavailable")?;
        match receiver.recv_timeout(operation_timeout) {
            Ok(result) => {
                let (count, received) = result?;
                response.copy_from_slice(&received);
                Ok(count)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.timed_out.store(true, Ordering::Release);
                anyhow::bail!(
                    "HID feature operation unavailable after timing out at {} ms",
                    operation_timeout.as_millis()
                )
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("HID feature worker unavailable")
            }
        }
    }

    fn pause(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

enum RequestAttempt {
    Success(RazerReport),
    Retry(anyhow::Error),
    Failed(anyhow::Error),
    Unsupported,
}

fn attempt_request_with<T: FeatureTransport>(
    transport: &T,
    request: &RazerReport,
    response_wait: Duration,
    operation_timeout: Duration,
) -> RequestAttempt {
    let request_payload = feature_report_payload(request);
    let mut response_buffer = [0u8; FEATURE_REPORT_LENGTH];
    response_buffer[0] = 0x00;
    let count = match transport.exchange(
        &request_payload,
        &mut response_buffer,
        response_wait,
        operation_timeout,
    ) {
        Ok(count) => count,
        Err(err) => return RequestAttempt::Retry(err),
    };
    if count != FEATURE_REPORT_LENGTH {
        return RequestAttempt::Retry(anyhow::anyhow!(
            "expected {} bytes, got {count}",
            FEATURE_REPORT_LENGTH
        ));
    }

    let response = match RazerReport::from_bytes(&response_buffer[1..]) {
        Ok(response) => response,
        Err(err) => return RequestAttempt::Retry(err.context("invalid response report")),
    };
    if !response.is_valid_crc() {
        return RequestAttempt::Retry(anyhow::anyhow!("invalid response crc"));
    }
    if !expected_response_matches(request, &response) {
        return RequestAttempt::Retry(anyhow::anyhow!("response did not match request"));
    }

    match response.status {
        STATUS_SUCCESSFUL => RequestAttempt::Success(response),
        STATUS_BUSY => RequestAttempt::Retry(anyhow::anyhow!("device returned STATUS_BUSY")),
        STATUS_NO_RESPONSE => RequestAttempt::Retry(anyhow::anyhow!("device returned no response")),
        STATUS_FAILURE => RequestAttempt::Failed(anyhow::anyhow!("device returned STATUS_FAILURE")),
        STATUS_NOT_SUPPORTED => RequestAttempt::Unsupported,
        other => RequestAttempt::Failed(anyhow::anyhow!("unexpected status: 0x{other:02X}")),
    }
}

fn response_wait(pid: u16) -> Duration {
    match pid {
        // Atheris and Orochi receivers use the long wait from OpenRazer.
        0x0062 | 0x0088 | 0x0089 => Duration::from_millis(400),
        _ => SEND_DELAY,
    }
}

fn display_name(device: &DiscoveredDevice) -> String {
    let name = device_map::known_device_support(device.pid).map_or_else(
        || device.product_name.clone(),
        |support| support.name.to_string(),
    );
    format!("{name}{}", device.interface_grouping.display_suffix())
}

fn poll_error(device: &DiscoveredDevice, err: anyhow::Error) -> PollError {
    scoped_poll_error(device, PollErrorScope::Device, err)
}

fn scoped_poll_error(
    device: &DiscoveredDevice,
    scope: PollErrorScope,
    err: anyhow::Error,
) -> PollError {
    PollError {
        device_key: device.key.clone(),
        display_name: display_name(device),
        pid: device.pid,
        scope,
        component: None,
        kind: PollErrorKind::classify_message(&format_error_chain(&err)),
        message: format_error_chain(&err),
    }
}

fn contextual_poll_error(
    device: &DiscoveredDevice,
    scope: PollErrorScope,
    err: anyhow::Error,
    context: &'static str,
) -> PollError {
    let kind = PollErrorKind::classify_message(&format_error_chain(&err));
    let err = err.context(context);
    PollError {
        device_key: device.key.clone(),
        display_name: display_name(device),
        pid: device.pid,
        scope,
        component: None,
        kind,
        message: format_error_chain(&err),
    }
}

fn format_error_chain(err: &anyhow::Error) -> String {
    format!("{err:#}")
}

fn scale_percent(raw: u8) -> u8 {
    // Match OpenRazer's user-facing conversion, which truncates: the daemon
    // computes (raw / 255) * 100 as a float and pylib applies int() to it
    // (floor), so a raw of 254 reads as 99%, not 100%.
    (raw as u16 * 100 / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::{
        BlockingFeatureDevice, FeatureTransport, HidTransport, MAX_RETRIES, QueryFailure,
        UnsupportedEvidence, candidate_probe_plan, charge_probe_plan, charge_query_result,
        device_transport_warnings, display_name, format_error_chain, mark_unsupported_incomplete,
        merge_query_failure, no_open_transport_failure, prioritize_probe_candidate,
        probe_request_with, record_query_result, scale_percent, update_cache_after_success,
    };
    use crate::config::PidCache;
    use crate::hid::protocol::{
        FEATURE_REPORT_LENGTH, STATUS_BUSY, STATUS_NO_RESPONSE, STATUS_NOT_SUPPORTED,
        STATUS_SUCCESSFUL, build_battery_request, build_charging_request,
    };
    use crate::hid::scanner::{DiscoveredDevice, InterfaceGrouping};
    use crate::model::{PollErrorKind, PollErrorScope, PollResult};
    use anyhow::{Result, bail};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    struct FakeTransport {
        replies: RefCell<VecDeque<Result<Vec<u8>>>>,
        attempts: Cell<usize>,
    }

    impl FakeTransport {
        fn new(replies: Vec<Result<Vec<u8>>>) -> Self {
            Self {
                replies: RefCell::new(replies.into()),
                attempts: Cell::new(0),
            }
        }
    }

    impl FeatureTransport for FakeTransport {
        fn exchange(
            &self,
            _request: &[u8],
            response: &mut [u8],
            _response_wait: Duration,
            _operation_timeout: Duration,
        ) -> Result<usize> {
            self.attempts.set(self.attempts.get() + 1);
            let Some(reply) = self.replies.borrow_mut().pop_front() else {
                bail!("no fake response")
            };
            let bytes = reply?;
            let count = bytes.len();
            response[..count].copy_from_slice(&bytes);
            Ok(count)
        }

        fn pause(&self, _duration: Duration) {}
    }

    struct RecordingTransport {
        candidate_index: usize,
        attempts: Rc<RefCell<Vec<(usize, u8)>>>,
    }

    impl FeatureTransport for RecordingTransport {
        fn exchange(
            &self,
            request: &[u8],
            response: &mut [u8],
            _response_wait: Duration,
            _operation_timeout: Duration,
        ) -> Result<usize> {
            let transaction_id = request[2];
            self.attempts
                .borrow_mut()
                .push((self.candidate_index, transaction_id));
            let status = if self.candidate_index == 0 && transaction_id == 0xFF {
                STATUS_SUCCESSFUL
            } else {
                STATUS_NO_RESPONSE
            };
            let bytes = feature_response_with_status(transaction_id, 180, status);
            response.copy_from_slice(&bytes);
            Ok(bytes.len())
        }

        fn pause(&self, _duration: Duration) {}
    }

    struct SlowFeatureDevice;

    impl BlockingFeatureDevice for SlowFeatureDevice {
        fn exchange_blocking(
            &self,
            _request: &[u8],
            response: &mut [u8],
            _response_wait: Duration,
        ) -> Result<usize> {
            std::thread::sleep(Duration::from_millis(500));
            Ok(response.len())
        }
    }

    fn feature_response_with_status(transaction_id: u8, battery_raw: u8, status: u8) -> Vec<u8> {
        let mut report = build_battery_request(transaction_id);
        report.status = status;
        report.arguments[1] = battery_raw;
        report.crc = report.calculate_crc();
        let mut bytes = vec![0; FEATURE_REPORT_LENGTH];
        bytes[1..].copy_from_slice(&report.to_bytes());
        bytes
    }

    fn feature_response(transaction_id: u8, battery_raw: u8) -> Vec<u8> {
        feature_response_with_status(transaction_id, battery_raw, STATUS_SUCCESSFUL)
    }

    fn charging_response_with_status(transaction_id: u8, charging: u8, status: u8) -> Vec<u8> {
        let mut report = build_charging_request(transaction_id);
        report.status = status;
        report.arguments[1] = charging;
        report.crc = report.calculate_crc();
        let mut bytes = vec![0; FEATURE_REPORT_LENGTH];
        bytes[1..].copy_from_slice(&report.to_bytes());
        bytes
    }

    #[test]
    fn scaling_truncates_like_reference() {
        // OpenRazer floors the percentage (int((raw / 255) * 100)); these
        // include the boundary cases where rounding would disagree (127, 254).
        assert_eq!(scale_percent(0), 0);
        assert_eq!(scale_percent(1), 0);
        assert_eq!(scale_percent(127), 49);
        assert_eq!(scale_percent(128), 50);
        assert_eq!(scale_percent(191), 74);
        assert_eq!(scale_percent(254), 99);
        assert_eq!(scale_percent(255), 100);
    }

    #[test]
    fn transport_retries_invalid_crc_then_uses_valid_response() {
        let mut invalid = feature_response(0x1F, 127);
        invalid[89] ^= 0x01;
        let candidates = [(
            0,
            FakeTransport::new(vec![Ok(invalid), Ok(feature_response(0x1F, 128))]),
        )];

        let success = probe_request_with(
            &candidates,
            &[0x1F],
            build_battery_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("second response should succeed");

        assert_eq!(success.report.arguments[1], 128);
        assert_eq!(candidates[0].1.attempts.get(), 2);
    }

    #[test]
    fn transport_retries_read_failure() {
        let candidates = [(
            0,
            FakeTransport::new(vec![
                Err(anyhow::anyhow!("receiver asleep")),
                Ok(feature_response(0x3F, 200)),
            ]),
        )];

        let success = probe_request_with(
            &candidates,
            &[0x3F],
            build_battery_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("retry should recover");

        assert_eq!(success.report.arguments[1], 200);
        assert_eq!(candidates[0].1.attempts.get(), 2);
    }

    #[test]
    fn review_round_24_hid_feature_operation_is_time_bounded() {
        let transport = HidTransport::new(SlowFeatureDevice).expect("start feature worker");
        let mut response = [0; FEATURE_REPORT_LENGTH];
        let started = Instant::now();

        let error = transport
            .exchange(
                &[0; FEATURE_REPORT_LENGTH],
                &mut response,
                Duration::ZERO,
                Duration::from_millis(20),
            )
            .expect_err("slow feature operation should time out");

        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(format!("{error:#}").contains("timing out"));
    }

    #[test]
    fn review_round_24_battery_transport_failures_keep_device_scope() {
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };
        let mut warnings = device_transport_warnings(
            &device,
            vec![anyhow::anyhow!(
                "interface 1 could not be opened: access denied"
            )],
        );
        let (_, charge_warnings) = charge_query_result(
            &device,
            Err(QueryFailure::Failed {
                errors: vec![anyhow::anyhow!("charging response timed out")],
            }),
        );
        warnings.extend(charge_warnings);

        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].scope, PollErrorScope::Device);
        assert_eq!(warnings[0].kind, PollErrorKind::AccessDenied);
        assert_eq!(warnings[1].scope, PollErrorScope::ChargeState);
    }

    #[test]
    fn review_transport_rejects_response_from_different_transaction() {
        let candidates = [(
            0,
            FakeTransport::new(vec![
                Ok(feature_response(0x1F, 200)),
                Ok(feature_response(0x3F, 120)),
            ]),
        )];

        let success = probe_request_with(
            &candidates,
            &[0x3F],
            build_battery_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("matching response should succeed after delayed response is rejected");

        assert_eq!(success.transaction_id, 0x3F);
        assert_eq!(success.report.transaction_id, 0x3F);
        assert_eq!(success.report.arguments[1], 120);
        assert_eq!(candidates[0].1.attempts.get(), 2);
    }

    #[test]
    fn review_transport_retries_busy_then_uses_completed_response() {
        let candidates = [(
            0,
            FakeTransport::new(vec![
                Ok(feature_response_with_status(0x1F, 180, STATUS_BUSY)),
                Ok(feature_response(0x1F, 120)),
            ]),
        )];

        let success = probe_request_with(
            &candidates,
            &[0x1F],
            build_battery_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("completed response should succeed after busy status");

        assert_eq!(success.report.arguments[1], 120);
        assert_eq!(candidates[0].1.attempts.get(), 2);
    }

    #[test]
    fn review_transport_rejects_busy_payload_after_retry_budget() {
        let candidates = [(
            0,
            FakeTransport::new(
                (0..MAX_RETRIES)
                    .map(|_| Ok(feature_response_with_status(0x1F, 180, STATUS_BUSY)))
                    .collect(),
            ),
        )];

        let failure = probe_request_with(
            &candidates,
            &[0x1F],
            build_battery_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("busy payload must not become a reading");

        assert!(matches!(failure, QueryFailure::Failed { .. }));
        assert_eq!(candidates[0].1.attempts.get(), MAX_RETRIES);
    }

    #[test]
    fn review_short_feature_report_is_a_protocol_diagnostic() {
        let candidates = [(
            0,
            FakeTransport::new(
                (0..MAX_RETRIES)
                    .map(|_| Ok(vec![0; FEATURE_REPORT_LENGTH - 1]))
                    .collect(),
            ),
        )];
        let failure = probe_request_with(
            &candidates,
            &[0x1F],
            build_battery_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("short reports must not become readings");
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };
        let mut result = PollResult::default();

        record_query_result(&mut result, &device, Err(failure));

        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].kind, PollErrorKind::Protocol);
        assert!(
            result.errors[0]
                .message
                .contains("expected 91 bytes, got 90")
        );
    }

    #[test]
    fn review_open_failures_remain_the_only_empty_transport_diagnostics() {
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };
        let mut result = PollResult::default();

        record_query_result(
            &mut result,
            &device,
            Err(no_open_transport_failure(vec![anyhow::anyhow!(
                "interface 2 could not be opened: access denied"
            )])),
        );

        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].kind, PollErrorKind::AccessDenied);
        assert!(result.errors[0].message.contains("interface 2"));
    }

    #[test]
    fn known_transaction_replaces_stale_cached_probe() {
        let mut cache = PidCache::default();
        cache.set(0x1234, 0x1F);

        assert!(update_cache_after_success(
            &mut cache,
            0x1234,
            Some(0x1F),
            Some(0x3F),
            0x3F,
        ));
        assert_eq!(cache.get(0x1234), None);
    }

    #[test]
    fn transport_preserves_explicit_not_supported_result() {
        let candidates = [(
            0,
            FakeTransport::new(vec![Ok(feature_response_with_status(
                0x1F,
                0,
                STATUS_NOT_SUPPORTED,
            ))]),
        )];

        let failure = probe_request_with(
            &candidates,
            &[0x1F],
            build_battery_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("unsupported command should remain typed");

        assert!(matches!(
            failure,
            QueryFailure::Unsupported {
                evidence: UnsupportedEvidence::Conclusive,
                auxiliary,
            } if auxiliary.is_empty()
        ));
    }

    #[test]
    fn review_unsupported_query_preserves_auxiliary_diagnostic() {
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };
        let mut result = PollResult::default();

        let failure = merge_query_failure(
            QueryFailure::Unsupported {
                evidence: UnsupportedEvidence::Conclusive,
                auxiliary: Vec::new(),
            },
            vec![anyhow::anyhow!("interface access denied")],
        );
        record_query_result(&mut result, &device, Err(failure));

        assert_eq!(result.errors.len(), 2);
        assert_eq!(result.errors[0].scope, PollErrorScope::Device);
        assert_eq!(result.errors[0].kind, PollErrorKind::PartialUnsupported);
        assert_eq!(result.errors[0].device_key, "mouse");
        assert_eq!(result.errors[0].pid, 0xFFFF);
        assert!(
            result.errors[0]
                .message
                .contains("one or more battery probes")
        );
        assert_eq!(result.errors[1].kind, PollErrorKind::AccessDenied);
        let json = serde_json::to_value(result).expect("serialize poll result");
        assert_eq!(json["errors"][0]["kind"], "partial-unsupported");
        assert_eq!(json["errors"][0]["scope"], "device");
        assert_eq!(json["errors"][1]["kind"], "access-denied");
    }

    #[test]
    fn diagnostic_classification_preserves_transport_source() {
        let replies = (0..MAX_RETRIES)
            .map(|_| {
                Err(anyhow::anyhow!("access denied by operating system")
                    .context("send_feature_report failed"))
            })
            .collect();
        let candidates = [(0, FakeTransport::new(replies))];

        let failure = probe_request_with(
            &candidates,
            &[0x1F],
            build_battery_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("transport retries should fail");
        let QueryFailure::Failed { errors } = failure else {
            panic!("transport error should remain a failed query")
        };

        assert_eq!(errors.len(), 1);
        assert_eq!(
            PollErrorKind::classify_message(&format_error_chain(&errors[0])),
            PollErrorKind::AccessDenied
        );
        assert!(format_error_chain(&errors[0]).contains("access denied by operating system"));
    }

    #[test]
    fn review_mixed_failures_preserve_each_typed_diagnostic() {
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };
        let failure = merge_query_failure(
            QueryFailure::Failed {
                errors: vec![anyhow::anyhow!("device returned STATUS_FAILURE")],
            },
            vec![anyhow::anyhow!(
                "interface 0 could not be opened: access denied"
            )],
        );
        let mut result = PollResult::default();

        record_query_result(&mut result, &device, Err(failure));

        assert_eq!(result.errors.len(), 2);
        assert_eq!(result.errors[0].kind, PollErrorKind::AccessDenied);
        assert_eq!(result.errors[1].kind, PollErrorKind::Protocol);
        let json = serde_json::to_value(result).expect("serialize poll result");
        assert_eq!(json["errors"][0]["kind"], "access-denied");
        assert_eq!(json["errors"][1]["kind"], "protocol");
    }

    #[test]
    fn review_hid_probes_every_transaction_before_retries() {
        let attempts = Rc::new(RefCell::new(Vec::new()));
        let candidates: Vec<_> = (0..4)
            .map(|candidate_index| {
                (
                    candidate_index as i32,
                    RecordingTransport {
                        candidate_index,
                        attempts: Rc::clone(&attempts),
                    },
                )
            })
            .collect();

        let success = probe_request_with(
            &candidates,
            &[0x1F, 0x3F, 0xFF],
            build_battery_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("0xFF fallback should succeed before any retry");

        assert_eq!(success.candidate_index, 0);
        assert_eq!(success.transaction_id, 0xFF);
        assert_eq!(success.report.arguments[1], 180);
        assert_eq!(
            *attempts.borrow(),
            vec![
                (0, 0x1F),
                (1, 0x1F),
                (2, 0x1F),
                (3, 0x1F),
                (0, 0x3F),
                (1, 0x3F),
                (2, 0x3F),
                (3, 0x3F),
                (0, 0xFF),
            ]
        );
    }

    #[test]
    fn review_hid_preserves_all_unsupported_probe_result() {
        let unsupported_replies = || {
            [0x1F, 0x3F, 0xFF]
                .into_iter()
                .map(|transaction_id| {
                    Ok(feature_response_with_status(
                        transaction_id,
                        0,
                        STATUS_NOT_SUPPORTED,
                    ))
                })
                .collect()
        };
        let candidates = [
            (0, FakeTransport::new(unsupported_replies())),
            (1, FakeTransport::new(unsupported_replies())),
        ];

        let result = probe_request_with(
            &candidates,
            &[0x1F, 0x3F, 0xFF],
            build_battery_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        );

        assert!(matches!(
            result,
            Err(QueryFailure::Unsupported {
                evidence: UnsupportedEvidence::Conclusive,
                auxiliary,
            }) if auxiliary.is_empty()
        ));
    }

    #[test]
    fn review_hid_preserves_mixed_unsupported_and_timeout_evidence() {
        let mut replies = vec![Ok(feature_response_with_status(
            0x1F,
            0,
            STATUS_NOT_SUPPORTED,
        ))];
        replies.extend(
            (0..MAX_RETRIES).map(|_| Ok(feature_response_with_status(0x3F, 0, STATUS_NO_RESPONSE))),
        );
        let candidates = [(0, FakeTransport::new(replies))];

        let failure = probe_request_with(
            &candidates,
            &[0x1F, 0x3F],
            build_battery_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("mixed evidence must not collapse to a generic failure");
        assert!(matches!(
            &failure,
            QueryFailure::Unsupported {
                evidence: UnsupportedEvidence::Partial,
                auxiliary,
            } if !auxiliary.is_empty()
        ));

        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };
        let mut result = PollResult::default();
        record_query_result(&mut result, &device, Err(failure));

        assert_eq!(result.errors.len(), 2);
        assert_eq!(result.errors[0].kind, PollErrorKind::PartialUnsupported);
        assert_eq!(result.errors[1].kind, PollErrorKind::DeviceUnavailable);
    }

    #[test]
    fn review_charge_probe_uses_a_supported_fallback_candidate() {
        let candidates = [
            (
                0,
                FakeTransport::new(vec![Ok(charging_response_with_status(
                    0x1F,
                    0,
                    STATUS_NOT_SUPPORTED,
                ))]),
            ),
            (
                1,
                FakeTransport::new(vec![Ok(charging_response_with_status(
                    0x1F,
                    1,
                    STATUS_SUCCESSFUL,
                ))]),
            ),
        ];
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };

        let query = probe_request_with(
            &candidates,
            &[0x1F],
            build_charging_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        );
        let (state, warnings) = charge_query_result(&device, query);

        assert_eq!(state, crate::model::ChargeState::Charging);
        assert!(warnings.is_empty());
        assert_eq!(candidates[0].1.attempts.get(), 1);
        assert_eq!(candidates[1].1.attempts.get(), 1);
    }

    #[test]
    fn review_charge_probe_prioritizes_battery_winning_candidate() {
        let mut candidates = [
            (
                0,
                FakeTransport::new(vec![Ok(charging_response_with_status(
                    0x1F,
                    0,
                    STATUS_NO_RESPONSE,
                ))]),
            ),
            (
                1,
                FakeTransport::new(vec![Ok(charging_response_with_status(
                    0x1F,
                    1,
                    STATUS_SUCCESSFUL,
                ))]),
            ),
        ];

        prioritize_probe_candidate(&mut candidates, 1);
        let success = probe_request_with(
            &candidates,
            &[0x1F],
            build_charging_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("battery-winning candidate should answer first");

        assert_eq!(candidates[0].0, 1);
        assert_eq!(success.candidate_index, 0);
        assert_eq!(success.report.arguments[1], 1);
        assert_eq!(candidates[0].1.attempts.get(), 1);
        assert_eq!(candidates[1].1.attempts.get(), 0);
    }

    #[test]
    fn review_round_22_ambiguous_charge_is_incomplete() {
        let candidates = [0, 1, 2];

        let ambiguous = charge_probe_plan(&candidates, InterfaceGrouping::AmbiguousSerialless);
        assert_eq!(ambiguous.candidates, &[0]);
        assert!(ambiguous.omitted);

        let failure = mark_unsupported_incomplete(
            QueryFailure::Unsupported {
                evidence: UnsupportedEvidence::Conclusive,
                auxiliary: Vec::new(),
            },
            ambiguous.omitted,
        );
        assert!(matches!(
            &failure,
            QueryFailure::Unsupported {
                evidence: UnsupportedEvidence::Partial,
                ..
            }
        ));

        let device = DiscoveredDevice {
            key: "00BF".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::AmbiguousSerialless,
            candidates: Vec::new(),
        };
        let (state, warnings) = charge_query_result(&device, Err(failure));
        assert_eq!(state, crate::model::ChargeState::Unavailable);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].kind, PollErrorKind::PartialUnsupported);

        let verified = charge_probe_plan(&candidates, InterfaceGrouping::VerifiedDevice);
        assert_eq!(verified.candidates, &[0, 1, 2]);
        assert!(!verified.omitted);
    }

    #[test]
    fn review_round_22_ambiguous_group_does_not_emit_device_reading() {
        let device = DiscoveredDevice {
            key: "00BF".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::AmbiguousSerialless,
            candidates: Vec::new(),
        };
        let mut result = PollResult::default();
        record_query_result(
            &mut result,
            &device,
            Ok((
                crate::model::BatteryState {
                    device_key: device.key.clone(),
                    display_name: display_name(&device),
                    pid: device.pid,
                    battery_raw: 128,
                    battery_percent: 50,
                    charge_state: crate::model::ChargeState::Unavailable,
                    observed_at: None,
                },
                Vec::new(),
            )),
        );

        assert!(result.devices.is_empty());
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].device_key, "00BF");
        assert_eq!(result.errors[0].kind, PollErrorKind::AmbiguousIdentity);
        assert_eq!(
            result.errors[0].display_name,
            "Razer Mouse (identity ambiguous)"
        );
        let json = serde_json::to_value(result).expect("serialize ambiguous result");
        assert_eq!(json["devices"], serde_json::json!([]));
        assert_eq!(json["errors"][0]["kind"], "ambiguous-identity");
    }

    #[test]
    fn review_charge_probe_preserves_partial_unsupported_evidence() {
        let candidates = [
            (
                0,
                FakeTransport::new(vec![Ok(charging_response_with_status(
                    0x1F,
                    0,
                    STATUS_NOT_SUPPORTED,
                ))]),
            ),
            (
                1,
                FakeTransport::new(
                    (0..MAX_RETRIES)
                        .map(|_| Ok(charging_response_with_status(0x1F, 0, STATUS_NO_RESPONSE)))
                        .collect(),
                ),
            ),
        ];
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };

        let query = probe_request_with(
            &candidates,
            &[0x1F],
            build_charging_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        );
        let (state, warnings) = charge_query_result(&device, query);

        assert_eq!(state, crate::model::ChargeState::Unavailable);
        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].scope, PollErrorScope::ChargeState);
        assert_eq!(warnings[0].kind, PollErrorKind::PartialUnsupported);
        assert_eq!(warnings[1].scope, PollErrorScope::ChargeState);
        assert_eq!(warnings[1].kind, PollErrorKind::DeviceUnavailable);
    }

    #[test]
    fn review_charge_context_preserves_underlying_protocol_kind() {
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };
        let query = Err(QueryFailure::Failed {
            errors: vec![anyhow::anyhow!("invalid response crc")],
        });

        let (state, warnings) = charge_query_result(&device, query);

        assert_eq!(state, crate::model::ChargeState::Unavailable);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].scope, PollErrorScope::ChargeState);
        assert_eq!(warnings[0].kind, PollErrorKind::Protocol);
        assert!(warnings[0].message.contains("charging status unavailable"));
        assert!(warnings[0].message.contains("invalid response crc"));
    }

    #[test]
    fn review_candidate_truncation_prevents_conclusive_unsupported() {
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };
        let candidates = [0, 1, 2, 3, 4];
        let (attempted, warning) = candidate_probe_plan(&candidates);
        let warning = warning.expect("fifth candidate is omitted");
        let failure = merge_query_failure(
            QueryFailure::Unsupported {
                evidence: UnsupportedEvidence::Conclusive,
                auxiliary: Vec::new(),
            },
            vec![anyhow::Error::msg(warning)],
        );
        let mut result = PollResult::default();

        record_query_result(&mut result, &device, Err(failure));

        assert_eq!(attempted, &[0, 1, 2, 3]);
        assert_eq!(result.errors[0].kind, PollErrorKind::PartialUnsupported);
        assert!(
            result.errors[0]
                .message
                .contains("one or more battery probes")
        );
        assert!(result.errors[1].message.contains("1 candidate interface"));
    }

    #[test]
    fn review_charge_probe_requires_conclusive_unsupported_evidence() {
        let candidates = [
            (
                0,
                FakeTransport::new(vec![Ok(charging_response_with_status(
                    0x1F,
                    0,
                    STATUS_NOT_SUPPORTED,
                ))]),
            ),
            (
                1,
                FakeTransport::new(vec![Ok(charging_response_with_status(
                    0x1F,
                    0,
                    STATUS_NOT_SUPPORTED,
                ))]),
            ),
        ];
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };

        let query = probe_request_with(
            &candidates,
            &[0x1F],
            build_charging_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        );
        let (state, warnings) = charge_query_result(&device, query);

        assert_eq!(state, crate::model::ChargeState::Unsupported);
        assert!(warnings.is_empty());
    }
}
