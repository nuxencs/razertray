use crate::model::BatteryState;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct Notifier {
    threshold: u8,
    cooldown: Duration,
    last_sent: HashMap<String, Instant>,
}

impl Notifier {
    pub fn new(threshold: u8, cooldown_minutes: u64) -> Self {
        Self {
            threshold,
            cooldown: Duration::from_secs(cooldown_minutes.saturating_mul(60)),
            last_sent: HashMap::new(),
        }
    }

    fn should_notify(&self, state: &BatteryState, now: Instant) -> bool {
        if state.is_charging || state.battery_percent > self.threshold {
            return false;
        }

        if let Some(last) = self.last_sent.get(&state.device_key)
            && now.duration_since(*last) < self.cooldown
        {
            return false;
        }

        true
    }

    pub fn maybe_notify_low_battery(&mut self, state: &BatteryState) -> bool {
        let now = Instant::now();
        if !self.should_notify(state, now) {
            return false;
        }

        if send_toast_low_battery(state).is_ok() {
            self.last_sent.insert(state.device_key.clone(), now);
            return true;
        }

        false
    }
}

#[cfg(target_os = "windows")]
fn send_toast_low_battery(state: &BatteryState) -> anyhow::Result<()> {
    use tauri_winrt_notification::Toast;

    Toast::new(Toast::POWERSHELL_APP_ID)
        .title("Razer Device Battery Low")
        .text1(&state.display_name)
        .text2(&format!("Battery at {}%", state.battery_percent))
        .show()?;

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn send_toast_low_battery(_state: &BatteryState) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Notifier;
    use crate::model::BatteryState;
    use std::time::{Duration, Instant};

    #[test]
    fn low_battery_ignored_while_charging() {
        let mut notifier = Notifier::new(15, 120);
        let state = BatteryState {
            device_key: "test".to_string(),
            display_name: "Test Mouse".to_string(),
            pid: 0x0072,
            battery_percent: 10,
            is_charging: true,
            supports_charging_status: true,
        };

        assert!(!notifier.maybe_notify_low_battery(&state));
    }

    #[test]
    fn cooldown_suppresses_repeat_notifications() {
        let mut notifier = Notifier::new(15, 120);
        let state = BatteryState {
            device_key: "test".to_string(),
            display_name: "Test Mouse".to_string(),
            pid: 0x0072,
            battery_percent: 10,
            is_charging: false,
            supports_charging_status: true,
        };

        let now = Instant::now();
        assert!(notifier.should_notify(&state, now));
        notifier.last_sent.insert(state.device_key.clone(), now);
        assert!(!notifier.should_notify(&state, now + Duration::from_secs(30)));
        assert!(notifier.should_notify(&state, now + notifier.cooldown + Duration::from_secs(1)));
    }
}
