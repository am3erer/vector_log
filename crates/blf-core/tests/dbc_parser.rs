use blf_core::{ByteOrder, CanMessageId, DbcDatabase, MultiplexRole, SignalType};

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

    assert_eq!(database.signal("VehicleStatus.Mux").unwrap().1.unit, None);
    let nodes: Vec<_> = database.nodes().collect();
    assert_eq!(
        nodes
            .iter()
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>(),
        ["Vector__XXX", "BCM",]
    );
    assert!(nodes.iter().all(|node| node.source.is_some()));
}

#[test]
fn treats_zero_zero_signal_limits_as_unspecified() {
    let source = r#"
BO_ 256 Status: 1 ECU
 SG_ Value : 0|8@1+ (1,0) [0|0] "" ECU
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    let (_, signal) = database.signal("Status.Value").unwrap();
    assert_eq!(signal.minimum, None);
    assert_eq!(signal.maximum, None);
}

#[test]
fn parses_signal_metadata_transmitters_and_signal_groups() {
    let source = r#"
BO_ 256 Status: 8 ECU_A
 SG_ Speed : 0|8@1+ (1,0) [0|255] "km/h" ECU_B,ECU_C
 SG_ State : 8|2@1+ (1,0) [0|3] "" ECU_B
BO_TX_BU_ 256 : ECU_A,ECU_GATEWAY;
SIG_GROUP_ 256 StatusGroup 10 : Speed State;
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    let message = database.message_by_id(256, false).unwrap();
    assert_eq!(message.transmitter, "ECU_A,ECU_GATEWAY");

    let speed = message.signal("Speed").unwrap();
    assert_eq!(speed.unit.as_deref(), Some("km/h"));
    assert_eq!(speed.receivers, ["ECU_B", "ECU_C"]);

    let groups: Vec<_> = database.signal_groups().collect();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].message_id.id, 256);
    assert_eq!(groups[0].name, "StatusGroup");
    assert_eq!(groups[0].repetition, 10);
    assert_eq!(groups[0].signal_names, ["Speed", "State"]);
}

#[test]
fn preserves_structured_dbc_attribute_assignments() {
    let source = r#"
BA_DEF_ "DatabaseName" STRING;
BA_DEF_ BO_ "CycleTime" INT 0 10000;
BA_DEF_ SG_ "SignalType" STRING;
BA_DEF_ BU_ "NodeRole" STRING;
BA_DEF_ EV_ "Environment" STRING;
BA_ "DatabaseName" "Vehicle";
BO_ 256 Status: 1 ECU
 SG_ Value : 0|8@1+ (1,0) [0|255] "" ECU
BA_ "CycleTime" BO_ 256 10;
BA_ "SignalType" SG_ 256 Value "status";
BA_ "NodeRole" BU_ ECU "controller";
BA_ "Environment" EV_ Ambient "outside";
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    let assignments: Vec<_> = database.attribute_assignments().collect();
    assert_eq!(assignments.len(), 5);

    assert_eq!(assignments[0].name, "DatabaseName");
    assert_eq!(assignments[0].scope, "global");
    assert_eq!(assignments[0].message_id, None);
    assert_eq!(assignments[0].object_name, None);
    assert_eq!(assignments[0].value, "Vehicle");

    assert_eq!(assignments[1].scope, "message");
    assert_eq!(
        assignments[1].message_id,
        Some(CanMessageId::new(256, false))
    );
    assert_eq!(assignments[1].value, "10");

    assert_eq!(assignments[2].scope, "signal");
    assert_eq!(
        assignments[2].message_id,
        Some(CanMessageId::new(256, false))
    );
    assert_eq!(assignments[2].signal_name.as_deref(), Some("Value"));
    assert_eq!(assignments[2].value, "status");

    assert_eq!(assignments[3].scope, "node");
    assert_eq!(assignments[3].object_name.as_deref(), Some("ECU"));
    assert_eq!(assignments[3].value, "controller");

    assert_eq!(assignments[4].scope, "environment");
    assert_eq!(assignments[4].object_name.as_deref(), Some("Ambient"));
    assert_eq!(assignments[4].value, "outside");
    assert!(assignments
        .iter()
        .all(|assignment| assignment.source.is_some()));
}

#[test]
fn preserves_relationship_attributes_separately_from_signal_attributes() {
    let source = r#"
BA_DEF_REL_ BU_SG_REL_ "GenSigTimeoutTime" INT 0 65535;
BA_DEF_DEF_REL_ "GenSigTimeoutTime" 0;
BO_ 256 Status: 1 ECU
 SG_ Value : 0|8@1+ (1,0) [0|255] "" ECU
BA_REL_ "GenSigTimeoutTime" BU_SG_REL_ ECU SG_ 256 Value 75;
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    let relationships: Vec<_> = database.relationship_attributes().collect();
    assert_eq!(relationships.len(), 1);
    assert_eq!(relationships[0].name, "GenSigTimeoutTime");
    assert_eq!(relationships[0].relationship, "BU_SG_REL_");
    assert_eq!(relationships[0].source_node, "ECU");
    assert_eq!(relationships[0].message_id.id, 256);
    assert!(!relationships[0].message_id.extended);
    assert_eq!(relationships[0].signal_name, "Value");
    assert_eq!(relationships[0].value, "75");
    assert_eq!(
        database
            .relationship_attributes_for("GenSigTimeoutTime")
            .count(),
        1
    );
    assert_eq!(
        database
            .relationship_attribute_defaults()
            .collect::<Vec<_>>(),
        vec![("GenSigTimeoutTime", "0")]
    );
    assert!(database.attribute_defaults().next().is_none());
    let definitions: Vec<_> = database.attribute_definitions().collect();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "GenSigTimeoutTime");
    assert_eq!(definitions[0].scope, "relationship");
    assert_eq!(definitions[0].relationship.as_deref(), Some("BU_SG_REL_"));
    assert_eq!(definitions[0].value_type, "INT");
    assert_eq!(definitions[0].minimum.as_deref(), Some("0"));
    assert_eq!(definitions[0].maximum.as_deref(), Some("65535"));
    assert_eq!(definitions[0].raw_arguments, ["0", "65535"]);
    assert!(database
        .signal("Status.Value")
        .unwrap()
        .1
        .attributes
        .is_empty());
}

#[test]
fn preserves_dbc_attribute_definition_types_and_arguments() {
    let source = r#"
BA_DEF_ "GlobalString" STRING;
BA_DEF_ BO_ "CycleTime" INT 0 10000;
BA_DEF_ SG_ "SignalScale" FLOAT -10.5 10.5;
BA_DEF_ BU_ "NodeAddress" HEX 0 0x7F;
BA_DEF_ EV_ "EnvironmentKind" ENUM "Unknown","Cabin","Outside";
BA_DEF_SGTYPE_ "SignalTypeKind" STRING;
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    let definitions: Vec<_> = database.attribute_definitions().collect();
    assert_eq!(definitions.len(), 6);

    let by_name = |name: &str| {
        definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap()
    };
    assert_eq!(by_name("GlobalString").scope, "global");
    assert_eq!(by_name("GlobalString").value_type, "STRING");
    assert_eq!(by_name("CycleTime").scope, "message");
    assert_eq!(by_name("CycleTime").minimum.as_deref(), Some("0"));
    assert_eq!(by_name("CycleTime").maximum.as_deref(), Some("10000"));
    assert_eq!(by_name("SignalScale").minimum.as_deref(), Some("-10.5"));
    assert_eq!(by_name("SignalScale").maximum.as_deref(), Some("10.5"));
    assert_eq!(by_name("NodeAddress").raw_arguments, ["0", "0x7F"]);
    assert_eq!(
        by_name("EnvironmentKind").enum_values,
        ["Unknown", "Cabin", "Outside"]
    );
    assert_eq!(by_name("SignalTypeKind").scope, "signal_type");
}

#[test]
fn preserves_dbc_value_tables_types_and_attribute_defaults() {
    let source = r#"
BA_DEF_ BO_ "CycleTime" INT 0 10000;
BA_DEF_DEF_ "CycleTime" 100;
VAL_TABLE_ IgnitionState 0 "Off" 1 "On";
SIGTYPE_VALTYPE_ 7 1;
BO_ 256 Status: 1 ECU
 SG_ State : 0|8@1+ (1,0) [0|255] "" ECU
SIG_VALTYPE_ 256 State : 1;
"#;
    let database = DbcDatabase::from_str(source).unwrap();

    assert_eq!(
        database.attribute_defaults().collect::<Vec<_>>(),
        vec![("CycleTime", "100")]
    );
    let tables: Vec<_> = database.value_tables().collect();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].0, "IgnitionState");
    assert_eq!(tables[0].1.get(0), Some("Off"));
    assert_eq!(tables[0].1.get(1), Some("On"));
    assert_eq!(
        database.signal_type_value_types().collect::<Vec<_>>(),
        vec![(&7, &1)]
    );
    assert_eq!(
        database.signal("Status.State").unwrap().1.value_type,
        Some(1)
    );
}

#[test]
fn rejects_invalid_dbc_signal_value_type_references() {
    let source = "SIG_VALTYPE_ 256 Missing : 1;\n";
    let error = DbcDatabase::from_str(source).unwrap_err();
    assert_eq!(error.field, "SIG_VALTYPE_");
    assert!(error.reason.contains("unknown CAN message"));

    let source = r#"
BO_ 256 Status: 1 ECU
 SG_ State : 0|8@1+ (1,0) [0|255] "" ECU
SIG_VALTYPE_ 256 Missing : 1;
"#;
    let error = DbcDatabase::from_str(source).unwrap_err();
    assert_eq!(error.field, "SIG_VALTYPE_");
    assert!(error.reason.contains("unknown CAN signal"));
}

#[test]
fn rejects_signal_group_references_to_unknown_objects() {
    let unknown_message = "SIG_GROUP_ 256 Group 1 : Speed;\n";
    let error = DbcDatabase::from_str(unknown_message).unwrap_err();
    assert_eq!(error.field, "SIG_GROUP_");
    assert!(error.reason.contains("unknown CAN message"));

    let unknown_signal = r#"
BO_ 256 Status: 8 ECU
 SG_ Speed : 0|8@1+ (1,0) [0|255] "" ECU
SIG_GROUP_ 256 Group 1 : Missing;
"#;
    let error = DbcDatabase::from_str(unknown_signal).unwrap_err();
    assert_eq!(error.field, "SIG_GROUP_");
    assert!(error.reason.contains("unknown CAN signal"));
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
BA_ "VFrameFormat" BO_ 2553607254 3;
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    let message = database.message_by_name("CLMIF").unwrap();
    assert_eq!(message.dlc, 196);
    assert_eq!(
        message.payload_kind,
        blf_core::CanPayloadKind::LogicalContainer
    );

    let physical = DbcDatabase::from_str(
        "BO_ 256 MalformedPhysical: 196 ECU\n SG_ Value : 0|8@1+ (1,0) [0|255] \"\" ECU\n",
    )
    .unwrap();
    assert_eq!(
        physical
            .message_by_name("MalformedPhysical")
            .unwrap()
            .payload_kind,
        blf_core::CanPayloadKind::Physical
    );
}

#[test]
fn extended_oversized_message_without_container_marker_stays_physical() {
    let source = r#"
BO_ 2553607254 UnmarkedExtended: 196 ECU
 SG_ Value : 0|8@1+ (1,0) [0|255] "" ECU
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    let message = database.message_by_name("UnmarkedExtended").unwrap();
    assert!(message.id.extended);
    assert_eq!(message.payload_kind, blf_core::CanPayloadKind::Physical);
}

#[test]
fn rejects_multiplex_ranges_before_unbounded_expansion() {
    let source = r#"
BO_ 256 Muxed: 8 ECU
 SG_ Mux M : 0|32@1+ (1,0) [0|4294967295] "" ECU
 SG_ Value m1 : 32|8@1+ (1,0) [0|255] "" ECU
SG_MUL_VAL_ 256 Value Mux 0-4294967295;
"#;
    let error = DbcDatabase::from_str(source).unwrap_err();
    assert_eq!(error.field, "SG_MUL_VAL_");
    assert!(error.reason.contains("value limit"));
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
fn enforces_attribute_definition_scope() {
    let wrong_scope = r#"
BA_DEF_ SG_ "Custom" INT 0 1;
BO_ 256 Status: 1 ECU
 SG_ Value : 0|8@1+ (1,0) [0|255] "" ECU
BA_ "Custom" BO_ 256 1;
"#;
    let error = DbcDatabase::from_str(wrong_scope).unwrap_err();
    assert!(error.reason.contains("unsupported attribute scope"));

    let wrong_vframe_scope = r#"
BA_DEF_ SG_ "VFrameFormat" ENUM "StandardCAN","StandardCAN_FD";
BO_ 256 Status: 1 ECU
 SG_ Value : 0|8@1+ (1,0) [0|255] "" ECU
BA_ "VFrameFormat" BO_ 256 14;
"#;
    let error = DbcDatabase::from_str(wrong_vframe_scope).unwrap_err();
    assert!(error.reason.contains("unsupported attribute scope"));

    let wrong_relationship = r#"
BA_DEF_ "Custom" INT 0 1;
BA_REL_ "Custom" BU_SG_REL_ ECU SG_ 256 Value 1;
"#;
    let error = DbcDatabase::from_str(wrong_relationship).unwrap_err();
    assert!(error.reason.contains("relationship attribute"));

    let retained = r#"
BA_DEF_ SG_ "Custom" INT 0 1;
BO_ 256 Status: 1 ECU
 SG_ Value : 0|8@1+ (1,0) [0|255] "" ECU
BA_ "Custom" SG_ 256 Value 1;
"#;
    let database = DbcDatabase::from_str(retained).unwrap();
    let (_, signal) = database.signal("Status.Value").unwrap();
    assert_eq!(
        signal.attributes.get("Custom").map(String::as_str),
        Some("1")
    );
}

#[test]
fn rejects_scoped_attribute_references_to_unknown_signals() {
    let source = r#"
BA_DEF_ SG_ "Custom" INT 0 1;
BO_ 256 Status: 1 ECU
 SG_ Value : 0|8@1+ (1,0) [0|255] "" ECU
BA_ "Custom" SG_ 256 Missing 1;
"#;
    let error = DbcDatabase::from_str(source).unwrap_err();
    assert!(error.reason.contains("unknown CAN signal"));
}

#[test]
fn rejects_unimplemented_metadata_outside_namespace() {
    for keyword in ["CAT_", "FILTER_", "SGTYPE_"] {
        let error = DbcDatabase::from_source(
            "metadata.dbc",
            &format!("VERSION \"1.0\"\n{keyword} malformed;\n"),
        )
        .unwrap_err();
        assert_eq!(error.field, keyword);
        assert!(error.reason.contains("unsupported"));
    }
}

#[test]
fn parses_multi_value_multiplex_ranges() {
    let source = r#"
BO_ 256 Muxed: 2 ECU
 SG_ Mux M : 0|8@1+ (1,0) [0|255] "" ECU
 SG_ Value : 8|8@1+ (1,0) [0|255] "" ECU
SG_MUL_VAL_ 256 Value Mux 1-3,5,7-8;
"#;
    let database = DbcDatabase::from_str(source).unwrap();
    let (_, signal) = database.signal("Muxed.Value").unwrap();
    assert_eq!(
        signal.multiplex,
        MultiplexRole::Branch([1, 2, 3, 5, 7, 8].into_iter().collect())
    );
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
fn rejects_non_finite_and_reversed_signal_limits() {
    for (limits, field) in [
        ("[NaN|1]", "minimum"),
        ("[0|inf]", "maximum"),
        ("[2|1]", "limits"),
    ] {
        let source =
            format!("BO_ 256 Status: 1 ECU\n SG_ Value : 0|8@1+ (1,0) {limits} \"\" ECU\n");
        let error = DbcDatabase::from_str(&source).unwrap_err();
        assert_eq!(error.field, field);
    }
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
fn rejects_unknown_vframe_format_values() {
    let source = "BO_ 256 Status: 8 ECU\nBA_ \"VFrameFormat\" BO_ 256 99;\n";
    let error = DbcDatabase::from_str(source).unwrap_err();
    assert_eq!(error.field, "VFrameFormat");
    assert!(error.reason.contains("unsupported"));
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
