use std::fs::File as StdFile;
use std::io::Read;
use std::path::Path;

use flate2::read::ZlibDecoder;
use memmap2::MmapOptions;

use crate::error::{BlfError, Result};
use crate::types::{
    read_system_time, read_u16, read_u32, read_u64, AppText, CanErrorFrame, CanErrorFrameExt,
    CanFdErrorFrame64, CanFdExtFrameData, CanFdMessage, CanFdMessage64, CanMessage, CanMessage2,
    FieldValue, FileStatistics, LinObject, Object, ObjectHeader, ObjectHeader2, ObjectHeaderBase,
    ObjectType, RestorePointContainer, VarObjectHeader, FILE_SIGNATURE, OBJECT_SIGNATURE,
};

const LOBJ_BYTES: [u8; 4] = [0x4C, 0x4F, 0x42, 0x4A];

pub fn parse_file_statistics(data: &[u8]) -> Result<FileStatistics> {
    if data.len() < 144 {
        return Err(BlfError::Truncated);
    }

    let signature = read_u32(data, 0)?;
    if signature != FILE_SIGNATURE {
        return Err(BlfError::Format(format!(
            "invalid file signature 0x{signature:08X}"
        )));
    }

    let mut reserved_file_statistics = [0u32; 16];
    for (index, value) in reserved_file_statistics.iter_mut().enumerate() {
        *value = read_u32(data, 80 + index * 4)?;
    }

    Ok(FileStatistics {
        signature,
        statistics_size: read_u32(data, 4)?,
        api_number: read_u32(data, 8)?,
        application_id: *data.get(12).ok_or(BlfError::Truncated)?,
        compression_level: *data.get(13).ok_or(BlfError::Truncated)?,
        application_major: *data.get(14).ok_or(BlfError::Truncated)?,
        application_minor: *data.get(15).ok_or(BlfError::Truncated)?,
        file_size: read_u64(data, 16)?,
        uncompressed_file_size: read_u64(data, 24)?,
        object_count: read_u32(data, 32)?,
        application_build: read_u32(data, 36)?,
        measurement_start_time: read_system_time(data, 40)?,
        last_object_time: read_system_time(data, 56)?,
        restore_points_offset: read_u64(data, 72)?,
        reserved_file_statistics,
    })
}

pub struct File {
    mmap: memmap2::Mmap,
    outer_pos: usize,
    decompressed: Vec<u8>,
    decompressed_pos: usize,
    pub file_statistics: FileStatistics,
    pub current_uncompressed_file_size: u64,
    pub current_object_count: u32,
    pub compression_level: i32,
    pub write_restore_points: bool,
    pub strict: bool,
    open: bool,
    eof: bool,
}

impl File {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(path, false)
    }

    pub fn open_with_options(path: impl AsRef<Path>, strict: bool) -> Result<Self> {
        let std_file = StdFile::open(path)?;
        let mmap = unsafe { MmapOptions::new().map(&std_file)? };
        if mmap.len() < 144 {
            return Err(BlfError::Truncated);
        }

        let file_statistics = parse_file_statistics(&mmap[..144])?;
        let outer_pos = file_statistics.statistics_size as usize;

        Ok(Self {
            mmap,
            outer_pos,
            decompressed: Vec::new(),
            decompressed_pos: 0,
            current_uncompressed_file_size: file_statistics.statistics_size as u64,
            current_object_count: 0,
            compression_level: 1,
            write_restore_points: true,
            file_statistics,
            strict,
            open: true,
            eof: false,
        })
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn eof(&self) -> bool {
        self.eof
    }

    pub fn close(&mut self) {
        self.open = false;
        self.eof = true;
    }

    pub fn read(&mut self) -> Result<Option<Object>> {
        loop {
            match self.parse_next_from_decompressed() {
                Ok(Some(object)) => {
                    if object.object_type() != ObjectType::Unknown115 {
                        self.current_object_count += 1;
                    }
                    return Ok(Some(object));
                }
                Ok(None) => {
                    if self.eof {
                        return Ok(None);
                    }
                }
                Err(BlfError::Incomplete) => {
                    if !self.load_next_log_container() {
                        self.eof = true;
                        return Ok(None);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn read_batch(&mut self, max_objects: usize) -> Result<Vec<Object>> {
        let mut objects = Vec::with_capacity(max_objects.min(4096));
        while objects.len() < max_objects {
            match self.read()? {
                Some(object) => objects.push(object),
                None => break,
            }
        }
        Ok(objects)
    }

    fn parse_next_from_decompressed(&mut self) -> Result<Option<Object>> {
        if self.decompressed_pos >= self.decompressed.len() {
            return Err(BlfError::Incomplete);
        }

        let Some(signature_pos) = find_lobj(&self.decompressed, self.decompressed_pos) else {
            return Err(BlfError::Incomplete);
        };

        let base = parse_object_header_base(&self.decompressed, signature_pos)?;
        let object_size = base.object_size as usize;
        let object_end = signature_pos
            .checked_add(object_size)
            .ok_or(BlfError::Format("object size overflow".to_string()))?;

        if object_end > self.decompressed.len() {
            return Err(BlfError::Incomplete);
        }

        let slice = &self.decompressed[signature_pos..object_end];
        let object = if is_lin_type(base.object_type) {
            Some(Object::Lin(parse_lin_object(slice, base)?))
        } else {
            match base.object_type {
                ObjectType::CanMessage => Some(Object::CanMessage(parse_can_message(slice, base)?)),
                ObjectType::CanMessage2 => {
                    Some(Object::CanMessage2(parse_can_message2(slice, base)?))
                }
                ObjectType::CanFdMessage => {
                    Some(Object::CanFdMessage(parse_can_fd_message(slice, base)?))
                }
                ObjectType::CanFdMessage64 => {
                    Some(Object::CanFdMessage64(parse_can_fd_message64(slice, base)?))
                }
                ObjectType::CanError => {
                    Some(Object::CanErrorFrame(parse_can_error_frame(slice, base)?))
                }
                ObjectType::CanErrorExt => Some(Object::CanErrorFrameExt(
                    parse_can_error_frame_ext(slice, base)?,
                )),
                ObjectType::CanFdError64 => Some(Object::CanFdErrorFrame64(
                    parse_can_fd_error_frame64(slice, base)?,
                )),
                ObjectType::AppText => Some(Object::AppText(parse_app_text(slice, base)?)),
                ObjectType::Unknown115 => Some(Object::RestorePointContainer(
                    parse_restore_point_container(slice, base)?,
                )),
                _ => {
                    if self.strict {
                        return Err(BlfError::UnsupportedObject(base.object_type.as_u32()));
                    }
                    None
                }
            }
        };

        self.decompressed_pos = object_end;
        Ok(object)
    }

    fn load_next_log_container(&mut self) -> bool {
        let Some(signature_pos) = find_lobj(&self.mmap, self.outer_pos) else {
            return false;
        };

        let base = match parse_object_header_base(&self.mmap, signature_pos) {
            Ok(base) => base,
            Err(_) => return false,
        };

        if base.object_type != ObjectType::LogContainer {
            return false;
        }

        let Ok(object_size) = usize::try_from(base.object_size) else {
            return false;
        };
        if object_size < 32 {
            return false;
        }

        let Some(header_end) = signature_pos.checked_add(object_size) else {
            return false;
        };
        if header_end > self.mmap.len() {
            return false;
        }

        let Ok(compression_method) = read_u16(&self.mmap, signature_pos + 16) else {
            return false;
        };
        let Ok(uncompressed_size) = read_u32(&self.mmap, signature_pos + 24) else {
            return false;
        };

        let compressed_size = object_size - 32;
        let compressed = &self.mmap[signature_pos + 32..signature_pos + 32 + compressed_size];

        let mut decompressed = match compression_method {
            0 => compressed.to_vec(),
            2 => {
                let mut output = Vec::with_capacity(uncompressed_size as usize);
                let decoder = ZlibDecoder::new(compressed);
                if decoder
                    .take((uncompressed_size as u64).saturating_add(1))
                    .read_to_end(&mut output)
                    .is_err()
                {
                    return false;
                }
                output
            }
            _ => return false,
        };

        if decompressed.len() != uncompressed_size as usize {
            return false;
        }

        let padding = object_size % 4;
        self.outer_pos = header_end + padding;
        self.current_uncompressed_file_size += (32 + uncompressed_size as u64).saturating_add(0);
        self.decompressed.append(&mut decompressed);
        true
    }
}

fn find_lobj(data: &[u8], start: usize) -> Option<usize> {
    data.get(start..)?
        .windows(4)
        .position(|window| window == LOBJ_BYTES)
        .map(|offset| start + offset)
}

fn parse_object_header_base(data: &[u8], offset: usize) -> Result<ObjectHeaderBase> {
    let signature = read_u32(data, offset)?;
    if signature != OBJECT_SIGNATURE {
        return Err(BlfError::Format(format!(
            "invalid object signature 0x{signature:08X}"
        )));
    }
    Ok(ObjectHeaderBase {
        signature,
        header_size: read_u16(data, offset + 4)?,
        header_version: read_u16(data, offset + 6)?,
        object_size: read_u32(data, offset + 8)?,
        object_type: ObjectType::from_raw(read_u32(data, offset + 12)?),
    })
}

fn parse_object_header(data: &[u8], offset: usize) -> Result<ObjectHeader> {
    let base = parse_object_header_base(data, offset)?;
    Ok(ObjectHeader {
        base,
        object_flags: read_u32(data, offset + 16)?,
        client_index: read_u16(data, offset + 20)?,
        object_version: read_u16(data, offset + 22)?,
        object_time_stamp: read_u64(data, offset + 24)?,
    })
}

#[allow(dead_code)]
fn parse_object_header2(data: &[u8], offset: usize) -> Result<ObjectHeader2> {
    let base = parse_object_header_base(data, offset)?;
    Ok(ObjectHeader2 {
        base,
        object_flags: read_u32(data, offset + 16)?,
        time_stamp_status: *data.get(offset + 20).ok_or(BlfError::Truncated)?,
        reserved_object_header: *data.get(offset + 21).ok_or(BlfError::Truncated)?,
        object_version: read_u16(data, offset + 22)?,
        object_time_stamp: read_u64(data, offset + 24)?,
        original_time_stamp: read_u64(data, offset + 32)?,
    })
}

#[allow(dead_code)]
fn parse_var_object_header(data: &[u8], offset: usize) -> Result<VarObjectHeader> {
    let base = parse_object_header_base(data, offset)?;
    Ok(VarObjectHeader {
        base,
        object_flags: read_u32(data, offset + 16)?,
        object_static_size: read_u16(data, offset + 20)?,
        object_version: read_u16(data, offset + 22)?,
        object_time_stamp: read_u64(data, offset + 24)?,
    })
}

fn parse_can_message(data: &[u8], _base: ObjectHeaderBase) -> Result<CanMessage> {
    let header = parse_object_header(data, 0)?;
    if header.base.header_version != 1 {
        return Err(BlfError::UnsupportedHeaderVersion(
            header.base.header_version,
        ));
    }
    let mut payload = [0u8; 8];
    payload.copy_from_slice(data.get(40..48).ok_or(BlfError::Truncated)?);
    Ok(CanMessage {
        header,
        channel: read_u16(data, 32)?,
        flags: *data.get(34).ok_or(BlfError::Truncated)?,
        dlc: *data.get(35).ok_or(BlfError::Truncated)?,
        id: read_u32(data, 36)?,
        data: payload,
    })
}

fn parse_can_message2(data: &[u8], base: ObjectHeaderBase) -> Result<CanMessage2> {
    let header = parse_object_header(data, 0)?;
    let fixed_size = 48usize;
    let data_len =
        base.object_size
            .checked_sub(fixed_size as u32)
            .ok_or(BlfError::Format("CanMessage2 size underflow".to_string()))? as usize;
    let data_start = 40usize;
    let payload = data
        .get(data_start..data_start + data_len)
        .ok_or(BlfError::Truncated)?
        .to_vec();
    let tail = data_start + data_len;
    Ok(CanMessage2 {
        header,
        channel: read_u16(data, 32)?,
        flags: *data.get(34).ok_or(BlfError::Truncated)?,
        dlc: *data.get(35).ok_or(BlfError::Truncated)?,
        id: read_u32(data, 36)?,
        data: payload,
        frame_length: read_u32(data, tail)?,
        bit_count: *data.get(tail + 4).ok_or(BlfError::Truncated)?,
        reserved_can_message1: *data.get(tail + 5).ok_or(BlfError::Truncated)?,
        reserved_can_message2: read_u16(data, tail + 6)?,
    })
}

fn parse_can_fd_message(data: &[u8], _base: ObjectHeaderBase) -> Result<CanFdMessage> {
    let header = parse_object_header(data, 0)?;
    let mut payload = [0u8; 64];
    payload.copy_from_slice(data.get(52..116).ok_or(BlfError::Truncated)?);
    Ok(CanFdMessage {
        header,
        channel: read_u16(data, 32)?,
        flags: *data.get(34).ok_or(BlfError::Truncated)?,
        dlc: *data.get(35).ok_or(BlfError::Truncated)?,
        id: read_u32(data, 36)?,
        frame_length: read_u32(data, 40)?,
        arb_bit_count: *data.get(44).ok_or(BlfError::Truncated)?,
        can_fd_flags: *data.get(45).ok_or(BlfError::Truncated)?,
        valid_data_bytes: *data.get(46).ok_or(BlfError::Truncated)?,
        reserved_can_fd_message1: *data.get(47).ok_or(BlfError::Truncated)?,
        reserved_can_fd_message2: read_u32(data, 48)?,
        data: payload,
        reserved_can_fd_message3: read_u32(data, 116)?,
    })
}

fn parse_can_fd_message64(data: &[u8], base: ObjectHeaderBase) -> Result<CanFdMessage64> {
    let header = parse_object_header(data, 0)?;
    let valid_data_bytes = *data.get(34).ok_or(BlfError::Truncated)? as usize;
    let fixed_before_data = 40usize;
    let data_start = 32 + fixed_before_data;
    let payload = data
        .get(data_start..data_start + valid_data_bytes)
        .ok_or(BlfError::Truncated)?
        .to_vec();

    let ext_data_offset = *data.get(67).ok_or(BlfError::Truncated)? as usize;
    let mut ext_data = None;
    if ext_data_offset != 0 && base.object_size as usize >= ext_data_offset + 8 {
        let ext_start = 32 + fixed_before_data + valid_data_bytes;
        let btr_ext_arb = read_u32(data, ext_start)?;
        let btr_ext_data = read_u32(data, ext_start + 4)?;
        let reserved_start = ext_start + 8;
        let reserved_end = base.object_size as usize;
        let reserved = data
            .get(reserved_start..reserved_end)
            .ok_or(BlfError::Truncated)?
            .to_vec();
        ext_data = Some(CanFdExtFrameData {
            btr_ext_arb,
            btr_ext_data,
            reserved_can_fd_ext_frame_data: reserved,
        });
    }

    Ok(CanFdMessage64 {
        header,
        channel: *data.get(32).ok_or(BlfError::Truncated)?,
        dlc: *data.get(33).ok_or(BlfError::Truncated)?,
        valid_data_bytes: valid_data_bytes as u8,
        tx_count: *data.get(35).ok_or(BlfError::Truncated)?,
        id: read_u32(data, 36)?,
        frame_length: read_u32(data, 40)?,
        flags: read_u32(data, 44)?,
        btr_cfg_arb: read_u32(data, 48)?,
        btr_cfg_data: read_u32(data, 52)?,
        time_offset_brs_ns: read_u32(data, 56)?,
        time_offset_crc_del_ns: read_u32(data, 60)?,
        bit_count: read_u16(data, 64)?,
        dir: *data.get(66).ok_or(BlfError::Truncated)?,
        ext_data_offset: ext_data_offset as u8,
        crc: read_u32(data, 68)?,
        data: payload,
        ext_data,
    })
}

fn parse_can_error_frame(data: &[u8], _base: ObjectHeaderBase) -> Result<CanErrorFrame> {
    let header = parse_object_header(data, 0)?;
    let length = read_u16(data, 34)?;
    let reserved = if length > 0 {
        Some(read_u32(data, 36)?)
    } else {
        None
    };
    Ok(CanErrorFrame {
        header,
        channel: read_u16(data, 32)?,
        length,
        reserved_can_error_frame: reserved,
    })
}

fn parse_can_error_frame_ext(data: &[u8], base: ObjectHeaderBase) -> Result<CanErrorFrameExt> {
    let header = parse_object_header(data, 0)?;
    let data_start = 56usize;
    let data_len = base
        .object_size
        .checked_sub(data_start as u32)
        .ok_or(BlfError::Format(
            "CanErrorFrameExt size underflow".to_string(),
        ))? as usize;
    let payload = data
        .get(data_start..data_start + data_len)
        .ok_or(BlfError::Truncated)?
        .to_vec();
    Ok(CanErrorFrameExt {
        header,
        channel: read_u16(data, 32)?,
        length: read_u16(data, 34)?,
        flags: read_u32(data, 36)?,
        ecc: *data.get(40).ok_or(BlfError::Truncated)?,
        position: *data.get(41).ok_or(BlfError::Truncated)?,
        dlc: *data.get(42).ok_or(BlfError::Truncated)?,
        reserved_can_error_frame_ext1: *data.get(43).ok_or(BlfError::Truncated)?,
        frame_length_in_ns: read_u32(data, 44)?,
        id: read_u32(data, 48)?,
        flags_ext: read_u16(data, 52)?,
        reserved_can_error_frame_ext2: read_u16(data, 54)?,
        data: payload,
    })
}

fn parse_can_fd_error_frame64(data: &[u8], base: ObjectHeaderBase) -> Result<CanFdErrorFrame64> {
    let header = parse_object_header(data, 0)?;
    let valid_data_bytes = *data.get(34).ok_or(BlfError::Truncated)? as usize;
    let fixed_before_data = 44usize;
    let data_start = 32 + fixed_before_data;
    let payload = data
        .get(data_start..data_start + valid_data_bytes)
        .ok_or(BlfError::Truncated)?
        .to_vec();

    let ext_data_offset = *data.get(42).ok_or(BlfError::Truncated)? as usize;
    let mut ext_data = None;
    if ext_data_offset != 0 && base.object_size as usize >= ext_data_offset + 8 {
        let ext_start = 32 + fixed_before_data + valid_data_bytes;
        let btr_ext_arb = read_u32(data, ext_start)?;
        let btr_ext_data = read_u32(data, ext_start + 4)?;
        let reserved = data
            .get(ext_start + 8..base.object_size as usize)
            .ok_or(BlfError::Truncated)?
            .to_vec();
        ext_data = Some(CanFdExtFrameData {
            btr_ext_arb,
            btr_ext_data,
            reserved_can_fd_ext_frame_data: reserved,
        });
    }

    Ok(CanFdErrorFrame64 {
        header,
        channel: *data.get(32).ok_or(BlfError::Truncated)?,
        dlc: *data.get(33).ok_or(BlfError::Truncated)?,
        valid_data_bytes: valid_data_bytes as u8,
        ecc: *data.get(35).ok_or(BlfError::Truncated)?,
        flags: read_u16(data, 36)?,
        error_code_ext: read_u16(data, 38)?,
        ext_flags: read_u16(data, 40)?,
        ext_data_offset: ext_data_offset as u8,
        reserved_can_fd_error_frame1: *data.get(43).ok_or(BlfError::Truncated)?,
        id: read_u32(data, 44)?,
        frame_length: read_u32(data, 48)?,
        btr_cfg_arb: read_u32(data, 52)?,
        btr_cfg_data: read_u32(data, 56)?,
        time_offset_brs_ns: read_u32(data, 60)?,
        time_offset_crc_del_ns: read_u32(data, 64)?,
        crc: read_u32(data, 68)?,
        error_position: read_u16(data, 72)?,
        reserved_can_fd_error_frame2: read_u16(data, 74)?,
        data: payload,
        ext_data,
    })
}

fn parse_app_text(data: &[u8], _base: ObjectHeaderBase) -> Result<AppText> {
    let header = parse_object_header(data, 0)?;
    let text_length = read_u32(data, 40)? as usize;
    let text_start = 48usize;
    let text = data
        .get(text_start..text_start + text_length)
        .ok_or(BlfError::Truncated)?
        .to_vec();
    Ok(AppText {
        header,
        source: read_u32(data, 32)?,
        reserved_app_text1: read_u32(data, 36)?,
        text_length: text_length as u32,
        reserved_app_text2: read_u32(data, 44)?,
        text,
    })
}

fn parse_restore_point_container(
    data: &[u8],
    _base: ObjectHeaderBase,
) -> Result<RestorePointContainer> {
    let header = parse_object_header(data, 0)?;
    let mut reserved = [0u8; 14];
    reserved.copy_from_slice(data.get(32..46).ok_or(BlfError::Truncated)?);
    let data_length = read_u16(data, 46)?;
    let payload = data
        .get(48..48 + data_length as usize)
        .ok_or(BlfError::Truncated)?
        .to_vec();
    Ok(RestorePointContainer {
        header,
        reserved_restore_point_container: reserved,
        data_length,
        data: payload,
    })
}

fn is_lin_type(object_type: ObjectType) -> bool {
    matches!(
        object_type,
        ObjectType::LinMessage
            | ObjectType::LinCrcError
            | ObjectType::LinDlcInfo
            | ObjectType::LinRcvError
            | ObjectType::LinSndError
            | ObjectType::LinSlvTimeout
            | ObjectType::LinSchedModch
            | ObjectType::LinSynError
            | ObjectType::LinBaudrate
            | ObjectType::LinSleep
            | ObjectType::LinWakeup
            | ObjectType::LinChecksumInfo
            | ObjectType::LinSpikeEvent
            | ObjectType::LinStatistic
            | ObjectType::LinMessage2
            | ObjectType::LinSndError2
            | ObjectType::LinSynError2
            | ObjectType::LinCrcError2
            | ObjectType::LinRcvError2
            | ObjectType::LinWakeup2
            | ObjectType::LinSpikeEvent2
            | ObjectType::LinLongDomSig
            | ObjectType::LinLongDomSig2
            | ObjectType::LinUnexpectedWakeup
            | ObjectType::LinShortOrSlowResponse
            | ObjectType::LinDisturbanceEvent
            | ObjectType::LinShortOrSlowResponse2
    )
}

fn push_u8_field(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    fields.push((
        name.to_string(),
        FieldValue::U8(*data.get(*offset).ok_or(BlfError::Truncated)?),
    ));
    *offset += 1;
    Ok(())
}

fn push_u16_field(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    fields.push((name.to_string(), FieldValue::U16(read_u16(data, *offset)?)));
    *offset += 2;
    Ok(())
}

fn push_u32_field(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    fields.push((name.to_string(), FieldValue::U32(read_u32(data, *offset)?)));
    *offset += 4;
    Ok(())
}

fn push_u64_field(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    fields.push((name.to_string(), FieldValue::U64(read_u64(data, *offset)?)));
    *offset += 8;
    Ok(())
}

fn push_i32_field(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    fields.push((
        name.to_string(),
        FieldValue::I32(read_u32(data, *offset)? as i32),
    ));
    *offset += 4;
    Ok(())
}

fn push_f64_field(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    let value = data.get(*offset..*offset + 8).ok_or(BlfError::Truncated)?;
    fields.push((
        name.to_string(),
        FieldValue::F64(f64::from_le_bytes(value.try_into().expect("fixed 8 bytes"))),
    ));
    *offset += 8;
    Ok(())
}

fn push_bytes_field(
    data: &[u8],
    offset: &mut usize,
    len: usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    fields.push((
        name.to_string(),
        FieldValue::Bytes(
            data.get(*offset..*offset + len)
                .ok_or(BlfError::Truncated)?
                .to_vec(),
        ),
    ));
    *offset += len;
    Ok(())
}

fn push_u16_vec_field(
    data: &[u8],
    offset: &mut usize,
    len: usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    let mut values = Vec::with_capacity(len);
    for index in 0..len {
        values.push(read_u16(data, *offset + index * 2)?);
    }
    fields.push((name.to_string(), FieldValue::U16Vec(values)));
    *offset += len * 2;
    Ok(())
}

fn push_u64_vec_field(
    data: &[u8],
    offset: &mut usize,
    len: usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    let mut values = Vec::with_capacity(len);
    for index in 0..len {
        values.push(read_u64(data, *offset + index * 8)?);
    }
    fields.push((name.to_string(), FieldValue::U64Vec(values)));
    *offset += len * 8;
    Ok(())
}

fn read_lin_bus_event(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
) -> Result<()> {
    push_u64_field(data, offset, fields, "sof")?;
    push_u32_field(data, offset, fields, "eventBaudrate")?;
    push_u16_field(data, offset, fields, "channel")?;
    push_u16_field(data, offset, fields, "reservedLinBusEvent")
}

fn read_lin_synch_field_event(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
) -> Result<()> {
    read_lin_bus_event(data, offset, fields)?;
    push_u64_field(data, offset, fields, "synchBreakLength")?;
    push_u64_field(data, offset, fields, "synchDelLength")
}

fn read_lin_message_descriptor(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
) -> Result<()> {
    read_lin_synch_field_event(data, offset, fields)?;
    push_u16_field(data, offset, fields, "supplierId")?;
    push_u16_field(data, offset, fields, "messageId")?;
    push_u8_field(data, offset, fields, "nad")?;
    push_u8_field(data, offset, fields, "id")?;
    push_u8_field(data, offset, fields, "dlc")?;
    push_u8_field(data, offset, fields, "checksumModel")
}

fn read_lin_databyte_timestamp_event(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
) -> Result<()> {
    read_lin_message_descriptor(data, offset, fields)?;
    push_u64_vec_field(data, offset, 9, fields, "databyteTimestamps")
}

fn parse_lin_object(data: &[u8], base: ObjectHeaderBase) -> Result<LinObject> {
    let header = parse_object_header(data, 0)?;
    let mut fields = Vec::new();
    let mut offset = 32usize;

    match base.object_type {
        ObjectType::LinMessage => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u8_field(data, &mut offset, &mut fields, "id")?;
            push_u8_field(data, &mut offset, &mut fields, "dlc")?;
            push_bytes_field(data, &mut offset, 8, &mut fields, "data")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmId")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmState")?;
            push_u8_field(data, &mut offset, &mut fields, "headerTime")?;
            push_u8_field(data, &mut offset, &mut fields, "fullTime")?;
            push_u16_field(data, &mut offset, &mut fields, "crc")?;
            push_u8_field(data, &mut offset, &mut fields, "dir")?;
            push_u8_field(data, &mut offset, &mut fields, "reservedLinMessage1")?;
            if offset + 4 <= data.len() {
                push_u32_field(data, &mut offset, &mut fields, "reservedLinMessage2")?;
            }
        }
        ObjectType::LinCrcError => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u8_field(data, &mut offset, &mut fields, "id")?;
            push_u8_field(data, &mut offset, &mut fields, "dlc")?;
            push_bytes_field(data, &mut offset, 8, &mut fields, "data")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmId")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmState")?;
            push_u8_field(data, &mut offset, &mut fields, "headerTime")?;
            push_u8_field(data, &mut offset, &mut fields, "fullTime")?;
            push_u16_field(data, &mut offset, &mut fields, "crc")?;
            push_u8_field(data, &mut offset, &mut fields, "dir")?;
            push_u8_field(data, &mut offset, &mut fields, "reservedLinCrcError1")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinCrcError2")?;
        }
        ObjectType::LinDlcInfo => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u8_field(data, &mut offset, &mut fields, "id")?;
            push_u8_field(data, &mut offset, &mut fields, "dlc")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinDlcInfo")?;
        }
        ObjectType::LinRcvError => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u8_field(data, &mut offset, &mut fields, "id")?;
            push_u8_field(data, &mut offset, &mut fields, "dlc")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmId")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmState")?;
            push_u8_field(data, &mut offset, &mut fields, "headerTime")?;
            push_u8_field(data, &mut offset, &mut fields, "fullTime")?;
            push_u8_field(data, &mut offset, &mut fields, "stateReason")?;
            push_u8_field(data, &mut offset, &mut fields, "offendingByte")?;
            push_u8_field(data, &mut offset, &mut fields, "shortError")?;
            push_u8_field(data, &mut offset, &mut fields, "timeoutDuringDlcDetection")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinReceiveError")?;
        }
        ObjectType::LinSndError => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u8_field(data, &mut offset, &mut fields, "id")?;
            push_u8_field(data, &mut offset, &mut fields, "dlc")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmId")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmState")?;
            push_u8_field(data, &mut offset, &mut fields, "headerTime")?;
            push_u8_field(data, &mut offset, &mut fields, "fullTime")?;
        }
        ObjectType::LinSlvTimeout => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u8_field(data, &mut offset, &mut fields, "slaveId")?;
            push_u8_field(data, &mut offset, &mut fields, "stateId")?;
            push_u32_field(data, &mut offset, &mut fields, "followStateId")?;
        }
        ObjectType::LinSchedModch => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u8_field(data, &mut offset, &mut fields, "oldMode")?;
            push_u8_field(data, &mut offset, &mut fields, "newMode")?;
            push_u32_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinSchedulerModeChange",
            )?;
        }
        ObjectType::LinSynError => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u16_field(data, &mut offset, &mut fields, "reservedLinSyncError1")?;
            push_u16_vec_field(data, &mut offset, 4, &mut fields, "timeDiff")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinSyncError2")?;
        }
        ObjectType::LinBaudrate => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u16_field(data, &mut offset, &mut fields, "reservedLinBaudrateEvent")?;
            push_i32_field(data, &mut offset, &mut fields, "baudrate")?;
        }
        ObjectType::LinSleep => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u8_field(data, &mut offset, &mut fields, "reason")?;
            push_u8_field(data, &mut offset, &mut fields, "flags")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinSleepModeEvent")?;
        }
        ObjectType::LinWakeup => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u8_field(data, &mut offset, &mut fields, "signal")?;
            push_u8_field(data, &mut offset, &mut fields, "external")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinWakeupEvent")?;
        }
        ObjectType::LinChecksumInfo => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u8_field(data, &mut offset, &mut fields, "id")?;
            push_u8_field(data, &mut offset, &mut fields, "checksumModel")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinChecksumInfo")?;
        }
        ObjectType::LinSpikeEvent => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u16_field(data, &mut offset, &mut fields, "reservedLinSpikeEvent")?;
            push_u32_field(data, &mut offset, &mut fields, "width")?;
        }
        ObjectType::LinStatistic => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u16_field(data, &mut offset, &mut fields, "reservedLinStatisticEvent1")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinStatisticEvent2")?;
            push_f64_field(data, &mut offset, &mut fields, "busLoad")?;
            push_u32_field(data, &mut offset, &mut fields, "burstsTotal")?;
            push_u32_field(data, &mut offset, &mut fields, "burstsOverrun")?;
            push_u32_field(data, &mut offset, &mut fields, "framesSent")?;
            push_u32_field(data, &mut offset, &mut fields, "framesReceived")?;
            push_u32_field(data, &mut offset, &mut fields, "framesUnanswered")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinStatisticEvent3")?;
        }
        ObjectType::LinDisturbanceEvent => {
            push_u16_field(data, &mut offset, &mut fields, "channel")?;
            push_u8_field(data, &mut offset, &mut fields, "id")?;
            push_u8_field(data, &mut offset, &mut fields, "disturbingFrameId")?;
            push_u32_field(data, &mut offset, &mut fields, "disturbanceType")?;
            push_u32_field(data, &mut offset, &mut fields, "byteIndex")?;
            push_u32_field(data, &mut offset, &mut fields, "bitIndex")?;
            push_u32_field(data, &mut offset, &mut fields, "bitOffsetInSixteenthBits")?;
            push_u32_field(
                data,
                &mut offset,
                &mut fields,
                "disturbanceLengthInSixteenthBits",
            )?;
        }
        ObjectType::LinMessage2 => {
            read_lin_databyte_timestamp_event(data, &mut offset, &mut fields)?;
            push_bytes_field(data, &mut offset, 8, &mut fields, "data")?;
            push_u16_field(data, &mut offset, &mut fields, "crc")?;
            push_u8_field(data, &mut offset, &mut fields, "dir")?;
            push_u8_field(data, &mut offset, &mut fields, "simulated")?;
            push_u8_field(data, &mut offset, &mut fields, "isEtf")?;
            push_u8_field(data, &mut offset, &mut fields, "etfAssocIndex")?;
            push_u8_field(data, &mut offset, &mut fields, "etfAssocEtfId")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmId")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmState")?;
            push_u8_field(data, &mut offset, &mut fields, "reservedLinMessage1")?;
            push_u16_field(data, &mut offset, &mut fields, "reservedLinMessage2")?;
            if offset < data.len() {
                push_u32_field(data, &mut offset, &mut fields, "respBaudrate")?;
            }
            if offset + 16 <= data.len() {
                push_f64_field(data, &mut offset, &mut fields, "exactHeaderBaudrate")?;
                push_u32_field(data, &mut offset, &mut fields, "earlyStopbitOffset")?;
                push_u32_field(data, &mut offset, &mut fields, "earlyStopbitOffsetResponse")?;
            }
        }
        ObjectType::LinSndError2 => {
            read_lin_message_descriptor(data, &mut offset, &mut fields)?;
            push_u64_field(data, &mut offset, &mut fields, "eoh")?;
            push_u8_field(data, &mut offset, &mut fields, "isEtf")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmId")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmState")?;
            push_u8_field(data, &mut offset, &mut fields, "reservedLinSendError1")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinSendError2")?;
            push_f64_field(data, &mut offset, &mut fields, "exactHeaderBaudrate")?;
            push_u32_field(data, &mut offset, &mut fields, "earlyStopbitOffset")?;
            if offset + 4 <= data.len() {
                push_u32_field(data, &mut offset, &mut fields, "reservedLinSendError3")?;
            }
        }
        ObjectType::LinSynError2 => {
            read_lin_synch_field_event(data, &mut offset, &mut fields)?;
            push_u16_vec_field(data, &mut offset, 4, &mut fields, "timeDiff")?;
        }
        ObjectType::LinCrcError2 => {
            read_lin_databyte_timestamp_event(data, &mut offset, &mut fields)?;
            push_bytes_field(data, &mut offset, 8, &mut fields, "data")?;
            push_u16_field(data, &mut offset, &mut fields, "crc")?;
            push_u8_field(data, &mut offset, &mut fields, "dir")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmId")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmState")?;
            push_u8_field(data, &mut offset, &mut fields, "simulated")?;
            push_u16_field(data, &mut offset, &mut fields, "reservedLinCrcError1")?;
            push_u32_field(data, &mut offset, &mut fields, "respBaudrate")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinCrcError2")?;
            push_f64_field(data, &mut offset, &mut fields, "exactHeaderBaudrate")?;
            push_u32_field(data, &mut offset, &mut fields, "earlyStopbitOffset")?;
            push_u32_field(data, &mut offset, &mut fields, "earlyStopbitOffsetResponse")?;
        }
        ObjectType::LinRcvError2 => {
            read_lin_databyte_timestamp_event(data, &mut offset, &mut fields)?;
            push_bytes_field(data, &mut offset, 8, &mut fields, "data")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmId")?;
            push_u8_field(data, &mut offset, &mut fields, "fsmState")?;
            push_u8_field(data, &mut offset, &mut fields, "stateReason")?;
            push_u8_field(data, &mut offset, &mut fields, "offendingByte")?;
            push_u8_field(data, &mut offset, &mut fields, "shortError")?;
            push_u8_field(data, &mut offset, &mut fields, "timeoutDuringDlcDetection")?;
            push_u8_field(data, &mut offset, &mut fields, "isEtf")?;
            push_u8_field(data, &mut offset, &mut fields, "hasDatabytes")?;
            push_u32_field(data, &mut offset, &mut fields, "respBaudrate")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinReceiveError")?;
            push_f64_field(data, &mut offset, &mut fields, "exactHeaderBaudrate")?;
            push_u32_field(data, &mut offset, &mut fields, "earlyStopbitOffset")?;
            push_u32_field(data, &mut offset, &mut fields, "earlyStopbitOffsetResponse")?;
        }
        ObjectType::LinWakeup2 => {
            read_lin_bus_event(data, &mut offset, &mut fields)?;
            push_u8_field(data, &mut offset, &mut fields, "lengthInfo")?;
            push_u8_field(data, &mut offset, &mut fields, "signal")?;
            push_u8_field(data, &mut offset, &mut fields, "external")?;
            push_u8_field(data, &mut offset, &mut fields, "reservedLinWakeupEvent1")?;
            push_u32_field(data, &mut offset, &mut fields, "reservedLinWakeupEvent2")?;
        }
        ObjectType::LinSpikeEvent2 => {
            read_lin_bus_event(data, &mut offset, &mut fields)?;
            push_u32_field(data, &mut offset, &mut fields, "width")?;
            push_u8_field(data, &mut offset, &mut fields, "internal")?;
            push_u8_field(data, &mut offset, &mut fields, "reservedLinSpikeEvent1")?;
            push_u16_field(data, &mut offset, &mut fields, "reservedLinSpikeEvent2")?;
        }
        ObjectType::LinLongDomSig => {
            read_lin_bus_event(data, &mut offset, &mut fields)?;
            push_u8_field(data, &mut offset, &mut fields, "type")?;
            push_u8_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinLongDomSignalEvent1",
            )?;
            push_u16_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinLongDomSignalEvent2",
            )?;
            push_u32_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinLongDomSignalEvent3",
            )?;
        }
        ObjectType::LinLongDomSig2 => {
            read_lin_bus_event(data, &mut offset, &mut fields)?;
            push_u8_field(data, &mut offset, &mut fields, "type")?;
            push_u8_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinLongDomSignalEvent1",
            )?;
            push_u16_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinLongDomSignalEvent2",
            )?;
            push_u32_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinLongDomSignalEvent3",
            )?;
            push_u64_field(data, &mut offset, &mut fields, "length")?;
        }
        ObjectType::LinUnexpectedWakeup => {
            read_lin_bus_event(data, &mut offset, &mut fields)?;
            push_u64_field(data, &mut offset, &mut fields, "width")?;
            push_u8_field(data, &mut offset, &mut fields, "signal")?;
            push_u8_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinUnexpectedWakeup1",
            )?;
            push_u16_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinUnexpectedWakeup2",
            )?;
            push_u32_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinUnexpectedWakeup3",
            )?;
        }
        ObjectType::LinShortOrSlowResponse => {
            read_lin_databyte_timestamp_event(data, &mut offset, &mut fields)?;
            push_u32_field(data, &mut offset, &mut fields, "numberOfRespBytes")?;
            push_bytes_field(data, &mut offset, 9, &mut fields, "respBytes")?;
            push_u8_field(data, &mut offset, &mut fields, "slowResponse")?;
            push_u8_field(data, &mut offset, &mut fields, "interruptedByBreak")?;
            push_u8_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinShortOrSlowResponse",
            )?;
        }
        ObjectType::LinShortOrSlowResponse2 => {
            read_lin_databyte_timestamp_event(data, &mut offset, &mut fields)?;
            push_u32_field(data, &mut offset, &mut fields, "numberOfRespBytes")?;
            push_bytes_field(data, &mut offset, 9, &mut fields, "respBytes")?;
            push_u8_field(data, &mut offset, &mut fields, "slowResponse")?;
            push_u8_field(data, &mut offset, &mut fields, "interruptedByBreak")?;
            push_u8_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinShortOrSlowResponse1",
            )?;
            push_f64_field(data, &mut offset, &mut fields, "exactHeaderBaudrate")?;
            push_u32_field(data, &mut offset, &mut fields, "earlyStopbitOffset")?;
            push_u32_field(
                data,
                &mut offset,
                &mut fields,
                "reservedLinShortOrSlowResponse2",
            )?;
        }
        _ => {
            return Err(BlfError::UnsupportedObject(base.object_type.as_u32()));
        }
    }

    Ok(LinObject {
        object_type: base.object_type,
        header,
        fields,
    })
}
