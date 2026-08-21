use hidapi::HidApi;
use std::collections::BTreeMap;
use std::ffi::CString;

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
    pub interface_grouping: InterfaceGrouping,
    pub candidates: Vec<InterfaceCandidate>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterfaceGrouping {
    VerifiedDevice,
    AmbiguousSerialless,
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
    let (key, interface_grouping) = device_identity(pid, serial_number);
    match devices.entry(key.clone()) {
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            entry.get_mut().candidates.push(candidate);
        }
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(DiscoveredDevice {
                key,
                pid,
                product_name,
                interface_grouping,
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

fn device_identity(pid: u16, serial_number: Option<&str>) -> (String, InterfaceGrouping) {
    if let Some(serial) = non_empty_text(serial_number) {
        (
            format!("{pid:04X}:{serial}"),
            InterfaceGrouping::VerifiedDevice,
        )
    } else {
        (format!("{pid:04X}"), InterfaceGrouping::AmbiguousSerialless)
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
    use super::{
        InterfaceCandidate, InterfaceGrouping, add_interface, candidate_score, device_identity,
        non_empty_text,
    };
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
        assert_eq!(
            device_identity(0x00BF, Some("ABC123")),
            ("00BF:ABC123".to_string(), InterfaceGrouping::VerifiedDevice)
        );
    }

    #[test]
    fn review_round_21_serialless_interfaces_form_one_ambiguous_group() {
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

        assert_eq!(devices.len(), 1);
        let device = devices.values().next().expect("device group");
        assert_eq!(device.key, "00BF");
        assert_eq!(
            device.interface_grouping,
            InterfaceGrouping::AmbiguousSerialless
        );
        assert_eq!(device.candidates.len(), 2);
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
        let device = devices.values().next().expect("device");
        assert_eq!(device.interface_grouping, InterfaceGrouping::VerifiedDevice);
        assert_eq!(device.candidates.len(), 2);
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
