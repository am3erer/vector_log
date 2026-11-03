# vector_log

Rust-backed Vector BLF reader with Python bindings. The implementation follows
the object layouts and decoding behavior of
[`Technica-Engineering/vector_blf`](https://github.com/Technica-Engineering/vector_blf)
and uses [`cantools`](https://github.com/cantools/cantools) as a reference for
CAN-oriented Python workflows.

This project provides reliable BLF reading and native DBC/LDF signal decoding.
BLF writing is not supported.

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

### Native signal decoding

DBC and LDF files are parsed by Rust and do not require `cantools` or
`ldfparser` at runtime. Bind every database explicitly to a `(bus, channel)`
pair. Database paths and `pathlib.Path` values are accepted, and a parsed
database handle can be reused:

```python
from pathlib import Path
from vector_log.blf_rs import DbcDatabase, LdfDatabase, open

reader = open(
    "trace.blf",
    databases={
        ("can", 1): Path("vehicle.dbc"),
        ("can", 2): DbcDatabase.from_file("adas.dbc"),
        ("lin", 3): LdfDatabase.from_file("body.ldf"),
    },
)

query = reader.signal("VehicleStatus.VehicleSpeed", bus="can", channel=1)
for sample in query.samples:
    print(sample.timestamp_ns, sample.value, sample.raw_value, sample.status)
```

`reader.signal()` resolves a qualified `Message.Signal` or `Frame.Signal`
within one explicit binding. `reader.signals(name=...)` is the explicit
same-name aggregation form. It returns every matching message/frame while
preserving `message_name`, `signal_name`, `frame_id`, and channel context; it
does not silently merge databases from other channels.

Signal queries are indexed by the requested database-defined message/frame and
signal. In tolerant mode, a CAN/LIN frame that is not defined by the selected
binding produces an `unknown_frame` sample with its channel, frame ID, raw
payload, requested signal context, and nullable decoded values. In strict mode
that sample is raised as `SignalQueryError`. Frames from other channels remain
outside the query.

Queries are lazy and own an independent Rust BLF cursor, so creating or
advancing a query does not consume ordinary `reader.read()` iteration or
another query. `collect()` materializes owned `SignalSample` objects:

```python
samples = reader.signal("VehicleStatus.VehicleSpeed", bus="can", channel=1).collect()
for sample in samples:
    print(sample.message_name, sample.physical_value, sample.data)
```

The default is tolerant decoding. A bad payload, unknown response state, LIN
checksum error, or missing response produces a sample with a non-`ok` status
and nullable `raw_value`, `physical_value`, `value`, and `value_text` when a
value cannot be extracted safely. Use `strict=True` to raise
`SignalQueryError` at the first decoding error:

```python
query = reader.signal("Status.Speed", bus="can", channel=1, strict=True)
```

`SignalSample` fields are `timestamp_ns`, `bus`, `channel`, `frame_id`,
`message_name`, `signal_name`, `raw_value`, `physical_value`, `value`,
`value_text`, `status`, `error`, and owned `data` bytes. `value` contains the
DBC/LDF choice text when a value table matches, otherwise the physical number.
Forensic LIN checksum-error samples may retain safely extracted values and
payload bytes, but their non-`ok` status means those values are untrusted.

`query.samples` is an alias for the lazy query itself, so both `for sample in
query` and `for sample in query.samples` are supported.

Optional materialization is available without making NumPy or PyArrow core
dependencies:

```python
query = reader.signal("Status.Speed", bus="can", channel=1)
numpy_columns = query.to_numpy()  # raw_value is exact signed/unsigned integer data; physical_value uses NaN

query = reader.signal("Status.Speed", bus="can", channel=1)
arrow_table = query.to_arrow()  # nullable Arrow columns and variable-length binary data
```

Both methods consume the lazy query. Use `samples_to_numpy()` or
`samples_to_arrow()` from `vector_log.blf_rs` when converting an existing
sample collection. Calling an optional converter without its package raises a
focused `ImportError`; raw BLF reading and signal iteration remain dependency
free.

Arrow columns are strongly typed: `raw_value` and `physical_value` are
numeric, `value_text` is nullable text, and `value` is a nullable text display
column so choice strings and non-choice numeric values can coexist in one
table. NumPy keeps `raw_value` exact, choosing masked `int64` or `uint64` when
the batch has a single compatible range and an object array for mixed signed
and unsigned extremes. Arrow chooses nullable `int64` or `uint64` and uses a
decimal integer column only for mixed ranges. `NaN` is used only for nullable
physical floating-point values. The Python
`SignalSample.value` property keeps its richer behavior and returns the choice
text when available, otherwise the physical number.

Object batch exports use the BLF timestamp contract directly: NumPy
`timestamp_ns` is a masked `uint64` array, so timestamps above `2**63 - 1`
remain exact and objects without a timestamp are represented by the mask.
Arrow uses a nullable `uint64` `timestamp_ns` column with the same semantics.

Rust signal queries reject duplicate `(bus, channel)` database bindings at
construction time, so an aggregate query cannot emit duplicate samples by
accident.

Python `reader.bind_database(bus, channel, database)` is an explicit rebind
operation: a later binding for the same `(bus, channel)` replaces the previous
database. Signal queries always use the current binding.

For LIN, checksum validation uses valid BLF checksum metadata when present and
otherwise falls back to the LDF frame rule, including classic checksum for
diagnostic frame IDs `0x3C` and `0x3D`. Receive-error records preserve their
Vector BLF `stateReason` in the error text. The BLF record stores only the
six-bit frame ID and does not identify which receive-error cause occurred, so
PID parity is not guessed; callers can use `validate_lin_identifier()` when a
protected identifier is available from another source.

Known optional LDF sections and explicitly vendor-prefixed (`Vendor_`)
block-shaped sections are reported by name and source location; their balanced
bodies are preserved only as opaque presence metadata. Ordinary frame/signal
decoding does not interpret those sections yet. Other unknown sections,
unterminated optional blocks, and malformed required LDF statements return
structured parse errors.

The optional `vector_log.blf_rs.cantools_integration` module remains available
as a compatibility bridge for applications that already own a cantools
database. It is not imported by the native reader and does not add cantools as
a runtime dependency; new code can use the native `DbcDatabase` path above.

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
and a recorded BLF file when those external workspace paths are available:

- 109 upstream normal fixture files, 447 decoded objects, and 0 parse errors
  (external workspace corpus; not bundled in this repository).
- Every decoded fixture object has non-empty `raw_data`.
- `RoofUnits.blf` decodes to 671 objects (external BLF fixture).
- Python attribute audit: 671 objects and 16,914 fields with 0 failures on
  `RoofUnits.blf`.
- Rust workspace tests: all passed, including parser, decoder, LIN-state,
  sample-contract, independent-cursor, and SignalQuery integration tests.
- Python API tests: all passed.
- Clippy with warnings denied: passed.
- `RoofUnits.blf`: 671 objects, with LIN traffic isolated to channel 3; a
  database whose frame IDs do not overlap the file correctly returns no signal
  samples rather than fabricating values.
