//! Cross-checks against a real CANoe capture.
//!
//! `blf/Logging2026-11-18_15-46-55.blf` is a medium-bench measurement holding
//! CAN FD, Ethernet frames and port events. The expectations below come from
//! two independent sources:
//!
//! * CANoe's own ASC export of the same measurement
//!   (`tests/data/ethernet_events.asc.txt`),
//! * the reference C++ reader (Technica-Engineering/vector_blf), run over the
//!   same file (`tests/data/frames.ref.txt` and the counts below).
//!
//! The capture itself is not committed, so these tests skip when it is absent.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use blf_core::{FieldValue, File, Object, ObjectType};

/// Object counts the reference reader produces for this capture.
///
/// `EthernetPhyState` is absent there because the reference implementation has
/// no such type; every other count is identical.
const REFERENCE_COUNTS: &[(&str, i64)] = &[
    ("CanDriverStatistic", 5_022),
    ("CanDriverError", 5_040),
    ("AppText", 37),
    ("SystemVariable", 735),
    ("CanFdMessage64", 71_197),
    ("EthernetStatus", 453),
    ("EthernetStatistic", 53_181),
    ("EthernetPhyState", 187),
    ("EthernetFrameEx", 306_744),
    ("RestorePointContainer", 7),
    ("TestStructure", 2),
];

fn capture_path() -> Option<PathBuf> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../blf/Logging2026-11-18_15-46-55.blf");
    path.exists().then_some(path)
}

fn golden(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    fs::read_to_string(path).unwrap()
}

fn field_u64(object: &Object, name: &str) -> Option<u64> {
    let Object::Structured(value) = object else {
        return None;
    };
    value.fields.iter().find_map(|(field, value)| {
        if field != name {
            return None;
        }
        match value {
            FieldValue::U8(value) => Some(u64::from(*value)),
            FieldValue::U16(value) => Some(u64::from(*value)),
            FieldValue::U32(value) => Some(u64::from(*value)),
            FieldValue::U64(value) => Some(*value),
            _ => None,
        }
    })
}

/// Timestamp of an object in nanoseconds, as the reader computes it.
fn object_timestamp(object: &Object) -> u64 {
    let header = object.header_info();
    if header.object_flags & 0x2 != 0 {
        header.timestamp
    } else if header.object_flags & 0x1 != 0 {
        header.timestamp.saturating_mul(10_000)
    } else {
        0
    }
}

#[test]
fn real_capture_object_counts_match_the_reference_reader() {
    let Some(path) = capture_path() else {
        eprintln!("skipping: capture not present");
        return;
    };
    let mut reader = File::open(&path).unwrap();
    let mut counts: BTreeMap<&'static str, i64> = BTreeMap::new();
    while let Some(object) = reader.read().unwrap() {
        *counts.entry(object.object_type().name()).or_default() += 1;
    }
    // Nothing may be dropped: containers are stitched, not truncated.
    assert_eq!(reader.skipped_object_count, 0);

    for (name, expected) in REFERENCE_COUNTS {
        let actual = counts.get(name).copied().unwrap_or(0);
        assert_eq!(actual, *expected, "{name} count");
    }
    assert_eq!(counts.values().sum::<i64>(), 442_605, "total objects");
}

#[test]
fn real_capture_port_events_match_the_asc_export() {
    let Some(path) = capture_path() else {
        eprintln!("skipping: capture not present");
        return;
    };
    let golden = golden("ethernet_events.asc.txt");

    // stat <ts> <ch> <hw> <link> <Mbit/s> <physical> <duplex> <clockMode> <pairs>
    // phy  <ts> <ch> <hw> <state> <event>
    let mut expected_status: Vec<(String, String, String, String, String, String)> = Vec::new();
    let mut expected_phy: Vec<(String, String, String)> = Vec::new();
    for line in golden.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        match f[0] {
            "stat" => expected_status.push((
                f[2].to_string(),
                f[4].to_string(),
                f[5].to_string(),
                f[6].to_string(),
                f[7].to_string(),
                f[9].to_string(),
            )),
            "phy" => expected_phy.push((f[2].to_string(), f[4].to_string(), f[5].to_string())),
            other => panic!("unexpected golden kind {other}"),
        }
    }

    let mut reader = File::open(&path).unwrap();
    let mut status = Vec::new();
    let mut phy = Vec::new();
    while let Some(object) = reader.read().unwrap() {
        match object.object_type() {
            ObjectType::EthernetStatus => {
                let channel = field_u64(&object, "channel").unwrap_or(0).to_string();
                let bitrate = field_u64(&object, "bitrate").unwrap_or(0) / 1000;
                let link = match blf_core::ethernet_link_status_name(
                    field_u64(&object, "linkStatus").unwrap_or(0) as u8,
                ) {
                    "link up" => "Link_up",
                    "link down" => "Link_down",
                    other => other,
                }
                .to_string();
                let physical = physical_name(blf_core::ethernet_phy_type_name(
                    field_u64(&object, "ethernetPhy").unwrap_or(0) as u8,
                ))
                .to_string();
                let duplex = match blf_core::ethernet_duplex_name(
                    field_u64(&object, "duplex").unwrap_or(0) as u8,
                ) {
                    "full duplex" => "Full",
                    "half duplex" => "Half",
                    _ => "-",
                }
                .to_string();
                let pairs = match blf_core::ethernet_pairs_name(
                    field_u64(&object, "pairs").unwrap_or(0) as u8,
                ) {
                    "one pair" => "1-pair",
                    "two pairs" => "2-pair",
                    "four pairs" => "4-pair",
                    _ => "-",
                }
                .to_string();
                status.push((channel, link, bitrate.to_string(), physical, duplex, pairs));
            }
            ObjectType::EthernetPhyState => {
                let channel = field_u64(&object, "channel").unwrap_or(0).to_string();
                let state = phy_state_token(blf_core::ethernet_phy_state_name(
                    field_u64(&object, "phyState").unwrap_or(0) as u8,
                ))
                .to_string();
                let event = phy_event_token(blf_core::ethernet_phy_event_name(
                    field_u64(&object, "phyEvent").unwrap_or(0) as u8,
                ))
                .to_string();
                phy.push((channel, state, event));
            }
            _ => {}
        }
    }

    assert_eq!(status.len(), expected_status.len(), "status event count");
    assert_eq!(phy.len(), expected_phy.len(), "phy event count");

    let mut ours = status;
    let mut theirs = expected_status;
    ours.sort();
    theirs.sort();
    assert_eq!(ours, theirs, "status events");

    let mut ours = phy;
    let mut theirs = expected_phy;
    ours.sort();
    theirs.sort();
    assert_eq!(ours, theirs, "phy events");
}

#[test]
fn real_capture_frames_match_the_reference_reader() {
    let Some(path) = capture_path() else {
        eprintln!("skipping: capture not present");
        return;
    };
    let golden = golden("frames.ref.txt");

    let mut expected_eth: Vec<String> = Vec::new();
    let mut expected_can: Vec<String> = Vec::new();
    for line in golden.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        match f[0] {
            // eth <ts> <ch> <hw> <dir> <len> <first4>
            "eth" => expected_eth.push(format!("{} {} {} {} {}", f[1], f[2], f[3], f[4], f[5])),
            // can <ts> <ch> <id> <dlc> <valid> <data>
            "can" => expected_can.push(format!(
                "{} {} {} {} {} {}",
                f[1],
                f[2],
                f[3],
                f[4],
                f[5],
                f.get(6).copied().unwrap_or("")
            )),
            other => panic!("unexpected golden kind {other}"),
        }
    }

    let mut reader = File::open(&path).unwrap();
    let mut eth: Vec<String> = Vec::new();
    let mut can: Vec<String> = Vec::new();
    while let Some(object) = reader.read().unwrap() {
        match object.object_type() {
            ObjectType::EthernetFrameEx if eth.len() < expected_eth.len() => {
                eth.push(format!(
                    "{} {} {} {} {}",
                    object_timestamp(&object),
                    field_u64(&object, "channel").unwrap_or(0),
                    field_u64(&object, "hardwareChannel").unwrap_or(0),
                    field_u64(&object, "dir").unwrap_or(0),
                    field_u64(&object, "frameLength").unwrap_or(0)
                ));
            }
            ObjectType::CanFdMessage64 if can.len() < expected_can.len() => {
                let Object::CanFdMessage64(frame) = &object else {
                    continue;
                };
                // The reference tool prints the first eight payload bytes.
                let data: String = frame
                    .data
                    .iter()
                    .take(usize::from(frame.valid_data_bytes).min(8))
                    .map(|b| format!("{b:02X}"))
                    .collect();
                can.push(format!(
                    "{} {} {} {} {} {}",
                    object_timestamp(&object),
                    frame.channel,
                    frame.id,
                    frame.dlc,
                    frame.valid_data_bytes,
                    data
                ));
            }
            _ => {}
        }
    }

    assert!(expected_eth.len() >= 20 && expected_can.len() >= 20);
    assert_eq!(eth.len(), expected_eth.len(), "ethernet frames read");
    assert_eq!(eth, expected_eth, "ethernet frame fields");
    assert_eq!(can, expected_can, "can frame fields");
}

/// CANoe names the physical layer the way its trace window does.
fn physical_name(name: &str) -> &'static str {
    match name {
        "100BASE-T1 (OABR)" => "BroadR-Reach",
        "100BASE-TX/1000BASE-T" => "IEEE 802.3",
        _ => "-",
    }
}

fn phy_state_token(name: &str) -> &'static str {
    match name {
        "normal state" => "normal",
        "sleep state" => "sleep",
        "power off state" => "power_off",
        "sleep request" => "sleep_request",
        _ => "invalid",
    }
}

fn phy_event_token(name: &str) -> &'static str {
    match name {
        "wakeup received" => "wakeup_recv",
        "wakeup sent" => "wakeup_sent",
        "sleep received" => "sleep_recv",
        "sleep sent" => "sleep_sent",
        "sleep abort" => "sleep_abort",
        "sleep ack received" => "sleep_ack_recv",
        "power off" => "power_off",
        "power on" => "power_on",
        "activated" => "activated",
        _ => "invalid",
    }
}

#[test]
fn real_capture_driver_errors_and_statistics_decode() {
    let Some(path) = capture_path() else {
        eprintln!("skipping: capture not present");
        return;
    };
    let mut reader = File::open(&path).unwrap();
    let mut driver_errors: BTreeMap<u32, i64> = BTreeMap::new();
    let mut statistics = 0;
    while let Some(object) = reader.read().unwrap() {
        match object.object_type() {
            ObjectType::CanDriverError => {
                let code = field_u64(&object, "errorCode").unwrap_or(0) as u32;
                *driver_errors.entry(code).or_default() += 1;
            }
            ObjectType::CanStatistic => {
                // Bus load is reported in 1/100 percent and must stay plausible.
                let load = field_u64(&object, "busLoad").unwrap_or(0);
                assert!(load <= 10_000, "bus load {load} out of range");
                statistics += 1;
            }
            _ => {}
        }
    }
    // Every driver error in this capture is a chip-state report.
    assert_eq!(driver_errors.values().sum::<i64>(), 5_040);
    for code in driver_errors.keys() {
        assert!(
            matches!(code, 102..=104),
            "unexpected driver error code {code}"
        );
    }
    assert_eq!(statistics, 5_022);
}
