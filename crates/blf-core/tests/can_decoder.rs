use blf_core::{
    decode_message, decode_signal, decode_signal_for_message, message_for_id, ByteOrder,
    CanDecodeError, CanMessageId, CanPayloadKind, CanSignal, DbcDatabase, DbcMessage,
    MultiplexRole, SignalType,
};

fn signal(name: &str, start: u16, length: u16, order: ByteOrder, kind: SignalType) -> CanSignal {
    CanSignal::new(name, start, length, order, kind)
}

#[test]
fn decodes_little_endian_unsigned_and_signed_values() {
    let unsigned = signal(
        "Unsigned",
        4,
        12,
        ByteOrder::LittleEndian,
        SignalType::Unsigned,
    );
    assert_eq!(
        decode_signal(&unsigned, &[0xF0, 0x0A]).unwrap().raw_value,
        0x0AF
    );

    let signed = signal("Signed", 0, 8, ByteOrder::LittleEndian, SignalType::Signed);
    assert_eq!(decode_signal(&signed, &[0xFE]).unwrap().raw_value, -2);
}

#[test]
fn decodes_motorola_signals_across_byte_boundaries() {
    let one_byte = signal("Big", 15, 8, ByteOrder::BigEndian, SignalType::Unsigned);
    assert_eq!(
        decode_signal(&one_byte, &[0x12, 0xA5]).unwrap().raw_value,
        0xA5
    );

    let crossing = signal(
        "Crossing",
        11,
        9,
        ByteOrder::BigEndian,
        SignalType::Unsigned,
    );
    let decoded = decode_signal(&crossing, &[0x05, 0x0B, 0x38]).unwrap();
    assert_eq!(decoded.raw_value, 0x167);
}

#[test]
fn applies_scaling_limits_and_choice_text() {
    let mut value = signal("Mode", 0, 2, ByteOrder::LittleEndian, SignalType::Unsigned);
    value.factor = 0.5;
    value.offset = 1.0;
    value.minimum = Some(1.0);
    value.maximum = Some(2.5);
    value.choices = blf_core::ValueTable::new([(3, "Active".to_string())]);
    let decoded = decode_signal(&value, &[3]).unwrap();
    assert_eq!(decoded.raw_value, 3);
    assert_eq!(decoded.physical_value, 2.5);
    assert_eq!(decoded.value_text.as_deref(), Some("Active"));
}

#[test]
fn rejects_short_payload_and_out_of_range_physical_value() {
    let value = signal("Value", 0, 8, ByteOrder::LittleEndian, SignalType::Unsigned);
    let message = DbcMessage {
        id: CanMessageId::new(1, false),
        name: "Message".to_string(),
        dlc: 8,
        is_fd: false,
        payload_kind: CanPayloadKind::Physical,
        signals: vec![value.clone()],
        comment: None,
        attributes: Default::default(),
        source: None,
    };
    assert!(matches!(
        decode_message(&message, &[1]),
        Err(CanDecodeError::PayloadTooShort { .. })
    ));

    let mut limited = value;
    limited.maximum = Some(0.0);
    assert!(matches!(
        decode_signal(&limited, &[1]),
        Err(CanDecodeError::ValueOutOfRange { .. })
    ));
}

#[test]
fn rejects_non_finite_or_reversed_signal_metadata() {
    let mut non_finite = signal("Value", 0, 8, ByteOrder::LittleEndian, SignalType::Unsigned);
    non_finite.factor = f64::NAN;
    assert!(matches!(
        decode_signal(&non_finite, &[1]),
        Err(CanDecodeError::InvalidMetadata { .. })
    ));

    let mut reversed = signal("Value", 0, 8, ByteOrder::LittleEndian, SignalType::Unsigned);
    reversed.minimum = Some(10.0);
    reversed.maximum = Some(1.0);
    assert!(matches!(
        decode_signal(&reversed, &[1]),
        Err(CanDecodeError::InvalidMetadata { .. })
    ));
}

#[test]
fn preserves_unsigned_64_bit_values_above_i64() {
    let value = signal("Wide", 0, 64, ByteOrder::LittleEndian, SignalType::Unsigned);
    assert_eq!(
        decode_signal(&value, &[0xFF; 8]).unwrap().raw_value,
        i128::from(u64::MAX)
    );
}

#[test]
fn logical_vector_message_uses_signal_width_for_physical_query_decode() {
    let message = DbcMessage {
        id: CanMessageId::new(0x123, false),
        name: "CLMIF".to_string(),
        dlc: 196,
        is_fd: false,
        payload_kind: CanPayloadKind::LogicalContainer,
        signals: vec![signal(
            "Selector",
            0,
            8,
            ByteOrder::LittleEndian,
            SignalType::Unsigned,
        )],
        comment: None,
        attributes: Default::default(),
        source: None,
    };
    let decoded = decode_signal_for_message(&message, "Selector", &[7])
        .unwrap()
        .unwrap();
    assert_eq!(decoded.raw_value, 7);
    assert_eq!(decode_message(&message, &[7]).unwrap()[0].raw_value, 7);
}

#[test]
fn physical_oversized_message_still_requires_the_declared_payload() {
    let message = DbcMessage {
        id: CanMessageId::new(0x123, false),
        name: "MalformedPhysical".to_string(),
        dlc: 196,
        is_fd: false,
        payload_kind: CanPayloadKind::Physical,
        signals: vec![signal(
            "Selector",
            0,
            8,
            ByteOrder::LittleEndian,
            SignalType::Unsigned,
        )],
        comment: None,
        attributes: Default::default(),
        source: None,
    };
    assert!(matches!(
        decode_signal_for_message(&message, "Selector", &[7]),
        Err(CanDecodeError::PayloadTooShort { required: 196, .. })
    ));
}

#[test]
fn targeted_decode_ignores_unrelated_invalid_signals() {
    let mut requested = signal(
        "Requested",
        0,
        8,
        ByteOrder::LittleEndian,
        SignalType::Unsigned,
    );
    requested.maximum = Some(255.0);
    let mut unrelated = signal(
        "Unrelated",
        8,
        16,
        ByteOrder::LittleEndian,
        SignalType::Unsigned,
    );
    unrelated.maximum = Some(1.0);
    let message = DbcMessage {
        id: CanMessageId::new(0x123, false),
        name: "Status".to_string(),
        dlc: 1,
        is_fd: false,
        payload_kind: CanPayloadKind::Physical,
        signals: vec![requested, unrelated],
        comment: None,
        attributes: Default::default(),
        source: None,
    };

    let decoded = decode_signal_for_message(&message, "Requested", &[42]).unwrap();
    assert_eq!(decoded.unwrap().raw_value, 42);
}

#[test]
fn multiplex_decodes_selector_and_only_active_branch() {
    let selector = signal("Mux", 0, 4, ByteOrder::LittleEndian, SignalType::Unsigned);
    let mut active = signal(
        "Active",
        8,
        8,
        ByteOrder::LittleEndian,
        SignalType::Unsigned,
    );
    active.multiplex = MultiplexRole::Branch([1].into_iter().collect());
    let mut inactive = signal(
        "Inactive",
        16,
        8,
        ByteOrder::LittleEndian,
        SignalType::Unsigned,
    );
    inactive.multiplex = MultiplexRole::Branch([2].into_iter().collect());
    let message = DbcMessage {
        id: CanMessageId::new(2, false),
        name: "Muxed".to_string(),
        dlc: 3,
        is_fd: false,
        payload_kind: CanPayloadKind::Physical,
        signals: vec![
            CanSignal {
                multiplex: MultiplexRole::Selector,
                ..selector
            },
            active,
            inactive,
        ],
        comment: None,
        attributes: Default::default(),
        source: None,
    };
    let decoded = decode_message(&message, &[1, 0xAA, 0xBB]).unwrap();
    assert_eq!(
        decoded
            .iter()
            .map(|value| value.signal_name.as_str())
            .collect::<Vec<_>>(),
        vec!["Mux", "Active"]
    );
}

#[test]
fn rejects_invalid_and_ambiguous_multiplex_selectors() {
    let mut selector = signal("Mux", 0, 8, ByteOrder::LittleEndian, SignalType::Signed);
    selector.multiplex = MultiplexRole::Selector;
    let mut branch = signal(
        "Branch",
        8,
        8,
        ByteOrder::LittleEndian,
        SignalType::Unsigned,
    );
    branch.multiplex = MultiplexRole::Branch([0].into_iter().collect());
    let message = DbcMessage {
        id: CanMessageId::new(4, false),
        name: "InvalidMux".to_string(),
        dlc: 2,
        is_fd: false,
        payload_kind: CanPayloadKind::Physical,
        signals: vec![selector.clone(), branch.clone()],
        comment: None,
        attributes: Default::default(),
        source: None,
    };
    assert!(matches!(
        decode_signal_for_message(&message, "Branch", &[0xFF, 1]),
        Err(CanDecodeError::InvalidMultiplexSelector { value: -1, .. })
    ));

    let mut second_selector = selector;
    second_selector.name = "Mux2".to_string();
    second_selector.start_bit = 2;
    let ambiguous = DbcMessage {
        signals: vec![selector_from_message(&message), second_selector, branch],
        ..message
    };
    assert!(matches!(
        decode_signal_for_message(&ambiguous, "Branch", &[1, 1]),
        Err(CanDecodeError::MultipleMultiplexSelectors { .. })
    ));
}

#[test]
fn rejects_common_signal_when_multiplex_branches_have_no_selector() {
    let mut branch = signal(
        "Branch",
        0,
        8,
        ByteOrder::LittleEndian,
        SignalType::Unsigned,
    );
    branch.multiplex = MultiplexRole::Branch([1].into_iter().collect());
    let message = DbcMessage {
        id: CanMessageId::new(5, false),
        name: "MalformedMux".to_string(),
        dlc: 1,
        is_fd: false,
        payload_kind: CanPayloadKind::Physical,
        signals: vec![
            signal(
                "Common",
                0,
                8,
                ByteOrder::LittleEndian,
                SignalType::Unsigned,
            ),
            branch,
        ],
        comment: None,
        attributes: Default::default(),
        source: None,
    };
    assert!(matches!(
        decode_signal_for_message(&message, "Common", &[1]),
        Err(CanDecodeError::MissingMultiplexSelector { .. })
    ));
}

fn selector_from_message(message: &DbcMessage) -> CanSignal {
    message
        .signals
        .iter()
        .find(|signal| signal.multiplex == MultiplexRole::Selector)
        .cloned()
        .expect("test message has a selector")
}

#[test]
fn message_lookup_requires_the_correct_standard_or_extended_form() {
    let database =
        DbcDatabase::from_str("VERSION \"1.0\"\nBO_ 2147483655 Extended: 8 ECU\n").unwrap();
    assert!(message_for_id(&database, 7, true).is_ok());
    assert!(matches!(
        message_for_id(&database, 7, false),
        Err(CanDecodeError::UnknownMessage { .. })
    ));
}
