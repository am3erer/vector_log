mod can;
mod database;
mod dbc;
mod error;
mod ldf;
mod lin;
mod query;
mod reader;
mod sample;
mod types;

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
pub use ldf::{
    decode_lin_frame, decode_lin_object, decode_lin_signal, protected_identifier,
    validate_lin_checksum, validate_lin_identifier, DecodedLinSignal, LdfParseError,
    LinDecodeError,
};
pub use lin::{classify_lin_object, LinObservation, LinResponseState};
pub use query::{QueryError, SignalQuery};
pub use reader::{parse_file_statistics, File, ReaderOptions};
pub use sample::{DecodeStatus, SampleContext, SampleValue, SignalSample};
pub use types::{
    AppText, CanErrorFrame, CanErrorFrameExt, CanFdErrorFrame64, CanFdExtFrameData, CanFdMessage,
    CanFdMessage64, CanMessage, CanMessage2, FieldValue, FileStatistics, LinObject, Object,
    ObjectHeader, ObjectHeader2, ObjectHeaderBase, ObjectType, RawObject, RawObjectHeader,
    RestorePoint, RestorePointContainer, RestorePoints, StructuredObject, SystemTime,
    VarObjectHeader, FILE_SIGNATURE, OBJECT_SIGNATURE,
};

/// Return the direction encoded in the low four bits of a classic CAN BLF
/// flags field. Vector uses zero for Rx and one for Tx; other values are
/// retained so callers can report an unknown direction without losing data.
pub fn can_flags_direction(flags: u8) -> u8 {
    flags & 0x0f
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
