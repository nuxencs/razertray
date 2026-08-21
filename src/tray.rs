use crate::APP_ID;
use crate::application::{
    AppCore, AppEvent, Command, ConfigRollback, PollId, TrayIconState, TrayView, Update,
};
use crate::autostart;
use crate::config::{self, AlertScope, AppConfig, ConfigRecovery, PidCache, ViewMode};
use crate::error_tracker::{ErrorNotice, ErrorTracker};
use crate::hid::client;
use crate::icon;
use crate::model::{PollError, PollOutcome, PollResult, SubsystemComponent};
use crate::notify::{self, Notifier};
use anyhow::{Context, Result};
use hidapi::HidApi;
use std::collections::BTreeSet;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

const EMPTY_RETRY_START_SECS: u64 = 2;
const THRESHOLD_OPTIONS: &[u8] = &[10, 15, 20, 25];
const INTERVAL_OPTIONS: &[(u64, &str)] = &[
    (30, "30 seconds"),
    (60, "1 minute"),
    (120, "2 minutes"),
    (300, "5 minutes"),
];

#[derive(Debug, Clone)]
enum UserEvent {
    Menu(String),
    PollStarted(PollId),
    PollFinished(PollId, PollOutcome),
}

enum WorkerCommand {
    Refresh,
    SetInterval(u64),
    Exit,
}

enum PidCacheIssue {
    Load(PollError),
    Save(PollError),
}

impl PidCacheIssue {
    fn diagnostic(&self) -> &PollError {
        match self {
            Self::Load(error) | Self::Save(error) => error,
        }
    }
}

struct MenuHandles {
    root: Menu,
    status_item: MenuItem,
    device_submenu: Submenu,
    refresh_item: MenuItem,
    view_mode_item: CheckMenuItem,
    alert_scope_item: CheckMenuItem,
    threshold_items: Vec<(u8, CheckMenuItem)>,
    interval_items: Vec<(u64, CheckMenuItem)>,
    autostart_item: CheckMenuItem,
    device_items: Vec<CheckMenuItem>,
}

impl MenuHandles {
    fn build(config: &AppConfig, autostart_enabled: Option<bool>) -> Result<Self> {
        let root = Menu::new();
        let status_item = MenuItem::new("Checking for Razer devices...", false, None);
        let device_submenu = Submenu::new("Displayed device", true);
        device_submenu.append(&MenuItem::new("Checking...", false, None))?;
        let refresh_item = MenuItem::with_id("refresh", "Refreshing...", false, None);

        let preferences = Submenu::new("Preferences", true);
        let view_mode_item = CheckMenuItem::with_id(
            "viewmode",
            "Show percentage as text",
            true,
            config.view_mode == ViewMode::Text,
            None,
        );
        let alert_scope_item = CheckMenuItem::with_id(
            "alertscope",
            "Alert for all devices",
            true,
            config.alert_scope == AlertScope::All,
            None,
        );
        let threshold_submenu = Submenu::new("Low-battery alert", true);
        let mut threshold_items = Vec::new();
        for threshold in THRESHOLD_OPTIONS {
            let item = CheckMenuItem::with_id(
                format!("threshold:{threshold}"),
                format!("{threshold}%"),
                true,
                config.low_battery_threshold == *threshold,
                None,
            );
            threshold_submenu.append(&item)?;
            threshold_items.push((*threshold, item));
        }

        let interval_submenu = Submenu::new("Check interval", true);
        let mut interval_items = Vec::new();
        for (seconds, label) in INTERVAL_OPTIONS {
            let item = CheckMenuItem::with_id(
                format!("interval:{seconds}"),
                *label,
                true,
                config.poll_interval_seconds == *seconds,
                None,
            );
            interval_submenu.append(&item)?;
            interval_items.push((*seconds, item));
        }

        let autostart_item = CheckMenuItem::with_id(
            "autostart",
            if autostart_enabled.is_some() {
                "Start at login"
            } else {
                "Start at login (status unavailable)"
            },
            autostart_enabled.is_some(),
            autostart_enabled.unwrap_or(false),
            None,
        );
        preferences.append_items(&[
            &view_mode_item,
            &alert_scope_item,
            &threshold_submenu,
            &interval_submenu,
            &autostart_item,
        ])?;

        let open_folder_item = MenuItem::with_id("open-folder", "Open app folder", true, None);
        let exit_item = MenuItem::with_id("exit", "Exit razertray", true, None);
        let separator_one = PredefinedMenuItem::separator();
        let separator_two = PredefinedMenuItem::separator();
        root.append_items(&[
            &status_item,
            &device_submenu,
            &refresh_item,
            &separator_one,
            &preferences,
            &open_folder_item,
            &separator_two,
            &exit_item,
        ])?;

        Ok(Self {
            root,
            status_item,
            device_submenu,
            refresh_item,
            view_mode_item,
            alert_scope_item,
            threshold_items,
            interval_items,
            autostart_item,
            device_items: Vec::new(),
        })
    }

    fn apply_view(&mut self, view: &TrayView) -> Result<()> {
        self.status_item.set_text(&view.status_text);
        self.refresh_item.set_enabled(view.refresh_enabled);
        self.refresh_item.set_text(if view.refresh_enabled {
            "Refresh now"
        } else {
            "Refreshing..."
        });
        self.view_mode_item
            .set_checked(view.config.view_mode == ViewMode::Text);
        self.alert_scope_item
            .set_checked(view.config.alert_scope == AlertScope::All);
        for (threshold, item) in &self.threshold_items {
            item.set_checked(*threshold == view.config.low_battery_threshold);
        }
        for (seconds, item) in &self.interval_items {
            item.set_checked(*seconds == view.config.poll_interval_seconds);
        }

        for item in self.device_submenu.items() {
            remove_item(&self.device_submenu, &item)?;
        }
        self.device_items.clear();
        if view.devices.is_empty() {
            let label = if view.refresh_enabled {
                "No devices available"
            } else {
                "Checking..."
            };
            self.device_submenu
                .append(&MenuItem::new(label, false, None))?;
        } else {
            for device in &view.devices {
                let item = CheckMenuItem::with_id(
                    format!("device:{}", device.id),
                    &device.label,
                    device.available,
                    device.displayed,
                    None,
                );
                self.device_submenu.append(&item)?;
                self.device_items.push(item);
            }
        }
        Ok(())
    }
}

pub fn run_tray_app(config: AppConfig, startup_recovery: Option<ConfigRecovery>) -> Result<()> {
    let exe_path = std::env::current_exe().context("failed resolving executable path")?;
    let autostart_enabled = match autostart::is_enabled(&exe_path) {
        Ok(enabled) => Some(enabled),
        Err(err) => {
            tracing::warn!("failed reading autostart state: {err}");
            None
        }
    };
    let config::PidCacheLoad {
        cache,
        state: cache_state,
    } = config::load_pid_cache_for_polling();
    let cache_diagnostic = cache_state.into_diagnostic();
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
        proxy,
        cmd_rx,
        cache,
        cache_diagnostic,
        config.poll_interval_seconds,
    );

    let now = Instant::now();
    let (mut core, initial_update) = AppCore::new(config, now);
    let mut menu = MenuHandles::build(core.config(), autostart_enabled)?;
    let initial_icon = icon::neutral_icon()?;
    let mut tray_icon = build_tray_icon(&menu.root, initial_icon)?;
    apply_projection(&mut menu, &mut tray_icon, &initial_update.view)?;
    let mut notifier = Notifier::new(
        core.config().low_battery_threshold,
        core.config().low_battery_cooldown_minutes,
    );
    let mut error_tracker = ErrorTracker::default();

    if let Some(recovery) = startup_recovery {
        tracing::warn!(
            "configuration recovery: {}",
            recovery.diagnostic_message()
        );
        let _ = notify::show_error(recovery.title(), recovery.notification_message());
    }

    match welcome_update(&mut core, notify::show_welcome, Instant::now()) {
        Ok(Some(update)) => {
            if let Some(rollback) = execute_commands(&update.commands, &mut notifier, &cmd_tx) {
                core.handle(AppEvent::RestoreConfig(rollback), Instant::now());
            }
        }
        Ok(None) => {}
        Err(err) => {
            tracing::warn!("failed showing welcome notification: {err}");
        }
    }

    let mut next_projection_at = None;
    event_loop.run(move |event, _target, control_flow| {
        *control_flow = next_projection_at
            .map(ControlFlow::WaitUntil)
            .unwrap_or(ControlFlow::Wait);

        let update = match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                core.handle(AppEvent::ProjectionTick, Instant::now())
            }
            Event::UserEvent(user_event) => match user_event {
                UserEvent::PollStarted(id) => {
                    core.handle(AppEvent::PollStarted(id), Instant::now())
                }
                UserEvent::PollFinished(id, result) => {
                    let (poll_errors, successful_ids, poll_completed) = match &result {
                        Ok(poll_result) => (
                            poll_result.errors.clone(),
                            poll_result
                                .devices
                                .iter()
                                .map(|device| device.device_key.clone())
                                .collect::<BTreeSet<_>>(),
                            true,
                        ),
                        Err(error) => (vec![error.clone()], BTreeSet::new(), false),
                    };
                    for notice in error_tracker.observe(
                        &poll_errors,
                        &successful_ids,
                        poll_completed,
                        Instant::now(),
                    ) {
                        log_error_notice(notice);
                    }
                    core.handle(AppEvent::PollFinished(id, result), Instant::now())
                }
                UserEvent::Menu(menu_id) => {
                    if menu_id == "refresh" {
                        let _ = cmd_tx.send(WorkerCommand::Refresh);
                        return;
                    }
                    if menu_id == "exit" {
                        let _ = cmd_tx.send(WorkerCommand::Exit);
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    if menu_id == "open-folder" {
                        if let Err(err) = open_app_folder() {
                            tracing::warn!("failed opening app folder: {err}");
                            let _ = notify::show_error(
                                "Could not open the app folder",
                                "Open %APPDATA%\\razertray in File Explorer.",
                            );
                        }
                        return;
                    }
                    if menu_id == "autostart" {
                        let requested = menu.autostart_item.is_checked();
                        match autostart::set_enabled(&exe_path, requested) {
                            Ok(()) => return,
                            Err(err) => {
                                menu.autostart_item.set_checked(!requested);
                                tracing::warn!("failed setting autostart: {err}");
                                let _ = notify::show_error(
                                    "Start at login was not changed",
                                    "Try again, or check Windows startup-app permissions.",
                                );
                                return;
                            }
                        }
                    } else if menu_id == "viewmode" {
                        let mode = if menu.view_mode_item.is_checked() {
                            ViewMode::Text
                        } else {
                            ViewMode::Icon
                        };
                        core.handle(AppEvent::SetViewMode(mode), Instant::now())
                    } else if menu_id == "alertscope" {
                        let scope = if menu.alert_scope_item.is_checked() {
                            AlertScope::All
                        } else {
                            AlertScope::Selected
                        };
                        core.handle(AppEvent::SetAlertScope(scope), Instant::now())
                    } else if let Some(raw) = menu_id.strip_prefix("threshold:") {
                        let Ok(threshold) = raw.parse::<u8>() else {
                            return;
                        };
                        core.handle(AppEvent::SetLowBatteryThreshold(threshold), Instant::now())
                    } else if let Some(raw) = menu_id.strip_prefix("interval:") {
                        let Ok(seconds) = raw.parse::<u64>() else {
                            return;
                        };
                        core.handle(AppEvent::SetPollInterval(seconds), Instant::now())
                    } else if let Some(device_id) = menu_id.strip_prefix("device:") {
                        core.handle(
                            AppEvent::SelectDevice(device_id.to_string()),
                            Instant::now(),
                        )
                    } else {
                        return;
                    }
                }
            },
            _ => return,
        };

        let update =
            if let Some(rollback) = execute_commands(&update.commands, &mut notifier, &cmd_tx) {
                core.handle(AppEvent::RestoreConfig(rollback), Instant::now())
            } else {
                update
            };
        notifier.set_policy(
            update.view.config.low_battery_threshold,
            update.view.config.low_battery_cooldown_minutes,
        );
        if let Err(err) = apply_projection(&mut menu, &mut tray_icon, &update.view) {
            tracing::warn!("failed applying tray view: {err}");
        }
        next_projection_at = update.view.next_refresh_at;
        *control_flow = next_projection_at
            .map(ControlFlow::WaitUntil)
            .unwrap_or(ControlFlow::Wait);
    });
}

fn welcome_update<F>(core: &mut AppCore, show: F, now: Instant) -> Result<Option<Update>>
where
    F: FnOnce() -> Result<()>,
{
    if core.config().welcome_shown {
        return Ok(None);
    }

    show()?;
    Ok(Some(core.handle(AppEvent::MarkWelcomeShown, now)))
}

fn execute_commands(
    commands: &[Command],
    notifier: &mut Notifier,
    worker: &mpsc::Sender<WorkerCommand>,
) -> Option<ConfigRollback> {
    for command in commands {
        match command {
            Command::SaveConfig { config, rollback } => {
                if let Err(err) = config::save_config(config) {
                    tracing::warn!("failed saving config: {err}");
                    let _ = notify::show_error(
                        "Settings were not saved",
                        "Check access to the razertray app folder, then try again.",
                    );
                    return Some(rollback.clone());
                }
            }
            Command::NotifyCandidate { reading, estimate } => {
                notifier.maybe_notify_low_battery(reading, *estimate);
            }
            Command::ApplyPollInterval(seconds) => {
                let _ = worker.send(WorkerCommand::SetInterval(*seconds));
            }
        }
    }
    None
}

fn log_error_notice(notice: ErrorNotice) {
    match notice {
        ErrorNotice::Started(error) => tracing::warn!(
            device = %error.display_name,
            scope = ?error.scope,
            kind = ?error.kind,
            "poll error: {}",
            error.message
        ),
        ErrorNotice::Repeated { error, suppressed } => tracing::warn!(
            device = %error.display_name,
            scope = ?error.scope,
            kind = ?error.kind,
            suppressed,
            "poll error continues: {}",
            error.message
        ),
        ErrorNotice::Recovered {
            display_name,
            scope,
            component,
        } => tracing::info!(
            device = %display_name,
            scope = ?scope,
            component = ?component,
            "{}",
            recovery_message(component)
        ),
    }
}

fn recovery_message(component: Option<SubsystemComponent>) -> &'static str {
    match component {
        Some(SubsystemComponent::Hid) => "HID subsystem recovered",
        Some(SubsystemComponent::PidCache) => "PID cache recovered",
        None => "device polling recovered",
    }
}

fn apply_projection(
    menu: &mut MenuHandles,
    tray_icon: &mut TrayIcon,
    view: &TrayView,
) -> Result<()> {
    menu.apply_view(view)?;
    let next_icon = match view.icon {
        TrayIconState::Unknown | TrayIconState::Stale => icon::neutral_icon()?,
        TrayIconState::Battery {
            percent,
            charge_state,
        } => {
            let charging = charge_state.is_charging();
            if view.config.text_mode() {
                icon::text_icon(percent, charging)?
            } else {
                icon::battery_icon(percent, charging)?
            }
        }
    };
    tray_icon.set_icon(Some(next_icon))?;
    tray_icon.set_tooltip(Some(&view.tooltip))?;
    Ok(())
}

fn build_tray_icon(menu: &Menu, icon: Icon) -> Result<TrayIcon> {
    TrayIconBuilder::new()
        .with_menu(Box::new(menu.clone()))
        .with_tooltip(APP_ID)
        .with_icon(icon)
        .build()
        .context("failed creating tray icon")
}

fn spawn_poll_worker(
    proxy: EventLoopProxy<UserEvent>,
    cmd_rx: mpsc::Receiver<WorkerCommand>,
    mut cache: PidCache,
    mut cache_diagnostic: Option<PollError>,
    initial_interval: u64,
) {
    thread::spawn(move || {
        let mut poll_interval = initial_interval.clamp(5, 3_600);
        let mut empty_backoff = EMPTY_RETRY_START_SECS;
        let mut next_wait = poll_interval;
        let mut api: Option<HidApi> = None;
        let mut api_init_error: Option<PollError> = None;
        let mut cache_dirty = false;
        let mut cache_issue = cache_diagnostic.take().map(PidCacheIssue::Load);
        let mut poll_number = 0_u64;
        let mut poll_now = true;

        loop {
            if poll_now {
                revalidate_pid_cache_load(&mut cache, &mut cache_issue, || {
                    config::load_or_create_pid_cache()
                });
                poll_number = poll_number.saturating_add(1);
                let poll_id = PollId::new(poll_number);
                let _ = proxy.send_event(UserEvent::PollStarted(poll_id));
                if api.is_none() {
                    match HidApi::new() {
                        Ok(handle) => {
                            api = Some(handle);
                            api_init_error = None;
                        }
                        Err(err) => {
                            let error = PollError::subsystem(format!(
                                "failed initializing HID access: {err}"
                            ));
                            tracing::warn!("{}", error.message);
                            api_init_error = Some(error);
                        }
                    }
                }

                let outcome = match api.as_mut() {
                    Some(handle) => match handle.refresh_devices() {
                        Ok(()) => {
                            let batch = client::poll_devices(handle, &mut cache);
                            cache_dirty |= batch.cache_changed;
                            Ok(batch.result)
                        }
                        Err(err) => Err(PollError::subsystem(format!(
                            "could not refresh HID devices: {err}"
                        ))),
                    },
                    None => Err(api_init_error
                        .clone()
                        .unwrap_or_else(|| PollError::subsystem("HID access is unavailable"))),
                };
                persist_pid_cache(&mut cache_dirty, &mut cache_issue, || {
                    config::save_pid_cache(&cache)
                });
                let outcome = attach_cache_diagnostic(
                    outcome,
                    cache_issue.as_ref().map(PidCacheIssue::diagnostic),
                );
                let found = outcome
                    .as_ref()
                    .is_ok_and(|result| !result.devices.is_empty());
                let _ = proxy.send_event(UserEvent::PollFinished(poll_id, outcome));
                if found {
                    empty_backoff = EMPTY_RETRY_START_SECS;
                    next_wait = poll_interval;
                } else {
                    next_wait = empty_backoff.min(poll_interval);
                    empty_backoff = (empty_backoff * 2).min(poll_interval);
                }
                poll_now = false;
            }

            match cmd_rx.recv_timeout(Duration::from_secs(next_wait)) {
                Ok(WorkerCommand::Refresh) => poll_now = true,
                Ok(WorkerCommand::SetInterval(seconds)) => {
                    poll_interval = seconds.clamp(5, 3_600);
                    next_wait = next_wait.min(poll_interval);
                }
                Ok(WorkerCommand::Exit) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => poll_now = true,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

fn attach_cache_diagnostic(outcome: PollOutcome, diagnostic: Option<&PollError>) -> PollOutcome {
    let Some(diagnostic) = diagnostic else {
        return outcome;
    };
    match outcome {
        Ok(mut result) => {
            result.errors.push(diagnostic.clone());
            Ok(result)
        }
        Err(error) => Ok(PollResult {
            devices: Vec::new(),
            errors: vec![error, diagnostic.clone()],
        }),
    }
}

fn revalidate_pid_cache_load<F>(cache: &mut PidCache, issue: &mut Option<PidCacheIssue>, load: F)
where
    F: FnOnce() -> Result<PidCache>,
{
    if !matches!(issue, Some(PidCacheIssue::Load(_))) {
        return;
    }
    match load() {
        Ok(loaded) => {
            *cache = loaded;
            *issue = None;
        }
        Err(error) => {
            let diagnostic = PollError::pid_cache(format!("PID cache unavailable: {error:#}"));
            *issue = Some(PidCacheIssue::Load(diagnostic));
        }
    }
}

fn persist_pid_cache<F>(dirty: &mut bool, issue: &mut Option<PidCacheIssue>, save: F)
where
    F: FnOnce() -> Result<()>,
{
    if matches!(issue, Some(PidCacheIssue::Load(_)))
        || (!*dirty && !matches!(issue, Some(PidCacheIssue::Save(_))))
    {
        return;
    }
    match save() {
        Ok(()) => {
            *dirty = false;
            *issue = None;
        }
        Err(error) => {
            tracing::warn!("failed saving PID cache: {error}");
            let diagnostic = PollError::pid_cache(format!("failed saving PID cache: {error:#}"));
            *issue = Some(PidCacheIssue::Save(diagnostic));
        }
    }
}

#[cfg(target_os = "windows")]
fn open_app_folder() -> Result<()> {
    std::process::Command::new("explorer.exe")
        .arg(config::app_data_dir())
        .spawn()
        .context("failed starting File Explorer")?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn open_app_folder() -> Result<()> {
    anyhow::bail!("opening the app folder is only supported on Windows")
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

#[cfg(test)]
mod tests {
    use super::{
        PidCacheIssue, attach_cache_diagnostic, persist_pid_cache, recovery_message,
        revalidate_pid_cache_load, welcome_update,
    };
    use crate::application::AppCore;
    use crate::config::{AppConfig, PidCache};
    use crate::model::{
        BatteryState, ChargeState, PollError, PollErrorKind, PollErrorScope, PollResult,
        SubsystemComponent,
    };
    use std::time::Instant;

    #[test]
    fn subsystem_access_failure_keeps_actionable_kind() {
        let denied = PollError::subsystem("HID access denied by Windows");
        let unknown = PollError::subsystem("HID refresh failed");

        assert_eq!(denied.kind, PollErrorKind::AccessDenied);
        assert_eq!(unknown.kind, PollErrorKind::Unknown);
    }

    #[test]
    fn reliability_welcome_is_marked_only_after_successful_delivery() {
        let now = Instant::now();
        let (mut core, _) = AppCore::new(AppConfig::default(), now);

        let failed = welcome_update(&mut core, || anyhow::bail!("toast delivery failed"), now);

        assert!(failed.is_err());
        assert!(!core.config().welcome_shown);

        let delivered = welcome_update(&mut core, || Ok(()), now)
            .expect("deliver welcome")
            .expect("welcome update");
        assert!(core.config().welcome_shown);
        assert!(matches!(
            delivered.commands.as_slice(),
            [crate::application::Command::SaveConfig { .. }]
        ));
    }

    #[test]
    fn review_round_17_cache_diagnostic_preserves_hid_failure() {
        let hid_error = PollError::subsystem("HID access denied");
        let cache_error = PollError::pid_cache("PID cache unavailable: permission denied");

        let result = attach_cache_diagnostic(Err(hid_error.clone()), Some(&cache_error))
            .expect("typed poll result");

        assert!(result.devices.is_empty());
        assert_eq!(result.errors, vec![hid_error, cache_error]);
        assert!(
            result
                .errors
                .iter()
                .all(|error| error.scope == PollErrorScope::Subsystem)
        );
    }

    #[test]
    fn review_round_18_repaired_cache_diagnostic_is_not_projected() {
        let result = attach_cache_diagnostic(Ok(PollResult::default()), None).expect("poll result");

        assert!(result.errors.is_empty());
    }

    #[test]
    fn review_round_19_cache_load_issue_is_revalidated_without_cache_changes() {
        let mut cache = PidCache::default();
        let mut issue = Some(PidCacheIssue::Load(PollError::pid_cache(
            "PID cache unavailable: access denied",
        )));
        let mut recovered = PidCache::default();
        recovered.set(0x1234, 0x3f);

        revalidate_pid_cache_load(&mut cache, &mut issue, || Ok(recovered));

        assert_eq!(cache.get(0x1234), Some(0x3f));
        assert!(issue.is_none());
    }

    #[test]
    fn review_round_19_cache_save_failure_keeps_reading_and_typed_diagnostic() {
        let mut dirty = true;
        let mut issue = None;

        persist_pid_cache(&mut dirty, &mut issue, || {
            anyhow::bail!("permission denied")
        });

        let reading = BatteryState {
            device_key: "mouse".to_string(),
            display_name: "Mouse".to_string(),
            pid: 0x1234,
            battery_raw: 128,
            battery_percent: 50,
            charge_state: ChargeState::NotCharging,
            observed_at: None,
        };
        let result = attach_cache_diagnostic(
            Ok(PollResult {
                devices: vec![reading.clone()],
                errors: Vec::new(),
            }),
            issue.as_ref().map(PidCacheIssue::diagnostic),
        )
        .expect("poll result");

        assert_eq!(result.devices, vec![reading]);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(
            result.errors[0].component,
            Some(SubsystemComponent::PidCache)
        );
        assert_eq!(result.errors[0].kind, PollErrorKind::AccessDenied);
        assert!(dirty);
    }

    #[test]
    fn review_round_20_cache_recovery_uses_component_wording() {
        assert_eq!(
            recovery_message(Some(SubsystemComponent::PidCache)),
            "PID cache recovered"
        );
    }
}
