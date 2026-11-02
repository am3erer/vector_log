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
- `iter_objects()` and direct iteration provide object iteration.
- `can_messages()` filters the stream to CAN-family messages.
- `file_statistics` exposes file metadata and measurement timestamps.
- `current_object_count`, `current_uncompressed_file_size`, and
  `compression_level` expose reader progress and container state.
- `good()`, `eof()`, and `close()` expose reader state and lifecycle control.
- `strict=True` makes unknown object IDs an error.

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

The byte properties have distinct meanings:

- `data` is the protocol payload on typed legacy objects.
- `payload` is the structured protocol payload when the object provides one.
- `raw_data` is the complete encoded BLF object, including its object header.
- `extensions` contains structured or unrecognised extension bytes.

For example, `reader.file_statistics.measurement_start_time.year` accesses a
decoded timestamp component, while `reader.file_statistics.fileSize` accesses
the original BLF field name.

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
- Python attribute audit: 1,118 objects and 19,894 fields with 0 failures.
- Rust workspace tests: 13 passed.
- Clippy with warnings denied: passed.
