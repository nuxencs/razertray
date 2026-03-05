use hidapi::HidApi;
use std::ffi::CString;

pub const RAZER_VID: u16 = 0x1532;

#[derive(Clone, Debug)]
pub struct DiscoveredDevice {
    pub key: String,
    pub pid: u16,
    pub path: CString,
    pub product_name: String,
    pub interface_number: i32,
    pub usage_page: u16,
    pub usage: u16,
    pub priority_score: u8,
}

pub fn scan_devices(api: &HidApi) -> Vec<DiscoveredDevice> {
    let mut best_by_key: std::collections::BTreeMap<String, DiscoveredDevice> =
        std::collections::BTreeMap::new();

    for dev in api.device_list().filter(|d| d.vendor_id() == RAZER_VID) {
        let path = dev.path().to_owned();
        let key = dedupe_key(dev.product_id(), dev.serial_number());

        let discovered = DiscoveredDevice {
            key: key.clone(),
            pid: dev.product_id(),
            path,
            product_name: non_empty_text(dev.product_string())
                .unwrap_or_else(|| format!("Razer Device {:04X}", dev.product_id())),
            interface_number: dev.interface_number(),
            usage_page: dev.usage_page(),
            usage: dev.usage(),
            priority_score: candidate_score(dev.interface_number(), dev.usage_page(), dev.usage()),
        };

        match best_by_key.entry(key) {
            std::collections::btree_map::Entry::Occupied(mut e) => {
                if discovered.priority_score < e.get().priority_score {
                    e.insert(discovered);
                }
            }
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(discovered);
            }
        }
    }

    let mut out: Vec<DiscoveredDevice> = best_by_key.into_values().collect();
    out.sort_by(|a, b| {
        a.priority_score
            .cmp(&b.priority_score)
            .then_with(|| a.pid.cmp(&b.pid))
            .then_with(|| a.key.cmp(&b.key))
    });
    out
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

fn dedupe_key(pid: u16, serial_number: Option<&str>) -> String {
    if let Some(serial) = non_empty_text(serial_number) {
        format!("{pid:04X}:{serial}")
    } else {
        format!("{pid:04X}")
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
    use super::{candidate_score, dedupe_key, non_empty_text};

    #[test]
    fn interface_priority_is_ordered() {
        assert!(candidate_score(0, 0x01, 0x02) < candidate_score(1, 0x01, 0x02));
        assert!(candidate_score(1, 0x01, 0x02) < candidate_score(1, 0xFF, 0xFF));
    }

    #[test]
    fn dedupe_key_uses_serial_when_available() {
        assert_eq!(dedupe_key(0x00BF, Some("ABC123")), "00BF:ABC123");
        assert_eq!(dedupe_key(0x00BF, Some("   ")), "00BF");
        assert_eq!(dedupe_key(0x00BF, None), "00BF");
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
