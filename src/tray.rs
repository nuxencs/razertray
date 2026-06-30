use crate::APP_ID;
use crate::autostart;
use crate::config::{self, AppConfig, PidCache};
use crate::forecast::{Forecast, Forecaster, Observation, format_duration_hm};
use crate::hid::client;
use crate::icon;
use crate::model::{BatteryState, PollError, PollResult};
use crate::notify::Notifier;
use anyhow::{Context, Result};
use hidapi::HidApi;
use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Debug, Clone)]
enum UserEvent {
    Menu(String),
    Poll(PollResult),
}

enum WorkerCommand {
    Refresh,
    Exit,
}

struct MenuHandles {
    root: Menu,
    status_item: MenuItem,
    select_submenu: Submenu,
    refresh_item: MenuItem,
    view_mode_item: CheckMenuItem,
    autostart_item: CheckMenuItem,
    exit_item: MenuItem,
    device_items: Vec<CheckMenuItem>,
}

impl MenuHandles {
    fn build(initial_autostart: bool, initial_text_mode: bool) -> Result<Self> {
        let root = Menu::new();

        let status_item = MenuItem::new("No supported Razer devices", false, None);
        let select_submenu = Submenu::new("Select Device", true);
        let refresh_item = MenuItem::with_id("refresh", "Refresh now", true, None);
        let view_mode_item = CheckMenuItem::with_id(
            "viewmode",
            "Show percentage as text",
            true,
            initial_text_mode,
            None,
        );
        let autostart_item =
            CheckMenuItem::with_id("autostart", "Start at login", true, initial_autostart, None);
        let exit_item = MenuItem::with_id("exit", "Exit", true, None);
        let separator = PredefinedMenuItem::separator();

        root.append_items(&[
            &status_item,
            &select_submenu,
            &refresh_item,
            &view_mode_item,
            &autostart_item,
            &separator,
            &exit_item,
        ])?;

        Ok(Self {
            root,
            status_item,
            select_submenu,
            refresh_item,
            view_mode_item,
            autostart_item,
            exit_item,
            device_items: Vec::new(),
        })
    }

    fn rebuild_device_menu(
        &mut self,
        devices: &[BatteryState],
        selected_device_id: &str,
    ) -> Result<()> {
        for item in self.select_submenu.items() {
            remove_item(&self.select_submenu, &item)?;
        }
        self.device_items.clear();

        if devices.is_empty() {
            let empty = MenuItem::new("No devices", false, None);
            self.select_submenu.append(&empty)?;
            return Ok(());
        }

        for device in devices {
            let checked = device.device_key == selected_device_id;
            let label = format!(
                "{} ({:04X}) - {}%{}",
                device.display_name,
                device.pid,
                device.battery_percent,
                if device.is_charging { " charging" } else { "" }
            );

            let menu_id = format!("device:{}", device.device_key);
            let item = CheckMenuItem::with_id(menu_id, label, true, checked, None);
            self.select_submenu.append(&item)?;
            self.device_items.push(item);
        }

        Ok(())
    }

    fn set_selected(&self, selected_device_id: &str) {
        for item in &self.device_items {
            let is_selected = item.id().0.strip_prefix("device:") == Some(selected_device_id);
            item.set_checked(is_selected);
        }
    }

    fn touch_ids(&self) {
        // Keep explicit reads so clippy/rustc treat these handles as used.
        // We retain them to keep the corresponding menu items alive.
        let _ = self.refresh_item.id();
        let _ = self.exit_item.id();
    }
}

pub fn run_tray_app(mut cfg: AppConfig) -> Result<()> {
    let exe_path = std::env::current_exe().context("failed resolving executable path")?;
    if let Err(err) = autostart::set_enabled(&exe_path, cfg.autostart) {
        tracing::warn!("failed to apply autostart setting: {err}");
    }

    let autostart_enabled = autostart::is_enabled().unwrap_or(cfg.autostart);

    let cache = config::load_or_create_pid_cache().unwrap_or_else(|_| PidCache::default());

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    MenuEvent::set_event_handler(Some({
        let proxy = proxy.clone();
        move |event: MenuEvent| {
            let _ = proxy.send_event(UserEvent::Menu(event.id.0.clone()));
        }
    }));

    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();

    spawn_poll_worker(
        proxy.clone(),
        cmd_rx,
        cache.clone(),
        cfg.poll_interval_seconds.max(5),
    );

    let mut text_mode = cfg.text_mode();
    let mut menu = MenuHandles::build(autostart_enabled, text_mode)?;
    menu.touch_ids();

    let initial_icon = icon::neutral_icon()?;
    let mut tray_icon = build_tray_icon(&menu.root, initial_icon)?;

    let mut notifier = Notifier::new(cfg.low_battery_threshold, cfg.low_battery_cooldown_minutes);
    let mut devices: Vec<BatteryState> = Vec::new();
    let mut selected_device_id = cfg.selected_device_id.clone();

    // Battery history → runout forecast, and the latest forecast per device so
    // menu/visual refreshes outside the poll path can reuse it.
    let mut forecaster = Forecaster::new();
    let mut forecasts: HashMap<String, Forecast> = HashMap::new();

    // Collapses the wireless-sleep error flood to a transition + periodic
    // summary + recovery line instead of one WARN per poll.
    let mut error_tracker = PollErrorTracker::new(ERROR_SUMMARY_INTERVAL);

    // Tolerate brief gaps: keep showing the last reading for up to this many
    // consecutive empty polls before clearing the tray to "no device".
    let mut missed_polls: u32 = 0;
    const MAX_MISSED_POLLS: u32 = 3;

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;

        if let Event::UserEvent(user_event) = event {
            match user_event {
                UserEvent::Menu(menu_id) => {
                    if menu_id == "refresh" {
                        let _ = cmd_tx.send(WorkerCommand::Refresh);
                    } else if menu_id == "exit" {
                        let _ = cmd_tx.send(WorkerCommand::Exit);
                        *control_flow = ControlFlow::Exit;
                    } else if menu_id == "autostart" {
                        // muda already toggled the checkbox before firing this
                        // event (see its `menu_selected`), so `is_checked()`
                        // holds the post-click state. Re-setting it here would
                        // toggle a second time and make the click a no-op.
                        let enabled = menu.autostart_item.is_checked();

                        if let Err(err) = autostart::set_enabled(&exe_path, enabled) {
                            tracing::warn!("failed to set autostart: {err}");
                        }

                        cfg.autostart = enabled;
                        if let Err(err) = config::save_config(&cfg) {
                            tracing::warn!("failed saving config: {err}");
                        }
                    } else if menu_id == "viewmode" {
                        // muda already toggled the check mark before firing this
                        // event, so is_checked() holds the post-click state.
                        text_mode = menu.view_mode_item.is_checked();
                        cfg.view_mode = if text_mode { "text" } else { "icon" }.to_string();

                        if let Err(err) = config::save_config(&cfg) {
                            tracing::warn!("failed saving config: {err}");
                        }

                        if let Err(err) = refresh_tray_visuals(
                            &mut tray_icon,
                            &devices,
                            &selected_device_id,
                            &menu.status_item,
                            text_mode,
                            forecasts.get(&selected_device_id),
                        ) {
                            tracing::warn!("failed updating tray visuals: {err}");
                        }
                    } else if let Some(device_id) = menu_id.strip_prefix("device:") {
                        selected_device_id = device_id.to_string();
                        cfg.selected_device_id = selected_device_id.clone();
                        menu.set_selected(&selected_device_id);

                        if let Err(err) = config::save_config(&cfg) {
                            tracing::warn!("failed saving config: {err}");
                        }

                        if let Err(err) = refresh_tray_visuals(
                            &mut tray_icon,
                            &devices,
                            &selected_device_id,
                            &menu.status_item,
                            text_mode,
                            forecasts.get(&selected_device_id),
                        ) {
                            tracing::warn!("failed updating tray visuals: {err}");
                        }
                    }
                }
                UserEvent::Poll(mut poll_result) => {
                    let now = Instant::now();
                    poll_result.sort_devices();
                    let PollResult {
                        devices: new_devices,
                        errors,
                    } = poll_result;

                    // Track failures before the transient-miss early return, so a
                    // device going to sleep is still recorded each cycle.
                    error_tracker.observe(now, &new_devices, &errors);

                    // Keep the last known reading through brief gaps: a poll that
                    // comes back empty while we still have devices is treated as a
                    // transient miss until it persists for MAX_MISSED_POLLS cycles.
                    if new_devices.is_empty() && !devices.is_empty() {
                        missed_polls += 1;
                        if missed_polls < MAX_MISSED_POLLS {
                            tracing::debug!(
                                "transient empty poll ({missed_polls}/{MAX_MISSED_POLLS}); keeping last reading"
                            );
                            return;
                        }
                    }
                    missed_polls = 0;
                    devices = new_devices;

                    // Feed each fresh reading to the forecaster and log a battery
                    // line whenever the level or charging state actually changes.
                    for device in &devices {
                        let obs = forecaster.observe(
                            &device.device_key,
                            device.battery_raw,
                            device.battery_percent,
                            device.is_charging,
                            now,
                        );
                        if obs.changed {
                            log_battery_line(device, &obs);
                        }
                        forecasts.insert(device.device_key.clone(), obs.forecast);
                    }
                    forecasts.retain(|key, _| devices.iter().any(|d| &d.device_key == key));

                    if ensure_selected_device(&mut selected_device_id, &devices) {
                        cfg.selected_device_id = selected_device_id.clone();
                        if let Err(err) = config::save_config(&cfg) {
                            tracing::warn!("failed saving config: {err}");
                        }
                    }

                    if let Err(err) = menu.rebuild_device_menu(&devices, &selected_device_id) {
                        tracing::warn!("failed rebuilding menu: {err}");
                    }

                    if let Err(err) = refresh_tray_visuals(
                        &mut tray_icon,
                        &devices,
                        &selected_device_id,
                        &menu.status_item,
                        text_mode,
                        forecasts.get(&selected_device_id),
                    ) {
                        tracing::warn!("failed refreshing visuals: {err}");
                    }

                    for device in &devices {
                        let forecast = forecasts
                            .get(&device.device_key)
                            .cloned()
                            .unwrap_or(Forecast::Estimating);
                        notifier.maybe_notify_low_battery(device, &forecast);
                    }
                }
            }
        }
    });
}

fn build_tray_icon(menu: &Menu, icon: Icon) -> Result<TrayIcon> {
    TrayIconBuilder::new()
        .with_menu(Box::new(menu.clone()))
        .with_tooltip(APP_ID)
        .with_icon(icon)
        .build()
        .context("failed creating tray icon")
}

fn refresh_tray_visuals(
    tray_icon: &mut TrayIcon,
    devices: &[BatteryState],
    selected_device_id: &str,
    status_item: &MenuItem,
    text_mode: bool,
    forecast: Option<&Forecast>,
) -> Result<()> {
    let selected = devices.iter().find(|d| d.device_key == selected_device_id);

    if let Some(device) = selected {
        let icon = if text_mode {
            icon::text_icon(device.battery_percent, device.is_charging)?
        } else {
            icon::battery_icon(device.battery_percent, device.is_charging)?
        };
        tray_icon.set_icon(Some(icon))?;

        let tooltip = device_tooltip(device, forecast);
        tray_icon.set_tooltip(Some(tooltip.clone()))?;
        status_item.set_text(&tooltip);
    } else {
        tray_icon.set_icon(Some(icon::neutral_icon()?))?;
        tray_icon.set_tooltip(Some("No supported Razer devices"))?;
        status_item.set_text("No supported Razer devices");
    }

    Ok(())
}

/// Tooltip / status text for a device, appending the runout estimate while
/// discharging (e.g. `DeathAdder: 78% · ~25h left`).
fn device_tooltip(device: &BatteryState, forecast: Option<&Forecast>) -> String {
    let mut text = format!("{}: {}%", device.display_name, device.battery_percent);
    if device.is_charging {
        text.push_str(" (charging)");
    } else if let Some(forecast @ Forecast::Discharging { .. }) = forecast {
        text.push_str(" · ");
        text.push_str(&forecast.short());
    }
    text
}

/// Emit one INFO line summarising a battery reading: the drop since the previous
/// reading plus the current runout estimate. This is the signal that used to be
/// missing from the log entirely.
fn log_battery_line(device: &BatteryState, obs: &Observation) {
    let charging = if device.is_charging {
        " (charging)"
    } else {
        ""
    };
    match obs.since_last {
        Some(since) if obs.delta_percent != 0 => tracing::info!(
            "{}: {}%{} ({:+}% in {}, {})",
            device.display_name,
            device.battery_percent,
            charging,
            obs.delta_percent,
            format_duration_hm(since),
            obs.forecast.detailed()
        ),
        _ => tracing::info!(
            "{}: {}%{} ({})",
            device.display_name,
            device.battery_percent,
            charging,
            obs.forecast.detailed()
        ),
    }
}

fn ensure_selected_device(selected_device_id: &mut String, devices: &[BatteryState]) -> bool {
    if devices.is_empty() {
        if !selected_device_id.is_empty() {
            selected_device_id.clear();
            return true;
        }
        return false;
    }

    if devices.iter().any(|d| d.device_key == *selected_device_id) {
        return false;
    }

    *selected_device_id = devices[0].device_key.clone();
    true
}

const EMPTY_RETRY_START_SECS: u64 = 2;

/// While a device keeps failing, re-log a summary at most this often instead of
/// once per poll.
const ERROR_SUMMARY_INTERVAL: Duration = Duration::from_secs(15 * 60);

fn spawn_poll_worker(
    proxy: EventLoopProxy<UserEvent>,
    cmd_rx: mpsc::Receiver<WorkerCommand>,
    mut cache: PidCache,
    poll_interval_seconds: u64,
) {
    thread::spawn(move || {
        // When a poll finds no device we retry quickly, doubling the delay each
        // time (2s, 4s, 8s, …) up to the normal interval, so a sleeping or
        // not-yet-ready device is picked up fast without hammering the USB bus
        // forever when nothing is connected.
        let mut empty_backoff = EMPTY_RETRY_START_SECS;

        // Create the HidApi handle once and reuse it across polls;
        // `refresh_devices` picks up newly attached/removed devices without a
        // full re-enumeration. The handle is recreated lazily if init ever fails.
        let mut api: Option<HidApi> = None;

        loop {
            if api.is_none() {
                match HidApi::new() {
                    Ok(a) => api = Some(a),
                    Err(err) => tracing::warn!("failed initializing hidapi: {err}"),
                }
            }

            let poll_result = match api.as_mut() {
                Some(a) => {
                    let _ = a.refresh_devices();
                    client::poll_devices(a, &mut cache)
                }
                None => PollResult {
                    devices: Vec::new(),
                    errors: vec![PollError {
                        device_key: String::new(),
                        display_name: String::new(),
                        pid: None,
                        message: "hidapi unavailable".to_string(),
                    }],
                },
            };

            if let Err(err) = config::save_pid_cache(&cache) {
                tracing::warn!("failed saving pid cache: {err}");
            }

            let found = !poll_result.devices.is_empty();
            let _ = proxy.send_event(UserEvent::Poll(poll_result));

            // Sleep the full interval once we have a device; otherwise back off
            // quickly so the next attempt is soon after a device appears.
            let wait = if found {
                empty_backoff = EMPTY_RETRY_START_SECS;
                poll_interval_seconds
            } else {
                let w = empty_backoff.min(poll_interval_seconds);
                empty_backoff = (empty_backoff * 2).min(poll_interval_seconds);
                w
            };

            match cmd_rx.recv_timeout(Duration::from_secs(wait)) {
                Ok(WorkerCommand::Refresh) => continue,
                Ok(WorkerCommand::Exit) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

fn remove_item(submenu: &Submenu, item: &tray_icon::menu::MenuItemKind) -> Result<()> {
    match item {
        tray_icon::menu::MenuItemKind::MenuItem(it) => submenu.remove(it)?,
        tray_icon::menu::MenuItemKind::Submenu(it) => submenu.remove(it)?,
        tray_icon::menu::MenuItemKind::Predefined(it) => submenu.remove(it)?,
        tray_icon::menu::MenuItemKind::Check(it) => submenu.remove(it)?,
        tray_icon::menu::MenuItemKind::Icon(it) => submenu.remove(it)?,
    }
    Ok(())
}

struct ErrState {
    /// Whether the current episode is benign wireless-sleep noise. We dedupe on
    /// this category, not the exact message: a sleeping device fails at different
    /// transport stages from poll to poll (all "unreachable"), and treating each
    /// wording as a new fault would re-flood the log and reset the summary timer.
    unreachable: bool,
    since: Instant,
    last_log: Instant,
    count: u32,
}

/// Throttles repeated poll failures: logs the first failure (and any change of
/// failure mode), a periodic summary while it persists, and a recovery line when
/// the device reads successfully again. Keeps the benign wireless-sleep noise out
/// of the log without hiding genuine, changing faults.
struct PollErrorTracker {
    states: HashMap<String, ErrState>,
    summary_interval: Duration,
}

impl PollErrorTracker {
    fn new(summary_interval: Duration) -> Self {
        Self {
            states: HashMap::new(),
            summary_interval,
        }
    }

    fn observe(&mut self, now: Instant, devices: &[BatteryState], errors: &[PollError]) {
        // Any device that read successfully clears (and reports) its error state.
        for device in devices {
            if let Some(state) = self.states.remove(&device.device_key) {
                tracing::info!(
                    "{} recovered after {} failed poll(s) over {}",
                    device.display_name,
                    state.count,
                    format_duration_hm(now.saturating_duration_since(state.since))
                );
            }
        }

        for error in errors {
            // Scan-level errors carry no device key; key them by message instead.
            let key = if error.device_key.is_empty() {
                error.message.clone()
            } else {
                error.device_key.clone()
            };

            let unreachable = error.is_unreachable();
            match self.states.get_mut(&key) {
                None => {
                    log_poll_failure(error);
                    self.states.insert(
                        key,
                        ErrState {
                            unreachable,
                            since: now,
                            last_log: now,
                            count: 1,
                        },
                    );
                }
                Some(state) => {
                    state.count += 1;
                    if state.unreachable != unreachable {
                        // Failure category flipped (e.g. asleep -> genuine fault):
                        // surface it and start a new episode.
                        log_poll_failure(error);
                        state.unreachable = unreachable;
                        state.since = now;
                        state.last_log = now;
                        state.count = 1;
                    } else if now.saturating_duration_since(state.last_log) >= self.summary_interval
                    {
                        tracing::info!(
                            "still failing: {error} ({} poll(s) over {})",
                            state.count,
                            format_duration_hm(now.saturating_duration_since(state.since))
                        );
                        state.last_log = now;
                    }
                }
            }
        }
    }
}

/// First-occurrence log for a poll failure. Benign wireless-sleep failures are
/// flagged as such (and the caller suppresses their repeats); anything else is a
/// plain WARN so genuine faults stay visible.
fn log_poll_failure(error: &PollError) {
    if error.is_unreachable() {
        tracing::warn!("poll error: {error} — device unreachable (asleep?); suppressing repeats");
    } else {
        tracing::warn!("poll error: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::{PollErrorTracker, device_tooltip, ensure_selected_device};
    use crate::forecast::Forecast;
    use crate::model::{BatteryState, PollError};
    use std::time::{Duration, Instant};

    fn mk_state(id: &str) -> BatteryState {
        BatteryState {
            device_key: id.to_string(),
            display_name: "X".to_string(),
            pid: 0x0001,
            battery_percent: 50,
            battery_raw: 128,
            is_charging: false,
            supports_charging_status: true,
        }
    }

    fn unreachable_err(key: &str) -> PollError {
        PollError {
            device_key: key.to_string(),
            display_name: "X".to_string(),
            pid: Some(0x0001),
            message: "send_feature_report failed".to_string(),
        }
    }

    #[test]
    fn selected_device_falls_back_to_first() {
        let devices = vec![mk_state("a"), mk_state("b")];
        let mut selected = "missing".to_string();
        let changed = ensure_selected_device(&mut selected, &devices);
        assert!(changed);
        assert_eq!(selected, "a");
    }

    #[test]
    fn tooltip_appends_runout_only_while_discharging() {
        let mut device = mk_state("a");
        device.display_name = "DeathAdder".to_string();
        device.battery_percent = 78;

        let forecast = Forecast::Discharging {
            rate_pct_per_h: 3.1,
            time_to_empty: Duration::from_secs(25 * 3600),
        };
        assert_eq!(
            device_tooltip(&device, Some(&forecast)),
            "DeathAdder: 78% · ~25h left"
        );
        assert_eq!(
            device_tooltip(&device, Some(&Forecast::Estimating)),
            "DeathAdder: 78%"
        );

        device.is_charging = true;
        assert_eq!(
            device_tooltip(&device, Some(&forecast)),
            "DeathAdder: 78% (charging)"
        );
    }

    #[test]
    fn error_tracker_tracks_episode_then_clears_on_recovery() {
        let mut tracker = PollErrorTracker::new(Duration::from_secs(900));
        let t0 = Instant::now();

        // Failure opens an episode.
        tracker.observe(t0, &[], &[unreachable_err("a")]);
        assert_eq!(tracker.states.len(), 1);
        assert_eq!(tracker.states["a"].count, 1);

        // Repeats accumulate without opening new episodes.
        tracker.observe(t0 + Duration::from_secs(60), &[], &[unreachable_err("a")]);
        assert_eq!(tracker.states["a"].count, 2);

        // A successful read clears the episode.
        tracker.observe(t0 + Duration::from_secs(120), &[mk_state("a")], &[]);
        assert!(tracker.states.is_empty());
    }

    #[test]
    fn error_tracker_resets_episode_when_failure_mode_changes() {
        let mut tracker = PollErrorTracker::new(Duration::from_secs(900));
        let t0 = Instant::now();
        tracker.observe(t0, &[], &[unreachable_err("a")]);
        tracker.observe(t0 + Duration::from_secs(60), &[], &[unreachable_err("a")]);
        assert_eq!(tracker.states["a"].count, 2);

        // Flipping category (benign sleep -> genuine fault) starts a new episode.
        let mut changed = unreachable_err("a");
        changed.message = "device returned STATUS_FAILURE".to_string();
        assert!(!changed.is_unreachable());
        tracker.observe(t0 + Duration::from_secs(120), &[], &[changed]);
        assert_eq!(tracker.states["a"].count, 1);
        assert!(!tracker.states["a"].unreachable);
    }

    #[test]
    fn error_tracker_keeps_one_episode_for_varying_sleep_messages() {
        // A sleeping device fails at different transport stages from poll to poll;
        // all are is_unreachable(), so they must stay a single episode (no reflood,
        // no episode reset) rather than re-logging each time the wording changes.
        let mut tracker = PollErrorTracker::new(Duration::from_secs(900));
        let t0 = Instant::now();

        let mut e1 = unreachable_err("a");
        e1.message = "send_feature_report failed".to_string();
        let mut e2 = unreachable_err("a");
        e2.message = "device stayed busy/unresponsive after retries".to_string();
        let mut e3 = unreachable_err("a");
        e3.message = "expected 91 bytes, got 0".to_string();
        assert!(e1.is_unreachable() && e2.is_unreachable() && e3.is_unreachable());

        tracker.observe(t0, &[], &[e1]);
        tracker.observe(t0 + Duration::from_secs(30), &[], &[e2]);
        tracker.observe(t0 + Duration::from_secs(60), &[], &[e3]);

        // One continuous episode: count accumulates, the start time is unchanged.
        assert_eq!(tracker.states["a"].count, 3);
        assert!(tracker.states["a"].unreachable);
    }
}
