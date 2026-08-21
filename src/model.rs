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
    Protocol,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PollError {
    pub device_key: String,
    pub display_name: String,
    pub pid: u16,
    pub kind: PollErrorKind,
    pub message: String,
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
