#!/usr/bin/env python3
"""Benchmark Rust-native lazy signal decoding through the Python API."""

from __future__ import annotations

import argparse
import statistics
import time

from vector_log.blf_rs import DbcDatabase, LdfDatabase, open


def measure(path, database, bus, channel, signal_names):
    start = time.perf_counter()
    with open(path, databases={(bus, channel): database}) as reader:
        count = sum(
            len(reader.signal(name, bus=bus, channel=channel).collect())
            for name in signal_names
        )
    return count, time.perf_counter() - start


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", help="BLF file")
    parser.add_argument("database", help="DBC or LDF database path")
    parser.add_argument(
        "signals",
        nargs="+",
        help="One or more qualified signal names, such as Status.Speed",
    )
    parser.add_argument("--bus", default="can")
    parser.add_argument("--channel", type=int, required=True)
    parser.add_argument("--repeat", type=int, default=5)
    args = parser.parse_args()
    if args.repeat < 1:
        parser.error("--repeat must be positive")
    database_path = args.database.lower()
    if database_path.endswith(".ldf"):
        database = LdfDatabase.from_file(args.database)
    else:
        database = DbcDatabase.from_file(args.database)
    samples = [
        measure(args.path, database, args.bus, args.channel, args.signals)
        for _ in range(args.repeat)
    ]
    count = samples[0][0]
    durations = [duration for _, duration in samples]
    mean_seconds = statistics.mean(durations)
    print(f"samples: {count}")
    print(f"repeat: {args.repeat}")
    print(f"mean_seconds: {mean_seconds:.6f}")
    print(f"min_seconds: {min(durations):.6f}")
    print(
        "samples_per_second: "
        f"{count / mean_seconds:.2f}" if count else "samples_per_second: 0.00"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
