//! LIN response-state classification for database decoding.
//!
//! BLF LIN records often carry a header DLC even when no response bytes were
//! received.  This module keeps response classification separate from signal
//! extraction so a header-only or error record cannot become a zero-filled
//! valid payload.

use crate::database::LinChecksum;
use crate::ldf::validate_lin_identifier;
use crate::types::{FieldValue, LinObject, ObjectType};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinResponseState {
    Response,
    HeaderOnly,
    NoResponse,
    SendError,
    ReceiveError,
    ChecksumError,
    ParityError,
    Unsupported,
}

impl LinResponseState {
    pub fn is_error(self) -> bool {
        matches!(
            self,
            Self::SendError
                | Self::ReceiveError
                | Self::ChecksumError
                | Self::ParityError
                | Self::Unsupported
        )
    }

    pub fn has_response(self) -> bool {
        matches!(
            self,
            Self::Response | Self::ChecksumError | Self::ParityError
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinObservation {
    pub state: LinResponseState,
    pub frame_id: Option<u8>,
    /// DLC announced by the LIN header. It is not proof that response bytes
    /// exist, especially for send-error and header-only records.
    pub header_dlc: Option<u8>,
    pub payload: Option<Vec<u8>>,
    pub direction: Option<u8>,
    pub checksum: Option<u8>,
    pub checksum_model: Option<LinChecksum>,
    pub error: Option<String>,
}

impl LinObservation {
    pub fn mark_parity_error(mut self, reason: impl Into<String>) -> Self {
        self.state = LinResponseState::ParityError;
        self.error = Some(reason.into());
        self
    }

    pub fn has_safe_payload(&self) -> bool {
        self.payload
            .as_ref()
            .is_some_and(|payload| !payload.is_empty())
            && self.state.has_response()
    }
}

pub fn classify_lin_object(object: &LinObject) -> LinObservation {
    let frame_id = field_u8(object, "id");
    let header_dlc = field_u8(object, "dlc");
    let direction = field_u8(object, "dir");
    let checksum = field_u16(object, "crc").map(|value| value as u8);
    let checksum_model = field_u8(object, "checksumModel").and_then(|value| match value {
        0 => Some(LinChecksum::Classic),
        1 => Some(LinChecksum::Enhanced),
        _ => None,
    });

    let mut observation = match object.object_type {
        ObjectType::LinMessage | ObjectType::LinMessage2 => {
            let payload = bounded_payload(object, header_dlc);
            match payload {
                Some(payload) if !payload.is_empty() => LinObservation {
                    state: LinResponseState::Response,
                    frame_id,
                    header_dlc,
                    payload: Some(payload),
                    direction,
                    checksum,
                    checksum_model,
                    error: None,
                },
                Some(_) => LinObservation {
                    state: LinResponseState::HeaderOnly,
                    frame_id,
                    header_dlc,
                    payload: None,
                    direction,
                    checksum,
                    checksum_model,
                    error: Some("LIN header has no response bytes".to_string()),
                },
                None => LinObservation {
                    state: LinResponseState::NoResponse,
                    frame_id,
                    header_dlc,
                    payload: None,
                    direction,
                    checksum,
                    checksum_model,
                    error: Some("LIN response payload is unavailable".to_string()),
                },
            }
        }
        ObjectType::LinCrcError | ObjectType::LinCrcError2 => LinObservation {
            state: LinResponseState::ChecksumError,
            frame_id,
            header_dlc,
            payload: bounded_payload(object, header_dlc),
            direction,
            checksum,
            checksum_model,
            error: Some("LIN checksum validation failed".to_string()),
        },
        ObjectType::LinSndError | ObjectType::LinSndError2 => LinObservation {
            state: LinResponseState::SendError,
            frame_id,
            header_dlc,
            payload: None,
            direction: None,
            checksum,
            checksum_model,
            error: Some("LIN send error; no response payload was recorded".to_string()),
        },
        ObjectType::LinRcvError | ObjectType::LinRcvError2 => {
            let has_data = field_u8(object, "hasDatabytes").unwrap_or(0) != 0;
            let state_reason = field_u8(object, "stateReason").unwrap_or(0);
            LinObservation {
                // The BLF specification describes this as a receive/protocol
                // error. Incorrect PID parity is one possible cause, but the
                // record does not identify that cause independently.
                state: LinResponseState::ReceiveError,
                frame_id,
                header_dlc,
                payload: if has_data {
                    bounded_payload(object, header_dlc)
                } else {
                    None
                },
                direction: None,
                checksum,
                checksum_model,
                error: Some(format!(
                    "LIN receive error (stateReason=0x{state_reason:02X})"
                )),
            }
        }
        ObjectType::LinDlcInfo | ObjectType::LinSlvTimeout => LinObservation {
            state: LinResponseState::NoResponse,
            frame_id,
            header_dlc,
            payload: None,
            direction: None,
            checksum,
            checksum_model,
            error: Some("LIN record does not contain a response payload".to_string()),
        },
        _ => LinObservation {
            state: LinResponseState::Unsupported,
            frame_id,
            header_dlc,
            payload: None,
            direction: None,
            checksum,
            checksum_model,
            error: Some(format!(
                "BLF object {} is not a decodable LIN response",
                object.object_type.as_u32()
            )),
        },
    };

    // Vector BLF normally stores the normalized six-bit frame ID. Only
    // validate parity when an adapter explicitly supplies the on-wire
    // protected identifier, avoiding a false parity error from a six-bit ID.
    if let Some(protected_id) = field_u8_any(
        object,
        &["protectedId", "protectedIdentifier", "protected_id", "pid"],
    ) {
        let frame_matches = observation
            .frame_id
            .is_none_or(|frame_id| frame_id == protected_id & 0x3F);
        if !frame_matches {
            observation = observation.mark_parity_error(format!(
                "LIN protected identifier 0x{protected_id:02X} does not match frame ID"
            ));
        } else if let Err(error) = validate_lin_identifier(protected_id) {
            observation = observation.mark_parity_error(error.to_string());
        }
    }
    observation
}

fn bounded_payload(object: &LinObject, header_dlc: Option<u8>) -> Option<Vec<u8>> {
    let data = match object.fields.iter().find(|(name, _)| name == "data") {
        Some((_, FieldValue::Bytes(data))) => data,
        _ => return None,
    };
    let length = usize::from(header_dlc?).min(data.len());
    Some(data[..length].to_vec())
}

fn field_u8(object: &LinObject, name: &str) -> Option<u8> {
    match object.fields.iter().find(|(field, _)| field == name) {
        Some((_, FieldValue::U8(value))) => Some(*value),
        _ => None,
    }
}

fn field_u8_any(object: &LinObject, names: &[&str]) -> Option<u8> {
    names.iter().find_map(|name| field_u8(object, name))
}

fn field_u16(object: &LinObject, name: &str) -> Option<u16> {
    match object.fields.iter().find(|(field, _)| field == name) {
        Some((_, FieldValue::U16(value))) => Some(*value),
        Some((_, FieldValue::U8(value))) => Some(u16::from(*value)),
        _ => None,
    }
}
