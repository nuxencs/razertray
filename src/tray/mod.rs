//! The Windows tray app: icon, menu, low-battery toasts and the poll thread.

mod autostart;
mod icon;
mod menu;
mod notify;
mod readings;
mod worker;

use crate::APP_ID;
use crate::app::load_pid_cache;
use crate::config::AppConfig;
use crate::model::{BatteryState, PollResult};
use anyhow::{Context, Result};
use menu::{MenuAction, NO_DEVICES, TrayMenu};
use notify::Notifier;
use readings::{PollUpdate, Readings};
use std::path::{Path, PathBuf};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{MenuEvent, MenuId};
use tray_icon::{TrayIcon, TrayIconBuilder};
use worker::Worker;

/// Events sent to the tray's event loop from other threads.
#[derive(Debug)]
enum UserEvent {
    Menu(MenuId),
    Poll(PollResult),
}

/// Runs the tray app until the user picks "Exit".
pub(crate) fn run(mut cfg: AppConfig) -> Result<()> {
    let exe_path = std::env::current_exe().context("failed resolving executable path")?;
    apply_autostart(&exe_path, cfg.autostart);
    // Show the real state in the menu, in case applying the setting failed.
    match autostart::is_enabled() {
        Ok(enabled) => cfg.autostart = enabled,
        Err(err) => tracing::warn!("failed reading autostart state: {err:#}"),
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some({
        let proxy = proxy.clone();
        move |event: MenuEvent| {
            // Fails only when the event loop is gone.
            let _ = proxy.send_event(UserEvent::Menu(event.id));
        }
    }));

    let worker = Worker::spawn(proxy, load_pid_cache(), cfg.poll_interval());
    let menu = TrayMenu::new(&cfg)?;
    let icon = TrayIconBuilder::new()
        .with_menu(Box::new(menu.root().clone()))
        .with_tooltip(APP_ID)
        .with_icon(icon::render(cfg.view_mode, None)?)
        .build()
        .context("failed creating tray icon")?;

    let mut app = TrayApp {
        readings: Readings::new(cfg.selected_device.clone()),
        notifier: Notifier::new(cfg.low_battery_threshold, cfg.low_battery_cooldown()),
        cfg,
        exe_path,
        menu,
        icon,
        worker,
    };

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = match event {
            Event::UserEvent(UserEvent::Menu(id)) => app.on_menu(&id),
            Event::UserEvent(UserEvent::Poll(result)) => {
                app.on_poll(result);
                ControlFlow::Wait
            }
            _ => ControlFlow::Wait,
        };
    })
}

/// Tray state, owned by the event loop.
struct TrayApp {
    cfg: AppConfig,
    exe_path: PathBuf,
    menu: TrayMenu,
    icon: TrayIcon,
    readings: Readings,
    notifier: Notifier,
    worker: Worker,
}

impl TrayApp {
    fn on_menu(&mut self, id: &MenuId) -> ControlFlow {
        let Some(action) = self.menu.action(id) else {
            return ControlFlow::Wait;
        };

        match action {
            MenuAction::Refresh => self.worker.refresh(),
            MenuAction::Exit => {
                self.worker.stop();
                return ControlFlow::Exit;
            }
            MenuAction::ToggleAutostart => {
                self.cfg.autostart = self.menu.autostart_checked();
                apply_autostart(&self.exe_path, self.cfg.autostart);
                self.save_config();
            }
            MenuAction::ToggleViewMode => {
                self.cfg.view_mode = self.menu.view_mode();
                self.save_config();
                self.refresh_icon();
            }
            MenuAction::Select(key) => {
                self.menu.check_device(&key);
                self.readings.select(key.clone());
                self.cfg.selected_device = Some(key);
                self.save_config();
                self.refresh_icon();
            }
        }
        ControlFlow::Wait
    }

    fn on_poll(&mut self, result: PollResult) {
        for failure in &result.failures {
            tracing::warn!("poll error: {failure}");
        }

        match self.readings.apply_poll(result.devices) {
            PollUpdate::Kept => {
                tracing::debug!("empty poll counted as a short gap; keeping last reading");
            }
            PollUpdate::Replaced { selection_changed } => {
                if selection_changed {
                    self.cfg.selected_device = self.readings.selected_key().cloned();
                    self.save_config();
                }
                if let Err(err) = self
                    .menu
                    .set_devices(self.readings.devices(), self.readings.selected_key())
                {
                    tracing::warn!("failed rebuilding menu: {err:#}");
                }
                self.refresh_icon();
                for device in self.readings.devices() {
                    self.notifier.notify_if_low(device);
                }
            }
        }
    }

    fn save_config(&self) {
        if let Err(err) = self.cfg.save() {
            tracing::warn!("failed saving config: {err:#}");
        }
    }

    /// Updates the icon, tooltip and status line for the selected device.
    fn refresh_icon(&mut self) {
        let selected = self.readings.selected();
        let status = selected.map_or_else(|| NO_DEVICES.to_owned(), status_text);
        self.menu.set_status(&status);

        let result = icon::render(self.cfg.view_mode, selected)
            .and_then(|icon| Ok(self.icon.set_icon(Some(icon))?))
            .and_then(|()| Ok(self.icon.set_tooltip(Some(&status))?));
        if let Err(err) = result {
            tracing::warn!("failed updating tray icon: {err:#}");
        }
    }
}

/// Tooltip and status line, for example `Razer Viper: 76% (charging)`.
fn status_text(device: &BatteryState) -> String {
    let charging = if device.charging { " (charging)" } else { "" };
    format!("{}: {}{charging}", device.name, device.percent)
}

fn apply_autostart(exe_path: &Path, enabled: bool) {
    let result = if enabled {
        autostart::enable(exe_path)
    } else {
        autostart::disable()
    };
    if let Err(err) = result {
        tracing::warn!("failed applying autostart setting: {err:#}");
    }
}
