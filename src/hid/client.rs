use crate::config::PidCache;
use crate::device_map;
use crate::hid::protocol::{
    FEATURE_REPORT_LENGTH, RazerReport, STATUS_BUSY, STATUS_FAILURE, STATUS_NO_RESPONSE,
    STATUS_NOT_SUPPORTED, STATUS_SUCCESSFUL, build_battery_request, build_charging_request,
    expected_response_matches, feature_report_payload,
};
use crate::hid::scanner::{DiscoveredDevice, InterfaceGrouping, scan_devices};
use crate::hid::worker;
use crate::model::{
    BatteryState, ChargeState, PollError, PollErrorKind, PollErrorScope, PollResult,
};
use anyhow::Result;
use hidapi::HidApi;
use std::thread;
use std::time::{Duration, Instant};

const MAX_CANDIDATES_PER_DEVICE: usize = 4;
const MAX_RETRIES: usize = 5;
const DEVICE_POLL_BUDGET: Duration = Duration::from_secs(8);
const FEATURE_IO_TIMEOUT: Duration = Duration::from_secs(1);
const FEATURE_IO_ALLOWANCE: Duration = Duration::from_millis(250);
const PROBE_SCHEDULING_ALLOWANCE: Duration = Duration::from_millis(250);
const MAX_PROBE_BUDGET: Duration = Duration::from_secs(16);
const SEND_DELAY: Duration = Duration::from_millis(60);
const RETRY_DELAY: Duration = Duration::from_millis(400);

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
    ProbeCoverage {
        failure: Box<QueryFailure>,
        omitted: usize,
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
    interface_failures: Vec<anyhow::Error>,
}

// Retain the direct in-process entry point for focused HID callers.
#[allow(dead_code)]
pub fn poll_devices(api: &HidApi, pid_cache: &mut PidCache) -> PollBatch {
    let discovered = scan_devices(api);
    poll_discovered_devices(discovered, pid_cache)
}

pub(crate) fn poll_discovered_devices(
    discovered: Vec<DiscoveredDevice>,
    pid_cache: &mut PidCache,
) -> PollBatch {
    let mut result = PollResult::default();
    let mut cache_changed = false;

    for device in discovered {
        let query = query_device(&device, pid_cache, &mut cache_changed);
        record_query_result(&mut result, &device, query);
    }

    result.sort_devices();
    PollBatch {
        result,
        cache_changed,
    }
}

pub(crate) fn maximum_poll_duration(device_count: usize) -> Duration {
    let device_count = u32::try_from(device_count).unwrap_or(u32::MAX);
    MAX_PROBE_BUDGET
        .saturating_mul(2)
        .saturating_mul(device_count)
}

fn record_query_result(
    result: &mut PollResult,
    device: &DiscoveredDevice,
    query: std::result::Result<(BatteryState, Vec<PollError>), QueryFailure>,
) {
    let (query, omitted) = match query {
        Err(QueryFailure::ProbeCoverage { failure, omitted }) => (Err(*failure), omitted),
        query => (query, 0),
    };
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
        Ok((_, warnings)) => result.errors.extend(ambiguous_probe_warnings(warnings)),
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
        Err(QueryFailure::ProbeCoverage { .. }) => unreachable!(),
    }
    if omitted > 0 {
        result.errors.extend(coverage_warnings(device, omitted));
    }
}

fn query_device(
    device: &DiscoveredDevice,
    pid_cache: &mut PidCache,
    cache_changed: &mut bool,
) -> std::result::Result<(BatteryState, Vec<PollError>), QueryFailure> {
    let known = device_map::known_device_support(device.pid);
    let display_name = display_name(device);
    let candidate_plan = candidate_probe_plan(&device.candidates);
    let mut transports = candidate_plan
        .candidates
        .iter()
        .map(|candidate| {
            (
                candidate.interface_number,
                ProcessTransport::new(candidate.path.to_bytes().to_vec()),
            )
        })
        .collect::<Vec<_>>();

    if transports.is_empty() {
        if pid_cache.get(device.pid).is_some() {
            *cache_changed |= pid_cache.remove(device.pid);
        }
        return Err(no_candidate_transport_failure(Vec::new()));
    }

    let cached = pid_cache.get(device.pid);
    let transaction_ids = battery_transaction_ids(cached, known);
    let response_wait = response_wait(device.pid);
    let battery_deadline = Instant::now()
        + probe_budget(
            transports.len() * transaction_ids.len(),
            response_wait,
            DEVICE_POLL_BUDGET,
        );
    let battery_probe = probe_request_with(
        &transports,
        &transaction_ids,
        build_battery_request,
        response_wait,
        battery_deadline,
    );

    let ProbeSuccess {
        candidate_index,
        transaction_id,
        report: battery_report,
        interface_failures,
    } = match battery_probe {
        Ok(found) => found,
        Err(failure) => {
            if cached.is_some() {
                *cache_changed |= pid_cache.remove(device.pid);
            }
            return Err(with_probe_coverage(failure, candidate_plan.omitted));
        }
    };
    let observed_at = Instant::now();

    let generated = known.map(|support| support.transaction_id);
    *cache_changed |=
        update_cache_after_success(pid_cache, device.pid, cached, generated, transaction_id);
    let battery_raw = battery_report.arguments[1];
    let battery_percent = scale_percent(battery_raw);
    let mut warnings =
        successful_probe_warnings(device, interface_failures, candidate_plan.omitted);
    let (charge_state, charge_warnings) = if known
        .is_some_and(|support| !support.supports_charging_status)
    {
        (ChargeState::Unsupported, Vec::new())
    } else {
        prioritize_probe_candidate(&mut transports, candidate_index);
        let charge_plan = charge_probe_plan(&transports, device.interface_grouping);
        let charge_deadline = Instant::now()
            + probe_budget(charge_plan.candidates.len(), response_wait, Duration::ZERO);
        let charge_probe = probe_request_with(
            charge_plan.candidates,
            &[transaction_id],
            build_charging_request,
            response_wait,
            charge_deadline,
        )
        .map_err(|failure| {
            mark_unsupported_incomplete(failure, candidate_plan.omitted > 0 || charge_plan.omitted)
        });
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

fn ambiguous_probe_warnings(warnings: Vec<PollError>) -> Vec<PollError> {
    warnings
        .into_iter()
        .map(|mut warning| {
            if warning.scope == PollErrorScope::Interface {
                warning.scope = PollErrorScope::ProbeCoverage;
            }
            warning
        })
        .collect()
}

fn successful_probe_warnings(
    device: &DiscoveredDevice,
    interface_failures: Vec<anyhow::Error>,
    omitted: usize,
) -> Vec<PollError> {
    let mut warnings = interface_failures
        .into_iter()
        .map(|error| scoped_poll_error(device, PollErrorScope::Interface, error))
        .collect::<Vec<_>>();
    warnings.extend(coverage_warnings(device, omitted));
    warnings
}

fn coverage_warnings(device: &DiscoveredDevice, omitted: usize) -> Vec<PollError> {
    coverage_failures(omitted)
        .into_iter()
        .map(|error| scoped_poll_error(device, PollErrorScope::ProbeCoverage, error))
        .collect()
}

fn no_candidate_transport_failure(mut failures: Vec<anyhow::Error>) -> QueryFailure {
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
        Ok(success) => {
            let state = if success.report.arguments[1] > 0 {
                ChargeState::Charging
            } else {
                ChargeState::NotCharging
            };
            (
                state,
                successful_probe_warnings(device, success.interface_failures, 0),
            )
        }
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
        Err(QueryFailure::ProbeCoverage { failure, .. }) => {
            charge_query_result(device, Err(*failure))
        }
    }
}

#[cfg(test)]
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
        QueryFailure::ProbeCoverage { failure, omitted } => QueryFailure::ProbeCoverage {
            failure: Box::new(merge_query_failure(*failure, auxiliary)),
            omitted,
        },
    }
}

fn with_probe_coverage(failure: QueryFailure, omitted: usize) -> QueryFailure {
    if omitted == 0 {
        return failure;
    }
    let failure = match failure {
        QueryFailure::Unsupported { auxiliary, .. } => QueryFailure::Unsupported {
            evidence: UnsupportedEvidence::Partial,
            auxiliary,
        },
        failure => failure,
    };
    QueryFailure::ProbeCoverage {
        failure: Box::new(failure),
        omitted,
    }
}

struct CandidateProbePlan<'a, T> {
    candidates: &'a [T],
    omitted: usize,
}

fn candidate_probe_plan<T>(candidates: &[T]) -> CandidateProbePlan<'_, T> {
    let attempted = candidates.len().min(MAX_CANDIDATES_PER_DEVICE);
    let omitted = candidates.len() - attempted;
    CandidateProbePlan {
        candidates: &candidates[..attempted],
        omitted,
    }
}

fn coverage_failures(omitted: usize) -> Vec<anyhow::Error> {
    (omitted > 0)
        .then(|| {
            anyhow::anyhow!("{omitted} candidate interface(s) skipped by the bounded probe limit")
        })
        .into_iter()
        .collect()
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
        QueryFailure::ProbeCoverage { failure, omitted } => QueryFailure::ProbeCoverage {
            failure: Box::new(mark_unsupported_incomplete(*failure, incomplete)),
            omitted,
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
                if now >= deadline {
                    budget_exhausted = true;
                    break 'rounds;
                }

                let request = build_request(transaction_id);
                let remaining_targets = (target_index..target_count)
                    .filter(|index| states[*index].is_active())
                    .count()
                    .max(1);
                let Some(operation_timeout) = operation_timeout(
                    deadline.saturating_duration_since(now),
                    remaining_targets,
                    response_wait,
                ) else {
                    budget_exhausted = true;
                    break 'rounds;
                };
                let attempt =
                    attempt_request_with(transport, &request, response_wait, operation_timeout);
                match attempt {
                    RequestAttempt::Success(report) => {
                        let interface_failures = candidates[..candidate_index]
                            .iter()
                            .enumerate()
                            .filter_map(|(failed_index, (interface_number, _))| {
                                let state_index =
                                    transaction_index * candidates.len() + failed_index;
                                fallback_interface_error(
                                    &states[state_index],
                                    *interface_number,
                                    transaction_id,
                                )
                            })
                            .collect();
                        return Ok(ProbeSuccess {
                            candidate_index,
                            transaction_id,
                            report,
                            interface_failures,
                        });
                    }
                    RequestAttempt::Retry(err) => {
                        states[target_index] = ProbeState::Retry(err);
                    }
                    RequestAttempt::Failed(err) => {
                        states[target_index] = ProbeState::Failed(err);
                    }
                    RequestAttempt::Unsupported => {
                        states[target_index] = ProbeState::Unsupported;
                    }
                }
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

fn probe_budget(
    target_count: usize,
    response_wait: Duration,
    minimum_budget: Duration,
) -> Duration {
    let target_count = u32::try_from(target_count).unwrap_or(u32::MAX);
    let reserved = minimum_operation_timeout(response_wait)
        .checked_mul(target_count)
        .and_then(|duration| duration.checked_add(PROBE_SCHEDULING_ALLOWANCE))
        .unwrap_or(MAX_PROBE_BUDGET);
    minimum_budget.max(reserved).min(MAX_PROBE_BUDGET)
}

fn operation_timeout(
    remaining: Duration,
    remaining_targets: usize,
    response_wait: Duration,
) -> Option<Duration> {
    let minimum = minimum_operation_timeout(response_wait);
    let later_targets = u32::try_from(remaining_targets.saturating_sub(1)).unwrap_or(u32::MAX);
    let reserved_for_later = minimum.checked_mul(later_targets)?;
    let available = remaining.checked_sub(reserved_for_later)?;
    (available >= minimum).then_some(FEATURE_IO_TIMEOUT.min(available))
}

fn minimum_operation_timeout(response_wait: Duration) -> Duration {
    response_wait.saturating_add(FEATURE_IO_ALLOWANCE)
}

fn fallback_interface_error(
    state: &ProbeState,
    interface_number: i32,
    transaction_id: u8,
) -> Option<anyhow::Error> {
    match state {
        ProbeState::Retry(error) | ProbeState::Failed(error) => Some(anyhow::anyhow!(
            "interface {interface_number}, tx 0x{transaction_id:02X}: {}",
            format_error_chain(error)
        )),
        ProbeState::Unsupported => Some(anyhow::anyhow!(
            "interface {interface_number}, tx 0x{transaction_id:02X}: device returned STATUS_NOT_SUPPORTED"
        )),
        ProbeState::Pending => None,
    }
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

struct ProcessTransport {
    path: Vec<u8>,
}

impl ProcessTransport {
    fn new(path: Vec<u8>) -> Self {
        Self { path }
    }
}

impl FeatureTransport for ProcessTransport {
    fn exchange(
        &self,
        request: &[u8],
        response: &mut [u8],
        response_wait: Duration,
        operation_timeout: Duration,
    ) -> Result<usize> {
        worker::exchange_feature(
            &self.path,
            request,
            response,
            response_wait,
            operation_timeout,
        )
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
        FEATURE_IO_ALLOWANCE, FeatureTransport, MAX_RETRIES, QueryFailure, UnsupportedEvidence,
        candidate_probe_plan, charge_probe_plan, charge_query_result, display_name,
        format_error_chain, mark_unsupported_incomplete, maximum_poll_duration,
        merge_query_failure, minimum_operation_timeout, no_candidate_transport_failure,
        operation_timeout, prioritize_probe_candidate, probe_budget, probe_request_with,
        record_query_result, scale_percent, successful_probe_warnings, update_cache_after_success,
        with_probe_coverage,
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

    struct TransactionFallbackTransport {
        attempts: RefCell<Vec<u8>>,
    }

    impl FeatureTransport for TransactionFallbackTransport {
        fn exchange(
            &self,
            request: &[u8],
            response: &mut [u8],
            _response_wait: Duration,
            _operation_timeout: Duration,
        ) -> Result<usize> {
            let transaction_id = request[2];
            self.attempts.borrow_mut().push(transaction_id);
            if transaction_id == 0x1F {
                bail!("isolated HID operation timed out")
            }
            let bytes = feature_response(transaction_id, 180);
            response.copy_from_slice(&bytes);
            Ok(bytes.len())
        }

        fn pause(&self, _duration: Duration) {}
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
    fn review_round_27_long_wait_targets_keep_startup_and_io_allowance() {
        let response_wait = Duration::from_millis(400);
        let minimum = minimum_operation_timeout(response_wait);
        let mut remaining = probe_budget(20, response_wait, Duration::ZERO);

        assert_eq!(minimum, response_wait + FEATURE_IO_ALLOWANCE);
        for remaining_targets in (1..=20).rev() {
            let timeout = operation_timeout(remaining, remaining_targets, response_wait)
                .expect("each first-round target should retain a meaningful attempt");
            assert!(timeout >= minimum);
            remaining = remaining.saturating_sub(timeout);
        }
    }

    #[test]
    fn review_round_28_batch_budget_scales_with_scheduled_devices() {
        assert_eq!(maximum_poll_duration(0), Duration::ZERO);
        assert_eq!(maximum_poll_duration(1), Duration::from_secs(32));
        assert_eq!(maximum_poll_duration(3), Duration::from_secs(96));
    }

    #[test]
    fn review_round_26_successful_fallback_keeps_interface_scope_and_details() {
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };
        let mut warnings = successful_probe_warnings(
            &device,
            vec![anyhow::anyhow!(
                "interface 1 could not be opened: access denied"
            )],
            0,
        );
        let (_, charge_warnings) = charge_query_result(
            &device,
            Err(QueryFailure::Failed {
                errors: vec![anyhow::anyhow!("charging response timed out")],
            }),
        );
        warnings.extend(charge_warnings);

        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].scope, PollErrorScope::Interface);
        assert_eq!(warnings[0].kind, PollErrorKind::AccessDenied);
        assert_eq!(
            warnings[0].message,
            "interface 1 could not be opened: access denied"
        );
        assert_eq!(warnings[1].scope, PollErrorScope::ChargeState);

        let encoded = serde_json::to_value(&warnings).expect("serialize typed diagnostics");
        assert_eq!(encoded[0]["scope"], "interface");
        assert_eq!(encoded[0]["kind"], "access-denied");
        assert_eq!(
            encoded[0]["message"],
            "interface 1 could not be opened: access denied"
        );
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
            Err(no_candidate_transport_failure(vec![anyhow::anyhow!(
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
        let transaction_ids = [0x1F, 0x3F, 0xFF];

        let success = probe_request_with(
            &candidates,
            &transaction_ids,
            build_battery_request,
            Duration::ZERO,
            Instant::now()
                + probe_budget(
                    candidates.len() * transaction_ids.len(),
                    Duration::ZERO,
                    Duration::ZERO,
                ),
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
    fn review_round_26_timeout_advances_to_next_transaction_in_fresh_isolation() {
        let transport = TransactionFallbackTransport {
            attempts: RefCell::new(Vec::new()),
        };
        let candidates = [(0, transport)];

        let success = probe_request_with(
            &candidates,
            &[0x1F, 0x3F, 0xFF],
            build_battery_request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("later transaction should run after isolated timeout");

        assert_eq!(success.transaction_id, 0x3F);
        assert_eq!(success.report.arguments[1], 180);
        assert_eq!(*candidates[0].1.attempts.borrow(), vec![0x1F, 0x3F]);
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
        let transaction_ids = [0x1F, 0x3F, 0xFF];

        let result = probe_request_with(
            &candidates,
            &transaction_ids,
            build_battery_request,
            Duration::ZERO,
            Instant::now()
                + probe_budget(
                    candidates.len() * transaction_ids.len(),
                    Duration::ZERO,
                    Duration::ZERO,
                ),
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
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].scope, PollErrorScope::Interface);
        assert!(warnings[0].message.contains("interface 0"));
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
                vec![crate::model::PollError {
                    device_key: device.key.clone(),
                    display_name: display_name(&device),
                    pid: device.pid,
                    scope: PollErrorScope::Interface,
                    component: None,
                    kind: PollErrorKind::AccessDenied,
                    message: "fallback interface was used during probing".to_string(),
                }],
            )),
        );

        assert!(result.devices.is_empty());
        assert_eq!(result.errors.len(), 2);
        assert_eq!(result.errors[0].device_key, "00BF");
        assert_eq!(result.errors[0].kind, PollErrorKind::AmbiguousIdentity);
        assert_eq!(result.errors[1].scope, PollErrorScope::ProbeCoverage);
        assert_eq!(
            result.errors[0].display_name,
            "Razer Mouse (identity ambiguous)"
        );
        let json = serde_json::to_value(result).expect("serialize ambiguous result");
        assert_eq!(json["devices"], serde_json::json!([]));
        assert_eq!(json["errors"][0]["kind"], "ambiguous-identity");
        assert_eq!(json["errors"][1]["scope"], "probe-coverage");
    }

    #[test]
    fn review_round_30_charge_truncation_keeps_unsupported_partial() {
        let battery_candidates = [0, 1, 2, 3, 4];
        let candidate_plan = candidate_probe_plan(&battery_candidates);
        let charge_plan =
            charge_probe_plan(candidate_plan.candidates, InterfaceGrouping::VerifiedDevice);
        let failure = mark_unsupported_incomplete(
            QueryFailure::Unsupported {
                evidence: UnsupportedEvidence::Conclusive,
                auxiliary: Vec::new(),
            },
            candidate_plan.omitted > 0 || charge_plan.omitted,
        );
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };

        let (state, warnings) = charge_query_result(&device, Err(failure));

        assert_eq!(candidate_plan.omitted, 1);
        assert!(!charge_plan.omitted);
        assert_eq!(state, crate::model::ChargeState::Unavailable);
        assert_eq!(warnings[0].kind, PollErrorKind::PartialUnsupported);
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
        let plan = candidate_probe_plan(&candidates);
        let failure = with_probe_coverage(
            QueryFailure::Unsupported {
                evidence: UnsupportedEvidence::Conclusive,
                auxiliary: Vec::new(),
            },
            plan.omitted,
        );
        let mut result = PollResult::default();

        record_query_result(&mut result, &device, Err(failure));

        assert_eq!(plan.candidates, &[0, 1, 2, 3]);
        assert_eq!(plan.omitted, 1);
        assert_eq!(result.errors[0].kind, PollErrorKind::PartialUnsupported);
        assert!(
            result.errors[0]
                .message
                .contains("one or more battery probes")
        );
        assert!(result.errors[1].message.contains("1 candidate interface"));
        assert_eq!(result.errors[1].scope, PollErrorScope::ProbeCoverage);
    }

    #[test]
    fn review_round_26_candidate_truncation_is_coverage_not_fallback() {
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            interface_grouping: InterfaceGrouping::VerifiedDevice,
            candidates: Vec::new(),
        };

        let warnings = successful_probe_warnings(&device, Vec::new(), 2);

        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].scope, PollErrorScope::ProbeCoverage);
        assert!(warnings[0].message.contains("2 candidate interface"));
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
