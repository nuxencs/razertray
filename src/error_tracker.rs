use crate::model::{PollError, PollErrorKind, PollErrorScope, SubsystemComponent};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

const REPEAT_INTERVAL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct IncidentKey {
    device_key: String,
    pid: u16,
    scope: PollErrorScope,
    component: Option<SubsystemComponent>,
}

impl From<&PollError> for IncidentKey {
    fn from(error: &PollError) -> Self {
        Self {
            device_key: error.device_key.clone(),
            pid: error.pid,
            scope: error.scope,
            component: error.component,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ErrorKey {
    incident: IncidentKey,
    kind: PollErrorKind,
    message: String,
}

impl From<&PollError> for ErrorKey {
    fn from(error: &PollError) -> Self {
        Self {
            incident: IncidentKey::from(error),
            kind: error.kind,
            message: error.message.clone(),
        }
    }
}

struct ActiveError {
    error: PollError,
    last_reported: Instant,
    suppressed: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ErrorNotice {
    Started(PollError),
    Repeated {
        error: PollError,
        suppressed: u64,
    },
    Recovered {
        display_name: String,
        scope: PollErrorScope,
        component: Option<SubsystemComponent>,
    },
}

#[derive(Default)]
pub struct ErrorTracker {
    active: BTreeMap<ErrorKey, ActiveError>,
}

impl ErrorTracker {
    pub fn observe(
        &mut self,
        errors: &[PollError],
        successful_device_ids: &BTreeSet<String>,
        poll_completed: bool,
        now: Instant,
    ) -> Vec<ErrorNotice> {
        let current: BTreeSet<ErrorKey> = errors.iter().map(ErrorKey::from).collect();
        let current_incidents: BTreeSet<IncidentKey> =
            current.iter().map(|key| key.incident.clone()).collect();
        let mut notices = Vec::new();

        for error in errors {
            let key = ErrorKey::from(error);
            match self.active.get_mut(&key) {
                None => {
                    notices.push(ErrorNotice::Started(error.clone()));
                    self.active.insert(
                        key,
                        ActiveError {
                            error: error.clone(),
                            last_reported: now,
                            suppressed: 0,
                        },
                    );
                }
                Some(active) => {
                    active.error = error.clone();
                    active.suppressed = active.suppressed.saturating_add(1);
                    if now.saturating_duration_since(active.last_reported) >= REPEAT_INTERVAL {
                        notices.push(ErrorNotice::Repeated {
                            error: active.error.clone(),
                            suppressed: active.suppressed,
                        });
                        active.last_reported = now;
                        active.suppressed = 0;
                    }
                }
            }
        }

        let absent: Vec<_> = self
            .active
            .keys()
            .filter(|key| !current.contains(*key) && poll_completed)
            .cloned()
            .collect();
        let mut recovered_incidents = BTreeSet::new();
        for key in absent {
            let replacement_is_active = current_incidents.contains(&key.incident);
            let recovered = !replacement_is_active
                && if key.incident.device_key.is_empty() {
                    true
                } else {
                    successful_device_ids.contains(&key.incident.device_key)
                };
            if let Some(active) = self.active.remove(&key) {
                if recovered && recovered_incidents.insert(key.incident) {
                    notices.push(ErrorNotice::Recovered {
                        display_name: active.error.display_name,
                        scope: active.error.scope,
                        component: active.error.component,
                    });
                }
            }
        }

        notices
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorNotice, ErrorTracker};
    use crate::model::{PollError, PollErrorKind, PollErrorScope};
    use std::time::{Duration, Instant};

    fn successful() -> std::collections::BTreeSet<String> {
        ["device".to_string()].into_iter().collect()
    }

    fn error() -> PollError {
        PollError {
            device_key: "device".to_string(),
            display_name: "Mouse".to_string(),
            pid: 1,
            scope: PollErrorScope::Device,
            component: None,
            kind: PollErrorKind::DeviceUnavailable,
            message: "receiver asleep".to_string(),
        }
    }

    #[test]
    fn reports_start_suppresses_repeats_and_reports_recovery() {
        let now = Instant::now();
        let mut tracker = ErrorTracker::default();
        assert!(matches!(
            tracker
                .observe(&[error()], &successful(), true, now)
                .as_slice(),
            [ErrorNotice::Started(_)]
        ));
        assert!(
            tracker
                .observe(
                    &[error()],
                    &successful(),
                    true,
                    now + Duration::from_secs(60)
                )
                .is_empty()
        );
        assert!(matches!(
            tracker
                .observe(
                    &[error()],
                    &successful(),
                    true,
                    now + Duration::from_secs(15 * 60)
                )
                .as_slice(),
            [ErrorNotice::Repeated { suppressed: 2, .. }]
        ));
        assert!(matches!(
            tracker
                .observe(&[], &successful(), true, now + Duration::from_secs(16 * 60))
                .as_slice(),
            [ErrorNotice::Recovered { .. }]
        ));
    }

    #[test]
    fn disappearance_is_not_reported_as_recovery() {
        let now = Instant::now();
        let mut tracker = ErrorTracker::default();
        tracker.observe(&[error()], &successful(), true, now);

        assert!(
            tracker
                .observe(
                    &[],
                    &std::collections::BTreeSet::new(),
                    true,
                    now + Duration::from_secs(60)
                )
                .is_empty()
        );
    }

    #[test]
    fn diagnostic_scope_distinguishes_active_errors() {
        let now = Instant::now();
        let mut tracker = ErrorTracker::default();
        tracker.observe(&[error()], &successful(), true, now);
        let mut charge_error = error();
        charge_error.scope = PollErrorScope::ChargeState;

        let notices = tracker.observe(
            &[charge_error],
            &successful(),
            true,
            now + Duration::from_secs(60),
        );

        assert!(matches!(
            notices.as_slice(),
            [
                ErrorNotice::Started(PollError {
                    scope: PollErrorScope::ChargeState,
                    ..
                }),
                ErrorNotice::Recovered {
                    scope: PollErrorScope::Device,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn review_distinct_same_kind_evidence_is_logged_and_throttled_independently() {
        let now = Instant::now();
        let mut tracker = ErrorTracker::default();
        let mut first = error();
        first.message = "interface 0 returned no response".to_string();
        let mut second = error();
        second.message = "interface 1 returned no response".to_string();

        let notices = tracker.observe(&[first.clone(), second.clone()], &successful(), true, now);

        assert_eq!(
            notices,
            vec![
                ErrorNotice::Started(first.clone()),
                ErrorNotice::Started(second.clone())
            ]
        );
        assert!(
            tracker
                .observe(
                    &[first, second],
                    &successful(),
                    true,
                    now + Duration::from_secs(60)
                )
                .is_empty()
        );
    }

    #[test]
    fn review_round_17_recovery_is_once_per_device_scope() {
        let now = Instant::now();
        let mut tracker = ErrorTracker::default();
        let mut first = error();
        first.message = "interface 0 returned no response".to_string();
        let mut second = error();
        second.message = "interface 1 returned no response".to_string();
        tracker.observe(&[first, second], &successful(), true, now);

        let notices = tracker.observe(&[], &successful(), true, now + Duration::from_secs(60));

        assert_eq!(
            notices,
            vec![ErrorNotice::Recovered {
                display_name: "Mouse".to_string(),
                scope: PollErrorScope::Device,
                component: None,
            }]
        );
    }

    #[test]
    fn review_replaced_detail_is_retired_without_false_recovery() {
        let now = Instant::now();
        let mut tracker = ErrorTracker::default();
        let mut first = error();
        first.message = "interface 0 returned no response".to_string();
        let mut second = error();
        second.message = "interface 1 returned no response".to_string();
        let no_success = std::collections::BTreeSet::new();

        tracker.observe(std::slice::from_ref(&first), &no_success, true, now);
        assert_eq!(
            tracker.observe(
                std::slice::from_ref(&second),
                &no_success,
                true,
                now + Duration::from_secs(60)
            ),
            vec![ErrorNotice::Started(second)]
        );
        assert_eq!(
            tracker.observe(
                std::slice::from_ref(&first),
                &no_success,
                true,
                now + Duration::from_secs(120)
            ),
            vec![ErrorNotice::Started(first)]
        );
    }

    #[test]
    fn review_error_kind_transition_does_not_report_recovery() {
        let now = Instant::now();
        let mut tracker = ErrorTracker::default();
        let mut timeout = error();
        timeout.scope = PollErrorScope::ChargeState;
        timeout.message = "charging status unavailable".to_string();
        let mut protocol = timeout.clone();
        protocol.kind = PollErrorKind::Protocol;
        protocol.message = "invalid charging response crc".to_string();

        tracker.observe(std::slice::from_ref(&timeout), &successful(), true, now);
        assert_eq!(
            tracker.observe(
                std::slice::from_ref(&protocol),
                &successful(),
                true,
                now + Duration::from_secs(60)
            ),
            vec![ErrorNotice::Started(protocol)]
        );
    }

    #[test]
    fn review_round_20_subsystem_components_recover_independently() {
        let now = Instant::now();
        let mut tracker = ErrorTracker::default();
        let hid = PollError::subsystem("HID access denied");
        let cache = PollError::pid_cache("PID cache unavailable: permission denied");
        let no_devices = std::collections::BTreeSet::new();

        tracker.observe(&[hid.clone(), cache], &no_devices, true, now);
        let notices = tracker.observe(&[hid], &no_devices, true, now + Duration::from_secs(60));

        assert_eq!(
            notices,
            vec![ErrorNotice::Recovered {
                display_name: "PID cache".to_string(),
                scope: PollErrorScope::Subsystem,
                component: Some(crate::model::SubsystemComponent::PidCache),
            }]
        );
    }
}
