//! Names for the values of CAN, LIN and Ethernet event records.
//!
//! Frame records carry the payload the user cares about; the remaining records
//! describe why a bus misbehaved or how it is doing. Their numbers only make
//! sense with the tables from the logging format documents, so they live here,
//! next to each other, and every table below is quoted from those documents.

/// Reason reported by `VBLEthernetRxError::mError`.
pub fn ethernet_rx_error_name(value: u32) -> &'static str {
    match value {
        1 => "data length error",
        2 => "invalid CRC",
        3 => "invalid data",
        4 => "collision detected",
        _ => "unknown error",
    }
}

/// Signal quality of a 100BASE-T1 link, from `VBLEthernetStatistic::mSQI`.
pub fn ethernet_sqi_name(value: i16) -> &'static str {
    match value {
        0 => "error occurring",
        1 => "no margin",
        2 => "marginal",
        3 => "acceptable",
        4 => "good",
        5 => "excellent",
        6 => "not available",
        _ => "unknown",
    }
}

/// Driver error code of `VBLCANDriverError`, from the logging format document.
pub fn can_driver_error_name(value: u32) -> &'static str {
    match value {
        0 => "timeout during board initialization",
        1 => "no event available in the receive queue",
        2 => "transmit queue full, request refused",
        3 => "unknown controller number",
        4 => "timeout during command",
        5 => "DPRAM overflow",
        6 => "event not allowed in the command queue",
        8 => "driver detected another hardware",
        9 => "parameter error in the measurement setup",
        10 => "parameter error in the measurement setup and command",
        11 => "function not implemented in this driver version",
        12 => "no access to the object memory",
        14 => "last message was not transferred",
        100 => "unknown send identifier",
        101 => "receive queue overrun",
        102 => "chip state bus off",
        103 => "chip state error passive",
        104 => "chip state error active",
        105 => "receive register overrun",
        106 => "firmware could not access the controller at boot",
        107 => "no valid DPRAM address",
        108 => "no interrupt from the CAN interface board",
        109 => "wrong module number",
        110 => "wrong pointer to the source buffer",
        111 => "address beyond the board memory",
        112 => "address plus size beyond the board memory",
        113 => "invalid controller number",
        114 => "FIFO entry larger than 16 bytes",
        115 => "driver specific error",
        217 => "enabling the CPU failed",
        218 => "setting the time stamp status bit failed",
        219 => "halt command failed",
        220 => "halt command failed",
        221 => "reset command failed",
        222 => "reset command failed",
        232 => "timeout waiting for FIFO data",
        248 => "unknown command",
        249 => "unknown function",
        250 => "wrong parameter format",
        251 => "wrong parameter",
        252 => "unexpected acknowledgement while waiting for data",
        455 => "unknown answer from the CAN interface board",
        _ => "unlisted driver error",
    }
}

/// Error type in the Vector CAN core error code, bits 0-5 of an error frame.
pub fn can_error_code_name(value: u8) -> &'static str {
    match value {
        0 => "bit error",
        1 => "form error",
        2 => "stuff error",
        3 => "other error",
        4 => "CRC error",
        5 => "acknowledge delimiter error",
        7 => "acknowledge error",
        _ => "unknown error",
    }
}

/// Direction in the Vector CAN core error code, bits 6-7 of an error frame.
pub fn can_error_direction_name(value: u8) -> &'static str {
    match value {
        0 => "RX NAK",
        1 => "TX NAK",
        2 => "RX",
        3 => "TX",
        _ => "unknown direction",
    }
}

/// Reason of `VBLLINSleepModeEvent::mReason`.
pub fn lin_sleep_reason_name(value: u8) -> &'static str {
    match value {
        0 => "start state, transition to sleep",
        1 => "go-to-sleep frame",
        2 => "bus idle timeout",
        3 => "silent sleep command",
        9 => "external wakeup signal",
        10 => "internal wakeup signal",
        11 => "bus traffic",
        18 => "bus traffic, sleep request ignored",
        _ => "unknown reason",
    }
}

/// State bits of `VBLLINSleepModeEvent::mFlags`.
pub fn lin_sleep_flags_text(value: u8) -> String {
    let before = if value & 0b001 != 0 {
        "awake"
    } else {
        "asleep"
    };
    let now = if value & 0b010 != 0 {
        "awake"
    } else {
        "asleep"
    };
    let cause = if value & 0b100 != 0 {
        "external"
    } else {
        "internal"
    };
    format!("{before} -> {now}, {cause} event")
}

/// Wakeup pulse validity, from `VBLLINWakeupEvent2::mLengthInfo`.
pub fn lin_wakeup_length_name(value: u8) -> &'static str {
    match value {
        0 => "wakeup length ok",
        1 => "wakeup too short",
        2 => "wakeup too long",
        _ => "unknown wakeup length",
    }
}

/// Checksum model, from `VBLLINChecksumInfo::mChecksumModel`.
pub fn lin_checksum_model_name(value: u8) -> &'static str {
    match value {
        0 => "classic",
        1 => "enhanced",
        0xFF => "unknown",
        _ => "reserved",
    }
}

/// Type of `VBLLINDisturbanceEvent::mDisturbanceType`.
pub fn lin_disturbance_type_name(value: u32) -> &'static str {
    match value {
        0 => "dominant disturbance",
        1 => "recessive disturbance",
        2 => "disturbance with a header",
        3 => "disturbance with a bitstream",
        4 => "disturbance with a variable bitstream",
        _ => "unknown disturbance",
    }
}

/// Position of a dominant signal in `VBLLINLongDomSignalEvent2::mType`.
pub fn lin_long_dom_signal_type_name(value: u8) -> &'static str {
    match value {
        0 => "detected",
        1 => "continues",
        2 => "finished",
        _ => "unknown phase",
    }
}

/// Whether a spike event was recorded or simulated, `VBLLINSpikeEvent2::mInternal`.
pub fn lin_spike_origin_name(value: u8) -> &'static str {
    match value {
        0 => "real",
        1 => "simulated",
        _ => "unknown origin",
    }
}

/// Kind of an application-text record, from `VBLAppText::mSource`.
pub fn app_text_source_name(source: u32) -> &'static str {
    match source {
        0 => "measurement comment",
        1 => "channel info",
        2 => "metadata",
        3 => "attachment",
        4 => "trace line",
        _ => "unknown source",
    }
}

/// Type of a system variable payload, from `VBLSystemVariable::mType`.
pub fn sysvar_type_name(kind: u32) -> &'static str {
    match kind {
        1 => "double",
        2 => "long",
        3 => "string",
        4 => "double array",
        5 => "long array",
        6 => "long long",
        7 => "byte array",
        _ => "unknown type",
    }
}

/// Where a trace-line text came from, `APPTEXT_TRACELINE_SOURCE`.
pub fn trace_line_source_name(source: u32) -> &'static str {
    match source {
        0x0 => "write to log",
        0x1 => "timer",
        0x2 => "write to X",
        0x3 => "node layer",
        0x4 => "CAPL on-board",
        _ => "unknown source",
    }
}

/// Direction of a CAN frame, `CAN_MSG_DIR`.
pub fn can_message_direction_name(flags: u8) -> &'static str {
    match flags & 0x0F {
        0 => "Rx",
        1 => "Tx",
        2 => "TxRq",
        _ => "reserved",
    }
}

/// Frame kinds a CAN record can carry beyond a plain data frame, from the
/// `CAN_MSG_RTR`, `CAN_MSG_WU` and `CAN_MSG_NERR` bits.
pub fn can_message_flags_text(flags: u8) -> String {
    let mut parts = Vec::new();
    if flags & 0x80 != 0 {
        parts.push("remote");
    }
    if flags & 0x40 != 0 {
        parts.push("wakeup");
    }
    if flags & 0x20 != 0 {
        parts.push("no error");
    }
    parts.join(", ")
}

/// CAN FD frame properties, from `CAN_FD_MSG_EDL`, `CAN_FD_MSG_BRS` and
/// `CAN_FD_MSG_ESI`.
pub fn can_fd_flags_text(flags: u32) -> String {
    let mut parts = Vec::new();
    if flags & 0x01 != 0 {
        parts.push("FD");
    }
    if flags & 0x02 != 0 {
        parts.push("BRS");
    }
    if flags & 0x04 != 0 {
        parts.push("ESI");
    }
    parts.join(" ")
}

/// Whether a `VBLSystemVariable` carries a signed value.
///
/// `mRepresentation` is 0 for signed and 1 for unsigned, and is only defined
/// for the integer types; every other type is unsigned by definition.
pub fn sysvar_is_signed(representation: u32) -> bool {
    representation == 0
}

/// Render the payload of a `VBLSystemVariable` according to its declared type.
///
/// The data block is a bare C value: a `double`, a 32-bit or 64-bit integer, a
/// NUL-terminated string, or a run of those for the array types. Anything the
/// type table does not name falls back to hex so the bytes stay visible.
///
/// `representation` follows the format document: 0 means the integer value is
/// signed, 1 means unsigned. Getting this wrong turns `-5` into `4294967291`.
pub fn sysvar_value_text(kind: u32, representation: u32, data: &[u8]) -> String {
    let signed = |bytes: &[u8]| -> Option<i64> {
        match bytes.len() {
            4 => Some(i64::from(i32::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3],
            ]))),
            8 => Some(i64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ])),
            _ => None,
        }
    };
    let unsigned = |bytes: &[u8]| -> Option<u64> {
        match bytes.len() {
            4 => Some(u64::from(u32::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3],
            ]))),
            8 => Some(u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ])),
            _ => None,
        }
    };
    let integer = |bytes: &[u8]| -> Option<String> {
        if sysvar_is_signed(representation) {
            signed(bytes).map(|value| value.to_string())
        } else {
            unsigned(bytes).map(|value| value.to_string())
        }
    };
    let as_double = |bytes: &[u8]| -> Option<f64> {
        (bytes.len() >= 8).then(|| f64::from_le_bytes(bytes[..8].try_into().unwrap()))
    };
    let text_of = |bytes: &[u8]| -> String {
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).to_string()
    };
    match kind {
        1 => as_double(data).map_or_else(|| hex_text(data), |value| format!("{value}")),
        2 => integer(data).unwrap_or_else(|| hex_text(data)),
        3 => format!("{:?}", text_of(data)),
        4 => {
            // A run of doubles; show the count so a long array stays readable.
            let count = data.len() / 8;
            let mut parts: Vec<String> = data
                .as_chunks::<8>()
                .0
                .iter()
                .take(4)
                .filter_map(|chunk| as_double(chunk).map(|value| format!("{value}")))
                .collect();
            if count > parts.len() {
                parts.push(format!("... ({count} values)"));
            }
            format!("[{}]", parts.join(", "))
        }
        5 => {
            let count = data.len() / 4;
            let mut parts: Vec<String> = data
                .as_chunks::<4>()
                .0
                .iter()
                .take(4)
                .filter_map(|chunk| integer(chunk))
                .collect();
            if count > parts.len() {
                parts.push(format!("... ({count} values)"));
            }
            format!("[{}]", parts.join(", "))
        }
        6 => integer(data).unwrap_or_else(|| hex_text(data)),
        7 => format!("{} bytes {}", data.len(), hex_text(data)),
        _ => hex_text(data),
    }
}

/// Two hex digits per byte, space separated.
fn hex_text(data: &[u8]) -> String {
    data.iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Decode the text of a `VBLAppText` record.
///
/// The header says the payload is MBCS, but CANoe writes the measurement
/// comment as UTF-16LE, so both encodings appear in the wild. A UTF-16 run has
/// a zero high byte in most characters, which is what the check below keys on.
pub fn app_text_text(bytes: &[u8]) -> String {
    let trimmed = match bytes.iter().position(|byte| *byte == 0) {
        Some(0) if bytes.len() > 1 => bytes,
        _ => bytes,
    };
    let utf16_like = trimmed.len() >= 4
        && trimmed.len() % 2 == 0
        && trimmed
            .iter()
            .skip(1)
            .step_by(2)
            .filter(|b| **b == 0)
            .count()
            * 3
            >= trimmed.len() / 2;
    if utf16_like {
        let units: Vec<u16> = trimmed
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .take_while(|unit| *unit != 0)
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        let end = trimmed
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(trimmed.len());
        String::from_utf8_lossy(&trimmed[..end]).to_string()
    }
}

/// Fields packed into `VBLAppText` when the source is `DBCHANNELINFO`.
pub fn app_text_channel_info(flags: u32) -> String {
    let version = flags & 0xFF;
    let channel = (flags >> 8) & 0xFF;
    let bus_type = (flags >> 16) & 0xFF;
    let can_fd = (flags >> 24) & 0x01;
    format!("channel {channel}, bus type {bus_type}, version {version}, CAN FD {can_fd}")
}

/// A trace-line record's flags: where the line came from and how it is shown.
pub fn app_text_trace_line(flags: u32) -> String {
    format!(
        "{}, {}, {}",
        trace_line_source_name(flags & 0x0F),
        if flags & 0x10 != 0 {
            "shown in trace"
        } else {
            "hidden"
        },
        if flags & 0x20 != 0 {
            "text only"
        } else {
            "with timestamp"
        }
    )
}

/// Control-message sub type of a MOST25 message, `VBLMOSTCtrl::mRTyp`.
pub fn most_control_sub_type_name(value: u16) -> &'static str {
    match value {
        0 => "normal",
        1 => "remote read",
        2 => "remote write",
        3 => "allocate",
        4 => "deallocate",
        5 => "get source",
        _ => "unused",
    }
}

/// Addressing mode of a MOST25 control message, `VBLMOSTCtrl::mRTypAdr`.
pub fn most_addressing_mode_name(value: u8) -> &'static str {
    match value {
        0x00 => "device",
        0x10 => "node position",
        0x20 => "broadcast",
        0x30 => "group cast",
        _ => "unknown",
    }
}

/// Transmission state of a MOST25 record, `VBLMOSTSpy::mState`.
pub fn most25_state_text(value: u8) -> String {
    let mut parts = vec![if value & 0x01 != 0 {
        "bus active"
    } else {
        "bus inactive"
    }];
    if value & 0x02 != 0 {
        parts.push("unlock");
    }
    if value & 0x10 != 0 {
        parts.push("ack");
    }
    if value & 0x20 != 0 {
        parts.push("nack");
    }
    if value & 0x40 != 0 {
        parts.push("send ok");
    } else {
        parts.push("send error");
    }
    parts.join(", ")
}

/// Transmission state of a MOST50/150 record.
pub fn most50_state_text(value: u8) -> String {
    let mut parts = vec![if value & 0x01 != 0 {
        "bus active"
    } else {
        "bus inactive"
    }];
    if value & 0x10 != 0 {
        parts.push("ack");
    }
    if value & 0x20 != 0 {
        parts.push("nack");
    }
    if value & 0x40 != 0 {
        parts.push("send ok");
    } else {
        parts.push("send error");
    }
    parts.join(", ")
}

/// Acknowledge code of a control message, `mAckNack`.
pub fn most_ack_text(value: u8) -> String {
    let mut parts = Vec::new();
    if value & 0x01 != 0 {
        parts.push("no response");
    }
    if value & 0x02 != 0 {
        parts.push("valid");
    }
    if value & 0x04 != 0 {
        parts.push("CRC error");
    }
    if value & 0x08 != 0 {
        parts.push("rx buffer full");
    }
    if value & 0x10 != 0 {
        parts.push("ack");
    }
    if value & 0x20 != 0 {
        parts.push("nack");
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(", ")
    }
}

/// Network state of the MOST supervisor, `VBLMOSTNetState::mStateNew`.
pub fn most_net_state_name(value: u16) -> &'static str {
    match value {
        0 => "undefined",
        1 => "ring break diagnostics",
        2 => "power off",
        3 => "net interface init",
        4 => "configuration not ok",
        5 => "configuration ok",
        6 => "net on / init ready",
        _ => "unknown state",
    }
}

/// Operation of a register event, `VBLMOSTReg::mSubType`.
pub fn most_register_sub_type_name(value: u32) -> &'static str {
    match value {
        0 => "unspecified",
        1 => "notify",
        2 => "read request",
        3 => "write request",
        4 => "read result",
        5 => "write result",
        6 => "read failed",
        7 => "write failed",
        _ => "unknown operation",
    }
}

/// Register identifier of a `VBLMOSTGenReg` record.
pub fn most_register_id_name(value: u32) -> &'static str {
    match value {
        0x0 => "empty",
        0x87 => "node position",
        0x89 => "group address",
        0x8A => "node address",
        0x8F => "node delay",
        0x90 => "open bypass count",
        _ => "other register",
    }
}

/// Transfer type of a MOST packet, `VBLMOSTPkt::mTransferType`.
pub fn most_transfer_type_name(value: u8) -> &'static str {
    match value {
        0 => "node",
        1 => "spy",
        2 => "error",
        _ => "unknown",
    }
}

/// Asynchronous-channel priority, `VBLMOSTPkt::mPriority`.
pub fn most_priority_name(value: u32) -> &'static str {
    match value {
        0 => "low",
        1 => "high",
        2 => "system",
        _ => "unknown",
    }
}

/// FlexRay channel selection, `mChannelMask`.
pub fn flexray_channel_name(value: u16) -> &'static str {
    match value {
        0 => "invalid",
        1 => "A",
        2 => "B",
        3 => "A+B",
        _ => "unknown",
    }
}

/// Communication controller type, `mTag`.
pub fn flexray_cc_type_name(value: u32) -> &'static str {
    match value {
        0 => "architecture independent",
        2 => "Cyclone I",
        3 => "BUSDOCTOR",
        4 => "Cyclone II",
        5 => "Vector VN interface",
        6 => "VN sync pulse",
        _ => "unknown controller",
    }
}

/// Declared flags of a FlexRay frame, `mFrameFlags` bits 0-6.
pub fn flexray_frame_flags_text(flags: u32) -> String {
    let mut parts = Vec::new();
    if flags & 0x01 != 0 {
        parts.push("null frame");
    }
    if flags & 0x02 != 0 {
        parts.push("valid data");
    }
    if flags & 0x04 != 0 {
        parts.push("sync");
    }
    if flags & 0x08 != 0 {
        parts.push("startup");
    }
    if flags & 0x10 != 0 {
        parts.push("payload preamble");
    }
    if flags & 0x20 != 0 {
        parts.push("reserved bit");
    }
    if flags & 0x40 != 0 {
        parts.push("error");
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(", ")
    }
}

/// Frame state of a FlexRay V6 message, `mFrameState`, for the VN interface.
pub fn flexray_frame_state_text(value: u8) -> String {
    let mut parts = Vec::new();
    if value & 0x01 != 0 {
        parts.push("syntax error");
    }
    if value & 0x02 != 0 {
        parts.push("content error");
    }
    if value & 0x04 != 0 {
        parts.push("slot boundary violation");
    }
    if value & 0x08 != 0 {
        parts.push("empty slot");
    }
    if value & 0x10 != 0 {
        parts.push("message lost");
    }
    if value & 0x20 != 0 {
        parts.push("valid");
    }
    if value & 0x40 != 0 {
        parts.push("tx conflict");
    }
    if value & 0x80 != 0 {
        parts.push("framing error");
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(", ")
    }
}

/// Direction of a FlexRay record, `mDir`.
pub fn flexray_direction_name(value: u8) -> &'static str {
    match value {
        0 => "Rx",
        1 => "Tx",
        2 => "TxRq",
        _ => "unknown",
    }
}

/// Link speed of an AFDX adapter, `VBLAfdxBusStatistic::mLinkSpeed`.
pub fn afdx_link_speed_name(value: u16) -> &'static str {
    match value {
        0 => "10 Mbit/s",
        1 => "100 Mbit/s",
        2 => "1000 Mbit/s",
        _ => "unknown",
    }
}

/// Protocol parity of an A429 channel, `VBLA429Status::mParity`.
pub fn a429_parity_name(value: u16) -> &'static str {
    match value {
        0 => "hardware default",
        1 => "disabled",
        2 => "odd",
        3 => "even",
        4 => "mark",
        5 => "space",
        _ => "unknown parity",
    }
}

/// Severity of an A429 error event, `VBLA429ErrorEvent::mErrorType`.
pub fn a429_error_type_name(value: u16) -> &'static str {
    match value {
        0 => "error",
        1 => "warning",
        2 => "informational",
        _ => "unknown severity",
    }
}

/// Category of a WLAN frame, from `VBLWlanFrame::mFlags`.
pub fn wlan_frame_flags_text(flags: u32) -> String {
    let mut parts = Vec::new();
    if flags & 0x01 != 0 {
        parts.push("beacon");
    }
    if flags & 0x02 != 0 {
        parts.push("management");
    }
    if flags & 0x04 != 0 {
        parts.push("data");
    }
    if flags & 0x08 != 0 {
        parts.push("control");
    }
    if flags & 0x10 != 0 {
        parts.push("retry");
    }
    if flags & 0x20 != 0 {
        parts.push("tx");
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(", ")
    }
}

/// State reported by a K-Line status event, `VBLKLineStatusEvent::mType`.
pub fn kline_status_name(value: u32) -> &'static str {
    match value {
        0 => "wake-up pattern",
        1 => "wake-up pattern finished",
        2 => "communication started",
        3 => "communication stopped",
        _ => "unknown status",
    }
}

/// Kind of a J1708 payload, from `VBLJ1708Message::mType`.
pub fn j1708_type_name(value: u8) -> &'static str {
    match value {
        0 => "MID",
        1 => "PID",
        2 => "data",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_values_keep_their_meaning() {
        assert_eq!(can_driver_error_name(102), "chip state bus off");
        assert_eq!(can_error_code_name(4), "CRC error");
        assert_eq!(can_error_direction_name(3), "TX");
        assert_eq!(lin_sleep_reason_name(1), "go-to-sleep frame");
        assert_eq!(
            lin_sleep_flags_text(0b111),
            "awake -> awake, external event"
        );
        assert_eq!(lin_wakeup_length_name(1), "wakeup too short");
        assert_eq!(lin_checksum_model_name(0xFF), "unknown");
        assert_eq!(lin_disturbance_type_name(2), "disturbance with a header");
        assert_eq!(lin_long_dom_signal_type_name(2), "finished");
        assert_eq!(lin_spike_origin_name(1), "simulated");
        assert_eq!(ethernet_rx_error_name(2), "invalid CRC");
        assert_eq!(ethernet_sqi_name(5), "excellent");
    }

    #[test]
    fn unlisted_values_stay_recognisable() {
        assert_eq!(can_driver_error_name(9999), "unlisted driver error");
        assert_eq!(can_error_code_name(6), "unknown error");
        assert_eq!(lin_sleep_reason_name(7), "unknown reason");
        assert_eq!(ethernet_rx_error_name(9), "unknown error");
        assert_eq!(ethernet_sqi_name(9), "unknown");
    }
}

#[cfg(test)]
mod sysvar_values {
    use super::*;

    #[test]
    fn integer_values_honour_the_representation_flag() {
        let minus_five = (-5i32).to_le_bytes();
        // 0 is signed, 1 is unsigned, exactly as the format document states.
        assert_eq!(sysvar_value_text(2, 0, &minus_five), "-5");
        assert_eq!(sysvar_value_text(2, 1, &minus_five), "4294967291");
        assert!(sysvar_is_signed(0));
        assert!(!sysvar_is_signed(1));

        let minus_one_long = (-1i64).to_le_bytes();
        assert_eq!(sysvar_value_text(6, 0, &minus_one_long), "-1");
        assert_eq!(
            sysvar_value_text(6, 1, &minus_one_long),
            "18446744073709551615"
        );
    }

    #[test]
    fn every_declared_type_renders_something() {
        assert_eq!(sysvar_value_text(1, 0, &2.5f64.to_le_bytes()), "2.5");
        assert_eq!(sysvar_value_text(2, 1, &1234u32.to_le_bytes()), "1234");
        assert_eq!(sysvar_value_text(3, 0, b"hello "), "\"hello\"");
        assert_eq!(sysvar_value_text(3, 0, b""), "\"\"");
        let doubles: Vec<u8> = [1.5f64, 2.5]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        assert_eq!(sysvar_value_text(4, 0, &doubles), "[1.5, 2.5]");
        let longs: Vec<u8> = [10u32, 20]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        assert_eq!(sysvar_value_text(5, 1, &longs), "[10, 20]");
        assert_eq!(
            sysvar_value_text(6, 1, &9_000_000_000u64.to_le_bytes()),
            "9000000000"
        );
        assert_eq!(sysvar_value_text(7, 0, &[0xDE, 0xAD]), "2 bytes DE AD");
        // An unlisted type and a truncated payload stay readable as hex.
        assert_eq!(sysvar_value_text(99, 0, &[0xAB, 0xCD]), "AB CD");
        assert_eq!(sysvar_value_text(1, 0, &[1, 2]), "01 02");
        assert_eq!(sysvar_value_text(2, 0, &[1]), "01");
    }

    #[test]
    fn long_arrays_stay_short_for_wide_payloads() {
        let values: Vec<u8> = (0..10u32)
            .flat_map(|value| (value * 100).to_le_bytes())
            .collect();
        let text = sysvar_value_text(5, 1, &values);
        assert!(text.contains("... (10 values)"), "{text}");
        assert!(text.starts_with("[0, 100, 200, 300"), "{text}");
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    /// Every table must answer for every byte value a file can hold.
    #[test]
    fn all_name_tables_cover_every_input() {
        for value in 0..=255u8 {
            assert!(!can_error_code_name(value).is_empty());
            assert!(!can_error_direction_name(value).is_empty());
            assert!(!lin_sleep_reason_name(value).is_empty());
            assert!(!lin_wakeup_length_name(value).is_empty());
            assert!(!lin_checksum_model_name(value).is_empty());
            assert!(!lin_long_dom_signal_type_name(value).is_empty());
            assert!(!lin_spike_origin_name(value).is_empty());
            assert!(!lin_sleep_flags_text(value).is_empty());
            assert!(!ethernet_sqi_name(i16::from(value)).is_empty());
        }
        for value in 0..=300u32 {
            assert!(!can_driver_error_name(value).is_empty());
            assert!(!lin_disturbance_type_name(value).is_empty());
            assert!(!ethernet_rx_error_name(value).is_empty());
        }
    }

    #[test]
    fn documented_tables_name_every_listed_value() {
        // The comments in `events.rs` quote these tables, so a duplicate or a
        // silent gap would show up as an unlisted value.
        let driver: Vec<u32> = vec![
            0, 1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 14, 100, 101, 102, 103, 104, 105, 106, 107, 108,
            109, 110, 111, 112, 113, 114, 115, 217, 218, 219, 220, 221, 222, 232, 248, 249, 250,
            251, 252, 455,
        ];
        for value in driver {
            let name = can_driver_error_name(value);
            assert_ne!(name, "unlisted driver error", "code {value}");
        }
        for value in [0, 1, 2, 3, 4, 5, 7] {
            assert_ne!(can_error_code_name(value), "unknown error", "code {value}");
        }
        for value in [0, 1, 2, 3, 9, 10, 11, 18] {
            assert_ne!(lin_sleep_reason_name(value), "unknown reason", "{value}");
        }
        for value in [0, 1, 2, 3, 4] {
            assert_ne!(
                lin_disturbance_type_name(value),
                "unknown disturbance",
                "{value}"
            );
        }
    }
}
