use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChargeState {
    Charging,
    NotCharging,
    Unavailable,
    Unsupported,
}

impl ChargeState {
    pub fn is_charging(self) -> bool {
        matches!(self, Self::Charging)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BatteryState {
    pub device_key: String,
    pub display_name: String,
    pub pid: u16,
    pub battery_raw: u8,
    pub battery_percent: u8,
    pub charge_state: ChargeState,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PollErrorKind {
    AccessDenied,
    DeviceUnavailable,
    Unsupported,
    PartialUnsupported,
    Protocol,
    Unknown,
}

impl PollErrorKind {
    pub(crate) fn classify_message(message: &str) -> Self {
        let normalized = message.to_ascii_lowercase();
        if ((normalized.contains("access") || normalized.contains("permission"))
            && normalized.contains("denied"))
            || normalized.contains("not permitted")
        {
            Self::AccessDenied
        } else if normalized.contains("open")
            || normalized.contains("unavailable")
            || normalized.contains("no response")
            || normalized.contains("time budget")
        {
            Self::DeviceUnavailable
        } else if normalized.contains("busy")
            || normalized.contains("crc")
            || normalized.contains("status")
            || normalized.contains("response")
        {
            Self::Protocol
        } else {
            Self::Unknown
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PollErrorScope {
    Device,
    ChargeState,
    Subsystem,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PollError {
    pub device_key: String,
    pub display_name: String,
    pub pid: u16,
    pub scope: PollErrorScope,
    pub kind: PollErrorKind,
    pub message: String,
}

impl PollError {
    pub(crate) fn subsystem(message: impl Into<String>) -> Self {
        let message = message.into();
        let kind = PollErrorKind::classify_message(&message);
        Self {
            device_key: String::new(),
            display_name: "HID subsystem".to_string(),
            pid: 0,
            scope: PollErrorScope::Subsystem,
            kind,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PollError, PollErrorKind};

    #[test]
    fn review_error_classification_handles_permission_denied_consistently() {
        assert_eq!(
            PollError::subsystem("permission denied while initializing HID").kind,
            PollErrorKind::AccessDenied
        );
        assert_eq!(
            PollErrorKind::classify_message("permission denied while opening interface"),
            PollErrorKind::AccessDenied
        );
        assert_eq!(
            PollError::subsystem("HID access is unavailable").kind,
            PollErrorKind::DeviceUnavailable
        );
    }
}

pub type PollOutcome = Result<PollResult, PollError>;

#[derive(Clone, Debug, Default, Serialize)]
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
