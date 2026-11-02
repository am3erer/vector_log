# vector_log

Rust-backed Vector BLF reader with Python bindings. The implementation follows
the object layouts and decoding behavior of
[`Technica-Engineering/vector_blf`](https://github.com/Technica-Engineering/vector_blf)
and uses [`cantools`](https://github.com/cantools/cantools) as a reference for
CAN-oriented Python workflows.

This project currently focuses on reliable BLF reading and inspection. BLF
writing is not supported.

## Supported BLF content

The reader handles file metadata, containers, headers, protocol objects, and
diagnostic objects, including:

- `FileStatistics`, `LogContainer`, and zlib-compressed containers.
- `ObjectHeaderBase`, `ObjectHeader`, `ObjectHeader2`, and `VarObjectHeader`.
- CAN, CAN FD, CAN error, and CAN FD error frames.
- LIN message, error, sleep, wakeup, and scheduler event objects.
- FlexRay, MOST, Ethernet, AFDX, A429, WLAN, J1708, and K-Line objects.
- Environment variables and system variables.
- Serial events, test structures, diagnostic objects, and Function Bus objects.
- Marker, trigger, data-lost, watermark, attribute, and distributed-object
  member objects.
- Application text and restore-point containers.

Non-reserved `vector_blf` object type IDs are decoded into named fields. The
complete encoded bytes and unrecognised extension bytes are retained, so newer
BLF extensions remain inspectable. Reserved IDs are skipped using their
`objectSize`, matching `vector_blf`.

Truly unknown object IDs are returned as `RawObject` in the default mode. Use
`strict=True` to reject an unknown object instead:

```python
from vector_log.blf_rs import open

with open("trace.blf", strict=True) as reader:
    for obj in reader:
        print(obj)
```

## Build and install

Run these commands from the repository root:

```bash
python3 -m pip install maturin
python3 -m maturin build --release
python3 -m maturin develop --release
```

If the local uv cache is read-only, use a writable temporary cache:

```bash
UV_CACHE_DIR=/tmp/vector-log-uv-cache python3 -m maturin develop --release
```

The Rust test suite can be run with:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Python API

The extension is exposed as `vector_log.blf_rs`:

```python
from vector_log.blf_rs import open

with open("trace.blf") as reader:
    print(reader.file_statistics.fileSize)
    for obj in reader:
        print(type(obj).__name__, obj.object_type)
        print(obj.raw_data)
```

Reader methods and properties include:

- `read()` reads one object and returns `None` at EOF.
- `read_batch(max_objects)` reads up to the requested number of objects.
- `read_numpy(max_objects=None)` exports the next objects as NumPy columns;
  payloads are zero-padded to the widest payload by default and paired with
  `data_length`. Pass `data_width=64` for a fixed CAN-FD-sized matrix.
- `read_arrow(max_objects=None)` exports the next objects as a PyArrow table;
  PyArrow is optional and payloads remain variable-length binary values.
- `iter_objects()` and direct iteration provide object iteration. It accepts
  `object_types`, `channels`, `frame_ids`, `start_ns`, `end_ns`, and a
  `predicate` for sequential filtering. Object types may be numeric IDs or
  class names.
- `iter_numpy(batch_size=4096)` and `iter_arrow(batch_size=4096)` stream
  columnar batches without retaining the complete file in memory.
- `can_messages()`, `can_error_frames()`, and `can_objects()` filter the
  stream to CAN-family objects and accept the same filters.
- `objects_to_csv()` and `objects_to_jsonl()` provide standard-library text
  exports; `write_csv()` and `write_jsonl()` stream them to a path or text
  file. Payload bytes are represented as space-separated hexadecimal text.
- `file_statistics` exposes file metadata and measurement timestamps.
- `current_object_count`, `current_uncompressed_file_size`, and
  `compression_level` expose reader progress and container state.
- `good()`, `eof()`, and `close()` expose reader state and lifecycle control.
- `strict=True` makes unknown object IDs an error.
- `max_container_size`, `max_object_size`, and `max_signature_scan` provide
  configurable safety limits for damaged or untrusted files. The default
  limits preserve the existing reader behavior. `BlfLimitError` and
  `BlfIoError` distinguish limit and I/O failures from format errors.

Every decoded object exposes its wire fields through `fields`, and the same
fields are available as Python attributes where the name is valid. Both the
camelCase names used by the BLF structures and snake_case aliases are
supported:

```python
obj.fields["channel"] == obj.channel
obj.frameLength == obj.frame_length
obj.header.objectSize == obj.header.object_size
obj.header.base.signature
```

The binding also exposes nested extension structures. For example, CAN FD
objects may provide:

```python
if obj.extData is not None:
    print(obj.extData.btrExtArb)
    print(obj.ext_data.btr_ext_data)
```

CAN-FD objects also expose protocol flag helpers such as `is_remote_frame`,
`is_tx_request`, `is_fd`, `bit_rate_switch`, `tx_acknowledge`, and `esi`.
Classic CAN `flags` are kept separate and are not interpreted as CAN-FD flags.

Reserved object type 115 is retained as `RestorePointContainer`. Restore-point
entries are decoded when the payload has the expected `(objectSize - 8) / 24`
layout; opaque payload variants remain available through `data` instead of
causing the reader to stop.

The byte properties have distinct meanings:

- `data` is the protocol payload on typed legacy objects.
- `payload` is the structured protocol payload when the object provides one.
- `raw_data` is the complete encoded BLF object, including its object header.
- `extensions` contains structured or unrecognised extension bytes.

For example, `reader.file_statistics.measurement_start_time.year` accesses a
decoded timestamp component, while `reader.file_statistics.fileSize` accesses
the original BLF field name.

The batch export helpers are also available for an existing iterable:

```python
from vector_log.blf_rs import objects_to_arrow, objects_to_numpy

objects = list(reader.can_messages())
columns = objects_to_numpy(objects)
table = objects_to_arrow(objects)  # requires optional pyarrow
```

The optional fuzz smoke harness and benchmark can be run without changing the
BLF reader:

```bash
PYTHONPATH=python python3 scripts/fuzz_reader.py --iterations 1000
PYTHONPATH=python python3 benchmarks/bench_reader.py trace.blf --repeat 5
```

The fuzz harness treats expected format, truncation, compression, and
unsupported-object errors as correct rejection; unexpected exceptions still
fail the process.

## Inspect a BLF file

`print_blf_info.py` prints file statistics and a summary by default:

```bash
python3 print_blf_info.py trace.blf
python3 print_blf_info.py trace.blf --limit 100
python3 print_blf_info.py trace.blf --limit 0
```

Use `--messages-only` to print every decoded object line without file
statistics or the final summary:

```bash
python3 print_blf_info.py trace.blf --messages-only
```

The script discovers the repository's local `python/` source directory when it
is launched from a checkout. LIN directions are shown as `Rx`, `Tx`, or `TxRq`.
LIN `headerDlc` is the frame length declared by the LIN message descriptor; it
is not a generic BLF header field. `LinMessage2` has a `dir` field, but
`LinSendError2` does not: that object type itself describes a transmitted LIN
header with no response. The script represents this event as
`event=TxHeaderNoResponse`.

## Validation

The current implementation has been checked against the upstream fixture set
and a recorded BLF file:

- 109 upstream normal fixture files, 447 decoded objects, and 0 parse errors.
- Every decoded fixture object has non-empty `raw_data`.
- `RoofUnits.blf` decodes to 671 objects.
- Python attribute audit: 671 objects and 16,914 fields with 0 failures on
  `RoofUnits.blf`.
- Rust workspace tests: 16 passed.
- Clippy with warnings denied: passed.
