use serde::Serialize;
use std::fmt;
use std::time::Instant;

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
    #[serde(skip)]
    pub(crate) observed_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PollErrorKind {
    AccessDenied,
    AmbiguousIdentity,
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
            || normalized.contains("not connected")
            || normalized.contains("disconnected")
            || normalized.contains("device_not_connected")
        {
            Self::DeviceUnavailable
        } else if normalized.contains("busy")
            || normalized.contains("crc")
            || normalized.contains("status")
            || normalized.contains("response")
            || (normalized.contains("expected ") && normalized.contains(" bytes, got "))
        {
            Self::Protocol
        } else {
            Self::Unknown
        }
    }
}

impl fmt::Display for PollErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AccessDenied => "access-denied",
            Self::AmbiguousIdentity => "ambiguous-identity",
            Self::DeviceUnavailable => "device-unavailable",
            Self::Unsupported => "unsupported",
            Self::PartialUnsupported => "partial-unsupported",
            Self::Protocol => "protocol",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PollErrorScope {
    Device,
    Interface,
    ProbeCoverage,
    ChargeState,
    Subsystem,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubsystemComponent {
    Hid,
    PidCache,
}

impl SubsystemComponent {
    fn display_name(self) -> &'static str {
        match self {
            Self::Hid => "HID subsystem",
            Self::PidCache => "PID cache",
        }
    }
}

impl fmt::Display for PollErrorScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Device => "device",
            Self::Interface => "interface",
            Self::ProbeCoverage => "probe-coverage",
            Self::ChargeState => "charge-state",
            Self::Subsystem => "subsystem",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PollError {
    pub device_key: String,
    pub display_name: String,
    pub pid: u16,
    pub scope: PollErrorScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<SubsystemComponent>,
    pub kind: PollErrorKind,
    pub message: String,
}

impl PollError {
    pub(crate) fn subsystem(message: impl Into<String>) -> Self {
        Self::subsystem_component(SubsystemComponent::Hid, message)
    }

    pub(crate) fn subsystem_component(
        component: SubsystemComponent,
        message: impl Into<String>,
    ) -> Self {
        let message = message.into();
        let kind = PollErrorKind::classify_message(&message);
        Self {
            device_key: String::new(),
            display_name: component.display_name().to_string(),
            pid: 0,
            scope: PollErrorScope::Subsystem,
            component: Some(component),
            kind,
            message,
        }
    }

    pub(crate) fn pid_cache(message: impl Into<String>) -> Self {
        Self::subsystem_component(SubsystemComponent::PidCache, message)
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
        assert_eq!(
            PollErrorKind::classify_message("The device is not connected"),
            PollErrorKind::DeviceUnavailable
        );
        assert_eq!(
            PollErrorKind::classify_message("ERROR_DEVICE_NOT_CONNECTED"),
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
