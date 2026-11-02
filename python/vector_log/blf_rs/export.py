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
                "timestamp_ns": _value(obj, "timestamp_ns", -1),
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
    return {
        "timestamp_ns": np.asarray([r["timestamp_ns"] for r in records], dtype=np.int64),
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
    return pa.table(
        {
            key: [record[key] for record in records]
            for key in (
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
        }
    )


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
    "objects_to_csv",
    "objects_to_jsonl",
    "write_csv",
    "write_jsonl",
]
