#!/usr/bin/env python3
"""Small reproducible BLF reader benchmark."""

from __future__ import annotations

import argparse
import statistics
import time

from vector_log.blf_rs import open


def measure(path, batch_size):
    start = time.perf_counter()
    count = 0
    with open(path) as reader:
        while True:
            batch = reader.read_batch(batch_size)
            count += len(batch)
            if not batch or len(batch) < batch_size:
                break
    return count, time.perf_counter() - start


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path")
    parser.add_argument("--repeat", type=int, default=5)
    parser.add_argument("--batch-size", type=int, default=1024)
    args = parser.parse_args()
    if args.repeat < 1 or args.batch_size < 1:
        parser.error("--repeat and --batch-size must be positive")
    samples = [measure(args.path, args.batch_size) for _ in range(args.repeat)]
    count = samples[0][0]
    durations = [duration for _, duration in samples]
    print(f"objects: {count}")
    print(f"repeat: {args.repeat}, batch_size: {args.batch_size}")
    print(f"mean_seconds: {statistics.mean(durations):.6f}")
    print(f"min_seconds: {min(durations):.6f}")
    mean_seconds = statistics.mean(durations)
    print(
        "objects_per_second: "
        f"{count / mean_seconds:.2f}" if count else "objects_per_second: 0.00"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
