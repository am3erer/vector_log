//! Lazy, database-backed signal queries over independent BLF cursors.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use thiserror::Error;

use crate::can::{decode_signal_for_message, CanDecodeError, DecodedCanSignal};
use crate::database::{
    BusKind, Database, DatabaseBinding, DatabaseError, DbcDatabase, DbcMessage, LdfDatabase,
    LinFrame, LinSignal,
};
use crate::ldf::{decode_lin_signal, DecodedLinSignal, LinDecodeError};
use crate::lin::{classify_lin_object, LinObservation, LinResponseState};
use crate::reader::File;
use crate::sample::{DecodeStatus, SampleValue, SignalSample};
use crate::types::{FieldValue, LinObject, Object};
use crate::{object_timestamp_ns, BlfError};

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
        signal_name: String,
    },
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
    signal_name: String,
    strict: bool,
    pending: VecDeque<Result<SignalSample, QueryError>>,
    terminated: bool,
}

impl std::fmt::Debug for SignalQuery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SignalQuery")
            .field("signal_name", &self.signal_name)
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
        Self::new_inner(source, bindings, signal_name.into(), strict, false)
    }

    pub fn aggregate(
        source: &File,
        bindings: Vec<DatabaseBinding>,
        signal_name: impl Into<String>,
        strict: bool,
    ) -> Result<Self, QueryError> {
        Self::new_inner(source, bindings, signal_name.into(), strict, true)
    }

    fn new_inner(
        source: &File,
        bindings: Vec<DatabaseBinding>,
        signal_name: String,
        strict: bool,
        aggregate: bool,
    ) -> Result<Self, QueryError> {
        let first_binding = bindings.first().cloned();
        let mut targets = Vec::new();
        let mut seen_bindings = HashSet::new();
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
                    for (message, signal) in find_can_signals(database, &signal_name, aggregate) {
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
                Database::Lin(database) => {
                    for (frame, signal) in find_lin_signals(database, &signal_name, aggregate) {
                        targets.push(BoundTarget {
                            channel: binding.channel(),
                            target: QueryTarget::Lin {
                                database: database.clone(),
                                frame_id: frame.id,
                                signal_name: signal.name.clone(),
                            },
                        });
                    }
                }
            }
        }
        if targets.is_empty() {
            let (bus, channel) = first_binding
                .map(|binding| (binding.bus(), binding.channel()))
                .unwrap_or((BusKind::Can, 0));
            return Err(QueryError::UnknownSignal {
                signal: signal_name,
                bus,
                channel,
            });
        }
        Ok(Self {
            cursor: source.independent_cursor(),
            targets,
            signal_name,
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
                ),
                Object::CanMessage2(value) => self.decode_can(
                    timestamp_ns,
                    value.channel,
                    value.id,
                    &value.data[..usize::from(value.dlc).min(value.data.len())],
                ),
                Object::CanFdMessage(value) => self.decode_can(
                    timestamp_ns,
                    value.channel,
                    value.id,
                    &value.data[..usize::from(value.valid_data_bytes).min(value.data.len())],
                ),
                Object::CanFdMessage64(value) => self.decode_can(
                    timestamp_ns,
                    u16::from(value.channel),
                    value.id,
                    &value.data[..usize::from(value.valid_data_bytes).min(value.data.len())],
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
                )));
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
                ))),
                Ok(None) => results.push(Ok(error_sample(
                    timestamp_ns,
                    BusKind::Can,
                    channel,
                    id,
                    message_name,
                    signal_name,
                    DecodeStatus::DecodeError,
                    format!("CAN multiplex signal {signal_name:?} is inactive"),
                    data,
                ))),
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
                ))),
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
                QueryTarget::Lin { database, .. } if database.frame_by_id(frame_id).is_some()
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
                )));
            }
            return results;
        }
        for target in channel_targets {
            let QueryTarget::Lin {
                frame_id: target_frame_id,
                database,
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
            let Some(frame) = database.frame_by_id(frame_id) else {
                continue;
            };
            if !observation.state.has_response() {
                results.push(Ok(error_sample(
                    timestamp_ns,
                    BusKind::Lin,
                    channel,
                    u32::from(frame_id),
                    &frame.name,
                    signal_name,
                    lin_status(observation.state),
                    observation
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("LIN state: {:?}", observation.state)),
                    payload,
                )));
                continue;
            }
            let decoded = if observation.state == LinResponseState::ChecksumError {
                frame
                    .signal(signal_name)
                    .map(|signal| decode_lin_signal(signal, payload))
                    .transpose()
            } else {
                decode_lin_frame_for_signal(database, frame, frame_id, &observation, signal_name)
            };
            match decoded {
                Ok(Some(value)) => results.push(Ok(lin_sample(
                    timestamp_ns,
                    channel,
                    frame_id,
                    frame,
                    value,
                    payload,
                    lin_status(observation.state),
                ))),
                Ok(None) => {}
                Err(error) => results.push(Ok(error_sample(
                    timestamp_ns,
                    BusKind::Lin,
                    channel,
                    u32::from(frame_id),
                    &frame.name,
                    signal_name,
                    lin_decode_status(&error),
                    error.to_string(),
                    payload,
                ))),
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
        SignalQuery::new(self, vec![binding], signal_name, strict)
    }

    pub fn aggregate_signal_query(
        &self,
        bindings: Vec<DatabaseBinding>,
        signal_name: impl Into<String>,
        strict: bool,
    ) -> Result<SignalQuery, QueryError> {
        SignalQuery::aggregate(self, bindings, signal_name, strict)
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

fn find_lin_signals<'a>(
    database: &'a LdfDatabase,
    name: &str,
    aggregate: bool,
) -> Vec<(&'a LinFrame, &'a LinSignal)> {
    if let Some(value) = database.signal(name) {
        return vec![value];
    }
    if aggregate {
        database
            .frames()
            .flat_map(|frame| {
                frame
                    .signals
                    .iter()
                    .filter(|signal| signal.name == name)
                    .map(move |signal| (frame, signal))
            })
            .collect()
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

fn decode_lin_frame_for_signal(
    _database: &LdfDatabase,
    frame: &LinFrame,
    frame_id: u8,
    observation: &LinObservation,
    signal_name: &str,
) -> Result<Option<DecodedLinSignal>, LinDecodeError> {
    let payload = observation
        .payload
        .as_deref()
        .ok_or(LinDecodeError::InvalidResponse {
            state: observation.state,
        })?;
    if payload.len() < usize::from(frame.dlc) {
        return Err(LinDecodeError::PayloadTooShort {
            required: usize::from(frame.dlc),
            actual: payload.len(),
        });
    }
    // Keep checksum validation in the common decoder. Checksum-error objects
    // are handled above so their safely extractable signal value is retained.
    if let Some(checksum) = observation.checksum {
        let model = observation.checksum_model.unwrap_or(frame.checksum);
        crate::validate_lin_checksum(frame_id, payload, checksum, model)?;
    }
    frame
        .signal(signal_name)
        .map(|signal| decode_lin_signal(signal, payload))
        .transpose()
}

fn can_sample(
    timestamp_ns: u64,
    channel: u16,
    frame_id: u32,
    message: &DbcMessage,
    decoded: DecodedCanSignal,
    data: &[u8],
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
}

fn lin_sample(
    timestamp_ns: u64,
    channel: u16,
    frame_id: u8,
    frame: &LinFrame,
    decoded: DecodedLinSignal,
    data: &[u8],
    status: DecodeStatus,
) -> SignalSample {
    let mut sample = SignalSample::ok(
        timestamp_ns,
        BusKind::Lin,
        channel,
        u32::from(frame_id),
        &frame.name,
        decoded.signal_name,
        decoded.raw_value,
        decoded.physical_value,
        sample_value(decoded.physical_value, decoded.value_text.clone()),
        decoded.value_text,
        data,
    );
    sample.status = status;
    if status.is_error() {
        sample.error = Some(status.to_string());
    }
    sample
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
