#!/usr/bin/env python3
"""Print file statistics and object summaries for a Vector BLF file."""

import argparse
from collections import Counter

from vector_log.blf_rs import open


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
    channel = getattr(obj, "channel", None)
    frame_id = getattr(obj, "id", None)
    dlc = getattr(obj, "dlc", None)
    direction = getattr(obj, "dir", None)
    data = getattr(obj, "data", None)

    parts = [
        f"[{index}] {class_name}",
        f"t={format_timestamp(timestamp)}",
    ]
    if channel is not None:
        parts.append(f"ch={channel}")
    if frame_id is not None:
        parts.append(f"id=0x{frame_id:02X}")
    if dlc is not None:
        parts.append(f"dlc={dlc}")
    if direction is not None:
        parts.append(f"dir={direction}")
    if data is not None:
        parts.append(f"data={format_bytes(data, dlc)}")

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
    args = parser.parse_args()

    with open(args.path) as reader:
        print_file_stats(reader.file_statistics)

        counts = Counter()
        total = 0
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

        print()
        print("== Object Summary ==")
        print(f"total objects: {total}")
        for class_name, count in sorted(counts.items(), key=lambda item: (-item[1], item[0])):
            print(f"{class_name:<28} {count}")


if __name__ == "__main__":
    main()
