use anyhow::{Result, bail};

pub const REPORT_LENGTH: usize = 90;
pub const FEATURE_REPORT_LENGTH: usize = 91;

pub const STATUS_BUSY: u8 = 0x01;
pub const STATUS_SUCCESSFUL: u8 = 0x02;
pub const STATUS_FAILURE: u8 = 0x03;
pub const STATUS_NO_RESPONSE: u8 = 0x04;
pub const STATUS_NOT_SUPPORTED: u8 = 0x05;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RazerReport {
    pub status: u8,
    pub transaction_id: u8,
    pub remaining_packets: u16,
    pub protocol_type: u8,
    pub data_size: u8,
    pub command_class: u8,
    pub command_id: u8,
    pub arguments: [u8; 80],
    pub crc: u8,
    pub reserved: u8,
}

impl Default for RazerReport {
    fn default() -> Self {
        Self {
            status: 0,
            transaction_id: 0,
            remaining_packets: 0,
            protocol_type: 0,
            data_size: 0,
            command_class: 0,
            command_id: 0,
            arguments: [0; 80],
            crc: 0,
            reserved: 0,
        }
    }
}

impl RazerReport {
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() != REPORT_LENGTH {
            bail!("expected {REPORT_LENGTH} bytes, got {}", data.len());
        }

        let mut arguments = [0u8; 80];
        arguments.copy_from_slice(&data[8..88]);

        Ok(Self {
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
        })
    }

    pub fn to_bytes(&self) -> [u8; REPORT_LENGTH] {
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

    pub fn calculate_crc(&self) -> u8 {
        let bytes = self.to_bytes();
        bytes[2..88].iter().fold(0u8, |crc, byte| crc ^ byte)
    }

    pub fn is_valid_crc(&self) -> bool {
        self.calculate_crc() == self.crc
    }
}

pub fn build_battery_request(transaction_id: u8) -> RazerReport {
    build_request(transaction_id, 0x07, 0x80, 0x02)
}

pub fn build_charging_request(transaction_id: u8) -> RazerReport {
    build_request(transaction_id, 0x07, 0x84, 0x02)
}

pub fn build_request(
    transaction_id: u8,
    command_class: u8,
    command_id: u8,
    data_size: u8,
) -> RazerReport {
    RazerReport {
        status: 0x00,
        transaction_id,
        remaining_packets: 0x0000,
        protocol_type: 0x00,
        data_size,
        command_class,
        command_id,
        arguments: [0; 80],
        crc: 0,
        reserved: 0,
    }
}

pub fn expected_response_matches(request: &RazerReport, response: &RazerReport) -> bool {
    response.remaining_packets == request.remaining_packets
        && response.command_class == request.command_class
        && response.command_id == request.command_id
}

pub fn feature_report_payload(report: &RazerReport) -> [u8; FEATURE_REPORT_LENGTH] {
    let mut payload = [0u8; FEATURE_REPORT_LENGTH];
    let mut request = *report;
    request.crc = request.calculate_crc();
    payload[1..].copy_from_slice(&request.to_bytes());
    payload
}

#[cfg(test)]
mod tests {
    use super::{
        RazerReport, STATUS_SUCCESSFUL, build_battery_request, expected_response_matches,
        feature_report_payload,
    };

    #[test]
    fn crc_matches_known_example() {
        let request = build_battery_request(0x3F);
        assert_eq!(request.calculate_crc(), 0x85);
    }

    #[test]
    fn parse_roundtrip() {
        let mut report = build_battery_request(0x1F);
        report.status = STATUS_SUCCESSFUL;
        report.arguments[1] = 127;
        report.crc = report.calculate_crc();

        let encoded = report.to_bytes();
        let decoded = RazerReport::from_bytes(&encoded).expect("decode should work");

        assert_eq!(decoded, report);
        assert!(decoded.is_valid_crc());
    }

    #[test]
    fn response_matching() {
        let req = build_battery_request(0x1F);
        let mut rsp = build_battery_request(0x1F);
        rsp.status = STATUS_SUCCESSFUL;
        rsp.crc = rsp.calculate_crc();

        assert!(expected_response_matches(&req, &rsp));

        let mut bad = rsp;
        bad.command_id = 0x84;
        bad.crc = bad.calculate_crc();
        assert!(!expected_response_matches(&req, &bad));
    }

    #[test]
    fn feature_payload_has_report_id_prefix() {
        let req = build_battery_request(0x1F);
        let payload = feature_report_payload(&req);
        assert_eq!(payload[0], 0x00);
        assert_eq!(payload.len(), 91);
    }
}
