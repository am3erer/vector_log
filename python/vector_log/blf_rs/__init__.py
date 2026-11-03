import os

from . import _core
from . import classes as _classes
from .export import (
    objects_to_arrow,
    objects_to_csv,
    objects_to_jsonl,
    objects_to_numpy,
    samples_to_arrow,
    samples_to_numpy,
    write_csv,
    write_jsonl,
)
from .classes import *
from .classes import (
    CAN_CLASS_NAMES,
    CAN_ERROR_CLASS_NAMES,
    CAN_OBJECT_CLASS_NAMES,
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

BlfFormatError = _core.BlfFormatError
BlfIoError = _core.BlfIoError
BlfLimitError = _core.BlfLimitError
BlfTruncatedError = _core.BlfTruncatedError
BlfCompressionError = _core.BlfCompressionError
BlfUnsupportedObjectError = _core.BlfUnsupportedObjectError
BlfUnsupportedVersionError = _core.BlfUnsupportedVersionError
SignalQueryError = _core.SignalQueryError
DbcDatabase = _core.DbcDatabase
LdfDatabase = _core.LdfDatabase


class SignalQuery:
    """Lazy Python view over a Rust-owned signal query."""

    def __init__(self, native):
        self._native = native

    def __iter__(self):
        return self

    @property
    def samples(self):
        """Return this query as the lazy iterable sample view."""
        return self

    def __next__(self):
        value = next(self._native, None)
        if value is None:
            raise StopIteration
        return SignalSample(value)

    @property
    def strict(self):
        return self._native.strict

    def collect(self):
        return list(self)

    def to_numpy(self):
        return samples_to_numpy(self)

    def to_arrow(self):
        return samples_to_arrow(self)


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
    def max_container_size(self):
        return self._native.max_container_size

    @property
    def max_object_size(self):
        return self._native.max_object_size

    @property
    def max_signature_scan(self):
        return self._native.max_signature_scan

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

    def bind_database(self, bus, channel, database):
        if isinstance(database, (str, bytes)) or hasattr(database, "__fspath__"):
            database = _load_database(bus, database)
        self._native.bind_database(str(bus), int(channel), database)
        return self

    def signal(self, signal_name, *, bus, channel, strict=False):
        return SignalQuery(
            self._native.signal(str(signal_name), str(bus), int(channel), bool(strict))
        )

    def signals(self, name, *, bus, channel, strict=False):
        return SignalQuery(
            self._native.signals(str(name), str(bus), int(channel), bool(strict))
        )

    def read_numpy(self, max_objects=None, *, data_width=None):
        """Read objects and return columnar NumPy arrays."""
        objects = self.iter_objects() if max_objects is None else self.read_batch(max_objects)
        return objects_to_numpy(objects, data_width=data_width)

    def read_arrow(self, max_objects=None, *, data_width=None):
        """Read objects and return a PyArrow table."""
        objects = self.iter_objects() if max_objects is None else self.read_batch(max_objects)
        return objects_to_arrow(objects, data_width=data_width)

    def iter_objects(
        self,
        *,
        object_types=None,
        channels=None,
        frame_ids=None,
        start_ns=None,
        end_ns=None,
        predicate=None,
    ):
        """Iterate objects matching optional metadata filters.

        Filters are applied during sequential reading and do not build an
        index or a second in-memory copy. Time bounds are inclusive.
        """
        object_types = _filter_values(object_types)
        channels = _filter_values(channels)
        frame_ids = _filter_values(frame_ids)
        while True:
            obj = self.read()
            if obj is None:
                return
            if object_types is not None and not _matches_object_type(obj, object_types):
                continue
            if channels is not None and getattr(obj, "channel", None) not in channels:
                continue
            if frame_ids is not None and getattr(obj, "frame_id", None) not in frame_ids:
                continue
            timestamp_ns = getattr(obj, "timestamp_ns", None)
            if start_ns is not None and (timestamp_ns is None or timestamp_ns < start_ns):
                continue
            if end_ns is not None and (timestamp_ns is None or timestamp_ns > end_ns):
                continue
            if predicate is not None and not predicate(obj):
                continue
            yield obj

    def iter_numpy(self, batch_size=4096, *, data_width=None, **filters):
        """Yield NumPy column batches while reading the file sequentially."""
        _validate_batch_size(batch_size)
        objects = self.iter_objects(**filters)
        while True:
            batch = _take(objects, batch_size)
            if not batch:
                return
            yield objects_to_numpy(batch, data_width=data_width)

    def iter_arrow(self, batch_size=4096, *, data_width=None, **filters):
        """Yield PyArrow table batches while reading the file sequentially."""
        _validate_batch_size(batch_size)
        objects = self.iter_objects(**filters)
        while True:
            batch = _take(objects, batch_size)
            if not batch:
                return
            yield objects_to_arrow(batch, data_width=data_width)

    def can_messages(self, **filters):
        yield from (
            obj for obj in self.iter_objects(**filters)
            if obj.__class__.__name__ in CAN_CLASS_NAMES
        )

    def can_error_frames(self, **filters):
        yield from (
            obj for obj in self.iter_objects(**filters)
            if obj.__class__.__name__ in CAN_ERROR_CLASS_NAMES
        )

    def can_objects(self, **filters):
        yield from (
            obj for obj in self.iter_objects(**filters)
            if obj.__class__.__name__ in CAN_OBJECT_CLASS_NAMES
        )

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_value, traceback):
        self.close()

    def __iter__(self):
        return self.iter_objects()


def open(
    path,
    strict=None,
    *,
    max_container_size=None,
    max_object_size=None,
    max_signature_scan=None,
    databases=None,
):
    reader = File(
        _core.open(
            os.fspath(path),
            strict,
            max_container_size,
            max_object_size,
            max_signature_scan,
        )
    )
    for binding, database in (databases or {}).items():
        try:
            bus, channel = binding
        except (TypeError, ValueError) as error:
            raise ValueError("database bindings must use (bus, channel) keys") from error
        reader.bind_database(bus, channel, database)
    return reader


def _load_database(bus, path):
    bus = str(bus).lower()
    if bus == "can":
        return DbcDatabase.from_file(os.fspath(path))
    if bus == "lin":
        return LdfDatabase.from_file(os.fspath(path))
    raise ValueError("bus must be 'can' or 'lin'")


def _filter_values(values):
    if values is None:
        return None
    if isinstance(values, (str, bytes, int)):
        return {values}
    try:
        return set(values)
    except TypeError:
        return {values}


def _matches_object_type(obj, values):
    object_type = getattr(obj, "object_type", None)
    class_name = getattr(obj, "class_name", type(obj).__name__)
    return object_type in values or class_name in values or type(obj).__name__ in values


def _take(iterator, count):
    batch = []
    for _ in range(count):
        try:
            batch.append(next(iterator))
        except StopIteration:
            break
    return batch


def _validate_batch_size(batch_size):
    if not isinstance(batch_size, int) or isinstance(batch_size, bool) or batch_size <= 0:
        raise ValueError("batch_size must be a positive integer")


__all__ = [
    "File",
    "open",
    "BlfFormatError",
    "BlfIoError",
    "BlfLimitError",
    "BlfTruncatedError",
    "BlfCompressionError",
    "BlfUnsupportedObjectError",
    "BlfUnsupportedVersionError",
    "SignalQueryError",
    "DbcDatabase",
    "LdfDatabase",
    "SignalQuery",
    "objects_to_numpy",
    "objects_to_arrow",
    "samples_to_numpy",
    "samples_to_arrow",
    "objects_to_csv",
    "objects_to_jsonl",
    "write_csv",
    "write_jsonl",
    *_classes.__all__,
]
