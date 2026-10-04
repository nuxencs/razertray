//! The device list the tray shows, and which device the icon tracks.

use crate::model::{BatteryState, DeviceKey};

/// Empty polls in a row that still count as a short gap. The tray keeps the
/// last reading through them, because a sleeping wireless mouse often misses
/// a single poll.
const MAX_MISSED_POLLS: u32 = 3;

/// The latest readings and the selected device.
#[derive(Debug, Default)]
pub(super) struct Readings {
    devices: Vec<BatteryState>,
    selected: Option<DeviceKey>,
    missed_polls: u32,
}

/// What [`Readings::apply_poll`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PollUpdate {
    /// The poll was empty and counts as a short gap; nothing changed.
    Kept,
    /// The readings changed. `selection_changed` is true when the selected
    /// device changed too, so the config must be saved.
    Replaced { selection_changed: bool },
}

impl Readings {
    pub(super) fn new(selected: Option<DeviceKey>) -> Self {
        Self {
            selected,
            ..Self::default()
        }
    }

    pub(super) fn devices(&self) -> &[BatteryState] {
        &self.devices
    }

    pub(super) fn selected_key(&self) -> Option<&DeviceKey> {
        self.selected.as_ref()
    }

    /// The reading of the selected device, if it is connected.
    pub(super) fn selected(&self) -> Option<&BatteryState> {
        let key = self.selected.as_ref()?;
        self.devices.iter().find(|d| &d.key == key)
    }

    pub(super) fn select(&mut self, key: DeviceKey) {
        self.selected = Some(key);
    }

    /// Takes the devices of a new poll.
    ///
    /// When the selected device is gone, the first device becomes selected,
    /// or none when the list is empty.
    pub(super) fn apply_poll(&mut self, devices: Vec<BatteryState>) -> PollUpdate {
        if devices.is_empty() && !self.devices.is_empty() {
            self.missed_polls += 1;
            if self.missed_polls < MAX_MISSED_POLLS {
                return PollUpdate::Kept;
            }
        }
        self.missed_polls = 0;
        self.devices = devices;

        let still_present = self
            .selected
            .as_ref()
            .is_some_and(|key| self.devices.iter().any(|d| &d.key == key));
        if still_present {
            return PollUpdate::Replaced {
                selection_changed: false,
            };
        }

        let first = self.devices.first().map(|d| d.key.clone());
        let selection_changed = first != self.selected;
        self.selected = first;
        PollUpdate::Replaced { selection_changed }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_MISSED_POLLS, PollUpdate, Readings};
    use crate::model::{BatteryState, DeviceKey, Percent};

    fn reading(pid: u16) -> BatteryState {
        BatteryState {
            key: DeviceKey::new(pid, None),
            name: format!("Mouse {pid}"),
            pid,
            percent: Percent::from_raw(128),
            charging: false,
        }
    }

    #[test]
    fn missing_selection_falls_back_to_first_device() {
        let mut readings = Readings::new(Some(DeviceKey::new(0x0099, None)));

        let update = readings.apply_poll(vec![reading(1), reading(2)]);

        assert_eq!(
            update,
            PollUpdate::Replaced {
                selection_changed: true
            }
        );
        assert_eq!(readings.selected().map(|d| d.pid), Some(1));
    }

    #[test]
    fn present_selection_is_kept() {
        let mut readings = Readings::new(Some(DeviceKey::new(2, None)));

        let update = readings.apply_poll(vec![reading(1), reading(2)]);

        assert_eq!(
            update,
            PollUpdate::Replaced {
                selection_changed: false
            }
        );
        assert_eq!(readings.selected().map(|d| d.pid), Some(2));
    }

    #[test]
    fn short_gaps_keep_the_last_reading() {
        let mut readings = Readings::new(None);
        readings.apply_poll(vec![reading(1)]);

        for _ in 1..MAX_MISSED_POLLS {
            assert_eq!(readings.apply_poll(Vec::new()), PollUpdate::Kept);
            assert_eq!(readings.selected().map(|d| d.pid), Some(1));
        }

        let update = readings.apply_poll(Vec::new());
        assert_eq!(
            update,
            PollUpdate::Replaced {
                selection_changed: true
            }
        );
        assert!(readings.devices().is_empty());
        assert_eq!(readings.selected_key(), None);
    }

    #[test]
    fn a_reading_resets_the_gap_count() {
        let mut readings = Readings::new(None);
        readings.apply_poll(vec![reading(1)]);
        assert_eq!(readings.apply_poll(Vec::new()), PollUpdate::Kept);
        readings.apply_poll(vec![reading(1)]);

        for _ in 1..MAX_MISSED_POLLS {
            assert_eq!(readings.apply_poll(Vec::new()), PollUpdate::Kept);
        }
    }

    #[test]
    fn empty_poll_without_readings_is_not_a_gap() {
        let mut readings = Readings::new(None);
        let update = readings.apply_poll(Vec::new());
        assert_eq!(
            update,
            PollUpdate::Replaced {
                selection_changed: false
            }
        );
    }
}
