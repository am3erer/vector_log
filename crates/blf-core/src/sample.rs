//! Owned signal samples and decode status values.

use std::fmt;

use crate::database::BusKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecodeStatus {
    Ok,
    HeaderOnly,
    NoResponse,
    ChecksumError,
    ParityError,
    SendError,
    ReceiveError,
    UnknownFrame,
    PayloadTooShort,
    DecodeError,
}

impl DecodeStatus {
    pub fn is_error(self) -> bool {
        !matches!(self, Self::Ok)
    }
}

impl fmt::Display for DecodeStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Ok => "ok",
            Self::HeaderOnly => "header_only",
            Self::NoResponse => "no_response",
            Self::ChecksumError => "checksum_error",
            Self::ParityError => "parity_error",
            Self::SendError => "send_error",
            Self::ReceiveError => "receive_error",
            Self::UnknownFrame => "unknown_frame",
            Self::PayloadTooShort => "payload_too_short",
            Self::DecodeError => "decode_error",
        };
        formatter.write_str(value)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SampleValue {
    Text(String),
    Physical(f64),
    Raw(i128),
}

impl SampleValue {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            Self::Physical(_) | Self::Raw(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SignalSample {
    pub timestamp_ns: u64,
    pub bus: BusKind,
    pub channel: u16,
    pub frame_id: u32,
    pub message_name: String,
    pub signal_name: String,
    pub raw_value: Option<i128>,
    pub physical_value: Option<f64>,
    pub value: Option<SampleValue>,
    pub value_text: Option<String>,
    pub status: DecodeStatus,
    pub error: Option<String>,
    pub data: Vec<u8>,
}

impl SignalSample {
    #[allow(clippy::too_many_arguments)]
    pub fn ok(
        timestamp_ns: u64,
        bus: BusKind,
        channel: u16,
        frame_id: u32,
        message_name: impl Into<String>,
        signal_name: impl Into<String>,
        raw_value: i128,
        physical_value: f64,
        value: SampleValue,
        value_text: Option<String>,
        data: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            timestamp_ns,
            bus,
            channel,
            frame_id,
            message_name: message_name.into(),
            signal_name: signal_name.into(),
            raw_value: Some(raw_value),
            physical_value: Some(physical_value),
            value: Some(value),
            value_text,
            status: DecodeStatus::Ok,
            error: None,
            data: data.into(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn error(
        timestamp_ns: u64,
        bus: BusKind,
        channel: u16,
        frame_id: u32,
        message_name: impl Into<String>,
        signal_name: impl Into<String>,
        status: DecodeStatus,
        error: impl Into<String>,
        data: impl Into<Vec<u8>>,
    ) -> Self {
        debug_assert!(status.is_error());
        Self {
            timestamp_ns,
            bus,
            channel,
            frame_id,
            message_name: message_name.into(),
            signal_name: signal_name.into(),
            raw_value: None,
            physical_value: None,
            value: None,
            value_text: None,
            status,
            error: Some(error.into()),
            data: data.into(),
        }
    }

    pub fn has_value(&self) -> bool {
        self.value.is_some()
    }

    pub fn is_error(&self) -> bool {
        self.status.is_error()
    }
}
