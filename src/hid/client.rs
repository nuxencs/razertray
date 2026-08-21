use crate::config::PidCache;
use crate::device_map;
use crate::hid::protocol::{
    FEATURE_REPORT_LENGTH, RazerReport, STATUS_BUSY, STATUS_FAILURE, STATUS_NO_RESPONSE,
    STATUS_NOT_SUPPORTED, STATUS_SUCCESSFUL, build_battery_request, build_charging_request,
    expected_response_matches, feature_report_payload,
};
use crate::hid::scanner::{DiscoveredDevice, scan_devices};
use crate::model::{
    BatteryState, ChargeState, PollError, PollErrorKind, PollErrorScope, PollResult,
};
use anyhow::{Context, Result, bail};
use hidapi::{HidApi, HidDevice};
use std::thread;
use std::time::{Duration, Instant};

const MAX_CANDIDATES_PER_DEVICE: usize = 4;
const MAX_RETRIES: usize = 5;
const DEVICE_POLL_BUDGET: Duration = Duration::from_secs(8);
const SEND_DELAY: Duration = Duration::from_millis(60);
const RETRY_DELAY: Duration = Duration::from_millis(400);

pub struct PollBatch {
    pub result: PollResult,
    pub cache_changed: bool,
}

#[derive(Debug)]
struct UnsupportedCommand;

impl std::fmt::Display for UnsupportedCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("device returned STATUS_NOT_SUPPORTED")
    }
}

impl std::error::Error for UnsupportedCommand {}

enum QueryFailure {
    Unsupported,
    Failed(anyhow::Error),
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
    query: std::result::Result<(BatteryState, Option<PollError>), QueryFailure>,
) {
    match query {
        Ok((state, warning)) => {
            result.devices.push(state);
            if let Some(warning) = warning {
                result.errors.push(warning);
            }
        }
        Err(QueryFailure::Unsupported) => result.errors.push(PollError {
            device_key: device.key.clone(),
            display_name: display_name(device),
            pid: device.pid,
            scope: PollErrorScope::Device,
            kind: PollErrorKind::Unsupported,
            message: "battery status is not supported by this device".to_string(),
        }),
        Err(QueryFailure::Failed(err)) => result.errors.push(poll_error(&device, err)),
    }
}

fn query_device(
    api: &HidApi,
    device: &DiscoveredDevice,
    pid_cache: &mut PidCache,
    cache_changed: &mut bool,
) -> std::result::Result<(BatteryState, Option<PollError>), QueryFailure> {
    let known = device_map::known_device_support(device.pid);
    let display_name = display_name(device);
    let mut failures = Vec::new();
    let mut unsupported_interfaces = 0_usize;
    let mut opened_interfaces = 0_usize;
    let deadline = Instant::now() + DEVICE_POLL_BUDGET;

    for candidate in device.candidates.iter().take(MAX_CANDIDATES_PER_DEVICE) {
        if Instant::now() >= deadline {
            failures.push("poll time budget exhausted".to_string());
            break;
        }
        let handle = match api.open_path(candidate.path.as_c_str()) {
            Ok(handle) => handle,
            Err(err) => {
                failures.push(format!(
                    "interface {} could not be opened: {err}",
                    candidate.interface_number
                ));
                continue;
            }
        };
        opened_interfaces += 1;

        match query_handle(
            &handle,
            device.pid,
            known,
            pid_cache,
            cache_changed,
            deadline,
        ) {
            Ok((transaction_id, battery_report)) => {
                let battery_raw = battery_report.arguments[1];
                let battery_percent = scale_percent(battery_raw);
                let (charge_state, warning) =
                    if known.is_some_and(|support| !support.supports_charging_status) {
                        (ChargeState::Unsupported, None)
                    } else {
                        match send_request(
                            &handle,
                            build_charging_request(transaction_id),
                            device.pid,
                            deadline,
                        ) {
                            Ok(report) if report.arguments[1] > 0 => (ChargeState::Charging, None),
                            Ok(_) => (ChargeState::NotCharging, None),
                            Err(err) if err.downcast_ref::<UnsupportedCommand>().is_some() => {
                                (ChargeState::Unsupported, None)
                            }
                            Err(err) => (
                                ChargeState::Unavailable,
                                Some(PollError {
                                    device_key: device.key.clone(),
                                    display_name: display_name.clone(),
                                    pid: device.pid,
                                    scope: PollErrorScope::ChargeState,
                                    kind: classify_error(&err),
                                    message: format!(
                                        "charging status unavailable: {}",
                                        format_error_chain(&err)
                                    ),
                                }),
                            ),
                        }
                    };

                return Ok((
                    BatteryState {
                        device_key: device.key.clone(),
                        display_name,
                        pid: device.pid,
                        battery_raw,
                        battery_percent,
                        charge_state,
                    },
                    warning,
                ));
            }
            Err(err) if err.downcast_ref::<UnsupportedCommand>().is_some() => {
                unsupported_interfaces += 1;
            }
            Err(err) => failures.push(format!(
                "interface {} failed: {}",
                candidate.interface_number,
                format_error_chain(&err)
            )),
        }
    }

    if failures.is_empty() && opened_interfaces > 0 && unsupported_interfaces == opened_interfaces {
        Err(QueryFailure::Unsupported)
    } else {
        Err(QueryFailure::Failed(anyhow::anyhow!(failures.join("; "))))
    }
}

fn query_handle(
    handle: &HidDevice,
    pid: u16,
    known: Option<&device_map::DeviceSupport>,
    pid_cache: &mut PidCache,
    cache_changed: &mut bool,
    deadline: Instant,
) -> Result<(u8, RazerReport)> {
    let cached = pid_cache.get(pid);
    let mut transaction_ids = Vec::with_capacity(3);
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

    let mut failures = Vec::new();
    let mut unsupported = 0_usize;
    let mut attempted = 0_usize;
    for transaction_id in transaction_ids {
        if Instant::now() >= deadline {
            failures.push("poll time budget exhausted".to_string());
            break;
        }
        attempted += 1;
        match send_request(handle, build_battery_request(transaction_id), pid, deadline) {
            Ok(report) => {
                let generated = known.map(|support| support.transaction_id);
                *cache_changed |=
                    update_cache_after_success(pid_cache, pid, cached, generated, transaction_id);
                return Ok((transaction_id, report));
            }
            Err(err) if err.downcast_ref::<UnsupportedCommand>().is_some() => {
                unsupported += 1;
            }
            Err(err) => failures.push(format!(
                "tx 0x{transaction_id:02X}: {}",
                format_error_chain(&err)
            )),
        }
    }

    if attempted > 0 && unsupported == attempted && failures.is_empty() {
        if cached.is_some() {
            *cache_changed |= pid_cache.remove(pid);
        }
        return Err(UnsupportedCommand.into());
    }

    if cached.is_some() {
        *cache_changed |= pid_cache.remove(pid);
    }
    bail!("unable to read battery ({})", failures.join(", "))
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
    ) -> Result<usize>;

    fn pause(&self, duration: Duration);
}

struct HidTransport<'a>(&'a HidDevice);

impl FeatureTransport for HidTransport<'_> {
    fn exchange(
        &self,
        request: &[u8],
        response: &mut [u8],
        response_wait: Duration,
    ) -> Result<usize> {
        self.0
            .send_feature_report(request)
            .context("send_feature_report failed")?;
        thread::sleep(response_wait);
        self.0
            .get_feature_report(response)
            .context("get_feature_report failed")
    }

    fn pause(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

fn send_request(
    handle: &HidDevice,
    request: RazerReport,
    pid: u16,
    deadline: Instant,
) -> Result<RazerReport> {
    send_request_with(&HidTransport(handle), request, response_wait(pid), deadline)
}

fn send_request_with<T: FeatureTransport>(
    transport: &T,
    request: RazerReport,
    response_wait: Duration,
    deadline: Instant,
) -> Result<RazerReport> {
    let request_payload = feature_report_payload(&request);
    let mut last_error = None;

    for attempt in 0..MAX_RETRIES {
        if Instant::now() >= deadline {
            last_error = Some(anyhow::anyhow!("poll time budget exhausted"));
            break;
        }

        let mut response_buffer = [0u8; FEATURE_REPORT_LENGTH];
        response_buffer[0] = 0x00;
        match transport.exchange(&request_payload, &mut response_buffer, response_wait) {
            Err(err) => last_error = Some(err),
            Ok(count) if count != FEATURE_REPORT_LENGTH => {
                last_error = Some(anyhow::anyhow!(
                    "expected {} bytes, got {count}",
                    FEATURE_REPORT_LENGTH
                ));
            }
            Ok(_) => match RazerReport::from_bytes(&response_buffer[1..]) {
                Err(err) => last_error = Some(err.context("invalid response report")),
                Ok(response) => {
                    if !response.is_valid_crc() {
                        last_error = Some(anyhow::anyhow!("invalid response crc"));
                    } else if !expected_response_matches(&request, &response) {
                        last_error = Some(anyhow::anyhow!("response did not match request"));
                    } else {
                        match response.status {
                            // OpenRazer treats BUSY as success because some
                            // devices return usable data with that status.
                            STATUS_SUCCESSFUL | STATUS_BUSY => return Ok(response),
                            STATUS_NO_RESPONSE => {
                                last_error = Some(anyhow::anyhow!("device returned no response"));
                            }
                            STATUS_FAILURE => bail!("device returned STATUS_FAILURE"),
                            STATUS_NOT_SUPPORTED => return Err(UnsupportedCommand.into()),
                            other => bail!("unexpected status: 0x{other:02X}"),
                        }
                    }
                }
            },
        }

        if attempt + 1 < MAX_RETRIES && Instant::now() < deadline {
            transport.pause(RETRY_DELAY.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("request exhausted retries")))
}

fn response_wait(pid: u16) -> Duration {
    match pid {
        // Atheris and Orochi receivers use the long wait from OpenRazer.
        0x0062 | 0x0088 | 0x0089 => Duration::from_millis(400),
        _ => SEND_DELAY,
    }
}

fn display_name(device: &DiscoveredDevice) -> String {
    device_map::known_device_support(device.pid).map_or_else(
        || device.product_name.clone(),
        |support| support.name.to_string(),
    )
}

fn poll_error(device: &DiscoveredDevice, err: anyhow::Error) -> PollError {
    PollError {
        device_key: device.key.clone(),
        display_name: display_name(device),
        pid: device.pid,
        scope: PollErrorScope::Device,
        kind: classify_error(&err),
        message: format_error_chain(&err),
    }
}

fn classify_error(err: &anyhow::Error) -> PollErrorKind {
    let message = format_error_chain(err).to_ascii_lowercase();
    if message.contains("access") || message.contains("permission") {
        PollErrorKind::AccessDenied
    } else if message.contains("open")
        || message.contains("unavailable")
        || message.contains("no response")
        || message.contains("time budget")
    {
        PollErrorKind::DeviceUnavailable
    } else if message.contains("busy")
        || message.contains("crc")
        || message.contains("status")
        || message.contains("response")
    {
        PollErrorKind::Protocol
    } else {
        PollErrorKind::Unknown
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
        FeatureTransport, MAX_RETRIES, QueryFailure, classify_error, format_error_chain,
        record_query_result, scale_percent, send_request_with, update_cache_after_success,
    };
    use crate::config::PidCache;
    use crate::hid::protocol::{
        FEATURE_REPORT_LENGTH, STATUS_BUSY, STATUS_NOT_SUPPORTED, STATUS_SUCCESSFUL,
        build_battery_request,
    };
    use crate::hid::scanner::DiscoveredDevice;
    use crate::model::{PollErrorKind, PollErrorScope, PollResult};
    use anyhow::{Result, bail};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
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
        let request = build_battery_request(0x1F);
        let mut invalid = feature_response(0x1F, 127);
        invalid[89] ^= 0x01;
        let transport = FakeTransport::new(vec![Ok(invalid), Ok(feature_response(0x1F, 128))]);

        let response = send_request_with(
            &transport,
            request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("second response should succeed");

        assert_eq!(response.arguments[1], 128);
        assert_eq!(transport.attempts.get(), 2);
    }

    #[test]
    fn transport_retries_read_failure() {
        let request = build_battery_request(0x3F);
        let transport = FakeTransport::new(vec![
            Err(anyhow::anyhow!("receiver asleep")),
            Ok(feature_response(0x3F, 200)),
        ]);

        let response = send_request_with(
            &transport,
            request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("retry should recover");

        assert_eq!(response.arguments[1], 200);
        assert_eq!(transport.attempts.get(), 2);
    }

    #[test]
    fn transport_accepts_busy_response_with_usable_data() {
        let request = build_battery_request(0x1F);
        let transport = FakeTransport::new(vec![Ok(feature_response_with_status(
            0x1F,
            180,
            STATUS_BUSY,
        ))]);

        let response = send_request_with(
            &transport,
            request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect("busy response can contain usable data");

        assert_eq!(response.arguments[1], 180);
        assert_eq!(transport.attempts.get(), 1);
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
        let request = build_battery_request(0x1F);
        let transport = FakeTransport::new(vec![Ok(feature_response_with_status(
            0x1F,
            0,
            STATUS_NOT_SUPPORTED,
        ))]);

        let error = send_request_with(
            &transport,
            request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("unsupported command should remain typed");

        assert!(error.downcast_ref::<super::UnsupportedCommand>().is_some());
    }

    #[test]
    fn unsupported_query_is_preserved_in_poll_result() {
        let device = DiscoveredDevice {
            key: "mouse".to_string(),
            pid: 0xFFFF,
            product_name: "Razer Mouse".to_string(),
            candidates: Vec::new(),
        };
        let mut result = PollResult::default();

        record_query_result(&mut result, &device, Err(QueryFailure::Unsupported));

        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].scope, PollErrorScope::Device);
        assert_eq!(result.errors[0].kind, PollErrorKind::Unsupported);
        assert_eq!(result.errors[0].device_key, "mouse");
        assert_eq!(result.errors[0].pid, 0xFFFF);
        let json = serde_json::to_value(result).expect("serialize poll result");
        assert_eq!(json["errors"][0]["kind"], "unsupported");
        assert_eq!(json["errors"][0]["scope"], "device");
    }

    #[test]
    fn diagnostic_classification_preserves_transport_source() {
        let request = build_battery_request(0x1F);
        let replies = (0..MAX_RETRIES)
            .map(|_| {
                Err(anyhow::anyhow!("access denied by operating system")
                    .context("send_feature_report failed"))
            })
            .collect();
        let transport = FakeTransport::new(replies);

        let transport_error = send_request_with(
            &transport,
            request,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("transport retries should fail");
        let aggregate = anyhow::anyhow!(
            "unable to read battery (tx 0x1F: {})",
            format_error_chain(&transport_error)
        );

        assert_eq!(classify_error(&aggregate), PollErrorKind::AccessDenied);
        assert!(format_error_chain(&aggregate).contains("access denied by operating system"));
    }
}
