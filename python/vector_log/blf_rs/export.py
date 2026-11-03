"""Optional batch exports for BLF objects.

The BLF reader itself has no third-party data-analysis dependency. NumPy and
PyArrow are imported only when the corresponding export is requested.
"""

from __future__ import annotations

from collections.abc import Iterable
import csv
import io
import json


EXPORT_FIELDS = (
    "timestamp_ns",
    "object_type",
    "class_name",
    "channel",
    "frame_id",
    "direction",
    "dlc",
    "valid_data_bytes",
    "data_length",
    "data",
)


def _objects(objects: Iterable):
    return list(objects)


def _value(obj, name, default=None):
    value = getattr(obj, name, default)
    return default if value is None else value


def _payload(obj):
    value = getattr(obj, "payload", b"")
    return bytes(value) if isinstance(value, (bytes, bytearray, memoryview)) else b""


def _records(objects):
    records = []
    for obj in objects:
        payload = _payload(obj)
        direction = _value(obj, "dir", -1)
        records.append(
            {
                # BLF timestamps are unsigned.  Keep absent timestamps as
                # None so nullable exports do not need a sentinel that can
                # overflow an unsigned column.
                "timestamp_ns": _value(obj, "timestamp_ns", None),
                "object_type": _value(obj, "object_type", -1),
                "class_name": str(_value(obj, "class_name", type(obj).__name__)),
                "channel": _value(obj, "channel", -1),
                "frame_id": _value(obj, "frame_id", _value(obj, "id", -1)),
                "direction": direction,
                "dlc": _value(obj, "dlc", -1),
                "valid_data_bytes": _value(obj, "valid_data_bytes", -1),
                "data": payload,
                "data_length": len(payload),
            }
        )
    return records


def objects_to_numpy(objects: Iterable, *, data_width: int | None = None):
    """Convert objects to columnar NumPy arrays.

    The result is a dictionary. ``data`` is a zero-padded ``uint8`` matrix;
    ``data_length`` identifies the meaningful prefix of every row. Set
    ``data_width=None`` to use the widest payload in the input.
    """
    try:
        import numpy as np
    except ImportError as error:  # pragma: no cover - depends on environment
        raise ImportError(
            "NumPy export requires the optional 'numpy' package"
        ) from error

    records = _records(_objects(objects))
    if data_width is None:
        data_width = max((record["data_length"] for record in records), default=0)
    if not isinstance(data_width, int) or data_width < 0:
        raise ValueError("data_width must be a non-negative integer or None")
    if any(record["data_length"] > data_width for record in records):
        raise ValueError("data_width is smaller than a payload in the input")

    data = np.zeros((len(records), data_width), dtype=np.uint8)
    for index, record in enumerate(records):
        if record["data"]:
            data[index, : record["data_length"]] = np.frombuffer(
                record["data"], dtype=np.uint8
            )
    timestamps = [0 if record["timestamp_ns"] is None else int(record["timestamp_ns"]) for record in records]
    timestamp_mask = [record["timestamp_ns"] is None for record in records]
    return {
        "timestamp_ns": np.ma.array(timestamps, mask=timestamp_mask, dtype=np.uint64),
        "object_type": np.asarray([r["object_type"] for r in records], dtype=np.int64),
        "class_name": np.asarray([r["class_name"] for r in records]),
        "channel": np.asarray([r["channel"] for r in records], dtype=np.int64),
        "frame_id": np.asarray([r["frame_id"] for r in records], dtype=np.int64),
        "direction": np.asarray([r["direction"] for r in records], dtype=np.int16),
        "dlc": np.asarray([r["dlc"] for r in records], dtype=np.int16),
        "valid_data_bytes": np.asarray(
            [r["valid_data_bytes"] for r in records], dtype=np.int16
        ),
        "data_length": np.asarray([r["data_length"] for r in records], dtype=np.int32),
        "data": data,
    }


def objects_to_arrow(objects: Iterable, *, data_width: int | None = None):
    """Convert objects to a PyArrow table.

    Arrow stores payloads as variable-length binary values. ``data_width`` is
    accepted for API symmetry with NumPy; when supplied, payloads longer than
    it are rejected and shorter values are left unpadded.
    """
    try:
        import pyarrow as pa
    except ImportError as error:  # pragma: no cover - depends on environment
        raise ImportError(
            "Arrow export requires the optional 'pyarrow' package"
        ) from error

    records = _records(_objects(objects))
    if data_width is not None:
        if not isinstance(data_width, int) or data_width < 0:
            raise ValueError("data_width must be a non-negative integer or None")
        if any(record["data_length"] > data_width for record in records):
            raise ValueError("data_width is smaller than a payload in the input")
    columns = {
        "timestamp_ns": pa.array(
            [record["timestamp_ns"] for record in records], type=pa.uint64()
        )
    }
    columns.update(
        {
            key: [record[key] for record in records]
            for key in (
                "object_type",
                "class_name",
                "channel",
                "frame_id",
                "direction",
                "dlc",
                "valid_data_bytes",
                "data_length",
                "data",
            )
        }
    )
    return pa.table(columns)


def samples_to_numpy(samples: Iterable):
    """Convert signal samples to nullable-friendly NumPy columns.

    ``physical_value`` uses ``NaN`` for missing values. ``raw_value`` selects
    an exact signed or unsigned 64-bit masked array when the input range fits;
    mixed ranges use an object array to avoid lossy coercion. Payload bytes
    remain variable-length Python objects because NumPy has no native nullable
    binary scalar.
    """
    try:
        import numpy as np
    except ImportError as error:  # pragma: no cover - depends on environment
        raise ImportError("NumPy export requires the optional 'numpy' package") from error
    rows = list(samples)
    numeric = lambda name: np.asarray(
        [np.nan if getattr(row, name, None) is None else getattr(row, name) for row in rows],
        dtype=np.float64,
    )
    raw_values = [
        0 if getattr(row, "raw_value", None) is None else int(row.raw_value)
        for row in rows
    ]
    raw_mask = [getattr(row, "raw_value", None) is None for row in rows]
    present_raw = [value for value, masked in zip(raw_values, raw_mask) if not masked]
    if present_raw and min(present_raw) >= 0 and max(present_raw) > 2**63 - 1:
        raw_dtype = np.uint64
    elif not present_raw or (
        min(present_raw) >= -(2**63) and max(present_raw) <= 2**63 - 1
    ):
        raw_dtype = np.int64
    else:
        raw_dtype = object
    return {
        "timestamp_ns": np.asarray(
            [getattr(row, "timestamp_ns", 0) for row in rows], dtype=np.uint64
        ),
        "bus": np.asarray([getattr(row, "bus", "") for row in rows]),
        "channel": np.asarray([getattr(row, "channel", -1) for row in rows], dtype=np.int64),
        "frame_id": np.asarray([getattr(row, "frame_id", -1) for row in rows], dtype=np.int64),
        "message_name": np.asarray([getattr(row, "message_name", "") for row in rows]),
        "signal_name": np.asarray([getattr(row, "signal_name", "") for row in rows]),
        "raw_value": np.ma.array(raw_values, mask=raw_mask, dtype=raw_dtype),
        "physical_value": numeric("physical_value"),
        "value": np.asarray([getattr(row, "value", None) for row in rows], dtype=object),
        "value_text": np.asarray([getattr(row, "value_text", None) for row in rows], dtype=object),
        "status": np.asarray([getattr(row, "status", "") for row in rows]),
        "error": np.asarray([getattr(row, "error", None) for row in rows], dtype=object),
        "data": np.asarray([bytes(getattr(row, "data", b"")) for row in rows], dtype=object),
    }


def samples_to_arrow(samples: Iterable):
    """Convert signal samples to a nullable PyArrow table.

    Arrow columns have one scalar type.  Since Python ``SignalSample.value``
    intentionally uses either choice text or a physical number, its Arrow
    representation is a nullable string display column.  Numeric consumers
    should use the typed ``raw_value`` and ``physical_value`` columns; the
    separate ``value_text`` column remains available for choice lookups.
    """
    try:
        import pyarrow as pa
    except ImportError as error:  # pragma: no cover - depends on environment
        raise ImportError("Arrow export requires the optional 'pyarrow' package") from error
    rows = list(samples)
    value_display = []
    for row in rows:
        value = getattr(row, "value", None)
        if value is None:
            value_display.append(None)
        elif isinstance(value, str):
            value_display.append(value)
        else:
            value_display.append(str(value))
    return pa.table(
        {
            "timestamp_ns": pa.array(
                [getattr(row, "timestamp_ns", None) for row in rows],
                type=pa.uint64(),
            ),
            "bus": pa.array([getattr(row, "bus", None) for row in rows], type=pa.string()),
            "channel": pa.array(
                [getattr(row, "channel", None) for row in rows], type=pa.uint16()
            ),
            "frame_id": pa.array(
                [getattr(row, "frame_id", None) for row in rows], type=pa.uint32()
            ),
            "message_name": pa.array(
                [getattr(row, "message_name", None) for row in rows], type=pa.string()
            ),
            "signal_name": pa.array(
                [getattr(row, "signal_name", None) for row in rows], type=pa.string()
            ),
            "raw_value": _raw_arrow_array(pa, rows),
            "physical_value": pa.array(
                [getattr(row, "physical_value", None) for row in rows], type=pa.float64()
            ),
            "value": pa.array(value_display, type=pa.string()),
            "value_text": pa.array(
                [getattr(row, "value_text", None) for row in rows], type=pa.string()
            ),
            "status": pa.array(
                [getattr(row, "status", None) for row in rows], type=pa.string()
            ),
            "error": pa.array(
                [getattr(row, "error", None) for row in rows], type=pa.string()
            ),
            "data": pa.array(
                [bytes(getattr(row, "data", b"")) for row in rows], type=pa.binary()
            ),
        }
    )


def _raw_arrow_array(pa, rows):
    values = [getattr(row, "raw_value", None) for row in rows]
    present = [int(value) for value in values if value is not None]
    if not present or (min(present) >= -(2**63) and max(present) <= 2**63 - 1):
        return pa.array(values, type=pa.int64())
    if min(present) >= 0 and max(present) <= 2**64 - 1:
        return pa.array(values, type=pa.uint64())
    # Signal raw values are at most 64 bits, so decimal128 precision 20 is
    # sufficient for a mixed signed/unsigned column while retaining nulls.
    return pa.array(values, type=pa.decimal128(20, 0))


def _export_rows(objects):
    for record in _records(objects):
        record = dict(record)
        record["data"] = record["data"].hex(" ")
        yield record


def objects_to_csv(objects: Iterable, *, include_header: bool = True) -> str:
    """Serialize objects to CSV using the stable batch-export schema."""
    output = io.StringIO(newline="")
    writer = csv.DictWriter(output, fieldnames=EXPORT_FIELDS)
    if include_header:
        writer.writeheader()
    writer.writerows(_export_rows(objects))
    return output.getvalue()


def write_csv(objects: Iterable, target, *, include_header: bool = True) -> None:
    """Write CSV to a filesystem path or an open text stream."""
    close_target = False
    if isinstance(target, (str, bytes)) or hasattr(target, "__fspath__"):
        target = open(target, "w", encoding="utf-8", newline="")
        close_target = True
    try:
        writer = csv.DictWriter(target, fieldnames=EXPORT_FIELDS)
        if include_header:
            writer.writeheader()
        writer.writerows(_export_rows(objects))
    finally:
        if close_target:
            target.close()


def _json_value(value):
    if isinstance(value, (bytes, bytearray, memoryview)):
        return bytes(value).hex(" ")
    if isinstance(value, dict):
        return {key: _json_value(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_json_value(item) for item in value]
    return value


def objects_to_jsonl(objects: Iterable) -> str:
    """Serialize objects as newline-delimited JSON records."""
    return "".join(
        json.dumps(_json_value(record), separators=(",", ":")) + "\n"
        for record in _export_rows(objects)
    )


def write_jsonl(objects: Iterable, target) -> None:
    """Write newline-delimited JSON to a filesystem path or text stream."""
    close_target = False
    if isinstance(target, (str, bytes)) or hasattr(target, "__fspath__"):
        target = open(target, "w", encoding="utf-8")
        close_target = True
    try:
        for record in _export_rows(objects):
            target.write(json.dumps(_json_value(record), separators=(",", ":")))
            target.write("\n")
    finally:
        if close_target:
            target.close()


__all__ = [
    "objects_to_numpy",
    "objects_to_arrow",
    "samples_to_numpy",
    "samples_to_arrow",
    "objects_to_csv",
    "objects_to_jsonl",
    "write_csv",
    "write_jsonl",
]
