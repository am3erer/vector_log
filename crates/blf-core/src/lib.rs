mod can;
mod database;
mod dbc;
mod error;
mod ethernet;
mod events;
mod ldf;
mod lin;
mod pcap;
mod query;
mod reader;
mod sample;
mod transport;
mod types;
mod uds;

pub use can::{
    decode_message, decode_signal, decode_signal_for_message, message_for_id, CanDecodeError,
    DecodedCanSignal,
};
pub use database::{
    BusKind, ByteOrder, CanMessageId, CanPayloadKind, CanSignal, CanSignalRef, Database,
    DatabaseBinding, DatabaseError, DatabaseResult, DbcAttributeAssignment, DbcAttributeDefinition,
    DbcDatabase, DbcMessage, DbcNode, DbcRelationshipAttribute, DbcSignalGroup, LdfDatabase,
    LdfOptionalSection, LinChecksum, LinDiagnosticFrame, LinDiagnosticSignal, LinDynamicFrame,
    LinDynamicFrameKind, LinEncoding, LinFrame, LinNode, LinNodeAttribute, LinPhysicalRange,
    LinScheduleEntry, LinSignal, LinSignalRef, MultiplexRole, SignalType, SourceSpan, ValueTable,
};
pub use error::{BlfError, Result};
pub use ethernet::{
    dissect, dissect_frame, dissect_frame_summary, dissect_object, dissect_object_summary,
    dissect_summary, ether_type_name, frame_bytes, ip_protocol_name, EthernetDissection,
    EthernetField, EthernetHeader,
};
pub use events::{
    a429_error_type_name, a429_parity_name, afdx_link_speed_name, app_text_channel_info,
    app_text_source_name, app_text_text, app_text_trace_line, can_driver_error_name,
    can_error_code_name, can_error_direction_name, can_fd_flags_text, can_message_direction_name,
    can_message_flags_text, ethernet_rx_error_name, ethernet_sqi_name, flexray_cc_type_name,
    flexray_channel_name, flexray_direction_name, flexray_frame_flags_text,
    flexray_frame_state_text, j1708_type_name, kline_status_name, lin_checksum_model_name,
    lin_disturbance_type_name, lin_long_dom_signal_type_name, lin_sleep_flags_text,
    lin_sleep_reason_name, lin_spike_origin_name, lin_wakeup_length_name, most25_state_text,
    most50_state_text, most_ack_text, most_addressing_mode_name, most_control_sub_type_name,
    most_net_state_name, most_priority_name, most_register_id_name, most_register_sub_type_name,
    most_transfer_type_name, sysvar_type_name, sysvar_value_text, trace_line_source_name,
    wlan_frame_flags_text,
};
pub use ldf::{
    decode_lin_frame, decode_lin_object, decode_lin_signal, protected_identifier,
    validate_lin_checksum, validate_lin_identifier, DecodedLinSignal, LdfParseError,
    LinDecodeError,
};
pub use lin::{classify_lin_object, LinObservation, LinResponseState};
pub use pcap::{ExportReport, LinkType, PcapWriter};
pub use query::{QueryError, SignalQuery};
pub use reader::{parse_file_statistics, File, ReaderOptions};
pub use sample::{DecodeStatus, SampleContext, SampleValue, SignalSample};
pub use transport::{
    classify_can, classify_lin, Reassembler, Reassembly, SessionKey, Transport, TransportFrame,
    LIN_MASTER_REQUEST, LIN_SLAVE_RESPONSE,
};
pub use types::{
    ethernet_clock_mode_name, ethernet_connector_name, ethernet_duplex_name,
    ethernet_link_status_name, ethernet_mdi_name, ethernet_pairs_name, ethernet_phy_event_name,
    ethernet_phy_state_name, ethernet_phy_type_name, AppText, CanErrorFrame, CanErrorFrameExt,
    CanFdErrorFrame64, CanFdExtFrameData, CanFdMessage, CanFdMessage64, CanMessage, CanMessage2,
    FieldValue, FileStatistics, LinObject, Object, ObjectHeader, ObjectHeader2, ObjectHeaderBase,
    ObjectHeaderInfo, ObjectType, RawObject, RawObjectHeader, RestorePoint, RestorePointContainer,
    RestorePoints, StructuredObject, SystemTime, VarObjectHeader, FILE_SIGNATURE, OBJECT_SIGNATURE,
};
pub use uds::{
    decode_isotp, decode_isotp_single, decode_uds, negative_response_name, service_name,
    IsoTpFrame, NegativeResponse, UdsMessage,
};

/// Return the direction encoded in the low four bits of a classic CAN BLF
/// flags field. Vector uses zero for Rx and one for Tx; other values are
/// retained so callers can report an unknown direction without losing data.
pub fn can_flags_direction(flags: u8) -> u8 {
    flags & 0x0f
}

/// Field names that hold the protocol payload of a structured object, with the
/// field that bounds them.  The order matters: the first field present wins.
const STRUCTURED_PAYLOAD_FIELDS: &[(&str, Option<&str>)] = &[
    ("dataBytes", Some("dataCount")),
    ("payLoad", Some("payLoadLength")),
    ("pktData", Some("pktDataLength")),
    ("frameData", Some("frameLength")),
    ("msg", Some("msgLen")),
    ("firstData", Some("firstDataLen")),
    ("a429Data", None),
    ("regData", Some("regDataLen")),
    ("text", Some("textLength")),
    ("data", None),
];

/// Extract the protocol payload of a structured object.
///
/// `StructuredObject::data` holds the *whole* object, so callers that want the
/// bytes inside a frame (Ethernet, WLAN, AFDX, MOST, ...) must use the named
/// field instead.  Returns `None` when no payload field is present.
pub fn structured_payload(object: &StructuredObject) -> Option<Vec<u8>> {
    for (name, length_name) in STRUCTURED_PAYLOAD_FIELDS {
        let Some((_, value)) = object.fields.iter().find(|(field, _)| field == name) else {
            continue;
        };
        let FieldValue::Bytes(bytes) = value else {
            continue;
        };
        let mut payload = bytes.clone();
        if let Some(length_name) = length_name {
            if let Some((_, length)) = object.fields.iter().find(|(field, _)| field == length_name)
            {
                if let Some(length) = field_as_usize(length) {
                    if length <= payload.len() {
                        payload.truncate(length);
                    }
                }
            }
        }
        return Some(payload);
    }
    None
}

fn field_as_usize(value: &FieldValue) -> Option<usize> {
    let number = match value {
        FieldValue::U8(value) => u64::from(*value),
        FieldValue::U16(value) => u64::from(*value),
        FieldValue::U32(value) => u64::from(*value),
        FieldValue::U64(value) => *value,
        FieldValue::I16(value) => u64::try_from(*value).ok()?,
        FieldValue::I32(value) => u64::try_from(*value).ok()?,
        FieldValue::I64(value) => u64::try_from(*value).ok()?,
        _ => return None,
    };
    usize::try_from(number).ok()
}

/// Convert a BLF object timestamp to nanoseconds using the object flags.
pub fn object_timestamp_ns(object: &Object) -> Option<u64> {
    let (flags, timestamp) = object_timestamp_parts(object)?;

    if flags & 0x2 != 0 {
        Some(timestamp)
    } else if flags & 0x1 != 0 {
        timestamp.checked_mul(10_000)
    } else {
        None
    }
}

pub fn object_timestamp_source(object: &Object) -> Option<&'static str> {
    let (flags, _) = object_timestamp_parts(object)?;
    if flags & 0x2 != 0 {
        Some("nanoseconds")
    } else if flags & 0x1 != 0 {
        Some("10_microseconds")
    } else {
        None
    }
}

fn object_timestamp_parts(object: &Object) -> Option<(u32, u64)> {
    Some(match object {
        Object::CanMessage(value) => (value.header.object_flags, value.header.object_time_stamp),
        Object::CanMessage2(value) => (value.header.object_flags, value.header.object_time_stamp),
        Object::CanFdMessage(value) => (value.header.object_flags, value.header.object_time_stamp),
        Object::CanFdMessage64(value) => {
            (value.header.object_flags, value.header.object_time_stamp)
        }
        Object::CanErrorFrame(value) => (value.header.object_flags, value.header.object_time_stamp),
        Object::CanErrorFrameExt(value) => {
            (value.header.object_flags, value.header.object_time_stamp)
        }
        Object::CanFdErrorFrame64(value) => {
            (value.header.object_flags, value.header.object_time_stamp)
        }
        Object::AppText(value) => (value.header.object_flags, value.header.object_time_stamp),
        Object::RestorePointContainer(value) => {
            (value.header.object_flags, value.header.object_time_stamp)
        }
        Object::Lin(value) => (value.header.object_flags, value.header.object_time_stamp),
        Object::Structured(value) => raw_header_timestamp(&value.header)?,
        Object::Raw(value) => raw_header_timestamp(&value.header)?,
    })
}

fn raw_header_timestamp(header: &RawObjectHeader) -> Option<(u32, u64)> {
    match header {
        RawObjectHeader::Object(value) => Some((value.object_flags, value.object_time_stamp)),
        RawObjectHeader::Object2(value) => Some((value.object_flags, value.object_time_stamp)),
        RawObjectHeader::Var(value) => Some((value.object_flags, value.object_time_stamp)),
        RawObjectHeader::Base(_) => None,
    }
}
