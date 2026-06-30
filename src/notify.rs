use crate::forecast::Forecast;
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

    pub fn maybe_notify_low_battery(&mut self, state: &BatteryState, forecast: &Forecast) -> bool {
        let now = Instant::now();
        if !self.should_notify(state, now) {
            return false;
        }

        if send_toast_low_battery(state, forecast).is_ok() {
            self.last_sent.insert(state.device_key.clone(), now);
            return true;
        }

        false
    }
}

/// Toast body text: lead with the runout estimate when we have one.
#[cfg(any(target_os = "windows", test))]
fn low_battery_body(forecast: &Forecast) -> String {
    match forecast {
        Forecast::Discharging { .. } => format!("{} — plug in soon", forecast.short()),
        _ => "Plug in charger soon".to_string(),
    }
}

#[cfg(target_os = "windows")]
fn send_toast_low_battery(state: &BatteryState, forecast: &Forecast) -> anyhow::Result<()> {
    use tauri_winrt_notification::Toast;
    let app_id = toast_app_id();

    Toast::new(app_id)
        .title(&low_battery_title(state))
        .text1("Battery low")
        .text2(&low_battery_body(forecast))
        .show()?;

    Ok(())
}

#[cfg(target_os = "windows")]
fn toast_app_id() -> &'static str {
    use crate::APP_ID;
    use std::sync::OnceLock;
    use tauri_winrt_notification::Toast;

    static AUMID_REGISTERED: OnceLock<bool> = OnceLock::new();

    if *AUMID_REGISTERED.get_or_init(|| match register_toast_aumid() {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(
                "failed registering toast AUMID, falling back to PowerShell sender: {err}"
            );
            false
        }
    }) {
        APP_ID
    } else {
        Toast::POWERSHELL_APP_ID
    }
}

#[cfg(target_os = "windows")]
fn register_toast_aumid() -> anyhow::Result<()> {
    use crate::APP_ID;
    use anyhow::Context;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let key_path = format!("SOFTWARE\\Classes\\AppUserModelId\\{APP_ID}");
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(&key_path)
        .with_context(|| format!("failed to create/open {key_path}"))?;

    key.set_value("DisplayName", &APP_ID)
        .context("failed writing DisplayName for toast AUMID")?;

    if let Ok(exe_path) = std::env::current_exe() {
        let _ = key.set_value("IconUri", &exe_path.display().to_string());
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn send_toast_low_battery(_state: &BatteryState, _forecast: &Forecast) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
fn low_battery_title(state: &BatteryState) -> String {
    format!("{}: {}%", state.display_name, state.battery_percent)
}

#[cfg(test)]
mod tests {
    use super::{Notifier, low_battery_body, low_battery_title};
    use crate::forecast::Forecast;
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
            battery_raw: 26,
            is_charging: true,
            supports_charging_status: true,
        };

        assert!(!notifier.maybe_notify_low_battery(&state, &Forecast::Charging));
    }

    #[test]
    fn title_includes_device_and_percent() {
        let state = BatteryState {
            device_key: "test".to_string(),
            display_name: "Test Mouse".to_string(),
            pid: 0x0072,
            battery_percent: 10,
            battery_raw: 26,
            is_charging: false,
            supports_charging_status: true,
        };

        assert_eq!(low_battery_title(&state), "Test Mouse: 10%");
    }

    #[test]
    fn body_includes_runout_when_discharging() {
        assert_eq!(
            low_battery_body(&Forecast::Estimating),
            "Plug in charger soon"
        );
        let fc = Forecast::Discharging {
            rate_pct_per_h: 4.0,
            time_to_empty: Duration::from_secs(90 * 60),
        };
        assert_eq!(low_battery_body(&fc), "~1h 30m left — plug in soon");
    }

    #[test]
    fn cooldown_suppresses_repeat_notifications() {
        let mut notifier = Notifier::new(15, 120);
        let state = BatteryState {
            device_key: "test".to_string(),
            display_name: "Test Mouse".to_string(),
            pid: 0x0072,
            battery_percent: 10,
            battery_raw: 26,
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
