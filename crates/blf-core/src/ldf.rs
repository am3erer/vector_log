//! Dependency-free LIN Description File parser and signal decoder.
//!
//! LDFs are intentionally parsed here instead of delegated to a Python
//! package.  The parser accepts the common LIN 2.x grammar while preserving
//! source locations for diagnostics and keeping supported optional sections
//! visible for callers.

use std::collections::{hash_map::Entry, HashMap};
use std::fs;
use std::path::Path;

use thiserror::Error;

use crate::database::{
    DatabaseError, DatabaseResult, LdfDatabase, LdfOptionalSection, LinChecksum, LinEncoding,
    LinFrame, LinNode, LinPhysicalRange, LinScheduleEntry, LinSignal, SourceSpan, ValueTable,
};
use crate::lin::{classify_lin_object, LinObservation, LinResponseState};
use crate::types::LinObject;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum LdfParseError {
    #[error("{0}")]
    Database(#[from] DatabaseError),
    #[error("invalid LIN identifier 0x{identifier:02X}: {reason}")]
    InvalidIdentifier { identifier: u8, reason: String },
    #[error("LIN payload is too short: required {required} bytes, got {actual}")]
    PayloadTooShort { required: usize, actual: usize },
    #[error("LIN signal {signal:?} is outside the payload")]
    SignalOutOfBounds { signal: String },
    #[error("LIN signal {signal:?} has unsupported length {bit_length}")]
    UnsupportedBitLength { signal: String, bit_length: u16 },
    #[error("LIN frame 0x{frame_id:02X} is not present in the database")]
    UnknownFrame { frame_id: u8 },
    #[error("LIN response does not contain a frame identifier")]
    MissingFrameIdentifier,
    #[error("LIN checksum mismatch: expected 0x{expected:02X}, got 0x{actual:02X}")]
    ChecksumMismatch { expected: u8, actual: u8 },
    #[error("LIN response is not decodable: {state:?}")]
    InvalidResponse { state: LinResponseState },
}

pub type LinDecodeError = LdfParseError;

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedLinSignal {
    pub signal_name: String,
    pub raw_value: i128,
    pub physical_value: f64,
    pub value_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct Token {
    text: String,
    line: usize,
    column: usize,
    quoted: bool,
}

fn db_error(
    path: &str,
    token: Option<&Token>,
    field: &str,
    reason: impl Into<String>,
) -> DatabaseError {
    DatabaseError::new(path, token.map_or(0, |token| token.line), field, reason)
}

fn lex(path: &str, source: &str) -> DatabaseResult<Vec<Token>> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    let mut line = 1;
    let mut column = 1;
    while index < chars.len() {
        let current = chars[index];
        if current.is_whitespace() {
            if current == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
            index += 1;
            continue;
        }
        if current == '/' && chars.get(index + 1) == Some(&'/') {
            index += 2;
            column += 2;
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
                column += 1;
            }
            continue;
        }
        if current == '/' && chars.get(index + 1) == Some(&'*') {
            index += 2;
            column += 2;
            let mut closed = false;
            while index < chars.len() {
                if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
                    index += 2;
                    column += 2;
                    closed = true;
                    break;
                }
                if chars[index] == '\n' {
                    line += 1;
                    column = 1;
                } else {
                    column += 1;
                }
                index += 1;
            }
            if !closed {
                return Err(db_error(
                    path,
                    None,
                    "comment",
                    "unterminated block comment",
                ));
            }
            continue;
        }
        let token_line = line;
        let token_column = column;
        if current == '"' {
            index += 1;
            column += 1;
            let mut value = String::new();
            let mut closed = false;
            while index < chars.len() {
                let character = chars[index];
                if character == '"' {
                    index += 1;
                    column += 1;
                    closed = true;
                    break;
                }
                if character == '\\' && index + 1 < chars.len() {
                    let escaped = chars[index + 1];
                    value.push(match escaped {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        other => other,
                    });
                    index += 2;
                    column += 2;
                } else {
                    value.push(character);
                    index += 1;
                    column += 1;
                }
            }
            if !closed {
                return Err(db_error(path, None, "string", "unterminated quoted string"));
            }
            tokens.push(Token {
                text: value,
                line: token_line,
                column: token_column,
                quoted: true,
            });
            continue;
        }
        if "{}:,;=()[]".contains(current) {
            tokens.push(Token {
                text: current.to_string(),
                line: token_line,
                column: token_column,
                quoted: false,
            });
            index += 1;
            column += 1;
            continue;
        }
        let start = index;
        while index < chars.len()
            && !chars[index].is_whitespace()
            && !"{}:,;=()[]\"".contains(chars[index])
            && !(chars[index] == '/' && chars.get(index + 1) == Some(&'/'))
        {
            index += 1;
            column += 1;
        }
        tokens.push(Token {
            text: chars[start..index].iter().collect(),
            line: token_line,
            column: token_column,
            quoted: false,
        });
    }
    Ok(tokens)
}

fn parse_u32(path: &str, token: &Token, field: &str) -> DatabaseResult<u32> {
    let text = token.text.trim();
    let value = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .map_or_else(|| text.parse::<u32>(), |hex| u32::from_str_radix(hex, 16));
    value.map_err(|_| {
        db_error(
            path,
            Some(token),
            field,
            format!("invalid integer {:?}", token.text),
        )
    })
}

fn parse_f64(path: &str, token: &Token, field: &str) -> DatabaseResult<f64> {
    token.text.parse().map_err(|_| {
        db_error(
            path,
            Some(token),
            field,
            format!("invalid number {:?}", token.text),
        )
    })
}

fn parse_u8(path: &str, token: &Token, field: &str) -> DatabaseResult<u8> {
    let value = parse_u32(path, token, field)?;
    u8::try_from(value).map_err(|_| db_error(path, Some(token), field, "value is outside 0..255"))
}

fn expect(
    tokens: &[Token],
    index: &mut usize,
    text: &str,
    path: &str,
    field: &str,
) -> DatabaseResult<Token> {
    let token = tokens
        .get(*index)
        .cloned()
        .ok_or_else(|| db_error(path, tokens.last(), field, format!("expected {:?}", text)))?;
    if token.text != text {
        return Err(db_error(
            path,
            Some(&token),
            field,
            format!("expected {:?}, got {:?}", text, token.text),
        ));
    }
    *index += 1;
    Ok(token)
}

fn next<'a>(
    tokens: &'a [Token],
    index: &mut usize,
    path: &str,
    field: &str,
) -> DatabaseResult<&'a Token> {
    let token = tokens
        .get(*index)
        .ok_or_else(|| db_error(path, tokens.last(), field, "unexpected end of statement"))?;
    *index += 1;
    Ok(token)
}

fn statement_tokens(tokens: &[Token], index: &mut usize, path: &str) -> DatabaseResult<Vec<Token>> {
    let mut result = Vec::new();
    while *index < tokens.len() {
        let token = tokens[*index].clone();
        *index += 1;
        if token.text == ";" {
            return Ok(result);
        }
        result.push(token);
    }
    Err(db_error(path, result.last(), "statement", "missing ';'"))
}

fn parse_nodes(
    path: &str,
    tokens: &[Token],
    index: &mut usize,
    database: &mut LdfDatabase,
) -> DatabaseResult<()> {
    expect(tokens, index, "{", path, "Nodes")?;
    while *index < tokens.len() && tokens[*index].text != "}" {
        let statement = statement_tokens(tokens, index, path)?;
        let significant: Vec<&Token> = statement.iter().filter(|token| token.text != ",").collect();
        if significant.len() < 2 {
            return Err(db_error(
                path,
                statement.first(),
                "Nodes",
                "node declaration is incomplete",
            ));
        }
        let kind = significant[0];
        if significant[1].text != ":" {
            return Err(db_error(
                path,
                Some(significant[1]),
                "Nodes",
                "expected ':'",
            ));
        }
        match kind.text.as_str() {
            "Master" => {
                let name = significant
                    .get(2)
                    .ok_or_else(|| db_error(path, Some(kind), "Master", "missing master name"))?;
                database.nodes.push(LinNode {
                    name: name.text.clone(),
                    configured_nad: None,
                    source: Some(SourceSpan::new(name.line, name.column)),
                });
            }
            "Slaves" => {
                for name in significant.iter().skip(2) {
                    if name.text == "ms" {
                        break;
                    }
                    database.nodes.push(LinNode {
                        name: name.text.clone(),
                        configured_nad: None,
                        source: Some(SourceSpan::new(name.line, name.column)),
                    });
                }
            }
            _ => {
                return Err(db_error(
                    path,
                    Some(kind),
                    "Nodes",
                    "unsupported node declaration",
                ))
            }
        }
    }
    expect(tokens, index, "}", path, "Nodes")?;
    Ok(())
}

fn parse_signal_definition(
    path: &str,
    statement: &[Token],
    pending: &mut HashMap<String, LinSignal>,
) -> DatabaseResult<()> {
    let mut index = 0;
    let name = next(statement, &mut index, path, "Signals")?;
    expect(statement, &mut index, ":", path, "Signals")?;
    let bit_length = parse_u32(
        path,
        next(statement, &mut index, path, "bit_length")?,
        "bit_length",
    )?;
    if bit_length == 0 || bit_length > 64 {
        return Err(db_error(
            path,
            statement.get(index.saturating_sub(1)),
            "bit_length",
            "LIN signal bit length must be between 1 and 64",
        ));
    }
    expect(statement, &mut index, ",", path, "Signals")?;
    let initial_token = next(statement, &mut index, path, "initial_value")?;
    let initial_value = if initial_token.text == "{" {
        let value = parse_u32(
            path,
            next(statement, &mut index, path, "initial_value")?,
            "initial_value",
        )?;
        while statement.get(index).is_some_and(|token| token.text != "}") {
            index += 1;
        }
        expect(statement, &mut index, "}", path, "initial_value")?;
        value
    } else {
        parse_u32(path, initial_token, "initial_value")?
    };
    expect(statement, &mut index, ",", path, "Signals")?;
    let publisher = next(statement, &mut index, path, "publisher")?;
    if publisher.quoted {
        return Err(db_error(
            path,
            Some(publisher),
            "publisher",
            "publisher must be a node name",
        ));
    }
    let mut subscribers = 0;
    while index < statement.len() {
        expect(statement, &mut index, ",", path, "Signals")?;
        let subscriber = next(statement, &mut index, path, "subscriber")?;
        if subscriber.quoted || subscriber.text == "," {
            return Err(db_error(
                path,
                Some(subscriber),
                "subscriber",
                "subscriber must be a node name",
            ));
        }
        subscribers += 1;
    }
    if subscribers == 0 {
        return Err(db_error(
            path,
            Some(publisher),
            "subscriber",
            "signal declaration requires at least one subscriber",
        ));
    }
    let signal = LinSignal {
        name: name.text.clone(),
        start_bit: 0,
        bit_length: u16::try_from(bit_length)
            .map_err(|_| db_error(path, Some(name), "bit_length", "outside u16"))?,
        initial_value: Some(i64::from(initial_value)),
        factor: 1.0,
        offset: 0.0,
        minimum: None,
        maximum: None,
        encoding: None,
        choices: ValueTable::default(),
        source: Some(SourceSpan::new(name.line, name.column)),
    };
    match pending.entry(name.text.clone()) {
        Entry::Vacant(entry) => {
            entry.insert(signal);
        }
        Entry::Occupied(_) => {
            return Err(db_error(
                path,
                Some(name),
                "Signals",
                format!("duplicate signal definition {:?}", name.text),
            ));
        }
    }
    Ok(())
}

fn parse_frame(
    path: &str,
    tokens: &[Token],
    index: &mut usize,
    database: &mut LdfDatabase,
    pending: &HashMap<String, LinSignal>,
) -> DatabaseResult<()> {
    let name = next(tokens, index, path, "Frames")?.clone();
    expect(tokens, index, ":", path, "Frames")?;
    let id_token = next(tokens, index, path, "frame_id")?;
    let id = parse_u8(path, id_token, "frame_id")?;
    if id > 0x3F {
        return Err(db_error(
            path,
            Some(id_token),
            "frame_id",
            format!("LIN frame identifier 0x{id:02X} exceeds the six-bit range"),
        ));
    }
    expect(tokens, index, ",", path, "Frames")?;
    let publisher = next(tokens, index, path, "publisher")?.clone();
    expect(tokens, index, ",", path, "Frames")?;
    let dlc_token = next(tokens, index, path, "dlc")?;
    let dlc = parse_u8(path, dlc_token, "dlc")?;
    if dlc > 8 {
        return Err(db_error(
            path,
            Some(dlc_token),
            "dlc",
            format!("LIN frame DLC {dlc} exceeds the maximum of 8 bytes"),
        ));
    }
    expect(tokens, index, "{", path, "Frames")?;
    let mut signals = Vec::new();
    while *index < tokens.len() && tokens[*index].text != "}" {
        let statement = statement_tokens(tokens, index, path)?;
        if statement.is_empty() {
            continue;
        }
        let significant: Vec<&Token> = statement.iter().filter(|token| token.text != ",").collect();
        if significant.len() < 2 {
            return Err(db_error(
                path,
                statement.first(),
                "frame_signal",
                "frame signal declaration requires a signal name and offset",
            ));
        }
        if significant.len() > 2 {
            return Err(db_error(
                path,
                significant.get(2).copied(),
                "frame_signal",
                "unexpected tokens after frame signal offset",
            ));
        }
        let signal_name = significant[0].text.clone();
        let offset = parse_u32(
            path,
            significant
                .get(1)
                .copied()
                .ok_or_else(|| db_error(path, statement.first(), "offset", "missing offset"))?,
            "offset",
        )?;
        let offset = u16::try_from(offset).map_err(|_| {
            db_error(
                path,
                significant.get(1).copied(),
                "offset",
                "signal offset is outside u16",
            )
        })?;
        let mut signal = pending.get(&signal_name).cloned().ok_or_else(|| {
            db_error(
                path,
                significant.first().copied(),
                "signal",
                format!(
                    "frame {:?} references undefined signal {:?}",
                    name.text, signal_name
                ),
            )
        })?;
        signal.start_bit = offset;
        signals.push(signal);
    }
    expect(tokens, index, "}", path, "Frames")?;
    database.insert_frame(LinFrame {
        id,
        name: name.text,
        dlc,
        publisher: publisher.text,
        // LIN diagnostic frames 0x3C/0x3D use the classic checksum. Other
        // ordinary frames use the enhanced checksum unless BLF metadata
        // provides an explicit model at decode time.
        checksum: if matches!(id, 0x3C | 0x3D) {
            LinChecksum::Classic
        } else {
            LinChecksum::Enhanced
        },
        signals,
        source: Some(SourceSpan::new(name.line, name.column)),
    })
}

fn parse_encoding(
    path: &str,
    tokens: &[Token],
    index: &mut usize,
    database: &mut LdfDatabase,
) -> DatabaseResult<()> {
    let name = next(tokens, index, path, "Signal_encoding_types")?.clone();
    if tokens.get(*index).is_some_and(|token| token.text == ":") {
        *index += 1;
    }
    expect(tokens, index, "{", path, "Signal_encoding_types")?;
    let mut logical = Vec::new();
    let mut ranges = Vec::new();
    while *index < tokens.len() && tokens[*index].text != "}" {
        let statement = statement_tokens(tokens, index, path)?;
        if statement.is_empty() {
            continue;
        }
        let significant: Vec<&Token> = statement.iter().filter(|token| token.text != ",").collect();
        let Some(kind_token) = significant.first().copied() else {
            return Err(db_error(
                path,
                statement.first(),
                "encoding",
                "empty encoding clause",
            ));
        };
        match kind_token.text.as_str() {
            "logical_value" => {
                if significant.len() != 3 {
                    return Err(db_error(
                        path,
                        Some(kind_token),
                        "logical_value",
                        "expected raw value and display text",
                    ));
                }
                let raw = i128::from(parse_u32(path, significant[1], "logical_value")?);
                logical.push((raw, significant[2].text.clone()));
            }
            "physical_value" => {
                if !(5..=7).contains(&significant.len()) {
                    return Err(db_error(
                        path,
                        Some(kind_token),
                        "physical_value",
                        "expected minimum, maximum, factor, offset, and optional unit/text",
                    ));
                }
                ranges.push(LinPhysicalRange {
                    minimum: parse_f64(path, significant[1], "minimum")?,
                    maximum: parse_f64(path, significant[2], "maximum")?,
                    factor: parse_f64(path, significant[3], "factor")?,
                    offset: parse_f64(path, significant[4], "offset")?,
                    unit: significant.get(5).map(|token| token.text.clone()),
                    text: significant.get(6).map(|token| token.text.clone()),
                });
            }
            other => {
                return Err(db_error(
                    path,
                    Some(kind_token),
                    "encoding",
                    format!("unsupported encoding clause {:?}", other),
                ));
            }
        }
    }
    expect(tokens, index, "}", path, "Signal_encoding_types")?;
    database.encodings.insert(
        name.text.clone(),
        LinEncoding {
            name: name.text,
            logical_values: ValueTable::new(logical),
            physical_ranges: ranges,
            source: Some(SourceSpan::new(name.line, name.column)),
        },
    );
    Ok(())
}

fn parse_schedule(
    path: &str,
    tokens: &[Token],
    index: &mut usize,
    database: &mut LdfDatabase,
) -> DatabaseResult<()> {
    let name = next(tokens, index, path, "Schedule_tables")?.clone();
    expect(tokens, index, "{", path, "Schedule_tables")?;
    let mut entries = Vec::new();
    while *index < tokens.len() && tokens[*index].text != "}" {
        let statement = statement_tokens(tokens, index, path)?;
        if statement.len() != 4
            || !statement[1].text.eq_ignore_ascii_case("delay")
            || !statement[3].text.eq_ignore_ascii_case("ms")
        {
            return Err(db_error(
                path,
                statement.first(),
                "Schedule_tables",
                "schedule entry must be '<frame> delay <milliseconds> ms'",
            ));
        }
        entries.push(LinScheduleEntry {
            frame_name: statement[0].text.clone(),
            delay_ms: parse_u32(path, &statement[2], "delay")?,
            source: Some(SourceSpan::new(statement[0].line, statement[0].column)),
        });
    }
    expect(tokens, index, "}", path, "Schedule_tables")?;
    database.schedules.insert(name.text, entries);
    Ok(())
}

fn parse_signal_representation(
    path: &str,
    tokens: &[Token],
    index: &mut usize,
    database: &mut LdfDatabase,
) -> DatabaseResult<()> {
    expect(tokens, index, "{", path, "Signal_representation")?;
    while *index < tokens.len() && tokens[*index].text != "}" {
        let statement = statement_tokens(tokens, index, path)?;
        let significant: Vec<&Token> = statement.iter().filter(|token| token.text != ",").collect();
        if significant.len() < 3 || significant[1].text != ":" {
            return Err(db_error(
                path,
                statement.first(),
                "Signal_representation",
                "invalid representation",
            ));
        }
        let encoding = significant[0].text.clone();
        for signal in significant.iter().skip(2) {
            database.set_signal_encoding_for_name(&signal.text, &encoding)?;
        }
    }
    expect(tokens, index, "}", path, "Signal_representation")?;
    Ok(())
}

impl LdfDatabase {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(source: &str) -> DatabaseResult<Self> {
        Self::from_source("<memory>", source)
    }

    pub fn from_file(path: impl AsRef<Path>) -> DatabaseResult<Self> {
        let path_ref = path.as_ref();
        let source = fs::read_to_string(path_ref).map_err(|error| {
            DatabaseError::new(path_ref.display().to_string(), 0, "file", error.to_string())
        })?;
        Self::from_source(path_ref.display().to_string(), &source)
    }

    pub fn from_source(path: impl Into<String>, source: &str) -> DatabaseResult<Self> {
        let path = path.into();
        let tokens = lex(&path, source)?;
        let mut database = LdfDatabase::with_source_path(path.clone());
        let mut pending_signals = HashMap::new();
        let mut index = 0;
        while index < tokens.len() {
            let keyword = tokens[index].text.clone();
            index += 1;
            match keyword.as_str() {
                "LIN_protocol_version" | "LIN_language_version" | "LIN_speed" => {
                    let statement = statement_tokens(&tokens, &mut index, &path)?;
                    let expected_len = if keyword == "LIN_speed" { 3 } else { 2 };
                    if statement.len() != expected_len || statement[0].text != "=" {
                        return Err(db_error(
                            &path,
                            statement.first(),
                            &keyword,
                            "assignment is incomplete or has unexpected tokens",
                        ));
                    }
                    if keyword == "LIN_speed" {
                        if !statement[2].text.eq_ignore_ascii_case("kbps") {
                            return Err(db_error(
                                &path,
                                Some(&statement[2]),
                                "LIN_speed",
                                "speed unit must be kbps",
                            ));
                        }
                        database.speed_kbps = Some(parse_f64(&path, &statement[1], "LIN_speed")?);
                    } else if keyword == "LIN_protocol_version" {
                        database.protocol_version = Some(statement[1].text.clone());
                    } else {
                        database.language_version = Some(statement[1].text.clone());
                    }
                }
                "LIN_description_file" => {
                    expect(&tokens, &mut index, ";", &path, "LIN_description_file")?;
                }
                "Channel_name" => {
                    expect(&tokens, &mut index, "=", &path, "Channel_name")?;
                    let channel = next(&tokens, &mut index, &path, "Channel_name")?;
                    if !channel.quoted {
                        return Err(db_error(
                            &path,
                            Some(channel),
                            "Channel_name",
                            "channel name must be quoted",
                        ));
                    }
                    database.channel_name = Some(channel.text.clone());
                    expect(&tokens, &mut index, ";", &path, "Channel_name")?;
                }
                "Nodes" => parse_nodes(&path, &tokens, &mut index, &mut database)?,
                "Signals" => {
                    expect(&tokens, &mut index, "{", &path, "Signals")?;
                    while index < tokens.len() && tokens[index].text != "}" {
                        let statement = statement_tokens(&tokens, &mut index, &path)?;
                        if !statement.is_empty() {
                            parse_signal_definition(&path, &statement, &mut pending_signals)?;
                        }
                    }
                    expect(&tokens, &mut index, "}", &path, "Signals")?;
                }
                "Frames" => {
                    expect(&tokens, &mut index, "{", &path, "Frames")?;
                    while index < tokens.len() && tokens[index].text != "}" {
                        parse_frame(&path, &tokens, &mut index, &mut database, &pending_signals)?;
                    }
                    expect(&tokens, &mut index, "}", &path, "Frames")?;
                }
                "Signal_encoding_types" => {
                    expect(&tokens, &mut index, "{", &path, "Signal_encoding_types")?;
                    while index < tokens.len() && tokens[index].text != "}" {
                        parse_encoding(&path, &tokens, &mut index, &mut database)?;
                    }
                    expect(&tokens, &mut index, "}", &path, "Signal_encoding_types")?;
                }
                "Schedule_tables" => {
                    expect(&tokens, &mut index, "{", &path, "Schedule_tables")?;
                    while index < tokens.len() && tokens[index].text != "}" {
                        parse_schedule(&path, &tokens, &mut index, &mut database)?;
                    }
                    expect(&tokens, &mut index, "}", &path, "Schedule_tables")?;
                }
                "Signal_representation" => {
                    parse_signal_representation(&path, &tokens, &mut index, &mut database)?;
                }
                // Optional diagnostic, sporadic, event-triggered, node
                // attribute, and vendor sections do not affect ordinary
                // signal decoding. Keep their presence and source location;
                // retain only their balanced body until a typed model exists.
                "Diagnostic_signals"
                | "Diagnostic_frames"
                | "Sporadic_frames"
                | "Event_triggered_frames"
                | "Node_attributes"
                | "Table_conversion" => {
                    let source = tokens
                        .get(index.saturating_sub(1))
                        .map(|token| SourceSpan::new(token.line, token.column));
                    database.optional_sections.push(LdfOptionalSection {
                        name: keyword,
                        source,
                    });
                    if tokens.get(index).map(|token| token.text.as_str()) == Some("{") {
                        skip_block(&tokens, &mut index, &path)?;
                    } else {
                        let _ = statement_tokens(&tokens, &mut index, &path)?;
                    }
                }
                other => {
                    let source = tokens
                        .get(index.saturating_sub(1))
                        .map(|token| SourceSpan::new(token.line, token.column));
                    if (other.starts_with("Vendor_") || other.starts_with("vendor_"))
                        && tokens.get(index).map(|token| token.text.as_str()) == Some("{")
                    {
                        database.optional_sections.push(LdfOptionalSection {
                            name: other.to_string(),
                            source,
                        });
                        skip_block(&tokens, &mut index, &path)?;
                    } else {
                        return Err(db_error(
                            &path,
                            tokens.get(index.saturating_sub(1)),
                            other,
                            "unsupported LDF statement",
                        ));
                    }
                }
            }
        }
        Ok(database)
    }
}

fn skip_block(tokens: &[Token], index: &mut usize, path: &str) -> DatabaseResult<()> {
    expect(tokens, index, "{", path, "section")?;
    let mut depth = 1;
    while *index < tokens.len() && depth > 0 {
        match tokens[*index].text.as_str() {
            "{" => depth += 1,
            "}" => depth -= 1,
            _ => {}
        }
        *index += 1;
    }
    if depth != 0 {
        Err(db_error(
            path,
            tokens.last(),
            "section",
            "unterminated block",
        ))
    } else {
        Ok(())
    }
}

pub fn validate_lin_identifier(identifier: u8) -> Result<(), LdfParseError> {
    let frame_id = identifier & 0x3F;
    let expected = protected_identifier(frame_id);
    if expected == identifier {
        Ok(())
    } else {
        Err(LdfParseError::InvalidIdentifier {
            identifier,
            reason: format!("expected protected identifier 0x{expected:02X}"),
        })
    }
}

pub fn protected_identifier(frame_id: u8) -> u8 {
    let id = frame_id & 0x3F;
    let p0 = (id ^ (id >> 1) ^ (id >> 2) ^ (id >> 4)) & 1;
    let p1 = (!((id >> 1) ^ (id >> 3) ^ (id >> 4) ^ (id >> 5))) & 1;
    id | (p0 << 6) | (p1 << 7)
}

pub fn validate_lin_checksum(
    frame_id: u8,
    payload: &[u8],
    checksum: u8,
    model: LinChecksum,
) -> Result<(), LdfParseError> {
    let mut sum = 0u16;
    if model == LinChecksum::Enhanced {
        sum += u16::from(if frame_id <= 0x3F {
            protected_identifier(frame_id)
        } else {
            frame_id
        });
    }
    for byte in payload {
        sum += u16::from(*byte);
        if sum > 0xFF {
            sum = (sum & 0xFF) + 1;
        }
    }
    let expected = !(sum as u8);
    if expected == checksum {
        Ok(())
    } else {
        Err(LdfParseError::ChecksumMismatch {
            expected,
            actual: checksum,
        })
    }
}

pub fn decode_lin_signal(
    signal: &LinSignal,
    data: &[u8],
) -> Result<DecodedLinSignal, LinDecodeError> {
    if signal.bit_length == 0 || signal.bit_length > 64 {
        return Err(LdfParseError::UnsupportedBitLength {
            signal: signal.name.clone(),
            bit_length: signal.bit_length,
        });
    }
    let start = usize::from(signal.start_bit);
    let length = usize::from(signal.bit_length);
    if start
        .checked_add(length)
        .is_none_or(|end| end > data.len() * 8)
    {
        return Err(LdfParseError::SignalOutOfBounds {
            signal: signal.name.clone(),
        });
    }
    let mut raw = 0u64;
    for offset in 0..length {
        raw |= u64::from((data[(start + offset) / 8] >> ((start + offset) % 8)) & 1) << offset;
    }
    let physical = raw as f64 * signal.factor + signal.offset;
    if signal.minimum.is_some_and(|minimum| physical < minimum)
        || signal.maximum.is_some_and(|maximum| physical > maximum)
    {
        return Err(LdfParseError::SignalOutOfBounds {
            signal: signal.name.clone(),
        });
    }
    Ok(DecodedLinSignal {
        signal_name: signal.name.clone(),
        raw_value: i128::from(raw),
        physical_value: physical,
        value_text: signal.choices.get(i128::from(raw)).map(str::to_owned),
    })
}

pub fn decode_lin_frame(
    database: &LdfDatabase,
    frame_id: u8,
    observation: &LinObservation,
) -> Result<Vec<DecodedLinSignal>, LinDecodeError> {
    if !observation.state.has_response() {
        return Err(LdfParseError::InvalidResponse {
            state: observation.state,
        });
    }
    let frame = database
        .frame_by_id(frame_id)
        .ok_or(LdfParseError::UnknownFrame { frame_id })?;
    let payload = observation
        .payload
        .as_deref()
        .ok_or(LdfParseError::InvalidResponse {
            state: observation.state,
        })?;
    if let Some(checksum) = observation.checksum {
        let model = observation.checksum_model.unwrap_or(frame.checksum);
        validate_lin_checksum(frame_id, payload, checksum, model)?;
    }
    if payload.len() < usize::from(frame.dlc) {
        return Err(LdfParseError::PayloadTooShort {
            required: usize::from(frame.dlc),
            actual: payload.len(),
        });
    }
    frame
        .signals
        .iter()
        .map(|signal| decode_lin_signal(signal, payload))
        .collect()
}

/// Decode a raw BLF LIN record while preserving the BLF object's response
/// classification and optional checksum metadata.  This is the preferred
/// entry point for callers iterating `File`, because it cannot accidentally
/// turn a send-error or header-only object into a zero-filled signal sample.
pub fn decode_lin_object(
    database: &LdfDatabase,
    object: &LinObject,
) -> Result<Vec<DecodedLinSignal>, LinDecodeError> {
    let observation = classify_lin_object(object);
    let frame_id = observation
        .frame_id
        .ok_or(LdfParseError::MissingFrameIdentifier)?;
    decode_lin_frame(database, frame_id, &observation)
}
