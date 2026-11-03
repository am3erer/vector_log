use std::fs;
use std::path::Path;
use std::sync::Arc;

use blf_core::{
    protected_identifier, BusKind, Database, DatabaseBinding, DbcDatabase, DecodeStatus, File,
    LdfDatabase,
};

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn file_statistics(uncompressed_size: u64, object_count: u32) -> Vec<u8> {
    let mut output = Vec::new();
    push_u32(&mut output, 0x4747_4F4C);
    push_u32(&mut output, 144);
    push_u32(&mut output, 4_080_200);
    output.extend_from_slice(&[1, 1, 1, 0]);
    push_u64(&mut output, 0);
    push_u64(&mut output, uncompressed_size);
    push_u32(&mut output, object_count);
    push_u32(&mut output, 1);
    for value in [2026u16, 11, 1, 1, 0, 0, 0, 0] {
        push_u16(&mut output, value);
    }
    for value in [2026u16, 11, 1, 1, 0, 0, 0, 0] {
        push_u16(&mut output, value);
    }
    push_u64(&mut output, 0);
    for _ in 0..16 {
        push_u32(&mut output, 0);
    }
    output
}

fn object_header(object_type: u32, object_size: u32, timestamp: u64) -> Vec<u8> {
    let mut output = Vec::new();
    push_u32(&mut output, 0x4A42_4F4C);
    push_u16(&mut output, 32);
    push_u16(&mut output, 1);
    push_u32(&mut output, object_size);
    push_u32(&mut output, object_type);
    push_u32(&mut output, 0x2);
    push_u16(&mut output, 0);
    push_u16(&mut output, 0);
    push_u64(&mut output, timestamp);
    output
}

fn can_message(channel: u16, id: u32, timestamp: u64, data: &[u8]) -> Vec<u8> {
    let mut output = object_header(1, 48, timestamp);
    push_u16(&mut output, channel);
    output.push(0);
    output.push(data.len() as u8);
    push_u32(&mut output, id);
    output.extend_from_slice(data);
    output.resize(48, 0);
    output
}

fn lin_message(channel: u16, id: u8, timestamp: u64, data: &[u8]) -> Vec<u8> {
    let mut output = object_header(11, 56, timestamp);
    push_u16(&mut output, channel);
    output.push(id);
    output.push(data.len() as u8);
    output.extend_from_slice(data);
    output.resize(48, 0);
    let mut sum = u16::from(protected_identifier(id));
    for byte in data {
        sum += u16::from(*byte);
        if sum > 0xFF {
            sum = (sum & 0xFF) + 1;
        }
    }
    push_u16(&mut output, u16::from(!(sum as u8)));
    output.push(0);
    output.push(0);
    output.resize(56, 0);
    output
}

fn lin_message_with_bad_checksum(channel: u16, id: u8, timestamp: u64, data: &[u8]) -> Vec<u8> {
    let mut output = lin_message(channel, id, timestamp, data);
    output[48..50].copy_from_slice(&0_u16.to_le_bytes());
    output
}

fn lin_crc_error2(channel: u16, id: u8, timestamp: u64, data: &[u8]) -> Vec<u8> {
    let object_size = 184;
    let mut output = object_header(60, object_size, timestamp);
    push_u64(&mut output, 0);
    push_u32(&mut output, 19_200);
    push_u16(&mut output, channel);
    push_u16(&mut output, 0);
    push_u64(&mut output, 0);
    push_u64(&mut output, 0);
    push_u16(&mut output, 1);
    push_u16(&mut output, 0x100);
    output.push(0);
    output.push(id);
    output.push(data.len() as u8);
    output.push(1);
    for _ in 0..9 {
        push_u64(&mut output, 0);
    }
    let mut payload = [0_u8; 8];
    payload[..data.len()].copy_from_slice(data);
    output.extend_from_slice(&payload);
    push_u16(&mut output, 0);
    output.push(1);
    output.push(0);
    output.push(0);
    output.push(0);
    push_u16(&mut output, 0);
    push_u32(&mut output, 19_200);
    push_u32(&mut output, 0);
    output.extend_from_slice(&0.0_f64.to_le_bytes());
    push_u32(&mut output, 0);
    push_u32(&mut output, 0);
    assert_eq!(output.len(), object_size as usize);
    output
}

fn log_container(uncompressed: &[u8]) -> Vec<u8> {
    let object_size = 32 + uncompressed.len() as u32;
    let mut output = Vec::new();
    push_u32(&mut output, 0x4A42_4F4C);
    push_u16(&mut output, 16);
    push_u16(&mut output, 1);
    push_u32(&mut output, object_size);
    push_u32(&mut output, 10);
    push_u16(&mut output, 0);
    push_u16(&mut output, 0);
    push_u32(&mut output, 0);
    push_u32(&mut output, uncompressed.len() as u32);
    push_u32(&mut output, 0);
    output.extend_from_slice(uncompressed);
    output.resize(output.len() + (object_size as usize % 4), 0);
    output
}

fn write_blf(path: &Path) {
    let mut inner = Vec::new();
    inner.extend_from_slice(&can_message(1, 0x100, 10, &[42, 0]));
    inner.extend_from_slice(&can_message(2, 0x100, 20, &[7, 0]));
    inner.extend_from_slice(&lin_message(3, 0x10, 30, &[9]));
    let mut output = file_statistics(inner.len() as u64, 3);
    output.extend_from_slice(&log_container(&inner));
    fs::write(path, output).unwrap();
}

fn write_lin_checksum_error_blf(path: &Path) {
    let inner = lin_crc_error2(3, 0x10, 40, &[0x2A]);
    let mut output = file_statistics(inner.len() as u64, 1);
    output.extend_from_slice(&log_container(&inner));
    fs::write(path, output).unwrap();
}

fn write_lin_bad_checksum_message_blf(path: &Path) {
    let inner = lin_message_with_bad_checksum(3, 0x10, 40, &[0x2A]);
    let mut output = file_statistics(inner.len() as u64, 1);
    output.extend_from_slice(&log_container(&inner));
    fs::write(path, output).unwrap();
}

fn write_unknown_frame_blf(path: &Path) {
    let mut inner = Vec::new();
    inner.extend_from_slice(&can_message(1, 0x101, 10, &[42]));
    inner.extend_from_slice(&lin_message(3, 0x11, 20, &[7]));
    let mut output = file_statistics(inner.len() as u64, 2);
    output.extend_from_slice(&log_container(&inner));
    fs::write(path, output).unwrap();
}

fn write_aggregate_can_blf(path: &Path) {
    let mut inner = Vec::new();
    inner.extend_from_slice(&can_message(1, 0x100, 10, &[42]));
    inner.extend_from_slice(&can_message(1, 0x101, 20, &[7]));
    let mut output = file_statistics(inner.len() as u64, 2);
    output.extend_from_slice(&log_container(&inner));
    fs::write(path, output).unwrap();
}

fn write_muxed_can_blf(path: &Path) {
    let inner = can_message(1, 0x100, 10, &[1, 0xAA, 0xBB]);
    let mut output = file_statistics(inner.len() as u64, 1);
    output.extend_from_slice(&log_container(&inner));
    fs::write(path, output).unwrap();
}

fn write_strict_blf(path: &Path) {
    let mut inner = Vec::new();
    inner.extend_from_slice(&can_message(1, 0x100, 10, &[42]));
    inner.extend_from_slice(&can_message(1, 0x100, 20, &[7, 0]));
    let mut output = file_statistics(inner.len() as u64, 2);
    output.extend_from_slice(&log_container(&inner));
    fs::write(path, output).unwrap();
}

fn write_corrupt_blf(path: &Path) {
    // The container is structurally valid, but its truncated object signature
    // cannot contain a complete BLF object header.  File::open succeeds and
    // the first cursor read returns a terminal BLF error.
    let truncated_object = 0x4A42_4F4Cu32.to_le_bytes();
    let mut output = file_statistics(truncated_object.len() as u64, 1);
    output.extend_from_slice(&log_container(&truncated_object));
    fs::write(path, output).unwrap();
}

fn can_database(name: &str) -> DbcDatabase {
    let source = format!(
        "VERSION \"1.0\"\nBO_ 256 {name}: 2 ECU\n SG_ Speed : 0|8@1+ (1,0) [0|255] \"\" ECU\n"
    );
    DbcDatabase::from_str(&source).unwrap()
}

fn aggregate_can_database() -> DbcDatabase {
    DbcDatabase::from_str(
        "VERSION \"1.0\"\n\
         BO_ 256 FirstStatus: 1 ECU\n\
          SG_ Speed : 0|8@1+ (1,0) [0|255] \"\" ECU\n\
         BO_ 257 SecondStatus: 1 ECU\n\
          SG_ Speed : 0|8@1+ (1,0) [0|255] \"\" ECU\n",
    )
    .unwrap()
}

fn muxed_can_database() -> DbcDatabase {
    DbcDatabase::from_str(
        "BO_ 256 Muxed: 3 ECU\n\
         SG_ Mux M : 0|4@1+ (1,0) [0|15] \"\" ECU\n\
         SG_ Active m1 : 8|8@1+ (1,0) [0|255] \"\" ECU\n\
         SG_ Inactive m2 : 16|8@1+ (1,0) [0|255] \"\" ECU\n",
    )
    .unwrap()
}

fn lin_database() -> LdfDatabase {
    LdfDatabase::from_str(
        "LIN_description_file;\nNodes { Master: ECU, 5 ms, 0.1 ms ; }\nSignals { State: 8, 0, ECU, ECU ; }\nFrames { Status: 16, ECU, 1 { State, 0 ; } }\n",
    )
    .unwrap()
}

#[test]
fn query_owns_an_independent_cursor_and_resolves_explicit_channels() {
    let path = std::env::temp_dir().join("blf-core-query-tests.blf");
    write_blf(&path);
    let file = File::open(&path).unwrap();
    let database = Arc::new(can_database("Status"));
    let binding_one =
        DatabaseBinding::new(BusKind::Can, 1, Database::Can(database.clone())).unwrap();
    let binding_two =
        DatabaseBinding::new(BusKind::Can, 2, Database::Can(database.clone())).unwrap();

    let mut query = file
        .signal_query(binding_one, "Status.Speed", false)
        .unwrap();
    assert_eq!(file.current_object_count, 0);
    assert_eq!(query.next().unwrap().unwrap().raw_value, Some(42));
    assert_eq!(file.current_object_count, 0);

    let second = file
        .signal_query(binding_two, "Status.Speed", false)
        .unwrap();
    let samples = second.collect().unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].channel, 2);
    assert_eq!(samples[0].raw_value, Some(7));
    assert!(query.next().is_none());
    fs::remove_file(path).ok();
}

#[test]
fn closed_reader_rejects_new_queries_but_existing_queries_keep_their_cursor() {
    let path = std::env::temp_dir().join("blf-core-query-closed-reader-tests.blf");
    write_blf(&path);
    let mut file = File::open(&path).unwrap();
    let binding = DatabaseBinding::new(
        BusKind::Can,
        1,
        Database::Can(Arc::new(can_database("Status"))),
    )
    .unwrap();
    let mut existing = file
        .signal_query(binding.clone(), "Status.Speed", false)
        .unwrap();

    file.close();
    assert!(matches!(
        file.signal_query(binding.clone(), "Status.Speed", false),
        Err(blf_core::QueryError::ReaderClosed)
    ));
    assert!(matches!(
        file.aggregate_signal_query(vec![binding], "Speed", false),
        Err(blf_core::QueryError::ReaderClosed)
    ));

    let sample = existing.next().unwrap().unwrap();
    assert_eq!(sample.raw_value, Some(42));
    fs::remove_file(path).ok();
}

#[test]
fn aggregate_query_preserves_channel_context_and_decodes_lin() {
    let path = std::env::temp_dir().join("blf-core-query-lin-tests.blf");
    write_blf(&path);
    let file = File::open(&path).unwrap();
    let database = Arc::new(can_database("Status"));
    let can_binding = DatabaseBinding::new(BusKind::Can, 1, Database::Can(database)).unwrap();
    let lin_binding =
        DatabaseBinding::new(BusKind::Lin, 3, Database::Lin(Arc::new(lin_database()))).unwrap();

    let samples = file
        .aggregate_signal_query(vec![can_binding], "Speed", false)
        .unwrap()
        .collect()
        .unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].channel, 1);

    let samples = file
        .signal_query(lin_binding, "Status.State", false)
        .unwrap()
        .collect()
        .unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].status, DecodeStatus::Ok);
    assert_eq!(samples[0].raw_value, Some(9));
    fs::remove_file(path).ok();
}

#[test]
fn strict_aggregate_query_does_not_misclassify_other_same_name_messages() {
    let path = std::env::temp_dir().join("blf-core-query-aggregate-same-name-tests.blf");
    write_aggregate_can_blf(&path);
    let file = File::open(&path).unwrap();
    let binding = DatabaseBinding::new(
        BusKind::Can,
        1,
        Database::Can(Arc::new(aggregate_can_database())),
    )
    .unwrap();

    let samples = file
        .aggregate_signal_query(vec![binding], "Speed", true)
        .unwrap()
        .collect()
        .unwrap();
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0].message_name, "FirstStatus");
    assert_eq!(samples[0].raw_value, Some(42));
    assert_eq!(samples[1].message_name, "SecondStatus");
    assert_eq!(samples[1].raw_value, Some(7));
    assert!(samples
        .iter()
        .all(|sample| sample.status == DecodeStatus::Ok));
    fs::remove_file(path).ok();
}

#[test]
fn inactive_multiplex_branch_is_omitted_in_both_query_modes() {
    let path = std::env::temp_dir().join("blf-core-query-inactive-mux-tests.blf");
    write_muxed_can_blf(&path);
    let file = File::open(&path).unwrap();
    let binding = DatabaseBinding::new(
        BusKind::Can,
        1,
        Database::Can(Arc::new(muxed_can_database())),
    )
    .unwrap();

    let tolerant = file
        .signal_query(binding.clone(), "Muxed.Inactive", false)
        .unwrap()
        .collect()
        .unwrap();
    assert!(tolerant.is_empty());

    let strict = file
        .signal_query(binding, "Muxed.Inactive", true)
        .unwrap()
        .collect()
        .unwrap();
    assert!(strict.is_empty());
    fs::remove_file(path).ok();
}

#[test]
fn binding_requires_matching_protocol() {
    let path = std::env::temp_dir().join("blf-core-query-binding-tests.blf");
    let error = DatabaseBinding::new(
        BusKind::Lin,
        1,
        Database::Can(Arc::new(can_database("Status"))),
    )
    .unwrap_err();
    assert!(error.to_string().contains("cannot be bound"));
    fs::remove_file(path).ok();
}

#[test]
fn aggregate_query_rejects_duplicate_bus_channel_bindings() {
    let path = std::env::temp_dir().join("blf-core-query-duplicate-binding-tests.blf");
    write_blf(&path);
    let file = File::open(&path).unwrap();
    let database = Arc::new(can_database("Status"));
    let first = DatabaseBinding::new(BusKind::Can, 1, Database::Can(database.clone())).unwrap();
    let second = DatabaseBinding::new(BusKind::Can, 1, Database::Can(database)).unwrap();

    let error = file
        .aggregate_signal_query(vec![first, second], "Status.Speed", false)
        .unwrap_err();
    assert!(error.to_string().contains("duplicate database binding"));
    fs::remove_file(path).ok();
}

#[test]
fn strict_query_terminates_after_first_decode_error() {
    let path = std::env::temp_dir().join("blf-core-query-strict-tests.blf");
    write_strict_blf(&path);
    let file = File::open(&path).unwrap();
    let database = Arc::new(
        DbcDatabase::from_str(
            "BO_ 256 Status: 2 ECU\n SG_ Wide : 0|16@1+ (1,0) [0|65535] \"\" ECU\n",
        )
        .unwrap(),
    );
    let binding = DatabaseBinding::new(BusKind::Can, 1, Database::Can(database)).unwrap();
    let mut query = file.signal_query(binding, "Status.Wide", true).unwrap();

    assert!(query.next().unwrap().is_err());
    assert!(query.next().is_none());
    fs::remove_file(path).ok();
}

#[test]
fn tolerant_query_terminates_after_unrecoverable_blf_read_error() {
    let path = std::env::temp_dir().join("blf-core-query-tolerant-read-error-tests.blf");
    write_corrupt_blf(&path);
    let file = File::open(&path).unwrap();
    let database = Arc::new(can_database("Status"));
    let binding = DatabaseBinding::new(BusKind::Can, 1, Database::Can(database)).unwrap();
    let mut query = file.signal_query(binding, "Status.Speed", false).unwrap();

    assert!(query.next().unwrap().is_err());
    assert!(query.next().is_none());
    fs::remove_file(path).ok();
}

#[test]
fn lin_checksum_error_query_keeps_value_and_status() {
    let path = std::env::temp_dir().join("blf-core-query-lin-checksum-error-tests.blf");
    write_lin_checksum_error_blf(&path);
    let file = File::open(&path).unwrap();
    let database = Arc::new(lin_database());
    let binding = DatabaseBinding::new(BusKind::Lin, 3, Database::Lin(database)).unwrap();

    let mut tolerant = file
        .signal_query(binding.clone(), "Status.State", false)
        .unwrap();
    let sample = tolerant.next().unwrap().unwrap();
    assert_eq!(sample.status, DecodeStatus::ChecksumError);
    assert_eq!(sample.raw_value, Some(0x2A));
    assert_eq!(sample.physical_value, Some(42.0_f64));
    assert!(tolerant.next().is_none());

    let mut strict = file.signal_query(binding, "Status.State", true).unwrap();
    assert!(strict.next().unwrap().is_err());
    assert!(strict.next().is_none());
    fs::remove_file(path).ok();
}

#[test]
fn lin_checksum_error_without_payload_keeps_checksum_status() {
    let path = std::env::temp_dir().join("blf-core-query-lin-no-payload-checksum-tests.blf");
    let inner = lin_crc_error2(3, 0x10, 40, &[]);
    let mut output = file_statistics(inner.len() as u64, 1);
    output.extend_from_slice(&log_container(&inner));
    fs::write(&path, output).unwrap();
    let file = File::open(&path).unwrap();
    let database = Arc::new(lin_database());
    let binding = DatabaseBinding::new(BusKind::Lin, 3, Database::Lin(database)).unwrap();

    let mut query = file.signal_query(binding, "Status.State", false).unwrap();
    let sample = query.next().unwrap().unwrap();
    assert_eq!(sample.status, DecodeStatus::ChecksumError);
    assert!(sample.raw_value.is_none());
    assert!(sample.physical_value.is_none());
    assert!(sample.error.as_deref().unwrap().contains("checksum"));
    assert!(query.next().is_none());
    fs::remove_file(path).ok();
}

#[test]
fn ordinary_lin_message_checksum_error_keeps_value_and_status() {
    let path = std::env::temp_dir().join("blf-core-query-lin-message-checksum-tests.blf");
    write_lin_bad_checksum_message_blf(&path);
    let file = File::open(&path).unwrap();
    let database = Arc::new(lin_database());
    let binding = DatabaseBinding::new(BusKind::Lin, 3, Database::Lin(database)).unwrap();

    let mut tolerant = file
        .signal_query(binding.clone(), "Status.State", false)
        .unwrap();
    let sample = tolerant.next().unwrap().unwrap();
    assert_eq!(sample.status, DecodeStatus::ChecksumError);
    assert_eq!(sample.raw_value, Some(0x2A));
    assert_eq!(sample.physical_value, Some(42.0_f64));
    assert!(sample.error.as_deref().unwrap().contains("checksum"));
    assert!(tolerant.next().is_none());

    let mut strict = file.signal_query(binding, "Status.State", true).unwrap();
    assert!(strict.next().unwrap().is_err());
    assert!(strict.next().is_none());
    fs::remove_file(path).ok();
}

#[test]
fn tolerant_query_reports_unknown_can_and_lin_frames() {
    let path = std::env::temp_dir().join("blf-core-query-unknown-frame-tests.blf");
    write_unknown_frame_blf(&path);
    let file = File::open(&path).unwrap();
    let can = DatabaseBinding::new(
        BusKind::Can,
        1,
        Database::Can(Arc::new(can_database("Status"))),
    )
    .unwrap();
    let mut can_query = file.signal_query(can, "Status.Speed", false).unwrap();
    let can_sample = can_query.next().unwrap().unwrap();
    assert_eq!(can_sample.status, DecodeStatus::UnknownFrame);
    assert_eq!(can_sample.frame_id, 0x101);
    assert_eq!(can_sample.message_name, "<unknown>");
    assert_eq!(can_sample.signal_name, "Speed");
    assert_eq!(can_sample.data, vec![42]);
    assert!(can_sample.raw_value.is_none());

    let lin =
        DatabaseBinding::new(BusKind::Lin, 3, Database::Lin(Arc::new(lin_database()))).unwrap();
    let mut lin_query = file.signal_query(lin, "Status.State", false).unwrap();
    let lin_sample = lin_query.next().unwrap().unwrap();
    assert_eq!(lin_sample.status, DecodeStatus::UnknownFrame);
    assert_eq!(lin_sample.frame_id, 0x11);
    assert_eq!(lin_sample.data, vec![7]);

    fs::remove_file(path).ok();
}

#[test]
fn strict_query_rejects_unknown_frames() {
    let path = std::env::temp_dir().join("blf-core-query-strict-unknown-frame-tests.blf");
    write_unknown_frame_blf(&path);
    let file = File::open(&path).unwrap();
    let binding = DatabaseBinding::new(
        BusKind::Can,
        1,
        Database::Can(Arc::new(can_database("Status"))),
    )
    .unwrap();
    let mut query = file.signal_query(binding, "Status.Speed", true).unwrap();
    let error = query.next().unwrap().unwrap_err();
    assert!(error.to_string().contains("unknown CAN frame"));
    assert!(query.next().is_none());
    fs::remove_file(path).ok();
}
