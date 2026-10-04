//! Finds connected Razer devices and picks one HID interface per device.

use crate::model::{DeviceKey, non_empty};
use hidapi::HidApi;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::ffi::CString;

/// Razer's USB vendor ID.
const RAZER_VID: u16 = 0x1532;
/// HID usage page "Generic Desktop".
const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
/// HID usage "Mouse" on the Generic Desktop page.
const USAGE_MOUSE: u16 = 0x02;

/// One Razer device, reduced to the HID interface that answers battery requests.
#[derive(Clone, Debug)]
pub(crate) struct DiscoveredDevice {
    pub(crate) key: DeviceKey,
    pub(crate) pid: u16,
    pub(crate) path: CString,
    pub(crate) product_name: String,
    /// Lower is better. See [`interface_rank`].
    rank: u8,
}

/// Lists Razer devices with one entry per [`DeviceKey`], best interface first.
pub(crate) fn scan_devices(api: &HidApi) -> Vec<DiscoveredDevice> {
    let mut best_by_key: BTreeMap<DeviceKey, DiscoveredDevice> = BTreeMap::new();

    for dev in api.device_list().filter(|d| d.vendor_id() == RAZER_VID) {
        let pid = dev.product_id();
        let discovered = DiscoveredDevice {
            key: DeviceKey::new(pid, dev.serial_number()),
            pid,
            path: dev.path().to_owned(),
            product_name: non_empty(dev.product_string())
                .map_or_else(|| format!("Razer Device {pid:04X}"), str::to_owned),
            rank: interface_rank(dev.interface_number(), dev.usage_page(), dev.usage()),
        };

        match best_by_key.entry(discovered.key.clone()) {
            Entry::Occupied(mut e) => {
                if discovered.rank < e.get().rank {
                    e.insert(discovered);
                }
            }
            Entry::Vacant(e) => {
                e.insert(discovered);
            }
        }
    }

    let mut out: Vec<DiscoveredDevice> = best_by_key.into_values().collect();
    out.sort_by(|a, b| (a.rank, a.pid, &a.key).cmp(&(b.rank, b.pid, &b.key)));
    out
}

/// Ranks a HID interface. The mouse interface 0 is where OpenRazer sends
/// mouse commands, so it comes first, then any other mouse interface.
fn interface_rank(interface_number: i32, usage_page: u16, usage: u16) -> u8 {
    let is_mouse = usage_page == USAGE_PAGE_GENERIC_DESKTOP && usage == USAGE_MOUSE;
    match (is_mouse, interface_number) {
        (true, 0) => 0,
        (true, _) => 1,
        (false, _) => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::{USAGE_MOUSE, USAGE_PAGE_GENERIC_DESKTOP, interface_rank};

    #[test]
    fn mouse_interface_zero_ranks_first() {
        let mouse0 = interface_rank(0, USAGE_PAGE_GENERIC_DESKTOP, USAGE_MOUSE);
        let mouse1 = interface_rank(1, USAGE_PAGE_GENERIC_DESKTOP, USAGE_MOUSE);
        let vendor0 = interface_rank(0, 0xFF00, 0x01);
        assert!(mouse0 < mouse1);
        assert!(mouse1 < vendor0);
    }
}
