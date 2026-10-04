//! Razer HID report format, mirroring OpenRazer's `driver/razercommon.h`.

use std::fmt;

/// Size of `struct razer_report` in OpenRazer. Every request and response has it.
pub(crate) const REPORT_LENGTH: usize = 90;
/// A report on the wire: HID report ID 0, then the Razer report.
pub(crate) const FEATURE_REPORT_LENGTH: usize = REPORT_LENGTH + 1;

/// Status byte of a report (OpenRazer `RAZER_CMD_*`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum Status {
    /// Set by the host on a request.
    New,
    Busy,
    Successful,
    Failure,
    Timeout,
    NotSupported,
}

impl TryFrom<u8> for Status {
    type Error = UnknownStatusError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(Self::New),
            0x01 => Ok(Self::Busy),
            0x02 => Ok(Self::Successful),
            0x03 => Ok(Self::Failure),
            0x04 => Ok(Self::Timeout),
            0x05 => Ok(Self::NotSupported),
            other => Err(UnknownStatusError(other)),
        }
    }
}

/// A status byte that OpenRazer does not define.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UnknownStatusError(u8);

impl fmt::Display for UnknownStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown report status 0x{:02X}", self.0)
    }
}

impl std::error::Error for UnknownStatusError {}

/// A device command, from OpenRazer's `driver/razerchromacommon.c`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Command {
    class: u8,
    id: u8,
    data_size: u8,
}

impl Command {
    /// Battery level as a raw `0..=255` [`RazerReport::value`]
    /// (`razer_chroma_misc_get_battery_level`).
    pub(crate) const BATTERY_LEVEL: Self = Self {
        class: 0x07,
        id: 0x80,
        data_size: 0x02,
    };
    /// Charging state: [`RazerReport::value`] is non-zero while charging
    /// (`razer_chroma_misc_get_charging_status`).
    pub(crate) const CHARGING_STATUS: Self = Self {
        class: 0x07,
        id: 0x84,
        data_size: 0x02,
    };
}

/// One Razer report (OpenRazer `struct razer_report`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RazerReport {
    pub(crate) status: u8,
    pub(crate) transaction_id: u8,
    pub(crate) remaining_packets: u16,
    pub(crate) protocol_type: u8,
    pub(crate) data_size: u8,
    pub(crate) command_class: u8,
    pub(crate) command_id: u8,
    pub(crate) arguments: [u8; 80],
    pub(crate) crc: u8,
    pub(crate) reserved: u8,
}

impl RazerReport {
    /// Builds a request for `command`, addressed with the device's `transaction_id`.
    pub(crate) fn request(command: Command, transaction_id: u8) -> Self {
        Self {
            status: 0x00,
            transaction_id,
            remaining_packets: 0,
            protocol_type: 0,
            data_size: command.data_size,
            command_class: command.class,
            command_id: command.id,
            arguments: [0; 80],
            crc: 0,
            reserved: 0,
        }
    }

    pub(crate) fn from_bytes(data: &[u8; REPORT_LENGTH]) -> Self {
        let mut arguments = [0u8; 80];
        arguments.copy_from_slice(&data[8..88]);

        Self {
            status: data[0],
            transaction_id: data[1],
            remaining_packets: u16::from_be_bytes([data[2], data[3]]),
            protocol_type: data[4],
            data_size: data[5],
            command_class: data[6],
            command_id: data[7],
            arguments,
            crc: data[88],
            reserved: data[89],
        }
    }

    pub(crate) fn to_bytes(self) -> [u8; REPORT_LENGTH] {
        let mut out = [0u8; REPORT_LENGTH];
        out[0] = self.status;
        out[1] = self.transaction_id;
        out[2..4].copy_from_slice(&self.remaining_packets.to_be_bytes());
        out[4] = self.protocol_type;
        out[5] = self.data_size;
        out[6] = self.command_class;
        out[7] = self.command_id;
        out[8..88].copy_from_slice(&self.arguments);
        out[88] = self.crc;
        out[89] = self.reserved;
        out
    }

    /// XOR of bytes 2..88, as in OpenRazer's `razer_calculate_crc`.
    pub(crate) fn compute_crc(&self) -> u8 {
        self.to_bytes()[2..88]
            .iter()
            .fold(0, |crc, byte| crc ^ byte)
    }

    pub(crate) fn has_valid_crc(&self) -> bool {
        self.compute_crc() == self.crc
    }

    pub(crate) fn status(&self) -> Result<Status, UnknownStatusError> {
        Status::try_from(self.status)
    }

    /// True when this response belongs to `request`.
    pub(crate) fn answers(&self, request: &Self) -> bool {
        self.remaining_packets == request.remaining_packets
            && self.command_class == request.command_class
            && self.command_id == request.command_id
    }

    /// The result byte of a response. OpenRazer reads it from `arguments[1]`.
    pub(crate) fn value(&self) -> u8 {
        self.arguments[1]
    }

    /// The bytes to send: report ID 0, then the report with its CRC filled in.
    pub(crate) fn to_feature_report(mut self) -> [u8; FEATURE_REPORT_LENGTH] {
        self.crc = self.compute_crc();
        let mut payload = [0u8; FEATURE_REPORT_LENGTH];
        payload[1..].copy_from_slice(&self.to_bytes());
        payload
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, RazerReport, Status, UnknownStatusError};
    use std::collections::HashSet;

    #[test]
    fn crc_matches_known_example() {
        let request = RazerReport::request(Command::BATTERY_LEVEL, 0x3F);
        assert_eq!(request.compute_crc(), 0x85);
    }

    #[test]
    fn bytes_roundtrip() {
        let mut report = RazerReport::request(Command::BATTERY_LEVEL, 0x1F);
        report.status = 0x02;
        report.remaining_packets = 0x0102;
        report.arguments[1] = 127;
        report.crc = report.compute_crc();

        let decoded = RazerReport::from_bytes(&report.to_bytes());

        assert_eq!(decoded, report);
        assert!(decoded.has_valid_crc());
        assert_eq!(decoded.status(), Ok(Status::Successful));
        assert_eq!(decoded.value(), 127);
    }

    #[test]
    fn response_must_match_request_command() {
        let request = RazerReport::request(Command::BATTERY_LEVEL, 0x1F);
        let mut response = request;
        response.status = 0x02;
        assert!(response.answers(&request));

        let other = RazerReport::request(Command::CHARGING_STATUS, 0x1F);
        assert!(!other.answers(&request));
    }

    #[test]
    fn feature_report_has_report_id_and_valid_crc() {
        let request = RazerReport::request(Command::CHARGING_STATUS, 0x1F);
        let payload = request.to_feature_report();

        assert_eq!(payload[0], 0x00);
        let body = payload[1..].try_into().expect("report length");
        let sent = RazerReport::from_bytes(body);
        assert!(sent.has_valid_crc());
        assert_eq!(sent.command_id, 0x84);
    }

    #[test]
    fn status_codes_map_to_distinct_statuses() {
        let known: HashSet<Status> = (0x00..=0x05)
            .map(|code| Status::try_from(code).expect("OpenRazer defines 0x00..=0x05"))
            .collect();
        assert_eq!(known.len(), 6);

        for code in 0x06..=u8::MAX {
            assert_eq!(Status::try_from(code), Err(UnknownStatusError(code)));
        }
    }
}
