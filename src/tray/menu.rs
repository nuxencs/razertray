//! The tray's right-click menu.

use crate::config::{AppConfig, ViewMode};
use crate::model::{BatteryState, DeviceKey};
use anyhow::Result;
use tray_icon::menu::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};

/// Status text when no device is connected.
pub(super) const NO_DEVICES: &str = "No supported Razer devices";

/// A menu click, resolved to what the user asked for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MenuAction {
    Refresh,
    ToggleViewMode,
    ToggleAutostart,
    Exit,
    Select(DeviceKey),
}

pub(super) struct TrayMenu {
    root: Menu,
    status: MenuItem,
    devices_submenu: Submenu,
    refresh: MenuItem,
    view_mode: CheckMenuItem,
    autostart: CheckMenuItem,
    exit: MenuItem,
    devices: Vec<(CheckMenuItem, DeviceKey)>,
}

impl TrayMenu {
    pub(super) fn new(cfg: &AppConfig) -> Result<Self> {
        let menu = Self {
            root: Menu::new(),
            status: MenuItem::new(NO_DEVICES, false, None),
            devices_submenu: Submenu::new("Select Device", true),
            refresh: MenuItem::new("Refresh now", true, None),
            view_mode: CheckMenuItem::new(
                "Show percentage as text",
                true,
                cfg.view_mode == ViewMode::Text,
                None,
            ),
            autostart: CheckMenuItem::new("Start at login", true, cfg.autostart, None),
            exit: MenuItem::new("Exit", true, None),
            devices: Vec::new(),
        };

        menu.root.append_items(&[
            &menu.status,
            &menu.devices_submenu,
            &menu.refresh,
            &menu.view_mode,
            &menu.autostart,
            &PredefinedMenuItem::separator(),
            &menu.exit,
        ])?;
        Ok(menu)
    }

    pub(super) fn root(&self) -> &Menu {
        &self.root
    }

    /// Resolves a clicked menu ID. `None` for IDs that are not actions.
    pub(super) fn action(&self, id: &MenuId) -> Option<MenuAction> {
        if id == self.refresh.id() {
            Some(MenuAction::Refresh)
        } else if id == self.view_mode.id() {
            Some(MenuAction::ToggleViewMode)
        } else if id == self.autostart.id() {
            Some(MenuAction::ToggleAutostart)
        } else if id == self.exit.id() {
            Some(MenuAction::Exit)
        } else {
            self.devices
                .iter()
                .find(|(item, _)| item.id() == id)
                .map(|(_, key)| MenuAction::Select(key.clone()))
        }
    }

    // The menu library toggles a check item before it reports the click, so
    // these read the state after the click.

    pub(super) fn view_mode(&self) -> ViewMode {
        if self.view_mode.is_checked() {
            ViewMode::Text
        } else {
            ViewMode::Icon
        }
    }

    pub(super) fn autostart_checked(&self) -> bool {
        self.autostart.is_checked()
    }

    pub(super) fn set_status(&self, text: &str) {
        self.status.set_text(text);
    }

    /// Replaces the device list, with a check mark on `selected`.
    pub(super) fn set_devices(
        &mut self,
        devices: &[BatteryState],
        selected: Option<&DeviceKey>,
    ) -> Result<()> {
        while self.devices_submenu.remove_at(0).is_some() {}
        self.devices.clear();

        if devices.is_empty() {
            self.devices_submenu
                .append(&MenuItem::new("No devices", false, None))?;
            return Ok(());
        }

        for device in devices {
            let checked = Some(&device.key) == selected;
            // Stable ID per device: polls rebuild the list, possibly while the
            // menu is open, and a click on a replaced item must still resolve.
            let id = format!("device:{}", device.key);
            let item = CheckMenuItem::with_id(id, device_label(device), true, checked, None);
            self.devices_submenu.append(&item)?;
            self.devices.push((item, device.key.clone()));
        }
        Ok(())
    }

    /// Moves the check mark to `selected`. Clicking a checked item unchecks
    /// it, so this also puts the mark back on a re-clicked item.
    pub(super) fn check_device(&self, selected: &DeviceKey) {
        for (item, key) in &self.devices {
            item.set_checked(key == selected);
        }
    }
}

fn device_label(device: &BatteryState) -> String {
    let charging = if device.charging { " charging" } else { "" };
    format!(
        "{} ({:04X}) - {}{charging}",
        device.name, device.pid, device.percent
    )
}

#[cfg(test)]
mod tests {
    use super::device_label;
    use crate::model::{BatteryState, DeviceKey, Percent};

    #[test]
    fn device_label_shows_name_pid_percent_and_charging() {
        let mut device = BatteryState {
            key: DeviceKey::new(0x00B6, None),
            name: "Razer Viper".to_owned(),
            pid: 0x00B6,
            percent: Percent::try_from(76).expect("valid percent"),
            charging: false,
        };
        assert_eq!(device_label(&device), "Razer Viper (00B6) - 76%");
        device.charging = true;
        assert_eq!(device_label(&device), "Razer Viper (00B6) - 76% charging");
    }
}
