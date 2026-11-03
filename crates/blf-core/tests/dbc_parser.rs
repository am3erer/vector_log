use blf_core::{ByteOrder, DbcDatabase, MultiplexRole, SignalType};

const DBC: &str = r#"
VERSION "1.0"
NS_ :
    NS_DESC_
BS_:
BU_: Vector__XXX BCM

BO_ 256 VehicleStatus: 8 BCM
 SG_ Mux M : 0|4@1+ (1,0) [0|15] "" BCM
 SG_ VehicleSpeed m1 : 8|16@1+ (0.01,0) [0|250] "km/h" BCM
 SG_ SignedTemperature : 24|8@0- (1,-40) [-40|215] "degC" BCM

BO_ 2147483651 ExtendedStatus: 8 BCM
 SG_ State : 0|2@1+ (1,0) [0|3] "" BCM

BO_ 512 FdStatus: 64 BCM
 SG_ Counter : 0|8@1+ (1,0) [0|255] "" BCM

VAL_ 256 Mux 1 "Active" 2 "Standby";
VAL_ 512 Counter 0 "Zero" 1 "One";
CM_ BO_ 256 "Vehicle status message";
CM_ SG_ 256 VehicleSpeed "Vehicle speed in km/h";
BA_ "VFrameFormat" BO_ 512 2;
"#;

#[test]
fn parses_messages_signals_choices_comments_and_attributes() {
    let database = DbcDatabase::from_str(DBC).unwrap();
    assert_eq!(database.version(), Some("1.0"));

    let message = database.message_by_id(256, false).unwrap();
    assert_eq!(message.name, "VehicleStatus");
    assert_eq!(message.comment.as_deref(), Some("Vehicle status message"));
    assert_eq!(message.signals.len(), 3);

    let (_, mux) = database.signal("VehicleStatus.Mux").unwrap();
    assert_eq!(mux.multiplex, MultiplexRole::Selector);
    assert_eq!(mux.choices.get(1), Some("Active"));

    let (_, speed) = database.signal("VehicleStatus.VehicleSpeed").unwrap();
    assert_eq!(speed.byte_order, ByteOrder::LittleEndian);
    assert_eq!(speed.signal_type, SignalType::Unsigned);
    assert_eq!(speed.factor, 0.01);
    assert_eq!(speed.minimum, Some(0.0));
    assert_eq!(speed.maximum, Some(250.0));
    assert_eq!(speed.comment.as_deref(), Some("Vehicle speed in km/h"));
    assert_eq!(
        speed.multiplex,
        MultiplexRole::Branch([1].into_iter().collect())
    );

    let (_, temperature) = database.signal("VehicleStatus.SignedTemperature").unwrap();
    assert_eq!(temperature.byte_order, ByteOrder::BigEndian);
    assert_eq!(temperature.signal_type, SignalType::Signed);
    assert_eq!(temperature.offset, -40.0);
}

#[test]
fn consumes_multiline_namespace_declaration_before_real_statements() {
    let source = r#"
VERSION "1.0"
NS_ :
    NS_DESC_
    CM_
    BA_DEF_
    BA_
    VAL_
BS_:
BU_: ECU
BO_ 256 Status: 1 ECU
 SG_ Mode : 0|8@1+ (1,0) [0|255] "" ECU
CM_ SG_ 256 Mode "mode after namespace";
VAL_ 256 Mode 1 "Active";
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    let (message, signal) = database.signal("Status.Mode").unwrap();
    assert_eq!(message.comment, None);
    assert_eq!(signal.comment.as_deref(), Some("mode after namespace"));
    assert_eq!(signal.choices.get(1), Some("Active"));
}

#[test]
fn accepts_multiline_comments_and_preserves_following_statements() {
    let source = r#"
VERSION "1.0"
BO_ 256 Status: 1 ECU
 SG_ Mode : 0|8@1+ (1,0) [0|255] "" ECU
CM_ SG_ 256 Mode "first line;
second line";
VAL_ 256 Mode 1 "Active";
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    let (_, signal) = database.signal("Status.Mode").unwrap();
    assert_eq!(signal.comment.as_deref(), Some("first line;\nsecond line"));
    assert_eq!(signal.choices.get(1), Some("Active"));
}

#[test]
fn accepts_global_database_comments() {
    let source = r#"
CM_ "database comment";
BO_ 256 Status: 1 ECU
 SG_ Mode : 0|8@1+ (1,0) [0|255] "" ECU
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    assert_eq!(
        database.attributes().next(),
        Some(("comment", "database comment"))
    );
}

#[test]
fn deduplicates_identical_vector_generated_signals_but_rejects_conflicts() {
    let duplicate = r#"
BO_ 256 Generated: 1 ECU
 SG_ Repeated : 0|8@1+ (1,0) [0|255] "" ECU
 SG_ Repeated : 0|8@1+ (1,0) [0|255] "" ECU
"#;
    let database = DbcDatabase::from_str(duplicate).unwrap();
    assert_eq!(
        database.message_by_name("Generated").unwrap().signals.len(),
        1
    );

    let conflict = duplicate.replacen("0|8@1+", "8|8@1+", 1);
    let error = DbcDatabase::from_str(&conflict).unwrap_err();
    assert_eq!(error.field, "SG_");
    assert!(error.reason.contains("duplicate signal name"));
}

#[test]
fn accepts_logical_vector_message_dlc_above_can_fd_payload_size() {
    let source = r#"
BO_ 2553607254 CLMIF: 196 ECU
 SG_ Selector : 0|8@1+ (1,0) [0|255] "" ECU
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    assert_eq!(database.message_by_name("CLMIF").unwrap().dlc, 196);
}

#[test]
fn parses_extended_id_and_can_fd_attribute() {
    let database = DbcDatabase::from_str(DBC).unwrap();
    let extended = database.message_by_id(3, true).unwrap();
    assert_eq!(extended.name, "ExtendedStatus");
    assert!(!extended.is_fd);

    let fd = database.message_by_id(512, false).unwrap();
    assert_eq!(fd.dlc, 64);
    assert!(fd.is_fd);
    assert_eq!(
        fd.attributes.get("VFrameFormat").map(String::as_str),
        Some("2")
    );
}

#[test]
fn malformed_dbc_reports_path_line_field_and_reason() {
    let error = DbcDatabase::from_source(
        "broken.dbc",
        "VERSION \"1.0\"\nBO_ 256 Broken: 8 Node\n SG_ Bad : 0|8@1+ (1,0) [0|255] \"\n",
    )
    .unwrap_err();
    assert_eq!(error.path, "broken.dbc");
    assert_eq!(error.line, 3);
    assert_eq!(error.field, "string");
    assert!(error.reason.contains("unterminated"));
}

#[test]
fn unknown_statement_is_not_silently_accepted() {
    let error = DbcDatabase::from_source("unknown.dbc", "BOGUS value;\n").unwrap_err();
    assert_eq!(error.path, "unknown.dbc");
    assert_eq!(error.line, 1);
    assert_eq!(error.field, "BOGUS");
    assert!(error.reason.contains("unsupported"));
}

#[test]
fn rejects_malformed_recognized_unsupported_statements() {
    for (statement, field) in [
        ("BA_DEF_;", "BA_DEF_"),
        ("SIG_GROUP_;", "SIG_GROUP_"),
        ("ENVVAR_;", "ENVVAR_"),
    ] {
        let error = DbcDatabase::from_source(
            "unsupported.dbc",
            &format!("VERSION \"1.0\"\n{statement}\n"),
        )
        .unwrap_err();
        assert_eq!(error.path, "unsupported.dbc");
        assert_eq!(error.line, 2);
        assert_eq!(error.field, field);
        assert!(error.reason.contains("unsupported"));
    }
}

#[test]
fn rejects_unsupported_comment_and_attribute_scopes() {
    for source in ["CM_ EV_ 1 \"bad\";\n", "BA_ \"X\" SG_;\n"] {
        let error = DbcDatabase::from_source("scope.dbc", source).unwrap_err();
        assert_eq!(error.path, "scope.dbc");
        assert_eq!(error.line, 1);
        assert!(error.reason.contains("quoted") || error.reason.contains("unsupported"));
    }
}

#[test]
fn rejects_malformed_comment_scope_structure() {
    for source in [
        "CM_ BO_ 256 extra \"bad\";\n",
        "CM_ SG_ 256 Signal extra \"bad\";\n",
    ] {
        let error = DbcDatabase::from_str(source).unwrap_err();
        assert_eq!(error.field, "CM_");
        assert!(error.reason.contains("definition must be"));
    }
}

#[test]
fn parses_negative_choice_values_and_comment_urls_without_truncation() {
    let source = r#"
BO_ 256 Status: 8 ECU
 SG_ SignedSignal : 0|8@1- (1,0) [-128|127] "" ECU
VAL_ 256 SignedSignal -1 "Invalid";
CM_ BO_ 256 "https://example.test/a//b";
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    let message = database.message_by_id(256, false).unwrap();
    assert_eq!(
        message.comment.as_deref(),
        Some("https://example.test/a//b")
    );
    let (_, signal) = database.signal("Status.SignedSignal").unwrap();
    assert_eq!(signal.choices.get(-1), Some("Invalid"));
}

#[test]
fn rejects_signal_bit_ranges_that_do_not_fit_the_model() {
    let too_wide = "BO_ 256 Status: 8 ECU\n SG_ Value : 0|65@1+ (1,0) [0|1] \"\" ECU\n";
    let error = DbcDatabase::from_str(too_wide).unwrap_err();
    assert_eq!(error.field, "bit_length");

    let too_far = "BO_ 256 Status: 8 ECU\n SG_ Value : 65536|1@1+ (1,0) [0|1] \"\" ECU\n";
    let error = DbcDatabase::from_str(too_far).unwrap_err();
    assert_eq!(error.field, "start_bit");
}

#[test]
fn rejects_unpaired_value_table_tokens() {
    let source =
        "BO_ 256 Status: 8 ECU\n SG_ Mode : 0|2@1+ (1,0) [0|3] \"\" ECU\nVAL_ 256 Mode 1 \"On\" 2;\n";
    let error = DbcDatabase::from_str(source).unwrap_err();
    assert_eq!(error.field, "VAL_");
    assert!(error.reason.contains("raw/text pairs"));
}

#[test]
fn accepts_multiple_signal_receivers() {
    let source = "BO_ 256 Status: 8 ECU\n SG_ Value : 0|8@1+ (1,0) [0|255] \"\" ECU_A ECU_B\n";
    let database = DbcDatabase::from_str(source).unwrap();
    assert!(database.signal("Status.Value").is_some());
}

#[test]
fn accepts_comma_separated_signal_receivers() {
    let source =
        "BO_ 256 Status: 8 ECU\n SG_ Value : 0|8@1+ (1,0) [0|255] \"\" ECU_A,ECU_B,ECU_C\n";
    let database = DbcDatabase::from_str(source).unwrap();
    assert!(database.signal("Status.Value").is_some());

    for malformed in [
        "BO_ 256 Status: 8 ECU\n SG_ Value : 0|8@1+ (1,0) [0|255] \"\" ,ECU_B\n",
        "BO_ 256 Status: 8 ECU\n SG_ Value : 0|8@1+ (1,0) [0|255] \"\" ECU_A,\n",
        "BO_ 256 Status: 8 ECU\n SG_ Value : 0|8@1+ (1,0) [0|255] \"\" ECU_A,,ECU_B\n",
    ] {
        assert!(DbcDatabase::from_str(malformed).is_err());
    }
}

#[test]
fn recognizes_standard_can_fd_vframe_format_values() {
    for value in ["14", "15", "StandardCAN_FD", "ExtendedCAN_FD"] {
        let source = format!("BO_ 256 Status: 64 ECU\nBA_ \"VFrameFormat\" BO_ 256 {value};\n");
        assert!(
            DbcDatabase::from_str(&source)
                .unwrap()
                .message_by_id(256, false)
                .unwrap()
                .is_fd
        );
    }
}

#[test]
fn rejects_trailing_structure_tokens_and_invalid_attribute_scopes() {
    let trailing = "BO_ 256 Status: 8 ECU extra\n SG_ Value : 0|8@1+ (1,0) [0|255] \"\" ECU\n";
    let error = DbcDatabase::from_str(trailing).unwrap_err();
    assert_eq!(error.field, "BO_");

    let invalid_scope = "BO_ 256 Status: 8 ECU\nBA_ \"X\" SG_ 256 Value 1;\n";
    let error = DbcDatabase::from_str(invalid_scope).unwrap_err();
    assert_eq!(error.field, "BA_");
    assert!(error.reason.contains("unsupported attribute scope"));
}

#[test]
fn rejects_unterminated_namespace_declarations() {
    let source = "NS_ :\n NS_DESC_\n BO_ 256 Status: 1 ECU\n";
    let error = DbcDatabase::from_source("unterminated.dbc", source).unwrap_err();
    assert_eq!(error.field, "NS_");
    assert!(error.reason.contains("not terminated"));
}
