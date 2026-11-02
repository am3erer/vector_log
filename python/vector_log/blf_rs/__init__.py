import os

from . import _core
from . import classes as _classes
from .classes import *
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
    FileStatistics,
    ObjectType,
    RestorePointContainer,
    object_from_data,
)


class File:
    def __init__(self, native):
        self._native = native

    @property
    def file_statistics(self):
        return FileStatistics(self._native.file_statistics)

    @property
    def fileStatistics(self):
        return self.file_statistics

    def is_open(self):
        return self._native.is_open()

    def good(self):
        return self._native.good()

    def eof(self):
        return self._native.eof()

    @property
    def current_uncompressed_file_size(self):
        return self._native.current_uncompressed_file_size

    @property
    def current_object_count(self):
        return self._native.current_object_count

    @property
    def compression_level(self):
        return self._native.compression_level

    @property
    def write_restore_points(self):
        return self._native.write_restore_points

    @property
    def currentUncompressedFileSize(self):
        return self.current_uncompressed_file_size

    @property
    def currentObjectCount(self):
        return self.current_object_count

    @property
    def compressionLevel(self):
        return self.compression_level

    @property
    def writeRestorePoints(self):
        return self.write_restore_points

    def close(self):
        self._native.close()

    def read(self):
        data = self._native.read()
        if data is None:
            return None
        return object_from_data(data)

    def read_batch(self, max_objects):
        return [object_from_data(data) for data in self._native.read_batch(max_objects)]

    def iter_objects(self):
        while True:
            obj = self.read()
            if obj is None:
                return
            yield obj

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

    def __iter__(self):
        return self.iter_objects()


def open(path, strict=None):
    return File(_core.open(os.fspath(path), strict))


__all__ = [
    "File",
    "open",
    *_classes.__all__,
]
