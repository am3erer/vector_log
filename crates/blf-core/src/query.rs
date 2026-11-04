//! Lazy, database-backed signal queries over independent BLF cursors.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use thiserror::Error;

use crate::can::{decode_signal_for_message, CanDecodeError, DecodedCanSignal};
use crate::database::{
    BusKind, Database, DatabaseBinding, DatabaseError, DbcDatabase, DbcMessage, LdfDatabase,
    LinSignal,
};
use crate::ldf::{decode_lin_signal, DecodedLinSignal, LinDecodeError};
use crate::lin::{classify_lin_object, LinResponseState};
use crate::reader::File;
use crate::sample::{DecodeStatus, SampleContext, SampleValue, SignalSample};
use crate::types::{FieldValue, LinObject, Object};
use crate::{can_flags_direction, object_timestamp_ns, object_timestamp_source, BlfError};

#[derive(Debug, Error)]
pub enum QueryError {
    #[error("BLF read failed: {0}")]
    Blf(#[from] BlfError),
    #[error("database binding failed: {0}")]
    Database(#[from] DatabaseError),
    #[error("signal {signal:?} is not present in the {bus} database bound to channel {channel}")]
    UnknownSignal {
        signal: String,
        bus: BusKind,
        channel: u16,
    },
    #[error("database kind {database_bus} cannot be queried as {requested_bus}")]
    BusMismatch {
        database_bus: BusKind,
        requested_bus: BusKind,
    },
    #[error("duplicate database binding for {bus} channel {channel}")]
    DuplicateBinding { bus: BusKind, channel: u16 },
    #[error("query object has no usable BLF timestamp")]
    MissingTimestamp,
    #[error("BLF reader is not open")]
    ReaderClosed,
    #[error("signal decode failed: {0}")]
    Decode(String),
}

#[derive(Debug, Clone)]
enum QueryTarget {
    Can {
        database: Arc<DbcDatabase>,
        id: u32,
        extended: bool,
        message_name: String,
        signal_name: String,
    },
    Lin {
        database: Arc<LdfDatabase>,
        frame_id: u8,
        diagnostic: bool,
        message_name: String,
        signal_name: String,
    },
}

#[derive(Debug, Clone)]
struct LinSignalTarget {
    frame_id: u8,
    diagnostic: bool,
    message_name: String,
    signal_name: String,
}

#[derive(Debug, Clone)]
struct BoundTarget {
    channel: u16,
    target: QueryTarget,
}

/// A lazy signal query. It owns an independent BLF cursor and never advances
/// the ordinary cursor held by the source `File`.
pub struct SignalQuery {
    cursor: File,
    targets: Vec<BoundTarget>,
    signal_names: Vec<String>,
    strict: bool,
    pending: VecDeque<Result<SignalSample, QueryError>>,
    terminated: bool,
}

impl std::fmt::Debug for SignalQuery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SignalQuery")
            .field("signal_names", &self.signal_names)
            .field("strict", &self.strict)
            .field("target_count", &self.targets.len())
            .finish_non_exhaustive()
    }
}

impl SignalQuery {
    pub fn new(
        source: &File,
        bindings: Vec<DatabaseBinding>,
        signal_name: impl Into<String>,
        strict: bool,
    ) -> Result<Self, QueryError> {
        Self::new_many(source, bindings, vec![signal_name.into()], strict)
    }

    pub fn new_many(
        source: &File,
        bindings: Vec<DatabaseBinding>,
        signal_names: Vec<String>,
        strict: bool,
    ) -> Result<Self, QueryError> {
        Self::new_many_inner(source, bindings, signal_names, strict, false)
    }

    pub fn aggregate(
        source: &File,
        bindings: Vec<DatabaseBinding>,
        signal_name: impl Into<String>,
        strict: bool,
    ) -> Result<Self, QueryError> {
        Self::new_many_inner(source, bindings, vec![signal_name.into()], strict, true)
    }

    pub fn aggregate_many(
        source: &File,
        bindings: Vec<DatabaseBinding>,
        signal_names: Vec<String>,
        strict: bool,
    ) -> Result<Self, QueryError> {
        Self::new_many_inner(source, bindings, signal_names, strict, true)
    }

    fn new_many_inner(
        source: &File,
        bindings: Vec<DatabaseBinding>,
        signal_names: Vec<String>,
        strict: bool,
        aggregate: bool,
    ) -> Result<Self, QueryError> {
        if signal_names.is_empty() {
            return Err(QueryError::UnknownSignal {
                signal: String::new(),
                bus: BusKind::Can,
                channel: 0,
            });
        }
        let first_binding = bindings.first().cloned();
        let mut targets = Vec::new();
        let mut seen_bindings = HashSet::new();
        let mut seen_targets = HashSet::new();
        for binding in &bindings {
            if !seen_bindings.insert((binding.bus(), binding.channel())) {
                return Err(QueryError::DuplicateBinding {
                    bus: binding.bus(),
                    channel: binding.channel(),
                });
            }
            if binding.database().bus() != binding.bus() {
                return Err(QueryError::BusMismatch {
                    database_bus: binding.database().bus(),
                    requested_bus: binding.bus(),
                });
            }
            match binding.database() {
                Database::Can(database) => {
                    for signal_name in &signal_names {
                        for (message, signal) in find_can_signals(database, signal_name, aggregate)
                        {
                            let key = (
                                binding.channel(),
                                message.id.id,
                                message.id.extended,
                                false,
                                signal.name.clone(),
                            );
                            if seen_targets.insert(key) {
                                targets.push(BoundTarget {
                                    channel: binding.channel(),
                                    target: QueryTarget::Can {
                                        database: database.clone(),
                                        id: message.id.id,
                                        extended: message.id.extended,
                                        message_name: message.name.clone(),
                                        signal_name: signal.name.clone(),
                                    },
                                });
                            }
                        }
                    }
                }
                Database::Lin(database) => {
                    for signal_name in &signal_names {
                        for target in find_lin_signals(database, signal_name, aggregate) {
                            let key = (
                                binding.channel(),
                                u32::from(target.frame_id),
                                false,
                                target.diagnostic,
                                target.signal_name.clone(),
                            );
                            if seen_targets.insert(key) {
                                targets.push(BoundTarget {
                                    channel: binding.channel(),
                                    target: QueryTarget::Lin {
                                        database: database.clone(),
                                        frame_id: target.frame_id,
                                        diagnostic: target.diagnostic,
                                        message_name: target.message_name,
                                        signal_name: target.signal_name,
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }
        if targets.is_empty() {
            let (bus, channel) = first_binding
                .map(|binding| (binding.bus(), binding.channel()))
                .unwrap_or((BusKind::Can, 0));
            return Err(QueryError::UnknownSignal {
                signal: signal_names.join(", "),
                bus,
                channel,
            });
        }
        Ok(Self {
            cursor: source.independent_cursor(),
            targets,
            signal_names,
            strict,
            pending: VecDeque::new(),
            terminated: false,
        })
    }

    pub fn strict(&self) -> bool {
        self.strict
    }

    pub fn collect(self) -> Result<Vec<SignalSample>, QueryError> {
        self.collect_results()
    }

    pub fn collect_results(self) -> Result<Vec<SignalSample>, QueryError> {
        let mut samples = Vec::new();
        for sample in self {
            samples.push(sample?);
        }
        Ok(samples)
    }

    fn pop_pending(&mut self) -> Option<Result<SignalSample, QueryError>> {
        let sample = self.pending.pop_front();
        if self.strict && sample.as_ref().is_some_and(Result::is_err) {
            self.terminated = true;
        }
        sample
    }
}

impl Iterator for SignalQuery {
    type Item = Result<SignalSample, QueryError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.terminated {
            return None;
        }
        loop {
            if let Some(sample) = self.pop_pending() {
                return Some(sample);
            }
            let object = match self.cursor.read() {
                Ok(Some(object)) => object,
                Ok(None) => return None,
                Err(error) => {
                    // A BLF read error leaves the cursor at an unusable
                    // position.  Terminate in both modes so callers receive
                    // one diagnostic and cannot loop on the same error.
                    self.terminated = true;
                    return Some(Err(QueryError::Blf(error)));
                }
            };
            let Some(timestamp_ns) = object_timestamp_ns(&object) else {
                if self.strict {
                    self.terminated = true;
                    return Some(Err(QueryError::MissingTimestamp));
                }
                continue;
            };
            let results = match &object {
                Object::CanMessage(value) => self.decode_can(
                    timestamp_ns,
                    value.channel,
                    value.id,
                    &value.data[..usize::from(value.dlc).min(value.data.len())],
                    can_context(
                        &object,
                        Some(value.dlc),
                        value.dlc,
                        Some(can_flags_direction(value.flags)),
                    ),
                ),
                Object::CanMessage2(value) => self.decode_can(
                    timestamp_ns,
                    value.channel,
                    value.id,
                    &value.data[..usize::from(value.dlc).min(value.data.len())],
                    can_context(
                        &object,
                        Some(value.dlc),
                        value.dlc,
                        Some(can_flags_direction(value.flags)),
                    ),
                ),
                Object::CanFdMessage(value) => self.decode_can(
                    timestamp_ns,
                    value.channel,
                    value.id,
                    &value.data[..usize::from(value.valid_data_bytes).min(value.data.len())],
                    can_context(
                        &object,
                        Some(value.dlc),
                        value.valid_data_bytes,
                        Some(can_flags_direction(value.flags)),
                    ),
                ),
                Object::CanFdMessage64(value) => self.decode_can(
                    timestamp_ns,
                    u16::from(value.channel),
                    value.id,
                    &value.data[..usize::from(value.valid_data_bytes).min(value.data.len())],
                    can_context(
                        &object,
                        Some(value.dlc),
                        value.valid_data_bytes,
                        Some(value.dir),
                    ),
                ),
                Object::Lin(value) => self.decode_lin(timestamp_ns, value),
                _ => Vec::new(),
            };
            for result in results {
                self.pending.push_back(result.and_then(|sample| {
                    if self.strict && sample.is_error() {
                        Err(QueryError::Decode(
                            sample
                                .error
                                .clone()
                                .unwrap_or_else(|| sample.status.to_string()),
                        ))
                    } else {
                        Ok(sample)
                    }
                }));
            }
            if let Some(sample) = self.pop_pending() {
                return Some(sample);
            }
        }
    }
}

impl SignalQuery {
    fn decode_can(
        &self,
        timestamp_ns: u64,
        channel: u16,
        raw_id: u32,
        data: &[u8],
        context: SampleContext,
    ) -> Vec<Result<SignalSample, QueryError>> {
        let (id, extended) = normalize_can_id(raw_id);
        let channel_targets: Vec<_> = self
            .targets
            .iter()
            .filter(|target| {
                target.channel == channel && matches!(&target.target, QueryTarget::Can { .. })
            })
            .collect();
        let mut results = Vec::new();
        let frame_known = channel_targets.iter().any(|target| {
            matches!(
                &target.target,
                QueryTarget::Can { database, .. } if database.message_by_id(id, extended).is_some()
            )
        });
        let has_matching_target = channel_targets.iter().any(|target| {
            matches!(
                &target.target,
                QueryTarget::Can {
                    id: target_id,
                    extended: target_extended,
                    ..
                } if *target_id == id && *target_extended == extended
            )
        });
        if !frame_known && !has_matching_target {
            if let Some(QueryTarget::Can {
                message_name,
                signal_name,
                ..
            }) = channel_targets.first().map(|target| &target.target)
            {
                results.push(Ok(error_sample(
                    timestamp_ns,
                    BusKind::Can,
                    channel,
                    id,
                    "<unknown>",
                    signal_name,
                    DecodeStatus::UnknownFrame,
                    format!("unknown CAN frame 0x{id:X} for signal {message_name}.{signal_name}"),
                    data,
                )
                .with_context(context.clone())));
            }
            return results;
        }
        for target in channel_targets {
            let QueryTarget::Can {
                id: target_id,
                extended: target_extended,
                database,
                message_name,
                signal_name,
                ..
            } = &target.target
            else {
                continue;
            };
            // In an aggregate query, only the target that owns the observed
            // frame may produce a sample. Check ownership before the lookup,
            // otherwise another same-name target can emit a false unknown.
            if *target_id != id || *target_extended != extended {
                continue;
            }
            let Some(message) = database.message_by_id(id, extended) else {
                continue;
            };
            let decoded = decode_signal_for_message(message, signal_name, data);
            match decoded {
                Ok(Some(signal)) => results.push(Ok(can_sample(
                    timestamp_ns,
                    channel,
                    id,
                    message,
                    signal,
                    data,
                    context.clone(),
                ))),
                // A named branch that is not selected by this frame is not a
                // decoding failure. Exact signal queries omit it, matching
                // decode_message's multiplex semantics in both modes.
                Ok(None) => {}
                Err(error) => results.push(Ok(error_sample(
                    timestamp_ns,
                    BusKind::Can,
                    channel,
                    id,
                    message_name,
                    signal_name,
                    can_status(&error),
                    error.to_string(),
                    data,
                )
                .with_context(context.clone()))),
            }
        }
        results
    }

    fn decode_lin(
        &self,
        timestamp_ns: u64,
        object: &LinObject,
    ) -> Vec<Result<SignalSample, QueryError>> {
        let Some(channel) = field_u16(object, "channel") else {
            return Vec::new();
        };
        let observation = classify_lin_object(object);
        let context = lin_context(object, &observation, timestamp_ns);
        let Some(frame_id) = observation.frame_id else {
            return Vec::new();
        };
        let channel_targets: Vec<_> = self
            .targets
            .iter()
            .filter(|target| {
                target.channel == channel && matches!(&target.target, QueryTarget::Lin { .. })
            })
            .collect();
        let mut results = Vec::new();
        let frame_known = channel_targets.iter().any(|target| {
            matches!(
                &target.target,
                QueryTarget::Lin {
                    database,
                    diagnostic,
                    ..
                } if if *diagnostic {
                    database.diagnostic_frame_by_id(frame_id).is_some()
                } else {
                    database.frame_by_id(frame_id).is_some()
                }
            )
        });
        let has_matching_target = channel_targets.iter().any(|target| {
            matches!(
                &target.target,
                QueryTarget::Lin {
                    frame_id: target_frame_id,
                    ..
                } if *target_frame_id == frame_id
            )
        });
        if !frame_known && !has_matching_target {
            if let Some(QueryTarget::Lin { signal_name, .. }) =
                channel_targets.first().map(|target| &target.target)
            {
                results.push(Ok(error_sample(
                    timestamp_ns,
                    BusKind::Lin,
                    channel,
                    u32::from(frame_id),
                    "<unknown>",
                    signal_name,
                    DecodeStatus::UnknownFrame,
                    format!("unknown LIN frame 0x{frame_id:02X} for signal {signal_name}"),
                    observation.payload.as_deref().unwrap_or_default(),
                )
                .with_context(context.clone())));
            }
            return results;
        }
        for target in channel_targets {
            let QueryTarget::Lin {
                frame_id: target_frame_id,
                database,
                diagnostic: diagnostic_frame,
                message_name,
                signal_name,
                ..
            } = &target.target
            else {
                continue;
            };
            // Do not let another same-name LIN frame manufacture an unknown
            // frame sample for this target.
            if *target_frame_id != frame_id {
                continue;
            }
            let payload = observation.payload.as_deref().unwrap_or_default();
            let (frame_name, frame_checksum, decoded) = if *diagnostic_frame {
                let Some(frame) = database.diagnostic_frame_by_id(frame_id) else {
                    continue;
                };
                let decoded = frame
                    .signal(signal_name)
                    .map(|signal| decode_lin_signal_for_dlc(frame.dlc, signal, payload))
                    .transpose();
                (frame.name.clone(), frame.checksum, decoded)
            } else {
                let Some(frame) = database.frame_by_id(frame_id) else {
                    continue;
                };
                let decoded = frame
                    .signal(signal_name)
                    .map(|signal| decode_lin_signal_for_dlc(frame.dlc, signal, payload))
                    .transpose();
                (frame.name.clone(), frame.checksum, decoded)
            };
            if !observation.has_safe_payload() {
                results.push(Ok(error_sample(
                    timestamp_ns,
                    BusKind::Lin,
                    channel,
                    u32::from(frame_id),
                    message_name,
                    signal_name,
                    lin_status(observation.state),
                    observation
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("LIN state: {:?}", observation.state)),
                    payload,
                )
                .with_context(context.clone())));
                continue;
            }
            let mut status = lin_status(observation.state);
            let mut diagnostic = observation.error.clone();
            // Checksum/parity is a status overlay. The bytes are still useful
            // when the complete frame payload is present, but a short payload
            // must never be promoted to a valid signal value.
            if status == DecodeStatus::Ok {
                if let Some(checksum) = observation.checksum {
                    let model = observation.checksum_model.unwrap_or(frame_checksum);
                    if let Err(error) =
                        crate::validate_lin_checksum(frame_id, payload, checksum, model)
                    {
                        status = DecodeStatus::ChecksumError;
                        diagnostic = Some(error.to_string());
                    }
                }
            }
            let dynamic_groups: Vec<_> = if *diagnostic_frame {
                Vec::new()
            } else {
                database.dynamic_frames_for_id(frame_id).collect()
            };
            if !dynamic_groups.is_empty() {
                let groups = dynamic_groups
                    .iter()
                    .map(|dynamic| {
                        let kind = match dynamic.kind {
                            crate::LinDynamicFrameKind::Sporadic => "sporadic",
                            crate::LinDynamicFrameKind::EventTriggered => "event-triggered",
                        };
                        let trigger = dynamic
                            .trigger_frame_id
                            .map_or_else(|| "unresolved".to_string(), |id| format!("0x{id:02X}"));
                        format!("{kind} {:?} (trigger={trigger})", dynamic.name)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let dynamic_diagnostic = format!(
                    "LIN frame 0x{frame_id:02X} response trigger context is not present in BLF record; groups: {groups}"
                );
                if status == DecodeStatus::Ok {
                    status = DecodeStatus::DynamicFrame;
                    diagnostic = Some(dynamic_diagnostic);
                } else {
                    diagnostic = Some(match diagnostic {
                        Some(existing) => format!("{existing}; {dynamic_diagnostic}"),
                        None => dynamic_diagnostic,
                    });
                }
            }
            match decoded {
                Ok(Some(value)) => results.push(Ok(lin_sample(
                    timestamp_ns,
                    channel,
                    frame_id,
                    &frame_name,
                    value,
                    payload,
                    status,
                    diagnostic,
                    context.clone(),
                ))),
                Ok(None) => {}
                Err(error) => results.push(Ok(error_sample(
                    timestamp_ns,
                    BusKind::Lin,
                    channel,
                    u32::from(frame_id),
                    message_name,
                    signal_name,
                    lin_decode_status(&error),
                    error.to_string(),
                    payload,
                )
                .with_context(context.clone()))),
            }
        }
        results
    }
}

impl File {
    pub fn signal_query(
        &self,
        binding: DatabaseBinding,
        signal_name: impl Into<String>,
        strict: bool,
    ) -> Result<SignalQuery, QueryError> {
        if !self.is_open() {
            return Err(QueryError::ReaderClosed);
        }
        SignalQuery::new(self, vec![binding], signal_name, strict)
    }

    pub fn aggregate_signal_query(
        &self,
        bindings: Vec<DatabaseBinding>,
        signal_name: impl Into<String>,
        strict: bool,
    ) -> Result<SignalQuery, QueryError> {
        if !self.is_open() {
            return Err(QueryError::ReaderClosed);
        }
        SignalQuery::aggregate(self, bindings, signal_name, strict)
    }

    pub fn signal_queries(
        &self,
        binding: DatabaseBinding,
        signal_names: Vec<String>,
        strict: bool,
    ) -> Result<SignalQuery, QueryError> {
        if !self.is_open() {
            return Err(QueryError::ReaderClosed);
        }
        SignalQuery::new_many(self, vec![binding], signal_names, strict)
    }

    pub fn aggregate_signal_queries(
        &self,
        bindings: Vec<DatabaseBinding>,
        signal_names: Vec<String>,
        strict: bool,
    ) -> Result<SignalQuery, QueryError> {
        if !self.is_open() {
            return Err(QueryError::ReaderClosed);
        }
        SignalQuery::aggregate_many(self, bindings, signal_names, strict)
    }
}

fn find_can_signals<'a>(
    database: &'a DbcDatabase,
    name: &str,
    aggregate: bool,
) -> Vec<(&'a DbcMessage, &'a crate::database::CanSignal)> {
    if let Some(value) = database.signal(name) {
        return vec![value];
    }
    if aggregate {
        database
            .messages()
            .flat_map(|message| {
                message
                    .signals
                    .iter()
                    .filter(|signal| signal.name == name)
                    .map(move |signal| (message, signal))
            })
            .collect()
    } else {
        Vec::new()
    }
}

fn find_lin_signals(database: &LdfDatabase, name: &str, aggregate: bool) -> Vec<LinSignalTarget> {
    if let Some(value) = database.signal(name) {
        return vec![LinSignalTarget {
            frame_id: value.0.id,
            diagnostic: false,
            message_name: value.0.name.clone(),
            signal_name: value.1.name.clone(),
        }];
    }
    if let Some(value) = database.diagnostic_signal(name) {
        return vec![LinSignalTarget {
            frame_id: value.0.id,
            diagnostic: true,
            message_name: value.0.name.clone(),
            signal_name: value.1.name.clone(),
        }];
    }
    if aggregate {
        let ordinary = database.frames().flat_map(|frame| {
            frame
                .signals
                .iter()
                .filter(|signal| signal.name == name)
                .map(move |signal| LinSignalTarget {
                    frame_id: frame.id,
                    diagnostic: false,
                    message_name: frame.name.clone(),
                    signal_name: signal.name.clone(),
                })
        });
        let diagnostic = database.diagnostic_frames().flat_map(|frame| {
            frame
                .signals
                .iter()
                .filter(|signal| signal.name == name)
                .map(move |signal| LinSignalTarget {
                    frame_id: frame.id,
                    diagnostic: true,
                    message_name: frame.name.clone(),
                    signal_name: signal.name.clone(),
                })
        });
        ordinary.chain(diagnostic).collect()
    } else {
        Vec::new()
    }
}

fn normalize_can_id(raw_id: u32) -> (u32, bool) {
    let extended = raw_id & 0x8000_0000 != 0;
    (
        if extended {
            raw_id & 0x1FFF_FFFF
        } else {
            raw_id
        },
        extended,
    )
}

fn can_status(error: &CanDecodeError) -> DecodeStatus {
    match error {
        CanDecodeError::PayloadTooShort { .. } => DecodeStatus::PayloadTooShort,
        _ => DecodeStatus::DecodeError,
    }
}

fn lin_status(state: LinResponseState) -> DecodeStatus {
    match state {
        LinResponseState::Response => DecodeStatus::Ok,
        LinResponseState::HeaderOnly => DecodeStatus::HeaderOnly,
        LinResponseState::NoResponse => DecodeStatus::NoResponse,
        LinResponseState::ChecksumError => DecodeStatus::ChecksumError,
        LinResponseState::ParityError => DecodeStatus::ParityError,
        LinResponseState::SendError => DecodeStatus::SendError,
        LinResponseState::ReceiveError => DecodeStatus::ReceiveError,
        LinResponseState::Unsupported => DecodeStatus::DecodeError,
    }
}

fn lin_decode_status(error: &LinDecodeError) -> DecodeStatus {
    match error {
        LinDecodeError::PayloadTooShort { .. } => DecodeStatus::PayloadTooShort,
        LinDecodeError::ChecksumMismatch { .. } => DecodeStatus::ChecksumError,
        LinDecodeError::InvalidResponse { state } => lin_status(*state),
        _ => DecodeStatus::DecodeError,
    }
}

fn decode_lin_signal_for_dlc(
    dlc: u8,
    signal: &LinSignal,
    payload: &[u8],
) -> Result<DecodedLinSignal, LinDecodeError> {
    if payload.len() < usize::from(dlc) {
        return Err(LinDecodeError::PayloadTooShort {
            required: usize::from(dlc),
            actual: payload.len(),
        });
    }
    decode_lin_signal(signal, payload)
}

fn can_sample(
    timestamp_ns: u64,
    channel: u16,
    frame_id: u32,
    message: &DbcMessage,
    decoded: DecodedCanSignal,
    data: &[u8],
    context: SampleContext,
) -> SignalSample {
    SignalSample::ok(
        timestamp_ns,
        BusKind::Can,
        channel,
        frame_id,
        &message.name,
        decoded.signal_name,
        decoded.raw_value,
        decoded.physical_value,
        sample_value(decoded.physical_value, decoded.value_text.clone()),
        decoded.value_text,
        data,
    )
    .with_context(context)
}

#[allow(clippy::too_many_arguments)]
fn lin_sample(
    timestamp_ns: u64,
    channel: u16,
    frame_id: u8,
    frame_name: &str,
    decoded: DecodedLinSignal,
    data: &[u8],
    status: DecodeStatus,
    error: Option<String>,
    context: SampleContext,
) -> SignalSample {
    let mut sample = SignalSample::ok(
        timestamp_ns,
        BusKind::Lin,
        channel,
        u32::from(frame_id),
        frame_name,
        decoded.signal_name,
        decoded.raw_value,
        decoded.physical_value,
        sample_value(decoded.physical_value, decoded.value_text.clone()),
        decoded.value_text,
        data,
    );
    sample.status = status;
    if status.is_error() {
        sample.error = Some(error.unwrap_or_else(|| status.to_string()));
    }
    sample.with_context(context)
}

fn sample_value(physical: f64, text: Option<String>) -> SampleValue {
    text.map_or(SampleValue::Physical(physical), SampleValue::Text)
}

#[allow(clippy::too_many_arguments)]
fn error_sample(
    timestamp_ns: u64,
    bus: BusKind,
    channel: u16,
    frame_id: u32,
    message_name: &str,
    signal_name: &str,
    status: DecodeStatus,
    error: String,
    data: &[u8],
) -> SignalSample {
    SignalSample::error(
        timestamp_ns,
        bus,
        channel,
        frame_id,
        message_name,
        signal_name,
        status,
        error,
        data,
    )
}

fn direction_name(value: u8) -> &'static str {
    match value {
        0 => "Rx",
        1 => "Tx",
        2 => "TxRq",
        _ => "Unknown",
    }
}

fn can_direction_name(value: u8) -> &'static str {
    match value {
        0 => "Rx",
        1 => "Tx",
        _ => "Unknown",
    }
}

fn can_context(
    object: &Object,
    dlc: Option<u8>,
    payload_length: u8,
    raw_direction: Option<u8>,
) -> SampleContext {
    SampleContext {
        object_type: object.object_type().as_u32(),
        object_class: object.object_type().name().to_string(),
        direction: raw_direction.map(can_direction_name).map(str::to_string),
        raw_direction,
        dlc,
        payload_length: usize::from(payload_length),
        timestamp_source: object_timestamp_source(object).map(str::to_string),
        ..SampleContext::default()
    }
}

fn lin_context(
    object: &LinObject,
    observation: &crate::lin::LinObservation,
    _timestamp_ns: u64,
) -> SampleContext {
    let databyte_timestamps = field_u64_vec(object, "databyteTimestamps");
    SampleContext {
        object_type: object.object_type.as_u32(),
        object_class: object.object_type.name().to_string(),
        direction: observation
            .direction
            .map(direction_name)
            .map(str::to_string),
        raw_direction: observation.direction,
        dlc: observation.header_dlc,
        header_dlc: observation.header_dlc,
        payload_length: observation.payload.as_ref().map_or(0, Vec::len),
        // Vector stores these as raw per-byte values in the LIN object. Their
        // unit/base is format-version dependent and is not assumed here.
        signal_timestamp_ns: None,
        databyte_timestamps,
        ..SampleContext::default()
    }
}

fn field_u64_vec(object: &LinObject, name: &str) -> Vec<u64> {
    object
        .fields
        .iter()
        .find_map(|(field, value)| {
            if field != name {
                return None;
            }
            match value {
                FieldValue::U64Vec(values) => Some(values.clone()),
                _ => Some(Vec::new()),
            }
        })
        .unwrap_or_default()
}

fn field_u16(object: &LinObject, name: &str) -> Option<u16> {
    object.fields.iter().find_map(|(field, value)| {
        if field != name {
            return None;
        }
        match value {
            FieldValue::U8(value) => Some(u16::from(*value)),
            FieldValue::U16(value) => Some(*value),
            FieldValue::U32(value) => u16::try_from(*value).ok(),
            _ => None,
        }
    })
}
