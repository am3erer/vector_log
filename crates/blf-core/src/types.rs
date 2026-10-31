use crate::error::{BlfError, Result};

pub const FILE_SIGNATURE: u32 = 0x4747_4F4C; // LOGG
pub const OBJECT_SIGNATURE: u32 = 0x4A42_4F4C; // LOBJ

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ObjectType {
    Unknown = 0,
    CanMessage = 1,
    CanError = 2,
    CanOverload = 3,
    CanStatistic = 4,
    AppTrigger = 5,
    EnvInteger = 6,
    EnvDouble = 7,
    EnvString = 8,
    EnvData = 9,
    LogContainer = 10,
    LinMessage = 11,
    LinCrcError = 12,
    LinDlcInfo = 13,
    LinRcvError = 14,
    LinSndError = 15,
    LinSlvTimeout = 16,
    LinSchedModch = 17,
    LinSynError = 18,
    LinBaudrate = 19,
    LinSleep = 20,
    LinWakeup = 21,
    MostSpy = 22,
    MostCtrl = 23,
    MostLightlock = 24,
    MostStatistic = 25,
    FlexrayData = 29,
    FlexraySync = 30,
    CanDriverError = 31,
    MostPkt = 32,
    MostPkt2 = 33,
    MostHwmode = 34,
    MostReg = 35,
    MostGenreg = 36,
    MostNetstate = 37,
    MostDatalost = 38,
    MostTrigger = 39,
    FlexrayCycle = 40,
    FlexrayMessage = 41,
    LinChecksumInfo = 42,
    LinSpikeEvent = 43,
    CanDriverSync = 44,
    FlexrayStatus = 45,
    GpsEvent = 46,
    FrError = 47,
    FrStatus = 48,
    FrStartcycle = 49,
    FrRcvmessage = 50,
    Realtimeclock = 51,
    LinStatistic = 54,
    J1708Message = 55,
    J1708VirtualMsg = 56,
    LinMessage2 = 57,
    LinSndError2 = 58,
    LinSynError2 = 59,
    LinCrcError2 = 60,
    LinRcvError2 = 61,
    LinWakeup2 = 62,
    LinSpikeEvent2 = 63,
    LinLongDomSig = 64,
    AppText = 65,
    FrRcvmessageEx = 66,
    MostStatisticex = 67,
    MostTxlight = 68,
    MostAlloctab = 69,
    MostStress = 70,
    EthernetFrame = 71,
    SysVariable = 72,
    CanErrorExt = 73,
    CanDriverErrorExt = 74,
    LinLongDomSig2 = 75,
    Most150Message = 76,
    Most150Pkt = 77,
    MostEthernetPkt = 78,
    Most150MessageFragment = 79,
    Most150PktFragment = 80,
    MostEthernetPktFragment = 81,
    MostSystemEvent = 82,
    Most150Alloctab = 83,
    Most50Message = 84,
    Most50Pkt = 85,
    CanMessage2 = 86,
    LinUnexpectedWakeup = 87,
    LinShortOrSlowResponse = 88,
    LinDisturbanceEvent = 89,
    SerialEvent = 90,
    OverrunError = 91,
    EventComment = 92,
    WlanFrame = 93,
    WlanStatistic = 94,
    MostEcl = 95,
    GlobalMarker = 96,
    AfdxFrame = 97,
    AfdxStatistic = 98,
    KlineStatusEvent = 99,
    CanFdMessage = 100,
    CanFdMessage64 = 101,
    EthernetRxError = 102,
    EthernetStatus = 103,
    CanFdError64 = 104,
    LinShortOrSlowResponse2 = 105,
    AfdxStatus = 106,
    AfdxBusStatistic = 107,
    AfdxErrorEvent = 109,
    A429Error = 110,
    A429Status = 111,
    A429BusStatistic = 112,
    A429Message = 113,
    EthernetStatistic = 114,
    Unknown115 = 115,
    TestStructure = 118,
    DiagRequestInterpretation = 119,
    EthernetFrameEx = 120,
    EthernetFrameForwarded = 121,
    EthernetErrorEx = 122,
    EthernetErrorForwarded = 123,
    FunctionBus = 124,
    DataLostBegin = 125,
    DataLostEnd = 126,
    WaterMarkEvent = 127,
    TriggerCondition = 128,
    CanSettingChanged = 129,
    DistributedObjectMember = 130,
    AttributeEvent = 131,
}

impl ObjectType {
    pub fn from_raw(value: u32) -> Self {
        match value {
            1 => Self::CanMessage,
            2 => Self::CanError,
            3 => Self::CanOverload,
            4 => Self::CanStatistic,
            5 => Self::AppTrigger,
            6 => Self::EnvInteger,
            7 => Self::EnvDouble,
            8 => Self::EnvString,
            9 => Self::EnvData,
            10 => Self::LogContainer,
            11 => Self::LinMessage,
            12 => Self::LinCrcError,
            13 => Self::LinDlcInfo,
            14 => Self::LinRcvError,
            15 => Self::LinSndError,
            16 => Self::LinSlvTimeout,
            17 => Self::LinSchedModch,
            18 => Self::LinSynError,
            19 => Self::LinBaudrate,
            20 => Self::LinSleep,
            21 => Self::LinWakeup,
            22 => Self::MostSpy,
            23 => Self::MostCtrl,
            24 => Self::MostLightlock,
            25 => Self::MostStatistic,
            29 => Self::FlexrayData,
            30 => Self::FlexraySync,
            31 => Self::CanDriverError,
            32 => Self::MostPkt,
            33 => Self::MostPkt2,
            34 => Self::MostHwmode,
            35 => Self::MostReg,
            36 => Self::MostGenreg,
            37 => Self::MostNetstate,
            38 => Self::MostDatalost,
            39 => Self::MostTrigger,
            40 => Self::FlexrayCycle,
            41 => Self::FlexrayMessage,
            42 => Self::LinChecksumInfo,
            43 => Self::LinSpikeEvent,
            44 => Self::CanDriverSync,
            45 => Self::FlexrayStatus,
            46 => Self::GpsEvent,
            47 => Self::FrError,
            48 => Self::FrStatus,
            49 => Self::FrStartcycle,
            50 => Self::FrRcvmessage,
            51 => Self::Realtimeclock,
            54 => Self::LinStatistic,
            55 => Self::J1708Message,
            56 => Self::J1708VirtualMsg,
            57 => Self::LinMessage2,
            58 => Self::LinSndError2,
            59 => Self::LinSynError2,
            60 => Self::LinCrcError2,
            61 => Self::LinRcvError2,
            62 => Self::LinWakeup2,
            63 => Self::LinSpikeEvent2,
            64 => Self::LinLongDomSig,
            65 => Self::AppText,
            66 => Self::FrRcvmessageEx,
            67 => Self::MostStatisticex,
            68 => Self::MostTxlight,
            69 => Self::MostAlloctab,
            70 => Self::MostStress,
            71 => Self::EthernetFrame,
            72 => Self::SysVariable,
            73 => Self::CanErrorExt,
            74 => Self::CanDriverErrorExt,
            75 => Self::LinLongDomSig2,
            76 => Self::Most150Message,
            77 => Self::Most150Pkt,
            78 => Self::MostEthernetPkt,
            79 => Self::Most150MessageFragment,
            80 => Self::Most150PktFragment,
            81 => Self::MostEthernetPktFragment,
            82 => Self::MostSystemEvent,
            83 => Self::Most150Alloctab,
            84 => Self::Most50Message,
            85 => Self::Most50Pkt,
            86 => Self::CanMessage2,
            87 => Self::LinUnexpectedWakeup,
            88 => Self::LinShortOrSlowResponse,
            89 => Self::LinDisturbanceEvent,
            90 => Self::SerialEvent,
            91 => Self::OverrunError,
            92 => Self::EventComment,
            93 => Self::WlanFrame,
            94 => Self::WlanStatistic,
            95 => Self::MostEcl,
            96 => Self::GlobalMarker,
            97 => Self::AfdxFrame,
            98 => Self::AfdxStatistic,
            99 => Self::KlineStatusEvent,
            100 => Self::CanFdMessage,
            101 => Self::CanFdMessage64,
            102 => Self::EthernetRxError,
            103 => Self::EthernetStatus,
            104 => Self::CanFdError64,
            105 => Self::LinShortOrSlowResponse2,
            106 => Self::AfdxStatus,
            107 => Self::AfdxBusStatistic,
            109 => Self::AfdxErrorEvent,
            110 => Self::A429Error,
            111 => Self::A429Status,
            112 => Self::A429BusStatistic,
            113 => Self::A429Message,
            114 => Self::EthernetStatistic,
            115 => Self::Unknown115,
            118 => Self::TestStructure,
            119 => Self::DiagRequestInterpretation,
            120 => Self::EthernetFrameEx,
            121 => Self::EthernetFrameForwarded,
            122 => Self::EthernetErrorEx,
            123 => Self::EthernetErrorForwarded,
            124 => Self::FunctionBus,
            125 => Self::DataLostBegin,
            126 => Self::DataLostEnd,
            127 => Self::WaterMarkEvent,
            128 => Self::TriggerCondition,
            129 => Self::CanSettingChanged,
            130 => Self::DistributedObjectMember,
            131 => Self::AttributeEvent,
            _ => Self::Unknown,
        }
    }

    pub fn as_u32(self) -> u32 {
        self as u32
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemTime {
    pub year: u16,
    pub month: u16,
    pub day_of_week: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub milliseconds: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStatistics {
    pub signature: u32,
    pub statistics_size: u32,
    pub api_number: u32,
    pub application_id: u8,
    pub compression_level: u8,
    pub application_major: u8,
    pub application_minor: u8,
    pub file_size: u64,
    pub uncompressed_file_size: u64,
    pub object_count: u32,
    pub application_build: u32,
    pub measurement_start_time: SystemTime,
    pub last_object_time: SystemTime,
    pub restore_points_offset: u64,
    pub reserved_file_statistics: [u32; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectHeaderBase {
    pub signature: u32,
    pub header_size: u16,
    pub header_version: u16,
    pub object_size: u32,
    pub object_type: ObjectType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectHeader {
    pub base: ObjectHeaderBase,
    pub object_flags: u32,
    pub client_index: u16,
    pub object_version: u16,
    pub object_time_stamp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectHeader2 {
    pub base: ObjectHeaderBase,
    pub object_flags: u32,
    pub time_stamp_status: u8,
    pub reserved_object_header: u8,
    pub object_version: u16,
    pub object_time_stamp: u64,
    pub original_time_stamp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarObjectHeader {
    pub base: ObjectHeaderBase,
    pub object_flags: u32,
    pub object_static_size: u16,
    pub object_version: u16,
    pub object_time_stamp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanMessage {
    pub header: ObjectHeader,
    pub channel: u16,
    pub flags: u8,
    pub dlc: u8,
    pub id: u32,
    pub data: [u8; 8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanMessage2 {
    pub header: ObjectHeader,
    pub channel: u16,
    pub flags: u8,
    pub dlc: u8,
    pub id: u32,
    pub data: Vec<u8>,
    pub frame_length: u32,
    pub bit_count: u8,
    pub reserved_can_message1: u8,
    pub reserved_can_message2: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanFdMessage {
    pub header: ObjectHeader,
    pub channel: u16,
    pub flags: u8,
    pub dlc: u8,
    pub id: u32,
    pub frame_length: u32,
    pub arb_bit_count: u8,
    pub can_fd_flags: u8,
    pub valid_data_bytes: u8,
    pub reserved_can_fd_message1: u8,
    pub reserved_can_fd_message2: u32,
    pub data: [u8; 64],
    pub reserved_can_fd_message3: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CanFdExtFrameData {
    pub btr_ext_arb: u32,
    pub btr_ext_data: u32,
    pub reserved_can_fd_ext_frame_data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanFdMessage64 {
    pub header: ObjectHeader,
    pub channel: u8,
    pub dlc: u8,
    pub valid_data_bytes: u8,
    pub tx_count: u8,
    pub id: u32,
    pub frame_length: u32,
    pub flags: u32,
    pub btr_cfg_arb: u32,
    pub btr_cfg_data: u32,
    pub time_offset_brs_ns: u32,
    pub time_offset_crc_del_ns: u32,
    pub bit_count: u16,
    pub dir: u8,
    pub ext_data_offset: u8,
    pub crc: u32,
    pub data: Vec<u8>,
    pub ext_data: Option<CanFdExtFrameData>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanErrorFrame {
    pub header: ObjectHeader,
    pub channel: u16,
    pub length: u16,
    pub reserved_can_error_frame: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanErrorFrameExt {
    pub header: ObjectHeader,
    pub channel: u16,
    pub length: u16,
    pub flags: u32,
    pub ecc: u8,
    pub position: u8,
    pub dlc: u8,
    pub reserved_can_error_frame_ext1: u8,
    pub frame_length_in_ns: u32,
    pub id: u32,
    pub flags_ext: u16,
    pub reserved_can_error_frame_ext2: u16,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanFdErrorFrame64 {
    pub header: ObjectHeader,
    pub channel: u8,
    pub dlc: u8,
    pub valid_data_bytes: u8,
    pub ecc: u8,
    pub flags: u16,
    pub error_code_ext: u16,
    pub ext_flags: u16,
    pub ext_data_offset: u8,
    pub reserved_can_fd_error_frame1: u8,
    pub id: u32,
    pub frame_length: u32,
    pub btr_cfg_arb: u32,
    pub btr_cfg_data: u32,
    pub time_offset_brs_ns: u32,
    pub time_offset_crc_del_ns: u32,
    pub crc: u32,
    pub error_position: u16,
    pub reserved_can_fd_error_frame2: u16,
    pub data: Vec<u8>,
    pub ext_data: Option<CanFdExtFrameData>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppText {
    pub header: ObjectHeader,
    pub source: u32,
    pub reserved_app_text1: u32,
    pub text_length: u32,
    pub reserved_app_text2: u32,
    pub text: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePoint {
    pub time_stamp: u64,
    pub compressed_file_position: u64,
    pub uncompressed_file_offset: u32,
    pub unknown_restore_point: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePointContainer {
    pub header: ObjectHeader,
    pub reserved_restore_point_container: [u8; 14],
    pub data_length: u16,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePoints {
    pub object_size: u32,
    pub object_interval: u32,
    pub restore_points: Vec<RestorePoint>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    I32(i32),
    F64(f64),
    Bytes(Vec<u8>),
    U16Vec(Vec<u16>),
    U64Vec(Vec<u64>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinObject {
    pub object_type: ObjectType,
    pub header: ObjectHeader,
    pub fields: Vec<(String, FieldValue)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Object {
    CanMessage(CanMessage),
    CanMessage2(CanMessage2),
    CanFdMessage(CanFdMessage),
    CanFdMessage64(CanFdMessage64),
    CanErrorFrame(CanErrorFrame),
    CanErrorFrameExt(CanErrorFrameExt),
    CanFdErrorFrame64(CanFdErrorFrame64),
    AppText(AppText),
    RestorePointContainer(RestorePointContainer),
    Lin(LinObject),
}

impl Object {
    pub fn object_type(&self) -> ObjectType {
        match self {
            Object::CanMessage(_) => ObjectType::CanMessage,
            Object::CanMessage2(_) => ObjectType::CanMessage2,
            Object::CanFdMessage(_) => ObjectType::CanFdMessage,
            Object::CanFdMessage64(_) => ObjectType::CanFdMessage64,
            Object::CanErrorFrame(_) => ObjectType::CanError,
            Object::CanErrorFrameExt(_) => ObjectType::CanErrorExt,
            Object::CanFdErrorFrame64(_) => ObjectType::CanFdError64,
            Object::AppText(_) => ObjectType::AppText,
            Object::RestorePointContainer(_) => ObjectType::Unknown115,
            Object::Lin(object) => object.object_type,
        }
    }
}

pub(crate) fn read_u16(data: &[u8], offset: usize) -> Result<u16> {
    let bytes = data.get(offset..offset + 2).ok_or(BlfError::Incomplete)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

pub(crate) fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data.get(offset..offset + 4).ok_or(BlfError::Incomplete)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

pub(crate) fn read_u64(data: &[u8], offset: usize) -> Result<u64> {
    let bytes = data.get(offset..offset + 8).ok_or(BlfError::Incomplete)?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

pub(crate) fn read_system_time(data: &[u8], offset: usize) -> Result<SystemTime> {
    Ok(SystemTime {
        year: read_u16(data, offset)?,
        month: read_u16(data, offset + 2)?,
        day_of_week: read_u16(data, offset + 4)?,
        day: read_u16(data, offset + 6)?,
        hour: read_u16(data, offset + 8)?,
        minute: read_u16(data, offset + 10)?,
        second: read_u16(data, offset + 12)?,
        milliseconds: read_u16(data, offset + 14)?,
    })
}
