#!/usr/bin/env python3
"""Print file statistics and object summaries for a Vector BLF file."""

import argparse
from collections import Counter
from pathlib import Path
import sys


PROJECT_ROOT = Path(__file__).resolve().parent
PYTHON_SOURCE = PROJECT_ROOT / "python"

try:
    from vector_log.blf_rs import open
except ModuleNotFoundError as error:
    if error.name != "vector_log":
        raise
    sys.path.insert(0, str(PYTHON_SOURCE))
    try:
        from vector_log.blf_rs import open
    except ImportError as error:
        raise SystemExit(
            "vector_log native extension is not built. Run "
            "`maturin develop --release` from the vector_log repository, "
            "then retry this command."
        ) from error
except ImportError as error:
    raise SystemExit(
        "vector_log native extension is not installed correctly. Run "
        "`maturin develop --release` from the vector_log repository, "
        "then retry this command."
    ) from error


def format_timestamp(ns):
    if ns is None:
        return "-"
    return f"{ns / 1_000_000_000.0:.9f}"


def format_bytes(data, length):
    if data is None:
        return None
    if length is not None and 0 < length < len(data):
        data = data[:length]
    return data.hex(" ").upper()


def format_direction(direction):
    labels = {
        0: "Rx",
        1: "Tx",
        2: "TxRq",
    }
    if direction is None:
        return None
    return labels.get(direction, f"Unknown({direction})")


def object_field(obj, name, default=None):
    fields = getattr(obj, "fields", None)
    if isinstance(fields, dict) and name in fields:
        return fields[name]
    return getattr(obj, name, default)


def message_data(obj, class_name):
    data_names = (
        "data",
        "dataBytes",
        "payLoad",
        "pktData",
        "frameData",
        "msg",
        "firstData",
        "a429Data",
        "regData",
        "text",
    )
    for name in data_names:
        value = object_field(obj, name)
        if isinstance(value, bytes):
            length_name = {
                "data": "dlc" if class_name.startswith("Lin") else "validDataBytes",
                "dataBytes": "dataCount",
                "payLoad": "payLoadLength",
                "pktData": "pktDataLength",
                "frameData": "frameLength",
                "msg": "msgLen",
                "firstData": "firstDataLen",
                "regData": "regDataLen",
                "text": "textLength",
            }.get(name)
            length = object_field(obj, length_name) if length_name else None
            return value, length
    return None, None


def lin_event_description(class_name):
    descriptions = {
        "LinSendError": "TxHeaderNoResponse",
        "LinSendError2": "TxHeaderNoResponse",
    }
    return descriptions.get(class_name)


def print_file_stats(stats):
    print("== File Statistics ==")
    fields = [
        ("signature", f"0x{stats['signature']:08X}"),
        ("statisticsSize", stats["statisticsSize"]),
        ("apiNumber", stats["apiNumber"]),
        ("applicationId", stats["applicationId"]),
        ("applicationVersion", f"{stats['applicationMajor']}.{stats['applicationMinor']}.{stats['applicationBuild']}"),
        ("compressionLevel", stats["compressionLevel"]),
        ("fileSize", stats["fileSize"]),
        ("uncompressedFileSize", stats["uncompressedFileSize"]),
        ("objectCount", stats["objectCount"]),
        ("restorePointsOffset", stats["restorePointsOffset"]),
    ]
    for name, value in fields:
        print(f"{name:<24} {value}")

    start = stats["measurementStartTime"]
    end = stats["lastObjectTime"]
    print(
        "measurementStartTime    "
        f"{start['year']:04d}-{start['month']:02d}-{start['day']:02d} "
        f"{start['hour']:02d}:{start['minute']:02d}:{start['second']:02d}.{start['milliseconds']:03d}"
    )
    print(
        "lastObjectTime          "
        f"{end['year']:04d}-{end['month']:02d}-{end['day']:02d} "
        f"{end['hour']:02d}:{end['minute']:02d}:{end['second']:02d}.{end['milliseconds']:03d}"
    )
    print()


def print_object(index, obj):
    class_name = type(obj).__name__
    timestamp = getattr(obj, "object_time_stamp", None)
    channel = object_field(obj, "channel")
    frame_id = object_field(obj, "id")
    dlc = object_field(obj, "dlc")
    direction = object_field(obj, "dir")
    data, data_length = message_data(obj, class_name)
    is_lin = class_name.startswith("Lin")
    event_description = lin_event_description(class_name)

    parts = [
        f"[{index}] {class_name}",
        f"t={format_timestamp(timestamp)}",
    ]
    if channel is not None:
        parts.append(f"ch={channel}")
    if frame_id is not None:
        parts.append(f"id=0x{frame_id:02X}")
    if dlc is not None:
        parts.append(f"{'headerDlc' if is_lin else 'dlc'}={dlc}")
    if direction is not None:
        parts.append(f"dir={format_direction(direction)}")
    elif event_description is not None:
        parts.append(f"event={event_description}")
    if data is not None:
        parts.append(f"data={format_bytes(data, data_length if data_length is not None else dlc)}")

    print("  ".join(parts))

    if class_name == "AppText":
        source = getattr(obj, "source", None)
        text = getattr(obj, "text", None)
        if isinstance(text, bytes):
            text = text.rstrip(b"\x00").decode("utf-8", errors="replace")
        print(f"      source={source} text={text}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", help="Path to a Vector BLF file")
    parser.add_argument(
        "--limit",
        type=int,
        default=50,
        help="Maximum number of objects to print; use 0 to print all",
    )
    parser.add_argument(
        "--messages-only",
        action="store_true",
        help="Print every object without file statistics or the summary",
    )
    args = parser.parse_args()

    with open(args.path) as reader:
        if not args.messages_only:
            print_file_stats(reader.file_statistics)

        counts = Counter()
        total = 0
        if args.messages_only:
            args.limit = 0
        else:
            print("== Objects ==")

        while True:
            obj = reader.read()
            if obj is None:
                break

            class_name = type(obj).__name__
            counts[class_name] += 1
            if args.limit == 0 or total < args.limit:
                print_object(total, obj)
            total += 1

        if not args.messages_only:
            print()
            print("== Object Summary ==")
            print(f"total objects: {total}")
            for class_name, count in sorted(counts.items(), key=lambda item: (-item[1], item[0])):
                print(f"{class_name:<28} {count}")


if __name__ == "__main__":
    main()
