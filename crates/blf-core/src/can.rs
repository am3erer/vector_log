//! DBC-defined CAN signal extraction and conversion.

use thiserror::Error;

use crate::database::{
    ByteOrder, CanPayloadKind, CanSignal, DbcDatabase, DbcMessage, MultiplexRole, SignalType,
};

#[derive(Debug, Clone, PartialEq, Error)]
pub enum CanDecodeError {
    #[error("CAN payload is too short: required {required} bytes, got {actual}")]
    PayloadTooShort { required: usize, actual: usize },
    #[error(
        "CAN signal {signal:?} has an invalid bit range: start={start_bit}, length={bit_length}"
    )]
    InvalidBitRange {
        signal: String,
        start_bit: u16,
        bit_length: u16,
    },
    #[error("CAN signal {signal:?} is outside the payload: required bit {required_bit}, available bits {available_bits}")]
    SignalOutOfBounds {
        signal: String,
        required_bit: usize,
        available_bits: usize,
    },
    #[error("CAN signal {signal:?} has an unsupported length {bit_length}")]
    UnsupportedBitLength { signal: String, bit_length: u16 },
    #[error("CAN signal {signal:?} decoded value {value} outside [{minimum}, {maximum}]")]
    ValueOutOfRange {
        signal: String,
        value: f64,
        minimum: f64,
        maximum: f64,
    },
    #[error("CAN message id 0x{id:X} ({kind}) was not found in the database")]
    UnknownMessage { id: u32, kind: &'static str },
    #[error("CAN multiplex selector is missing from message {message:?}")]
    MissingMultiplexSelector { message: String },
    #[error("CAN message {message:?} has multiple multiplex selectors")]
    MultipleMultiplexSelectors { message: String },
    #[error("CAN multiplex selector {value} is invalid for message {message:?}")]
    InvalidMultiplexSelector { message: String, value: i128 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedCanSignal {
    pub signal_name: String,
    pub raw_value: i128,
    pub physical_value: f64,
    pub value_text: Option<String>,
}

/// Find a DBC message using both the numeric identifier and standard/extended
/// form. The form is intentionally explicit because the same numeric value can
/// legally occur in separate standard and extended namespaces.
pub fn message_for_id(
    database: &DbcDatabase,
    id: u32,
    extended: bool,
) -> Result<&DbcMessage, CanDecodeError> {
    database
        .message_by_id(id, extended)
        .ok_or(CanDecodeError::UnknownMessage {
            id,
            kind: if extended { "extended" } else { "standard" },
        })
}

pub fn decode_signal(signal: &CanSignal, data: &[u8]) -> Result<DecodedCanSignal, CanDecodeError> {
    let raw_bits = extract_raw(signal, data)?;
    let raw_value = match signal.signal_type {
        SignalType::Unsigned => i128::from(raw_bits),
        SignalType::Signed => i128::from(sign_extend(raw_bits, signal.bit_length)),
    };
    let physical_value = raw_value as f64 * signal.factor + signal.offset;
    let minimum = signal.minimum.unwrap_or(f64::NEG_INFINITY);
    let maximum = signal.maximum.unwrap_or(f64::INFINITY);
    if physical_value < minimum || physical_value > maximum {
        return Err(CanDecodeError::ValueOutOfRange {
            signal: signal.name.clone(),
            value: physical_value,
            minimum,
            maximum,
        });
    }
    Ok(DecodedCanSignal {
        signal_name: signal.name.clone(),
        raw_value,
        physical_value,
        value_text: signal.choices.get(raw_value).map(str::to_owned),
    })
}

pub fn decode_message(
    message: &DbcMessage,
    data: &[u8],
) -> Result<Vec<DecodedCanSignal>, CanDecodeError> {
    let required = usize::from(message.dlc);
    if message.payload_kind == CanPayloadKind::Physical && data.len() < required {
        return Err(CanDecodeError::PayloadTooShort {
            required,
            actual: data.len(),
        });
    }

    let selector_value = multiplex_selector_value(message, data)?;

    if message
        .signals
        .iter()
        .any(|signal| matches!(signal.multiplex, MultiplexRole::Branch(_)))
        && selector_value.is_none()
    {
        return Err(CanDecodeError::MissingMultiplexSelector {
            message: message.name.clone(),
        });
    }

    message
        .signals
        .iter()
        .filter(|signal| match &signal.multiplex {
            MultiplexRole::None | MultiplexRole::Selector => true,
            MultiplexRole::Branch(values) => selector_value
                .map(|value| values.contains(&value))
                .unwrap_or(false),
        })
        .map(|signal| decode_signal(signal, data))
        .collect()
}

/// Decode one named signal without applying a logical Vector message DLC as
/// a physical payload requirement. This is for container messages whose
/// declared DLC can exceed the 64-byte CAN-FD wire payload limit.
pub fn decode_signal_for_message(
    message: &DbcMessage,
    signal_name: &str,
    data: &[u8],
) -> Result<Option<DecodedCanSignal>, CanDecodeError> {
    // Ordinary CAN/CAN-FD messages have a physical DLC.  Logical Vector
    // container messages may declare a larger application-level DLC, so those
    // continue to validate only the requested signal's bit range below.
    if message.payload_kind == CanPayloadKind::Physical && data.len() < usize::from(message.dlc) {
        return Err(CanDecodeError::PayloadTooShort {
            required: usize::from(message.dlc),
            actual: data.len(),
        });
    }
    let Some(signal) = message.signal(signal_name) else {
        return Ok(None);
    };
    let selector_value = multiplex_selector_value(message, data)?;
    if let MultiplexRole::Branch(values) = &signal.multiplex {
        if !selector_value.is_some_and(|value| values.contains(&value)) {
            return Ok(None);
        }
    }
    decode_signal(signal, data).map(Some)
}

fn multiplex_selector_value(
    message: &DbcMessage,
    data: &[u8],
) -> Result<Option<u32>, CanDecodeError> {
    let selectors: Vec<_> = message
        .signals
        .iter()
        .filter(|signal| signal.multiplex == MultiplexRole::Selector)
        .collect();
    if selectors.len() > 1 {
        return Err(CanDecodeError::MultipleMultiplexSelectors {
            message: message.name.clone(),
        });
    }
    let Some(selector) = selectors.first() else {
        if message
            .signals
            .iter()
            .any(|signal| matches!(signal.multiplex, MultiplexRole::Branch(_)))
        {
            return Err(CanDecodeError::MissingMultiplexSelector {
                message: message.name.clone(),
            });
        }
        return Ok(None);
    };
    let decoded = decode_signal(selector, data)?;
    let value =
        u32::try_from(decoded.raw_value).map_err(|_| CanDecodeError::InvalidMultiplexSelector {
            message: message.name.clone(),
            value: decoded.raw_value,
        })?;
    Ok(Some(value))
}

fn extract_raw(signal: &CanSignal, data: &[u8]) -> Result<u64, CanDecodeError> {
    let bit_length = usize::from(signal.bit_length);
    if bit_length == 0 || bit_length > 64 {
        return Err(CanDecodeError::UnsupportedBitLength {
            signal: signal.name.clone(),
            bit_length: signal.bit_length,
        });
    }

    match signal.byte_order {
        ByteOrder::LittleEndian => extract_little_endian(signal, data),
        ByteOrder::BigEndian => extract_big_endian(signal, data),
    }
}

fn extract_little_endian(signal: &CanSignal, data: &[u8]) -> Result<u64, CanDecodeError> {
    let start = usize::from(signal.start_bit);
    let length = usize::from(signal.bit_length);
    let available_bits = data.len().saturating_mul(8);
    let end = start
        .checked_add(length)
        .ok_or_else(|| CanDecodeError::InvalidBitRange {
            signal: signal.name.clone(),
            start_bit: signal.start_bit,
            bit_length: signal.bit_length,
        })?;
    if end > available_bits {
        return Err(CanDecodeError::SignalOutOfBounds {
            signal: signal.name.clone(),
            required_bit: end,
            available_bits,
        });
    }

    let mut raw = 0u64;
    for offset in 0..length {
        let bit = start + offset;
        let byte = data[bit / 8];
        raw |= u64::from((byte >> (bit % 8)) & 1) << offset;
    }
    Ok(raw)
}

fn extract_big_endian(signal: &CanSignal, data: &[u8]) -> Result<u64, CanDecodeError> {
    let mut bit = usize::from(signal.start_bit);
    let length = usize::from(signal.bit_length);
    let available_bits = data.len().saturating_mul(8);
    if bit >= available_bits {
        return Err(CanDecodeError::SignalOutOfBounds {
            signal: signal.name.clone(),
            required_bit: bit + 1,
            available_bits,
        });
    }

    let mut raw = 0u64;
    for _ in 0..length {
        if bit >= available_bits {
            return Err(CanDecodeError::SignalOutOfBounds {
                signal: signal.name.clone(),
                required_bit: bit + 1,
                available_bits,
            });
        }
        raw = (raw << 1) | u64::from((data[bit / 8] >> (bit % 8)) & 1);
        // DBC Motorola numbering walks toward bit 0, then continues at the
        // MSB of the next byte (the network-order/sawtooth numbering used by
        // cantools and Vector databases).
        bit = if bit % 8 == 0 { bit + 15 } else { bit - 1 };
    }
    Ok(raw)
}

fn sign_extend(raw: u64, bit_length: u16) -> i64 {
    if bit_length == 64 {
        raw as i64
    } else {
        let sign_bit = 1u64 << (u32::from(bit_length) - 1);
        if raw & sign_bit != 0 {
            (raw | (!0u64 << u32::from(bit_length))) as i64
        } else {
            raw as i64
        }
    }
}
