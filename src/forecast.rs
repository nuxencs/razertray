use crate::model::{BatteryState, ChargeState};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const MIN_SAMPLE_SPAN: Duration = Duration::from_secs(30 * 60);
const MAX_SAMPLE_GAP: Duration = Duration::from_secs(6 * 60 * 60);
const MIN_RAW_DROP: u8 = 5;
const MAX_ESTIMATE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Estimate {
    pub remaining: Duration,
}

#[derive(Clone, Copy, Debug)]
struct Segment {
    started_at: Instant,
    started_raw: u8,
    last_at: Instant,
    lowest_raw: u8,
}

#[derive(Default)]
pub struct Forecaster {
    segments: HashMap<String, Segment>,
}

impl Forecaster {
    pub fn observe(&mut self, reading: &BatteryState, now: Instant) -> Option<Estimate> {
        if reading.charge_state != ChargeState::NotCharging {
            self.segments.remove(&reading.device_key);
            return None;
        }

        let segment = self
            .segments
            .entry(reading.device_key.clone())
            .or_insert(Segment {
                started_at: now,
                started_raw: reading.battery_raw,
                last_at: now,
                lowest_raw: reading.battery_raw,
            });

        let gap = now.saturating_duration_since(segment.last_at);
        if gap > MAX_SAMPLE_GAP || reading.battery_raw > segment.lowest_raw.saturating_add(2) {
            *segment = Segment {
                started_at: now,
                started_raw: reading.battery_raw,
                last_at: now,
                lowest_raw: reading.battery_raw,
            };
            return None;
        }

        segment.last_at = now;
        segment.lowest_raw = segment.lowest_raw.min(reading.battery_raw);
        let span = now.saturating_duration_since(segment.started_at);
        let drop = segment.started_raw.saturating_sub(reading.battery_raw);
        if span < MIN_SAMPLE_SPAN || drop < MIN_RAW_DROP || reading.battery_raw == 0 {
            return None;
        }

        let seconds_per_raw = span.as_secs_f64() / f64::from(drop);
        let remaining_secs = (seconds_per_raw * f64::from(reading.battery_raw)) as u64;
        let remaining = Duration::from_secs(remaining_secs).min(MAX_ESTIMATE);
        Some(Estimate { remaining })
    }
}

pub fn format_estimate(estimate: Estimate) -> String {
    let seconds = estimate.remaining.as_secs();
    if seconds < 60 {
        "~<1 min left".to_string()
    } else if seconds < 3_600 {
        format!("~{} min left", seconds / 60)
    } else if seconds < 48 * 3_600 {
        format!("~{} h left", seconds / 3_600)
    } else {
        format!("~{} days left", seconds / (24 * 3_600))
    }
}

#[cfg(test)]
mod tests {
    use super::{Estimate, Forecaster, format_estimate};
    use crate::model::{BatteryState, ChargeState};
    use std::time::{Duration, Instant};

    fn reading(raw: u8, charge_state: ChargeState) -> BatteryState {
        BatteryState {
            device_key: "mouse".to_string(),
            display_name: "Mouse".to_string(),
            pid: 1,
            battery_raw: raw,
            battery_percent: (u16::from(raw) * 100 / 255) as u8,
            charge_state,
        }
    }

    #[test]
    fn estimate_requires_a_meaningful_discharge_window() {
        let now = Instant::now();
        let mut forecaster = Forecaster::default();
        assert_eq!(
            forecaster.observe(&reading(200, ChargeState::NotCharging), now),
            None
        );
        assert_eq!(
            forecaster.observe(
                &reading(196, ChargeState::NotCharging),
                now + Duration::from_secs(60 * 60)
            ),
            None
        );
        assert!(
            forecaster
                .observe(
                    &reading(190, ChargeState::NotCharging),
                    now + Duration::from_secs(2 * 60 * 60),
                )
                .is_some()
        );
    }

    #[test]
    fn charging_resets_the_discharge_segment() {
        let now = Instant::now();
        let mut forecaster = Forecaster::default();
        forecaster.observe(&reading(200, ChargeState::NotCharging), now);
        assert_eq!(
            forecaster.observe(
                &reading(190, ChargeState::Charging),
                now + Duration::from_secs(60 * 60)
            ),
            None
        );
        assert_eq!(
            forecaster.observe(
                &reading(185, ChargeState::NotCharging),
                now + Duration::from_secs(2 * 60 * 60)
            ),
            None
        );
    }

    #[test]
    fn cumulative_upward_drift_resets_the_segment() {
        let now = Instant::now();
        let mut forecaster = Forecaster::default();
        forecaster.observe(&reading(200, ChargeState::NotCharging), now);
        assert!(
            forecaster
                .observe(
                    &reading(190, ChargeState::NotCharging),
                    now + Duration::from_secs(60 * 60),
                )
                .is_some()
        );
        forecaster.observe(
            &reading(192, ChargeState::NotCharging),
            now + Duration::from_secs(70 * 60),
        );

        assert_eq!(
            forecaster.observe(
                &reading(194, ChargeState::NotCharging),
                now + Duration::from_secs(80 * 60),
            ),
            None
        );
    }

    #[test]
    fn estimate_format_uses_conservative_whole_units() {
        assert_eq!(
            format_estimate(Estimate {
                remaining: Duration::from_secs(30),
            }),
            "~<1 min left"
        );
        assert_eq!(
            format_estimate(Estimate {
                remaining: Duration::from_secs(6 * 60 + 59),
            }),
            "~6 min left"
        );
        assert_eq!(
            format_estimate(Estimate {
                remaining: Duration::from_secs(47 * 3_600 + 59 * 60),
            }),
            "~47 h left"
        );
        assert_eq!(
            format_estimate(Estimate {
                remaining: Duration::from_secs(48 * 3_600 + 59 * 60),
            }),
            "~2 days left"
        );
    }
}
