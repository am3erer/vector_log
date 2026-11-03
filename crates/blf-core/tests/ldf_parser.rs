use blf_core::{
    decode_lin_frame, decode_lin_signal, protected_identifier, validate_lin_checksum,
    validate_lin_identifier, LdfDatabase, LinChecksum, LinObservation, LinResponseState, LinSignal,
    SourceSpan, ValueTable,
};

const LDF: &str = r#"
LIN_description_file;
LIN_protocol_version = "2.2";
LIN_language_version = "2.2";
LIN_speed = 19.2 kbps;
Channel_name = "TEST_LIN";

Nodes {
  Master: MasterNode, 5 ms, 0.1 ms ;
  Slaves: SlaveA, SlaveB ;
}

Signals {
  Mode: 2, 0, MasterNode, SlaveA ;
  Speed: 8, 0, SlaveA, MasterNode ;
}

Frames {
  Status: 1, SlaveA, 2 {
    Mode, 0 ;
    Speed, 8 ;
  }
}

Schedule_tables {
  Normal {
    Status delay 10 ms ;
  }
}

Signal_encoding_types {
  ModeEncoding {
    logical_value, 0, "Off" ;
    logical_value, 1, "On" ;
  }
  SpeedEncoding {
    physical_value, 0, 255, 0.5, 10 ;
  }
}

Signal_representation {
  ModeEncoding: Mode ;
  SpeedEncoding: Speed ;
}
"#;

#[test]
fn parses_ldf_sections_and_applies_signal_representation() {
    let database = LdfDatabase::from_str(LDF).unwrap();
    assert_eq!(database.protocol_version(), Some("2.2"));
    assert_eq!(database.language_version(), Some("2.2"));
    assert_eq!(database.speed_kbps(), Some(19.2));
    assert_eq!(database.channel_name(), Some("TEST_LIN"));
    assert_eq!(database.nodes().count(), 3);
    assert_eq!(
        database
            .schedules()
            .find(|(name, _)| *name == "Normal")
            .unwrap()
            .1[0]
            .delay_ms,
        10
    );
    assert_eq!(database.encodings().count(), 2);

    let frame = database.frame_by_name("Status").unwrap();
    assert_eq!(frame.id, 1);
    assert_eq!(frame.dlc, 2);
    assert_eq!(frame.signals[0].start_bit, 0);
    assert_eq!(frame.signals[0].bit_length, 2);
    assert_eq!(frame.signals[0].choices.get(1), Some("On"));
    assert_eq!(frame.signals[1].factor, 0.5);
    assert_eq!(frame.signals[1].offset, 10.0);
    assert!(database.optional_sections().next().is_none());
}

#[test]
fn exposes_optional_sections_instead_of_silently_dropping_them() {
    let source = r#"
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { State: 1, 0, ECU, ECU ; }
Frames { Status: 1, ECU, 1 { State, 0 ; } }
Sporadic_frames { StatusGroup { Status ; } }
Event_triggered_frames { Trigger { Status, Status ; } }
"#;
    let database = LdfDatabase::from_str(source).unwrap();
    let sections: Vec<_> = database
        .optional_sections()
        .map(|section| section.name.as_str())
        .collect();
    assert_eq!(sections, ["Sporadic_frames", "Event_triggered_frames"]);
}

#[test]
fn preserves_unknown_block_sections_as_opaque_optional_sections() {
    let source = r#"
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { State: 1, 0, ECU, ECU ; }
Frames { Status: 1, ECU, 1 { State, 0 ; } }
Vendor_future_section { Nested { value ; } }
"#;
    let database = LdfDatabase::from_str(source).unwrap();
    assert_eq!(
        database
            .optional_sections()
            .last()
            .map(|section| section.name.as_str()),
        Some("Vendor_future_section")
    );
}

#[test]
fn rejects_unrecognized_opaque_block_sections() {
    let source = r#"
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { State: 1, 0, ECU, ECU ; }
Frames { Status: 1, ECU, 1 { State, 0 ; } }
UnknownBlock { arbitrary content ; }
"#;
    let error = LdfDatabase::from_str(source).unwrap_err();
    assert_eq!(error.field, "UnknownBlock");
    assert!(error.reason.contains("unsupported"));
}

#[test]
fn decodes_little_endian_lin_signals_and_rejects_bad_payload_state() {
    let mut signal = LinSignal {
        name: "Value".to_string(),
        start_bit: 3,
        bit_length: 5,
        initial_value: Some(0),
        factor: 2.0,
        offset: -1.0,
        minimum: Some(-1.0),
        maximum: Some(61.0),
        encoding: None,
        choices: ValueTable::new([(3, "Three".to_string())]),
        source: Some(SourceSpan::new(1, 1)),
    };
    let decoded = decode_lin_signal(&signal, &[0x18]).unwrap();
    assert_eq!(decoded.raw_value, 3);
    assert_eq!(decoded.physical_value, 5.0);
    assert_eq!(decoded.value_text.as_deref(), Some("Three"));

    signal.maximum = Some(2.0);
    assert!(decode_lin_signal(&signal, &[0x18]).is_err());

    let observation = LinObservation {
        state: LinResponseState::HeaderOnly,
        frame_id: Some(1),
        header_dlc: Some(1),
        payload: None,
        direction: None,
        checksum: None,
        checksum_model: None,
        error: Some("header only".to_string()),
    };
    let database = LdfDatabase::from_str(LDF).unwrap();
    assert!(decode_lin_frame(&database, 1, &observation).is_err());
}

#[test]
fn validates_protected_identifier_and_classic_or_enhanced_checksum() {
    assert_eq!(protected_identifier(0x01), 0xC1);
    assert!(validate_lin_identifier(0xC1).is_ok());
    assert!(validate_lin_identifier(0x41).is_err());

    let payload = [0x01_u8, 0x02];
    let classic_checksum = !3_u8;
    assert!(validate_lin_checksum(0x01, &payload, classic_checksum, LinChecksum::Classic).is_ok());

    let enhanced_sum = u16::from(protected_identifier(0x01)) + 1 + 2;
    let enhanced_checksum = !(enhanced_sum as u8);
    assert!(
        validate_lin_checksum(0x01, &payload, enhanced_checksum, LinChecksum::Enhanced).is_ok()
    );
}

#[test]
fn parses_real_workspace_ldf_files_when_available() {
    let paths = [
        "/home/nio/workspace/switch_mirror/Input/medium/ldf/roof_lin.ldf",
        "/home/nio/workspace/switch_mirror/Input/medium/ldf/GWILM1_LIN_lin.ldf",
    ];
    for path in paths {
        if !std::path::Path::new(path).exists() {
            continue;
        }
        let database = LdfDatabase::from_file(path).unwrap();
        assert!(!database.is_empty());
        assert!(database.encodings().next().is_some());
        assert!(database.schedules().next().is_some());
    }
}

#[test]
fn rejects_signal_offsets_that_do_not_fit_the_model() {
    let source = r#"
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { State: 1, 0, ECU, ECU ; }
Frames { Status: 1, ECU, 1 { State, 65536 ; } }
"#;
    let error = LdfDatabase::from_str(source).unwrap_err();
    assert_eq!(error.field, "offset");
}

#[test]
fn rejects_frame_references_to_undefined_signals() {
    let source = r#"
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { Declared: 1, 0, ECU, ECU ; }
Frames { Status: 1, ECU, 1 { Missing, 0 ; } }
"#;
    let error = LdfDatabase::from_source("undefined.ldf", source).unwrap_err();
    assert_eq!(error.field, "signal");
    assert_eq!(error.line, 5);
    assert!(error.reason.contains("Missing"));
    assert!(error.reason.contains("Status"));
}

#[test]
fn rejects_empty_frame_signal_declarations_without_panicking() {
    let source = r#"
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { State: 1, 0, ECU, ECU ; }
Frames { Status: 1, ECU, 1 { , ; } }
"#;
    let error = LdfDatabase::from_str(source).unwrap_err();
    assert_eq!(error.field, "frame_signal");
}

#[test]
fn rejects_empty_encoding_clauses_without_panicking() {
    let source = r#"
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { State: 1, 0, ECU, ECU ; }
Signal_encoding_types { StateEncoding { , ; } }
"#;
    let error = LdfDatabase::from_str(source).unwrap_err();
    assert_eq!(error.field, "encoding");
}

#[test]
fn rejects_incomplete_and_unknown_encoding_clauses() {
    for clause in [
        "logical_value, 1 ;",
        "physical_value, 0, 255, 1 ;",
        "unknown_value, 0, \"Unknown\" ;",
    ] {
        let source = format!(
            "LIN_description_file;\nNodes {{ Master: ECU, 5 ms, 0.1 ms ; }}\nSignal_encoding_types {{ StateEncoding {{ {clause} }} }}\n"
        );
        assert!(
            LdfDatabase::from_str(&source).is_err(),
            "clause should be rejected: {clause}"
        );
    }
}

#[test]
fn rejects_malformed_metadata_and_schedule_entries() {
    for source in [
        "LIN_description_file;\nLIN_speed 19.2 kbps;\n",
        "LIN_description_file;\nLIN_speed = 19.2 mhz;\n",
        "LIN_description_file;\nSchedule_tables { Normal { Status 10 ms ; } }\n",
    ] {
        assert!(
            LdfDatabase::from_str(source).is_err(),
            "malformed LDF should be rejected: {source}"
        );
    }
}

#[test]
fn rejects_duplicate_and_malformed_signal_definitions() {
    let duplicate = r#"
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals {
  State: 1, 0, ECU, ECU ;
  State: 2, 0, ECU, ECU ;
}
"#;
    let error = LdfDatabase::from_str(duplicate).unwrap_err();
    assert_eq!(error.field, "Signals");
    assert!(error.reason.contains("duplicate"));

    let missing_subscriber = "LIN_description_file;\nSignals { State: 1, 0, ECU ; }\n";
    let error = LdfDatabase::from_str(missing_subscriber).unwrap_err();
    assert_eq!(error.field, "subscriber");

    let zero_length = "LIN_description_file;\nSignals { State: 0, 0, ECU, ECU ; }\n";
    let error = LdfDatabase::from_str(zero_length).unwrap_err();
    assert_eq!(error.field, "bit_length");
}

#[test]
fn checksum_validation_falls_back_to_the_ldf_frame_model() {
    let source = r#"
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { State: 1, 0, ECU, ECU ; }
Frames { Diagnostic: 60, ECU, 1 { State, 0 ; } }
"#;
    let database = LdfDatabase::from_str(source).unwrap();
    let observation = LinObservation {
        state: LinResponseState::Response,
        frame_id: Some(0x3C),
        header_dlc: Some(1),
        payload: Some(vec![1]),
        direction: None,
        checksum: Some(!1),
        checksum_model: None,
        error: None,
    };
    assert!(decode_lin_frame(&database, 0x3C, &observation).is_ok());
}

#[test]
fn diagnostic_frame_ids_use_classic_checksum() {
    let source = r#"
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { State: 1, 0, ECU, ECU ; }
Frames { Diagnostic: 60, ECU, 1 { State, 0 ; } }
"#;
    let database = LdfDatabase::from_str(source).unwrap();
    assert_eq!(
        database.frame_by_id(0x3C).unwrap().checksum,
        LinChecksum::Classic
    );
}

#[test]
fn rejects_frame_identifiers_outside_the_lin_six_bit_range() {
    let source = r#"
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { State: 1, 0, ECU, ECU ; }
Frames { Invalid: 64, ECU, 1 { State, 0 ; } }
"#;
    let error = LdfDatabase::from_str(source).unwrap_err();
    assert_eq!(error.field, "frame_id");
    assert!(error.reason.contains("six-bit"));
}

#[test]
fn rejects_frame_dlc_above_eight_bytes() {
    let source = r#"
LIN_description_file;
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { State: 1, 0, ECU, ECU ; }
Frames { Invalid: 1, ECU, 9 { State, 0 ; } }
"#;
    let error = LdfDatabase::from_str(source).unwrap_err();
    assert_eq!(error.field, "dlc");
    assert!(error.reason.contains("maximum of 8"));
}
