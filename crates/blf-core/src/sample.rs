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
    DynamicFrame,
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
            Self::DynamicFrame => "dynamic_frame",
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
    pub object_type: u32,
    pub object_class: String,
    pub direction: Option<String>,
    pub raw_direction: Option<u8>,
    pub is_tx: bool,
    pub is_rx: bool,
    pub dlc: Option<u8>,
    pub payload_length: usize,
    pub header_dlc: Option<u8>,
    pub timestamp_source: Option<String>,
    pub signal_timestamp_ns: Option<u64>,
    pub databyte_timestamps: Vec<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct SampleContext {
    pub object_type: u32,
    pub object_class: String,
    pub direction: Option<String>,
    pub raw_direction: Option<u8>,
    pub dlc: Option<u8>,
    pub header_dlc: Option<u8>,
    pub payload_length: usize,
    pub timestamp_source: Option<String>,
    pub signal_timestamp_ns: Option<u64>,
    pub databyte_timestamps: Vec<u64>,
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
            object_type: 0,
            object_class: String::new(),
            direction: None,
            raw_direction: None,
            is_tx: false,
            is_rx: false,
            dlc: None,
            payload_length: 0,
            header_dlc: None,
            timestamp_source: None,
            signal_timestamp_ns: None,
            databyte_timestamps: Vec::new(),
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
            object_type: 0,
            object_class: String::new(),
            direction: None,
            raw_direction: None,
            is_tx: false,
            is_rx: false,
            dlc: None,
            payload_length: 0,
            header_dlc: None,
            timestamp_source: None,
            signal_timestamp_ns: None,
            databyte_timestamps: Vec::new(),
        }
    }

    pub fn with_context(mut self, context: SampleContext) -> Self {
        self.object_type = context.object_type;
        self.object_class = context.object_class;
        self.raw_direction = context.raw_direction;
        self.direction = context.direction;
        self.is_tx = self.raw_direction == Some(1);
        self.is_rx = self.raw_direction == Some(0);
        self.dlc = context.dlc;
        self.header_dlc = context.header_dlc;
        self.payload_length = context.payload_length;
        self.timestamp_source = context.timestamp_source;
        self.signal_timestamp_ns = context.signal_timestamp_ns;
        self.databyte_timestamps = context.databyte_timestamps;
        self
    }

    pub fn has_value(&self) -> bool {
        self.value.is_some()
    }

    pub fn is_error(&self) -> bool {
        self.status.is_error()
    }
}
