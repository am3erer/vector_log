use std::fs;

use blf_core::{File, Object, ObjectType};

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
    for _ in 0..object_size % 4 {
        container.push(0);
    }
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
