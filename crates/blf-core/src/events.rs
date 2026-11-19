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
