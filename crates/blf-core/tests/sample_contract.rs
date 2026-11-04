use blf_core::{BusKind, DecodeStatus, SampleContext, SampleValue, SignalSample};

#[test]
fn successful_sample_exposes_raw_physical_and_choice_values() {
    let sample = SignalSample::ok(
        8_025_000_002,
        BusKind::Can,
        3,
        0x123,
        "VehicleStatus",
        "VehicleSpeed",
        42,
        4.2,
        SampleValue::Text("Cruising".to_string()),
        Some("Cruising".to_string()),
        vec![0x2A, 0x00],
    );

    assert_eq!(sample.timestamp_ns, 8_025_000_002);
    assert_eq!(sample.raw_value, Some(42));
    assert_eq!(sample.physical_value, Some(4.2));
    assert_eq!(
        sample.value,
        Some(SampleValue::Text("Cruising".to_string()))
    );
    assert_eq!(sample.value_text.as_deref(), Some("Cruising"));
    assert_eq!(sample.status, DecodeStatus::Ok);
    assert!(!sample.is_error());
}

#[test]
fn error_sample_has_explicit_null_values_and_owned_payload() {
    let mut sample = SignalSample::error(
        9,
        BusKind::Lin,
        1,
        0x22,
        "BodyStatus",
        "DoorState",
        DecodeStatus::NoResponse,
        "LIN response payload is unavailable",
        vec![0x22],
    );
    let original_error = sample.error.clone();
    let original_data = sample.data.clone();
    sample.data[0] = 0xFF;

    assert!(sample.raw_value.is_none());
    assert!(sample.physical_value.is_none());
    assert!(sample.value.is_none());
    assert!(sample.value_text.is_none());
    assert_eq!(sample.error, original_error);
    assert_ne!(sample.data, original_data);
    assert!(sample.is_error());
    assert!(!sample.has_value());
}

#[test]
fn sample_value_keeps_numeric_values_distinct_from_choice_text() {
    assert_eq!(SampleValue::Raw(-1), SampleValue::Raw(-1));
    assert_eq!(SampleValue::Physical(1.5), SampleValue::Physical(1.5));
    assert_eq!(SampleValue::Text("On".to_string()).as_text(), Some("On"));
    assert_eq!(SampleValue::Raw(1).as_text(), None);
    assert_eq!(DecodeStatus::ChecksumError.to_string(), "checksum_error");
}

#[test]
fn sample_contract_preserves_unsigned_64_bit_raw_values() {
    let sample = SignalSample::ok(
        1,
        BusKind::Can,
        1,
        1,
        "Status",
        "Counter",
        i128::from(u64::MAX),
        1.0,
        SampleValue::Physical(1.0),
        None,
        [],
    );
    assert_eq!(sample.raw_value, Some(i128::from(u64::MAX)));
}

#[test]
fn sample_context_preserves_protocol_and_direction_metadata() {
    let sample = SignalSample::ok(
        10,
        BusKind::Lin,
        3,
        1,
        "Status",
        "State",
        1,
        1.0,
        SampleValue::Physical(1.0),
        None,
        [1],
    )
    .with_context(SampleContext {
        object_type: 57,
        object_class: "LinMessage2".to_string(),
        direction: Some("Tx".to_string()),
        raw_direction: Some(1),
        dlc: Some(2),
        header_dlc: Some(2),
        payload_length: 1,
        timestamp_source: Some("nanoseconds".to_string()),
        signal_timestamp_ns: None,
        databyte_timestamps: vec![11, 22],
    });

    assert_eq!(sample.object_type, 57);
    assert_eq!(sample.object_class, "LinMessage2");
    assert_eq!(sample.direction.as_deref(), Some("Tx"));
    assert_eq!(sample.raw_direction, Some(1));
    assert!(sample.is_tx);
    assert!(!sample.is_rx);
    assert_eq!(sample.dlc, Some(2));
    assert_eq!(sample.header_dlc, Some(2));
    assert_eq!(sample.payload_length, 1);
    assert_eq!(sample.databyte_timestamps, vec![11, 22]);
}
