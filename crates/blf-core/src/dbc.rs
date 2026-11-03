//! Small, dependency-free DBC parser.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::database::{
    ByteOrder, CanMessageId, CanPayloadKind, CanSignal, DatabaseError, DatabaseResult, DbcDatabase,
    DbcMessage, MultiplexRole, SignalType, SourceSpan, ValueTable,
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
    token.text.parse().map_err(|_| {
        error(
            path,
            Some(token),
            field,
            format!("invalid number {:?}", token.text),
        )
    })
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
        // Vector's logical-container convention uses an extended identifier
        // together with an application-level DLC above the CAN-FD wire limit.
        // Keep ordinary oversized definitions physical so malformed DBC data
        // cannot bypass payload validation by number alone.
        payload_kind: if id.extended && dlc > 64 {
            CanPayloadKind::LogicalContainer
        } else {
            CanPayloadKind::Physical
        },
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
    index += 1;
    expect(path, tokens, index, "]", "SG_")?;
    index += 1;
    let unit = tokens
        .get(index)
        .ok_or_else(|| error(path, tokens.last(), "unit", "missing unit"))?;
    index += 1;
    let receiver = tokens
        .get(index)
        .ok_or_else(|| error(path, tokens.last(), "receiver", "missing receiver"))?;
    let _ = unit;
    if receiver.quoted || receiver.text == "," {
        return Err(error(
            path,
            Some(receiver),
            "receiver",
            "receiver must be an ECU name",
        ));
    }
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
    let mut signal = CanSignal::new(name, start_bit, bit_length, byte_order, signal_type);
    signal.factor = factor;
    signal.offset = offset;
    signal.minimum = Some(minimum);
    signal.maximum = Some(maximum);
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
            }
            database.set_message_attribute(id, name.clone(), value.clone())?;
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
                    database.set_signal_attribute(id, &tokens[4].text, name, value)?;
                }
                scope => database.set_scoped_attribute(scope, &tokens[3].text, name, value),
            }
        }
        _ if tokens.len() == 3
            && !matches!(
                tokens[2].text.as_str(),
                "BO_" | "SG_" | "EV_" | "BU_" | "NS_" | "ENVVAR_"
            ) =>
        {
            database.set_attribute(name, tokens[2].text.clone());
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
    Relationship(String),
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
            let quoted = tokens.iter().position(|token| token.quoted);
            require_quoted_token(path, quoted.and_then(|index| tokens.get(index)), keyword)?;
            if quoted.is_none_or(|index| index + 1 >= tokens.len()) {
                return Err(error(
                    path,
                    tokens.last(),
                    keyword,
                    "attribute definition is missing its type",
                ));
            }
            if let Some(index) = quoted {
                attribute_definitions.insert(tokens[index].text.clone(), scope);
            }
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
            let quoted = tokens.iter().position(|token| token.quoted);
            require_quoted_token(path, quoted.and_then(|index| tokens.get(index)), keyword)?;
            if quoted.is_none_or(|index| index + 1 >= tokens.len()) {
                return Err(error(
                    path,
                    tokens.last(),
                    keyword,
                    "unsupported or incomplete relationship attribute definition",
                ));
            }
            if let Some(index) = quoted {
                attribute_definitions.insert(
                    tokens[index].text.clone(),
                    AttributeScope::Relationship(relationship.text.clone()),
                );
            }
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
            parse_u32(path, &tokens[1], "message_id")?;
            expect(path, tokens, 2, ":", keyword)?;
            validate_name_list(path, &tokens[3..], keyword)?;
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
    let mut expect_name = true;
    for token in tokens {
        if token.text == "," {
            if expect_name {
                return Err(error(
                    path,
                    Some(token),
                    field,
                    "unexpected comma in name list",
                ));
            }
            expect_name = true;
        } else {
            if token.quoted || !expect_name {
                return Err(error(path, Some(token), field, "invalid name list"));
            }
            expect_name = false;
        }
    }
    if expect_name {
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
            Ok(())
        }
        "BA_DEF_" | "BA_DEF_DEF_" | "BA_DEF_REL_" | "BA_DEF_DEF_REL_" | "BA_REL_" | "BO_TX_BU_"
        | "SIG_GROUP_" => parse_standard_metadata(path, &tokens, database, attribute_definitions),
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
