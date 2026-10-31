class _Object:
    __slots__ = ("_data",)

    def __init__(self, data):
        object.__setattr__(self, "_data", data)

    def __getattr__(self, name):
        data = object.__getattribute__(self, "_data")
        if name in data:
            return data[name]
        raise AttributeError(name)

    @property
    def object_type(self):
        return self._data["objectType"]

    @property
    def object_time_stamp(self):
        return self._data.get("objectTimeStamp")

    def __repr__(self):
        return f"{type(self).__name__}({self._data!r})"


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


CLASS_BY_NAME = {
    cls.__name__: cls
    for cls in (
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
}

CAN_CLASS_NAMES = {
    "CanMessage",
    "CanMessage2",
    "CanFdMessage",
    "CanFdMessage64",
}


def object_from_data(data):
    cls = CLASS_BY_NAME.get(data.get("class"), _Object)
    return cls(data)
