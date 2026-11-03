use std::fs::File as StdFile;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use flate2::read::ZlibDecoder;
use memmap2::MmapOptions;

use crate::error::{BlfError, Result};
use crate::types::{
    object_type_name, read_system_time, read_u16, read_u32, read_u64, AppText, CanErrorFrame,
    CanErrorFrameExt, CanFdErrorFrame64, CanFdExtFrameData, CanFdMessage, CanFdMessage64,
    CanMessage, CanMessage2, FieldValue, FileStatistics, LinObject, Object, ObjectHeader,
    ObjectHeader2, ObjectHeaderBase, ObjectType, RawObject, RawObjectHeader, RestorePointContainer,
    StructuredObject, VarObjectHeader, FILE_SIGNATURE, OBJECT_SIGNATURE,
};

const LOBJ_BYTES: [u8; 4] = [0x4C, 0x4F, 0x42, 0x4A];
const FILE_STATISTICS_SIZE: usize = 144;
const LOG_CONTAINER_HEADER_SIZE: usize = 32;
const MAX_UNCOMPRESSED_CONTAINER_SIZE: u64 = 512 * 1024 * 1024;
const MAX_OBJECT_SIGNATURE_SCAN: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct ReaderOptions {
    pub strict: bool,
    /// Preserve truly unknown object IDs as `Object::Raw`.
    pub preserve_unknown_objects: bool,
    /// Skip truly unknown object IDs instead of returning `Object::Raw`.
    /// This is retained as an explicit override for callers that enable raw
    /// preservation globally and still want a filtered stream.
    pub skip_unknown_objects: bool,
    pub max_uncompressed_container_size: u64,
    pub max_object_size: u64,
    pub max_object_signature_scan: usize,
}

impl Default for ReaderOptions {
    fn default() -> Self {
        Self {
            strict: false,
            preserve_unknown_objects: false,
            skip_unknown_objects: false,
            max_uncompressed_container_size: MAX_UNCOMPRESSED_CONTAINER_SIZE,
            max_object_size: u64::from(u32::MAX),
            max_object_signature_scan: MAX_OBJECT_SIGNATURE_SCAN,
        }
    }
}

fn checked_offset(offset: usize, amount: usize) -> Result<usize> {
    offset
        .checked_add(amount)
        .ok_or_else(|| BlfError::Format("BLF offset overflow".to_string()))
}

fn data_range(data: &[u8], start: usize, length: usize) -> Result<&[u8]> {
    let end = checked_offset(start, length)?;
    data.get(start..end).ok_or(BlfError::Truncated)
}

pub fn parse_file_statistics(data: &[u8]) -> Result<FileStatistics> {
    if data.len() < FILE_STATISTICS_SIZE {
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
        extensions: data
            .get(FILE_STATISTICS_SIZE..)
            .unwrap_or_default()
            .to_vec(),
    })
}

pub struct File {
    mmap: Arc<memmap2::Mmap>,
    outer_pos: usize,
    decompressed: Vec<u8>,
    decompressed_pos: usize,
    pub file_statistics: FileStatistics,
    pub current_uncompressed_file_size: u64,
    pub current_object_count: u32,
    pub compression_level: i32,
    pub write_restore_points: bool,
    pub strict: bool,
    pub options: ReaderOptions,
    open: bool,
    eof: bool,
}

impl File {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(path, false)
    }

    pub fn open_with_options(path: impl AsRef<Path>, strict: bool) -> Result<Self> {
        let options = ReaderOptions {
            strict,
            ..ReaderOptions::default()
        };
        Self::open_with_reader_options(path, options)
    }

    pub fn open_with_reader_options(
        path: impl AsRef<Path>,
        options: ReaderOptions,
    ) -> Result<Self> {
        let std_file = StdFile::open(path)?;
        let mmap = unsafe { MmapOptions::new().map(&std_file)? };
        if mmap.len() < FILE_STATISTICS_SIZE {
            return Err(BlfError::Truncated);
        }

        let statistics_header = parse_file_statistics(&mmap[..FILE_STATISTICS_SIZE])?;
        let statistics_size = usize::try_from(statistics_header.statistics_size)
            .map_err(|_| BlfError::Format("statistics size does not fit usize".to_string()))?;
        if statistics_size < FILE_STATISTICS_SIZE {
            return Err(BlfError::Format(format!(
                "statistics size {} is smaller than the BLF file statistics header",
                statistics_header.statistics_size
            )));
        }
        if statistics_size > mmap.len() {
            return Err(BlfError::Truncated);
        }
        let file_statistics = parse_file_statistics(&mmap[..statistics_size])?;

        Ok(Self {
            mmap: Arc::new(mmap),
            outer_pos: statistics_size,
            decompressed: Vec::new(),
            decompressed_pos: 0,
            current_uncompressed_file_size: file_statistics.statistics_size as u64,
            current_object_count: 0,
            compression_level: file_statistics.compression_level as i32,
            write_restore_points: true,
            file_statistics,
            strict: options.strict,
            options,
            open: true,
            eof: false,
        })
    }

    /// Create a fresh sequential cursor over the same immutable BLF source.
    ///
    /// The returned cursor does not share decompression buffers, logical
    /// positions, EOF state, or object counters with this reader. Only the
    /// memory-mapped source and immutable reader configuration are shared.
    pub fn independent_cursor(&self) -> Self {
        Self {
            mmap: Arc::clone(&self.mmap),
            outer_pos: self.file_statistics.statistics_size as usize,
            decompressed: Vec::new(),
            decompressed_pos: 0,
            file_statistics: self.file_statistics.clone(),
            current_uncompressed_file_size: self.file_statistics.statistics_size as u64,
            current_object_count: 0,
            compression_level: self.compression_level,
            write_restore_points: self.write_restore_points,
            strict: self.strict,
            options: self.options,
            open: true,
            eof: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn good(&self) -> bool {
        self.open && !self.eof
    }

    pub fn eof(&self) -> bool {
        self.eof
    }

    pub fn close(&mut self) {
        self.open = false;
        self.eof = true;
    }

    pub fn read(&mut self) -> Result<Option<Object>> {
        if !self.open {
            return Ok(None);
        }
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
                    if !self.load_next_log_container()? {
                        self.eof = true;
                        let remaining = &self.decompressed[self.decompressed_pos..];
                        if remaining.is_empty()
                            || (remaining.len() <= 3 && remaining.iter().all(|byte| *byte == 0))
                        {
                            return Ok(None);
                        }
                        return Err(BlfError::Truncated);
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

        let signature_pos = find_lobj(
            &self.decompressed,
            self.decompressed_pos,
            self.options.max_object_signature_scan,
        )?;

        let base = parse_object_header_base(&self.decompressed, signature_pos)?;
        validate_object_size(&base)?;
        self.check_object_size(base.object_size)?;
        let object_size = base.object_size as usize;
        let declared_end = signature_pos
            .checked_add(object_size)
            .ok_or(BlfError::Format("object size overflow".to_string()))?;

        if declared_end > self.decompressed.len() {
            return Err(BlfError::Incomplete);
        }

        // Older converter output contains GlobalMarker records whose header
        // objectSize omits the variable strings. The upstream reader consumes
        // the declared strings and then skips the declared alignment bytes.
        let (object_end, stream_end) =
            recover_global_marker_size(&self.decompressed, signature_pos, declared_end, base)?;
        let effective_size = object_end
            .checked_sub(signature_pos)
            .ok_or_else(|| BlfError::Format("recovered object size underflow".to_string()))?;
        self.check_object_size_u64(effective_size as u64)?;

        let slice = self
            .decompressed
            .get(signature_pos..object_end)
            .ok_or(BlfError::Truncated)?;
        if is_reserved_type(base.object_type) {
            self.decompressed_pos = stream_end;
            return Ok(None);
        }
        let object = match (|| -> Result<Option<Object>> {
            if is_lin_type(base.object_type) {
                Ok(Some(Object::Lin(parse_lin_object(slice, base)?)))
            } else {
                match base.object_type {
                    ObjectType::CanMessage => {
                        Ok(Some(Object::CanMessage(parse_can_message(slice, base)?)))
                    }
                    ObjectType::CanMessage2 => {
                        Ok(Some(Object::CanMessage2(parse_can_message2(slice, base)?)))
                    }
                    ObjectType::CanFdMessage => Ok(Some(Object::CanFdMessage(
                        parse_can_fd_message(slice, base)?,
                    ))),
                    ObjectType::CanFdMessage64 => Ok(Some(Object::CanFdMessage64(
                        parse_can_fd_message64(slice, base)?,
                    ))),
                    ObjectType::CanError => Ok(Some(Object::CanErrorFrame(parse_can_error_frame(
                        slice, base,
                    )?))),
                    ObjectType::CanErrorExt => Ok(Some(Object::CanErrorFrameExt(
                        parse_can_error_frame_ext(slice, base)?,
                    ))),
                    ObjectType::CanFdError64 => Ok(Some(Object::CanFdErrorFrame64(
                        parse_can_fd_error_frame64(slice, base)?,
                    ))),
                    ObjectType::AppText => Ok(Some(Object::AppText(parse_app_text(slice, base)?))),
                    ObjectType::Unknown115 => Ok(Some(Object::RestorePointContainer(
                        parse_restore_point_container(slice, base)?,
                    ))),
                    _ => {
                        if self.strict && base.object_type == ObjectType::Unknown {
                            return Err(BlfError::UnsupportedObject(base.raw_object_type));
                        }
                        if base.object_type == ObjectType::Unknown {
                            if self.options.skip_unknown_objects
                                || !self.options.preserve_unknown_objects
                            {
                                self.decompressed_pos = stream_end;
                                Ok(None)
                            } else {
                                Ok(Some(Object::Raw(parse_raw_object(slice, base)?)))
                            }
                        } else {
                            Ok(Some(Object::Structured(parse_structured_object(
                                slice, base,
                            )?)))
                        }
                    }
                }
            }
        })() {
            Err(BlfError::Incomplete) => return Err(BlfError::Truncated),
            result => result?,
        };

        self.decompressed_pos = stream_end;
        Ok(object)
    }

    fn load_next_log_container(&mut self) -> Result<bool> {
        if self.outer_pos >= self.mmap.len() {
            return Ok(false);
        }

        let signature_pos = self.outer_pos;
        let base = parse_object_header_base(&self.mmap, signature_pos)?;

        if base.object_type != ObjectType::LogContainer {
            return Err(BlfError::Format(format!(
                "expected log container at offset {signature_pos}, found object type {}",
                base.object_type.as_u32()
            )));
        }
        validate_object_size(&base)?;
        self.check_object_size(base.object_size)?;
        if (base.object_size as usize) < LOG_CONTAINER_HEADER_SIZE {
            return Err(BlfError::Format(
                "log container is smaller than its fixed header".to_string(),
            ));
        }

        let object_size = usize::try_from(base.object_size)
            .map_err(|_| BlfError::Format("log container size does not fit usize".to_string()))?;

        let Some(header_end) = signature_pos.checked_add(object_size) else {
            return Err(BlfError::Format("log container size overflow".to_string()));
        };
        if header_end > self.mmap.len() {
            return Err(BlfError::Truncated);
        }

        let Ok(compression_method) = read_u16(&self.mmap, signature_pos + 16) else {
            return Err(BlfError::Truncated);
        };
        let Ok(uncompressed_size) = read_u32(&self.mmap, signature_pos + 24) else {
            return Err(BlfError::Truncated);
        };

        if u64::from(uncompressed_size) > self.options.max_uncompressed_container_size {
            return Err(BlfError::Limit {
                kind: "uncompressed log container",
                limit: self.options.max_uncompressed_container_size,
                actual: u64::from(uncompressed_size),
            });
        }

        let compressed_size = object_size - LOG_CONTAINER_HEADER_SIZE;
        let compressed_start = signature_pos + LOG_CONTAINER_HEADER_SIZE;
        let compressed_end = compressed_start
            .checked_add(compressed_size)
            .ok_or_else(|| BlfError::Format("compressed data size overflow".to_string()))?;
        let compressed = self
            .mmap
            .get(compressed_start..compressed_end)
            .ok_or(BlfError::Truncated)?;

        let mut decompressed = match compression_method {
            0 => {
                let actual = compressed.len() as u64;
                if actual > self.options.max_uncompressed_container_size {
                    return Err(BlfError::Limit {
                        kind: "uncompressed log container",
                        limit: self.options.max_uncompressed_container_size,
                        actual,
                    });
                }
                compressed.to_vec()
            }
            2 => {
                let uncompressed_size_usize = usize::try_from(uncompressed_size).map_err(|_| {
                    BlfError::Format("uncompressed size does not fit usize".to_string())
                })?;
                let mut output = Vec::with_capacity(uncompressed_size_usize);
                let decoder = ZlibDecoder::new(compressed);
                if decoder
                    .take((uncompressed_size as u64).saturating_add(1))
                    .read_to_end(&mut output)
                    .is_err()
                {
                    return Err(BlfError::Compression(
                        "failed to decompress log container".to_string(),
                    ));
                }
                output
            }
            _ => {
                return Err(BlfError::Compression(format!(
                    "unsupported compression method {compression_method}"
                )))
            }
        };

        if decompressed.len() != uncompressed_size as usize {
            return Err(BlfError::Compression(format!(
                "decompressed size mismatch: expected {}, got {}",
                uncompressed_size,
                decompressed.len()
            )));
        }

        let padding = object_size % 4;
        self.outer_pos = header_end
            .checked_add(padding)
            .ok_or_else(|| BlfError::Format("log container padding overflow".to_string()))?;
        self.current_uncompressed_file_size = self
            .current_uncompressed_file_size
            .checked_add(LOG_CONTAINER_HEADER_SIZE as u64 + u64::from(uncompressed_size))
            .ok_or_else(|| BlfError::Format("uncompressed file size overflow".to_string()))?;

        if self.decompressed_pos > 0 {
            self.decompressed = self.decompressed.split_off(self.decompressed_pos);
            self.decompressed_pos = 0;
        }
        self.decompressed.append(&mut decompressed);
        Ok(true)
    }

    fn check_object_size(&self, object_size: u32) -> Result<()> {
        self.check_object_size_u64(u64::from(object_size))
    }

    fn check_object_size_u64(&self, object_size: u64) -> Result<()> {
        if object_size > self.options.max_object_size {
            return Err(BlfError::Limit {
                kind: "BLF object",
                limit: self.options.max_object_size,
                actual: object_size,
            });
        }
        Ok(())
    }
}

fn find_lobj(data: &[u8], start: usize, max_scan: usize) -> Result<usize> {
    let remaining = data.get(start..).ok_or(BlfError::Incomplete)?;
    if remaining.starts_with(&LOBJ_BYTES) {
        return Ok(start);
    }

    if remaining.len() < 4 {
        return Err(BlfError::Incomplete);
    }

    // vector_blf scans forward for the next LOBJ signature when a converter
    // leaves non-object bytes between records. Keep that compatibility while
    // bounding work on damaged input so a large garbage region cannot turn a
    // single read into an unbounded search.
    let scan_len = remaining
        .len()
        .min(max_scan.saturating_add(LOBJ_BYTES.len() - 1));
    let scan = &remaining[..scan_len];
    if let Some(offset) = scan
        .windows(LOBJ_BYTES.len())
        .position(|window| window == LOBJ_BYTES)
    {
        return Ok(start + offset);
    }

    if remaining.len() > max_scan {
        return Err(BlfError::Format(format!(
            "LOBJ signature not found within {} bytes",
            max_scan
        )));
    }

    Err(BlfError::Format(
        "expected LOBJ object signature at logical stream position".to_string(),
    ))
}

fn validate_object_size(base: &ObjectHeaderBase) -> Result<()> {
    if base.object_size < 16 {
        return Err(BlfError::Format(format!(
            "object size {} is smaller than ObjectHeaderBase",
            base.object_size
        )));
    }
    if u32::from(base.header_size) > base.object_size {
        return Err(BlfError::Format(format!(
            "header size {} exceeds object size {}",
            base.header_size, base.object_size
        )));
    }
    Ok(())
}

fn recover_global_marker_size(
    data: &[u8],
    signature_pos: usize,
    declared_end: usize,
    base: ObjectHeaderBase,
) -> Result<(usize, usize)> {
    if base.object_type != ObjectType::GlobalMarker {
        return Ok((
            declared_end,
            object_stream_end(data, declared_end, base.object_size as usize),
        ));
    }

    // GlobalMarker's fixed payload ends after reservedGlobalMarker4. Its
    // three length fields are byte counts for the strings that follow it.
    let header_size = usize::from(base.header_size);
    let lengths_start = signature_pos
        .checked_add(header_size + 16)
        .ok_or_else(|| BlfError::Format("GlobalMarker offset overflow".to_string()))?;
    let group_len = usize::try_from(read_u32(data, lengths_start)?)
        .map_err(|_| BlfError::Format("GlobalMarker group length overflow".to_string()))?;
    let marker_start = checked_offset(lengths_start, 4)?;
    let description_start = checked_offset(lengths_start, 8)?;
    let marker_len = usize::try_from(read_u32(data, marker_start)?)
        .map_err(|_| BlfError::Format("GlobalMarker marker length overflow".to_string()))?;
    let description_len = usize::try_from(read_u32(data, description_start)?)
        .map_err(|_| BlfError::Format("GlobalMarker description length overflow".to_string()))?;
    let fixed_payload_size = 40usize;
    let content_end = signature_pos
        .checked_add(header_size + fixed_payload_size)
        .and_then(|value| value.checked_add(group_len))
        .and_then(|value| value.checked_add(marker_len))
        .and_then(|value| value.checked_add(description_len))
        .ok_or_else(|| BlfError::Format("GlobalMarker size overflow".to_string()))?;

    if content_end <= declared_end || content_end > data.len() {
        return Ok((
            declared_end,
            object_stream_end(data, declared_end, base.object_size as usize),
        ));
    }

    // The malformed historical record declares a shorter object size, so its
    // alignment is based on the declared size after the actual string data.
    let stream_end = object_stream_end(data, content_end, base.object_size as usize);
    Ok((content_end, stream_end))
}

fn object_stream_end(data: &[u8], declared_end: usize, object_size: usize) -> usize {
    if data_range(data, declared_end, LOBJ_BYTES.len())
        .ok()
        .is_some_and(|window| window == LOBJ_BYTES)
    {
        return declared_end;
    }

    let max_padding = object_size % 4;
    for padding in 1..=max_padding {
        let Some(candidate) = declared_end.checked_add(padding) else {
            break;
        };
        if data_range(data, candidate, LOBJ_BYTES.len())
            .ok()
            .is_some_and(|window| window == LOBJ_BYTES)
        {
            return candidate;
        }
    }

    if data.len().saturating_sub(declared_end) <= max_padding {
        return data.len();
    }

    // There is no following object in the current decompressed chunk. Do not
    // manufacture a truncation error merely because the final object is not
    // followed by materialized alignment bytes.
    declared_end.min(data.len())
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
        header_size: read_u16(data, checked_offset(offset, 4)?)?,
        header_version: read_u16(data, checked_offset(offset, 6)?)?,
        object_size: read_u32(data, checked_offset(offset, 8)?)?,
        object_type: ObjectType::from_raw(read_u32(data, checked_offset(offset, 12)?)?),
        raw_object_type: read_u32(data, checked_offset(offset, 12)?)?,
    })
}

fn parse_object_header(data: &[u8], offset: usize) -> Result<ObjectHeader> {
    let base = parse_object_header_base(data, offset)?;
    validate_object_size(&base)?;
    if base.header_version != 1 {
        return Err(BlfError::UnsupportedHeaderVersion(base.header_version));
    }
    if base.header_size < 32 || base.object_size < 32 {
        return Err(BlfError::Format(
            "object does not contain a complete ObjectHeader".to_string(),
        ));
    }
    Ok(ObjectHeader {
        base,
        object_flags: read_u32(data, checked_offset(offset, 16)?)?,
        client_index: read_u16(data, checked_offset(offset, 20)?)?,
        object_version: read_u16(data, checked_offset(offset, 22)?)?,
        object_time_stamp: read_u64(data, checked_offset(offset, 24)?)?,
    })
}

#[allow(dead_code)]
fn parse_object_header2(data: &[u8], offset: usize) -> Result<ObjectHeader2> {
    let base = parse_object_header_base(data, offset)?;
    validate_object_size(&base)?;
    // A number of upstream binlog fixtures contain ObjectHeader2 bytes with
    // headerVersion still set to 1. vector_blf reads the concrete 40-byte
    // header selected by the object class and does not reject that legacy
    // metadata. The physical header size is therefore the reliable signal.
    if base.header_size < 40 || base.object_size < 40 {
        return Err(BlfError::Format(
            "object does not contain a complete ObjectHeader2".to_string(),
        ));
    }
    Ok(ObjectHeader2 {
        base,
        object_flags: read_u32(data, checked_offset(offset, 16)?)?,
        time_stamp_status: *data
            .get(checked_offset(offset, 20)?)
            .ok_or(BlfError::Truncated)?,
        reserved_object_header: *data
            .get(checked_offset(offset, 21)?)
            .ok_or(BlfError::Truncated)?,
        object_version: read_u16(data, checked_offset(offset, 22)?)?,
        object_time_stamp: read_u64(data, checked_offset(offset, 24)?)?,
        original_time_stamp: read_u64(data, checked_offset(offset, 32)?)?,
    })
}

fn parse_raw_header(data: &[u8], base: ObjectHeaderBase) -> Result<RawObjectHeader> {
    if matches!(
        base.object_type,
        ObjectType::FunctionBus | ObjectType::TriggerCondition
    ) {
        return Ok(RawObjectHeader::Var(parse_var_object_header(data, 0)?));
    }

    // ObjectHeader2 is 40 bytes on the wire. Some historical Vector files
    // report headerVersion=1 for it, so choose the representation by the
    // declared physical size before falling back to the version field.
    if base.header_size >= 40 {
        return Ok(RawObjectHeader::Object2(parse_object_header2(data, 0)?));
    }

    match base.header_version {
        1 => Ok(RawObjectHeader::Object(parse_object_header(data, 0)?)),
        2 => Ok(RawObjectHeader::Object2(parse_object_header2(data, 0)?)),
        _ => Ok(RawObjectHeader::Base(base)),
    }
}

fn parse_raw_object(data: &[u8], base: ObjectHeaderBase) -> Result<RawObject> {
    let header = parse_raw_header(data, base)?;
    let header_size = usize::from(base.header_size);
    let payload = data.get(header_size..).ok_or(BlfError::Truncated)?.to_vec();
    Ok(RawObject {
        header,
        class_name: object_type_name(base.raw_object_type).to_string(),
        payload,
        data: data.to_vec(),
    })
}

#[derive(Clone, Copy)]
enum FieldSpec {
    U8(&'static str),
    U16(&'static str),
    I16(&'static str),
    U32(&'static str),
    I32(&'static str),
    U64(&'static str),
    F64(&'static str),
    Bytes(&'static str, usize),
    U16Vec(&'static str, usize),
    U32Vec(&'static str, usize),
    U64Vec(&'static str, usize),
    DynamicBytes(&'static str, &'static str),
    DynamicString(&'static str, &'static str),
    DynamicUtf16(&'static str, &'static str),
    RemainingBytes(&'static str),
}

struct StructuredReader<'a> {
    data: &'a [u8],
    offset: usize,
    fields: Vec<(String, FieldValue)>,
}

impl<'a> StructuredReader<'a> {
    fn new(data: &'a [u8], offset: usize) -> Self {
        Self {
            data,
            offset,
            fields: Vec::new(),
        }
    }

    fn get_len(&self, name: &str) -> Result<usize> {
        self.fields
            .iter()
            .rev()
            .find(|(field, _)| field == name)
            .and_then(|(_, value)| match value {
                FieldValue::U8(value) => Some(usize::from(*value)),
                FieldValue::U16(value) => Some(usize::from(*value)),
                FieldValue::U32(value) => usize::try_from(*value).ok(),
                FieldValue::U64(value) => usize::try_from(*value).ok(),
                _ => None,
            })
            .ok_or_else(|| BlfError::Format(format!("dynamic field length {name} is unavailable")))
    }

    fn push(&mut self, name: &str, value: FieldValue, size: usize) -> Result<()> {
        let next_offset = checked_offset(self.offset, size)?;
        self.fields.push((name.to_string(), value));
        self.offset = next_offset;
        Ok(())
    }

    fn u8(&mut self, name: &'static str) -> Result<()> {
        let value = *self.data.get(self.offset).ok_or(BlfError::Truncated)?;
        self.push(name, FieldValue::U8(value), 1)
    }

    fn u16(&mut self, name: &'static str) -> Result<()> {
        let value = read_u16(self.data, self.offset)?;
        self.push(name, FieldValue::U16(value), 2)
    }

    fn i16(&mut self, name: &'static str) -> Result<()> {
        let value = read_u16(self.data, self.offset)? as i16;
        self.push(name, FieldValue::I16(value), 2)
    }

    fn u32(&mut self, name: &'static str) -> Result<()> {
        let value = read_u32(self.data, self.offset)?;
        self.push(name, FieldValue::U32(value), 4)
    }

    fn i32(&mut self, name: &'static str) -> Result<()> {
        let value = read_u32(self.data, self.offset)? as i32;
        self.push(name, FieldValue::I32(value), 4)
    }

    fn u64(&mut self, name: &'static str) -> Result<()> {
        let value = read_u64(self.data, self.offset)?;
        self.push(name, FieldValue::U64(value), 8)
    }

    fn f64(&mut self, name: &'static str) -> Result<()> {
        let bytes = data_range(self.data, self.offset, 8)?;
        self.push(
            name,
            FieldValue::F64(f64::from_le_bytes(bytes.try_into().expect("8 bytes"))),
            8,
        )
    }

    fn bytes(&mut self, name: &'static str, size: usize) -> Result<()> {
        let value = data_range(self.data, self.offset, size)?.to_vec();
        self.push(name, FieldValue::Bytes(value), size)
    }

    fn string(&mut self, name: &'static str, size: usize) -> Result<()> {
        let value = data_range(self.data, self.offset, size)?;
        self.push(
            name,
            FieldValue::String(String::from_utf8_lossy(value).into_owned()),
            size,
        )
    }

    fn utf16(&mut self, name: &'static str, units: usize) -> Result<()> {
        let byte_length = units.checked_mul(2).ok_or(BlfError::Truncated)?;
        let raw = data_range(self.data, self.offset, byte_length)?;
        let (chunks, _) = raw.as_chunks::<2>();
        let values = chunks
            .iter()
            .map(|chunk| u16::from_le_bytes(*chunk))
            .collect::<Vec<_>>();
        self.push(
            name,
            FieldValue::String(String::from_utf16_lossy(&values)),
            byte_length,
        )
    }

    fn u16_vec(&mut self, name: &'static str, count: usize) -> Result<()> {
        let mut values = Vec::with_capacity(count);
        for index in 0..count {
            let byte_offset = index.checked_mul(2).ok_or(BlfError::Truncated)?;
            values.push(read_u16(
                self.data,
                checked_offset(self.offset, byte_offset)?,
            )?);
        }
        self.push(
            name,
            FieldValue::U16Vec(values),
            count.checked_mul(2).ok_or(BlfError::Truncated)?,
        )
    }

    fn u32_vec(&mut self, name: &'static str, count: usize) -> Result<()> {
        let mut values = Vec::with_capacity(count);
        for index in 0..count {
            let byte_offset = index.checked_mul(4).ok_or(BlfError::Truncated)?;
            values.push(read_u32(
                self.data,
                checked_offset(self.offset, byte_offset)?,
            )?);
        }
        self.push(
            name,
            FieldValue::U32Vec(values),
            count.checked_mul(4).ok_or(BlfError::Truncated)?,
        )
    }

    fn u64_vec(&mut self, name: &'static str, count: usize) -> Result<()> {
        let mut values = Vec::with_capacity(count);
        for index in 0..count {
            let byte_offset = index.checked_mul(8).ok_or(BlfError::Truncated)?;
            values.push(read_u64(
                self.data,
                checked_offset(self.offset, byte_offset)?,
            )?);
        }
        self.push(
            name,
            FieldValue::U64Vec(values),
            count.checked_mul(8).ok_or(BlfError::Truncated)?,
        )
    }

    fn dynamic_bytes(&mut self, name: &'static str, length: &'static str) -> Result<()> {
        let size = self.get_len(length)?;
        self.bytes(name, size)
    }

    fn dynamic_string(&mut self, name: &'static str, length: &'static str) -> Result<()> {
        let size = self.get_len(length)?;
        self.string(name, size)
    }

    fn dynamic_utf16(&mut self, name: &'static str, length: &'static str) -> Result<()> {
        let units = self.get_len(length)?;
        self.utf16(name, units)
    }

    fn remaining_bytes(&mut self, name: &'static str) -> Result<()> {
        let value = self
            .data
            .get(self.offset..)
            .ok_or(BlfError::Truncated)?
            .to_vec();
        let size = value.len();
        self.push(name, FieldValue::Bytes(value), size)
    }

    fn apply(&mut self, spec: FieldSpec) -> Result<()> {
        match spec {
            FieldSpec::U8(name) => self.u8(name),
            FieldSpec::U16(name) => self.u16(name),
            FieldSpec::I16(name) => self.i16(name),
            FieldSpec::U32(name) => self.u32(name),
            FieldSpec::I32(name) => self.i32(name),
            FieldSpec::U64(name) => self.u64(name),
            FieldSpec::F64(name) => self.f64(name),
            FieldSpec::Bytes(name, size) => self.bytes(name, size),
            FieldSpec::U16Vec(name, count) => self.u16_vec(name, count),
            FieldSpec::U32Vec(name, count) => self.u32_vec(name, count),
            FieldSpec::U64Vec(name, count) => self.u64_vec(name, count),
            FieldSpec::DynamicBytes(name, length) => self.dynamic_bytes(name, length),
            FieldSpec::DynamicString(name, length) => self.dynamic_string(name, length),
            FieldSpec::DynamicUtf16(name, length) => self.dynamic_utf16(name, length),
            FieldSpec::RemainingBytes(name) => self.remaining_bytes(name),
        }
    }
}

fn parse_schema(
    data: &[u8],
    start: usize,
    schema: &[FieldSpec],
) -> Result<(Vec<(String, FieldValue)>, usize)> {
    let mut reader = StructuredReader::new(data, start);
    for spec in schema {
        reader.apply(*spec)?;
    }
    Ok((reader.fields, reader.offset))
}

fn parse_structured_object(data: &[u8], base: ObjectHeaderBase) -> Result<StructuredObject> {
    let header = parse_raw_header(data, base)?;
    let (fields, offset) = if base.object_type == ObjectType::SerialEvent {
        parse_serial_event(data, usize::from(base.header_size))?
    } else {
        let schema = structured_schema(base.object_type);
        parse_schema(data, usize::from(base.header_size), schema)?
    };
    Ok(StructuredObject {
        object_type: base.object_type,
        header,
        class_name: object_type_name(base.raw_object_type).to_string(),
        fields,
        extensions: data.get(offset..).ok_or(BlfError::Truncated)?.to_vec(),
        data: data.to_vec(),
    })
}

fn parse_serial_event(data: &[u8], start: usize) -> Result<(Vec<(String, FieldValue)>, usize)> {
    let mut reader = StructuredReader::new(data, start);
    reader.u32("flags")?;
    reader.u32("port")?;
    reader.u32("baudrate")?;
    reader.u32("reservedSerialEvent")?;

    let flags = reader.get_len("flags")?;
    if flags & 0x04 != 0 {
        reader.u8("byte")?;
        reader.bytes("reservedSerialEventUnion", 15)?;
    } else if flags & 0x08 != 0 {
        reader.u8("compactLength")?;
        reader.bytes("compactData", 15)?;
    } else {
        reader.u32("dataLength")?;
        reader.u32("timeStampsLength")?;
        reader.u64("reservedGeneralSerialEvent")?;

        let data_length = reader.get_len("dataLength")?;
        reader.bytes("data", data_length)?;
        let timestamps_length = reader.get_len("timeStampsLength")?;
        // Validate the complete untrusted timestamp region before allocating a
        // vector based on its element count.
        let timestamp_data = data_range(data, reader.offset, timestamps_length)?;
        let timestamp_count = timestamps_length / 8;
        let mut timestamps = Vec::with_capacity(timestamp_count);
        let (timestamp_chunks, _) = timestamp_data.as_chunks::<8>();
        for timestamp_bytes in timestamp_chunks.iter().take(timestamp_count) {
            timestamps.push(i64::from_le_bytes(*timestamp_bytes));
        }
        reader.offset = checked_offset(
            reader.offset,
            timestamp_count.checked_mul(8).ok_or(BlfError::Truncated)?,
        )?;
        reader
            .fields
            .push(("timeStamps".to_string(), FieldValue::I64Vec(timestamps)));
        let remainder = timestamps_length % 8;
        if remainder != 0 {
            reader.bytes("timeStampsRemainder", remainder)?;
        }
    }

    Ok((reader.fields, reader.offset))
}

fn structured_schema(object_type: ObjectType) -> &'static [FieldSpec] {
    use FieldSpec::*;
    match object_type {
        ObjectType::CanOverload => &[
            U16("channel"),
            U16("reservedCanOverloadFrame1"),
            U32("reservedCanOverloadFrame2"),
        ],
        ObjectType::CanStatistic => &[
            U16("channel"),
            U16("busLoad"),
            U32("standardDataFrames"),
            U32("extendedDataFrames"),
            U32("standardRemoteFrames"),
            U32("extendedRemoteFrames"),
            U32("errorFrames"),
            U32("overloadFrames"),
            U32("reservedCanDriverStatistic"),
        ],
        ObjectType::AppTrigger => &[
            U64("preTriggerTime"),
            U64("postTriggerTime"),
            U16("channel"),
            U16("flags"),
            U32("appSpecific2"),
        ],
        ObjectType::EnvInteger
        | ObjectType::EnvDouble
        | ObjectType::EnvString
        | ObjectType::EnvData => &[
            U32("nameLength"),
            U32("dataLength"),
            U64("reservedEnvironmentVariable"),
            DynamicString("name", "nameLength"),
            DynamicBytes("data", "dataLength"),
        ],
        ObjectType::MostSpy => &[
            U16("channel"),
            U8("dir"),
            U8("reservedMostSpy1"),
            U32("sourceAdr"),
            U32("destAdr"),
            Bytes("msg", 17),
            U8("reservedMostSpy2"),
            U16("rTyp"),
            U8("rTypAdr"),
            U8("state"),
            U8("reservedMostSpy3"),
            U8("ackNack"),
            U32("crc"),
        ],
        ObjectType::MostCtrl => &[
            U16("channel"),
            U8("dir"),
            U8("reservedMostCtrl1"),
            U32("sourceAdr"),
            U32("destAdr"),
            Bytes("msg", 17),
            U8("reservedMostCtrl2"),
            U16("rTyp"),
            U8("rTypAdr"),
            U8("state"),
            U8("reservedMostCtrl3"),
            U8("ackNack"),
            U32("reservedMostCtrl4"),
        ],
        ObjectType::MostLightlock => &[U16("channel"), I16("state"), U32("reservedMostLightLock")],
        ObjectType::MostStatistic => &[
            U16("channel"),
            U16("pktCnt"),
            I32("frmCnt"),
            I32("lightCnt"),
            I32("bufferLevel"),
        ],
        ObjectType::FlexrayData => &[
            U16("channel"),
            U8("mux"),
            U8("len"),
            U16("messageId"),
            U16("crc"),
            U8("dir"),
            U8("reservedFlexRayData1"),
            U16("reservedFlexRayData2"),
            Bytes("dataBytes", 12),
        ],
        ObjectType::FlexraySync => &[
            U16("channel"),
            U8("mux"),
            U8("len"),
            U16("messageId"),
            U16("crc"),
            U8("dir"),
            U8("reservedFlexRaySync1"),
            U16("reservedFlexRaySync2"),
            Bytes("dataBytes", 11),
            U8("cycle"),
        ],
        ObjectType::CanDriverError => &[
            U16("channel"),
            U8("txErrors"),
            U8("rxErrors"),
            U32("errorCode"),
        ],
        ObjectType::MostPkt | ObjectType::MostPkt2 => most_pkt_schema(),
        ObjectType::MostHwmode => &[
            U16("channel"),
            U16("reservedMostHwMode"),
            U16("hwMode"),
            U16("hwModeMask"),
        ],
        ObjectType::MostReg => &[
            U16("channel"),
            U8("subType"),
            U8("reservedMostReg"),
            U32("handle"),
            U32("offset"),
            U16("chip"),
            U16("regDataLen"),
            Bytes("regData", 16),
        ],
        ObjectType::MostGenreg => &[
            U16("channel"),
            U8("subType"),
            U8("reservedMostGenReg1"),
            U32("handle"),
            U16("regId"),
            U16("reservedMostGenReg2"),
            U32("reservedMostGenReg3"),
            U64("regValue"),
        ],
        ObjectType::MostNetstate => &[
            U16("channel"),
            U16("stateNew"),
            U16("stateOld"),
            U16("reservedMostNetState"),
        ],
        ObjectType::MostDatalost => &[
            U16("channel"),
            U16("reservedMostDataLost"),
            U32("info"),
            U32("lostMsgsCtrl"),
            U32("lostMsgsAsync"),
            U64("lastGoodTimeStampNs"),
            U64("nextGoodTimeStampNs"),
        ],
        ObjectType::MostTrigger => &[
            U16("channel"),
            U16("reservedMostTrigger"),
            U16("mode"),
            U16("hw"),
            U32("previousTriggerValue"),
            U32("currentTriggerValue"),
        ],
        ObjectType::FlexrayCycle => &[
            U16("channel"),
            U8("dir"),
            U8("lowTime"),
            U32("fpgaTick"),
            U32("fpgaTickOverflow"),
            U32("clientIndexFlexRayV6StartCycleEvent"),
            U32("clusterTime"),
            Bytes("dataBytes", 2),
            U16("reservedFlexRayV6StartCycleEvent"),
        ],
        ObjectType::FlexrayMessage => &[
            U16("channel"),
            U8("dir"),
            U8("lowTime"),
            U32("fpgaTick"),
            U32("fpgaTickOverflow"),
            U32("clientIndexFlexRayV6Message"),
            U32("clusterTime"),
            U16("frameId"),
            U16("headerCrc"),
            U16("frameState"),
            U8("length"),
            U8("cycle"),
            U8("headerBitMask"),
            U8("reservedFlexRayV6Message1"),
            U16("reservedFlexRayV6Message2"),
            Bytes("dataBytes", 64),
        ],
        ObjectType::CanDriverSync => &[
            U16("channel"),
            U8("flags"),
            U8("reservedCanDriverHwSync1"),
            U32("reservedCanDriverHwSync2"),
        ],
        ObjectType::FlexrayStatus => &[
            U16("channel"),
            U16("version"),
            U16("statusType"),
            U16("infoMask1"),
            U16("infoMask2"),
            U16("infoMask3"),
            U16Vec("reservedFlexRayStatusEvent", 18),
        ],
        ObjectType::GpsEvent => &[
            U32("flags"),
            U16("channel"),
            U16("reservedGpsEvent"),
            F64("latitude"),
            F64("longitude"),
            F64("altitude"),
            F64("speed"),
            F64("course"),
        ],
        ObjectType::FrError => &[
            U16("channel"),
            U16("version"),
            U16("channelMask"),
            U8("cycle"),
            U8("reservedFlexRayVFrError1"),
            U32("clientIndexFlexRayVFrError"),
            U32("clusterNo"),
            U32("tag"),
            U32Vec("data", 4),
            U32("reservedFlexRayVFrError2"),
        ],
        ObjectType::FrStatus => &[
            U16("channel"),
            U16("version"),
            U16("channelMask"),
            U8("cycle"),
            U8("reservedFlexRayVFrStatus1"),
            U32("clientIndexFlexRayVFrStatus"),
            U32("clusterNo"),
            U32("wus"),
            U32("ccSyncState"),
            U32("tag"),
            U32Vec("data", 2),
            U16Vec("reservedFlexRayVFrStatus2", 18),
        ],
        ObjectType::FrStartcycle => &[
            U16("channel"),
            U16("version"),
            U16("channelMask"),
            U8("dir"),
            U8("cycle"),
            U32("clientIndexFlexRayVFrStartCycle"),
            U32("clusterNo"),
            U16("nmSize"),
            Bytes("dataBytes", 12),
            U16("reservedFlexRayVFrStartCycle1"),
            U32("tag"),
            U32Vec("data", 5),
            U64("reservedFlexRayVFrStartCycle2"),
        ],
        ObjectType::FrRcvmessage => flexray_receive_schema(),
        ObjectType::Realtimeclock => &[U64("time"), U64("loggingOffset")],
        ObjectType::J1708Message | ObjectType::J1708VirtualMsg => &[
            U16("channel"),
            U8("dir"),
            U8("reservedJ1708Message1"),
            U16("error"),
            U8("size"),
            Bytes("data", 255),
            U16("reservedJ1708Message2"),
        ],
        ObjectType::FrRcvmessageEx => flexray_receive_ex_schema(),
        ObjectType::MostStatisticex => &[
            U16("channel"),
            U16("reservedMostStatisticEx1"),
            U32("codingErrors"),
            U32("frameCounter"),
            U32("reservedMostStatisticEx2"),
        ],
        ObjectType::MostTxlight => &[U16("channel"), U16("state"), U32("reservedMostTxLight")],
        ObjectType::MostAlloctab => &[
            U16("channel"),
            U16("length"),
            U32("reservedMostAllocTab"),
            DynamicBytes("tableData", "length"),
        ],
        ObjectType::MostStress => &[
            U16("channel"),
            U16("state"),
            U16("mode"),
            U16("reservedMostStress"),
        ],
        ObjectType::EthernetFrame => &[
            Bytes("sourceAddress", 6),
            U16("channel"),
            Bytes("destinationAddress", 6),
            U16("dir"),
            U16("type"),
            U16("tpid"),
            U16("tci"),
            U16("payLoadLength"),
            U64("reservedEthernetFrame"),
            DynamicBytes("payLoad", "payLoadLength"),
        ],
        ObjectType::SysVariable => &[
            U32("type"),
            U32("representation"),
            U64("reservedSystemVariable1"),
            U32("nameLength"),
            U32("dataLength"),
            U64("reservedSystemVariable2"),
            DynamicString("name", "nameLength"),
            DynamicBytes("data", "dataLength"),
        ],
        ObjectType::CanDriverErrorExt => &[
            U16("channel"),
            U8("txErrors"),
            U8("rxErrors"),
            U32("errorCode"),
            U32("flags"),
            U8("state"),
            U8("reservedCanDriverErrorExt1"),
            U16("reservedCanDriverErrorExt2"),
            U32Vec("reservedCanDriverErrorExt3", 4),
        ],
        ObjectType::CanSettingChanged => &[
            U16("channel"),
            U8("changedType"),
            U32("btrExtArb"),
            U32("btrExtData"),
        ],
        ObjectType::Most150Message => most150_message_schema(),
        ObjectType::Most150Pkt => most150_pkt_schema(),
        ObjectType::MostEthernetPkt => most_ethernet_pkt_schema(),
        ObjectType::Most150MessageFragment => most150_message_fragment_schema(),
        ObjectType::Most150PktFragment => most150_pkt_fragment_schema(),
        ObjectType::MostEthernetPktFragment => most_ethernet_fragment_schema(),
        ObjectType::MostSystemEvent => &[
            U16("channel"),
            U16("id"),
            U32("value"),
            U32("valueOld"),
            U32("reservedMostSystemEvent"),
        ],
        ObjectType::Most150Alloctab => &[
            U16("channel"),
            U16("eventModeFlags"),
            U16("freeBytes"),
            U16("length"),
            U64("reservedMost150AllocTab"),
            DynamicBytes("tableData", "length"),
        ],
        ObjectType::Most50Message => most50_message_schema(),
        ObjectType::Most50Pkt => most50_pkt50_schema(),
        ObjectType::SerialEvent => &[
            U32("flags"),
            U32("port"),
            U32("baudrate"),
            U32("reservedSerialEvent"),
        ],
        ObjectType::OverrunError => &[U32("busType"), U16("channel"), U16("reservedDriverOverrun")],
        ObjectType::EventComment => &[
            U32("commentedEventType"),
            U32("textLength"),
            U64("reservedEventComment"),
            DynamicString("text", "textLength"),
        ],
        ObjectType::WlanFrame => &[
            U16("channel"),
            U16("flags"),
            U8("dir"),
            U8("radioChannel"),
            I16("signalStrength"),
            U16("signalQuality"),
            U16("frameLength"),
            U32("reservedWlanFrame"),
            DynamicBytes("frameData", "frameLength"),
        ],
        ObjectType::WlanStatistic => &[
            U16("channel"),
            U16("flags"),
            U32("rxPacketCount"),
            U32("rxByteCount"),
            U32("txPacketCount"),
            U32("txByteCount"),
            U32("collisionCount"),
            U32("errorCount"),
            U32("reservedWlanStatistic"),
        ],
        ObjectType::MostEcl => &[
            U16("channel"),
            U16("mode"),
            U16("eclState"),
            U16("reservedMostEcl"),
        ],
        ObjectType::GlobalMarker => &[
            U32("commentedEventType"),
            U32("foregroundColor"),
            U32("backgroundColor"),
            U8("isRelocatable"),
            U8("reservedGlobalMarker1"),
            U16("reservedGlobalMarker2"),
            U32("groupNameLength"),
            U32("markerNameLength"),
            U32("descriptionLength"),
            U32("reservedGlobalMarker3"),
            U64("reservedGlobalMarker4"),
            DynamicString("groupName", "groupNameLength"),
            DynamicString("markerName", "markerNameLength"),
            DynamicString("description", "descriptionLength"),
        ],
        ObjectType::AfdxFrame => &[
            Bytes("sourceAddress", 6),
            U16("channel"),
            Bytes("destinationAddress", 6),
            U16("dir"),
            U16("type"),
            U16("tpid"),
            U16("tci"),
            U8("ethChannel"),
            U8("reservedAfdxFrame1"),
            U16("afdxFlags"),
            U16("reservedAfdxFrame2"),
            U32("bagUsec"),
            U16("payLoadLength"),
            U16("reservedAfdxFrame3"),
            U32("reservedAfdxFrame4"),
            DynamicBytes("payLoad", "payLoadLength"),
        ],
        ObjectType::AfdxStatistic => afdx_statistic_schema(),
        ObjectType::KlineStatusEvent => &[
            U16("type"),
            U16("dataLen"),
            U32("port"),
            U64("reservedKLineStatusEvent"),
            U64Vec("data", 3),
        ],
        ObjectType::EthernetRxError => &[
            U16("structLength"),
            U16("channel"),
            U16("dir"),
            U16("hardwareChannel"),
            U32("fcs"),
            U16("frameDataLength"),
            U16("reservedEthernetRxError"),
            U32("error"),
            DynamicBytes("frameData", "frameDataLength"),
        ],
        ObjectType::EthernetStatus => &[
            U16("channel"),
            U16("flags"),
            U8("linkStatus"),
            U8("ethernetPhy"),
            U8("duplex"),
            U8("mdi"),
            U8("connector"),
            U8("clockMode"),
            U8("pairs"),
            U8("hardwareChannel"),
            U32("bitrate"),
        ],
        ObjectType::AfdxStatus => afdx_status_schema(),
        ObjectType::AfdxBusStatistic => afdx_bus_statistic_schema(),
        ObjectType::AfdxErrorEvent => &[
            U16("channel"),
            U16("errorLevel"),
            U32("sourceIdentifier"),
            Bytes("errorText", 512),
            Bytes("errorAttributes", 512),
        ],
        ObjectType::A429Error => &[
            U16("channel"),
            U16("errorType"),
            U32("sourceIdentifier"),
            U32("errReason"),
            Bytes("errorText", 512),
            Bytes("errorAttributes", 512),
            U32("reservedA429Error"),
        ],
        ObjectType::A429Status => &[
            U16("channel"),
            U8("dir"),
            U8("reservedA429Status1"),
            U16("parity"),
            U16("reservedA429Status2"),
            U32("minGap"),
            U32("bitrate"),
            U32("minBitrate"),
            U32("maxBitrate"),
        ],
        ObjectType::A429BusStatistic => &[
            U16("channel"),
            U8("dir"),
            U8("reservedA429BusStatistic"),
            U32("busload"),
            U32("dataTotal"),
            U32("errorTotal"),
            U32("bitrate"),
            U16("parityErrors"),
            U16("bitrateErrors"),
            U16("gapErrors"),
            U16("lineErrors"),
            U16("formatErrors"),
            U16("dutyFactorErrors"),
            U16("wordLenErrors"),
            U16("codingErrors"),
            U16("idleErrors"),
            U16("levelErrors"),
            U16Vec("labelCount", 256),
        ],
        ObjectType::A429Message => &[
            Bytes("a429Data", 4),
            U16("channel"),
            U8("dir"),
            U8("reservedA429Message1"),
            U32("bitrate"),
            I32("errReason"),
            U16("errPosition"),
            U16("reservedA429Message2"),
            U32("reservedA429Message3"),
            U64("frameGap"),
            U32("frameLength"),
            U16("msgCtrl"),
            U16("reservedA429Message4"),
            U32("cycleTime"),
            U32("error"),
            U32("bitLenOfLastBit"),
            U32("reservedA429Message5"),
        ],
        ObjectType::EthernetStatistic => &[
            U16("channel"),
            U16("reservedEthernetStatistic1"),
            U32("reservedEthernetStatistic2"),
            U64("rcvOk_HW"),
            U64("xmitOk_HW"),
            U64("rcvError_HW"),
            U64("xmitError_HW"),
            U64("rcvBytes_HW"),
            U64("xmitBytes_HW"),
            U64("rcvNoBuffer_HW"),
            I16("sqi"),
            U16("hardwareChannel"),
            U32("reservedEthernetStatistic3"),
        ],
        ObjectType::TestStructure => &[
            U32("executionObjectIdentify"),
            U16("type"),
            U16("reservedTestStructure"),
            U32("uniqueNo"),
            U16("action"),
            U16("result"),
            U32("executingObjectNameLength"),
            U32("nameLength"),
            U32("textLength"),
            DynamicUtf16("executingObjectName", "executingObjectNameLength"),
            DynamicUtf16("name", "nameLength"),
            DynamicUtf16("text", "textLength"),
        ],
        ObjectType::DiagRequestInterpretation => &[
            U32("diagDescriptionHandle"),
            U32("diagVariantHandle"),
            U32("diagServiceHandle"),
            U32("ecuQualifierLength"),
            U32("variantQualifierLength"),
            U32("serviceQualifierLength"),
            DynamicString("ecuQualifier", "ecuQualifierLength"),
            DynamicString("variantQualifier", "variantQualifierLength"),
            DynamicString("serviceQualifier", "serviceQualifierLength"),
        ],
        ObjectType::EthernetFrameEx => ethernet_frame_ex_schema(),
        ObjectType::EthernetFrameForwarded => ethernet_frame_forwarded_schema(),
        ObjectType::EthernetErrorEx | ObjectType::EthernetErrorForwarded => {
            ethernet_error_ex_schema()
        }
        ObjectType::FunctionBus => &[
            U32("functionBusObjectType"),
            U32("veType"),
            U32("nameLength"),
            U32("dataLength"),
            DynamicString("name", "nameLength"),
            DynamicBytes("data", "dataLength"),
        ],
        ObjectType::DataLostBegin => &[U32("queueIdentifier")],
        ObjectType::DataLostEnd => &[
            U32("queueIdentifier"),
            U64("firstObjectLostTimeStamp"),
            U32("numberOfLostEvents"),
        ],
        ObjectType::WaterMarkEvent => &[U32("queueState"), U32("reservedWaterMarkEvent")],
        ObjectType::TriggerCondition => &[
            U32("state"),
            U32("triggerBlockNameLength"),
            U32("triggerConditionLength"),
            DynamicString("triggerBlockName", "triggerBlockNameLength"),
            DynamicString("triggerCondition", "triggerConditionLength"),
        ],
        ObjectType::DistributedObjectMember => &[
            U32("memberType"),
            U32("detailType"),
            U32("pathLength"),
            U32("dataLength"),
            DynamicString("path", "pathLength"),
            DynamicBytes("data", "dataLength"),
        ],
        ObjectType::AttributeEvent => &[
            U32("mainAttributableObjectPathLength"),
            U32("memberPathLength"),
            U32("attributeDefinitionPathLength"),
            U32("dataLength"),
            DynamicString(
                "mainAttributableObjectPath",
                "mainAttributableObjectPathLength",
            ),
            DynamicString("memberPath", "memberPathLength"),
            DynamicString("attributeDefinitionPath", "attributeDefinitionPathLength"),
            DynamicBytes("data", "dataLength"),
        ],
        _ => &[],
    }
}

fn most_pkt_schema() -> &'static [FieldSpec] {
    use FieldSpec::*;
    static SCHEMA: [FieldSpec; 18] = [
        U16("channel"),
        U8("dir"),
        U8("reservedMostPkt1"),
        U32("sourceAdr"),
        U32("destAdr"),
        U8("arbitration"),
        U8("timeRes"),
        U8("quadsToFollow"),
        U8("reservedMostPkt2"),
        U16("crc"),
        U8("priority"),
        U8("transferType"),
        U8("state"),
        U8("reservedMostPkt3"),
        U16("reservedMostPkt4"),
        U32("pktDataLength"),
        U32("reservedMostPkt5"),
        DynamicBytes("pktData", "pktDataLength"),
    ];
    &SCHEMA
}

fn most150_message_schema() -> &'static [FieldSpec] {
    // The only difference between MOST150 message and packet is the variable
    // member name; their wire layouts are identical.
    static SCHEMA: [FieldSpec; 17] = [
        FieldSpec::U16("channel"),
        FieldSpec::U8("dir"),
        FieldSpec::U8("reservedMost150Message1"),
        FieldSpec::U32("sourceAdr"),
        FieldSpec::U32("destAdr"),
        FieldSpec::U8("transferType"),
        FieldSpec::U8("state"),
        FieldSpec::U8("ackNack"),
        FieldSpec::U8("reservedMost150Message2"),
        FieldSpec::U32("crc"),
        FieldSpec::U8("pAck"),
        FieldSpec::U8("cAck"),
        FieldSpec::U8("priority"),
        FieldSpec::U8("pIndex"),
        FieldSpec::U32("msgLen"),
        FieldSpec::U32("reservedMost150Message3"),
        FieldSpec::DynamicBytes("msg", "msgLen"),
    ];
    &SCHEMA
}

fn most150_pkt_schema() -> &'static [FieldSpec] {
    static SCHEMA: [FieldSpec; 17] = [
        FieldSpec::U16("channel"),
        FieldSpec::U8("dir"),
        FieldSpec::U8("reservedMost150Pkt1"),
        FieldSpec::U32("sourceAdr"),
        FieldSpec::U32("destAdr"),
        FieldSpec::U8("transferType"),
        FieldSpec::U8("state"),
        FieldSpec::U8("ackNack"),
        FieldSpec::U8("reservedMost150Pkt2"),
        FieldSpec::U32("crc"),
        FieldSpec::U8("pAck"),
        FieldSpec::U8("cAck"),
        FieldSpec::U8("priority"),
        FieldSpec::U8("pIndex"),
        FieldSpec::U32("pktDataLength"),
        FieldSpec::U32("reservedMost150Pkt3"),
        FieldSpec::DynamicBytes("pktData", "pktDataLength"),
    ];
    &SCHEMA
}

fn most150_message_fragment_schema() -> &'static [FieldSpec] {
    static SCHEMA: [FieldSpec; 16] = [
        FieldSpec::U16("channel"),
        FieldSpec::U8("reservedMost150MessageFragment1"),
        FieldSpec::U8("ackNack"),
        FieldSpec::U32("validMask"),
        FieldSpec::U32("sourceAdr"),
        FieldSpec::U32("destAdr"),
        FieldSpec::U8("pAck"),
        FieldSpec::U8("cAck"),
        FieldSpec::U8("priority"),
        FieldSpec::U8("pIndex"),
        FieldSpec::U32("crc"),
        FieldSpec::U32("dataLen"),
        FieldSpec::U32("dataLenAnnounced"),
        FieldSpec::U32("firstDataLen"),
        FieldSpec::U32("reservedMost150MessageFragment2"),
        FieldSpec::DynamicBytes("firstData", "firstDataLen"),
    ];
    &SCHEMA
}

fn most150_pkt_fragment_schema() -> &'static [FieldSpec] {
    static SCHEMA: [FieldSpec; 16] = [
        FieldSpec::U16("channel"),
        FieldSpec::U8("reservedMost150PktFragment1"),
        FieldSpec::U8("ackNack"),
        FieldSpec::U32("validMask"),
        FieldSpec::U32("sourceAdr"),
        FieldSpec::U32("destAdr"),
        FieldSpec::U8("pAck"),
        FieldSpec::U8("cAck"),
        FieldSpec::U8("priority"),
        FieldSpec::U8("pIndex"),
        FieldSpec::U32("crc"),
        FieldSpec::U32("dataLen"),
        FieldSpec::U32("dataLenAnnounced"),
        FieldSpec::U32("firstDataLen"),
        FieldSpec::U32("reservedMost150PktFragment2"),
        FieldSpec::DynamicBytes("firstData", "firstDataLen"),
    ];
    &SCHEMA
}

fn most_ethernet_pkt_schema() -> &'static [FieldSpec] {
    use FieldSpec::*;
    static SCHEMA: [FieldSpec; 17] = [
        U16("channel"),
        U8("dir"),
        U8("reservedMostEthernetPkt1"),
        U32("reservedMostEthernetPkt2"),
        U64("sourceMacAdr"),
        U64("destMacAdr"),
        U8("transferType"),
        U8("state"),
        U8("ackNack"),
        U8("reservedMostEthernetPkt3"),
        U32("crc"),
        U8("pAck"),
        U8("cAck"),
        U16("reservedMostEthernetPkt4"),
        U32("pktDataLength"),
        U64("reservedMostEthernetPkt5"),
        DynamicBytes("pktData", "pktDataLength"),
    ];
    &SCHEMA
}

fn most_ethernet_fragment_schema() -> &'static [FieldSpec] {
    use FieldSpec::*;
    static SCHEMA: [FieldSpec; 15] = [
        U16("channel"),
        U8("reservedMostEthernetPktFragment1"),
        U8("ackNack"),
        U32("validMask"),
        U64("sourceMacAdr"),
        U64("destMacAdr"),
        U8("pAck"),
        U8("cAck"),
        U16("reservedMostEthernetPktFragment2"),
        U32("crc"),
        U32("dataLen"),
        U32("dataLenAnnounced"),
        U32("firstDataLen"),
        U32("reservedMostEthernetPktFragment3"),
        DynamicBytes("firstData", "firstDataLen"),
    ];
    &SCHEMA
}

fn most50_message_schema() -> &'static [FieldSpec] {
    static SCHEMA: [FieldSpec; 16] = [
        FieldSpec::U16("channel"),
        FieldSpec::U8("dir"),
        FieldSpec::U8("reservedMost50Message1"),
        FieldSpec::U32("sourceAdr"),
        FieldSpec::U32("destAdr"),
        FieldSpec::U8("transferType"),
        FieldSpec::U8("state"),
        FieldSpec::U8("ackNack"),
        FieldSpec::U8("reservedMost50Message2"),
        FieldSpec::U32("crc"),
        FieldSpec::U16("reservedMost50Message3"),
        FieldSpec::U8("priority"),
        FieldSpec::U8("reservedMost50Message4"),
        FieldSpec::U32("msgLen"),
        FieldSpec::U32("reservedMost50Message5"),
        FieldSpec::DynamicBytes("msg", "msgLen"),
    ];
    &SCHEMA
}

fn most50_pkt50_schema() -> &'static [FieldSpec] {
    static SCHEMA: [FieldSpec; 16] = [
        FieldSpec::U16("channel"),
        FieldSpec::U8("dir"),
        FieldSpec::U8("reservedMost50Pkt1"),
        FieldSpec::U32("sourceAdr"),
        FieldSpec::U32("destAdr"),
        FieldSpec::U8("transferType"),
        FieldSpec::U8("state"),
        FieldSpec::U8("ackNack"),
        FieldSpec::U8("reservedMost50Pkt2"),
        FieldSpec::U32("crc"),
        FieldSpec::U16("reservedMost50Pkt3"),
        FieldSpec::U8("priority"),
        FieldSpec::U8("reservedMost50Pkt4"),
        FieldSpec::U32("pktDataLength"),
        FieldSpec::U32("reservedMost50Pkt5"),
        FieldSpec::DynamicBytes("pktData", "pktDataLength"),
    ];
    &SCHEMA
}

fn flexray_receive_schema() -> &'static [FieldSpec] {
    use FieldSpec::*;
    static SCHEMA: [FieldSpec; 22] = [
        U16("channel"),
        U16("version"),
        U16("channelMask"),
        U8("dir"),
        U8("reservedFlexRayVFrReceiveMsg1"),
        U32("clientIndexFlexRayVFrReceiveMsg"),
        U32("clusterNo"),
        U16("frameId"),
        U16("headerCrc1"),
        U16("headerCrc2"),
        U16("byteCount"),
        U16("dataCount"),
        U8("cycle"),
        U8("reservedFlexRayVFrReceiveMsg2"),
        U32("tag"),
        U32("data"),
        U32("frameFlags"),
        U32("appParameter"),
        Bytes("dataBytes", 254),
        U16("reservedFlexRayVFrReceiveMsg3"),
        U32("reservedFlexRayVFrReceiveMsg4"),
        Bytes("reservedFlexRayVFrReceiveMsgExtension", 0),
    ];
    &SCHEMA[..21]
}

fn flexray_receive_ex_schema() -> &'static [FieldSpec] {
    use FieldSpec::*;
    static SCHEMA: [FieldSpec; 24] = [
        U16("channel"),
        U16("version"),
        U16("channelMask"),
        U16("dir"),
        U32("clientIndexFlexRayVFrReceiveMsgEx"),
        U32("clusterNo"),
        U16("frameId"),
        U16("headerCrc1"),
        U16("headerCrc2"),
        U16("byteCount"),
        U16("dataCount"),
        U16("cycle"),
        U32("tag"),
        U32("data"),
        U32("frameFlags"),
        U32("appParameter"),
        U32("frameCrc"),
        U32("frameLengthNs"),
        U16("frameId1"),
        U16("pduOffset"),
        U16("blfLogMask"),
        U16Vec("reservedFlexRayVFrReceiveMsgEx1", 13),
        DynamicBytes("dataBytes", "dataCount"),
        RemainingBytes("reservedFlexRayVFrReceiveMsgEx2"),
    ];
    &SCHEMA
}

fn ethernet_frame_ex_schema() -> &'static [FieldSpec] {
    static FRAME: [FieldSpec; 11] = [
        FieldSpec::U16("structLength"),
        FieldSpec::U16("flags"),
        FieldSpec::U16("channel"),
        FieldSpec::U16("hardwareChannel"),
        FieldSpec::U64("frameDuration"),
        FieldSpec::U32("frameChecksum"),
        FieldSpec::U16("dir"),
        FieldSpec::U16("frameLength"),
        FieldSpec::U32("frameHandle"),
        FieldSpec::U32("reservedEthernetFrameEx"),
        FieldSpec::DynamicBytes("frameData", "frameLength"),
    ];
    &FRAME
}

fn ethernet_frame_forwarded_schema() -> &'static [FieldSpec] {
    static FRAME: [FieldSpec; 11] = [
        FieldSpec::U16("structLength"),
        FieldSpec::U16("flags"),
        FieldSpec::U16("channel"),
        FieldSpec::U16("hardwareChannel"),
        FieldSpec::U64("frameDuration"),
        FieldSpec::U32("frameChecksum"),
        FieldSpec::U16("dir"),
        FieldSpec::U16("frameLength"),
        FieldSpec::U32("frameHandle"),
        FieldSpec::U32("reservedEthernetFrameForwarded"),
        FieldSpec::DynamicBytes("frameData", "frameLength"),
    ];
    &FRAME
}

fn ethernet_error_ex_schema() -> &'static [FieldSpec] {
    static FRAME: [FieldSpec; 11] = [
        FieldSpec::U16("structLength"),
        FieldSpec::U16("flags"),
        FieldSpec::U16("channel"),
        FieldSpec::U16("hardwareChannel"),
        FieldSpec::U64("frameDuration"),
        FieldSpec::U32("frameChecksum"),
        FieldSpec::U16("dir"),
        FieldSpec::U16("frameLength"),
        FieldSpec::U32("frameHandle"),
        FieldSpec::U32("error"),
        FieldSpec::DynamicBytes("frameData", "frameLength"),
    ];
    &FRAME
}

fn afdx_statistic_schema() -> &'static [FieldSpec] {
    use FieldSpec::*;
    static SCHEMA: [FieldSpec; 15] = [
        U16("channel"),
        U16("flags"),
        U32("rxPacketCount"),
        U32("rxByteCount"),
        U32("txPacketCount"),
        U32("txByteCount"),
        U32("collisionCount"),
        U32("errorCount"),
        U32("statDroppedRedundantPacketCount"),
        U32("statRedundantErrorPacketCount"),
        U32("statIntegrityErrorPacketCount"),
        U32("statAvrgPeriodMsec"),
        U32("statAvrgJitterMysec"),
        U32("vlid"),
        U32("statDuration"),
    ];
    &SCHEMA
}

fn afdx_bus_statistic_schema() -> &'static [FieldSpec] {
    use FieldSpec::*;
    static SCHEMA: [FieldSpec; 21] = [
        U16("channel"),
        U16("flags"),
        U32("statDuration"),
        U32("statRxPacketCountHW"),
        U32("statTxPacketCountHW"),
        U32("statRxErrorCountHW"),
        U32("statTxErrorCountHW"),
        U32("statRxBytesHW"),
        U32("statTxBytesHW"),
        U32("statRxPacketCount"),
        U32("statTxPacketCount"),
        U32("statDroppedPacketCount"),
        U32("statInvalidPacketCount"),
        U32("statLostPacketCount"),
        U8("line"),
        U8("linkStatus"),
        U16("linkSpeed"),
        U16("linkLost"),
        U16("reservedAfdxBusStatistic1"),
        U32("reservedAfdxBusStatistic2"),
        U8("reservedAfdxBusStatisticExtension"),
    ];
    &SCHEMA[..20]
}

fn afdx_status_schema() -> &'static [FieldSpec] {
    use FieldSpec::*;
    static SCHEMA: [FieldSpec; 26] = [
        U16("channel"),
        U16("reservedAfdxStatus1"),
        U16("statusA_flags"),
        U8("statusA_linkStatus"),
        U8("statusA_ethernetPhy"),
        U8("statusA_duplex"),
        U8("statusA_mdi"),
        U8("statusA_connector"),
        U8("statusA_clockMode"),
        U8("statusA_pairs"),
        U8("statusA_reservedAfdxLineStatus1"),
        U16("statusA_reservedAfdxLineStatus2"),
        U32("statusA_bitrate"),
        U16("statusB_flags"),
        U8("statusB_linkStatus"),
        U8("statusB_ethernetPhy"),
        U8("statusB_duplex"),
        U8("statusB_mdi"),
        U8("statusB_connector"),
        U8("statusB_clockMode"),
        U8("statusB_pairs"),
        U8("statusB_reservedAfdxLineStatus1"),
        U16("statusB_reservedAfdxLineStatus2"),
        U32("statusB_bitrate"),
        U32("reservedAfdxStatus2"),
        U8("reservedAfdxStatusExtension"),
    ];
    &SCHEMA[..25]
}

#[allow(dead_code)]
fn parse_var_object_header(data: &[u8], offset: usize) -> Result<VarObjectHeader> {
    let base = parse_object_header_base(data, offset)?;
    Ok(VarObjectHeader {
        base,
        object_flags: read_u32(data, checked_offset(offset, 16)?)?,
        object_static_size: read_u16(data, checked_offset(offset, 20)?)?,
        object_version: read_u16(data, checked_offset(offset, 22)?)?,
        object_time_stamp: read_u64(data, checked_offset(offset, 24)?)?,
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
        raw_data: data.to_vec(),
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
    let data_end = checked_offset(data_start, data_len)?;
    let payload = data
        .get(data_start..data_end)
        .ok_or(BlfError::Truncated)?
        .to_vec();
    let tail = data_end;
    let bit_count_offset = checked_offset(tail, 4)?;
    let reserved_one_offset = checked_offset(tail, 5)?;
    let reserved_two_offset = checked_offset(tail, 6)?;
    Ok(CanMessage2 {
        header,
        channel: read_u16(data, 32)?,
        flags: *data.get(34).ok_or(BlfError::Truncated)?,
        dlc: *data.get(35).ok_or(BlfError::Truncated)?,
        id: read_u32(data, 36)?,
        data: payload,
        frame_length: read_u32(data, tail)?,
        bit_count: *data.get(bit_count_offset).ok_or(BlfError::Truncated)?,
        reserved_can_message1: *data.get(reserved_one_offset).ok_or(BlfError::Truncated)?,
        reserved_can_message2: read_u16(data, reserved_two_offset)?,
        raw_data: data.to_vec(),
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
        raw_data: data.to_vec(),
    })
}

fn parse_can_fd_message64(data: &[u8], base: ObjectHeaderBase) -> Result<CanFdMessage64> {
    let header = parse_object_header(data, 0)?;
    let valid_data_bytes = *data.get(34).ok_or(BlfError::Truncated)? as usize;
    let fixed_before_data = 40usize;
    let data_start = checked_offset(32, fixed_before_data)?;
    let data_end = checked_offset(data_start, valid_data_bytes)?;
    let payload = data
        .get(data_start..data_end)
        .ok_or(BlfError::Truncated)?
        .to_vec();

    let ext_data_offset = *data.get(67).ok_or(BlfError::Truncated)? as usize;
    let object_end = usize::try_from(base.object_size)
        .map_err(|_| BlfError::Format("CanFdMessage64 size does not fit usize".to_string()))?;
    let ext_data_end = ext_data_offset.checked_add(8);
    let has_ext_data = ext_data_offset != 0 && ext_data_end.is_some_and(|end| object_end >= end);
    let mut ext_data = None;
    let mut reserved_can_fd_ext_frame_data = Vec::new();
    if has_ext_data {
        let ext_start = data_end;
        let btr_ext_arb = read_u32(data, ext_start)?;
        let btr_ext_data = read_u32(data, checked_offset(ext_start, 4)?)?;
        let reserved_start = checked_offset(ext_start, 8)?;
        let reserved_end = object_end;
        let reserved = data
            .get(reserved_start..reserved_end)
            .ok_or(BlfError::Truncated)?
            .to_vec();
        ext_data = Some(CanFdExtFrameData {
            btr_ext_arb,
            btr_ext_data,
            reserved_can_fd_ext_frame_data: reserved,
        });
    } else {
        reserved_can_fd_ext_frame_data = data
            .get(data_end..object_end)
            .ok_or(BlfError::Truncated)?
            .to_vec();
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
        reserved_can_fd_ext_frame_data,
        raw_data: data.to_vec(),
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
        raw_data: data.to_vec(),
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
    let data_end = checked_offset(data_start, data_len)?;
    let payload = data
        .get(data_start..data_end)
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
        raw_data: data.to_vec(),
    })
}

fn parse_can_fd_error_frame64(data: &[u8], base: ObjectHeaderBase) -> Result<CanFdErrorFrame64> {
    let header = parse_object_header(data, 0)?;
    let valid_data_bytes = *data.get(34).ok_or(BlfError::Truncated)? as usize;
    let fixed_before_data = 44usize;
    let data_start = checked_offset(32, fixed_before_data)?;
    let data_end = checked_offset(data_start, valid_data_bytes)?;
    let payload = data
        .get(data_start..data_end)
        .ok_or(BlfError::Truncated)?
        .to_vec();

    let ext_data_offset = *data.get(42).ok_or(BlfError::Truncated)? as usize;
    let object_end = usize::try_from(base.object_size)
        .map_err(|_| BlfError::Format("CanFdErrorFrame64 size does not fit usize".to_string()))?;
    let ext_data_end = ext_data_offset.checked_add(8);
    let has_ext_data = ext_data_offset != 0 && ext_data_end.is_some_and(|end| object_end >= end);
    let mut ext_data = None;
    let mut reserved_can_fd_ext_frame_data = Vec::new();
    if has_ext_data {
        let ext_start = data_end;
        let btr_ext_arb = read_u32(data, ext_start)?;
        let btr_ext_data = read_u32(data, checked_offset(ext_start, 4)?)?;
        let reserved = data
            .get(checked_offset(ext_start, 8)?..object_end)
            .ok_or(BlfError::Truncated)?
            .to_vec();
        ext_data = Some(CanFdExtFrameData {
            btr_ext_arb,
            btr_ext_data,
            reserved_can_fd_ext_frame_data: reserved,
        });
    } else {
        reserved_can_fd_ext_frame_data = data
            .get(data_end..object_end)
            .ok_or(BlfError::Truncated)?
            .to_vec();
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
        reserved_can_fd_ext_frame_data,
        raw_data: data.to_vec(),
    })
}

fn parse_app_text(data: &[u8], _base: ObjectHeaderBase) -> Result<AppText> {
    let header = parse_object_header(data, 0)?;
    let text_length = read_u32(data, 40)? as usize;
    let text_start = 48usize;
    let text = data
        .get(text_start..checked_offset(text_start, text_length)?)
        .ok_or(BlfError::Truncated)?
        .to_vec();
    Ok(AppText {
        header,
        source: read_u32(data, 32)?,
        reserved_app_text1: read_u32(data, 36)?,
        text_length: text_length as u32,
        reserved_app_text2: read_u32(data, 44)?,
        text,
        raw_data: data.to_vec(),
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
        .get(48..checked_offset(48, data_length as usize)?)
        .ok_or(BlfError::Truncated)?
        .to_vec();
    let restore_points = parse_restore_points(&payload)?;
    Ok(RestorePointContainer {
        header,
        reserved_restore_point_container: reserved,
        data_length,
        data: payload,
        restore_points,
        raw_data: data.to_vec(),
    })
}

fn parse_restore_points(data: &[u8]) -> Result<Option<crate::types::RestorePoints>> {
    if data.is_empty() {
        return Ok(None);
    }
    if data.len() < 8 {
        // Object type 115 is reserved in vector_blf. Some Vector writers use
        // it for restore points, while others leave an opaque payload here.
        // Keep the latter readable and expose the bytes through `data`.
        return Ok(None);
    }

    let object_size = read_u32(data, 0)?;
    let object_size_usize = usize::try_from(object_size).map_err(|_| {
        BlfError::Format("restore point object size does not fit usize".to_string())
    })?;
    if object_size_usize < 8 || object_size_usize > data.len() {
        return Ok(None);
    }

    let point_bytes = object_size_usize - 8;
    if point_bytes % 24 != 0 {
        return Ok(None);
    }

    let mut restore_points = Vec::with_capacity(point_bytes / 24);
    for offset in (8..object_size_usize).step_by(24) {
        restore_points.push(crate::types::RestorePoint {
            time_stamp: read_u64(data, offset)?,
            compressed_file_position: read_u64(data, checked_offset(offset, 8)?)?,
            uncompressed_file_offset: read_u32(data, checked_offset(offset, 16)?)?,
            unknown_restore_point: read_u32(data, checked_offset(offset, 20)?)?,
        });
    }

    Ok(Some(crate::types::RestorePoints {
        object_size,
        object_interval: read_u32(data, 4)?,
        restore_points,
    }))
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

fn is_reserved_type(object_type: ObjectType) -> bool {
    matches!(
        object_type,
        ObjectType::Reserved26
            | ObjectType::Reserved27
            | ObjectType::Reserved28
            | ObjectType::Reserved52
            | ObjectType::Reserved53
            | ObjectType::Reserved108
            | ObjectType::Reserved116
            | ObjectType::Reserved117
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
    *offset = checked_offset(*offset, 1)?;
    Ok(())
}

fn push_u16_field(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    fields.push((name.to_string(), FieldValue::U16(read_u16(data, *offset)?)));
    *offset = checked_offset(*offset, 2)?;
    Ok(())
}

fn push_u32_field(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    fields.push((name.to_string(), FieldValue::U32(read_u32(data, *offset)?)));
    *offset = checked_offset(*offset, 4)?;
    Ok(())
}

fn push_u64_field(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    fields.push((name.to_string(), FieldValue::U64(read_u64(data, *offset)?)));
    *offset = checked_offset(*offset, 8)?;
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
    *offset = checked_offset(*offset, 4)?;
    Ok(())
}

fn push_f64_field(
    data: &[u8],
    offset: &mut usize,
    fields: &mut Vec<(String, FieldValue)>,
    name: &str,
) -> Result<()> {
    let value = data_range(data, *offset, 8)?;
    fields.push((
        name.to_string(),
        FieldValue::F64(f64::from_le_bytes(value.try_into().expect("fixed 8 bytes"))),
    ));
    *offset = checked_offset(*offset, 8)?;
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
        FieldValue::Bytes(data_range(data, *offset, len)?.to_vec()),
    ));
    *offset = checked_offset(*offset, len)?;
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
        let byte_offset = index.checked_mul(2).ok_or(BlfError::Truncated)?;
        values.push(read_u16(data, checked_offset(*offset, byte_offset)?)?);
    }
    fields.push((name.to_string(), FieldValue::U16Vec(values)));
    *offset = checked_offset(*offset, len.checked_mul(2).ok_or(BlfError::Truncated)?)?;
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
        let byte_offset = index.checked_mul(8).ok_or(BlfError::Truncated)?;
        values.push(read_u64(data, checked_offset(*offset, byte_offset)?)?);
    }
    fields.push((name.to_string(), FieldValue::U64Vec(values)));
    *offset = checked_offset(*offset, len.checked_mul(8).ok_or(BlfError::Truncated)?)?;
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
    let mut offset = usize::from(base.header_size);

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
            if data_range(data, offset, 4).is_ok() {
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
            if data.len().saturating_sub(offset) >= 4 {
                push_u32_field(data, &mut offset, &mut fields, "respBaudrate")?;
            }
            if data.len().saturating_sub(offset) >= 16 {
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
            if data_range(data, offset, 4).is_ok() {
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
        extensions: data.get(offset..).ok_or(BlfError::Truncated)?.to_vec(),
        raw_data: data.to_vec(),
    })
}
