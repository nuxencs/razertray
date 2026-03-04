#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatteryState {
    pub device_key: String,
    pub display_name: String,
    pub pid: u16,
    pub battery_percent: u8,
    pub is_charging: bool,
    pub supports_charging_status: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PollResult {
    pub devices: Vec<BatteryState>,
    pub errors: Vec<String>,
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
