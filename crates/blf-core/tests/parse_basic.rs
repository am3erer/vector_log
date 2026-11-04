use std::fs;
use std::io::Write;

use flate2::write::ZlibEncoder;
use flate2::Compression;

use blf_core::{File, Object, ObjectType, ReaderOptions};

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_system_time(out: &mut Vec<u8>) {
    for value in [2026u16, 10, 5, 31, 12, 30, 45, 123] {
        push_u16(out, value);
    }
}

fn file_statistics(uncompressed_size: u64, object_count: u32) -> Vec<u8> {
    let mut data = Vec::new();
    push_u32(&mut data, 0x4747_4F4C);
    push_u32(&mut data, 144);
    push_u32(&mut data, 4_080_200);
    data.extend_from_slice(&[1, 1, 1, 0]);
    push_u64(&mut data, 0);
    push_u64(&mut data, uncompressed_size);
    push_u32(&mut data, object_count);
    push_u32(&mut data, 1);
    push_system_time(&mut data);
    push_system_time(&mut data);
    push_u64(&mut data, 0);
    for _ in 0..16 {
        push_u32(&mut data, 0);
    }
    data
}

fn object_header(object_type: u32, object_size: u32, timestamp: u64) -> Vec<u8> {
    let mut data = Vec::new();
    push_u32(&mut data, 0x4A42_4F4C);
    push_u16(&mut data, 32);
    push_u16(&mut data, 1);
    push_u32(&mut data, object_size);
    push_u32(&mut data, object_type);
    push_u32(&mut data, 0x0000_0002);
    push_u16(&mut data, 0);
    push_u16(&mut data, 0);
    push_u64(&mut data, timestamp);
    data
}

fn object_header2(object_type: u32, object_size: u32, timestamp: u64, original: u64) -> Vec<u8> {
    let mut data = Vec::new();
    push_u32(&mut data, 0x4A42_4F4C);
    push_u16(&mut data, 40);
    push_u16(&mut data, 2);
    push_u32(&mut data, object_size);
    push_u32(&mut data, object_type);
    push_u32(&mut data, 0x0000_0002);
    data.push(1);
    data.push(0);
    push_u16(&mut data, 0);
    push_u64(&mut data, timestamp);
    push_u64(&mut data, original);
    data
}

fn legacy_object_header2(
    object_type: u32,
    object_size: u32,
    timestamp: u64,
    original: u64,
) -> Vec<u8> {
    let mut data = object_header2(object_type, object_size, timestamp, original);
    data[6..8].copy_from_slice(&1u16.to_le_bytes());
    data
}

fn can_message() -> Vec<u8> {
    let mut object = object_header(1, 48, 100);
    push_u16(&mut object, 1);
    object.push(1);
    object.push(2);
    push_u32(&mut object, 0x123);
    object.extend_from_slice(&[0xAA, 0xBB, 0, 0, 0, 0, 0, 0]);
    object
}

fn can_message2() -> Vec<u8> {
    let mut object = object_header(86, 52, 200);
    push_u16(&mut object, 2);
    object.push(0);
    object.push(4);
    push_u32(&mut object, 0x456);
    object.extend_from_slice(&[1, 2, 3, 4]);
    push_u32(&mut object, 1234);
    object.push(42);
    object.push(0);
    push_u16(&mut object, 0);
    object
}

fn can_fd_message64() -> Vec<u8> {
    let mut object = object_header(101, 84, 300);
    object.push(3);
    object.push(9);
    object.push(12);
    object.push(1);
    push_u32(&mut object, 0x789);
    push_u32(&mut object, 5678);
    push_u32(&mut object, 0);
    push_u32(&mut object, 0);
    push_u32(&mut object, 0);
    push_u32(&mut object, 0);
    push_u32(&mut object, 0);
    push_u16(&mut object, 100);
    object.push(1);
    object.push(0);
    push_u32(&mut object, 0xDEAD_BEEF);
    object.extend_from_slice(&[10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21]);
    object
}

fn lin_message() -> Vec<u8> {
    let mut object = object_header(11, 56, 500);
    push_u16(&mut object, 1);
    object.push(0x10);
    object.push(8);
    object.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    object.extend_from_slice(&[0, 0, 0, 0]);
    push_u16(&mut object, 0x1234);
    object.push(1);
    object.push(0);
    push_u32(&mut object, 0);
    object
}

fn lin_message2(object_size: u32, extension: &[u8]) -> Vec<u8> {
    let mut object = object_header(57, object_size, 600);
    // LinDatabyteTimestampEvent: bus/sync fields, descriptor, and 9 timestamps.
    object.extend_from_slice(&[0; 112]);
    object.extend_from_slice(&[0; 8]); // data
    push_u16(&mut object, 0x1234); // crc
    object.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]); // flags and FSM fields
    push_u16(&mut object, 0x5678); // reservedLinMessage2
    object.extend_from_slice(extension);
    assert_eq!(object.len(), object_size as usize);
    object
}

fn restore_point_container() -> Vec<u8> {
    let mut restore_points = Vec::new();
    push_u32(&mut restore_points, 56);
    push_u32(&mut restore_points, 25);
    push_u64(&mut restore_points, 1000);
    push_u64(&mut restore_points, 2000);
    push_u32(&mut restore_points, 3000);
    push_u32(&mut restore_points, 4000);
    push_u64(&mut restore_points, 5000);
    push_u64(&mut restore_points, 6000);
    push_u32(&mut restore_points, 7000);
    push_u32(&mut restore_points, 8000);

    let object_size = 48 + restore_points.len() as u32;
    let mut object = object_header(115, object_size, 900);
    object.extend_from_slice(&[0; 14]);
    push_u16(&mut object, restore_points.len() as u16);
    object.extend_from_slice(&restore_points);
    assert_eq!(object.len(), object_size as usize);
    object
}

fn var_object_header(object_type: u32, object_size: u32, timestamp: u64) -> Vec<u8> {
    let mut data = object_header(object_type, object_size, timestamp);
    data[20..22].copy_from_slice(&32u16.to_le_bytes());
    data
}

fn malformed_global_marker() -> Vec<u8> {
    let mut object = object_header(96, 571, 700);
    object.extend_from_slice(&[0; 16]); // event type and colors
    push_u32(&mut object, 131);
    push_u32(&mut object, 131);
    push_u32(&mut object, 261);
    object.extend_from_slice(&[0; 4]); // reservedGlobalMarker3
    object.extend_from_slice(&[0; 8]); // reservedGlobalMarker4
    object.extend_from_slice(&[b'G'; 131]);
    object.extend_from_slice(&[b'M'; 131]);
    object.extend_from_slice(&[b'D'; 261]);
    assert_eq!(object.len(), 595);
    object
}

fn log_container(uncompressed: &[u8]) -> Vec<u8> {
    let object_size = 32 + uncompressed.len() as u32;
    let mut container = Vec::new();
    push_u32(&mut container, 0x4A42_4F4C);
    push_u16(&mut container, 16);
    push_u16(&mut container, 1);
    push_u32(&mut container, object_size);
    push_u32(&mut container, 10);
    push_u16(&mut container, 0);
    push_u16(&mut container, 0);
    push_u32(&mut container, 0);
    push_u32(&mut container, uncompressed.len() as u32);
    push_u32(&mut container, 0);
    container.extend_from_slice(uncompressed);
    container.resize(container.len() + object_size as usize % 4, 0);
    container
}

fn compressed_log_container(uncompressed: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(uncompressed).unwrap();
    let compressed = encoder.finish().unwrap();
    let object_size = 32 + compressed.len() as u32;
    let mut container = Vec::new();
    push_u32(&mut container, 0x4A42_4F4C);
    push_u16(&mut container, 16);
    push_u16(&mut container, 1);
    push_u32(&mut container, object_size);
    push_u32(&mut container, 10);
    push_u16(&mut container, 2);
    push_u16(&mut container, 0);
    push_u32(&mut container, 0);
    push_u32(&mut container, uncompressed.len() as u32);
    push_u32(&mut container, 0);
    container.extend_from_slice(&compressed);
    container.resize(container.len() + object_size as usize % 4, 0);
    container
}

fn write_blf(path: &std::path::Path) {
    let inner = {
        let mut inner = Vec::new();
        inner.extend_from_slice(&can_message());
        inner.extend_from_slice(&can_message2());
        inner.extend_from_slice(&can_fd_message64());
        inner.extend_from_slice(&lin_message());
        inner
    };
    let mut file = file_statistics(inner.len() as u64, 4);
    file.extend_from_slice(&log_container(&inner));
    fs::write(path, file).unwrap();
}

fn write_blf_with_inner(path: &std::path::Path, inner: &[u8], object_count: u32) {
    let mut file = file_statistics(inner.len() as u64, object_count);
    file.extend_from_slice(&log_container(inner));
    fs::write(path, file).unwrap();
}

#[test]
fn parses_p0_objects() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("basic.blf");
    write_blf(&path);

    let mut reader = File::open(&path).unwrap();
    assert_eq!(reader.file_statistics.object_count, 4);
    assert_eq!(reader.file_statistics.statistics_size, 144);

    let first = reader.read().unwrap().unwrap();
    assert_eq!(first.object_type(), ObjectType::CanMessage);
    match first {
        Object::CanMessage(frame) => {
            assert_eq!(frame.id, 0x123);
            assert_eq!(&frame.data[..2], &[0xAA, 0xBB]);
            assert_eq!(frame.header.object_time_stamp, 100);
        }
        _ => panic!("wrong object"),
    }

    let second = reader.read().unwrap().unwrap();
    assert_eq!(second.object_type(), ObjectType::CanMessage2);
    match second {
        Object::CanMessage2(frame) => {
            assert_eq!(frame.id, 0x456);
            assert_eq!(frame.data, vec![1, 2, 3, 4]);
        }
        _ => panic!("wrong object"),
    }

    let third = reader.read().unwrap().unwrap();
    assert_eq!(third.object_type(), ObjectType::CanFdMessage64);
    match third {
        Object::CanFdMessage64(frame) => {
            assert_eq!(frame.valid_data_bytes, 12);
            assert_eq!(frame.data.len(), 12);
            assert_eq!(frame.data[0], 10);
            assert!(frame.ext_data.is_none());
        }
        _ => panic!("wrong object"),
    }

    let fourth = reader.read().unwrap().unwrap();
    assert_eq!(fourth.object_type(), ObjectType::LinMessage);
    match fourth {
        Object::Lin(frame) => {
            assert_eq!(frame.header.object_time_stamp, 500);
            assert_eq!(frame.object_type, ObjectType::LinMessage);
            assert!(frame
                .fields
                .iter()
                .any(|(name, value)| name == "id" && value == &blf_core::FieldValue::U8(0x10)));
        }
        _ => panic!("wrong object"),
    }

    assert!(reader.read().unwrap().is_none());
}

#[test]
fn independent_cursors_do_not_share_read_state() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("independent-cursors.blf");
    write_blf(&path);

    let mut ordinary = File::open(&path).unwrap();
    let mut first = ordinary.independent_cursor();
    let mut second = ordinary.independent_cursor();

    assert_eq!(
        ordinary.read().unwrap().unwrap().object_type(),
        ObjectType::CanMessage
    );
    assert_eq!(
        first.read().unwrap().unwrap().object_type(),
        ObjectType::CanMessage
    );
    assert_eq!(
        second.read().unwrap().unwrap().object_type(),
        ObjectType::CanMessage
    );

    assert_eq!(
        ordinary.read().unwrap().unwrap().object_type(),
        ObjectType::CanMessage2
    );
    assert_eq!(
        first.read().unwrap().unwrap().object_type(),
        ObjectType::CanMessage2
    );
    assert_eq!(
        second.read().unwrap().unwrap().object_type(),
        ObjectType::CanMessage2
    );
    assert_eq!(ordinary.current_object_count, 2);
    assert_eq!(first.current_object_count, 2);
    assert_eq!(second.current_object_count, 2);
}

#[test]
fn tolerant_reader_skips_truncated_object_and_reads_following_object() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("truncated-object.blf");
    let mut inner = can_message();
    inner.extend_from_slice(&object_header(1, 48, 100));
    inner.extend_from_slice(&[0; 4]);
    inner.extend_from_slice(&can_message2());
    write_blf_with_inner(&path, &inner, 2);

    let mut reader = File::open(&path).unwrap();
    assert!(reader.read().unwrap().is_some());
    assert_eq!(
        reader.read().unwrap().unwrap().object_type(),
        ObjectType::CanMessage2
    );
    assert_eq!(reader.skipped_object_count, 1);
    assert!(reader.read().unwrap().is_none());
}

#[test]
fn tolerant_reader_recovers_incomplete_object_header() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("incomplete-object-header.blf");
    let mut inner = object_header(1, 32, 100);
    inner[4..6].copy_from_slice(&16u16.to_le_bytes());
    inner.extend_from_slice(&can_message2());
    write_blf_with_inner(&path, &inner, 2);

    let mut reader = File::open(&path).unwrap();
    assert_eq!(
        reader.read().unwrap().unwrap().object_type(),
        ObjectType::CanMessage2
    );
    assert_eq!(reader.skipped_object_count, 1);
    assert!(reader.read().unwrap().is_none());
}

#[test]
fn accepts_zero_header_size_vector_variant() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("zero-header-size-vector-variant.blf");
    let mut inner = can_fd_message64();
    inner[4..6].copy_from_slice(&0u16.to_le_bytes());
    write_blf_with_inner(&path, &inner, 1);

    let mut reader = File::open(&path).unwrap();
    assert_eq!(
        reader.read().unwrap().unwrap().object_type(),
        ObjectType::CanFdMessage64
    );
    assert!(reader.read().unwrap().is_none());
}

#[test]
fn tolerant_reader_skips_bad_container_tail_and_reads_next_container() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("truncated-container-tail.blf");
    let mut output = file_statistics(0, 2);
    let mut first = object_header(1, 48, 100);
    first.extend_from_slice(&[0; 4]);
    output.extend_from_slice(&log_container(&first));
    output.extend_from_slice(&log_container(&can_message2()));
    fs::write(&path, output).unwrap();

    let mut reader = File::open(&path).unwrap();
    assert_eq!(
        reader.read().unwrap().unwrap().object_type(),
        ObjectType::CanMessage2
    );
    assert_eq!(reader.skipped_object_count, 1);
    assert!(reader.read().unwrap().is_none());
}

#[test]
fn strict_reader_reports_truncated_object_instead_of_eof() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("strict-truncated-object.blf");
    let mut inner = object_header(1, 48, 100);
    inner.extend_from_slice(&[0; 4]);
    write_blf_with_inner(&path, &inner, 1);

    let mut reader = File::open_with_options(&path, true).unwrap();
    assert!(matches!(reader.read(), Err(blf_core::BlfError::Truncated)));
    assert_eq!(reader.skipped_object_count, 0);
}

#[test]
fn rejects_zero_sized_unknown_objects() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("zero-sized-object.blf");
    let mut inner = object_header(999, 0, 100);
    inner.truncate(16);
    write_blf_with_inner(&path, &inner, 1);

    let mut reader = File::open(&path).unwrap();
    assert!(matches!(reader.read(), Err(blf_core::BlfError::Format(_))));
}

#[test]
fn reports_truncated_log_container() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("truncated-container.blf");
    let mut file = file_statistics(4, 0);
    push_u32(&mut file, 0x4A42_4F4C);
    push_u16(&mut file, 16);
    push_u16(&mut file, 1);
    push_u32(&mut file, 36);
    push_u32(&mut file, 10);
    push_u16(&mut file, 0);
    push_u16(&mut file, 0);
    push_u32(&mut file, 0);
    push_u32(&mut file, 4);
    push_u32(&mut file, 0);
    fs::write(&path, file).unwrap();

    let mut reader = File::open(&path).unwrap();
    assert!(matches!(reader.read(), Err(blf_core::BlfError::Truncated)));
}

#[test]
fn enforces_configured_object_size_limit() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("limited-object.blf");
    let object = can_message();
    write_blf_with_inner(&path, &object, 1);

    let options = ReaderOptions {
        max_object_size: 47,
        ..ReaderOptions::default()
    };
    let mut reader = File::open_with_reader_options(&path, options).unwrap();
    assert!(matches!(
        reader.read(),
        Err(blf_core::BlfError::Limit {
            kind: "BLF object",
            limit: 47,
            actual: 80,
        })
    ));
}

#[test]
fn enforces_configured_container_size_limit_before_copying_payload() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("limited-container.blf");
    let inner = vec![0u8; 64];
    write_blf_with_inner(&path, &inner, 0);

    let options = ReaderOptions {
        max_object_size: 32 + 32,
        ..ReaderOptions::default()
    };
    let mut reader = File::open_with_reader_options(&path, options).unwrap();
    assert!(matches!(
        reader.read(),
        Err(blf_core::BlfError::Limit {
            kind: "BLF object",
            limit: 64,
            actual: 96,
        })
    ));
}

#[test]
fn enforces_actual_uncompressed_payload_limit_before_copying_uncompressed_container() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("limited-uncompressed-payload.blf");
    let inner = vec![0u8; 64];
    let mut file = file_statistics(inner.len() as u64, 0);
    file.extend_from_slice(&log_container(&inner));
    // Keep the declared size below the limit while leaving the actual stored
    // payload larger. The reader must check before calling to_vec().
    file[144 + 24..144 + 28].copy_from_slice(&8u32.to_le_bytes());
    fs::write(&path, file).unwrap();

    let options = ReaderOptions {
        max_uncompressed_container_size: 32,
        ..ReaderOptions::default()
    };
    let mut reader = File::open_with_reader_options(&path, options).unwrap();
    assert!(matches!(
        reader.read(),
        Err(blf_core::BlfError::Limit {
            kind: "uncompressed log container",
            limit: 32,
            actual: 64,
        })
    ));
}

#[test]
fn parses_structured_and_unknown_objects() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("all-known-objects.blf");
    let mut inner = object_header(3, 40, 1003);
    inner.extend_from_slice(&[1, 0, 2, 0]);
    inner.extend_from_slice(&3u32.to_le_bytes());
    let mut most = object_header2(33, 73, 1033, 2033);
    most.extend_from_slice(&[0; 32]);
    most.extend_from_slice(&[0xA5]);
    inner.extend_from_slice(&most);
    let mut unknown = object_header(999, 33, 1999);
    unknown.push(0xA5);
    inner.extend_from_slice(&unknown);
    write_blf_with_inner(&path, &inner, 3);

    let mut reader = File::open(&path).unwrap();
    match reader.read().unwrap().unwrap() {
        Object::Structured(value) => {
            assert_eq!(value.object_type, ObjectType::CanOverload);
            assert!(value.extensions.is_empty());
            assert!(value.fields.iter().any(|(name, _)| name == "channel"));
        }
        _ => panic!("expected structured CAN object"),
    }
    match reader.read().unwrap().unwrap() {
        Object::Structured(value) => {
            assert_eq!(value.object_type, ObjectType::MostPkt2);
            assert_eq!(value.extensions, vec![0xA5]);
            match value.header {
                blf_core::RawObjectHeader::Object2(header) => {
                    assert_eq!(header.object_time_stamp, 1033);
                    assert_eq!(header.original_time_stamp, 2033);
                }
                _ => panic!("expected ObjectHeader2"),
            }
        }
        _ => panic!("expected structured MOST object"),
    }
    assert!(reader.read().unwrap().is_none());

    let mut skipping = File::open_with_reader_options(
        &path,
        ReaderOptions {
            skip_unknown_objects: true,
            ..ReaderOptions::default()
        },
    )
    .unwrap();
    assert!(matches!(skipping.read(), Ok(Some(Object::Structured(_)))));
    assert!(matches!(skipping.read(), Ok(Some(Object::Structured(_)))));
    assert!(skipping.read().unwrap().is_none());

    let mut preserving = File::open_with_reader_options(
        &path,
        ReaderOptions {
            preserve_unknown_objects: true,
            ..ReaderOptions::default()
        },
    )
    .unwrap();
    assert!(matches!(preserving.read(), Ok(Some(Object::Structured(_)))));
    assert!(matches!(preserving.read(), Ok(Some(Object::Structured(_)))));
    match preserving.read().unwrap().unwrap() {
        Object::Raw(raw) => {
            let raw_type = match raw.header {
                blf_core::RawObjectHeader::Object(header) => header.base.raw_object_type,
                blf_core::RawObjectHeader::Object2(header) => header.base.raw_object_type,
                blf_core::RawObjectHeader::Var(header) => header.base.raw_object_type,
                blf_core::RawObjectHeader::Base(header) => header.raw_object_type,
            };
            assert_eq!(raw_type, 999);
            assert_eq!(raw.payload, vec![0xA5]);
        }
        _ => panic!("expected unknown raw object when preservation is enabled"),
    }
}

#[test]
fn parses_restore_point_entries() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("restore-points.blf");
    let inner = restore_point_container();
    write_blf_with_inner(&path, &inner, 1);

    let mut reader = File::open(&path).unwrap();
    match reader.read().unwrap().unwrap() {
        Object::RestorePointContainer(value) => {
            let points = value.restore_points.expect("restore point payload");
            assert_eq!(points.object_size, 56);
            assert_eq!(points.object_interval, 25);
            assert_eq!(points.restore_points.len(), 2);
            assert_eq!(points.restore_points[0].time_stamp, 1000);
            assert_eq!(points.restore_points[0].compressed_file_position, 2000);
            assert_eq!(points.restore_points[1].uncompressed_file_offset, 7000);
            assert_eq!(points.restore_points[1].unknown_restore_point, 8000);
        }
        _ => panic!("expected restore point container"),
    }
    assert!(reader.read().unwrap().is_none());
}

#[test]
fn preserves_opaque_reserved_restore_point_payload() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("opaque-restore-point-payload.blf");
    let mut object = object_header(115, 56, 901);
    object.extend_from_slice(&[0; 14]);
    push_u16(&mut object, 8);
    object.extend_from_slice(&[0x78, 0x9c, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
    assert_eq!(object.len(), 56);
    write_blf_with_inner(&path, &object, 1);

    let mut reader = File::open(&path).unwrap();
    match reader.read().unwrap().unwrap() {
        Object::RestorePointContainer(value) => {
            assert_eq!(
                value.data,
                vec![0x78, 0x9c, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]
            );
            assert!(value.restore_points.is_none());
        }
        _ => panic!("expected restore point container"),
    }
    assert!(reader.read().unwrap().is_none());
}

#[test]
fn uses_var_object_header_for_variable_objects() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("var-object-header.blf");

    let mut function_bus = var_object_header(124, 53, 1);
    function_bus.extend_from_slice(&[1, 0, 0, 0]);
    function_bus.extend_from_slice(&[2, 0, 0, 0]);
    function_bus.extend_from_slice(&[3, 0, 0, 0]);
    function_bus.extend_from_slice(&[2, 0, 0, 0]);
    function_bus.extend_from_slice(b"abcde");
    assert_eq!(function_bus.len(), 53);
    write_blf_with_inner(&path, &function_bus, 1);

    let mut reader = File::open(&path).unwrap();
    match reader.read().unwrap().unwrap() {
        Object::Structured(value) => {
            match value.header {
                blf_core::RawObjectHeader::Var(header) => {
                    assert_eq!(header.object_static_size, 32);
                    assert_eq!(header.object_time_stamp, 1);
                }
                _ => panic!("expected VarObjectHeader"),
            }
            assert!(value.fields.iter().any(|(name, value)| {
                name == "nameLength" && value == &blf_core::FieldValue::U32(3)
            }));
        }
        _ => panic!("expected structured FunctionBus object"),
    }
}

#[test]
fn parses_legacy_header_version_with_object_header2_size() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("legacy-object-header2.blf");
    let mut inner = legacy_object_header2(33, 73, 1033, 2033);
    inner.extend_from_slice(&[0; 32]);
    inner.push(0xA5);
    write_blf_with_inner(&path, &inner, 1);

    let mut reader = File::open(&path).unwrap();
    match reader.read().unwrap().unwrap() {
        Object::Structured(value) => {
            if let blf_core::RawObjectHeader::Object2(header) = value.header {
                assert_eq!(header.base.header_version, 1);
                assert_eq!(header.time_stamp_status, 1);
                assert_eq!(header.original_time_stamp, 2033);
            } else {
                panic!("expected physical ObjectHeader2");
            }
        }
        _ => panic!("expected structured object"),
    }
    assert!(reader.read().unwrap().is_none());
}

#[test]
fn preserves_file_statistics_and_lin_extensions() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("statistics-and-lin-extensions.blf");

    let mut inner = lin_message2(165, &[0xA1]);
    inner.extend_from_slice(&lin_message2(
        184,
        &[
            0xB2, 0xB3, 0xB4, 0xB5, // response baudrate
            0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, // exact baudrate
            0xD2, 0xD3, 0xD4, 0xD5, // early stopbit offsets
            0xE2, 0xE3, 0xE4, 0xE5,
        ],
    ));
    let mut file = file_statistics(inner.len() as u64, 2);
    file[4..8].copy_from_slice(&146u32.to_le_bytes());
    file.extend_from_slice(&[0xF1, 0xF2]);
    file.extend_from_slice(&log_container(&inner));
    fs::write(&path, file).unwrap();

    let mut reader = File::open(&path).unwrap();
    assert_eq!(reader.file_statistics.statistics_size, 146);
    assert_eq!(reader.file_statistics.extensions, vec![0xF1, 0xF2]);

    match reader.read().unwrap().unwrap() {
        Object::Lin(value) => {
            assert_eq!(value.extensions, vec![0xA1]);
            assert!(!value.fields.iter().any(|(name, _)| name == "respBaudrate"));
        }
        _ => panic!("expected LinMessage2"),
    }
    match reader.read().unwrap().unwrap() {
        Object::Lin(value) => {
            assert_eq!(value.extensions, vec![]);
            assert!(value.fields.iter().any(|(name, _)| name == "respBaudrate"));
            assert!(value
                .fields
                .iter()
                .any(|(name, _)| name == "exactHeaderBaudrate"));
        }
        _ => panic!("expected LinMessage2"),
    }
    assert!(reader.read().unwrap().is_none());
}

#[test]
fn recovers_upstream_global_marker_size_bug() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("malformed-global-marker.blf");
    let mut inner = malformed_global_marker();
    inner.extend_from_slice(&[0; 3]); // upstream skips objectSize % 4 padding
    write_blf_with_inner(&path, &inner, 1);

    let mut reader = File::open(&path).unwrap();
    match reader.read().unwrap().unwrap() {
        Object::Structured(value) => {
            assert_eq!(value.object_type, ObjectType::GlobalMarker);
            assert_eq!(value.data.len(), 595);
            assert!(value.extensions.is_empty());
            assert!(value.fields.iter().any(|(name, value)| {
                name == "groupNameLength" && value == &blf_core::FieldValue::U32(131)
            }));
        }
        _ => panic!("expected GlobalMarker"),
    }
    assert!(reader.read().unwrap().is_none());
}

#[test]
fn recovered_global_marker_size_is_limited() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("limited-malformed-global-marker.blf");
    let inner = malformed_global_marker();
    let mut file = file_statistics(inner.len() as u64, 1);
    file.extend_from_slice(&compressed_log_container(&inner));
    fs::write(&path, file).unwrap();

    let options = ReaderOptions {
        max_object_size: 572,
        ..ReaderOptions::default()
    };
    let mut reader = File::open_with_reader_options(&path, options).unwrap();
    assert!(matches!(
        reader.read(),
        Err(blf_core::BlfError::Limit {
            kind: "BLF object",
            limit: 572,
            actual: 595,
        })
    ));
}

#[test]
fn reads_after_close_return_eof() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("closed-reader.blf");
    write_blf(&path);

    let mut reader = File::open(&path).unwrap();
    reader.close();
    assert!(!reader.is_open());
    assert!(reader.read().unwrap().is_none());
}

#[test]
fn strict_mode_rejects_unknown_numeric_object_type() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("unknown-object-strict.blf");
    let mut inner = object_header(999, 33, 100);
    inner.push(0xA5);
    write_blf_with_inner(&path, &inner, 1);

    let mut reader = File::open_with_options(&path, true).unwrap();
    assert!(matches!(
        reader.read(),
        Err(blf_core::BlfError::UnsupportedObject(999))
    ));
}

#[test]
fn parses_serial_event_union_variants_and_can_setting_timings() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("serial-and-can-setting.blf");
    let mut inner = Vec::new();

    let mut single = object_header(90, 64, 1);
    push_u32(&mut single, 0x04);
    push_u32(&mut single, 3);
    push_u32(&mut single, 19_200);
    push_u32(&mut single, 0);
    single.push(0xA5);
    single.resize(64, 0);
    inner.extend_from_slice(&single);

    let mut compact = object_header(90, 64, 2);
    push_u32(&mut compact, 0x08);
    push_u32(&mut compact, 4);
    push_u32(&mut compact, 115_200);
    push_u32(&mut compact, 0);
    compact.push(3);
    compact.extend_from_slice(&[1, 2, 3]);
    compact.resize(64, 0);
    inner.extend_from_slice(&compact);

    let mut general = object_header(90, 32 + 16 + 16 + 3 + 16, 3);
    push_u32(&mut general, 0);
    push_u32(&mut general, 5);
    push_u32(&mut general, 9_600);
    push_u32(&mut general, 0);
    push_u32(&mut general, 3);
    push_u32(&mut general, 16);
    push_u64(&mut general, 0);
    general.extend_from_slice(&[9, 8, 7]);
    push_u64(&mut general, 11);
    push_u64(&mut general, -22i64 as u64);
    inner.extend_from_slice(&general);
    inner.push(0);

    let mut setting = object_header(129, 43, 4);
    push_u16(&mut setting, 7);
    setting.push(1);
    push_u32(&mut setting, 0x1122_3344);
    push_u32(&mut setting, 0x5566_7788);
    inner.extend_from_slice(&setting);

    write_blf_with_inner(&path, &inner, 4);
    let mut reader = File::open(&path).unwrap();

    match reader.read().unwrap().unwrap() {
        Object::Structured(value) => {
            assert_eq!(value.object_type, ObjectType::SerialEvent);
            assert_eq!(value.extensions, vec![]);
            assert!(value.fields.iter().any(|(name, value)| {
                name == "byte" && value == &blf_core::FieldValue::U8(0xA5)
            }));
        }
        _ => panic!("expected single-byte serial event"),
    }
    match reader.read().unwrap().unwrap() {
        Object::Structured(value) => {
            assert!(value.fields.iter().any(|(name, value)| {
                name == "compactLength" && value == &blf_core::FieldValue::U8(3)
            }));
            assert!(value.fields.iter().any(|(name, value)| {
                name == "compactData"
                    && value
                        == &blf_core::FieldValue::Bytes(vec![
                            1, 2, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                        ])
            }));
        }
        _ => panic!("expected compact serial event"),
    }
    match reader.read().unwrap().unwrap() {
        Object::Structured(value) => {
            assert!(value.fields.iter().any(|(name, value)| {
                name == "data" && value == &blf_core::FieldValue::Bytes(vec![9, 8, 7])
            }));
            assert!(value.fields.iter().any(|(name, value)| {
                name == "timeStamps" && value == &blf_core::FieldValue::I64Vec(vec![11, -22])
            }));
        }
        _ => panic!("expected general serial event"),
    }
    match reader.read().unwrap().unwrap() {
        Object::Structured(value) => {
            assert!(value.fields.iter().any(|(name, value)| {
                name == "btrExtArb" && value == &blf_core::FieldValue::U32(0x1122_3344)
            }));
            assert!(value.fields.iter().any(|(name, value)| {
                name == "btrExtData" && value == &blf_core::FieldValue::U32(0x5566_7788)
            }));
        }
        _ => panic!("expected CAN setting changed event"),
    }
    assert!(reader.read().unwrap().is_none());
}

#[test]
fn skips_nonzero_object_alignment_padding() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nonzero-object-padding.blf");

    let mut app_text = object_header(65, 49, 1);
    push_u32(&mut app_text, 0x1234);
    push_u32(&mut app_text, 0);
    push_u32(&mut app_text, 1);
    push_u32(&mut app_text, 0);
    app_text.push(b"x"[0]);
    assert_eq!(app_text.len(), 49);

    let mut inner = app_text;
    inner.extend_from_slice(&[0xA5, 0xA6, 0xA7]);
    inner.extend_from_slice(&object_header(3, 40, 2));
    inner.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
    write_blf_with_inner(&path, &inner, 2);

    let mut reader = File::open(&path).unwrap();
    assert_eq!(
        reader.read().unwrap().unwrap().object_type(),
        ObjectType::AppText
    );
    assert_eq!(
        reader.read().unwrap().unwrap().object_type(),
        ObjectType::CanOverload
    );
    assert!(reader.read().unwrap().is_none());
}

#[test]
fn rejects_dynamic_payload_length_outside_object() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dynamic-payload-overflow.blf");

    let mut inner = object_header(6, 48, 1);
    push_u32(&mut inner, u32::MAX);
    push_u32(&mut inner, 0);
    push_u64(&mut inner, 0);
    assert_eq!(inner.len(), 48);
    write_blf_with_inner(&path, &inner, 1);

    let mut reader = File::open(&path).unwrap();
    assert!(matches!(reader.read(), Err(blf_core::BlfError::Truncated)));
}

#[test]
fn rejects_huge_serial_timestamp_length_without_allocating() {
    let dir = std::env::temp_dir().join("blf-core-tests");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("serial-timestamp-overflow.blf");

    let mut inner = object_header(90, 48, 1);
    push_u32(&mut inner, 0);
    push_u32(&mut inner, 0);
    push_u32(&mut inner, 9_600);
    push_u32(&mut inner, 0);
    push_u32(&mut inner, 0);
    push_u32(&mut inner, u32::MAX);
    assert_eq!(inner.len(), 56);
    let object_size = inner.len() as u32;
    inner[8..12].copy_from_slice(&object_size.to_le_bytes());
    write_blf_with_inner(&path, &inner, 1);

    let mut reader = File::open(&path).unwrap();
    assert!(matches!(reader.read(), Err(blf_core::BlfError::Truncated)));
}
