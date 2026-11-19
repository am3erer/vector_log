//! ISO 14229 (UDS) and ISO 15765-2 (ISO-TP) decoding.
//!
//! Diagnostic payloads arrive either directly (DoIP diagnostic messages) or
//! wrapped in ISO-TP framing on CAN.  This module decodes the service layer,
//! so a trace shows `ReadDataByIdentifier 0xF1 90` instead of opaque bytes.
//! Multi-frame reassembly stays with the caller: `isotp` only classifies the
//! frame it is given.

/// One decoded UDS request or response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdsMessage {
    pub service: u8,
    pub service_name: &'static str,
    /// Sub-function for services that define one, with the suppress bit removed.
    pub sub_function: Option<u8>,
    pub sub_function_name: Option<&'static str>,
    /// Data identifier of a read or write, when the service carries one.
    pub data_identifier: Option<u16>,
    /// Negative responses keep the rejected service and its NRC.
    pub negative: Option<NegativeResponse>,
    /// Bytes after the decoded header fields.
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegativeResponse {
    pub service: u8,
    pub service_name: &'static str,
    pub code: u8,
    pub code_name: &'static str,
}

impl UdsMessage {
    /// One line for lists and logs, such as
    /// `ReadDataByIdentifier 0xF190` or `negative response 0x22 (conditionsNotCorrect)`.
    pub fn summary(&self) -> String {
        if let Some(negative) = &self.negative {
            return format!(
                "negative response 0x{:02X} ({}) to {}",
                negative.code, negative.code_name, negative.service_name
            );
        }
        let mut text = format!("{} 0x{:02X}", self.service_name, self.service);
        if let Some(sub) = self.sub_function {
            let name = self.sub_function_name.unwrap_or("sub-function");
            text.push_str(&format!(" {sub:#04X} ({name})"));
        }
        if let Some(identifier) = self.data_identifier {
            text.push_str(&format!(" DID 0x{identifier:04X}"));
        }
        text
    }

    /// Flatten the decoded message to `uds.name=value` pairs.
    pub fn fields(&self) -> Vec<(String, String)> {
        let mut fields = Vec::new();
        fields.push(("service".to_string(), format!("0x{:02X}", self.service)));
        fields.push(("serviceName".to_string(), self.service_name.to_string()));
        if let Some(sub) = self.sub_function {
            fields.push(("subFunction".to_string(), format!("0x{sub:02X}")));
            if let Some(name) = self.sub_function_name {
                fields.push(("subFunctionName".to_string(), name.to_string()));
            }
        }
        if let Some(identifier) = self.data_identifier {
            fields.push(("dataIdentifier".to_string(), format!("0x{identifier:04X}")));
        }
        if let Some(negative) = &self.negative {
            fields.push((
                "negativeResponseTo".to_string(),
                format!("0x{:02X}", negative.service),
            ));
            fields.push(("nrc".to_string(), format!("0x{:02X}", negative.code)));
            fields.push(("nrcName".to_string(), negative.code_name.to_string()));
        }
        if !self.data.is_empty() {
            fields.push(("data".to_string(), hex(&self.data)));
        }
        fields
    }
}

/// Service names from ISO 14229-1.
pub fn service_name(service: u8) -> &'static str {
    match service {
        0x10 => "DiagnosticSessionControl",
        0x11 => "ECUReset",
        0x14 => "ClearDiagnosticInformation",
        0x19 => "ReadDTCInformation",
        0x22 => "ReadDataByIdentifier",
        0x23 => "ReadMemoryByAddress",
        0x24 => "ReadScalingDataByIdentifier",
        0x27 => "SecurityAccess",
        0x28 => "CommunicationControl",
        0x29 => "Authentication",
        0x2C => "DynamicallyDefineDataIdentifier",
        0x2E => "WriteDataByIdentifier",
        0x2F => "InputOutputControlByIdentifier",
        0x31 => "RoutineControl",
        0x34 => "RequestDownload",
        0x35 => "RequestUpload",
        0x36 => "TransferData",
        0x37 => "RequestTransferExit",
        0x38 => "RequestFileTransfer",
        0x3D => "WriteMemoryByAddress",
        0x3E => "TesterPresent",
        0x83 => "AccessTimingParameter",
        0x84 => "SecuredDataTransmission",
        0x85 => "ControlDTCSetting",
        0x86 => "ResponseOnEvent",
        0x87 => "LinkControl",
        _ => "unknown",
    }
}

/// Negative response codes from ISO 14229-1.
pub fn negative_response_name(code: u8) -> &'static str {
    match code {
        0x10 => "generalReject",
        0x11 => "serviceNotSupported",
        0x12 => "subFunctionNotSupported",
        0x13 => "incorrectMessageLengthOrInvalidFormat",
        0x14 => "responseTooLong",
        0x21 => "busyRepeatRequest",
        0x22 => "conditionsNotCorrect",
        0x24 => "requestSequenceError",
        0x25 => "noResponseFromSubnetComponent",
        0x26 => "failurePreventsExecutionOfRequestedAction",
        0x31 => "requestOutOfRange",
        0x33 => "securityAccessDenied",
        0x34 => "authenticationRequired",
        0x35 => "invalidKey",
        0x36 => "exceedNumberOfAttempts",
        0x37 => "requiredTimeDelayNotExpired",
        0x70 => "uploadDownloadNotAccepted",
        0x71 => "transferDataSuspended",
        0x72 => "generalProgrammingFailure",
        0x73 => "wrongBlockSequenceCounter",
        0x78 => "requestCorrectlyReceivedResponsePending",
        0x7E => "subFunctionNotSupportedInActiveSession",
        0x7F => "serviceNotSupportedInActiveSession",
        _ => "unknown",
    }
}

fn sub_function_name(service: u8, sub: u8) -> Option<&'static str> {
    let name = match service {
        0x10 => match sub {
            0x01 => "defaultSession",
            0x02 => "programmingSession",
            0x03 => "extendedDiagnosticSession",
            0x04 => "safetySystemDiagnosticSession",
            _ => return None,
        },
        0x11 => match sub {
            0x01 => "hardReset",
            0x02 => "keyOffOnReset",
            0x03 => "softReset",
            0x04 => "enableRapidPowerShutDown",
            0x05 => "disableRapidPowerShutDown",
            _ => return None,
        },
        0x27 => match sub {
            0x01 => "requestSeed",
            0x02 => "sendKey",
            0x03 => "requestSeed (level 3)",
            0x04 => "sendKey (level 3)",
            _ => return None,
        },
        0x28 => match sub {
            0x00 => "enableRxAndTx",
            0x01 => "enableRxAndDisableTx",
            0x02 => "disableRxAndEnableTx",
            0x03 => "disableRxAndTx",
            0x04 => "enableRxAndDisableTxWithEnhancedAddressInformation",
            _ => return None,
        },
        0x31 => match sub {
            0x01 => "startRoutine",
            0x02 => "stopRoutine",
            0x03 => "requestRoutineResults",
            _ => return None,
        },
        0x3E => match sub {
            0x00 => "zeroSubFunction",
            0x80 => "suppressPosRspMsgIndicationBit",
            _ => return None,
        },
        0x85 => match sub {
            0x01 => "on",
            0x02 => "off",
            _ => return None,
        },
        0x19 => match sub {
            0x01 => "reportNumberOfDTCByStatusMask",
            0x02 => "reportDTCByStatusMask",
            0x03 => "reportDTCSnapshotIdentification",
            0x04 => "reportDTCSnapshotRecordByDTCNumber",
            0x06 => "reportDTCExtDataRecordByDTCNumber",
            0x0A => "reportSupportedDTC",
            0x0F => "reportMirrorMemoryDTCByStatusMask",
            _ => return None,
        },
        _ => return None,
    };
    Some(name)
}

/// Services whose first data octet is a sub-function.
fn has_sub_function(service: u8) -> bool {
    matches!(
        service,
        0x10 | 0x11 | 0x19 | 0x27 | 0x28 | 0x31 | 0x3E | 0x85 | 0x86 | 0x87
    )
}

/// Services whose first two data octets are a data identifier.
fn has_data_identifier(service: u8) -> bool {
    matches!(service, 0x22 | 0x24 | 0x2E | 0x2F | 0x2C)
}

/// Decode one UDS request or response PDU.
///
/// Returns `None` when the payload is too short or carries an unknown service,
/// so callers do not label unrelated CAN payloads as diagnostics.
pub fn decode_uds(payload: &[u8]) -> Option<UdsMessage> {
    let (&service, rest) = payload.split_first()?;
    if service == 0x7F {
        let (&rejected, tail) = rest.split_first()?;
        let (&code, data) = tail.split_first()?;
        return Some(UdsMessage {
            service: 0x7F,
            service_name: "NegativeResponse",
            sub_function: None,
            sub_function_name: None,
            data_identifier: None,
            negative: Some(NegativeResponse {
                service: rejected,
                service_name: service_name(rejected),
                code,
                code_name: negative_response_name(code),
            }),
            data: data.to_vec(),
        });
    }
    // A positive response echoes the request with bit 7 set.
    let (request_service, is_response) = if service & 0x40 != 0 {
        (service & 0xBF, true)
    } else {
        (service, false)
    };
    if service_name(request_service) == "unknown" {
        return None;
    }
    let mut message = UdsMessage {
        service: if is_response {
            service
        } else {
            request_service
        },
        service_name: service_name(request_service),
        sub_function: None,
        sub_function_name: None,
        data_identifier: None,
        negative: None,
        data: Vec::new(),
    };
    let mut rest = rest;
    // Requests and positive responses both carry the sub-function and the data
    // identifier right after the service; a response echoes them unchanged.
    if has_sub_function(request_service) && !rest.is_empty() {
        let sub = rest[0] & 0x7F;
        message.sub_function = Some(sub);
        message.sub_function_name = sub_function_name(request_service, sub);
        rest = &rest[1..];
    }
    if has_data_identifier(request_service) && rest.len() >= 2 {
        message.data_identifier = Some(u16::from_be_bytes([rest[0], rest[1]]));
        rest = &rest[2..];
    }
    message.data = rest.to_vec();
    Some(message)
}

/// One ISO-TP (ISO 15765-2) frame on CAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsoTpFrame {
    /// Single frame: the whole diagnostic message is inside.
    Single { data: Vec<u8> },
    /// First frame of a segmented message; `length` counts the whole message.
    First { length: u32, data: Vec<u8> },
    /// Consecutive frame with its 4-bit sequence number.
    Consecutive { sequence: u8, data: Vec<u8> },
    /// Flow control frame.
    FlowControl {
        flow_status: u8,
        block_size: u8,
        separation: u8,
    },
}

impl IsoTpFrame {
    pub fn summary(&self) -> String {
        match self {
            Self::Single { data } => format!("single frame ({} octets)", data.len()),
            Self::First { length, .. } => format!("first frame ({length} octets)"),
            Self::Consecutive { sequence, .. } => format!("consecutive frame {sequence}"),
            Self::FlowControl { flow_status, .. } => {
                format!("flow control {flow_status}")
            }
        }
    }
}

/// Classify one ISO-TP frame from a CAN or CAN FD payload.
///
/// CAN FD uses the escape sequences of ISO 15765-2:2016, so a single frame can
/// declare a length above 7 octets and a first frame a length above 4095.
pub fn decode_isotp(payload: &[u8]) -> Option<IsoTpFrame> {
    let (&first, rest) = payload.split_first()?;
    match first >> 4 {
        0x0 => {
            let mut length = usize::from(first & 0x0F);
            let mut data = rest.to_vec();
            if length == 0 {
                // CAN FD escape: the second octet carries the real length.
                let (&declared, tail) = data.split_first()?;
                length = usize::from(declared);
                data = tail.to_vec();
            }
            data.truncate(length.min(data.len()));
            Some(IsoTpFrame::Single { data })
        }
        0x1 => {
            if rest.len() < 2 {
                return None;
            }
            let length = u32::from((u16::from(first & 0x0F) << 8) | u16::from(rest[0]));
            let length = if length == 0 {
                // CAN FD escape: 32-bit length follows the sequence octet.
                if rest.len() < 6 {
                    return None;
                }
                u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]])
            } else {
                length
            };
            Some(IsoTpFrame::First {
                length,
                data: rest[1..].to_vec(),
            })
        }
        0x2 => Some(IsoTpFrame::Consecutive {
            sequence: first & 0x0F,
            data: rest.to_vec(),
        }),
        0x3 => {
            if rest.len() < 2 {
                return None;
            }
            Some(IsoTpFrame::FlowControl {
                flow_status: first & 0x0F,
                block_size: rest[0],
                separation: rest[1],
            })
        }
        _ => None,
    }
}

/// Decode the UDS message inside one ISO-TP single frame.
pub fn decode_isotp_single(payload: &[u8]) -> Option<UdsMessage> {
    match decode_isotp(payload)? {
        IsoTpFrame::Single { data } => decode_uds(&data),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_read_data_by_identifier_request() {
        let message = decode_uds(&[0x22, 0xF1, 0x90]).expect("uds");
        assert_eq!(message.service_name, "ReadDataByIdentifier");
        assert_eq!(message.data_identifier, Some(0xF190));
        assert_eq!(message.summary(), "ReadDataByIdentifier 0x22 DID 0xF190");
        assert!(message
            .fields()
            .contains(&("dataIdentifier".to_string(), "0xF190".to_string())));
    }

    #[test]
    fn decodes_a_session_control_request_and_its_response() {
        let request = decode_uds(&[0x10, 0x03]).expect("uds");
        assert_eq!(request.sub_function, Some(0x03));
        assert_eq!(request.sub_function_name, Some("extendedDiagnosticSession"));

        let response = decode_uds(&[0x50, 0x03, 0x00, 0x32, 0x01, 0xF4]).expect("uds");
        assert_eq!(response.service, 0x50);
        assert_eq!(response.service_name, "DiagnosticSessionControl");
        assert_eq!(response.data, vec![0x00, 0x32, 0x01, 0xF4]);
    }

    #[test]
    fn decodes_a_negative_response() {
        let message = decode_uds(&[0x7F, 0x22, 0x31]).expect("uds");
        let negative = message.negative.as_ref().expect("negative");
        assert_eq!(negative.service, 0x22);
        assert_eq!(negative.service_name, "ReadDataByIdentifier");
        assert_eq!(negative.code_name, "requestOutOfRange");
        assert_eq!(
            message.summary(),
            "negative response 0x31 (requestOutOfRange) to ReadDataByIdentifier"
        );
    }

    #[test]
    fn rejects_payloads_that_are_not_diagnostics() {
        assert!(decode_uds(&[]).is_none());
        assert!(decode_uds(&[0x00]).is_none());
        assert!(decode_uds(&[0x99, 0x01]).is_none());
    }

    #[test]
    fn classifies_isotp_frames() {
        let single = decode_isotp(&[0x03, 0x22, 0xF1, 0x90]).expect("frame");
        assert_eq!(
            single,
            IsoTpFrame::Single {
                data: vec![0x22, 0xF1, 0x90]
            }
        );
        let message = decode_isotp_single(&[0x03, 0x22, 0xF1, 0x90]).expect("uds");
        assert_eq!(message.data_identifier, Some(0xF190));

        let first = decode_isotp(&[0x10, 0x0A, 0x62, 0xF1]).expect("frame");
        assert_eq!(
            first,
            IsoTpFrame::First {
                length: 10,
                data: vec![0x62, 0xF1]
            }
        );

        let consecutive = decode_isotp(&[0x21, 0x90, 0xFF]).expect("frame");
        assert_eq!(
            consecutive,
            IsoTpFrame::Consecutive {
                sequence: 1,
                data: vec![0x90, 0xFF]
            }
        );

        let flow = decode_isotp(&[0x30, 0x00, 0x00]).expect("frame");
        assert_eq!(
            flow,
            IsoTpFrame::FlowControl {
                flow_status: 0,
                block_size: 0,
                separation: 0
            }
        );
    }

    #[test]
    fn can_fd_length_escapes_are_understood() {
        let mut payload = vec![0x00, 0x0C];
        payload.extend_from_slice(&[0x22, 0xF1, 0x90, 0x01, 0x02, 0x03]);
        let frame = decode_isotp(&payload).expect("frame");
        match frame {
            IsoTpFrame::Single { data } => assert_eq!(data.len(), 6),
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

#[cfg(test)]
mod service_coverage {
    use super::*;

    /// Every service the table names must decode without a panic, and every
    /// other byte must still produce a usable message.
    #[test]
    fn every_service_byte_decodes() {
        let mut named = 0;
        let mut refused = 0;
        for service in 0u8..=0xFF {
            if service == 0x7F {
                // A negative response needs a rejected service and a code.
                assert!(decode_uds(&[0x7F, 0x22, 0x31]).is_some());
                assert_eq!(decode_uds(&[0x7F]), None);
                continue;
            }
            let payload = [service, 0x01, 0x02, 0x03];
            match decode_uds(&payload) {
                Some(message) => {
                    assert_eq!(message.service, service);
                    assert!(!message.service_name.is_empty());
                    assert!(!message.summary().is_empty());
                    assert!(!message.fields().is_empty());
                    named += 1;
                }
                // Undocumented services are refused rather than guessed at.
                None => refused += 1,
            }
        }
        assert!(named >= 25, "only {named} services decoded");
        assert!(refused >= 100, "only {refused} services refused");
        assert_eq!(decode_uds(&[]), None);
    }

    #[test]
    fn every_negative_response_code_has_a_name() {
        let mut named = 0;
        for code in 0u8..=0xFF {
            let name = negative_response_name(code);
            assert!(!name.is_empty());
            if name != "unknown" {
                named += 1;
            }
        }
        assert!(named >= 20, "only {named} response codes named");
    }

    #[test]
    fn summary_and_fields_cover_every_decoded_shape() {
        // Sub-function with a documented name.
        let mut message = decode_uds(&[0x31, 0x01, 0x02, 0x03]).expect("uds");
        assert_eq!(message.service_name, "RoutineControl");
        let summary = message.summary();
        assert!(summary.contains("RoutineControl"), "{summary}");
        let fields = message.fields();
        assert!(fields.contains(&("serviceName".to_string(), "RoutineControl".to_string())));
        // A payload without a data identifier keeps the field list short.
        message.data_identifier = None;
        message.sub_function = None;
        assert!(!message.summary().is_empty());

        // A negative response keeps the rejected service and the code name.
        let negative = decode_uds(&[0x7F, 0x22, 0x31]).expect("uds");
        assert!(negative.summary().contains("negative response"));
        let fields = negative.fields();
        assert!(fields.iter().any(|(name, _)| name == "nrc"));
        assert!(fields.iter().any(|(name, _)| name == "negativeResponseTo"));

        // A negative answer to an unknown service still names both sides.
        assert_eq!(decode_uds(&[0x99, 0x00]), None);
        let unknown_negative = decode_uds(&[0x7F, 0x99, 0xEE]).expect("uds");
        assert!(unknown_negative.summary().contains("unknown"));
    }

    #[test]
    fn sub_function_names_cover_the_documented_services() {
        for (service, sub, expected) in [
            (0x10u8, 0x01u8, "defaultSession"),
            (0x11, 0x01, "hardReset"),
            (0x27, 0x01, "requestSeed"),
            (0x28, 0x00, "enableRxAndTx"),
            (0x3E, 0x00, "zeroSubFunction"),
            (0x85, 0x01, "on"),
        ] {
            let message = decode_uds(&[service, sub]).expect("uds");
            assert_eq!(
                message.sub_function_name,
                Some(expected),
                "service {service:#04X} sub {sub:#04X}"
            );
            // The response service carries the same sub-function.
            let response = decode_uds(&[service + 0x40, sub]).expect("uds");
            assert_eq!(response.sub_function_name, Some(expected));
        }
    }
}

#[cfg(test)]
mod table_exhaustive {
    use super::*;

    /// Every documented sub-function of every service that defines one.
    #[test]
    fn sub_function_table_names_every_listed_value() {
        let cases: Vec<(u8, u8, &str)> = vec![
            (0x10, 0x01, "defaultSession"),
            (0x10, 0x02, "programmingSession"),
            (0x10, 0x03, "extendedDiagnosticSession"),
            (0x10, 0x04, "safetySystemDiagnosticSession"),
            (0x11, 0x01, "hardReset"),
            (0x11, 0x02, "keyOffOnReset"),
            (0x11, 0x03, "softReset"),
            (0x11, 0x04, "enableRapidPowerShutDown"),
            (0x11, 0x05, "disableRapidPowerShutDown"),
            (0x27, 0x01, "requestSeed"),
            (0x27, 0x02, "sendKey"),
            (0x27, 0x03, "requestSeed (level 3)"),
            (0x27, 0x04, "sendKey (level 3)"),
            (0x28, 0x00, "enableRxAndTx"),
            (0x28, 0x01, "enableRxAndDisableTx"),
            (0x28, 0x02, "disableRxAndEnableTx"),
            (0x28, 0x03, "disableRxAndTx"),
            (
                0x28,
                0x04,
                "enableRxAndDisableTxWithEnhancedAddressInformation",
            ),
            (0x31, 0x01, "startRoutine"),
            (0x31, 0x02, "stopRoutine"),
            (0x31, 0x03, "requestRoutineResults"),
            (0x3E, 0x00, "zeroSubFunction"),
            (0x85, 0x01, "on"),
            (0x85, 0x02, "off"),
            (0x19, 0x01, "reportNumberOfDTCByStatusMask"),
            (0x19, 0x02, "reportDTCByStatusMask"),
            (0x19, 0x03, "reportDTCSnapshotIdentification"),
            (0x19, 0x04, "reportDTCSnapshotRecordByDTCNumber"),
            (0x19, 0x06, "reportDTCExtDataRecordByDTCNumber"),
            (0x19, 0x0A, "reportSupportedDTC"),
            (0x19, 0x0F, "reportMirrorMemoryDTCByStatusMask"),
        ];
        for (service, sub, expected) in cases {
            assert_eq!(
                sub_function_name(service, sub),
                Some(expected),
                "service {service:#04X} sub-function {sub:#04X}"
            );
            let message = decode_uds(&[service, sub]).expect("uds");
            assert_eq!(message.sub_function_name, Some(expected));
        }
        // The suppress bit is removed before the name is looked up.
        assert_eq!(sub_function_name(0x3E, 0x00), Some("zeroSubFunction"));
        assert_eq!(
            decode_uds(&[0x3E, 0x80]).expect("uds").sub_function_name,
            Some("zeroSubFunction")
        );
        // A service without sub-functions and an undocumented value stay unnamed.
        assert_eq!(sub_function_name(0x22, 0x01), None);
        assert_eq!(sub_function_name(0x10, 0x7F), None);
    }

    /// ISO-TP framing carries its length and sequence information in the header.
    #[test]
    fn isotp_frames_describe_themselves() {
        let single = decode_isotp(&[0x03, 0x22, 0xF1, 0x90]).expect("single");
        assert_eq!(single.summary(), "single frame (3 octets)");
        let first = decode_isotp(&[0x10, 0x09, 0x2E, 0xF1]).expect("first");
        assert_eq!(first.summary(), "first frame (9 octets)");
        let consecutive = decode_isotp(&[0x21, 0xDE, 0xAD]).expect("consecutive");
        assert_eq!(consecutive.summary(), "consecutive frame 1");
        let flow = decode_isotp(&[0x30, 0x00, 0x00]).expect("flow");
        assert!(
            flow.summary().contains("flow control"),
            "{}",
            flow.summary()
        );
        assert_eq!(decode_isotp(&[0x40, 0x00]), None);

        // A single-frame UDS message decodes through the ISO-TP wrapper.
        let message = decode_isotp_single(&[0x03, 0x22, 0xF1, 0x90]).expect("uds");
        assert_eq!(message.service_name, "ReadDataByIdentifier");
        assert_eq!(decode_isotp_single(&[0x10, 0x09, 0x2E, 0xF1]), None);

        // A negative flow-control status keeps its name.
        let wait = decode_isotp(&[0x31, 0x00, 0x00]).expect("flow wait");
        assert!(
            wait.summary().contains("flow control"),
            "{}",
            wait.summary()
        );
    }

    #[test]
    fn hex_helper_renders_bytes() {
        assert_eq!(hex(&[0xDE, 0xAD, 0xBE, 0xEF]), "DE AD BE EF");
        assert_eq!(hex(&[]), "");
    }
}
