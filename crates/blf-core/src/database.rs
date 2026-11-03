//! Immutable database definitions shared by the DBC/LDF parsers and decoders.
//!
//! The models deliberately keep CAN and LIN protocol details separate.  Only
//! names, value tables, diagnostics, and binding metadata are shared.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BusKind {
    Can,
    Lin,
}

impl BusKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Can => "can",
            Self::Lin => "lin",
        }
    }
}

impl fmt::Display for BusKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{path}:{line}:{field}: {reason}")]
pub struct DatabaseError {
    pub path: String,
    pub line: usize,
    pub field: String,
    pub reason: String,
}

impl DatabaseError {
    pub fn new(
        path: impl Into<String>,
        line: usize,
        field: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            line,
            field: field.into(),
            reason: reason.into(),
        }
    }
}

pub type DatabaseResult<T> = Result<T, DatabaseError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    pub line: usize,
    pub column: usize,
}

impl SourceSpan {
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ValueTable {
    values: BTreeMap<i128, String>,
}

impl ValueTable {
    pub fn new(values: impl IntoIterator<Item = (i128, String)>) -> Self {
        Self {
            values: values.into_iter().collect(),
        }
    }

    pub fn get(&self, raw_value: i128) -> Option<&str> {
        self.values.get(&raw_value).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&i128, &String)> {
        self.values.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ByteOrder {
    LittleEndian,
    BigEndian,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignalType {
    Unsigned,
    Signed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanPayloadKind {
    /// A normal CAN or CAN-FD frame whose DLC describes the physical payload.
    Physical,
    /// A Vector logical/container message whose DBC DLC is application-level.
    LogicalContainer,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum MultiplexRole {
    #[default]
    None,
    Selector,
    Branch(BTreeSet<u32>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CanSignal {
    pub name: String,
    pub start_bit: u16,
    pub bit_length: u16,
    pub byte_order: ByteOrder,
    pub signal_type: SignalType,
    pub factor: f64,
    pub offset: f64,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub choices: ValueTable,
    pub multiplex: MultiplexRole,
    pub attributes: BTreeMap<String, String>,
    pub comment: Option<String>,
    pub source: Option<SourceSpan>,
}

impl CanSignal {
    pub fn new(
        name: impl Into<String>,
        start_bit: u16,
        bit_length: u16,
        byte_order: ByteOrder,
        signal_type: SignalType,
    ) -> Self {
        Self {
            name: name.into(),
            start_bit,
            bit_length,
            byte_order,
            signal_type,
            factor: 1.0,
            offset: 0.0,
            minimum: None,
            maximum: None,
            choices: ValueTable::default(),
            multiplex: MultiplexRole::None,
            attributes: BTreeMap::new(),
            comment: None,
            source: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CanMessageId {
    pub id: u32,
    pub extended: bool,
}

impl CanMessageId {
    pub const fn new(id: u32, extended: bool) -> Self {
        Self { id, extended }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DbcMessage {
    pub id: CanMessageId,
    pub name: String,
    pub dlc: u8,
    pub is_fd: bool,
    pub payload_kind: CanPayloadKind,
    pub signals: Vec<CanSignal>,
    pub comment: Option<String>,
    pub attributes: BTreeMap<String, String>,
    pub source: Option<SourceSpan>,
}

impl DbcMessage {
    pub fn signal(&self, name: &str) -> Option<&CanSignal> {
        self.signals.iter().find(|signal| signal.name == name)
    }

    pub(crate) fn signal_mut(&mut self, name: &str) -> Option<&mut CanSignal> {
        self.signals.iter_mut().find(|signal| signal.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanSignalRef {
    pub message_id: CanMessageId,
    pub signal_index: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DbcDatabase {
    pub(crate) version: Option<String>,
    pub(crate) source_path: Option<String>,
    pub(crate) attributes: BTreeMap<String, String>,
    pub(crate) scoped_attributes: BTreeMap<String, String>,
    messages_by_id: BTreeMap<CanMessageId, Arc<DbcMessage>>,
    messages_by_name: HashMap<String, CanMessageId>,
    signals_by_qualified_name: HashMap<String, CanSignalRef>,
}

impl Default for DbcDatabase {
    fn default() -> Self {
        Self::new()
    }
}

impl DbcDatabase {
    pub(crate) fn new() -> Self {
        Self {
            version: None,
            source_path: None,
            attributes: BTreeMap::new(),
            scoped_attributes: BTreeMap::new(),
            messages_by_id: BTreeMap::new(),
            messages_by_name: HashMap::new(),
            signals_by_qualified_name: HashMap::new(),
        }
    }

    pub(crate) fn with_source_path(path: impl Into<String>) -> Self {
        let mut database = Self::new();
        database.source_path = Some(path.into());
        database
    }

    pub(crate) fn insert_message(&mut self, message: DbcMessage) -> DatabaseResult<()> {
        if self.messages_by_id.contains_key(&message.id) {
            return Err(DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                message.source.map_or(0, |span| span.line),
                "BO_",
                format!("duplicate CAN message id 0x{:X}", message.id.id),
            ));
        }
        if self.messages_by_name.contains_key(&message.name) {
            return Err(DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                message.source.map_or(0, |span| span.line),
                "BO_",
                format!("duplicate CAN message name {:?}", message.name),
            ));
        }

        let message_id = message.id;
        let message_name = message.name.clone();
        let message = Arc::new(message);
        for signal in &message.signals {
            let qualified_name = format!("{}.{}", message.name, signal.name);
            if self.signals_by_qualified_name.contains_key(&qualified_name) {
                return Err(DatabaseError::new(
                    self.source_path.as_deref().unwrap_or("<memory>"),
                    signal.source.map_or(0, |span| span.line),
                    "SG_",
                    format!("duplicate signal name {:?}", qualified_name),
                ));
            }
        }
        for (signal_index, signal) in message.signals.iter().enumerate() {
            self.signals_by_qualified_name.insert(
                format!("{}.{}", message.name, signal.name),
                CanSignalRef {
                    message_id,
                    signal_index,
                },
            );
        }
        self.messages_by_name.insert(message_name, message_id);
        self.messages_by_id.insert(message_id, message);
        Ok(())
    }

    pub(crate) fn add_signal(
        &mut self,
        message_id: CanMessageId,
        signal: CanSignal,
    ) -> DatabaseResult<()> {
        let qualified_name = {
            let message = self.messages_by_id.get(&message_id).ok_or_else(|| {
                DatabaseError::new(
                    self.source_path.as_deref().unwrap_or("<memory>"),
                    signal.source.map_or(0, |span| span.line),
                    "SG_",
                    format!("unknown CAN message id 0x{:X}", message_id.id),
                )
            })?;
            format!("{}.{}", message.name, signal.name)
        };
        if self.signals_by_qualified_name.contains_key(&qualified_name) {
            let signal_ref = self
                .signals_by_qualified_name
                .get(&qualified_name)
                .expect("qualified signal index exists");
            let existing = &self
                .messages_by_id
                .get(&message_id)
                .expect("message index exists")
                .signals[signal_ref.signal_index];
            let mut existing_without_source = existing.clone();
            let mut signal_without_source = signal.clone();
            existing_without_source.source = None;
            signal_without_source.source = None;
            if existing_without_source == signal_without_source {
                return Ok(());
            }
            return Err(DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                signal.source.map_or(0, |span| span.line),
                "SG_",
                format!("duplicate signal name {:?}", qualified_name),
            ));
        }
        let message = self.messages_by_id.get_mut(&message_id).unwrap();
        let message = Arc::get_mut(message).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                signal.source.map_or(0, |span| span.line),
                "SG_",
                "message is shared and cannot be modified",
            )
        })?;
        let signal_index = message.signals.len();
        message.signals.push(signal);
        self.signals_by_qualified_name.insert(
            qualified_name,
            CanSignalRef {
                message_id,
                signal_index,
            },
        );
        Ok(())
    }

    pub fn message_by_id(&self, id: u32, extended: bool) -> Option<&DbcMessage> {
        self.messages_by_id
            .get(&CanMessageId::new(id, extended))
            .map(Arc::as_ref)
    }

    pub fn message_by_name(&self, name: &str) -> Option<&DbcMessage> {
        let id = self.messages_by_name.get(name)?;
        self.messages_by_id.get(id).map(Arc::as_ref)
    }

    pub fn signal(&self, qualified_name: &str) -> Option<(&DbcMessage, &CanSignal)> {
        let signal_ref = self.signals_by_qualified_name.get(qualified_name)?;
        let message = self.messages_by_id.get(&signal_ref.message_id)?;
        Some((message, &message.signals[signal_ref.signal_index]))
    }

    pub fn messages(&self) -> impl Iterator<Item = &DbcMessage> {
        self.messages_by_id.values().map(Arc::as_ref)
    }

    pub fn signal_names(&self) -> impl Iterator<Item = &str> {
        self.signals_by_qualified_name.keys().map(String::as_str)
    }

    pub(crate) fn set_attribute(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.attributes.insert(name.into(), value.into());
    }

    pub(crate) fn set_message_comment(
        &mut self,
        id: CanMessageId,
        comment: impl Into<String>,
    ) -> DatabaseResult<()> {
        let message = self.messages_by_id.get_mut(&id).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "CM_",
                format!("unknown CAN message id 0x{:X}", id.id),
            )
        })?;
        Arc::get_mut(message)
            .ok_or_else(|| {
                DatabaseError::new(
                    self.source_path.as_deref().unwrap_or("<memory>"),
                    0,
                    "CM_",
                    "message is shared and cannot be modified",
                )
            })?
            .comment = Some(comment.into());
        Ok(())
    }

    pub(crate) fn set_message_attribute(
        &mut self,
        id: CanMessageId,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> DatabaseResult<()> {
        let message = self.messages_by_id.get_mut(&id).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "BA_",
                format!("unknown CAN message id 0x{:X}", id.id),
            )
        })?;
        Arc::get_mut(message)
            .ok_or_else(|| {
                DatabaseError::new(
                    self.source_path.as_deref().unwrap_or("<memory>"),
                    0,
                    "BA_",
                    "message is shared and cannot be modified",
                )
            })?
            .attributes
            .insert(name.into(), value.into());
        Ok(())
    }

    pub(crate) fn set_message_is_fd(
        &mut self,
        id: CanMessageId,
        is_fd: bool,
    ) -> DatabaseResult<()> {
        let message = self.messages_by_id.get_mut(&id).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "BA_",
                format!("unknown CAN message id 0x{:X}", id.id),
            )
        })?;
        let message = Arc::get_mut(message).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "BA_",
                "message is shared and cannot be modified",
            )
        })?;
        message.is_fd = is_fd;
        Ok(())
    }

    pub(crate) fn set_signal_attribute(
        &mut self,
        id: CanMessageId,
        signal_name: &str,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> DatabaseResult<()> {
        let message = self.messages_by_id.get_mut(&id).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "BA_",
                format!("unknown CAN message id 0x{:X}", id.id),
            )
        })?;
        let message = Arc::get_mut(message).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "BA_",
                "message is shared and cannot be modified",
            )
        })?;
        let message_name = message.name.clone();
        let signal = message.signal_mut(signal_name).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "BA_",
                format!("unknown CAN signal {:?}.{:?}", message_name, signal_name),
            )
        })?;
        signal.attributes.insert(name.into(), value.into());
        Ok(())
    }

    pub(crate) fn set_scoped_attribute(
        &mut self,
        scope: &str,
        object: &str,
        name: impl Into<String>,
        value: impl Into<String>,
    ) {
        self.scoped_attributes
            .insert(format!("{scope}:{object}:{}", name.into()), value.into());
    }

    pub fn scoped_attributes(&self) -> impl Iterator<Item = (&String, &String)> {
        self.scoped_attributes.iter()
    }

    pub(crate) fn add_multiplex_ranges(
        &mut self,
        id: CanMessageId,
        signal_name: &str,
        selector_name: &str,
        ranges: impl IntoIterator<Item = (u32, u32)>,
    ) -> DatabaseResult<()> {
        let message = self.messages_by_id.get_mut(&id).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "SG_MUL_VAL_",
                format!("unknown CAN message id 0x{:X}", id.id),
            )
        })?;
        let message = Arc::get_mut(message).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "SG_MUL_VAL_",
                "message is shared and cannot be modified",
            )
        })?;
        let message_name = message.name.clone();
        if !message.signals.iter().any(|signal| {
            signal.name == selector_name && signal.multiplex == MultiplexRole::Selector
        }) {
            return Err(DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "SG_MUL_VAL_",
                format!("unknown multiplex selector {:?}", selector_name),
            ));
        }
        let signal = message.signal_mut(signal_name).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "SG_MUL_VAL_",
                format!("unknown CAN signal {:?}.{:?}", message_name, signal_name),
            )
        })?;
        let mut values = match &signal.multiplex {
            MultiplexRole::Branch(values) => values.clone(),
            MultiplexRole::None => BTreeSet::new(),
            MultiplexRole::Selector => {
                return Err(DatabaseError::new(
                    self.source_path.as_deref().unwrap_or("<memory>"),
                    0,
                    "SG_MUL_VAL_",
                    "multiplex selector cannot be a branch",
                ));
            }
        };
        for (start, end) in ranges {
            values.extend(start..=end);
        }
        signal.multiplex = MultiplexRole::Branch(values);
        Ok(())
    }

    pub(crate) fn set_signal_comment(
        &mut self,
        message_id: CanMessageId,
        signal_name: &str,
        comment: impl Into<String>,
    ) -> DatabaseResult<()> {
        let message = self.messages_by_id.get_mut(&message_id).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "CM_",
                format!("unknown CAN message id 0x{:X}", message_id.id),
            )
        })?;
        let message = Arc::get_mut(message).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "CM_",
                "message is shared and cannot be modified",
            )
        })?;
        let signal = message
            .signals
            .iter_mut()
            .find(|signal| signal.name == signal_name)
            .ok_or_else(|| {
                DatabaseError::new(
                    self.source_path.as_deref().unwrap_or("<memory>"),
                    0,
                    "CM_",
                    format!("unknown signal {:?}", signal_name),
                )
            })?;
        signal.comment = Some(comment.into());
        Ok(())
    }

    pub(crate) fn set_signal_choices(
        &mut self,
        message_id: CanMessageId,
        signal_name: &str,
        choices: ValueTable,
    ) -> DatabaseResult<()> {
        let message = self.messages_by_id.get_mut(&message_id).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "VAL_",
                format!("unknown CAN message id 0x{:X}", message_id.id),
            )
        })?;
        let message = Arc::get_mut(message).ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "VAL_",
                "message is shared and cannot be modified",
            )
        })?;
        let signal = message
            .signals
            .iter_mut()
            .find(|signal| signal.name == signal_name)
            .ok_or_else(|| {
                DatabaseError::new(
                    self.source_path.as_deref().unwrap_or("<memory>"),
                    0,
                    "VAL_",
                    format!("unknown signal {:?}", signal_name),
                )
            })?;
        signal.choices = choices;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.messages_by_id.len()
    }

    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    pub fn source_path(&self) -> Option<&str> {
        self.source_path.as_deref()
    }

    pub fn attributes(&self) -> impl Iterator<Item = (&str, &str)> {
        self.attributes
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.messages_by_id.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinChecksum {
    Classic,
    Enhanced,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinSignal {
    pub name: String,
    pub start_bit: u16,
    pub bit_length: u16,
    pub initial_value: Option<i64>,
    pub factor: f64,
    pub offset: f64,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub encoding: Option<String>,
    pub choices: ValueTable,
    pub source: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinPhysicalRange {
    pub minimum: f64,
    pub maximum: f64,
    pub factor: f64,
    pub offset: f64,
    pub unit: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinEncoding {
    pub name: String,
    pub logical_values: ValueTable,
    pub physical_ranges: Vec<LinPhysicalRange>,
    pub source: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinFrame {
    pub id: u8,
    pub name: String,
    pub dlc: u8,
    pub publisher: String,
    pub checksum: LinChecksum,
    pub signals: Vec<LinSignal>,
    pub source: Option<SourceSpan>,
}

impl LinFrame {
    pub fn signal(&self, name: &str) -> Option<&LinSignal> {
        self.signals.iter().find(|signal| signal.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinNode {
    pub name: String,
    pub configured_nad: Option<u8>,
    pub source: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinScheduleEntry {
    pub frame_name: String,
    pub delay_ms: u32,
    pub source: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LdfOptionalSection {
    pub name: String,
    pub source: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinSignalRef {
    pub frame_id: u8,
    pub signal_index: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LdfDatabase {
    pub(crate) source_path: Option<String>,
    pub(crate) channel_name: Option<String>,
    pub(crate) protocol_version: Option<String>,
    pub(crate) language_version: Option<String>,
    pub(crate) speed_kbps: Option<f64>,
    pub(crate) nodes: Vec<LinNode>,
    pub(crate) schedules: BTreeMap<String, Vec<LinScheduleEntry>>,
    pub(crate) encodings: BTreeMap<String, LinEncoding>,
    pub(crate) optional_sections: Vec<LdfOptionalSection>,
    frames_by_id: BTreeMap<u8, Arc<LinFrame>>,
    frames_by_name: HashMap<String, u8>,
    signals_by_qualified_name: HashMap<String, LinSignalRef>,
}

impl Default for LdfDatabase {
    fn default() -> Self {
        Self::new()
    }
}

impl LdfDatabase {
    pub(crate) fn new() -> Self {
        Self {
            source_path: None,
            channel_name: None,
            protocol_version: None,
            language_version: None,
            speed_kbps: None,
            nodes: Vec::new(),
            schedules: BTreeMap::new(),
            encodings: BTreeMap::new(),
            optional_sections: Vec::new(),
            frames_by_id: BTreeMap::new(),
            frames_by_name: HashMap::new(),
            signals_by_qualified_name: HashMap::new(),
        }
    }

    pub(crate) fn with_source_path(path: impl Into<String>) -> Self {
        let mut database = Self::new();
        database.source_path = Some(path.into());
        database
    }

    pub(crate) fn insert_frame(&mut self, frame: LinFrame) -> DatabaseResult<()> {
        if self.frames_by_id.contains_key(&frame.id) {
            return Err(DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                frame.source.map_or(0, |span| span.line),
                "frame_id",
                format!("duplicate LIN frame id 0x{:02X}", frame.id),
            ));
        }
        if self.frames_by_name.contains_key(&frame.name) {
            return Err(DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                frame.source.map_or(0, |span| span.line),
                "frame_name",
                format!("duplicate LIN frame name {:?}", frame.name),
            ));
        }

        let frame_id = frame.id;
        let frame_name = frame.name.clone();
        let frame = Arc::new(frame);
        for (signal_index, signal) in frame.signals.iter().enumerate() {
            let qualified_name = format!("{}.{}", frame.name, signal.name);
            if self
                .signals_by_qualified_name
                .insert(
                    qualified_name.clone(),
                    LinSignalRef {
                        frame_id,
                        signal_index,
                    },
                )
                .is_some()
            {
                return Err(DatabaseError::new(
                    self.source_path.as_deref().unwrap_or("<memory>"),
                    signal.source.map_or(0, |span| span.line),
                    "signal",
                    format!("duplicate signal name {:?}", qualified_name),
                ));
            }
        }
        self.frames_by_name.insert(frame_name, frame_id);
        self.frames_by_id.insert(frame_id, frame);
        Ok(())
    }

    pub fn frame_by_id(&self, id: u8) -> Option<&LinFrame> {
        self.frames_by_id.get(&id).map(Arc::as_ref)
    }

    pub fn frame_by_name(&self, name: &str) -> Option<&LinFrame> {
        let id = self.frames_by_name.get(name)?;
        self.frames_by_id.get(id).map(Arc::as_ref)
    }

    pub fn signal(&self, qualified_name: &str) -> Option<(&LinFrame, &LinSignal)> {
        let signal_ref = self.signals_by_qualified_name.get(qualified_name)?;
        let frame = self.frames_by_id.get(&signal_ref.frame_id)?;
        Some((frame, &frame.signals[signal_ref.signal_index]))
    }

    pub(crate) fn set_signal_encoding_for_name(
        &mut self,
        signal_name: &str,
        encoding_name: &str,
    ) -> DatabaseResult<()> {
        let encoding = self.encodings.get(encoding_name).cloned().ok_or_else(|| {
            DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "Signal_representation",
                format!("unknown encoding type {:?}", encoding_name),
            )
        })?;
        let mut found = false;
        for frame in self.frames_by_id.values_mut() {
            let frame = Arc::get_mut(frame).ok_or_else(|| {
                DatabaseError::new(
                    self.source_path.as_deref().unwrap_or("<memory>"),
                    0,
                    "Signal_representation",
                    "frame is shared and cannot be modified",
                )
            })?;
            for signal in &mut frame.signals {
                if signal.name != signal_name {
                    continue;
                }
                found = true;
                signal.encoding = Some(encoding_name.to_string());
                signal.choices = encoding.logical_values.clone();
                if let Some(range) = encoding.physical_ranges.first() {
                    signal.factor = range.factor;
                    signal.offset = range.offset;
                    signal.minimum = Some(range.minimum);
                    signal.maximum = Some(range.maximum);
                }
            }
        }
        if found {
            Ok(())
        } else {
            Err(DatabaseError::new(
                self.source_path.as_deref().unwrap_or("<memory>"),
                0,
                "Signal_representation",
                format!("unknown signal {:?}", signal_name),
            ))
        }
    }

    pub fn frames(&self) -> impl Iterator<Item = &LinFrame> {
        self.frames_by_id.values().map(Arc::as_ref)
    }

    pub fn source_path(&self) -> Option<&str> {
        self.source_path.as_deref()
    }

    pub fn channel_name(&self) -> Option<&str> {
        self.channel_name.as_deref()
    }

    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }

    pub fn language_version(&self) -> Option<&str> {
        self.language_version.as_deref()
    }

    pub fn speed_kbps(&self) -> Option<f64> {
        self.speed_kbps
    }

    pub fn nodes(&self) -> impl Iterator<Item = &LinNode> {
        self.nodes.iter()
    }

    pub fn schedules(&self) -> impl Iterator<Item = (&str, &[LinScheduleEntry])> {
        self.schedules
            .iter()
            .map(|(name, entries)| (name.as_str(), entries.as_slice()))
    }

    pub fn encodings(&self) -> impl Iterator<Item = (&str, &LinEncoding)> {
        self.encodings
            .iter()
            .map(|(name, encoding)| (name.as_str(), encoding))
    }

    /// Optional sections remain visible for diagnostics instead of being
    /// silently discarded when ordinary frame decoding does not use them.
    pub fn optional_sections(&self) -> impl Iterator<Item = &LdfOptionalSection> {
        self.optional_sections.iter()
    }

    pub fn len(&self) -> usize {
        self.frames_by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames_by_id.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Database {
    Can(Arc<DbcDatabase>),
    Lin(Arc<LdfDatabase>),
}

impl Database {
    pub fn bus(&self) -> BusKind {
        match self {
            Self::Can(_) => BusKind::Can,
            Self::Lin(_) => BusKind::Lin,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DatabaseBinding {
    bus: BusKind,
    channel: u16,
    database: Database,
}

impl DatabaseBinding {
    pub fn new(bus: BusKind, channel: u16, database: Database) -> DatabaseResult<Self> {
        if database.bus() != bus {
            return Err(DatabaseError::new(
                "<binding>",
                0,
                "bus",
                format!(
                    "database kind {} cannot be bound to {} channel {}",
                    database.bus(),
                    bus,
                    channel
                ),
            ));
        }
        Ok(Self {
            bus,
            channel,
            database,
        })
    }

    pub fn bus(&self) -> BusKind {
        self.bus
    }

    pub fn channel(&self) -> u16 {
        self.channel
    }

    pub fn database(&self) -> &Database {
        &self.database
    }
}
