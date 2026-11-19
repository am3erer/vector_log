//! Checks against a BLF written by Vector's own binlog library.
//!
//! `tests/data/official_all_types.blf` was produced with the official
//! `BLWriteObject` API of the binlog library shipped in `blf_dump/`, holding one
//! instance of every CAN, LIN and Ethernet object type the writer accepts, each
//! filled with a distinctive value. The expectations below are exactly the
//! values written into the C structs, so a mismatch means our field order, width
//! or naming drifted from CANoe's own layout.
//!
//! `BL_OBJ_TYPE_ETHERNET_FRAME` (71) is missing: the official writer emits a
//! size that disagrees with the bytes it writes, so that type is only covered by
//! the sample capture in `real_log.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use blf_core::{FieldValue, File, Object};

fn fixture() -> Vec<Object> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/official_all_types.blf");
    let mut reader = File::open(path).unwrap();
    let mut objects = Vec::new();
    while let Some(object) = reader.read().unwrap() {
        objects.push(object);
    }
    assert_eq!(reader.skipped_object_count, 0);
    objects
}

fn fields(object: &Object) -> BTreeMap<String, FieldValue> {
    use FieldValue::{U16 as F16, U32 as F32, U64 as F64_, U8 as F8};
    let mut map = BTreeMap::new();
    let mut put = |name: &str, value: FieldValue| {
        map.insert(name.to_string(), value);
    };
    match object {
        Object::Structured(value) => {
            return value.fields.iter().cloned().collect();
        }
        Object::Lin(value) => {
            return value.fields.iter().cloned().collect();
        }
        // The CAN records are typed structs rather than schema-driven bodies.
        Object::CanMessage(value) => {
            put("channel", F16(value.channel));
            put("flags", F8(value.flags));
            put("dlc", F8(value.dlc));
            put("id", F32(value.id));
        }
        Object::CanMessage2(value) => {
            put("channel", F16(value.channel));
            put("flags", F8(value.flags));
            put("dlc", F8(value.dlc));
            put("id", F32(value.id));
            put("frameLength", F32(value.frame_length));
            put("bitCount", F8(value.bit_count));
        }
        Object::CanFdMessage(value) => {
            put("channel", F16(value.channel));
            put("flags", F8(value.flags));
            put("dlc", F8(value.dlc));
            put("id", F32(value.id));
            put("frameLength", F32(value.frame_length));
            put("validDataBytes", F8(value.valid_data_bytes));
        }
        Object::CanFdMessage64(value) => {
            put("channel", F8(value.channel));
            put("dlc", F8(value.dlc));
            put("validDataBytes", F8(value.valid_data_bytes));
            put("id", F32(value.id));
            put("frameLength", F32(value.frame_length));
            put("flags", F32(value.flags));
            put("btrCfgArb", F32(value.btr_cfg_arb));
            put("btrCfgData", F32(value.btr_cfg_data));
            put("bitCount", F16(value.bit_count));
            put("dir", F8(value.dir));
            put("crc", F32(value.crc));
        }
        Object::CanErrorFrame(value) => {
            put("channel", F16(value.channel));
            put("length", F16(value.length));
        }
        Object::CanErrorFrameExt(value) => {
            put("channel", F16(value.channel));
            put("length", F16(value.length));
            put("flags", F32(value.flags));
            put("ecc", F8(value.ecc));
            put("position", F8(value.position));
            put("dlc", F8(value.dlc));
            put("frameLengthInNs", F32(value.frame_length_in_ns));
            put("id", F32(value.id));
            put("flagsExt", F16(value.flags_ext));
        }
        Object::CanFdErrorFrame64(value) => {
            put("channel", F8(value.channel));
            put("dlc", F8(value.dlc));
            put("validDataBytes", F8(value.valid_data_bytes));
            put("ecc", F8(value.ecc));
            put("flags", F16(value.flags));
            put("errorCodeExt", F16(value.error_code_ext));
            put("extFlags", F16(value.ext_flags));
            put("id", F32(value.id));
            put("frameLength", F32(value.frame_length));
            put("crc", F32(value.crc));
            put("errorPosition", F16(value.error_position));
        }
        _ => return BTreeMap::new(),
    }
    let _ = F64_;
    map
}

fn expect_u64(objects: &[Object], name: &str, expected: &[(&str, u64)]) {
    let object = objects
        .iter()
        .find(|object| object.object_type().name() == name)
        .unwrap_or_else(|| panic!("{name} missing from the fixture"));
    let fields = fields(object);
    for (field, want) in expected {
        let value = fields
            .get(*field)
            .unwrap_or_else(|| panic!("{name}.{field} missing"));
        let got = match value {
            FieldValue::U8(v) => u64::from(*v),
            FieldValue::U16(v) => u64::from(*v),
            FieldValue::U32(v) => u64::from(*v),
            FieldValue::U64(v) => *v,
            FieldValue::I32(v) => *v as u64,
            other => panic!("{name}.{field} has unexpected type {other:?}"),
        };
        assert_eq!(got, *want, "{name}.{field}");
    }
}

#[test]
fn fixture_holds_every_written_type_once() {
    let objects = fixture();
    assert_eq!(objects.len(), 47);
    // Two restore-point containers frame the object list; every other record is
    // a distinct CAN, LIN or Ethernet type.
    let mut seen: BTreeMap<&'static str, i64> = BTreeMap::new();
    for object in &objects {
        *seen.entry(object.object_type().name()).or_default() += 1;
    }
    assert_eq!(seen.remove("RestorePointContainer"), Some(2));
    assert_eq!(seen.len(), 45, "unexpected type count: {seen:?}");
    assert!(seen.values().all(|count| *count == 1), "{seen:?}");
}

#[test]
fn can_objects_keep_their_documented_layout() {
    let objects = fixture();
    expect_u64(
        &objects,
        "CanMessage",
        &[("channel", 1), ("dlc", 3), ("id", 0x123)],
    );
    expect_u64(
        &objects,
        "CanMessage2",
        &[
            ("channel", 2),
            ("dlc", 2),
            ("id", 0x456),
            ("frameLength", 500),
            ("bitCount", 108),
        ],
    );
    expect_u64(
        &objects,
        "CanFdMessage",
        &[
            ("channel", 3),
            ("dlc", 10),
            ("id", 0x789),
            ("validDataBytes", 5),
            ("frameLength", 700),
        ],
    );
    expect_u64(
        &objects,
        "CanFdMessage64",
        &[
            ("channel", 4),
            ("dlc", 12),
            ("validDataBytes", 9),
            ("id", 0x7AB),
            ("frameLength", 900),
            ("btrCfgArb", 0x1111),
            ("btrCfgData", 0x2222),
            ("bitCount", 200),
            ("dir", 1),
            ("crc", 0xCAFE),
        ],
    );
    expect_u64(&objects, "CanErrorFrame", &[("channel", 5), ("length", 12)]);
    expect_u64(
        &objects,
        "CanErrorFrameExt",
        &[
            ("channel", 6),
            ("flags", 5),
            ("ecc", 0xCB),
            ("position", 17),
            ("dlc", 8),
            ("frameLengthInNs", 1234),
            ("id", 0x321),
            ("flagsExt", 1),
        ],
    );
    expect_u64(
        &objects,
        "CanFdErrorFrame64",
        &[
            ("channel", 7),
            ("dlc", 10),
            ("validDataBytes", 4),
            ("ecc", 0x44),
            ("flags", 0x11),
            ("errorCodeExt", 0x22),
            ("extFlags", 0x33),
            ("id", 0x555),
            ("frameLength", 4321),
            ("crc", 0xBEEF),
            ("errorPosition", 9),
        ],
    );
    expect_u64(&objects, "CanOverloadFrame", &[("channel", 8)]);
    expect_u64(
        &objects,
        "CanDriverStatistic",
        &[
            ("channel", 9),
            ("busLoad", 1234),
            ("standardDataFrames", 1200),
            ("extendedDataFrames", 34),
            ("standardRemoteFrames", 5),
            ("extendedRemoteFrames", 6),
            ("errorFrames", 7),
            ("overloadFrames", 8),
        ],
    );
    expect_u64(
        &objects,
        "CanDriverError",
        &[
            ("channel", 10),
            ("txErrors", 3),
            ("rxErrors", 4),
            ("errorCode", 102),
        ],
    );
    expect_u64(
        &objects,
        "CanDriverErrorExt",
        &[
            ("channel", 11),
            ("txErrors", 5),
            ("rxErrors", 6),
            ("errorCode", 101),
            ("flags", 15),
            ("state", 2),
        ],
    );
    expect_u64(
        &objects,
        "CanDriverHwSync",
        &[("channel", 12), ("flags", 5)],
    );
    expect_u64(
        &objects,
        "CanSettingChanged",
        &[("channel", 13), ("changedType", 1)],
    );
}

#[test]
fn lin_objects_keep_their_documented_layout() {
    let objects = fixture();
    expect_u64(
        &objects,
        "LinMessage",
        &[
            ("channel", 1),
            ("id", 0x11),
            ("dlc", 2),
            ("fsmId", 1),
            ("fsmState", 2),
            ("headerTime", 3),
            ("fullTime", 4),
            ("crc", 0x5A),
            ("dir", 0),
        ],
    );
    expect_u64(
        &objects,
        "LinDlcInfo",
        &[("channel", 5), ("id", 0x13), ("dlc", 4)],
    );
    expect_u64(
        &objects,
        "LinReceiveError",
        &[
            ("channel", 6),
            ("id", 0x14),
            ("dlc", 1),
            ("fsmId", 4),
            ("fsmState", 5),
            ("headerTime", 7),
            ("fullTime", 8),
            ("stateReason", 0x21),
            ("offendingByte", 0x99),
            ("shortError", 1),
        ],
    );
    expect_u64(
        &objects,
        "LinSendError",
        &[
            ("channel", 8),
            ("id", 0x15),
            ("dlc", 1),
            ("fsmId", 6),
            ("fsmState", 7),
            ("headerTime", 9),
            ("fullTime", 10),
        ],
    );
    expect_u64(
        &objects,
        "LinSlaveTimeout",
        &[
            ("channel", 10),
            ("slaveId", 0x21),
            ("stateId", 3),
            ("followStateId", 4),
        ],
    );
    expect_u64(
        &objects,
        "LinSchedulerModeChange",
        &[("channel", 11), ("oldMode", 1), ("newMode", 2)],
    );
    expect_u64(
        &objects,
        "LinBaudrateEvent",
        &[("channel", 14), ("baudrate", 19_200)],
    );
    expect_u64(
        &objects,
        "LinSleepModeEvent",
        &[("channel", 15), ("reason", 1), ("flags", 3)],
    );
    expect_u64(
        &objects,
        "LinWakeupEvent",
        &[("channel", 16), ("signal", 0x3C), ("external", 1)],
    );
    expect_u64(
        &objects,
        "LinWakeupEvent2",
        &[
            ("channel", 17),
            ("lengthInfo", 1),
            ("signal", 0x4D),
            ("external", 1),
        ],
    );
    expect_u64(
        &objects,
        "LinChecksumInfo",
        &[("channel", 18), ("id", 0x17), ("checksumModel", 1)],
    );
    expect_u64(&objects, "LinSpikeEvent", &[("channel", 19), ("width", 42)]);
    expect_u64(
        &objects,
        "LinSpikeEvent2",
        &[("channel", 20), ("width", 43), ("internal", 1)],
    );
    expect_u64(
        &objects,
        "LinLongDomSignalEvent",
        &[("channel", 21), ("type", 1)],
    );
    expect_u64(
        &objects,
        "LinLongDomSignalEvent2",
        &[("channel", 22), ("type", 2), ("length", 123_456)],
    );
    expect_u64(
        &objects,
        "LinDisturbanceEvent",
        &[
            ("channel", 23),
            ("id", 0x18),
            ("disturbingFrameId", 0x19),
            ("disturbanceType", 2),
            ("byteIndex", 3),
            ("bitIndex", 4),
            ("bitOffsetInSixteenthBits", 5),
            ("disturbanceLengthInSixteenthBits", 6),
        ],
    );
    expect_u64(
        &objects,
        "LinUnexpectedWakeup",
        &[("channel", 26), ("width", 77), ("signal", 0x5E)],
    );
    // The bus event header of the LIN2 records carries its own channel.
    expect_u64(&objects, "LinMessage2", &[("channel", 2)]);
    expect_u64(&objects, "LinCrcError2", &[("channel", 4)]);
    expect_u64(&objects, "LinReceiveError2", &[("channel", 7)]);
    expect_u64(&objects, "LinSendError2", &[("channel", 9)]);
    expect_u64(&objects, "LinSyncError2", &[("channel", 13)]);
    expect_u64(&objects, "LinShortOrSlowResponse", &[("channel", 24)]);
    expect_u64(&objects, "LinShortOrSlowResponse2", &[("channel", 25)]);

    let statistic = objects
        .iter()
        .find(|object| object.object_type().name() == "LinStatisticEvent")
        .expect("LinStatisticEvent");
    let fields = fields(statistic);
    match fields.get("busLoad") {
        // The bus load is a double in this record.
        Some(FieldValue::F64(value)) => assert!((*value - 12.5).abs() < 1e-9),
        other => panic!("LinStatisticEvent.busLoad is {other:?}"),
    }
}

#[test]
fn ethernet_objects_keep_their_documented_layout() {
    let objects = fixture();
    expect_u64(
        &objects,
        "EthernetFrameEx",
        &[
            ("channel", 2),
            ("hardwareChannel", 7),
            ("dir", 1),
            ("frameLength", 64),
            ("frameChecksum", 0x1234),
            ("frameHandle", 9),
            ("frameDuration", 1500),
        ],
    );
    expect_u64(
        &objects,
        "EthernetRxError",
        &[
            ("channel", 3),
            ("dir", 0),
            ("hardwareChannel", 11),
            ("fcs", 0xDEAD_BEEF),
            ("frameDataLength", 32),
            ("error", 2),
        ],
    );
    expect_u64(
        &objects,
        "EthernetStatistic",
        &[
            ("channel", 4),
            ("rcvOk_HW", 1000),
            ("xmitOk_HW", 900),
            ("rcvError_HW", 2),
            ("xmitError_HW", 3),
            ("rcvBytes_HW", 4000),
            ("xmitBytes_HW", 5000),
            ("rcvNoBuffer_HW", 7),
            ("hardwareChannel", 12),
        ],
    );
    expect_u64(
        &objects,
        "EthernetStatus",
        &[
            ("channel", 5),
            ("flags", 0x1CF),
            ("linkStatus", 2),
            ("ethernetPhy", 2),
            ("duplex", 2),
            ("mdi", 1),
            ("connector", 1),
            ("clockMode", 2),
            ("pairs", 1),
            ("hardwareChannel", 13),
            ("bitrate", 100_000),
            // The field the reference C++ reader does not know yet.
            ("linkUpDuration", 42),
        ],
    );
    expect_u64(
        &objects,
        "EthernetPhyState",
        &[
            ("channel", 6),
            ("flags", 7),
            ("phyState", 2),
            ("phyEvent", 8),
            ("hardwareChannel", 14),
        ],
    );

    // SQI is a signed 16-bit value on the wire.
    let statistic = objects
        .iter()
        .find(|object| object.object_type().name() == "EthernetStatistic")
        .expect("EthernetStatistic");
    match fields(statistic).get("sqi") {
        Some(FieldValue::I16(value)) => assert_eq!(*value, 4),
        other => panic!("EthernetStatistic.sqi is {other:?}"),
    }
}

#[test]
fn fixture_values_match_the_written_payload_bytes() {
    // Spot-check a payload that the writer filled with a counter pattern.
    let objects = fixture();
    let frame = objects
        .iter()
        .find(|object| object.object_type().name() == "EthernetFrameEx")
        .expect("EthernetFrameEx");
    let fields = fields(frame);
    match fields.get("frameData") {
        Some(FieldValue::Bytes(data)) => {
            assert_eq!(data.len(), 64);
            assert!(data.iter().enumerate().all(|(i, b)| *b == i as u8));
        }
        other => panic!("EthernetFrameEx.frameData is {other:?}"),
    }
    assert!(fs::metadata(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/official_all_types.blf")
    )
    .is_ok());
}
