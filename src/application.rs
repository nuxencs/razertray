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
    Stale,
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
    pub diagnostics: Vec<PollError>,
    pub status_text: String,
    pub tooltip: String,
    pub icon: TrayIconState,
    pub devices: Vec<DeviceChoiceView>,
    pub refresh_enabled: bool,
    pub forecast: Option<Estimate>,
    pub next_refresh_at: Option<Instant>,
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
    ProjectionTick,
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
    readings: BTreeMap<String, TrackedReading>,
    available: BTreeSet<String>,
    last_displayed_id: Option<String>,
    last_errors: Vec<PollError>,
    has_polled: bool,
    forecaster: Forecaster,
    forecasts: BTreeMap<String, Estimate>,
}

impl AppCore {
    pub fn new(mut config: AppConfig, now: Instant) -> (Self, Update) {
        config.validate();
        let core = Self {
            config,
            polling: PollActivity::Idle,
            active_poll: None,
            readings: BTreeMap::new(),
            available: BTreeSet::new(),
            last_displayed_id: None,
            last_errors: Vec::new(),
            has_polled: false,
            forecaster: Forecaster::default(),
            forecasts: BTreeMap::new(),
        };
        let update = core.update(now, Vec::new());
        (core, update)
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
                            let observed_at = device.observed_at.unwrap_or(now).min(now);
                            match self.forecaster.observe(&device, observed_at) {
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
                                    observed_at,
                                },
                            );
                        }
                        self.last_errors = result.errors;
                        if let Some(id) = self.available_display_id().map(str::to_string) {
                            self.last_displayed_id = Some(id);
                        }
                        self.invalidate_unavailable_forecasts();
                        self.enqueue_notification_candidates(&mut commands, now);
                    }
                    Err(error) => {
                        self.available.clear();
                        self.invalidate_unavailable_forecasts();
                        self.last_errors = vec![error];
                    }
                }
            }
            AppEvent::ProjectionTick => {}
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

    fn invalidate_unavailable_forecasts(&mut self) {
        let unavailable: Vec<_> = self
            .readings
            .keys()
            .filter(|id| !self.available.contains(*id))
            .cloned()
            .collect();
        for id in unavailable {
            self.forecaster.invalidate(&id);
            self.forecasts.remove(&id);
        }
    }

    fn projected_forecast(&self, device_key: &str, now: Instant) -> Option<Estimate> {
        self.forecasts
            .get(device_key)
            .copied()
            .and_then(|estimate| estimate.project(now))
    }

    fn enqueue_notification_candidates(&self, commands: &mut Vec<Command>, now: Instant) {
        let Some(displayed) = self.displayed_reading() else {
            return;
        };

        match self.config.alert_scope {
            AlertScope::Selected => {
                if self.available.contains(&displayed.reading.device_key) {
                    commands.push(Command::NotifyCandidate {
                        reading: displayed.reading.clone(),
                        estimate: self.projected_forecast(&displayed.reading.device_key, now),
                    });
                }
            }
            AlertScope::All => {
                for id in &self.available {
                    if let Some(reading) = self.readings.get(id) {
                        commands.push(Command::NotifyCandidate {
                            reading: reading.reading.clone(),
                            estimate: self.projected_forecast(id, now),
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
        let diagnostics: Vec<_> = self
            .ordered_diagnostics(displayed)
            .into_iter()
            .cloned()
            .collect();
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
        } else if let Some(error) = diagnostics.first() {
            let (scope, kind) = diagnostic_classification(&diagnostics)
                .unwrap_or((error.scope, crate::model::PollErrorKind::Unknown));
            ObservationView::Failed {
                scope,
                kind,
                message: diagnostic_summary(&diagnostics).unwrap_or_else(|| error.message.clone()),
            }
        } else {
            ObservationView::NoDevice
        };

        let forecast = match &observation {
            ObservationView::Fresh { reading } => self.projected_forecast(&reading.device_key, now),
            _ => None,
        };
        let stale_refresh_at = match displayed {
            Some(tracked) if !self.available.contains(&tracked.reading.device_key) => {
                next_age_refresh_at(tracked.observed_at, now)
            }
            _ => None,
        };
        let next_refresh_at = match (forecast.map(Estimate::refresh_at), stale_refresh_at) {
            (Some(forecast), Some(stale)) => Some(forecast.min(stale)),
            (forecast, stale) => forecast.or(stale),
        };
        let (status_text, tooltip, icon) =
            presentation(&observation, self.polling, forecast, &diagnostics);
        let devices = self.device_choices();
        TrayView {
            polling: self.polling,
            observation,
            diagnostics,
            status_text,
            tooltip,
            icon,
            devices,
            refresh_enabled: self.polling == PollActivity::Idle,
            forecast,
            next_refresh_at,
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

    fn ordered_diagnostics(&self, displayed: Option<&TrackedReading>) -> Vec<&PollError> {
        let preferred = (!self.config.selected_device_id.is_empty())
            .then_some(self.config.selected_device_id.as_str());
        let displayed = displayed.map(|tracked| tracked.reading.device_key.as_str());
        let mut diagnostics: Vec<_> = self.last_errors.iter().enumerate().collect();
        diagnostics.sort_by_key(|(index, error)| {
            let priority = if preferred == Some(error.device_key.as_str()) {
                0
            } else if displayed == Some(error.device_key.as_str()) {
                1
            } else {
                2
            };
            (priority, *index)
        });
        diagnostics.into_iter().map(|(_, error)| error).collect()
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
    diagnostics: &[PollError],
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
            append_diagnostics(&mut text, diagnostics);
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
            append_diagnostics(&mut text, diagnostics);
            (text.clone(), text, TrayIconState::Stale)
        }
        ObservationView::NoDevice => {
            let text = "No battery-capable Razer device found".to_string();
            (text.clone(), text, TrayIconState::Unknown)
        }
        ObservationView::Failed { scope, kind, .. } => {
            let text = diagnostic_summary(diagnostics)
                .unwrap_or_else(|| poll_error_status(*scope, *kind).to_string());
            (text.clone(), text, TrayIconState::Unknown)
        }
    }
}

fn append_diagnostics(text: &mut String, diagnostics: &[PollError]) {
    match diagnostics {
        [] => {}
        [error] => {
            text.push_str(" - ");
            if error.scope == PollErrorScope::Subsystem {
                text.push_str(&diagnostic_status(error));
            } else {
                text.push_str(&error.display_name);
                text.push_str(": ");
                text.push_str(poll_error_status(error.scope, error.kind));
            }
        }
        _ => {
            if let Some(summary) = diagnostic_summary(diagnostics) {
                text.push_str(" - ");
                text.push_str(&summary);
            }
        }
    }
}

fn diagnostic_summary(diagnostics: &[PollError]) -> Option<String> {
    let first = diagnostics.first()?;
    if diagnostics.len() == 1 {
        return Some(diagnostic_status(first));
    }

    if let Some((scope, kind)) = diagnostic_classification(diagnostics) {
        let device_count = diagnostics
            .iter()
            .filter(|error| !error.device_key.is_empty())
            .map(|error| error.device_key.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let status = if device_count > 1 {
            multi_device_poll_error_status(scope, kind, device_count)
        } else {
            poll_error_status(scope, kind).to_string()
        };
        return Some(format!("{} ({} reports)", status, diagnostics.len()));
    }

    Some(format!(
        "Hardware state indeterminate ({} reports) - run --diagnose for details",
        diagnostics.len()
    ))
}

fn diagnostic_status(error: &PollError) -> String {
    if error.scope == PollErrorScope::Subsystem {
        format!(
            "{}: {}",
            error.display_name,
            subsystem_error_status(error.kind)
        )
    } else {
        poll_error_status(error.scope, error.kind).to_string()
    }
}

fn subsystem_error_status(kind: crate::model::PollErrorKind) -> &'static str {
    match kind {
        crate::model::PollErrorKind::AccessDenied => {
            "Access denied - check permissions and refresh"
        }
        crate::model::PollErrorKind::Protocol => "Response invalid - refresh to retry",
        crate::model::PollErrorKind::Unsupported => "Unsupported by this system",
        crate::model::PollErrorKind::PartialUnsupported => {
            "Support is indeterminate - refresh to retry"
        }
        _ => "Unavailable - refresh to retry",
    }
}

fn diagnostic_classification(
    diagnostics: &[PollError],
) -> Option<(PollErrorScope, crate::model::PollErrorKind)> {
    let first = diagnostics.first()?;
    diagnostics
        .iter()
        .all(|error| error.scope == first.scope && error.kind == first.kind)
        .then_some((first.scope, first.kind))
}

fn poll_error_status(scope: PollErrorScope, kind: crate::model::PollErrorKind) -> &'static str {
    match (scope, kind) {
        (PollErrorScope::Device, crate::model::PollErrorKind::AccessDenied) => {
            "Device access denied - check permissions and refresh"
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::AmbiguousIdentity) => {
            "Device identity is ambiguous - disconnect duplicate serialless devices"
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::DeviceUnavailable) => {
            "Device unavailable - wake or reconnect it"
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::Unsupported) => {
            "Battery reporting is unsupported by this device"
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::PartialUnsupported) => {
            "Battery support is indeterminate - refresh to retry"
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
        (PollErrorScope::ChargeState, crate::model::PollErrorKind::PartialUnsupported) => {
            "Charging status support is indeterminate - refresh to retry"
        }
        (PollErrorScope::ChargeState, crate::model::PollErrorKind::Protocol) => {
            "Charging status response invalid - refresh to retry"
        }
        (PollErrorScope::ChargeState, _) => "Charging status unavailable - refresh to retry",
        (PollErrorScope::Subsystem, crate::model::PollErrorKind::AccessDenied) => {
            "System component access denied - check permissions and refresh"
        }
        (PollErrorScope::Subsystem, crate::model::PollErrorKind::Protocol) => {
            "System component response invalid - refresh to retry"
        }
        (PollErrorScope::Subsystem, crate::model::PollErrorKind::Unsupported) => {
            "System component is unsupported"
        }
        (PollErrorScope::Subsystem, crate::model::PollErrorKind::PartialUnsupported) => {
            "System component support is indeterminate - refresh to retry"
        }
        (PollErrorScope::Subsystem, _) => "System component unavailable - refresh to retry",
    }
}

fn multi_device_poll_error_status(
    scope: PollErrorScope,
    kind: crate::model::PollErrorKind,
    device_count: usize,
) -> String {
    match (scope, kind) {
        (PollErrorScope::Device, crate::model::PollErrorKind::AccessDenied) => {
            format!("Access denied for {device_count} devices - check permissions and refresh")
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::AmbiguousIdentity) => {
            format!("Identity is ambiguous for {device_count} device groups")
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::DeviceUnavailable) => {
            format!("{device_count} devices unavailable - wake or reconnect them")
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::Unsupported) => {
            format!("Battery reporting is unsupported by {device_count} devices")
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::PartialUnsupported) => {
            format!(
                "Battery support is indeterminate for {device_count} devices - refresh to retry"
            )
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::Protocol) => {
            format!("Battery responses invalid for {device_count} devices - refresh to retry")
        }
        (PollErrorScope::Device, crate::model::PollErrorKind::Unknown) => {
            format!("Battery readings unavailable for {device_count} devices - refresh to retry")
        }
        (PollErrorScope::ChargeState, crate::model::PollErrorKind::AccessDenied) => format!(
            "Charging status access denied for {device_count} devices - check permissions and refresh"
        ),
        (PollErrorScope::ChargeState, crate::model::PollErrorKind::Unsupported) => {
            format!("Charging status is not reported by {device_count} devices")
        }
        (PollErrorScope::ChargeState, crate::model::PollErrorKind::PartialUnsupported) => format!(
            "Charging status support is indeterminate for {device_count} devices - refresh to retry"
        ),
        (PollErrorScope::ChargeState, crate::model::PollErrorKind::Protocol) => format!(
            "Charging status responses invalid for {device_count} devices - refresh to retry"
        ),
        (PollErrorScope::ChargeState, _) => {
            format!("Charging status unavailable for {device_count} devices - refresh to retry")
        }
        (PollErrorScope::Subsystem, _) => poll_error_status(scope, kind).to_string(),
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

fn next_age_refresh_at(observed_at: Instant, now: Instant) -> Option<Instant> {
    let elapsed = now.saturating_duration_since(observed_at).as_secs();
    let boundary = if elapsed < 60 {
        60
    } else if elapsed < 3_600 {
        elapsed
            .saturating_div(60)
            .saturating_add(1)
            .saturating_mul(60)
    } else {
        elapsed
            .saturating_div(3_600)
            .saturating_add(1)
            .saturating_mul(3_600)
    };
    observed_at.checked_add(Duration::from_secs(boundary))
}

#[cfg(test)]
mod tests {
    use super::{
        AppCore, AppEvent, Command, ObservationView, PollActivity, PollId, TrayIconState, Update,
    };
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
            observed_at: None,
        }
    }

    fn started_core(config: AppConfig, now: Instant) -> (AppCore, PollId, Update) {
        let (mut core, _) = AppCore::new(config, now);
        let poll_id = PollId::new(1);
        let update = core.handle(AppEvent::PollStarted(poll_id), now);
        (core, poll_id, update)
    }

    fn unsupported_error(id: &str) -> PollError {
        PollError {
            device_key: id.to_string(),
            display_name: format!("Mouse {id}"),
            pid: 1,
            scope: PollErrorScope::Device,
            component: None,
            kind: PollErrorKind::Unsupported,
            message: "battery status is not supported by this device".to_string(),
        }
    }

    #[test]
    fn review_round_22_worker_event_owns_initial_poll_id() {
        let now = Instant::now();
        let (mut core, initial) = AppCore::new(AppConfig::default(), now);
        assert_eq!(initial.view.polling, PollActivity::Idle);
        assert!(initial.view.refresh_enabled);

        let update = core.handle(AppEvent::PollStarted(PollId::new(41)), now);
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
        let (mut core, first, _) = started_core(cfg, now);
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

        let second = PollId::new(2);
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

        let third = PollId::new(3);
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
        let (mut core, first, _) = started_core(AppConfig::default(), now);
        let stale = PollId::new(2);
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
    fn review_round_17_stale_age_refreshes_between_polls() {
        let now = Instant::now();
        let (mut core, first, _) = started_core(AppConfig::default(), now);
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
        let second = PollId::new(2);
        core.handle(AppEvent::PollStarted(second), now);
        let update = core.handle(
            AppEvent::PollFinished(second, Ok(PollResult::default())),
            now + Duration::from_secs(120),
        );

        assert!(matches!(
            update.view.observation,
            ObservationView::Stale { age, .. } if age == Duration::from_secs(120)
        ));
        assert_eq!(update.view.icon, TrayIconState::Stale);
        assert_eq!(
            update.view.next_refresh_at,
            Some(now + Duration::from_secs(180))
        );

        let refreshed = core.handle(AppEvent::ProjectionTick, now + Duration::from_secs(180));
        assert!(
            refreshed
                .view
                .status_text
                .contains("last updated 3 min ago")
        );
        assert_eq!(
            refreshed.view.next_refresh_at,
            Some(now + Duration::from_secs(240))
        );
    }

    #[test]
    fn failed_config_write_can_restore_projected_state() {
        let now = Instant::now();
        let (mut core, _, _) = started_core(AppConfig::default(), now);
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
        let (mut core, _, _) = started_core(AppConfig::default(), now);
        let changed = core.handle(AppEvent::SetPollInterval(300), now);

        assert!(matches!(
            changed.commands.as_slice(),
            [Command::SaveConfig { .. }, Command::ApplyPollInterval(300)]
        ));
    }

    #[test]
    fn review_forecast_projection_counts_down_and_expires_between_polls() {
        let now = Instant::now();
        let (mut core, first, _) = started_core(AppConfig::default(), now);
        core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![reading("mouse", 100)],
                    errors: Vec::new(),
                }),
            ),
            now,
        );
        let second = PollId::new(2);
        let measured_at = now + Duration::from_secs(30 * 60);
        core.handle(AppEvent::PollStarted(second), measured_at);
        let measured = core.handle(
            AppEvent::PollFinished(
                second,
                Ok(PollResult {
                    devices: vec![reading("mouse", 50)],
                    errors: Vec::new(),
                }),
            ),
            measured_at,
        );
        assert_eq!(
            measured.view.forecast.map(|estimate| estimate.remaining),
            Some(Duration::from_secs(30 * 60))
        );

        let countdown = core.handle(
            AppEvent::ProjectionTick,
            measured_at + Duration::from_secs(10 * 60 + 1),
        );
        assert_eq!(
            countdown.view.forecast.map(|estimate| estimate.remaining),
            Some(Duration::from_secs(20 * 60 - 1))
        );
        assert!(countdown.view.status_text.contains("~19 min left"));

        let expired = core.handle(
            AppEvent::ProjectionTick,
            measured_at + Duration::from_secs(30 * 60),
        );
        assert_eq!(expired.view.forecast, None);
        assert!(!expired.view.status_text.contains("left"));
    }

    #[test]
    fn review_round_18_forecast_projects_tray_and_notification() {
        let now = Instant::now();
        let (mut core, first, _) = started_core(AppConfig::default(), now);
        let mut first_reading = reading("mouse", 100);
        first_reading.observed_at = Some(now);
        core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![first_reading],
                    errors: Vec::new(),
                }),
            ),
            now + Duration::from_secs(8),
        );

        let second = PollId::new(2);
        let observed_at = now + Duration::from_secs(30 * 60);
        core.handle(AppEvent::PollStarted(second), observed_at);
        let mut second_reading = reading("mouse", 50);
        second_reading.observed_at = Some(observed_at);
        let update = core.handle(
            AppEvent::PollFinished(
                second,
                Ok(PollResult {
                    devices: vec![second_reading],
                    errors: Vec::new(),
                }),
            ),
            observed_at + Duration::from_secs(16),
        );

        assert_eq!(
            update.view.forecast.map(|estimate| estimate.remaining),
            Some(Duration::from_secs(30 * 60 - 16))
        );
        assert!(matches!(
            update.commands.as_slice(),
            [Command::NotifyCandidate {
                estimate: Some(estimate),
                ..
            }] if estimate.remaining == Duration::from_secs(30 * 60 - 16)
        ));
    }

    #[test]
    fn review_round_18_unavailable_poll_resets_forecast_calibration() {
        let now = Instant::now();
        let (mut core, first, _) = started_core(AppConfig::default(), now);
        let mut initial = reading("mouse", 78);
        initial.battery_raw = 200;
        initial.observed_at = Some(now);
        core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![initial],
                    errors: Vec::new(),
                }),
            ),
            now,
        );

        let unavailable = PollId::new(2);
        core.handle(
            AppEvent::PollStarted(unavailable),
            now + Duration::from_secs(2 * 60 * 60),
        );
        core.handle(
            AppEvent::PollFinished(unavailable, Ok(PollResult::default())),
            now + Duration::from_secs(2 * 60 * 60),
        );

        let returned = PollId::new(3);
        let returned_at = now + Duration::from_secs(5 * 60 * 60);
        core.handle(AppEvent::PollStarted(returned), returned_at);
        let mut returned_reading = reading("mouse", 74);
        returned_reading.battery_raw = 190;
        returned_reading.observed_at = Some(returned_at);
        let reset = core.handle(
            AppEvent::PollFinished(
                returned,
                Ok(PollResult {
                    devices: vec![returned_reading],
                    errors: Vec::new(),
                }),
            ),
            returned_at,
        );
        assert_eq!(reset.view.forecast, None);

        let recalibration = PollId::new(4);
        let recalibrated_at = returned_at + Duration::from_secs(30 * 60);
        core.handle(AppEvent::PollStarted(recalibration), recalibrated_at);
        let mut recalibrated_reading = reading("mouse", 70);
        recalibrated_reading.battery_raw = 180;
        recalibrated_reading.observed_at = Some(recalibrated_at);
        let recalibrated = core.handle(
            AppEvent::PollFinished(
                recalibration,
                Ok(PollResult {
                    devices: vec![recalibrated_reading],
                    errors: Vec::new(),
                }),
            ),
            recalibrated_at,
        );
        assert_eq!(
            recalibrated
                .view
                .forecast
                .map(|estimate| estimate.remaining),
            Some(Duration::from_secs(9 * 60 * 60))
        );
    }

    #[test]
    fn review_round_18_cache_diagnostic_preserves_component_name() {
        let now = Instant::now();
        let (mut core, first, _) = started_core(AppConfig::default(), now);
        let update = core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: Vec::new(),
                    errors: vec![PollError::pid_cache(
                        "PID cache unavailable: permission denied",
                    )],
                }),
            ),
            now,
        );

        assert_eq!(
            update.view.status_text,
            "PID cache: Access denied - check permissions and refresh"
        );
        assert!(!update.view.status_text.contains("HID"));
    }

    #[test]
    fn stale_view_keeps_the_last_displayed_fallback() {
        let now = Instant::now();
        let cfg = AppConfig {
            selected_device_id: "preferred".to_string(),
            ..AppConfig::default()
        };
        let (mut core, first, _) = started_core(cfg, now);
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
        let second = PollId::new(2);
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
        let third = PollId::new(3);
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
        let (mut core, first, _) = started_core(AppConfig::default(), now);
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
        let second = PollId::new(2);
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
        let (mut core, first, _) = started_core(AppConfig::default(), now);
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
        let second = PollId::new(2);
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
        let (mut core, first, _) = started_core(AppConfig::default(), now);
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
        let second = PollId::new(2);
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
            update.view.diagnostics.first().map(|error| error.kind),
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
        let (mut core, first, _) = started_core(cfg, now);
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
        let second = PollId::new(2);
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
                .diagnostics
                .first()
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
    fn review_preferred_diagnostic_orders_without_hiding_other_errors() {
        let now = Instant::now();
        let cfg = AppConfig {
            selected_device_id: "preferred".to_string(),
            ..AppConfig::default()
        };
        let (mut core, first, _) = started_core(cfg, now);
        let update = core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: vec![reading("fallback", 60)],
                    errors: vec![
                        PollError {
                            device_key: "other".to_string(),
                            display_name: "Mouse other".to_string(),
                            pid: 3,
                            scope: PollErrorScope::Device,
                            component: None,
                            kind: PollErrorKind::Protocol,
                            message: "invalid response".to_string(),
                        },
                        PollError {
                            device_key: "fallback".to_string(),
                            display_name: "Mouse fallback".to_string(),
                            pid: 2,
                            scope: PollErrorScope::ChargeState,
                            component: None,
                            kind: PollErrorKind::AccessDenied,
                            message: "charging status access denied".to_string(),
                        },
                        unsupported_error("preferred"),
                    ],
                }),
            ),
            now,
        );

        assert_eq!(
            update
                .view
                .diagnostics
                .iter()
                .map(|error| error.device_key.as_str())
                .collect::<Vec<_>>(),
            vec!["preferred", "fallback", "other"]
        );
        assert!(update.view.status_text.contains("3 reports"));
        assert!(!update.view.status_text.contains("invalid response"));
        assert!(
            !update
                .view
                .status_text
                .contains("charging status access denied")
        );
        assert!(update.view.status_text.len() < 160);
    }

    #[test]
    fn review_homogeneous_diagnostics_keep_their_conclusive_kind() {
        let now = Instant::now();
        let (mut core, first, _) = started_core(AppConfig::default(), now);
        let update = core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: Vec::new(),
                    errors: vec![unsupported_error("a"), unsupported_error("b")],
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
        assert!(!update.view.status_text.contains("indeterminate"));
        assert!(update.view.status_text.contains("unsupported by 2 devices"));
        assert!(!update.view.status_text.contains("this device"));
        assert!(update.view.status_text.contains("2 reports"));
        assert_eq!(
            update
                .view
                .diagnostics
                .iter()
                .map(|error| error.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["Mouse a", "Mouse b"]
        );
    }

    #[test]
    fn unsupported_battery_query_has_explicit_tray_status() {
        let now = Instant::now();
        let (mut core, first, _) = started_core(AppConfig::default(), now);

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
            update.view.diagnostics.first().map(|error| error.kind),
            Some(PollErrorKind::Unsupported)
        );
    }

    #[test]
    fn review_round_22_ambiguous_identity_has_explicit_tray_status() {
        let now = Instant::now();
        let (mut core, first, _) = started_core(AppConfig::default(), now);
        let update = core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: Vec::new(),
                    errors: vec![PollError {
                        device_key: "00BF".to_string(),
                        display_name: "Razer Mouse (identity ambiguous)".to_string(),
                        pid: 0x00BF,
                        scope: PollErrorScope::Device,
                        component: None,
                        kind: PollErrorKind::AmbiguousIdentity,
                        message: "serialless interfaces cannot be assigned to one physical device"
                            .to_string(),
                    }],
                }),
            ),
            now,
        );

        assert!(matches!(
            update.view.observation,
            ObservationView::Failed {
                kind: PollErrorKind::AmbiguousIdentity,
                ..
            }
        ));
        assert_eq!(
            update.view.status_text,
            "Device identity is ambiguous - disconnect duplicate serialless devices"
        );
    }

    #[test]
    fn review_mixed_device_evidence_remains_indeterminate_and_visible() {
        let now = Instant::now();
        let (mut core, first, _) = started_core(AppConfig::default(), now);
        let mut unsupported = unsupported_error("mouse");
        unsupported.kind = PollErrorKind::PartialUnsupported;
        unsupported.message = "one or more battery probes reported unsupported status".to_string();

        let update = core.handle(
            AppEvent::PollFinished(
                first,
                Ok(PollResult {
                    devices: Vec::new(),
                    errors: vec![
                        unsupported,
                        PollError {
                            device_key: "mouse".to_string(),
                            display_name: "Mouse mouse".to_string(),
                            pid: 1,
                            scope: PollErrorScope::Device,
                            component: None,
                            kind: PollErrorKind::AccessDenied,
                            message: "interface access denied".to_string(),
                        },
                    ],
                }),
            ),
            now,
        );

        assert!(matches!(
            update.view.observation,
            ObservationView::Failed {
                kind: PollErrorKind::Unknown,
                ..
            }
        ));
        assert_eq!(update.view.diagnostics.len(), 2);
        assert!(
            update
                .view
                .status_text
                .contains("Hardware state indeterminate")
        );
        assert!(update.view.status_text.contains("2 reports"));
        assert!(!update.view.status_text.contains("unsupported status"));
        assert!(!update.view.status_text.contains("access denied"));
        assert_eq!(
            update
                .view
                .diagnostics
                .iter()
                .map(|error| error.message.as_str())
                .collect::<Vec<_>>(),
            vec![
                "one or more battery probes reported unsupported status",
                "interface access denied"
            ]
        );
    }

    #[test]
    fn diagnostic_charge_state_failure_does_not_claim_device_failure() {
        let now = Instant::now();
        let (mut core, first, _) = started_core(AppConfig::default(), now);
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
                        component: None,
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
