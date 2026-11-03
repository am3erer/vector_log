use blf_core::{
    classify_lin_object, FieldValue, LinObject, LinResponseState, ObjectHeader, ObjectHeaderBase,
    ObjectType,
};

fn object(object_type: ObjectType, fields: Vec<(&str, FieldValue)>) -> LinObject {
    LinObject {
        object_type,
        header: ObjectHeader {
            base: ObjectHeaderBase {
                signature: 0,
                header_size: 32,
                header_version: 1,
                object_size: 0,
                object_type,
                raw_object_type: object_type.as_u32(),
            },
            object_flags: 0,
            client_index: 0,
            object_version: 0,
            object_time_stamp: 0,
        },
        fields: fields
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
        extensions: Vec::new(),
        raw_data: Vec::new(),
    }
}

#[test]
fn normal_response_uses_dlc_to_bound_payload() {
    let observation = classify_lin_object(&object(
        ObjectType::LinMessage2,
        vec![
            ("id", FieldValue::U8(1)),
            ("dlc", FieldValue::U8(2)),
            ("dir", FieldValue::U8(1)),
            ("data", FieldValue::Bytes(vec![0xFD, 0xFE, 0xFF, 0x00])),
        ],
    ));
    assert_eq!(observation.state, LinResponseState::Response);
    assert_eq!(observation.frame_id, Some(1));
    assert_eq!(observation.header_dlc, Some(2));
    assert_eq!(observation.payload, Some(vec![0xFD, 0xFE]));
    assert!(observation.has_safe_payload());
}

#[test]
fn zero_dlc_is_header_only_and_has_no_value_payload() {
    let observation = classify_lin_object(&object(
        ObjectType::LinMessage2,
        vec![
            ("id", FieldValue::U8(2)),
            ("dlc", FieldValue::U8(0)),
            ("data", FieldValue::Bytes(vec![0, 0, 0, 0])),
        ],
    ));
    assert_eq!(observation.state, LinResponseState::HeaderOnly);
    assert_eq!(observation.payload, None);
    assert!(!observation.has_safe_payload());
}

#[test]
fn send_error_dlc_does_not_create_a_payload_or_direction() {
    let observation = classify_lin_object(&object(
        ObjectType::LinSndError2,
        vec![
            ("id", FieldValue::U8(3)),
            ("dlc", FieldValue::U8(2)),
            ("dir", FieldValue::U8(1)),
        ],
    ));
    assert_eq!(observation.state, LinResponseState::SendError);
    assert_eq!(observation.header_dlc, Some(2));
    assert_eq!(observation.payload, None);
    assert_eq!(observation.direction, None);
    assert!(!observation.has_safe_payload());
}

#[test]
fn checksum_error_keeps_safely_extractable_bytes() {
    let observation = classify_lin_object(&object(
        ObjectType::LinCrcError2,
        vec![
            ("id", FieldValue::U8(4)),
            ("dlc", FieldValue::U8(2)),
            ("data", FieldValue::Bytes(vec![1, 2, 3])),
        ],
    ));
    assert_eq!(observation.state, LinResponseState::ChecksumError);
    assert_eq!(observation.payload, Some(vec![1, 2]));
    assert!(observation.has_safe_payload());
    assert!(observation.error.unwrap().contains("checksum"));
}

#[test]
fn parity_error_can_downgrade_a_response_without_dropping_bytes() {
    let observation = classify_lin_object(&object(
        ObjectType::LinMessage,
        vec![
            ("id", FieldValue::U8(5)),
            ("dlc", FieldValue::U8(1)),
            ("data", FieldValue::Bytes(vec![9])),
        ],
    ))
    .mark_parity_error("protected identifier parity mismatch");
    assert_eq!(observation.state, LinResponseState::ParityError);
    assert_eq!(observation.payload, Some(vec![9]));
    assert!(observation.has_safe_payload());
}

#[test]
fn explicit_protected_identifier_is_validated_before_extraction() {
    let observation = classify_lin_object(&object(
        ObjectType::LinMessage,
        vec![
            ("id", FieldValue::U8(1)),
            ("protectedId", FieldValue::U8(0x41)),
            ("dlc", FieldValue::U8(1)),
            ("data", FieldValue::Bytes(vec![9])),
        ],
    ));
    assert_eq!(observation.state, LinResponseState::ParityError);
    assert_eq!(observation.frame_id, Some(1));
    assert_eq!(observation.payload, Some(vec![9]));
    assert!(observation.has_safe_payload());
    assert!(observation.error.unwrap().contains("protected identifier"));
}

#[test]
fn unrelated_lin_event_is_not_treated_as_a_response() {
    let observation = classify_lin_object(&object(ObjectType::LinWakeup2, vec![]));
    assert_eq!(observation.state, LinResponseState::Unsupported);
    assert_eq!(observation.payload, None);
}

#[test]
fn receive_error_preserves_protocol_state_reason_without_guessing_cause() {
    let observation = classify_lin_object(&object(
        ObjectType::LinRcvError2,
        vec![
            ("id", FieldValue::U8(5)),
            ("dlc", FieldValue::U8(1)),
            ("stateReason", FieldValue::U8(0x13)),
            ("hasDatabytes", FieldValue::U8(0)),
        ],
    ));
    assert_eq!(observation.state, LinResponseState::ReceiveError);
    assert!(observation.error.unwrap().contains("0x13"));
}
