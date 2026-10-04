//! Domain types shared by the HID poller, the CLI and the tray.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable identity of one physical device, also stored in `config.toml`.
///
/// It is the product ID, plus the serial number when the device reports one,
/// so two mice of the same model stay apart.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub(crate) struct DeviceKey(String);

impl DeviceKey {
    pub(crate) fn new(pid: u16, serial_number: Option<&str>) -> Self {
        match non_empty(serial_number) {
            Some(serial) => Self(format!("{pid:04X}:{serial}")),
            None => Self(format!("{pid:04X}")),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Returns the trimmed text, or `None` when it is missing or blank.
pub(crate) fn non_empty(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|s| !s.is_empty())
}

/// Battery charge in percent. Always in `0..=100`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct Percent(u8);

impl Percent {
    /// Converts a raw device level (`0..=255`) the way OpenRazer does.
    ///
    /// OpenRazer computes `int((raw / 255) * 100)`, which truncates, so a raw
    /// 254 reads as 99%, not 100%.
    pub(crate) fn from_raw(raw: u8) -> Self {
        let scaled = u16::from(raw) * 100 / 255;
        Self(u8::try_from(scaled).expect("raw * 100 / 255 is at most 100"))
    }

    pub(crate) const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Percent {
    type Error = PercentRangeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value <= 100 {
            Ok(Self(value))
        } else {
            Err(PercentRangeError(value))
        }
    }
}

impl fmt::Display for Percent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}%", self.0)
    }
}

/// A percent value above 100.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PercentRangeError(u8);

impl fmt::Display for PercentRangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "percent out of range: {}", self.0)
    }
}

impl std::error::Error for PercentRangeError {}

/// One battery reading.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BatteryState {
    pub(crate) key: DeviceKey,
    pub(crate) name: String,
    pub(crate) pid: u16,
    pub(crate) percent: Percent,
    pub(crate) charging: bool,
}

/// The result of one poll over all connected Razer devices.
#[derive(Debug, Default)]
pub(crate) struct PollResult {
    /// Readings, sorted by name, then product ID, then key.
    pub(crate) devices: Vec<BatteryState>,
    pub(crate) failures: Vec<PollFailure>,
}

/// A device that was found but did not give a reading.
#[derive(Debug)]
pub(crate) struct PollFailure {
    pub(crate) name: String,
    pub(crate) pid: u16,
    pub(crate) error: anyhow::Error,
}

impl fmt::Display for PollFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({:04X}): {:#}", self.name, self.pid, self.error)
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceKey, Percent, PercentRangeError, non_empty};

    #[test]
    fn device_key_uses_serial_when_available() {
        assert_eq!(
            DeviceKey::new(0x00BF, Some("ABC123")).as_str(),
            "00BF:ABC123"
        );
        assert_eq!(
            DeviceKey::new(0x00BF, Some(" ABC123 ")).as_str(),
            "00BF:ABC123"
        );
        assert_eq!(DeviceKey::new(0x00BF, Some("   ")).as_str(), "00BF");
        assert_eq!(DeviceKey::new(0x00BF, None).as_str(), "00BF");
    }

    #[test]
    fn non_empty_trims_and_filters_blank_text() {
        assert_eq!(
            non_empty(Some("  DeathAdder V4 Pro  ")),
            Some("DeathAdder V4 Pro")
        );
        assert_eq!(non_empty(Some("   ")), None);
        assert_eq!(non_empty(None), None);
    }

    #[test]
    fn raw_level_scaling_truncates_like_openrazer() {
        // These include the boundary cases where rounding would disagree (127, 254).
        let cases = [
            (0, 0),
            (1, 0),
            (127, 49),
            (128, 50),
            (191, 74),
            (254, 99),
            (255, 100),
        ];
        for (raw, expected) in cases {
            assert_eq!(Percent::from_raw(raw).get(), expected, "raw {raw}");
        }
    }

    #[test]
    fn every_raw_level_is_a_valid_percent() {
        for raw in 0..=u8::MAX {
            let percent = Percent::from_raw(raw);
            assert_eq!(Percent::try_from(percent.get()), Ok(percent));
        }
    }

    #[test]
    fn percent_rejects_values_above_100() {
        assert_eq!(Percent::try_from(100).map(Percent::get), Ok(100));
        assert_eq!(Percent::try_from(101), Err(PercentRangeError(101)));
    }
}
