//! Reads battery level and charging state from every connected Razer device.

use crate::config::PidCache;
use crate::device_map;
use crate::hid::protocol::{Command, FEATURE_REPORT_LENGTH, REPORT_LENGTH, RazerReport, Status};
use crate::hid::scanner::{DiscoveredDevice, scan_devices};
use crate::model::{BatteryState, Percent, PollFailure, PollResult};
use anyhow::{Context, Result, bail};
use hidapi::{HidApi, HidDevice};
use std::thread;
use std::time::Duration;

/// Attempts per request while the device answers Busy or Timeout.
const MAX_ATTEMPTS: usize = 6;
/// Wait between sending a request and reading the response. About twice
/// OpenRazer's 31 ms for newer wireless receivers
/// (`RAZER_NEW_MOUSE_RECEIVER_WAIT_US`). Shorter waits get more Busy answers.
const RESPONSE_DELAY: Duration = Duration::from_millis(60);
/// Extra wait after a Busy or Timeout answer, before the next attempt. The
/// worst case per request is `MAX_ATTEMPTS * (RESPONSE_DELAY + RETRY_DELAY)`.
const RETRY_DELAY: Duration = Duration::from_millis(400);
/// Transaction IDs to try for devices that are not in the device map, in the
/// order OpenRazer's mouse driver uses them.
const PROBE_TRANSACTION_IDS: [u8; 3] = [0x1F, 0x3F, 0xFF];

/// Polls every connected Razer device. Probed transaction IDs go into `pid_cache`.
pub(crate) fn poll_devices(api: &HidApi, pid_cache: &mut PidCache) -> PollResult {
    let mut result = PollResult::default();

    for device in scan_devices(api) {
        match query_device(api, &device, pid_cache) {
            Ok(state) => result.devices.push(state),
            Err(error) => result.failures.push(PollFailure {
                name: device.product_name,
                pid: device.pid,
                error,
            }),
        }
    }

    result
        .devices
        .sort_by(|a, b| (&a.name, a.pid, &a.key).cmp(&(&b.name, b.pid, &b.key)));
    result
}

fn query_device(
    api: &HidApi,
    device: &DiscoveredDevice,
    pid_cache: &mut PidCache,
) -> Result<BatteryState> {
    let known = device_map::known_device_support(device.pid);
    let handle = api
        .open_path(device.path.as_c_str())
        .with_context(|| format!("failed opening path for {:04X}", device.pid))?;

    let transaction_id = match known {
        Some(support) => support.transaction_id,
        None => {
            if let Some(cached) = pid_cache.get(device.pid) {
                cached
            } else {
                let probed = probe_transaction_id(&handle, device.pid)?;
                pid_cache.insert(device.pid, probed);
                probed
            }
        }
    };

    let battery = send_request(
        &handle,
        RazerReport::request(Command::BATTERY_LEVEL, transaction_id),
    )?;

    // Unknown devices may still report charging; a failed request reads as "not charging".
    let supports_charging_status = known.is_none_or(|support| support.supports_charging_status);
    let charging = supports_charging_status
        && match send_request(
            &handle,
            RazerReport::request(Command::CHARGING_STATUS, transaction_id),
        ) {
            Ok(report) => report.value() > 0,
            Err(err) => {
                tracing::debug!(
                    "charging status request failed for {:04X}: {err:#}",
                    device.pid
                );
                false
            }
        };

    Ok(BatteryState {
        key: device.key.clone(),
        name: known.map_or_else(
            || device.product_name.clone(),
            |support| support.name.to_owned(),
        ),
        pid: device.pid,
        percent: Percent::from_raw(battery.value()),
        charging,
    })
}

fn probe_transaction_id(handle: &HidDevice, pid: u16) -> Result<u8> {
    PROBE_TRANSACTION_IDS
        .into_iter()
        .find(|&tx| send_request(handle, RazerReport::request(Command::BATTERY_LEVEL, tx)).is_ok())
        .with_context(|| format!("unable to determine transaction id for {pid:04X}"))
}

fn send_request(handle: &HidDevice, request: RazerReport) -> Result<RazerReport> {
    let payload = request.to_feature_report();

    let mut attempt = 1;
    loop {
        handle
            .send_feature_report(&payload)
            .context("send_feature_report failed")?;

        thread::sleep(RESPONSE_DELAY);

        // Byte 0 is the report ID (0) that hidapi expects on input.
        let mut buffer = [0u8; FEATURE_REPORT_LENGTH];
        let count = handle
            .get_feature_report(&mut buffer)
            .context("get_feature_report failed")?;
        if count != FEATURE_REPORT_LENGTH {
            bail!("expected {FEATURE_REPORT_LENGTH} bytes, got {count}");
        }

        let body: &[u8; REPORT_LENGTH] = buffer[1..]
            .try_into()
            .expect("FEATURE_REPORT_LENGTH is REPORT_LENGTH + 1");
        let response = RazerReport::from_bytes(body);
        if !response.has_valid_crc() {
            bail!("invalid response crc");
        }
        if !response.answers(&request) {
            bail!("response did not match request");
        }

        match response.status()? {
            Status::Successful => return Ok(response),
            Status::Busy | Status::Timeout if attempt < MAX_ATTEMPTS => {
                thread::sleep(RETRY_DELAY);
                attempt += 1;
            }
            status @ (Status::Busy | Status::Timeout) => {
                bail!("device still answered {status:?} after {MAX_ATTEMPTS} attempts")
            }
            status => bail!("device answered {status:?}"),
        }
    }
}
