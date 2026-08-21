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
    last_estimate: Option<TimedEstimate>,
}

#[derive(Clone, Copy, Debug)]
struct TimedEstimate {
    observed_at: Instant,
    remaining: Duration,
}

impl Segment {
    fn new(now: Instant, raw: u8) -> Self {
        Self {
            started_at: now,
            started_raw: raw,
            last_at: now,
            lowest_raw: raw,
            last_estimate: None,
        }
    }
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
            .or_insert_with(|| Segment::new(now, reading.battery_raw));

        let gap = now.saturating_duration_since(segment.last_at);
        if gap > MAX_SAMPLE_GAP || reading.battery_raw > segment.lowest_raw.saturating_add(2) {
            *segment = Segment::new(now, reading.battery_raw);
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
        let calculated = Duration::from_secs(remaining_secs).min(MAX_ESTIMATE);
        let remaining = if let Some(previous) = segment.last_estimate {
            let elapsed = now.saturating_duration_since(previous.observed_at);
            if elapsed >= previous.remaining {
                *segment = Segment::new(now, reading.battery_raw);
                return None;
            }
            calculated.min(previous.remaining - elapsed)
        } else {
            calculated
        };
        segment.last_estimate = Some(TimedEstimate {
            observed_at: now,
            remaining,
        });
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

    #[test]
    fn review_forecast_counts_down_across_quantized_plateaus() {
        let now = Instant::now();
        let mut forecaster = Forecaster::default();
        forecaster.observe(&reading(200, ChargeState::NotCharging), now);
        let first = forecaster
            .observe(
                &reading(190, ChargeState::NotCharging),
                now + Duration::from_secs(60 * 60),
            )
            .expect("first estimate");
        let plateau = forecaster
            .observe(
                &reading(190, ChargeState::NotCharging),
                now + Duration::from_secs(2 * 60 * 60),
            )
            .expect("plateau estimate");
        let faster_drop = forecaster
            .observe(
                &reading(180, ChargeState::NotCharging),
                now + Duration::from_secs(150 * 60),
            )
            .expect("later estimate");

        assert_eq!(first.remaining, Duration::from_secs(19 * 60 * 60));
        assert_eq!(plateau.remaining, Duration::from_secs(18 * 60 * 60));
        assert!(faster_drop.remaining <= plateau.remaining - Duration::from_secs(30 * 60));
    }

    #[test]
    fn review_forecast_gap_reset_starts_an_independent_countdown() {
        let now = Instant::now();
        let mut forecaster = Forecaster::default();
        forecaster.observe(&reading(200, ChargeState::NotCharging), now);
        let first = forecaster
            .observe(
                &reading(100, ChargeState::NotCharging),
                now + Duration::from_secs(60 * 60),
            )
            .expect("first estimate");
        assert_eq!(first.remaining, Duration::from_secs(60 * 60));

        assert_eq!(
            forecaster.observe(
                &reading(100, ChargeState::NotCharging),
                now + Duration::from_secs(8 * 60 * 60),
            ),
            None
        );
        let reset = forecaster
            .observe(
                &reading(90, ChargeState::NotCharging),
                now + Duration::from_secs(9 * 60 * 60),
            )
            .expect("reset estimate");

        assert_eq!(reset.remaining, Duration::from_secs(9 * 60 * 60));
        assert!(reset.remaining > first.remaining);
    }

    #[test]
    fn review_expired_forecast_resets_before_recalibration() {
        let now = Instant::now();
        let mut forecaster = Forecaster::default();
        forecaster.observe(&reading(200, ChargeState::NotCharging), now);
        let first_at = now + Duration::from_secs(60 * 60);
        let first = forecaster
            .observe(&reading(10, ChargeState::NotCharging), first_at)
            .expect("first estimate");
        let expired_at = first_at + first.remaining + Duration::from_secs(1);

        assert_eq!(
            forecaster.observe(&reading(10, ChargeState::NotCharging), expired_at),
            None
        );
        let recalibrated = forecaster
            .observe(
                &reading(5, ChargeState::NotCharging),
                expired_at + Duration::from_secs(30 * 60),
            )
            .expect("recalibrated estimate");

        assert!(recalibrated.remaining > Duration::ZERO);
    }
}
