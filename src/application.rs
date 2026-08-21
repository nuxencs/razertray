use crate::config::{AlertScope, AppConfig, ViewMode};
use crate::forecast::{Estimate, Forecaster, format_estimate};
use crate::model::{BatteryState, ChargeState, PollError, PollErrorScope, PollOutcome};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PollActivity {
    Idle,
    Checking,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservationView {
    NeverObserved,
    Fresh {
        reading: BatteryState,
    },
    Stale {
        reading: BatteryState,
        age: Duration,
    },
    NoDevice,
    Failed {
        scope: PollErrorScope,
        kind: crate::model::PollErrorKind,
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrayIconState {
    Unknown,
    Battery {
        percent: u8,
        charge_state: ChargeState,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceChoiceView {
    pub id: String,
    pub label: String,
    pub displayed: bool,
    pub available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrayView {
    pub polling: PollActivity,
    pub observation: ObservationView,
    pub diagnostic: Option<PollError>,
    pub status_text: String,
    pub tooltip: String,
    pub icon: TrayIconState,
    pub devices: Vec<DeviceChoiceView>,
    pub refresh_enabled: bool,
    pub forecast: Option<Estimate>,
    pub config: AppConfig,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PollId(u64);

impl PollId {
    pub fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug)]
pub enum AppEvent {
    PollStarted(PollId),
    PollFinished(PollId, PollOutcome),
    SelectDevice(String),
    SetViewMode(ViewMode),
    SetAlertScope(AlertScope),
    SetLowBatteryThreshold(u8),
    SetPollInterval(u64),
    MarkWelcomeShown,
    RestoreConfig(ConfigRollback),
}

#[derive(Clone, Debug)]
pub struct ConfigRollback {
    config: AppConfig,
    last_displayed_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Command {
    SaveConfig {
        config: AppConfig,
        rollback: ConfigRollback,
    },
    ApplyPollInterval(u64),
    NotifyCandidate {
        reading: BatteryState,
        estimate: Option<Estimate>,
    },
}

#[derive(Clone, Debug)]
pub struct Update {
    pub view: TrayView,
    pub commands: Vec<Command>,
}

#[derive(Clone, Debug)]
struct TrackedReading {
    reading: BatteryState,
    observed_at: Instant,
}

pub struct AppCore {
    config: AppConfig,
    polling: PollActivity,
    active_poll: Option<PollId>,
    next_poll_id: u64,
    readings: BTreeMap<String, TrackedReading>,
    available: BTreeSet<String>,
    last_displayed_id: Option<String>,
    last_errors: Vec<PollError>,
    has_polled: bool,
    forecaster: Forecaster,
    forecasts: BTreeMap<String, Estimate>,
}

impl AppCore {
    pub fn new(mut config: AppConfig, now: Instant) -> (Self, PollId, Update) {
        config.validate();
        let mut core = Self {
            config,
            polling: PollActivity::Idle,
            active_poll: None,
            next_poll_id: 1,
            readings: BTreeMap::new(),
            available: BTreeSet::new(),
            last_displayed_id: None,
            last_errors: Vec::new(),
            has_polled: false,
            forecaster: Forecaster::default(),
            forecasts: BTreeMap::new(),
        };
        let poll_id = core.next_poll_id();
        core.polling = PollActivity::Checking;
        core.active_poll = Some(poll_id);
        let update = core.update(now, Vec::new());
        (core, poll_id, update)
    }

    pub fn next_poll_id(&mut self) -> PollId {
        let id = PollId(self.next_poll_id);
        self.next_poll_id = self.next_poll_id.saturating_add(1);
        id
    }

    pub fn handle(&mut self, event: AppEvent, now: Instant) -> Update {
        let mut commands = Vec::new();
        match event {
            AppEvent::PollStarted(id) => {
                if self.polling == PollActivity::Idle {
                    self.polling = PollActivity::Checking;
                    self.active_poll = Some(id);
                }
            }
            AppEvent::PollFinished(id, outcome) => {
                if self.active_poll != Some(id) {
                    return self.update(now, commands);
                }
                self.polling = PollActivity::Idle;
                self.active_poll = None;
                self.has_polled = true;
                match outcome {
                    Ok(mut result) => {
                        result.sort_devices();
                        self.available.clear();
                        for device in result.devices {
                            match self.forecaster.observe(&device, now) {
                                Some(estimate) => {
                                    self.forecasts.insert(device.device_key.clone(), estimate);
                                }
                                None => {
                                    self.forecasts.remove(&device.device_key);
                                }
                            }
                            self.available.insert(device.device_key.clone());
                            self.readings.insert(
                                device.device_key.clone(),
                                TrackedReading {
                                    reading: device,
                                    observed_at: now,
                                },
                            );
                        }
                        self.last_errors = result.errors;
                        if let Some(id) = self.available_display_id().map(str::to_string) {
                            self.last_displayed_id = Some(id);
                        }
                        self.enqueue_notification_candidates(&mut commands);
                    }
                    Err(error) => {
                        self.available.clear();
                        self.last_errors = vec![error];
                    }
                }
            }
            AppEvent::SelectDevice(id) => {
                let rollback = self.config_rollback();
                if self.available.contains(&id) {
                    self.last_displayed_id = Some(id.clone());
                }
                self.config.selected_device_id = id;
                self.save_config_command(rollback, &mut commands);
            }
            AppEvent::SetViewMode(mode) => {
                let rollback = self.config_rollback();
                self.config.view_mode = mode;
                self.save_config_command(rollback, &mut commands);
            }
            AppEvent::SetAlertScope(scope) => {
                let rollback = self.config_rollback();
                self.config.alert_scope = scope;
                self.save_config_command(rollback, &mut commands);
            }
            AppEvent::SetLowBatteryThreshold(threshold) => {
                let rollback = self.config_rollback();
                self.config.low_battery_threshold = threshold;
                self.config.validate();
                self.save_config_command(rollback, &mut commands);
            }
            AppEvent::SetPollInterval(seconds) => {
                let rollback = self.config_rollback();
                self.config.poll_interval_seconds = seconds;
                self.config.validate();
                if self.save_config_command(rollback, &mut commands) {
                    commands.push(Command::ApplyPollInterval(
                        self.config.poll_interval_seconds,
                    ));
                }
            }
            AppEvent::MarkWelcomeShown => {
                let rollback = self.config_rollback();
                self.config.welcome_shown = true;
                self.save_config_command(rollback, &mut commands);
            }
            AppEvent::RestoreConfig(rollback) => {
                self.config = rollback.config;
                self.last_displayed_id = rollback.last_displayed_id;
            }
        }
        self.update(now, commands)
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    fn config_rollback(&self) -> ConfigRollback {
        ConfigRollback {
            config: self.config.clone(),
            last_displayed_id: self.last_displayed_id.clone(),
        }
    }

    fn save_config_command(&self, rollback: ConfigRollback, commands: &mut Vec<Command>) -> bool {
        if self.config != rollback.config {
            commands.push(Command::SaveConfig {
                config: self.config.clone(),
                rollback,
            });
            true
        } else {
            false
        }
    }

    fn enqueue_notification_candidates(&self, commands: &mut Vec<Command>) {
        let Some(displayed) = self.displayed_reading() else {
            return;
        };

        match self.config.alert_scope {
            AlertScope::Selected => {
                if self.available.contains(&displayed.reading.device_key) {
                    commands.push(Command::NotifyCandidate {
                        reading: displayed.reading.clone(),
                        estimate: self.forecasts.get(&displayed.reading.device_key).copied(),
                    });
                }
            }
            AlertScope::All => {
                for id in &self.available {
                    if let Some(reading) = self.readings.get(id) {
                        commands.push(Command::NotifyCandidate {
                            reading: reading.reading.clone(),
                            estimate: self.forecasts.get(id).copied(),
                        });
                    }
                }
            }
        }
    }

    fn update(&self, now: Instant, commands: Vec<Command>) -> Update {
        Update {
            view: self.project(now),
            commands,
        }
    }

    fn project(&self, now: Instant) -> TrayView {
        let displayed = self.displayed_reading();
        let diagnostic = self.projected_diagnostic(displayed).cloned();
        let observation = if let Some(tracked) = displayed {
            if self.available.contains(&tracked.reading.device_key) {
                ObservationView::Fresh {
                    reading: tracked.reading.clone(),
                }
            } else {
                ObservationView::Stale {
                    reading: tracked.reading.clone(),
                    age: now.saturating_duration_since(tracked.observed_at),
                }
            }
        } else if !self.has_polled {
            ObservationView::NeverObserved
        } else if let Some(error) = &diagnostic {
            ObservationView::Failed {
                scope: error.scope,
                kind: error.kind,
                message: error.message.clone(),
            }
        } else {
            ObservationView::NoDevice
        };

        let forecast = match &observation {
            ObservationView::Fresh { reading } => self.forecasts.get(&reading.device_key).copied(),
            _ => None,
        };
        let (status_text, tooltip, icon) =
            presentation(&observation, self.polling, forecast, diagnostic.as_ref());
        let devices = self.device_choices();
        TrayView {
            polling: self.polling,
            observation,
            diagnostic,
            status_text,
            tooltip,
            icon,
            devices,
            refresh_enabled: self.polling == PollActivity::Idle,
            forecast,
            config: self.config.clone(),
        }
    }

    fn displayed_reading(&self) -> Option<&TrackedReading> {
        if let Some(id) = self.available_display_id() {
            return self.readings.get(id);
        }

        self.last_displayed_id
            .as_ref()
            .and_then(|id| self.readings.get(id))
            .or_else(|| {
                self.readings
                    .get(&self.config.selected_device_id)
                    .filter(|_| !self.config.selected_device_id.is_empty())
            })
    }

    fn projected_diagnostic(&self, displayed: Option<&TrackedReading>) -> Option<&PollError> {
        if !self.config.selected_device_id.is_empty()
            && let Some(error) = self
                .last_errors
                .iter()
                .find(|error| error.device_key == self.config.selected_device_id)
        {
            return Some(error);
        }

        if let Some(displayed) = displayed
            && let Some(error) = self
                .last_errors
                .iter()
                .find(|error| error.device_key == displayed.reading.device_key)
        {
            return Some(error);
        }

        self.last_errors.first()
    }

    fn available_display_id(&self) -> Option<&str> {
        if !self.config.selected_device_id.is_empty()
            && self.available.contains(&self.config.selected_device_id)
        {
            return Some(&self.config.selected_device_id);
        }
        self.available.iter().next().map(String::as_str)
    }

    fn device_choices(&self) -> Vec<DeviceChoiceView> {
        let displayed_id = self
            .displayed_reading()
            .map(|tracked| &tracked.reading.device_key);
        let mut ids = self.available.clone();
        if !self.config.selected_device_id.is_empty()
            && self.readings.contains_key(&self.config.selected_device_id)
        {
            ids.insert(self.config.selected_device_id.clone());
        }
        if let Some(last_displayed_id) = &self.last_displayed_id
            && self.readings.contains_key(last_displayed_id)
        {
            ids.insert(last_displayed_id.clone());
        }

        ids.into_iter()
            .filter_map(|id| {
                let tracked = self.readings.get(&id)?;
                let available = self.available.contains(&id);
                let preferred = id == self.config.selected_device_id;
                let mut label = format!(
                    "{} - {}%{}",
                    tracked.reading.display_name,
                    tracked.reading.battery_percent,
                    charge_suffix(tracked.reading.charge_state)
                );
                if preferred && !available {
                    label.push_str(" (preferred, unavailable)");
                } else if !available {
                    label.push_str(" (unavailable)");
                }
                Some(DeviceChoiceView {
                    displayed: displayed_id == Some(&id),
                    id,
                    label,
                    available,
                })
            })
            .collect()
    }
}

fn presentation(
    observation: &ObservationView,
    polling: PollActivity,
    forecast: Option<Estimate>,
    diagnostic: Option<&PollError>,
) -> (String, String, TrayIconState) {
    match observation {
        ObservationView::NeverObserved => {
            let text = "Checking for Razer devices...".to_string();
            (text.clone(), text, TrayIconState::Unknown)
        }
        ObservationView::Fresh { reading } => {
            let mut text = format!(
                "{}: {}%{}",
                reading.display_name,
                reading.battery_percent,
                charge_suffix(reading.charge_state)
            );
            if let Some(estimate) = forecast {
                text.push_str(" - ");
                text.push_str(&format_estimate(estimate));
            }
            append_diagnostic(&mut text, diagnostic);
            let status = if polling == PollActivity::Checking {
                format!("{text} - refreshing...")
            } else {
                text.clone()
            };
            (
                status,
                text,
                TrayIconState::Battery {
                    percent: reading.battery_percent,
                    charge_state: reading.charge_state,
                },
            )
        }
        ObservationView::Stale { reading, age } => {
            let mut text = format!(
                "{}: {}%{} - last updated {}",
                reading.display_name,
                reading.battery_percent,
                charge_suffix(reading.charge_state),
                format_age(*age)
            );
            append_diagnostic(&mut text, diagnostic);
            (
                text.clone(),
                text,
                TrayIconState::Battery {
                    percent: reading.battery_percent,
                    charge_state: reading.charge_state,
                },
            )
        }
        ObservationView::NoDevice => {
            let text = "No battery-capable Razer device found".to_string();
            (text.clone(), text, TrayIconState::Unknown)
        }
        ObservationView::Failed { scope, kind, .. } => {
            let text = poll_error_status(*scope, *kind).to_string();
            (text.clone(), text, TrayIconState::Unknown)
        }
    }
}

fn append_diagnostic(text: &mut String, diagnostic: Option<&PollError>) {
    if let Some(error) = diagnostic {
        text.push_str(" - ");
        text.push_str(&error.display_name);
        text.push_str(": ");
        text.push_str(poll_error_status(error.scope, error.kind));
    }
}

fn poll_error_status(scope: PollErrorScope, kind: crate::model::PollErrorKind) -> &'static str {
    match (scope, kind) {
        (PollErrorScope::Device, crate::model::PollErrorKind::AccessDenied) => {
            "Device access denied - check permissions and refresh"
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::DeviceUnavailable) => {
            "Device unavailable - wake or reconnect it"
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::Unsupported) => {
            "Battery reporting is unsupported by this device"
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::Protocol) => {
            "Battery response invalid - refresh to retry"
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::Unknown) => {
            "Battery reading unavailable - refresh to retry"
        }
        (PollErrorScope::ChargeState, crate::model::PollErrorKind::AccessDenied) => {
            "Charging status access denied - check permissions and refresh"
        }
        (PollErrorScope::ChargeState, crate::model::PollErrorKind::Unsupported) => {
            "Charging status is not reported by this device"
        }
        (PollErrorScope::ChargeState, crate::model::PollErrorKind::Protocol) => {
            "Charging status response invalid - refresh to retry"
        }
        (PollErrorScope::ChargeState, _) => "Charging status unavailable - refresh to retry",
        (PollErrorScope::Subsystem, crate::model::PollErrorKind::AccessDenied) => {
            "HID access denied - check permissions and refresh"
        }
        (PollErrorScope::Subsystem, crate::model::PollErrorKind::Protocol) => {
            "HID response invalid - refresh to retry"
        }
        (PollErrorScope::Subsystem, crate::model::PollErrorKind::Unsupported) => {
            "HID subsystem is unsupported"
        }
        (PollErrorScope::Subsystem, _) => "HID access unavailable - refresh to retry",
    }
}

fn charge_suffix(state: ChargeState) -> &'static str {
    match state {
        ChargeState::Charging => " (charging)",
        ChargeState::NotCharging => "",
        ChargeState::Unavailable => " (charge state unavailable)",
        ChargeState::Unsupported => " (charge state not reported)",
    }
}

fn format_age(age: Duration) -> String {
    if age < Duration::from_secs(60) {
        "less than a minute ago".to_string()
    } else if age < Duration::from_secs(3_600) {
        format!("{} min ago", age.as_secs() / 60)
    } else {
        format!("{} h ago", age.as_secs() / 3_600)
    }
}

#[cfg(test)]
mod tests {
    use super::{AppCore, AppEvent, Command, ObservationView, PollActivity};
    use crate::config::AppConfig;
    use crate::model::{
        BatteryState, ChargeState, PollError, PollErrorKind, PollErrorScope, PollResult,
    };
    use std::time::{Duration, Instant};

    fn reading(id: &str, percent: u8) -> BatteryState {
        BatteryState {
            device_key: id.to_string(),
            display_name: format!("Mouse {id}"),
            pid: 1,
            battery_raw: percent,
            battery_percent: percent,
            charge_state: ChargeState::NotCharging,
        }
    }

    fn unsupported_error(id: &str) -> PollError {
        PollError {
            device_key: id.to_string(),
            display_name: format!("Mouse {id}"),
            pid: 1,
            scope: PollErrorScope::Device,
            kind: PollErrorKind::Unsupported,
            message: "battery status is not supported by this device".to_string(),
        }
    }

    #[test]
    fn boot_is_checking_and_refresh_is_disabled() {
        let now = Instant::now();
        let (_core, _poll_id, update) = AppCore::new(AppConfig::default(), now);
        assert_eq!(update.view.polling, PollActivity::Checking);
        assert_eq!(update.view.observation, ObservationView::NeverObserved);
        assert!(!update.view.refresh_enabled);
    }

    #[test]
    fn preferred_device_survives_temporary_absence_and_returns() {
        let now = Instant::now();
        let mut cfg = AppConfig {
            selected_device_id: "preferred".to_string(),
            ..AppConfig::default()
        };
        cfg.validate();
        let (mut core, first, _) = AppCore::new(cfg, now);
        core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![reading("preferred", 70), reading("fallback", 60)],
                    errors: Vec::new(),
                }),
            ),
            now,
        );

        let second = core.next_poll_id();
        core.handle(AppEvent::PollStarted(second), now);
        let missing = core.handle(
            AppEvent::PollFinished(
                second,
                Ok(PollResult {
                    devices: vec![reading("fallback", 59)],
                    errors: Vec::new(),
                }),
            ),
            now + Duration::from_secs(60),
        );
        assert_eq!(core.config().selected_device_id, "preferred");
        assert!(
            matches!(missing.view.observation, ObservationView::Fresh { reading } if reading.device_key == "fallback")
        );

        let third = core.next_poll_id();
        core.handle(AppEvent::PollStarted(third), now);
        let returned = core.handle(
            AppEvent::PollFinished(
                third,
                Ok(PollResult {
                    devices: vec![reading("preferred", 68), reading("fallback", 58)],
                    errors: Vec::new(),
                }),
            ),
            now + Duration::from_secs(120),
        );
        assert!(
            matches!(returned.view.observation, ObservationView::Fresh { reading } if reading.device_key == "preferred")
        );
    }

    #[test]
    fn stale_poll_ids_do_not_replace_newer_state() {
        let now = Instant::now();
        let (mut core, first, _) = AppCore::new(AppConfig::default(), now);
        let stale = core.next_poll_id();
        let ignored = core.handle(
            AppEvent::PollFinished(
                stale,
                Ok(PollResult {
                    devices: vec![reading("old", 10)],
                    errors: Vec::new(),
                }),
            ),
            now,
        );
        assert_eq!(ignored.view.observation, ObservationView::NeverObserved);

        let accepted = core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![reading("new", 80)],
                    errors: Vec::new(),
                }),
            ),
            now,
        );
        assert!(
            matches!(accepted.view.observation, ObservationView::Fresh { reading } if reading.device_key == "new")
        );
    }

    #[test]
    fn last_reading_becomes_stale_when_no_fallback_is_available() {
        let now = Instant::now();
        let (mut core, first, _) = AppCore::new(AppConfig::default(), now);
        core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![reading("mouse", 50)],
                    errors: Vec::new(),
                }),
            ),
            now,
        );
        let second = core.next_poll_id();
        core.handle(AppEvent::PollStarted(second), now);
        let update = core.handle(
            AppEvent::PollFinished(second, Ok(PollResult::default())),
            now + Duration::from_secs(120),
        );

        assert!(matches!(
            update.view.observation,
            ObservationView::Stale { age, .. } if age == Duration::from_secs(120)
        ));
    }

    #[test]
    fn failed_config_write_can_restore_projected_state() {
        let now = Instant::now();
        let (mut core, _, _) = AppCore::new(AppConfig::default(), now);
        let changed = core.handle(AppEvent::SetViewMode(crate::config::ViewMode::Text), now);
        let rollback = match changed.commands.as_slice() {
            [Command::SaveConfig { rollback, .. }] => rollback.clone(),
            commands => panic!("unexpected commands: {commands:?}"),
        };
        let restored = core.handle(AppEvent::RestoreConfig(rollback), now);

        assert_eq!(
            restored.view.config.view_mode,
            crate::config::ViewMode::Icon
        );
        assert!(restored.commands.is_empty());
    }

    #[test]
    fn poll_interval_applies_only_after_its_save_command() {
        let now = Instant::now();
        let (mut core, _, _) = AppCore::new(AppConfig::default(), now);
        let changed = core.handle(AppEvent::SetPollInterval(300), now);

        assert!(matches!(
            changed.commands.as_slice(),
            [Command::SaveConfig { .. }, Command::ApplyPollInterval(300)]
        ));
    }

    #[test]
    fn stale_view_keeps_the_last_displayed_fallback() {
        let now = Instant::now();
        let cfg = AppConfig {
            selected_device_id: "preferred".to_string(),
            ..AppConfig::default()
        };
        let (mut core, first, _) = AppCore::new(cfg, now);
        core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![reading("preferred", 70), reading("fallback", 60)],
                    errors: Vec::new(),
                }),
            ),
            now,
        );
        let second = core.next_poll_id();
        core.handle(AppEvent::PollStarted(second), now);
        core.handle(
            AppEvent::PollFinished(
                second,
                Ok(PollResult {
                    devices: vec![reading("fallback", 59)],
                    errors: Vec::new(),
                }),
            ),
            now + Duration::from_secs(60),
        );
        let third = core.next_poll_id();
        core.handle(AppEvent::PollStarted(third), now);
        let stale = core.handle(
            AppEvent::PollFinished(third, Ok(PollResult::default())),
            now + Duration::from_secs(120),
        );

        assert!(
            matches!(stale.view.observation, ObservationView::Stale { reading, .. } if reading.device_key == "fallback")
        );
        assert_eq!(core.config().selected_device_id, "preferred");
    }

    #[test]
    fn reliability_selected_device_becomes_the_stale_view() {
        let now = Instant::now();
        let (mut core, first, _) = AppCore::new(AppConfig::default(), now);
        core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![reading("a", 70), reading("b", 60)],
                    errors: Vec::new(),
                }),
            ),
            now,
        );

        let selected = core.handle(AppEvent::SelectDevice("b".to_string()), now);
        assert!(
            matches!(selected.view.observation, ObservationView::Fresh { reading } if reading.device_key == "b")
        );
        let second = core.next_poll_id();
        core.handle(AppEvent::PollStarted(second), now);
        let stale = core.handle(
            AppEvent::PollFinished(second, Ok(PollResult::default())),
            now + Duration::from_secs(60),
        );

        assert!(
            matches!(stale.view.observation, ObservationView::Stale { reading, .. } if reading.device_key == "b")
        );
    }

    #[test]
    fn reliability_failed_selection_save_restores_display_history() {
        let now = Instant::now();
        let (mut core, first, _) = AppCore::new(AppConfig::default(), now);
        core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![reading("a", 70), reading("b", 60)],
                    errors: Vec::new(),
                }),
            ),
            now,
        );

        let selected = core.handle(AppEvent::SelectDevice("b".to_string()), now);
        let rollback = match selected.commands.as_slice() {
            [Command::SaveConfig { rollback, .. }] => rollback.clone(),
            commands => panic!("unexpected commands: {commands:?}"),
        };
        core.handle(AppEvent::RestoreConfig(rollback), now);
        let second = core.next_poll_id();
        core.handle(AppEvent::PollStarted(second), now);
        let stale = core.handle(
            AppEvent::PollFinished(second, Ok(PollResult::default())),
            now + Duration::from_secs(60),
        );

        assert!(
            matches!(stale.view.observation, ObservationView::Stale { reading, .. } if reading.device_key == "a")
        );
        assert!(core.config().selected_device_id.is_empty());
    }

    #[test]
    fn reliability_stale_reading_keeps_current_unsupported_diagnostic() {
        let now = Instant::now();
        let (mut core, first, _) = AppCore::new(AppConfig::default(), now);
        core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![reading("mouse", 70)],
                    errors: Vec::new(),
                }),
            ),
            now,
        );
        let second = core.next_poll_id();
        core.handle(AppEvent::PollStarted(second), now);

        let update = core.handle(
            AppEvent::PollFinished(
                second,
                Ok(PollResult {
                    devices: Vec::new(),
                    errors: vec![unsupported_error("mouse")],
                }),
            ),
            now + Duration::from_secs(60),
        );

        assert!(
            matches!(update.view.observation, ObservationView::Stale { reading, .. } if reading.device_key == "mouse")
        );
        assert_eq!(
            update.view.diagnostic.as_ref().map(|error| error.kind),
            Some(PollErrorKind::Unsupported)
        );
        assert!(
            update
                .view
                .status_text
                .contains("Battery reporting is unsupported by this device")
        );
    }

    #[test]
    fn reliability_fallback_reading_keeps_preferred_device_diagnostic() {
        let now = Instant::now();
        let cfg = AppConfig {
            selected_device_id: "preferred".to_string(),
            ..AppConfig::default()
        };
        let (mut core, first, _) = AppCore::new(cfg, now);
        core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![reading("preferred", 70), reading("fallback", 60)],
                    errors: Vec::new(),
                }),
            ),
            now,
        );
        let second = core.next_poll_id();
        core.handle(AppEvent::PollStarted(second), now);

        let update = core.handle(
            AppEvent::PollFinished(
                second,
                Ok(PollResult {
                    devices: vec![reading("fallback", 59)],
                    errors: vec![unsupported_error("preferred")],
                }),
            ),
            now + Duration::from_secs(60),
        );

        assert!(
            matches!(update.view.observation, ObservationView::Fresh { reading } if reading.device_key == "fallback")
        );
        assert_eq!(
            update
                .view
                .diagnostic
                .as_ref()
                .map(|error| error.device_key.as_str()),
            Some("preferred")
        );
        assert!(update.view.status_text.contains("Mouse preferred"));
        assert!(
            update
                .view
                .tooltip
                .contains("Battery reporting is unsupported by this device")
        );
    }

    #[test]
    fn unsupported_battery_query_has_explicit_tray_status() {
        let now = Instant::now();
        let (mut core, first, _) = AppCore::new(AppConfig::default(), now);

        let update = core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: Vec::new(),
                    errors: vec![unsupported_error("mouse")],
                }),
            ),
            now,
        );

        assert!(matches!(
            update.view.observation,
            ObservationView::Failed {
                kind: PollErrorKind::Unsupported,
                ..
            }
        ));
        assert_eq!(
            update.view.status_text,
            "Battery reporting is unsupported by this device"
        );
        assert_eq!(
            update.view.diagnostic.as_ref().map(|error| error.kind),
            Some(PollErrorKind::Unsupported)
        );
    }

    #[test]
    fn diagnostic_charge_state_failure_does_not_claim_device_failure() {
        let now = Instant::now();
        let (mut core, first, _) = AppCore::new(AppConfig::default(), now);
        let mut state = reading("mouse", 70);
        state.charge_state = ChargeState::Unavailable;

        let update = core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![state],
                    errors: vec![PollError {
                        device_key: "mouse".to_string(),
                        display_name: "Mouse mouse".to_string(),
                        pid: 1,
                        scope: PollErrorScope::ChargeState,
                        kind: PollErrorKind::DeviceUnavailable,
                        message: "charging status unavailable".to_string(),
                    }],
                }),
            ),
            now,
        );

        assert!(matches!(
            update.view.observation,
            ObservationView::Fresh { reading }
                if reading.charge_state == ChargeState::Unavailable
        ));
        assert!(
            update
                .view
                .status_text
                .contains("Charging status unavailable")
        );
        assert!(update.view.tooltip.contains("Charging status unavailable"));
        assert!(!update.view.status_text.contains("Device unavailable"));
    }
}
