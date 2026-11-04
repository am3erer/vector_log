//! Small, dependency-free DBC parser.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::database::{
    ByteOrder, CanMessageId, CanPayloadKind, CanSignal, DatabaseError, DatabaseResult,
    DbcAttributeAssignment, DbcAttributeDefinition, DbcDatabase, DbcMessage, DbcNode,
    DbcRelationshipAttribute, DbcSignalGroup, MultiplexRole, SignalType, SourceSpan, ValueTable,
};

#[derive(Debug, Clone, PartialEq)]
struct Token {
    text: String,
    line: usize,
    column: usize,
    quoted: bool,
}

fn error(
    path: &str,
    token: Option<&Token>,
    field: &str,
    reason: impl Into<String>,
) -> DatabaseError {
    DatabaseError::new(path, token.map_or(0, |value| value.line), field, reason)
}

fn lex_line(path: &str, line: &str, line_number: usize) -> DatabaseResult<Vec<Token>> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index].is_whitespace() {
            index += 1;
            continue;
        }
        let column = index + 1;
        if chars[index] == '"' {
            index += 1;
            let mut value = String::new();
            let mut closed = false;
            while index < chars.len() {
                match chars[index] {
                    '"' => {
                        index += 1;
                        closed = true;
                        break;
                    }
                    '\\' if index + 1 < chars.len() => {
                        index += 1;
                        value.push(match chars[index] {
                            'n' => '\n',
                            'r' => '\r',
                            't' => '\t',
                            other => other,
                        });
                        index += 1;
                    }
                    other => {
                        value.push(other);
                        index += 1;
                    }
                }
            }
            if !closed {
                return Err(error(
                    path,
                    Some(&Token {
                        text: String::new(),
                        line: line_number,
                        column,
                        quoted: false,
                    }),
                    "string",
                    "unterminated quoted string",
                ));
            }
            tokens.push(Token {
                text: value,
                line: line_number,
                column,
                quoted: true,
            });
            continue;
        }
        if ":|@(),[]{};".contains(chars[index]) {
            tokens.push(Token {
                text: chars[index].to_string(),
                line: line_number,
                column,
                quoted: false,
            });
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len()
            && !chars[index].is_whitespace()
            && !":|@(),[]{};\"".contains(chars[index])
        {
            index += 1;
        }
        tokens.push(Token {
            text: chars[start..index].iter().collect(),
            line: line_number,
            column: start + 1,
            quoted: false,
        });
    }
    Ok(tokens)
}

fn parse_u32(path: &str, token: &Token, field: &str) -> DatabaseResult<u32> {
    let value = if let Some(hex) = token
        .text
        .strip_prefix("0x")
        .or_else(|| token.text.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16)
    } else {
        token.text.parse()
    };
    value.map_err(|_| {
        error(
            path,
            Some(token),
            field,
            format!("invalid integer {:?}", token.text),
        )
    })
}

fn parse_i128(path: &str, token: &Token, field: &str) -> DatabaseResult<i128> {
    let text = token.text.trim();
    let (negative, digits) = text
        .strip_prefix('-')
        .map_or((false, text), |value| (true, value));
    let value = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i128::from_str_radix(hex, 16).map(|value| if negative { -value } else { value })
    } else {
        text.parse()
    };
    value.map_err(|_| {
        error(
            path,
            Some(token),
            field,
            format!("invalid integer {:?}", token.text),
        )
    })
}

fn without_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quoted = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if quoted => index += 2,
            b'"' => {
                quoted = !quoted;
                index += 1;
            }
            b'/' if !quoted && bytes.get(index + 1) == Some(&b'/') => return &line[..index],
            _ => index += 1,
        }
    }
    line
}

fn quote_is_open(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut quoted = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if quoted => index += 2,
            b'"' => {
                quoted = !quoted;
                index += 1;
            }
            b'/' if !quoted && bytes.get(index + 1) == Some(&b'/') => break,
            _ => index += 1,
        }
    }
    quoted
}

fn decode_source(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(source) => source,
        Err(error) => error.into_bytes().into_iter().map(char::from).collect(),
    }
}

fn parse_f64(path: &str, token: &Token, field: &str) -> DatabaseResult<f64> {
    let value = token.text.parse::<f64>().map_err(|_| {
        error(
            path,
            Some(token),
            field,
            format!("invalid number {:?}", token.text),
        )
    })?;
    if !value.is_finite() {
        return Err(error(path, Some(token), field, "number must be finite"));
    }
    Ok(value)
}

fn expect<'a>(
    path: &str,
    tokens: &'a [Token],
    index: usize,
    text: &str,
    field: &str,
) -> DatabaseResult<&'a Token> {
    let token = tokens
        .get(index)
        .ok_or_else(|| error(path, tokens.last(), field, format!("expected {:?}", text)))?;
    if token.text != text {
        return Err(error(
            path,
            Some(token),
            field,
            format!("expected {:?}, got {:?}", text, token.text),
        ));
    }
    Ok(token)
}

fn message_id(path: &str, token: &Token) -> DatabaseResult<CanMessageId> {
    let raw = parse_u32(path, token, "message_id")?;
    let extended = raw & 0x8000_0000 != 0;
    let id = if extended { raw & 0x1FFF_FFFF } else { raw };
    if (!extended && id > 0x7FF) || (extended && id > 0x1FFF_FFFF) {
        return Err(error(
            path,
            Some(token),
            "message_id",
            "CAN identifier is out of range",
        ));
    }
    Ok(CanMessageId::new(id, extended))
}

fn parse_bo(path: &str, tokens: &[Token], database: &mut DbcDatabase) -> DatabaseResult<()> {
    if tokens.len() != 6 {
        return Err(error(
            path,
            tokens.first(),
            "BO_",
            "message definition must contain id, name, ':', DLC, and transmitter",
        ));
    }
    let id = message_id(path, &tokens[1])?;
    expect(path, tokens, 3, ":", "BO_")?;
    let dlc = parse_u32(path, &tokens[4], "dlc")?;
    let dlc = u8::try_from(dlc).map_err(|_| {
        error(
            path,
            Some(&tokens[4]),
            "dlc",
            "DBC message DLC is outside the supported 0..255 range",
        )
    })?;
    database.insert_message(DbcMessage {
        id,
        name: tokens[2].text.clone(),
        dlc,
        is_fd: false,
        // Payload kind is assigned only after an explicit Vector
        // VFrameFormat marker is parsed.  An oversized DLC alone is not
        // sufficient to relax physical payload validation.
        payload_kind: CanPayloadKind::Physical,
        transmitter: tokens[5].text.clone(),
        signals: Vec::new(),
        comment: None,
        attributes: Default::default(),
        source: Some(SourceSpan::new(tokens[0].line, tokens[0].column)),
    })
}

fn parse_signal(
    path: &str,
    tokens: &[Token],
    message_id: CanMessageId,
    database: &mut DbcDatabase,
) -> DatabaseResult<()> {
    if tokens.len() < 17 {
        return Err(error(
            path,
            tokens.first(),
            "SG_",
            "signal definition is incomplete",
        ));
    }
    let mut index = 1;
    let name = tokens[index].text.clone();
    index += 1;
    let multiplex = match tokens.get(index).map(|token| token.text.as_str()) {
        Some("M") => {
            index += 1;
            MultiplexRole::Selector
        }
        Some(value) if value.starts_with('m') => {
            let selector = value[1..].parse::<u32>().map_err(|_| {
                error(
                    path,
                    tokens.get(index),
                    "multiplex",
                    "invalid multiplex selector",
                )
            })?;
            index += 1;
            MultiplexRole::Branch([selector].into_iter().collect())
        }
        _ => MultiplexRole::None,
    };
    expect(path, tokens, index, ":", "SG_")?;
    index += 1;
    let start_bit = parse_u32(
        path,
        tokens
            .get(index)
            .ok_or_else(|| error(path, tokens.last(), "start_bit", "missing start bit"))?,
        "start_bit",
    )?;
    index += 1;
    expect(path, tokens, index, "|", "SG_")?;
    index += 1;
    let bit_length = parse_u32(
        path,
        tokens
            .get(index)
            .ok_or_else(|| error(path, tokens.last(), "bit_length", "missing bit length"))?,
        "bit_length",
    )?;
    index += 1;
    expect(path, tokens, index, "@", "SG_")?;
    index += 1;
    let byte_order_token = tokens
        .get(index)
        .ok_or_else(|| error(path, tokens.last(), "byte_order", "missing byte order"))?;
    let byte_order = match byte_order_token.text.chars().next() {
        Some('1') => ByteOrder::LittleEndian,
        Some('0') => ByteOrder::BigEndian,
        _ => {
            return Err(error(
                path,
                Some(byte_order_token),
                "byte_order",
                "expected 0 or 1",
            ))
        }
    };
    let signal_type = match byte_order_token.text.chars().nth(1) {
        Some('+') => SignalType::Unsigned,
        Some('-') => SignalType::Signed,
        _ => {
            return Err(error(
                path,
                Some(byte_order_token),
                "signal_type",
                "expected + or -",
            ))
        }
    };
    index += 1;
    expect(path, tokens, index, "(", "SG_")?;
    index += 1;
    let factor = parse_f64(
        path,
        tokens
            .get(index)
            .ok_or_else(|| error(path, tokens.last(), "factor", "missing factor"))?,
        "factor",
    )?;
    index += 1;
    expect(path, tokens, index, ",", "SG_")?;
    index += 1;
    let offset = parse_f64(
        path,
        tokens
            .get(index)
            .ok_or_else(|| error(path, tokens.last(), "offset", "missing offset"))?,
        "offset",
    )?;
    index += 1;
    expect(path, tokens, index, ")", "SG_")?;
    index += 1;
    expect(path, tokens, index, "[", "SG_")?;
    index += 1;
    let minimum = parse_f64(
        path,
        tokens
            .get(index)
            .ok_or_else(|| error(path, tokens.last(), "minimum", "missing minimum"))?,
        "minimum",
    )?;
    index += 1;
    expect(path, tokens, index, "|", "SG_")?;
    index += 1;
    let maximum = parse_f64(
        path,
        tokens
            .get(index)
            .ok_or_else(|| error(path, tokens.last(), "maximum", "missing maximum"))?,
        "maximum",
    )?;
    if minimum > maximum {
        return Err(error(
            path,
            tokens.get(index - 1),
            "limits",
            "minimum must not exceed maximum",
        ));
    }
    index += 1;
    expect(path, tokens, index, "]", "SG_")?;
    index += 1;
    let unit_token = tokens
        .get(index)
        .ok_or_else(|| error(path, tokens.last(), "unit", "missing unit"))?;
    if !unit_token.quoted {
        return Err(error(path, Some(unit_token), "unit", "unit must be quoted"));
    }
    // An empty quoted unit is the DBC spelling for an absent unit. Match the
    // nullable metadata contract used by database consumers.
    let unit = (!unit_token.text.is_empty()).then(|| unit_token.text.clone());
    index += 1;
    let receiver = tokens
        .get(index)
        .ok_or_else(|| error(path, tokens.last(), "receiver", "missing receiver"))?;
    let mut receivers = Vec::new();
    if receiver.quoted || receiver.text == "," {
        return Err(error(
            path,
            Some(receiver),
            "receiver",
            "receiver must be an ECU name",
        ));
    }
    receivers.push(receiver.text.clone());
    index += 1;
    while index < tokens.len() {
        if tokens[index].text == "," {
            let comma = &tokens[index];
            index += 1;
            let receiver = tokens.get(index).ok_or_else(|| {
                error(
                    path,
                    Some(comma),
                    "receiver",
                    "receiver list cannot end with a comma",
                )
            })?;
            if receiver.quoted || receiver.text == "," {
                return Err(error(
                    path,
                    Some(receiver),
                    "receiver",
                    "receiver must be an ECU name after a comma",
                ));
            }
            receivers.push(receiver.text.clone());
            index += 1;
        } else {
            let receiver = &tokens[index];
            if receiver.quoted {
                return Err(error(
                    path,
                    Some(receiver),
                    "receiver",
                    "receiver must be an ECU name",
                ));
            }
            // Accept both comma-separated and legacy whitespace-separated
            // receiver lists emitted by existing DBC generators.
            receivers.push(receiver.text.clone());
            index += 1;
        }
    }
    let start_bit = u16::try_from(start_bit).map_err(|_| {
        error(
            path,
            tokens.get(3),
            "start_bit",
            "signal start bit is outside u16",
        )
    })?;
    let bit_length = u16::try_from(bit_length).map_err(|_| {
        error(
            path,
            tokens.get(5),
            "bit_length",
            "signal bit length is outside u16",
        )
    })?;
    if bit_length == 0 || bit_length > 64 {
        return Err(error(
            path,
            tokens.get(5),
            "bit_length",
            "signal bit length must be between 1 and 64",
        ));
    }
    // DBC uses [0|0] as the conventional "no physical limits specified"
    // marker. Keep both bounds absent so decoding does not reject valid raw
    // values merely because a generator omitted signal limits.
    let (minimum, maximum) = if minimum == 0.0 && maximum == 0.0 {
        (None, None)
    } else {
        (Some(minimum), Some(maximum))
    };
    let mut signal = CanSignal::new(name, start_bit, bit_length, byte_order, signal_type);
    signal.factor = factor;
    signal.offset = offset;
    signal.minimum = minimum;
    signal.maximum = maximum;
    signal.unit = unit;
    signal.receivers = receivers;
    signal.multiplex = multiplex;
    signal.source = Some(SourceSpan::new(tokens[0].line, tokens[0].column));
    database.add_signal(message_id, signal)
}

fn parse_val(path: &str, tokens: &[Token], database: &mut DbcDatabase) -> DatabaseResult<()> {
    if tokens.len() < 5 {
        return Err(error(
            path,
            tokens.first(),
            "VAL_",
            "value table is incomplete",
        ));
    }
    let id = message_id(path, &tokens[1])?;
    let signal = tokens[2].text.as_str();
    if !(tokens.len() - 3).is_multiple_of(2) {
        return Err(error(
            path,
            tokens.last(),
            "VAL_",
            "value table must contain raw/text pairs",
        ));
    }
    let mut values = Vec::new();
    let mut index = 3;
    while index + 1 < tokens.len() {
        let raw = parse_i128(path, &tokens[index], "VAL_")?;
        values.push((raw, tokens[index + 1].text.clone()));
        index += 2;
    }
    database.set_signal_choices(id, signal, ValueTable::new(values))
}

fn parse_value_table(
    path: &str,
    tokens: &[Token],
    database: &mut DbcDatabase,
) -> DatabaseResult<()> {
    if tokens.len() < 4 || !tokens.len().is_multiple_of(2) {
        return Err(error(
            path,
            tokens.first(),
            "VAL_TABLE_",
            "value table must contain a name and raw/text pairs",
        ));
    }
    let name = &tokens[1];
    if name.quoted {
        return Err(error(
            path,
            Some(name),
            "VAL_TABLE_",
            "value table name must be unquoted",
        ));
    }
    let mut values = Vec::new();
    let mut index = 2;
    while index + 1 < tokens.len() {
        let raw = parse_i128(path, &tokens[index], "VAL_TABLE_")?;
        let text = &tokens[index + 1];
        if !text.quoted {
            return Err(error(
                path,
                Some(text),
                "VAL_TABLE_",
                "value table text must be quoted",
            ));
        }
        values.push((raw, text.text.clone()));
        index += 2;
    }
    database.set_value_table(name.text.clone(), ValueTable::new(values));
    Ok(())
}

fn parse_signal_value_type(
    path: &str,
    tokens: &[Token],
    database: &mut DbcDatabase,
) -> DatabaseResult<()> {
    if tokens.len() != 4 && tokens.len() != 5 {
        return Err(error(
            path,
            tokens.first(),
            "SIG_VALTYPE_",
            "statement must contain message id, signal name, and value type",
        ));
    }
    let id = message_id(path, &tokens[1])?;
    let value_index = if tokens.len() == 5 {
        expect(path, tokens, 3, ":", "SIG_VALTYPE_")?;
        4
    } else {
        3
    };
    let value_type = parse_u32(path, &tokens[value_index], "value_type")?;
    let value_type = u8::try_from(value_type).map_err(|_| {
        error(
            path,
            Some(&tokens[value_index]),
            "value_type",
            "value type is outside the supported 0..255 range",
        )
    })?;
    database.set_signal_value_type(id, &tokens[2].text, value_type)
}

fn parse_signal_type_value_type(
    path: &str,
    tokens: &[Token],
    database: &mut DbcDatabase,
) -> DatabaseResult<()> {
    if tokens.len() != 3 {
        return Err(error(
            path,
            tokens.first(),
            "SIGTYPE_VALTYPE_",
            "statement must contain signal type and value type",
        ));
    }
    let signal_type = parse_u32(path, &tokens[1], "signal_type")?;
    let value_type = parse_u32(path, &tokens[2], "value_type")?;
    let signal_type = u8::try_from(signal_type).map_err(|_| {
        error(
            path,
            Some(&tokens[1]),
            "signal_type",
            "signal type is outside the supported 0..255 range",
        )
    })?;
    let value_type = u8::try_from(value_type).map_err(|_| {
        error(
            path,
            Some(&tokens[2]),
            "value_type",
            "value type is outside the supported 0..255 range",
        )
    })?;
    database.set_signal_type_value_type(signal_type, value_type);
    Ok(())
}

fn parse_comment(path: &str, tokens: &[Token], database: &mut DbcDatabase) -> DatabaseResult<()> {
    if tokens.len() < 2 {
        return Err(error(
            path,
            tokens.first(),
            "CM_",
            "comment definition is incomplete",
        ));
    }
    match tokens[1].text.as_str() {
        "BO_" => {
            if tokens.len() != 4 {
                return Err(error(
                    path,
                    tokens.first(),
                    "CM_",
                    "message comment definition must be `CM_ BO_ <id> \"comment\"`",
                ));
            }
            let id = message_id(path, &tokens[2])?;
            let text = &tokens[3];
            if !text.quoted {
                return Err(error(path, Some(text), "CM_", "comment must be quoted"));
            }
            database.set_message_comment(id, text.text.clone())
        }
        "SG_" => {
            if tokens.len() != 5 {
                return Err(error(
                    path,
                    tokens.first(),
                    "CM_",
                    "signal comment definition must be `CM_ SG_ <id> <signal> \"comment\"`",
                ));
            }
            let id = message_id(path, &tokens[2])?;
            let text = &tokens[4];
            if !text.quoted {
                return Err(error(path, Some(text), "CM_", "comment must be quoted"));
            }
            database.set_signal_comment(id, &tokens[3].text, text.text.clone())
        }
        _ => {
            if tokens.len() != 2 || !tokens[1].quoted {
                return Err(error(
                    path,
                    tokens.first(),
                    "CM_",
                    "global comment must contain exactly one quoted value",
                ));
            }
            database.set_attribute("comment", tokens[1].text.clone());
            Ok(())
        }
    }
}

fn parse_attribute(
    path: &str,
    tokens: &[Token],
    database: &mut DbcDatabase,
    attribute_definitions: &HashMap<String, AttributeScope>,
) -> DatabaseResult<()> {
    if tokens.len() < 3 || !tokens[1].quoted {
        return Err(error(
            path,
            tokens.first(),
            "BA_",
            "attribute definition is incomplete",
        ));
    }
    let name = tokens[1].text.clone();
    let expected_scope = match tokens[2].text.as_str() {
        "BO_" => AttributeScope::Message,
        "SG_" => AttributeScope::Signal,
        "BU_" => AttributeScope::Node,
        "EV_" => AttributeScope::Environment,
        _ => AttributeScope::Global,
    };
    let is_defined = attribute_definitions.get(&name).map_or(
        name == "VFrameFormat" && expected_scope == AttributeScope::Message,
        |scope| *scope == expected_scope,
    );
    match tokens[2].text.as_str() {
        "BO_" => {
            if !is_defined {
                return Err(error(
                    path,
                    Some(&tokens[1]),
                    "BA_",
                    format!("unsupported attribute scope for {:?}", name),
                ));
            }
            if tokens.len() != 5 {
                return Err(error(
                    path,
                    tokens.first(),
                    "BA_",
                    "message attribute must contain exactly one value",
                ));
            }
            let id = message_id(
                path,
                tokens
                    .get(3)
                    .ok_or_else(|| error(path, tokens.last(), "BA_", "missing message id"))?,
            )?;
            let value = tokens[4].text.clone();
            if name == "VFrameFormat" {
                let is_fd = parse_vframe_format(path, &tokens[4])?;
                database.set_message_is_fd(id, is_fd)?;
                if tokens[4].text == "3" {
                    database.set_message_payload_kind(id, CanPayloadKind::LogicalContainer)?;
                }
            }
            database.set_message_attribute(id, name.clone(), value.clone())?;
            database.add_attribute_assignment(DbcAttributeAssignment {
                name,
                scope: "message".to_owned(),
                message_id: Some(id),
                signal_name: None,
                object_name: None,
                value,
                source: Some(SourceSpan::new(tokens[0].line, tokens[0].column)),
            });
        }
        "SG_" | "BU_" | "EV_" => {
            if !is_defined {
                return Err(error(
                    path,
                    Some(&tokens[1]),
                    "BA_",
                    format!("unsupported attribute scope for {:?}", name),
                ));
            }
            let expected_len = match tokens[2].text.as_str() {
                "SG_" => 6,
                "BU_" | "EV_" => 5,
                _ => unreachable!(),
            };
            if tokens.len() != expected_len {
                return Err(error(
                    path,
                    tokens.first(),
                    "BA_",
                    "scoped attribute assignment has an invalid shape",
                ));
            }
            let value = tokens[expected_len - 1].text.clone();
            match tokens[2].text.as_str() {
                "SG_" => {
                    let id = message_id(path, &tokens[3])?;
                    let signal_name = tokens[4].text.clone();
                    database.set_signal_attribute(id, &signal_name, name.clone(), value.clone())?;
                    database.add_attribute_assignment(DbcAttributeAssignment {
                        name,
                        scope: "signal".to_owned(),
                        message_id: Some(id),
                        signal_name: Some(signal_name),
                        object_name: None,
                        value,
                        source: Some(SourceSpan::new(tokens[0].line, tokens[0].column)),
                    });
                }
                scope => {
                    let object_name = tokens[3].text.clone();
                    database.set_scoped_attribute(scope, &object_name, name.clone(), value.clone());
                    database.add_attribute_assignment(DbcAttributeAssignment {
                        name,
                        scope: match scope {
                            "BU_" => "node",
                            "EV_" => "environment",
                            _ => scope,
                        }
                        .to_owned(),
                        message_id: None,
                        signal_name: None,
                        object_name: Some(object_name),
                        value,
                        source: Some(SourceSpan::new(tokens[0].line, tokens[0].column)),
                    });
                }
            }
        }
        _ if tokens.len() == 3
            && !matches!(
                tokens[2].text.as_str(),
                "BO_" | "SG_" | "EV_" | "BU_" | "NS_" | "ENVVAR_"
            ) =>
        {
            let value = tokens[2].text.clone();
            database.set_attribute(name.clone(), value.clone());
            database.add_attribute_assignment(DbcAttributeAssignment {
                name,
                scope: "global".to_owned(),
                message_id: None,
                signal_name: None,
                object_name: None,
                value,
                source: Some(SourceSpan::new(tokens[0].line, tokens[0].column)),
            });
        }
        other => {
            return Err(error(
                path,
                tokens.get(2),
                "BA_",
                format!("unsupported attribute scope {:?}", other),
            ));
        }
    }
    Ok(())
}

fn require_quoted_token(path: &str, token: Option<&Token>, field: &str) -> DatabaseResult<()> {
    match token {
        Some(token) if token.quoted => Ok(()),
        Some(token) => Err(error(path, Some(token), field, "expected a quoted name")),
        None => Err(error(path, None, field, "missing quoted name")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttributeScope {
    Global,
    Message,
    Signal,
    Node,
    Environment,
    SignalType,
    Relationship(String),
}

fn attribute_scope_name(scope: &AttributeScope) -> (&'static str, Option<&str>) {
    match scope {
        AttributeScope::Global => ("global", None),
        AttributeScope::Message => ("message", None),
        AttributeScope::Signal => ("signal", None),
        AttributeScope::Node => ("node", None),
        AttributeScope::Environment => ("environment", None),
        AttributeScope::SignalType => ("signal_type", None),
        AttributeScope::Relationship(relationship) => ("relationship", Some(relationship)),
    }
}

fn parse_attribute_definition(
    path: &str,
    tokens: &[Token],
    database: &mut DbcDatabase,
    attribute_definitions: &mut HashMap<String, AttributeScope>,
    scope: AttributeScope,
    name_index: usize,
    type_index: usize,
) -> DatabaseResult<()> {
    let keyword = tokens
        .first()
        .map(|token| token.text.as_str())
        .unwrap_or("BA_DEF_");
    let name = tokens.get(name_index).ok_or_else(|| {
        error(
            path,
            tokens.last(),
            keyword,
            "attribute definition is missing its name",
        )
    })?;
    require_quoted_token(path, Some(name), keyword)?;
    let value_type = tokens.get(type_index).ok_or_else(|| {
        error(
            path,
            tokens.last(),
            keyword,
            "attribute definition is missing its type",
        )
    })?;
    if value_type.quoted {
        return Err(error(
            path,
            Some(value_type),
            keyword,
            "attribute definition type must be unquoted",
        ));
    }

    let raw_arguments = tokens[type_index + 1..]
        .iter()
        .map(|token| token.text.clone())
        .collect::<Vec<_>>();
    let (minimum, maximum, enum_values) = match value_type.text.as_str() {
        "INT" | "HEX" | "FLOAT" => {
            if raw_arguments.len() != 2 {
                return Err(error(
                    path,
                    tokens.last(),
                    keyword,
                    format!(
                        "{} attribute definition must contain minimum and maximum",
                        value_type.text
                    ),
                ));
            }
            (
                Some(raw_arguments[0].clone()),
                Some(raw_arguments[1].clone()),
                Vec::new(),
            )
        }
        "ENUM" => {
            let mut values = Vec::new();
            for token in &tokens[type_index + 1..] {
                if token.text != "," {
                    if !token.quoted {
                        return Err(error(
                            path,
                            Some(token),
                            keyword,
                            "ENUM attribute values must be quoted",
                        ));
                    }
                    values.push(token.text.clone());
                }
            }
            (None, None, values)
        }
        "STRING" => {
            if !raw_arguments.is_empty() {
                return Err(error(
                    path,
                    tokens.last(),
                    keyword,
                    "STRING attribute definition must not have arguments",
                ));
            }
            (None, None, Vec::new())
        }
        _ => (None, None, Vec::new()),
    };
    let (scope_name, relationship) = attribute_scope_name(&scope);
    database.set_attribute_definition(DbcAttributeDefinition {
        name: name.text.clone(),
        scope: scope_name.to_owned(),
        relationship: relationship.map(str::to_owned),
        value_type: value_type.text.clone(),
        minimum,
        maximum,
        enum_values,
        raw_arguments,
        source: Some(SourceSpan::new(tokens[0].line, tokens[0].column)),
    });
    attribute_definitions.insert(name.text.clone(), scope);
    Ok(())
}

fn parse_standard_metadata(
    path: &str,
    tokens: &[Token],
    database: &mut DbcDatabase,
    attribute_definitions: &mut HashMap<String, AttributeScope>,
) -> DatabaseResult<()> {
    let keyword = tokens
        .first()
        .map(|token| token.text.as_str())
        .unwrap_or("DBC");
    if tokens.len() < 2 {
        return Err(error(
            path,
            tokens.first(),
            keyword,
            "unsupported or incomplete metadata statement",
        ));
    }
    match keyword {
        "BA_DEF_" => {
            let scope = match tokens.get(1).map(|token| token.text.as_str()) {
                Some("BO_") => AttributeScope::Message,
                Some("SG_") => AttributeScope::Signal,
                Some("BU_") => AttributeScope::Node,
                Some("EV_") => AttributeScope::Environment,
                Some(value) if tokens.get(1).is_some_and(|token| token.quoted) => {
                    let _ = value;
                    AttributeScope::Global
                }
                _ => {
                    return Err(error(
                        path,
                        tokens.get(1),
                        keyword,
                        "attribute definition has an invalid scope",
                    ));
                }
            };
            let (name_index, type_index) = if tokens.get(1).is_some_and(|token| token.quoted) {
                (1, 2)
            } else {
                (2, 3)
            };
            parse_attribute_definition(
                path,
                tokens,
                database,
                attribute_definitions,
                scope,
                name_index,
                type_index,
            )?;
        }
        "BA_DEF_DEF_" | "BA_DEF_DEF_REL_" => {
            require_quoted_token(path, tokens.get(1), keyword)?;
            if tokens.len() < 3 {
                return Err(error(
                    path,
                    tokens.last(),
                    keyword,
                    "unsupported or incomplete attribute default",
                ));
            }
            if keyword == "BA_DEF_DEF_REL_" {
                database.set_relationship_attribute_default(
                    tokens[1].text.clone(),
                    tokens[2].text.clone(),
                );
            } else {
                database.set_attribute_default(tokens[1].text.clone(), tokens[2].text.clone());
            }
        }
        "BA_DEF_REL_" => {
            let relationship = tokens.get(1).filter(|token| !token.quoted).ok_or_else(|| {
                error(
                    path,
                    tokens.get(1),
                    keyword,
                    "relationship attribute definition is missing its relationship scope",
                )
            })?;
            parse_attribute_definition(
                path,
                tokens,
                database,
                attribute_definitions,
                AttributeScope::Relationship(relationship.text.clone()),
                2,
                3,
            )?;
        }
        "BA_DEF_SGTYPE_" => {
            let (name_index, type_index) =
                if tokens.get(1).is_some_and(|token| token.text == "SGTYPE_") {
                    (2, 3)
                } else {
                    (1, 2)
                };
            parse_attribute_definition(
                path,
                tokens,
                database,
                attribute_definitions,
                AttributeScope::SignalType,
                name_index,
                type_index,
            )?;
        }
        "BA_REL_" => {
            require_quoted_token(path, tokens.get(1), keyword)?;
            if tokens.len() < 8 {
                return Err(error(
                    path,
                    tokens.last(),
                    keyword,
                    "relationship attribute assignment is incomplete",
                ));
            }
            let relationship = tokens.get(2).ok_or_else(|| {
                error(
                    path,
                    tokens.last(),
                    keyword,
                    "relationship attribute assignment is missing its relationship scope",
                )
            })?;
            match attribute_definitions.get(&tokens[1].text) {
                Some(AttributeScope::Relationship(expected)) if expected == &relationship.text => {}
                Some(_) => {
                    return Err(error(
                        path,
                        Some(&tokens[1]),
                        keyword,
                        "attribute is not defined as a relationship attribute",
                    ));
                }
                None => {
                    return Err(error(
                        path,
                        Some(&tokens[1]),
                        keyword,
                        "relationship attribute is not defined",
                    ));
                }
            }
            if relationship.text == "BU_SG_REL_" {
                if tokens.get(4).map(|token| token.text.as_str()) != Some("SG_") {
                    return Err(error(
                        path,
                        tokens.get(4),
                        keyword,
                        "BU_SG_REL_ assignment must reference SG_",
                    ));
                }
                let id = message_id(
                    path,
                    tokens.get(5).ok_or_else(|| {
                        error(
                            path,
                            tokens.last(),
                            keyword,
                            "relationship is missing message id",
                        )
                    })?,
                )?;
                let signal_name = tokens.get(6).ok_or_else(|| {
                    error(
                        path,
                        tokens.last(),
                        keyword,
                        "relationship is missing signal name",
                    )
                })?;
                if database.message_by_id(id.id, id.extended).is_none() {
                    return Err(error(
                        path,
                        Some(&tokens[5]),
                        keyword,
                        format!("unknown CAN message id 0x{:X}", id.id),
                    ));
                }
                let message = database
                    .message_by_id(id.id, id.extended)
                    .expect("message was checked above");
                if message.signal(&signal_name.text).is_none() {
                    return Err(error(
                        path,
                        Some(signal_name),
                        keyword,
                        format!("unknown CAN signal {:?}", signal_name.text),
                    ));
                }
                database.add_relationship_attribute(DbcRelationshipAttribute {
                    name: tokens[1].text.clone(),
                    relationship: relationship.text.clone(),
                    source_node: tokens[3].text.clone(),
                    message_id: id,
                    signal_name: signal_name.text.clone(),
                    value: tokens[7].text.clone(),
                    source: Some(SourceSpan::new(tokens[0].line, tokens[0].column)),
                })?;
            }
        }
        "BO_TX_BU_" => {
            if tokens.len() < 4 {
                return Err(error(
                    path,
                    tokens.first(),
                    keyword,
                    "unsupported or incomplete transmitter list",
                ));
            }
            let id = message_id(path, &tokens[1])?;
            expect(path, tokens, 2, ":", keyword)?;
            validate_name_list(path, &tokens[3..], keyword)?;
            let transmitters = tokens[3..]
                .iter()
                .filter(|token| token.text != ",")
                .map(|token| token.text.as_str())
                .collect::<Vec<_>>();
            database.set_message_transmitter(id, transmitters.join(","))?;
        }
        "SIG_GROUP_" => {
            if tokens.len() < 6 {
                return Err(error(
                    path,
                    tokens.first(),
                    keyword,
                    "unsupported or incomplete signal group definition",
                ));
            }
            if !tokens.iter().any(|token| token.text == ":") {
                return Err(error(
                    path,
                    tokens.last(),
                    keyword,
                    "unsupported signal group definition without ':'",
                ));
            }
            let id = message_id(path, &tokens[1])?;
            let group_name = &tokens[2];
            if group_name.quoted || group_name.text == ":" {
                return Err(error(
                    path,
                    Some(group_name),
                    keyword,
                    "signal group name must be an unquoted identifier",
                ));
            }
            let repetition = parse_u32(path, &tokens[3], "repetition")?;
            expect(path, tokens, 4, ":", keyword)?;
            let signal_tokens = &tokens[5..];
            if signal_tokens.is_empty() {
                return Err(error(
                    path,
                    tokens.last(),
                    keyword,
                    "signal group must contain at least one signal",
                ));
            }
            let message = database.message_by_id(id.id, id.extended).ok_or_else(|| {
                error(
                    path,
                    Some(&tokens[1]),
                    keyword,
                    format!("unknown CAN message id 0x{:X}", id.id),
                )
            })?;
            let mut signal_names = Vec::with_capacity(signal_tokens.len());
            for signal in signal_tokens {
                if signal.quoted || signal.text == "," {
                    return Err(error(
                        path,
                        Some(signal),
                        keyword,
                        "signal group members must be unquoted signal names",
                    ));
                }
                if message.signal(&signal.text).is_none() {
                    return Err(error(
                        path,
                        Some(signal),
                        keyword,
                        format!("unknown CAN signal {:?}", signal.text),
                    ));
                }
                signal_names.push(signal.text.clone());
            }
            database.add_signal_group(DbcSignalGroup {
                message_id: id,
                name: group_name.text.clone(),
                repetition,
                signal_names,
                source: Some(SourceSpan::new(tokens[0].line, tokens[0].column)),
            })?;
        }
        _ => {
            return Err(error(
                path,
                tokens.first(),
                keyword,
                "unsupported DBC metadata statement",
            ));
        }
    }
    Ok(())
}

fn validate_name_list(path: &str, tokens: &[Token], field: &str) -> DatabaseResult<()> {
    let mut saw_name = false;
    let mut expect_name = true;
    for token in tokens {
        if token.text == "," {
            if !saw_name || expect_name {
                return Err(error(
                    path,
                    Some(token),
                    field,
                    "unexpected comma in name list",
                ));
            }
            expect_name = true;
        } else {
            if token.quoted {
                return Err(error(path, Some(token), field, "invalid name list"));
            }
            // DBC files in the wild use both `A,B` and `A B` for this list.
            // Whitespace is already discarded by the lexer, so a name after
            // another name is a valid whitespace-separated entry.
            saw_name = true;
            expect_name = false;
        }
    }
    if !saw_name || expect_name {
        return Err(error(
            path,
            tokens.last(),
            field,
            "name list cannot end with a comma",
        ));
    }
    Ok(())
}

fn parse_u32_text(path: &str, token: &Token, text: &str, field: &str) -> DatabaseResult<u32> {
    let synthetic = Token {
        text: text.to_string(),
        line: token.line,
        column: token.column,
        quoted: false,
    };
    parse_u32(path, &synthetic, field)
}

fn parse_multiplex_ranges(
    path: &str,
    tokens: &[Token],
    field: &str,
) -> DatabaseResult<Vec<(u32, u32)>> {
    if tokens.is_empty() {
        return Err(error(path, None, field, "missing multiplex value range"));
    }
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        if tokens[index].text == "," {
            return Err(error(
                path,
                Some(&tokens[index]),
                field,
                "unexpected comma in multiplex value range",
            ));
        }
        let token = &tokens[index];
        let (start, end, consumed) = if let Some((start, end)) = token.text.split_once('-') {
            if start.is_empty() || end.is_empty() {
                return Err(error(
                    path,
                    Some(token),
                    field,
                    "invalid multiplex value range",
                ));
            }
            (
                parse_u32_text(path, token, start, field)?,
                parse_u32_text(path, token, end, field)?,
                1,
            )
        } else {
            let start = parse_u32(path, token, field)?;
            if tokens.get(index + 1).is_some_and(|value| value.text == "-") {
                let end_token = tokens.get(index + 2).ok_or_else(|| {
                    error(
                        path,
                        Some(token),
                        field,
                        "multiplex range is missing its end",
                    )
                })?;
                (start, parse_u32(path, end_token, field)?, 3)
            } else {
                (start, start, 1)
            }
        };
        if end < start {
            return Err(error(
                path,
                Some(token),
                field,
                "multiplex range end precedes its start",
            ));
        }
        ranges.push((start, end));
        index += consumed;
        if index < tokens.len() {
            if tokens[index].text != "," {
                return Err(error(
                    path,
                    Some(&tokens[index]),
                    field,
                    "multiplex ranges must be comma-separated",
                ));
            }
            index += 1;
            if index == tokens.len() {
                return Err(error(
                    path,
                    tokens.last(),
                    field,
                    "multiplex range list cannot end with a comma",
                ));
            }
        }
    }
    Ok(ranges)
}

fn parse_sg_mul_val(
    path: &str,
    tokens: &[Token],
    database: &mut DbcDatabase,
) -> DatabaseResult<()> {
    if tokens.len() < 5 {
        return Err(error(
            path,
            tokens.first(),
            "SG_MUL_VAL_",
            "statement must contain message id, signal, selector, and range",
        ));
    }
    let id = message_id(path, &tokens[1])?;
    let ranges = parse_multiplex_ranges(path, &tokens[4..], "SG_MUL_VAL_")?;
    database.add_multiplex_ranges(id, &tokens[2].text, &tokens[3].text, ranges)
}

fn is_can_fd_format(value: &str) -> bool {
    value.eq_ignore_ascii_case("CAN FD")
        || value.eq_ignore_ascii_case("StandardCAN_FD")
        || value.eq_ignore_ascii_case("ExtendedCAN_FD")
        || value == "2"
        || value == "14"
        || value == "15"
}

fn parse_vframe_format(path: &str, token: &Token) -> DatabaseResult<bool> {
    let value = token.text.as_str();
    if value.eq_ignore_ascii_case("CAN FD")
        || value.eq_ignore_ascii_case("StandardCAN_FD")
        || value.eq_ignore_ascii_case("ExtendedCAN_FD")
    {
        return Ok(true);
    }
    if value.eq_ignore_ascii_case("StandardCAN")
        || value.eq_ignore_ascii_case("ExtendedCAN")
        || value.eq_ignore_ascii_case("reserved")
        || value.eq_ignore_ascii_case("J1939PG")
    {
        return Ok(false);
    }
    if let Ok(number) = value.parse::<u8>() {
        if number <= 15 {
            return Ok(is_can_fd_format(value));
        }
    }
    Err(error(
        path,
        Some(token),
        "VFrameFormat",
        format!("unsupported VFrameFormat value {:?}", value),
    ))
}

fn parse_line(
    path: &str,
    line: &str,
    line_number: usize,
    current_message: Option<CanMessageId>,
    database: &mut DbcDatabase,
    attribute_definitions: &mut HashMap<String, AttributeScope>,
) -> DatabaseResult<()> {
    let content = without_line_comment(line);
    let mut tokens = lex_line(path, content, line_number)?;
    if tokens.last().is_some_and(|token| token.text == ";") {
        tokens.pop();
    }
    if tokens.is_empty() {
        return Ok(());
    }
    match tokens[0].text.as_str() {
        "VERSION" => {
            if tokens.len() != 2 {
                return Err(error(
                    path,
                    tokens.get(2).or_else(|| tokens.first()),
                    "VERSION",
                    "version declaration must contain exactly one quoted value",
                ));
            }
            let version = tokens
                .get(1)
                .ok_or_else(|| error(path, tokens.first(), "VERSION", "missing version string"))?;
            if !version.quoted {
                return Err(error(
                    path,
                    Some(version),
                    "VERSION",
                    "version must be quoted",
                ));
            }
            database.version = Some(version.text.clone());
            Ok(())
        }
        "BO_" => parse_bo(path, &tokens, database),
        "SG_" => parse_signal(
            path,
            &tokens,
            current_message.ok_or_else(|| {
                error(
                    path,
                    tokens.first(),
                    "SG_",
                    "signal appears before a message",
                )
            })?,
            database,
        ),
        "VAL_" => parse_val(path, &tokens, database),
        "CM_" => parse_comment(path, &tokens, database),
        "BA_" => parse_attribute(path, &tokens, database, attribute_definitions),
        "BS_" => {
            if tokens.len() != 2 || tokens[1].text != ":" {
                return Err(error(
                    path,
                    tokens.get(1).or_else(|| tokens.first()),
                    "BS_",
                    "bus-speed declaration must be exactly `BS_ :`",
                ));
            }
            Ok(())
        }
        "BU_" => {
            if tokens.get(1).is_none_or(|token| token.text != ":")
                || tokens[2..].iter().any(|token| token.text == ":")
            {
                return Err(error(
                    path,
                    tokens.get(1).or_else(|| tokens.first()),
                    "BU_",
                    "node declaration must start with `BU_ :` and contain no extra separators",
                ));
            }
            if tokens.len() == 2 {
                return Err(error(
                    path,
                    tokens.last(),
                    "BU_",
                    "node declaration must contain at least one node",
                ));
            }
            for node in &tokens[2..] {
                if node.quoted || node.text == "," {
                    return Err(error(
                        path,
                        Some(node),
                        "BU_",
                        "node name must be an unquoted token",
                    ));
                }
                database.add_node(DbcNode {
                    name: node.text.clone(),
                    source: Some(SourceSpan::new(node.line, node.column)),
                })?;
            }
            Ok(())
        }
        "BA_DEF_" | "BA_DEF_DEF_" | "BA_DEF_REL_" | "BA_DEF_DEF_REL_" | "BA_DEF_SGTYPE_"
        | "BA_REL_" | "BO_TX_BU_" | "SIG_GROUP_" => {
            parse_standard_metadata(path, &tokens, database, attribute_definitions)
        }
        "VAL_TABLE_" => parse_value_table(path, &tokens, database),
        "SIG_VALTYPE_" => parse_signal_value_type(path, &tokens, database),
        "SIGTYPE_VALTYPE_" => parse_signal_type_value_type(path, &tokens, database),
        "SG_MUL_VAL_" => parse_sg_mul_val(path, &tokens, database),
        other => Err(error(
            path,
            tokens.first(),
            other,
            "unsupported DBC statement",
        )),
    }
}

impl DbcDatabase {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(source: &str) -> DatabaseResult<Self> {
        Self::from_source("<memory>", source)
    }

    pub fn from_file(path: impl AsRef<Path>) -> DatabaseResult<Self> {
        let path_ref = path.as_ref();
        let source = fs::read(path_ref).map_err(|value| {
            DatabaseError::new(path_ref.display().to_string(), 0, "file", value.to_string())
        })?;
        Self::from_source(path_ref.display().to_string(), &decode_source(source))
    }

    pub fn from_source(path: impl Into<String>, source: &str) -> DatabaseResult<Self> {
        let path = path.into();
        let mut database = DbcDatabase::with_source_path(path.clone());
        let mut current_message = None;
        let mut in_namespace_declaration = false;
        let mut logical_line = String::new();
        let mut logical_start = 0;
        let mut attribute_definitions = HashMap::new();
        for (line_index, line) in source.lines().enumerate() {
            if logical_line.is_empty() {
                logical_start = line_index + 1;
            } else {
                logical_line.push('\n');
            }
            logical_line.push_str(line);
            if quote_is_open(without_line_comment(&logical_line)) {
                continue;
            }
            let line = std::mem::take(&mut logical_line);
            let content = without_line_comment(&line);
            let tokens = lex_line(&path, content, line_index + 1)?;
            let first_token = tokens.first().map(|token| token.text.as_str());
            if in_namespace_declaration {
                if first_token == Some("BS_") {
                    in_namespace_declaration = false;
                }
                continue;
            }
            if first_token == Some("NS_") {
                in_namespace_declaration = true;
                continue;
            }
            if first_token == Some("BO_") {
                current_message = Some(message_id(
                    &path,
                    tokens
                        .get(1)
                        .ok_or_else(|| error(&path, tokens.first(), "BO_", "missing message id"))?,
                )?);
            }
            parse_line(
                &path,
                &line,
                logical_start,
                current_message,
                &mut database,
                &mut attribute_definitions,
            )?;
        }
        if !logical_line.is_empty() {
            let content = without_line_comment(&logical_line);
            let tokens = lex_line(&path, content, logical_start)?;
            if !tokens.is_empty() {
                parse_line(
                    &path,
                    &logical_line,
                    logical_start,
                    current_message,
                    &mut database,
                    &mut attribute_definitions,
                )?;
            }
        }
        if in_namespace_declaration {
            return Err(error(
                &path,
                None,
                "NS_",
                "namespace declaration is not terminated by BS_",
            ));
        }
        Ok(database)
    }
}
