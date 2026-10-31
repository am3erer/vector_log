from . import _core
from .classes import (
    CAN_CLASS_NAMES,
    AppText,
    CanErrorFrame,
    CanErrorFrameExt,
    CanFdErrorFrame64,
    CanFdMessage,
    CanFdMessage64,
    CanMessage,
    CanMessage2,
    LinBaudrateEvent,
    LinChecksumInfo,
    LinCrcError,
    LinCrcError2,
    LinDisturbanceEvent,
    LinDlcInfo,
    LinLongDomSignalEvent,
    LinLongDomSignalEvent2,
    LinMessage,
    LinMessage2,
    LinReceiveError,
    LinReceiveError2,
    LinSchedulerModeChange,
    LinSendError,
    LinSendError2,
    LinShortOrSlowResponse,
    LinShortOrSlowResponse2,
    LinSlaveTimeout,
    LinSleepModeEvent,
    LinSpikeEvent,
    LinSpikeEvent2,
    LinStatisticEvent,
    LinSyncError,
    LinSyncError2,
    LinUnexpectedWakeup,
    LinWakeupEvent,
    LinWakeupEvent2,
    RestorePointContainer,
    object_from_data,
)


class File:
    def __init__(self, native):
        self._native = native

    @property
    def file_statistics(self):
        return self._native.file_statistics

    def is_open(self):
        return self._native.is_open()

    def eof(self):
        return self._native.eof()

    def close(self):
        self._native.close()

    def read(self):
        data = self._native.read()
        if data is None:
            return None
        return object_from_data(data)

    def read_batch(self, max_objects):
        return [object_from_data(data) for data in self._native.read_batch(max_objects)]

    def can_messages(self):
        while True:
            obj = self.read()
            if obj is None:
                return
            if obj.__class__.__name__ in CAN_CLASS_NAMES:
                yield obj

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_value, traceback):
        self.close()


def open(path, strict=None):
    return File(_core.open(path, strict))


__all__ = [
    "File",
    "open",
    "AppText",
    "CanErrorFrame",
    "CanErrorFrameExt",
    "CanFdErrorFrame64",
    "CanFdMessage",
    "CanFdMessage64",
    "CanMessage",
    "CanMessage2",
    "LinBaudrateEvent",
    "LinChecksumInfo",
    "LinCrcError",
    "LinCrcError2",
    "LinDisturbanceEvent",
    "LinDlcInfo",
    "LinLongDomSignalEvent",
    "LinLongDomSignalEvent2",
    "LinMessage",
    "LinMessage2",
    "LinReceiveError",
    "LinReceiveError2",
    "LinSchedulerModeChange",
    "LinSendError",
    "LinSendError2",
    "LinShortOrSlowResponse",
    "LinShortOrSlowResponse2",
    "LinSlaveTimeout",
    "LinSleepModeEvent",
    "LinSpikeEvent",
    "LinSpikeEvent2",
    "LinStatisticEvent",
    "LinSyncError",
    "LinSyncError2",
    "LinUnexpectedWakeup",
    "LinWakeupEvent",
    "LinWakeupEvent2",
    "RestorePointContainer",
]
