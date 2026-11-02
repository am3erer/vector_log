#!/usr/bin/env python3
"""Deterministic BLF reader fuzz smoke test.

This harness treats expected BLF errors as successful rejection and fails only
when the parser leaks an unexpected exception. It is intentionally dependency
free so CI can run it without a fuzzing framework.
"""

from __future__ import annotations

import argparse
import random
import tempfile
from pathlib import Path

from vector_log.blf_rs import (
    BlfCompressionError,
    BlfFormatError,
    BlfTruncatedError,
    BlfUnsupportedObjectError,
    BlfUnsupportedVersionError,
    open,
)


EXPECTED = (
    BlfCompressionError,
    BlfFormatError,
    BlfTruncatedError,
    BlfUnsupportedObjectError,
    BlfUnsupportedVersionError,
    OSError,
)


def mutate(seed: bytes, rng: random.Random) -> bytes:
    data = bytearray(seed)
    if not data:
        data.extend(rng.randbytes(rng.randrange(1, 256)))
    for _ in range(rng.randrange(1, 6)):
        operation = rng.randrange(3)
        if operation == 0 and data:
            data[rng.randrange(len(data))] ^= 1 << rng.randrange(8)
        elif operation == 1 and len(data) < 1 << 20:
            offset = rng.randrange(len(data) + 1)
            data[offset:offset] = rng.randbytes(rng.randrange(1, 16))
        elif len(data) > 1:
            del data[rng.randrange(len(data)) : rng.randrange(len(data))]
    return bytes(data)


def exercise(payload: bytes) -> None:
    with tempfile.NamedTemporaryFile(suffix=".blf") as file:
        file.write(payload)
        file.flush()
        try:
            with open(file.name) as reader:
                for _ in reader:
                    pass
        except EXPECTED:
            return


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("seeds", nargs="*", type=Path)
    parser.add_argument("--iterations", type=int, default=100)
    parser.add_argument("--seed", type=int, default=0)
    args = parser.parse_args()
    if args.iterations < 1:
        parser.error("--iterations must be positive")
    seeds = [path.read_bytes() for path in args.seeds]
    rng = random.Random(args.seed)
    for index in range(args.iterations):
        seed = seeds[index % len(seeds)] if seeds else b""
        exercise(mutate(seed, rng))
    print(f"fuzz smoke passed: {args.iterations} cases")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
