use std::sync::Arc;

use blf_core::{
    BusKind, Database, DatabaseBinding, DatabaseError, DbcDatabase, LdfDatabase, LinChecksum,
};

const DBC: &str = r#"
VERSION "1.0"
BO_ 291 VehicleStatus: 8 ECU
 SG_ VehicleSpeed : 8|16@1+ (0.01,0) [0|250] "km/h" ECU
VAL_ 291 VehicleSpeed 0 "Stopped";
CM_ BO_ 291 "status frame";
"#;

const LDF: &str = r#"
LIN_description_file;
LIN_protocol_version = "2.2";
LIN_language_version = "2.2";
Nodes { Master: ECU, 5 ms, 0.1 ms ; }
Signals { DoorState: 2, 0, ECU, ECU ; }
Frames { BodyStatus: 34, ECU, 4 { DoorState, 0 ; } }
"#;

#[test]
fn dbc_indexes_messages_and_qualified_signals() {
    let database = DbcDatabase::from_source("vehicle.dbc", DBC).unwrap();

    let message = database.message_by_id(0x123, false).unwrap();
    assert_eq!(message.name, "VehicleStatus");
    assert_eq!(
        database.message_by_name("VehicleStatus").unwrap().id.id,
        0x123
    );
    let (message, signal) = database.signal("VehicleStatus.VehicleSpeed").unwrap();
    assert_eq!(message.name, "VehicleStatus");
    assert_eq!(signal.factor, 0.01);
    assert_eq!(signal.choices.get(0), Some("Stopped"));
    assert_eq!(database.len(), 1);
}

#[test]
fn duplicate_definitions_have_location_rich_errors() {
    let error = DbcDatabase::from_source(
        "bad.dbc",
        &format!("{}\nBO_ 291 VehicleStatus: 8 ECU\n", DBC),
    )
    .unwrap_err();
    assert_eq!(error.path, "bad.dbc");
    assert_eq!(error.line, 8);
    assert_eq!(error.field, "BO_");
    assert!(error.reason.contains("duplicate"));
    assert_eq!(
        error.to_string(),
        "bad.dbc:8:BO_: duplicate CAN message id 0x123"
    );
}

#[test]
fn lin_model_keeps_frame_and_schedule_semantics() {
    let database = LdfDatabase::from_str(LDF).unwrap();
    let (frame, signal) = database.signal("BodyStatus.DoorState").unwrap();
    assert_eq!(frame.checksum, LinChecksum::Enhanced);
    assert_eq!(signal.bit_length, 2);
    assert!(database.schedules().next().is_none());
}

#[test]
fn bindings_reject_cross_protocol_database_use_and_can_share_handles() {
    let can = Arc::new(DbcDatabase::from_str(DBC).unwrap());
    let database = Database::Can(can.clone());
    let binding = DatabaseBinding::new(BusKind::Can, 1, database).unwrap();
    assert_eq!(binding.channel(), 1);
    assert!(Arc::ptr_eq(
        match binding.database() {
            Database::Can(ref value) => value,
            Database::Lin(_) => panic!("wrong database kind"),
        },
        &can
    ));

    let error = DatabaseBinding::new(BusKind::Lin, 1, Database::Can(can)).unwrap_err();
    assert_eq!(error.path, "<binding>");
    assert_eq!(error.field, "bus");
    assert!(error.reason.contains("cannot be bound"));
}

#[test]
fn database_error_is_structured_and_cloneable() {
    let error = DatabaseError::new("file.ldf", 7, "Nodes", "missing closing brace");
    let cloned = error.clone();
    assert_eq!(cloned.path, "file.ldf");
    assert_eq!(cloned.line, 7);
    assert_eq!(cloned.field, "Nodes");
}
