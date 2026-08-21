use hidapi::HidApi;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};

pub const RAZER_VID: u16 = 0x1532;

#[derive(Clone, Debug)]
pub struct InterfaceCandidate {
    pub path: CString,
    pub interface_number: i32,
    pub priority_score: u8,
}

#[derive(Clone, Debug)]
pub struct DiscoveredDevice {
    pub key: String,
    pub pid: u16,
    pub product_name: String,
    pub candidates: Vec<InterfaceCandidate>,
}

pub fn scan_devices(api: &HidApi) -> Vec<DiscoveredDevice> {
    let mut best_by_key = BTreeMap::new();

    for dev in api.device_list().filter(|d| d.vendor_id() == RAZER_VID) {
        let candidate = InterfaceCandidate {
            path: dev.path().to_owned(),
            interface_number: dev.interface_number(),
            priority_score: candidate_score(dev.interface_number(), dev.usage_page(), dev.usage()),
        };
        add_interface(
            &mut best_by_key,
            dev.product_id(),
            dev.serial_number(),
            non_empty_text(dev.product_string())
                .unwrap_or_else(|| format!("Razer Device {:04X}", dev.product_id())),
            candidate,
        );
    }

    let mut out: Vec<DiscoveredDevice> = best_by_key
        .into_values()
        .map(|mut device| {
            device.candidates.sort_by(|a, b| {
                a.priority_score
                    .cmp(&b.priority_score)
                    .then_with(|| a.interface_number.cmp(&b.interface_number))
                    .then_with(|| a.path.as_bytes().cmp(b.path.as_bytes()))
            });
            device
        })
        .collect();
    out.sort_by(|a, b| a.pid.cmp(&b.pid).then_with(|| a.key.cmp(&b.key)));
    out
}

fn add_interface(
    devices: &mut BTreeMap<String, DiscoveredDevice>,
    pid: u16,
    serial_number: Option<&str>,
    product_name: String,
    candidate: InterfaceCandidate,
) {
    let key = device_key(pid, serial_number, candidate.path.as_c_str());
    match devices.entry(key.clone()) {
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            entry.get_mut().candidates.push(candidate);
        }
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(DiscoveredDevice {
                key,
                pid,
                product_name,
                candidates: vec![candidate],
            });
        }
    }
}

fn non_empty_text(value: Option<&str>) -> Option<String> {
    value.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn device_key(pid: u16, serial_number: Option<&str>, path: &CStr) -> String {
    if let Some(serial) = non_empty_text(serial_number) {
        format!("{pid:04X}:{serial}")
    } else {
        let mut key = format!("{pid:04X}:path:");
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        for byte in path.to_bytes() {
            key.push(char::from(HEX[usize::from(byte >> 4)]));
            key.push(char::from(HEX[usize::from(byte & 0x0F)]));
        }
        key
    }
}

fn candidate_score(interface_number: i32, usage_page: u16, usage: u16) -> u8 {
    if usage_page == 0x01 && usage == 0x02 && interface_number == 0 {
        0
    } else if usage_page == 0x01 && usage == 0x02 {
        1
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::{InterfaceCandidate, add_interface, candidate_score, device_key, non_empty_text};
    use std::collections::BTreeMap;
    use std::ffi::CString;

    fn candidate(path: &str, interface_number: i32) -> InterfaceCandidate {
        InterfaceCandidate {
            path: CString::new(path).expect("path"),
            interface_number,
            priority_score: 0,
        }
    }

    #[test]
    fn interface_priority_is_ordered() {
        assert!(candidate_score(0, 0x01, 0x02) < candidate_score(1, 0x01, 0x02));
        assert!(candidate_score(1, 0x01, 0x02) < candidate_score(1, 0xFF, 0xFF));
    }

    #[test]
    fn device_key_uses_serial_when_available() {
        let first = CString::new("physical-a-interface-0").expect("path");
        let second = CString::new("physical-a-interface-1").expect("path");

        assert_eq!(
            device_key(0x00BF, Some("ABC123"), first.as_c_str()),
            "00BF:ABC123"
        );
        assert_eq!(
            device_key(0x00BF, Some("ABC123"), second.as_c_str()),
            "00BF:ABC123"
        );
    }

    #[test]
    fn review_round_20_serialless_paths_are_not_grouped_as_one_device() {
        let mut devices = BTreeMap::new();
        add_interface(
            &mut devices,
            0x00BF,
            None,
            "Mouse".to_string(),
            candidate("physical-a-interface-0", 0),
        );
        add_interface(
            &mut devices,
            0x00BF,
            None,
            "Mouse".to_string(),
            candidate("physical-b-interface-0", 0),
        );

        assert_eq!(devices.len(), 2);
        assert!(devices.values().all(|device| device.candidates.len() == 1));
    }

    #[test]
    fn serialized_interfaces_remain_grouped_for_fallback() {
        let mut devices = BTreeMap::new();
        add_interface(
            &mut devices,
            0x00BF,
            Some("ABC123"),
            "Mouse".to_string(),
            candidate("physical-a-interface-0", 0),
        );
        add_interface(
            &mut devices,
            0x00BF,
            Some("ABC123"),
            "Mouse".to_string(),
            candidate("physical-a-interface-1", 1),
        );

        assert_eq!(devices.len(), 1);
        assert_eq!(devices.values().next().expect("device").candidates.len(), 2);
    }

    #[test]
    fn non_empty_text_trims_and_filters_empty() {
        assert_eq!(
            non_empty_text(Some("DeathAdder V4 Pro")),
            Some("DeathAdder V4 Pro".into())
        );
        assert_eq!(
            non_empty_text(Some("  DeathAdder V4 Pro  ")),
            Some("DeathAdder V4 Pro".into())
        );
        assert_eq!(non_empty_text(Some("")), None);
        assert_eq!(non_empty_text(Some("   ")), None);
        assert_eq!(non_empty_text(None), None);
    }
}
