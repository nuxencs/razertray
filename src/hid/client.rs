use crate::config::PidCache;
use crate::device_map;
use crate::hid::protocol::{
    FEATURE_REPORT_LENGTH, RazerReport, STATUS_BUSY, STATUS_FAILURE, STATUS_NO_RESPONSE,
    STATUS_NOT_SUPPORTED, STATUS_SUCCESSFUL, build_battery_request, build_charging_request,
    expected_response_matches, feature_report_payload,
};
use crate::hid::scanner::{DiscoveredDevice, scan_devices};
use crate::model::{BatteryState, PollResult};
use anyhow::{Context, Result, bail};
use hidapi::{HidApi, HidDevice};
use std::thread;
use std::time::Duration;

const MAX_RETRIES: usize = 6;
const SEND_DELAY: Duration = Duration::from_millis(60);
const RETRY_DELAY: Duration = Duration::from_millis(400);

pub fn poll_devices(api: &HidApi, pid_cache: &mut PidCache) -> PollResult {
    let discovered = scan_devices(api);
    let mut result = PollResult::default();

    for device in discovered {
        match query_device(api, &device, pid_cache) {
            Ok(state) => result.devices.push(state),
            Err(err) => result.errors.push(format!(
                "{} ({:04X}): {err}",
                device.product_name, device.pid
            )),
        }
    }

    result.sort_devices();
    result
}

fn query_device(
    api: &HidApi,
    device: &DiscoveredDevice,
    pid_cache: &mut PidCache,
) -> Result<BatteryState> {
    let known = device_map::known_device_support(device.pid);
    let mut handle = api
        .open_path(device.path.as_c_str())
        .with_context(|| format!("failed opening path for {:04X}", device.pid))?;

    let transaction_id = match known {
        Some(support) => support.transaction_id,
        None => match pid_cache.get(device.pid) {
            Some(cached) => cached,
            None => {
                let probed = probe_transaction_id(&mut handle, device.pid)?;
                pid_cache.set(device.pid, probed);
                probed
            }
        },
    };

    let battery_report = send_request(&mut handle, build_battery_request(transaction_id))?;
    let battery_percent = scale_percent(battery_report.arguments[1]);

    let supports_charging_status = match known {
        Some(support) => support.supports_charging_status,
        None => true,
    };

    let is_charging = if supports_charging_status {
        match send_request(&mut handle, build_charging_request(transaction_id)) {
            Ok(report) => report.arguments[1] > 0,
            Err(_) => false,
        }
    } else {
        false
    };

    let display_name = known
        .map(|support| support.name.to_string())
        .unwrap_or_else(|| device.product_name.clone());

    Ok(BatteryState {
        device_key: device.key.clone(),
        display_name,
        pid: device.pid,
        battery_percent,
        is_charging,
        supports_charging_status,
    })
}

fn probe_transaction_id(handle: &mut HidDevice, pid: u16) -> Result<u8> {
    // OpenRazer mouse drivers use this transaction-id probe order for battery requests.
    for tx in [0x1F_u8, 0x3F_u8, 0xFF_u8] {
        if send_request(handle, build_battery_request(tx)).is_ok() {
            return Ok(tx);
        }
    }

    bail!("unable to determine transaction id for {:04X}", pid)
}

fn send_request(handle: &mut HidDevice, request: RazerReport) -> Result<RazerReport> {
    let request_payload = feature_report_payload(&request);

    for attempt in 0..MAX_RETRIES {
        handle
            .send_feature_report(&request_payload)
            .context("send_feature_report failed")?;

        thread::sleep(SEND_DELAY);

        let mut response_buffer = [0u8; FEATURE_REPORT_LENGTH];
        response_buffer[0] = 0x00;

        let count = handle
            .get_feature_report(&mut response_buffer)
            .context("get_feature_report failed")?;

        if count != FEATURE_REPORT_LENGTH {
            bail!("expected {} bytes, got {count}", FEATURE_REPORT_LENGTH);
        }

        let response = RazerReport::from_bytes(&response_buffer[1..])?;

        if !response.is_valid_crc() {
            bail!("invalid response crc");
        }

        if !expected_response_matches(&request, &response) {
            bail!("response did not match request");
        }

        match response.status {
            STATUS_SUCCESSFUL => return Ok(response),
            STATUS_BUSY | STATUS_NO_RESPONSE if attempt + 1 < MAX_RETRIES => {
                thread::sleep(RETRY_DELAY);
            }
            STATUS_FAILURE => bail!("device returned STATUS_FAILURE"),
            STATUS_NOT_SUPPORTED => bail!("device returned STATUS_NOT_SUPPORTED"),
            STATUS_BUSY | STATUS_NO_RESPONSE => {
                bail!("device stayed busy/unresponsive after retries")
            }
            other => bail!("unexpected status: 0x{other:02X}"),
        }
    }

    bail!("request exhausted retries")
}

fn scale_percent(raw: u8) -> u8 {
    // Match OpenRazer's user-facing conversion, which truncates: the daemon
    // computes (raw / 255) * 100 as a float and pylib applies int() to it
    // (floor), so a raw of 254 reads as 99%, not 100%.
    (raw as u16 * 100 / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::scale_percent;

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
}
