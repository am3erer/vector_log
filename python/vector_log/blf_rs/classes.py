from enum import IntEnum


class ObjectType(IntEnum):
    UNKNOWN = 0
    CAN_MESSAGE = 1
    CAN_ERROR = 2
    CAN_OVERLOAD = 3
    CAN_STATISTIC = 4
    APP_TRIGGER = 5
    ENV_INTEGER = 6
    ENV_DOUBLE = 7
    ENV_STRING = 8
    ENV_DATA = 9
    LOG_CONTAINER = 10
    LIN_MESSAGE = 11
    LIN_CRC_ERROR = 12
    LIN_DLC_INFO = 13
    LIN_RCV_ERROR = 14
    LIN_SND_ERROR = 15
    LIN_SLV_TIMEOUT = 16
    LIN_SCHED_MODCH = 17
    LIN_SYN_ERROR = 18
    LIN_BAUDRATE = 19
    LIN_SLEEP = 20
    LIN_WAKEUP = 21
    MOST_SPY = 22
    MOST_CTRL = 23
    MOST_LIGHTLOCK = 24
    MOST_STATISTIC = 25
    RESERVED26 = 26
    RESERVED27 = 27
    RESERVED28 = 28
    FLEXRAY_DATA = 29
    FLEXRAY_SYNC = 30
    CAN_DRIVER_ERROR = 31
    MOST_PKT = 32
    MOST_PKT2 = 33
    MOST_HWMODE = 34
    MOST_REG = 35
    MOST_GENREG = 36
    MOST_NETSTATE = 37
    MOST_DATALOST = 38
    MOST_TRIGGER = 39
    FLEXRAY_CYCLE = 40
    FLEXRAY_MESSAGE = 41
    LIN_CHECKSUM_INFO = 42
    LIN_SPIKE_EVENT = 43
    CAN_DRIVER_SYNC = 44
    FLEXRAY_STATUS = 45
    GPS_EVENT = 46
    FR_ERROR = 47
    FR_STATUS = 48
    FR_STARTCYCLE = 49
    FR_RCVMESSAGE = 50
    REALTIMECLOCK = 51
    RESERVED52 = 52
    RESERVED53 = 53
    LIN_STATISTIC = 54
    J1708_MESSAGE = 55
    J1708_VIRTUAL_MSG = 56
    LIN_MESSAGE2 = 57
    LIN_SND_ERROR2 = 58
    LIN_SYN_ERROR2 = 59
    LIN_CRC_ERROR2 = 60
    LIN_RCV_ERROR2 = 61
    LIN_WAKEUP2 = 62
    LIN_SPIKE_EVENT2 = 63
    LIN_LONG_DOM_SIG = 64
    APP_TEXT = 65
    FR_RCVMESSAGE_EX = 66
    MOST_STATISTICEX = 67
    MOST_TXLIGHT = 68
    MOST_ALLOCTAB = 69
    MOST_STRESS = 70
    ETHERNET_FRAME = 71
    SYS_VARIABLE = 72
    CAN_ERROR_EXT = 73
    CAN_DRIVER_ERROR_EXT = 74
    LIN_LONG_DOM_SIG2 = 75
    MOST_150_MESSAGE = 76
    MOST_150_PKT = 77
    MOST_ETHERNET_PKT = 78
    MOST_150_MESSAGE_FRAGMENT = 79
    MOST_150_PKT_FRAGMENT = 80
    MOST_ETHERNET_PKT_FRAGMENT = 81
    MOST_SYSTEM_EVENT = 82
    MOST_150_ALLOCTAB = 83
    MOST_50_MESSAGE = 84
    MOST_50_PKT = 85
    CAN_MESSAGE2 = 86
    LIN_UNEXPECTED_WAKEUP = 87
    LIN_SHORT_OR_SLOW_RESPONSE = 88
    LIN_DISTURBANCE_EVENT = 89
    SERIAL_EVENT = 90
    OVERRUN_ERROR = 91
    EVENT_COMMENT = 92
    WLAN_FRAME = 93
    WLAN_STATISTIC = 94
    MOST_ECL = 95
    GLOBAL_MARKER = 96
    AFDX_FRAME = 97
    AFDX_STATISTIC = 98
    KLINE_STATUSEVENT = 99
    CAN_FD_MESSAGE = 100
    CAN_FD_MESSAGE_64 = 101
    ETHERNET_RX_ERROR = 102
    ETHERNET_STATUS = 103
    CAN_FD_ERROR_64 = 104
    LIN_SHORT_OR_SLOW_RESPONSE2 = 105
    AFDX_STATUS = 106
    AFDX_BUS_STATISTIC = 107
    RESERVED108 = 108
    AFDX_ERROR_EVENT = 109
    A429_ERROR = 110
    A429_STATUS = 111
    A429_BUS_STATISTIC = 112
    A429_MESSAGE = 113
    ETHERNET_STATISTIC = 114
    UNKNOWN115 = 115
    RESERVED116 = 116
    RESERVED117 = 117
    TEST_STRUCTURE = 118
    DIAG_REQUEST_INTERPRETATION = 119
    ETHERNET_FRAME_EX = 120
    ETHERNET_FRAME_FORWARDED = 121
    ETHERNET_ERROR_EX = 122
    ETHERNET_ERROR_FORWARDED = 123
    FUNCTION_BUS = 124
    DATA_LOST_BEGIN = 125
    DATA_LOST_END = 126
    WATER_MARK_EVENT = 127
    TRIGGER_CONDITION = 128
    CAN_SETTING_CHANGED = 129
    DISTRIBUTED_OBJECT_MEMBER = 130
    ATTRIBUTE_EVENT = 131


def _snake_to_camel(name):
    parts = name.split("_")
    if len(parts) == 1:
        return name
    return parts[0] + "".join(part[:1].upper() + part[1:] for part in parts[1:])


def _wrap_value(value):
    if isinstance(value, dict):
        return _AttributeMapping(value)
    if isinstance(value, list):
        return [_wrap_value(item) for item in value]
    return value


class _AttributeMapping:
    __slots__ = ("_data",)

    def __init__(self, data):
        object.__setattr__(self, "_data", data)

    def __getattr__(self, name):
        data = object.__getattribute__(self, "_data")
        if name in data:
            return _wrap_value(data[name])
        camel_name = _snake_to_camel(name)
        if camel_name in data:
            return _wrap_value(data[camel_name])
        raise AttributeError(name)

    def __getitem__(self, key):
        return _wrap_value(self._data[key])

    def get(self, key, default=None):
        return self._data.get(key, default)

    def keys(self):
        return self._data.keys()

    def items(self):
        return self._data.items()

    def __repr__(self):
        return f"{type(self).__name__}({self._data!r})"


class Header(_AttributeMapping):
    """Decoded BLF object header with attribute and mapping access."""


class ExtData(_AttributeMapping):
    """Decoded optional CAN-FD timing extension."""


class RestorePoint(_AttributeMapping):
    """One restore point entry from a BLF restore-point container."""


class RestorePoints(_AttributeMapping):
    """Decoded restore-point metadata and entries."""


class FileStatistics(_AttributeMapping):
    """BLF file statistics with attribute and mapping access."""

    @property
    def measurement_start_time(self):
        value = self._data.get("measurementStartTime")
        return _wrap_value(value)

    @property
    def last_object_time(self):
        value = self._data.get("lastObjectTime")
        return _wrap_value(value)


class _Object(_AttributeMapping):
    __slots__ = ()

    def __init__(self, data):
        super().__init__(data)

    @property
    def object_type(self):
        return self._data["objectType"]

    @property
    def class_name(self):
        """Canonical BLF class name, including for typed legacy objects."""
        return self._data.get("className", self._data.get("class", type(self).__name__))

    @property
    def header(self):
        value = self._data.get("header")
        return Header(value) if isinstance(value, dict) else value

    @property
    def extData(self):
        value = self._data.get("extData")
        return ExtData(value) if isinstance(value, dict) else value

    @property
    def ext_data(self):
        return self.extData

    @property
    def timestamp_ns(self):
        """Object timestamp converted using the BLF object time flags."""
        return self._data.get("timestampNs")

    @property
    def direction(self):
        """Human-readable direction for objects that expose ``dir``."""
        value = self._data.get("dir")
        return {0: "Rx", 1: "Tx", 2: "TxRq"}.get(value, value)

    @property
    def is_rx(self):
        return self._data.get("dir") == 0

    @property
    def is_tx(self):
        return self._data.get("dir") == 1

    @property
    def raw_direction(self):
        return self._data.get("dir")

    @property
    def source_address(self):
        return self._data.get("sourceAddress", self._data.get("sourceAdr"))

    @property
    def destination_address(self):
        return self._data.get("destinationAddress", self._data.get("destAdr"))

    @property
    def frame_id(self):
        """CAN/FlexRay-style frame identifier alias for ``id``."""
        return self._data.get("frameId", self._data.get("id"))

    @property
    def can_fd_flags(self):
        """CAN-FD protocol flags, when present on the object."""
        if "canFdFlags" in self._data:
            return self._data["canFdFlags"]
        if self.class_name in {"CanFdMessage64", "CanFdErrorFrame64"}:
            return self._data.get("flags")
        return None

    def _can_fd_flag(self, mask):
        flags = self.can_fd_flags
        return flags is not None and bool(int(flags) & mask)

    @property
    def is_remote_frame(self):
        return self._can_fd_flag(0x0010)

    @property
    def is_tx_request(self):
        return self._can_fd_flag(0x0080)

    @property
    def tx_acknowledge(self):
        return self._can_fd_flag(0x0040)

    @property
    def is_fd(self):
        return self._can_fd_flag(0x1000)

    @property
    def bit_rate_switch(self):
        return self._can_fd_flag(0x2000)

    @property
    def esi(self):
        return self._can_fd_flag(0x4000)

    @property
    def object_time_stamp(self):
        return self._data.get("objectTimeStamp")

    @property
    def object_flags(self):
        return self._data.get("objectFlags")

    @property
    def fields(self):
        return self._data.get("fields", {})

    @property
    def payload(self):
        """Decoded payload bytes when the binding exposes them.

        Older typed objects expose their protocol payload as ``data``.  The
        property keeps callers from having to special-case those objects.
        """
        if "payload" in self._data:
            return self._data["payload"]
        candidates = (
            ("data", None),
            ("dataBytes", "dataCount"),
            ("payLoad", "payLoadLength"),
            ("pktData", "pktDataLength"),
            ("frameData", "frameLength"),
            ("msg", "msgLen"),
            ("firstData", "firstDataLen"),
            ("a429Data", None),
            ("regData", "regDataLen"),
            ("text", "textLength"),
        )
        value = None
        length = None
        for name, length_name in candidates:
            candidate = self._data.get(name)
            if isinstance(candidate, bytes):
                value = candidate
                length = self._data.get(length_name) if length_name else None
                break
        if not isinstance(value, bytes):
            return b""
        if self.class_name in {"CanFdMessage", "CanFdMessage64", "CanFdErrorFrame64"}:
            length = self._data.get("validDataBytes")
        elif self.class_name.startswith("Can") or self.class_name.startswith("Lin"):
            length = self._data.get("dlc")
        if isinstance(length, int) and 0 <= length <= len(value):
            return value[:length]
        return value

    @property
    def payload_length(self):
        return len(self.payload)

    @property
    def raw_data(self):
        """Complete encoded BLF object, including its object header."""
        return self._data.get("rawData", self._data.get("data", b""))

    @property
    def extensions(self):
        """Unparsed bytes after the known object layout, if any."""
        return self._data.get("extensions", b"")

    @property
    def data_bytes(self):
        """Alias for the protocol data field without shadowing ``data``."""
        value = self._data.get("data")
        return value if isinstance(value, bytes) else b""

class CanMessage(_Object):
    pass


class CanMessage2(_Object):
    pass


class CanFdMessage(_Object):
    pass


class CanFdMessage64(_Object):
    pass


class CanErrorFrame(_Object):
    pass


class CanErrorFrameExt(_Object):
    pass


class CanFdErrorFrame64(_Object):
    pass


class AppText(_Object):
    pass


class RestorePointContainer(_Object):
    @property
    def restore_points(self):
        value = self._data.get("restorePoints")
        return RestorePoints(value) if isinstance(value, dict) else value


class RawObject(_Object):
    """An unknown BLF object retaining its original bytes."""

    pass


class StructuredObject(_Object):
    """A known BLF object with named fields and original bytes."""

    pass


class LinMessage(_Object):
    pass


class LinCrcError(_Object):
    pass


class LinDlcInfo(_Object):
    pass


class LinReceiveError(_Object):
    pass


class LinSendError(_Object):
    pass


class LinSlaveTimeout(_Object):
    pass


class LinSchedulerModeChange(_Object):
    pass


class LinSyncError(_Object):
    pass


class LinBaudrateEvent(_Object):
    pass


class LinSleepModeEvent(_Object):
    pass


class LinWakeupEvent(_Object):
    pass


class LinChecksumInfo(_Object):
    pass


class LinSpikeEvent(_Object):
    pass


class LinStatisticEvent(_Object):
    pass


class LinMessage2(_Object):
    pass


class LinSendError2(_Object):
    pass


class LinSyncError2(_Object):
    pass


class LinCrcError2(_Object):
    pass


class LinReceiveError2(_Object):
    pass


class LinWakeupEvent2(_Object):
    pass


class LinSpikeEvent2(_Object):
    pass


class LinLongDomSignalEvent(_Object):
    pass


class LinLongDomSignalEvent2(_Object):
    pass


class LinUnexpectedWakeup(_Object):
    pass


class LinShortOrSlowResponse(_Object):
    pass


class LinDisturbanceEvent(_Object):
    pass


class LinShortOrSlowResponse2(_Object):
    pass


_TYPED_CLASSES = (
        CanMessage,
        CanMessage2,
        CanFdMessage,
        CanFdMessage64,
        CanErrorFrame,
        CanErrorFrameExt,
        CanFdErrorFrame64,
        AppText,
        RestorePointContainer,
        LinMessage,
        LinCrcError,
        LinDlcInfo,
        LinReceiveError,
        LinSendError,
        LinSlaveTimeout,
        LinSchedulerModeChange,
        LinSyncError,
        LinBaudrateEvent,
        LinSleepModeEvent,
        LinWakeupEvent,
        LinChecksumInfo,
        LinSpikeEvent,
        LinStatisticEvent,
        LinMessage2,
        LinSendError2,
        LinSyncError2,
        LinCrcError2,
        LinReceiveError2,
        LinWakeupEvent2,
        LinSpikeEvent2,
        LinLongDomSignalEvent,
        LinLongDomSignalEvent2,
        LinUnexpectedWakeup,
        LinShortOrSlowResponse,
        LinDisturbanceEvent,
        LinShortOrSlowResponse2,
)

_RAW_CLASS_NAMES = (
    "CanOverloadFrame", "CanDriverStatistic", "AppTrigger", "EnvironmentVariable",
    "MostSpy", "MostCtrl", "MostLightLock", "MostStatistic", "FlexRayData",
    "FlexRaySync", "CanDriverError", "MostPkt", "MostPkt2", "MostHwMode",
    "MostReg", "MostGenReg", "MostNetState", "MostDataLost", "MostTrigger",
    "FlexRayV6StartCycleEvent", "FlexRayV6Message", "CanDriverHwSync",
    "FlexRayStatusEvent", "GpsEvent", "FlexRayVFrError", "FlexRayVFrStatus",
    "FlexRayVFrStartCycle", "FlexRayVFrReceiveMsg", "RealtimeClock", "J1708Message",
    "FlexRayVFrReceiveMsgEx", "MostStatisticEx", "MostTxLight", "MostAllocTab",
    "MostStress", "EthernetFrame", "SystemVariable", "CanDriverErrorExt",
    "Most150Message", "Most150Pkt", "MostEthernetPkt", "Most150MessageFragment",
    "Most150PktFragment", "MostEthernetPktFragment", "MostSystemEvent",
    "Most150AllocTab", "Most50Message", "Most50Pkt", "SerialEvent", "DriverOverrun",
    "EventComment", "WlanFrame", "WlanStatistic", "MostEcl", "GlobalMarker",
    "AfdxFrame", "AfdxStatistic", "KLineStatusEvent", "EthernetRxError",
    "EthernetStatus", "AfdxStatus", "AfdxBusStatistic", "AfdxErrorEvent", "A429Error",
    "A429Status", "A429BusStatistic", "A429Message", "EthernetStatistic", "TestStructure",
    "DiagRequestInterpretation", "EthernetFrameEx", "EthernetFrameForwarded",
    "EthernetErrorEx", "EthernetErrorForwarded", "FunctionBus", "DataLostBegin",
    "DataLostEnd", "WaterMarkEvent", "TriggerCondition", "CanSettingChanged",
    "DistributedObjectMember", "AttributeEvent",
)

for _name in _RAW_CLASS_NAMES:
    globals()[_name] = type(_name, (StructuredObject,), {})

CLASS_BY_NAME = {
    cls.__name__: cls
    for cls in _TYPED_CLASSES
    + (StructuredObject, RawObject)
    + tuple(globals()[name] for name in _RAW_CLASS_NAMES)
}

CAN_CLASS_NAMES = {
    "CanMessage",
    "CanMessage2",
    "CanFdMessage",
    "CanFdMessage64",
}

CAN_ERROR_CLASS_NAMES = {
    "CanErrorFrame",
    "CanErrorFrameExt",
    "CanFdErrorFrame64",
    "CanOverloadFrame",
    "CanDriverError",
    "CanDriverErrorExt",
    "CanDriverHwSync",
    "DriverOverrun",
}

CAN_OBJECT_CLASS_NAMES = CAN_CLASS_NAMES | CAN_ERROR_CLASS_NAMES | {
    "CanDriverStatistic",
    "CanSettingChanged",
}


def object_from_data(data):
    cls = CLASS_BY_NAME.get(
        data.get("className"), CLASS_BY_NAME.get(data.get("class"), RawObject)
    )
    return cls(data)


__all__ = [
    "ObjectType",
    "Header",
    "ExtData",
    "RestorePoint",
    "RestorePoints",
    "FileStatistics",
    "RawObject",
    "StructuredObject",
    "object_from_data",
    "CAN_CLASS_NAMES",
    "CAN_ERROR_CLASS_NAMES",
    "CAN_OBJECT_CLASS_NAMES",
    *(cls.__name__ for cls in _TYPED_CLASSES),
    *_RAW_CLASS_NAMES,
]
