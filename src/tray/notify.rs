//! Low-battery toast notifications. The format is documented in `docs/notifications.md`.

use crate::model::{BatteryState, DeviceKey};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{Level, event};

/// Sends at most one low-battery toast per device per cooldown.
#[derive(Debug)]
pub(super) struct Notifier {
    /// Notify at or below this percentage.
    threshold: u8,
    cooldown: Duration,
    last_sent: HashMap<DeviceKey, Instant>,
    #[cfg(target_os = "windows")]
    app_id: std::cell::OnceCell<&'static str>,
}

impl Notifier {
    pub(super) fn new(threshold: u8, cooldown: Duration) -> Self {
        Self {
            threshold,
            cooldown,
            last_sent: HashMap::new(),
            #[cfg(target_os = "windows")]
            app_id: std::cell::OnceCell::new(),
        }
    }

    /// Shows a toast when `device` is low, not charging, and out of cooldown.
    /// A failed toast is logged and tried again on the next poll.
    pub(super) fn notify_if_low(&mut self, device: &BatteryState) {
        let now = Instant::now();
        if !self.is_due(device, now) {
            return;
        }

        match self.show_toast(device) {
            Ok(()) => {
                self.last_sent.insert(device.key.clone(), now);
                event!(
                    name: "notify.toast.success",
                    Level::INFO,
                    device.name = %device.name,
                    battery.percent = device.percent.get(),
                    "low-battery toast shown for {{device.name}} at {{battery.percent}}%",
                );
            }
            Err(err) => {
                event!(
                    name: "notify.toast.failure",
                    Level::WARN,
                    exception.message = %format_args!("{err:#}"),
                    "cannot show the low-battery toast: {{exception.message}}",
                );
            }
        }
    }

    fn is_due(&self, device: &BatteryState, now: Instant) -> bool {
        if device.charging || device.percent.get() > self.threshold {
            return false;
        }
        self.last_sent
            .get(&device.key)
            .is_none_or(|last| now.duration_since(*last) >= self.cooldown)
    }

    #[cfg(target_os = "windows")]
    fn show_toast(&self, device: &BatteryState) -> anyhow::Result<()> {
        use tauri_winrt_notification::Toast;

        Toast::new(self.app_id())
            .title(&toast_title(device))
            .text1("Battery low")
            .text2("Plug in charger soon")
            .show()?;
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    #[expect(clippy::unused_self, reason = "same signature as the Windows version")]
    fn show_toast(&self, _device: &BatteryState) -> anyhow::Result<()> {
        anyhow::bail!("notifications are only supported on Windows")
    }

    /// The toast sender ID. Registers our own on first use, and falls back
    /// to the PowerShell ID if that fails, so the toast still shows.
    #[cfg(target_os = "windows")]
    fn app_id(&self) -> &'static str {
        use tauri_winrt_notification::Toast;

        self.app_id.get_or_init(|| match register_app_id() {
            Ok(()) => crate::APP_ID,
            Err(err) => {
                event!(
                    name: "notify.app_id.failure",
                    Level::WARN,
                    exception.message = %format_args!("{err:#}"),
                    "cannot register the toast sender, using the PowerShell sender: {{exception.message}}",
                );
                Toast::POWERSHELL_APP_ID
            }
        })
    }
}

/// Registers the AppUserModelId that Windows shows as the toast sender.
#[cfg(target_os = "windows")]
fn register_app_id() -> anyhow::Result<()> {
    use crate::APP_ID;
    use anyhow::Context;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let key_path = format!("SOFTWARE\\Classes\\AppUserModelId\\{APP_ID}");
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(&key_path)
        .with_context(|| format!("failed to create/open {key_path}"))?;

    key.set_value("DisplayName", &APP_ID)
        .context("failed writing DisplayName for toast AUMID")?;

    // Best effort: without an icon, Windows shows a generic one.
    if let Ok(exe_path) = std::env::current_exe()
        && let Err(err) = key.set_value("IconUri", &exe_path.as_os_str())
    {
        event!(
            name: "notify.app_id_icon.failure",
            Level::DEBUG,
            exception.message = %err,
            "cannot set the toast sender icon: {{exception.message}}",
        );
    }

    Ok(())
}

#[cfg(any(target_os = "windows", test))]
fn toast_title(device: &BatteryState) -> String {
    format!("{}: {}", device.name, device.percent)
}

#[cfg(test)]
mod tests {
    use super::{Notifier, toast_title};
    use crate::model::{BatteryState, DeviceKey, Percent};
    use std::time::{Duration, Instant};

    const COOLDOWN: Duration = Duration::from_secs(2 * 60 * 60);

    fn device(percent: u8, charging: bool) -> BatteryState {
        BatteryState {
            key: DeviceKey::new(0x0072, None),
            name: "Test Mouse".to_owned(),
            pid: 0x0072,
            percent: Percent::try_from(percent).expect("valid percent"),
            charging,
        }
    }

    #[test]
    fn due_at_or_below_threshold_only() {
        let notifier = Notifier::new(15, COOLDOWN);
        let now = Instant::now();
        assert!(notifier.is_due(&device(15, false), now));
        assert!(!notifier.is_due(&device(16, false), now));
    }

    #[test]
    fn not_due_while_charging() {
        let notifier = Notifier::new(15, COOLDOWN);
        assert!(!notifier.is_due(&device(5, true), Instant::now()));
    }

    #[test]
    fn cooldown_suppresses_repeats_per_device() {
        let mut notifier = Notifier::new(15, COOLDOWN);
        let low = device(10, false);
        let now = Instant::now();
        notifier.last_sent.insert(low.key.clone(), now);

        assert!(!notifier.is_due(&low, now + Duration::from_secs(30)));
        assert!(notifier.is_due(&low, now + COOLDOWN));

        let other = BatteryState {
            key: DeviceKey::new(0x0073, None),
            ..device(10, false)
        };
        assert!(notifier.is_due(&other, now));
    }

    #[test]
    fn title_includes_device_and_percent() {
        assert_eq!(toast_title(&device(10, false)), "Test Mouse: 10%");
    }
}
