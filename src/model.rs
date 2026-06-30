use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatteryState {
    pub device_key: String,
    pub display_name: String,
    pub pid: u16,
    /// Floored whole-percent value shown to the user (matches OpenRazer).
    pub battery_percent: u8,
    /// Raw 0..=255 reading kept for rate math: ~0.4% per step, far less noisy
    /// than the floored percentage when estimating discharge rate.
    pub battery_raw: u8,
    pub is_charging: bool,
    pub supports_charging_status: bool,
}

/// A device that was discovered but could not be read this poll. Carries enough
/// structure to throttle repeated logging and to classify benign wireless-sleep
/// failures separately from genuine faults.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PollError {
    /// Dedupe key for the device; empty for scan-level errors (e.g. no hidapi).
    pub device_key: String,
    /// Resolved device name (device-map name when known), so error and success
    /// lines refer to the device the same way.
    pub display_name: String,
    pub pid: Option<u16>,
    pub message: String,
}

impl PollError {
    /// True for benign, transient transport failures that a wireless device in
    /// sleep produces every poll. These are expected and should not be logged at
    /// WARN on every cycle. Kept in sync with the failure messages produced in
    /// `hid::client` (covered by tests there and here).
    pub fn is_unreachable(&self) -> bool {
        const SLEEP_MARKERS: &[&str] = &[
            "send_feature_report failed",
            "get_feature_report failed",
            "device stayed busy/unresponsive after retries",
            "request exhausted retries",
        ];
        SLEEP_MARKERS
            .iter()
            .any(|marker| self.message.starts_with(marker))
            // partial-read hiccups: "expected N bytes, got M"
            || self.message.starts_with("expected ")
    }
}

impl fmt::Display for PollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.pid {
            Some(pid) => write!(f, "{} ({:04X}): {}", self.display_name, pid, self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PollResult {
    pub devices: Vec<BatteryState>,
    pub errors: Vec<PollError>,
}

impl PollResult {
    pub fn sort_devices(&mut self) {
        self.devices.sort_by(|a, b| {
            a.display_name
                .cmp(&b.display_name)
                .then_with(|| a.pid.cmp(&b.pid))
                .then_with(|| a.device_key.cmp(&b.device_key))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::PollError;

    fn err(message: &str) -> PollError {
        PollError {
            device_key: "00BF".to_string(),
            display_name: "Test Mouse".to_string(),
            pid: Some(0x00BF),
            message: message.to_string(),
        }
    }

    #[test]
    fn sleep_failures_classified_unreachable() {
        assert!(err("send_feature_report failed").is_unreachable());
        assert!(err("get_feature_report failed").is_unreachable());
        assert!(err("device stayed busy/unresponsive after retries").is_unreachable());
        assert!(err("request exhausted retries").is_unreachable());
        assert!(err("expected 91 bytes, got 0").is_unreachable());
    }

    #[test]
    fn real_faults_not_unreachable() {
        assert!(!err("device returned STATUS_FAILURE").is_unreachable());
        assert!(!err("device returned STATUS_NOT_SUPPORTED").is_unreachable());
        assert!(!err("invalid response crc").is_unreachable());
        assert!(!err("response did not match request").is_unreachable());
        assert!(!err("unable to determine transaction id for 00BF").is_unreachable());
    }

    #[test]
    fn display_with_and_without_pid() {
        assert_eq!(
            err("send_feature_report failed").to_string(),
            "Test Mouse (00BF): send_feature_report failed"
        );
        let scan_err = PollError {
            device_key: String::new(),
            display_name: String::new(),
            pid: None,
            message: "hidapi unavailable".to_string(),
        };
        assert_eq!(scan_err.to_string(), "hidapi unavailable");
    }
}
