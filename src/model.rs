#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BatteryState {
    pub(crate) device_key: String,
    pub(crate) display_name: String,
    pub(crate) pid: u16,
    pub(crate) battery_percent: u8,
    pub(crate) is_charging: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PollResult {
    pub(crate) devices: Vec<BatteryState>,
    pub(crate) errors: Vec<String>,
}

impl PollResult {
    pub(crate) fn sort_devices(&mut self) {
        self.devices.sort_by(|a, b| {
            a.display_name
                .cmp(&b.display_name)
                .then_with(|| a.pid.cmp(&b.pid))
                .then_with(|| a.device_key.cmp(&b.device_key))
        });
    }
}
