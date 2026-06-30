//! In-memory battery runout forecasting.
//!
//! For each device we keep a tiny amount of state and estimate the discharge
//! rate by *anchoring* at the start of the current discharge segment: the rate
//! is the average drop from that anchor to the latest reading. On a device that
//! sleeps often (so samples are sparse and irregular) this is far more stable
//! than a sliding-window fit, and the raw 0..=255 reading (instead of the
//! floored percentage) keeps quantization noise out of the estimate.
//!
//! Nothing here touches the device — it is pure post-processing of readings the
//! poll loop already collects, so it has zero effect on battery life.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Full-scale raw battery value reported by the hardware.
const RAW_FULL: f32 = 255.0;

/// Don't trust a runout estimate until the discharge segment is at least this
/// old — early on, a single raw step implies a wildly wrong rate.
const MIN_SEGMENT: Duration = Duration::from_secs(15 * 60);

/// ...and until the battery has dropped at least this many raw steps within the
/// segment (5/255 ≈ 2%), so the estimate rests on a real trend, not noise.
const MIN_RAW_DROP: u8 = 5;

/// A rise of more than this many raw steps *above the segment's lowest point*
/// ends the current discharge segment (the battery was charged or the cell
/// swapped), so we re-anchor. Comparing against the running minimum — rather than
/// the segment start — catches a recharge even when it stays below the level the
/// segment began at, and catches a gradual charge that climbs a step at a time.
/// This matters most for devices that don't report charging status (their
/// `is_charging` is always false, so the charging branch never resets us).
const RISE_RESET_RAW: u8 = 2;

/// The runout forecast for a single device at one point in time.
#[derive(Clone, Debug, PartialEq)]
pub enum Forecast {
    /// Device is charging; no runout estimate.
    Charging,
    /// Not enough discharge history yet to estimate.
    Estimating,
    /// Discharging at `rate_pct_per_h`, empty in `time_to_empty`.
    Discharging {
        rate_pct_per_h: f32,
        time_to_empty: Duration,
    },
}

impl Forecast {
    /// Short form for the tray tooltip / status line, e.g. `~25h left`.
    pub fn short(&self) -> String {
        match self {
            Forecast::Charging => "charging".to_string(),
            Forecast::Estimating => "estimating…".to_string(),
            Forecast::Discharging { time_to_empty, .. } => {
                format!("~{} left", format_duration_hm(*time_to_empty))
            }
        }
    }

    /// Detailed form for the log line, e.g. `~25h left @ 3.1%/h`.
    pub fn detailed(&self) -> String {
        match self {
            Forecast::Charging => "charging".to_string(),
            Forecast::Estimating => "estimating runtime…".to_string(),
            Forecast::Discharging {
                rate_pct_per_h,
                time_to_empty,
            } => format!(
                "~{} left @ {:.1}%/h",
                format_duration_hm(*time_to_empty),
                rate_pct_per_h
            ),
        }
    }
}

/// What a single observation produced: enough to decide whether to log and what
/// to show.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    /// The whole-percent level changed, charging state flipped, or this is the
    /// first reading for the device — i.e. worth a log line.
    pub changed: bool,
    /// Whole-percent change since the previous reading (negative = drained).
    pub delta_percent: i16,
    /// Wall time since the previous reading, if any.
    pub since_last: Option<Duration>,
    pub forecast: Forecast,
}

struct Sample {
    at: Instant,
    percent: u8,
    charging: bool,
}

#[derive(Default)]
struct Track {
    /// Start of the current discharge segment: `(time, raw)`. `None` while
    /// charging or before the first discharging reading.
    anchor: Option<(Instant, u8)>,
    /// Lowest raw value seen since the anchor; used to detect a recharge as a
    /// rise above the segment's low point. Only meaningful while `anchor` is set.
    seg_min: u8,
    last: Option<Sample>,
}

/// Holds per-device history and turns each reading into an [`Observation`].
#[derive(Default)]
pub struct Forecaster {
    tracks: HashMap<String, Track>,
}

impl Forecaster {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a reading for `device_key` and compute the current forecast.
    pub fn observe(
        &mut self,
        device_key: &str,
        raw: u8,
        percent: u8,
        charging: bool,
        now: Instant,
    ) -> Observation {
        let track = self.tracks.entry(device_key.to_string()).or_default();

        let (changed, delta_percent, since_last) = match &track.last {
            Some(prev) => (
                prev.percent != percent || prev.charging != charging,
                percent as i16 - prev.percent as i16,
                Some(now.saturating_duration_since(prev.at)),
            ),
            None => (true, 0, None),
        };

        // Maintain the discharge-segment anchor.
        if charging {
            track.anchor = None;
        } else {
            match track.anchor {
                // Open a fresh segment.
                None => {
                    track.anchor = Some((now, raw));
                    track.seg_min = raw;
                }
                // A rise above the segment's lowest point means the battery is
                // being charged (even if charging status isn't reported, and even
                // if it climbs gradually or stays below where the segment began):
                // re-anchor so the recharge isn't averaged into the rate.
                Some(_) if raw > track.seg_min.saturating_add(RISE_RESET_RAW) => {
                    track.anchor = Some((now, raw));
                    track.seg_min = raw;
                }
                Some(_) => track.seg_min = track.seg_min.min(raw),
            }
        }

        let forecast = Self::forecast_from(track.anchor, raw, charging, now);

        track.last = Some(Sample {
            at: now,
            percent,
            charging,
        });

        Observation {
            changed,
            delta_percent,
            since_last,
            forecast,
        }
    }

    fn forecast_from(
        anchor: Option<(Instant, u8)>,
        raw: u8,
        charging: bool,
        now: Instant,
    ) -> Forecast {
        if charging {
            return Forecast::Charging;
        }

        let Some((anchor_at, anchor_raw)) = anchor else {
            return Forecast::Estimating;
        };

        let elapsed = now.saturating_duration_since(anchor_at);
        let drop = anchor_raw.saturating_sub(raw);

        if elapsed < MIN_SEGMENT || drop < MIN_RAW_DROP {
            return Forecast::Estimating;
        }

        let secs = elapsed.as_secs_f32();
        if secs <= 0.0 {
            return Forecast::Estimating;
        }

        let raw_per_sec = drop as f32 / secs;
        let rate_pct_per_h = raw_per_sec / RAW_FULL * 100.0 * 3600.0;
        let time_to_empty = Duration::from_secs_f32(raw as f32 / raw_per_sec);

        Forecast::Discharging {
            rate_pct_per_h,
            time_to_empty,
        }
    }
}

/// Format a duration as a compact `Hh Mm` / `Mm` string.
pub fn format_duration_hm(d: Duration) -> String {
    let total_minutes = d.as_secs() / 60;
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    match (hours, minutes) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn first_reading_is_changed_with_no_delta() {
        let mut f = Forecaster::new();
        let t0 = Instant::now();
        let obs = f.observe("a", 255, 100, false, t0);
        assert!(obs.changed);
        assert_eq!(obs.delta_percent, 0);
        assert_eq!(obs.since_last, None);
        assert_eq!(obs.forecast, Forecast::Estimating);
    }

    #[test]
    fn unchanged_percent_is_not_changed() {
        let mut f = Forecaster::new();
        let t0 = Instant::now();
        f.observe("a", 200, 78, false, t0);
        let obs = f.observe("a", 199, 78, false, at(t0, 60));
        assert!(!obs.changed);
        assert_eq!(obs.delta_percent, 0);
        assert_eq!(obs.since_last, Some(Duration::from_secs(60)));
    }

    #[test]
    fn percent_drop_reports_negative_delta_and_changed() {
        let mut f = Forecaster::new();
        let t0 = Instant::now();
        f.observe("a", 200, 78, false, t0);
        let obs = f.observe("a", 195, 76, false, at(t0, 600));
        assert!(obs.changed);
        assert_eq!(obs.delta_percent, -2);
    }

    #[test]
    fn estimates_after_min_segment_and_drop() {
        let mut f = Forecaster::new();
        let t0 = Instant::now();
        // Anchor at 200 raw, then drop 10 raw over one hour.
        f.observe("a", 200, 78, false, t0);
        let obs = f.observe("a", 190, 74, false, at(t0, 3600));
        match obs.forecast {
            Forecast::Discharging {
                rate_pct_per_h,
                time_to_empty,
            } => {
                // 10 raw/h = 10/255*100 ≈ 3.92%/h.
                assert!(
                    (rate_pct_per_h - 3.92).abs() < 0.05,
                    "rate {rate_pct_per_h}"
                );
                // 190 raw remaining at 10 raw/h = 19h.
                let hours = time_to_empty.as_secs_f32() / 3600.0;
                assert!((hours - 19.0).abs() < 0.2, "hours {hours}");
            }
            other => panic!("expected discharging, got {other:?}"),
        }
    }

    #[test]
    fn still_estimating_before_min_segment() {
        let mut f = Forecaster::new();
        let t0 = Instant::now();
        f.observe("a", 200, 78, false, t0);
        // Big drop but only 5 minutes elapsed -> not trusted yet.
        let obs = f.observe("a", 190, 74, false, at(t0, 300));
        assert_eq!(obs.forecast, Forecast::Estimating);
    }

    #[test]
    fn still_estimating_before_min_drop() {
        let mut f = Forecaster::new();
        let t0 = Instant::now();
        f.observe("a", 200, 78, false, t0);
        // Long time but only 2 raw of drop -> below MIN_RAW_DROP.
        let obs = f.observe("a", 198, 77, false, at(t0, 3600));
        assert_eq!(obs.forecast, Forecast::Estimating);
    }

    #[test]
    fn charging_clears_anchor_and_reports_charging() {
        let mut f = Forecaster::new();
        let t0 = Instant::now();
        f.observe("a", 200, 78, false, t0);
        let obs = f.observe("a", 205, 80, true, at(t0, 600));
        assert_eq!(obs.forecast, Forecast::Charging);

        // After charging, a fresh discharge segment must re-anchor: an estimate
        // right after unplug should not reuse the pre-charge anchor.
        let obs = f.observe("a", 230, 90, false, at(t0, 1200));
        assert_eq!(obs.forecast, Forecast::Estimating);
        // ...and only a drop measured *after* re-anchor counts.
        let obs = f.observe("a", 220, 86, false, at(t0, 1200 + 3600));
        match obs.forecast {
            Forecast::Discharging { .. } => {}
            other => panic!("expected discharging after re-anchor, got {other:?}"),
        }
    }

    #[test]
    fn level_rise_without_charging_reanchors() {
        let mut f = Forecaster::new();
        let t0 = Instant::now();
        // Discharge a bit, build a valid segment.
        f.observe("a", 200, 78, false, t0);
        let obs = f.observe("a", 190, 74, false, at(t0, 3600));
        assert!(matches!(obs.forecast, Forecast::Discharging { .. }));

        // Battery jumps up (swap / late charge flag) -> segment resets.
        let obs = f.observe("a", 230, 90, false, at(t0, 3660));
        assert_eq!(obs.forecast, Forecast::Estimating);
    }

    #[test]
    fn gradual_recharge_without_charging_status_resets() {
        // Devices that don't report charging status always send is_charging=false,
        // so a recharge must be detected purely from the level rising above the
        // segment's low point — even when it climbs one raw step at a time and
        // stays below where the segment began.
        let mut f = Forecaster::new();
        let t0 = Instant::now();
        f.observe("a", 200, 78, false, t0);
        let obs = f.observe("a", 150, 58, false, at(t0, 7200));
        assert!(matches!(obs.forecast, Forecast::Discharging { .. }));

        // Gradual climb, no charging flag, never exceeding the segment start (200).
        f.observe("a", 151, 59, false, at(t0, 7260));
        f.observe("a", 152, 59, false, at(t0, 7320));
        let obs = f.observe("a", 153, 60, false, at(t0, 7380)); // 153 > seg_min(150)+2
        assert_eq!(obs.forecast, Forecast::Estimating);
    }

    #[test]
    fn tracks_are_per_device() {
        let mut f = Forecaster::new();
        let t0 = Instant::now();
        f.observe("a", 200, 78, false, t0);
        f.observe("b", 100, 39, false, t0);
        let a = f.observe("a", 190, 74, false, at(t0, 3600));
        let b = f.observe("b", 95, 37, false, at(t0, 3600));
        assert!(matches!(a.forecast, Forecast::Discharging { .. }));
        assert!(matches!(b.forecast, Forecast::Discharging { .. }));
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration_hm(Duration::from_secs(0)), "0m");
        assert_eq!(format_duration_hm(Duration::from_secs(47 * 60)), "47m");
        assert_eq!(format_duration_hm(Duration::from_secs(3600)), "1h");
        assert_eq!(
            format_duration_hm(Duration::from_secs(25 * 3600 + 10 * 60)),
            "25h 10m"
        );
    }

    #[test]
    fn forecast_strings() {
        assert_eq!(Forecast::Charging.short(), "charging");
        assert_eq!(Forecast::Estimating.short(), "estimating…");
        let d = Forecast::Discharging {
            rate_pct_per_h: 3.4,
            time_to_empty: Duration::from_secs(25 * 3600),
        };
        assert_eq!(d.short(), "~25h left");
        assert_eq!(d.detailed(), "~25h left @ 3.4%/h");
    }
}
