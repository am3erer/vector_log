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

Truly unknown object IDs are skipped by default, matching the historical
raw-reader contract. Use `preserve_unknown_objects=True` to retain them as
`RawObject`, or use `strict=True` to reject an unknown object instead:

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

### Terminal UI

The workspace also provides `vector-log-tui`, a keyboard-first terminal viewer
for BLF messages. It uses the Rust standard library and ANSI terminal control,
so it does not add a `ratatui` or `crossterm` dependency and works well over
SSH or inside `tmux`:

```bash
cargo run -p blf-tui -- trace.blf
cargo run -p blf-tui -- trace.blf \
  --dbc 1=vehicle.dbc \
  --ldf 3=body.ldf
# Browse databases alone: no trace, starts in the database browser.
cargo run -p blf-tui -- --dbc 1=vehicle.dbc --ldf 3=body.ldf
```

The binary is built, not installed:

```bash
cargo build -p blf-tui             # target/debug/vector-log-tui
cargo build --release -p blf-tui   # target/release/vector-log-tui
```

Run it from the repository root, for example
`./target/debug/vector-log-tui --dbc 1=vehicle.dbc`, or let cargo locate it with
`cargo run -p blf-tui -- --dbc 1=vehicle.dbc`. It is not on `PATH` unless you link
or alias it yourself, and it has no connection to the sources, so rebuild after
pulling changes. Add `--offline` to either command when the machine cannot reach
the crate registry.

The trace path, when given, must be the first argument. Without one, at least
one `--dbc`/`--ldf` is required and the viewer boots into the database browser
with `(no trace)` in the header; the message, detail and curve modes are
unreachable, `Esc` does not leave the browser, and trace-only keys report
`No trace loaded`. `vector-log-tui --help` documents both invocations.

The application scans the BLF at startup, then allows message filtering and
selection, per-message detail inspection, channel-specific DBC/LDF binding,
signal decoding, and an ASCII signal curve. Database paths can also be bound
while the viewer is running with `b`, using `can CHANNEL PATH` or
`lin CHANNEL PATH`. Press `d` to open the database browser. Its layout follows
the CANdb++-style workflow used by the companion `candb_tui` project: the left
column shows the bound database and its object categories, the center column
lists messages/signals/frames, and the right column shows details for the
selected object. One pane has focus at a time and the browser has three focus
stops: the tree, the object list and the details. `Left`/`Right` and `h`/`l`
step towards the first or last stop, `Tab`/`Shift-Tab` cycle through them, and
the arrow keys act on the focused pane. The focused pane draws the strong
selection marker and background; the other pane keeps its row visible with a
dimmed `·` marker. The details column wraps long lines to its width and scrolls:
with the details focused, `j`/`k` and `Up`/`Down` scroll by one line,
`PageUp`/`PageDown` by one screen, and `Home`/`End` jump to the first or last
line. Its header reports the visible range as `Details  lines 1-18 of 41`, or
just `lines 1-18 of 41` when the column is narrow, and the scroll starts over
when another object is selected. Below 100 columns the three columns do not
leave enough room for that, so a focused details pane drops the tree and object
columns and uses the whole terminal width: `name: EngineState`,
`transmitter: Engine` and `GenMsgCycleTime = 100` then stay on one line.
`Left`/`Right`, `h`/`l` or `Tab`/`Shift-Tab` bring the three-column layout back
by focusing the tree or the object list, and at 100 columns and above the
layout is always the three-column one. The database name is a non-selectable
header line in the tree pane, because `[`/`]` switch loaded databases. Long text
never disappears from the viewer: the message preview, both message-detail
columns, the database details and the help descriptions wrap while counting wide
CJK glyphs as two cells. A line breaks after its last space when it has one,
otherwise in front of the last `(`, `=`, `,`, `/` or `-` it contains, so a
value, a list member or an attribute specification starts a new line instead
of a word being split; only a run without any break point is split inside
the word. Only the short labels of the tabular message list and of the tree
and object columns end in `...`, because their text is shown next to them.
The detail column groups each object into bold headings with indented items
and dim notes, separated by a blank line, so message metadata, attribute
blocks, signal lists and node relations stay readable instead of running
together. It includes each DBC message's transmitter and the deduplicated
receivers of its signals. A DBC signal shows its message's transmitter next to
its deduplicated receivers; a LIN signal shows its publisher next to its
deduplicated subscribers, and a LIN frame shows the union of its signals'
subscribers. A DBC node groups its relations under the `messages sent`,
`messages received`, `signals sent` and `signals received` headings; a LIN node
groups the frames it publishes and the signals it subscribes to. Message,
signal and node details carry an
`attributes:` block: every assigned attribute is shown with its definition and
default, followed by a `defaults:` list with the `BA_DEF_DEF_` defaults of that
scope's definitions which the object does not assign. An attribute therefore
keeps its definition's default until a `BA_` assignment overrides it.
Assignments, defaults, receivers, subscribers and signal names each get their
own line, so a long enum value or receiver list stays readable. The Attributes
category shows each definition's type, range or enum members, default and
assignment count. A detail list only stops after 500 entries and reports the
remaining count.

Key controls are:

```text
j/k or Up/Down    move through messages, signals, database rows or detail lines
PageUp/PageDown   move one screen at a time
Home/End          jump to the first or last item or detail line
g                 jump by trace time, +offset or #position
G                 jump to the last message
] / [             next / previous occurrence of the same frame id
Enter             open the selected message, or focus objects / report the
                  selection in the database browser
/                 filter messages, or search objects in the database browser
b                 bind a DBC/LDF database
d                 open the DBC/LDF browser
Left/Right, h/l   step between tree, object and details pane focus
Tab, Shift-Tab    cycle tree, object and details pane focus (below 100 columns
                  the focused details use the whole width)
[/]               switch loaded database
e                 export the filtered messages with decoded signals to CSV
p                 open a curve for the selected numeric signal
?                 open the key help screen
Esc/Left          go back
q                 quit
```

The message list, detail and curve modes need a trace. A database-only session
stays in the browser, where `Esc` cannot go back to a message list; the keys
that act on the trace (`e`, `g`, `G`, `p` and `Esc` from the browser) report
`No trace loaded`, and the rest of the browser works as usual.

`g` opens a prompt that takes three forms, and reports where it landed:

```text
10.000770     the trace clock: the first message at or after 10.000770s
+10           10 s after the first message of the visible list
#1234         position 1234 in the visible list
```

The bare form uses the timestamps the message list displays, so a time copied
from a row lands where it says; `@10.000770` is accepted as an explicit alias.
`+` counts from the first message of the visible list because bench traces
rarely start at zero. Jumps respect the active filter, so `/0x1` followed by
`g 5` lands on the first visible `0x1` frame at or after 5 s on the trace clock.
After a jump the status line names the landed message with both clocks, for
example `At 5.025000s (+3.510000s from start) (118 / 219)`. `]` and `[` step
between the occurrences of the selected frame id and report which one of how
many is shown.

`?` opens a help screen in every mode, grouping the bindings by mode
(Messages, Detail, Curve and Databases) and finishing with the global keys. It
closes with `?`, `q` or `Esc`. A description that does not fit wraps under its
key instead of being cut. When the list does not fit, `j`/`k`, the arrow keys,
PageUp/PageDown and Home/End scroll it, and the title shows which rows are
visible. The bottom line shows the active prompt while typing a filter,
binding, database filter or goto value, and the status message otherwise.

Opening a message shows its decoded signals on the right. The left column also
reports the cycle timing of that frame id on the same bus and channel: average,
minimum and maximum period, mean jitter, and how many frames the log holds.

Pressing `e` writes `<trace>.signals.csv` next to the trace, with one row per
filtered message and one column per decoded signal (up to 64 signal columns).
Filtering to a single frame id first turns the export into a time series for a
bench report.

Curves stream the signal from the reader and keep at most 200,000 samples; the
title says when a longer signal was cut. Run `vector-log-tui --help` for the
command line usage.

The current viewer keeps message summaries in memory for responsive navigation;
very large logs may use more memory than the streaming reader. Mouse input is
supported for row selection and wheel navigation. In the curve view, Left/Right
pans the visible sample window, `+`/`-` zooms it, and Home/End jump to either
boundary. Background indexing and multi-signal plots remain future extensions.

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

Parsed database handles expose their native metadata through read-only Python
views. `DbcDatabase` provides `version`, `attributes`, `scoped_attributes`,
`nodes`, sorted `signal_names`, `messages`, and `signal_groups`. DBC nodes
preserve their declared names and source locations. Each message includes
`id`, `extended`, `name`, `dlc`, `transmitter`, `is_fd`, `payload_kind`,
`signals`, `comment`, `attributes`, and `source`; each signal includes its bit
layout, byte order, signedness, scaling, limits, `unit`, `receivers`, choice
table, multiplex role, attributes, comment, and source location. Signal groups
preserve their `message_id`, `message_extended`, `name`, `repetition`, member
`signal_names`, and source location. Use `message_by_id()`,
`message_by_name()`, and `signal()` for indexed lookup:

An empty DBC unit (`""`) is exposed as `None`; non-empty units are preserved
verbatim.

DBC metadata views also preserve named `value_tables`, `attribute_defaults`,
and the raw numeric `signal_type_value_types` mapping. Relationship metadata is
kept separate through `relationship_attributes`,
`relationship_attributes_for(name)`, and `relationship_attribute_defaults`.
Relationship assignments preserve their relationship kind, source node, message
ID, signal name, raw value, and source location; they are not merged into
ordinary message or signal attributes. A CAN signal exposes its
attribute definitions through `attribute_definitions`; each definition keeps its
name, scope, optional relationship kind, DBC value type, raw numeric arguments,
enumeration values, and source location. This includes `BA_DEF_SGTYPE_` signal
type definitions, while raw bounds remain strings so hexadecimal and large
integer spellings are not lost. Every ordinary `BA_` assignment is also
available through `attribute_assignments`, preserving its target scope,
message/signal or object target, raw value, and source location; the existing
message, signal, and scoped attribute views remain available for convenient
lookup.
optional `value_type` declaration from `SIG_VALTYPE_`. Types `1` and `2` decode
the signal as IEEE-754 binary32 and binary64 respectively, in the signal's DBC
byte order, while retaining the exact raw bit pattern in `raw_value` and
applying the normal DBC scale/offset to `physical_value`. Other value types
retain the normal integer behavior. Float signals must be exactly 32 or 64 bits;
IEEE NaN and Infinity are preserved. The parser accepts both standard
colon-form and legacy no-colon `SIG_VALTYPE_` statements. DBC's conventional
`[0|0]` signal limit marker is exposed as `None` for both bounds, meaning that
the decoder does not apply an omitted physical range.

```python
dbc = DbcDatabase.from_file("vehicle.dbc")
message = dbc.message_by_id(0x123, extended=False)
print(message["signals"])
print(dbc.signal("VehicleStatus.VehicleSpeed")["signal"])
```

`LdfDatabase` exposes `channel_name`, protocol and language versions,
`speed_kbps`, `nodes`, `frames`, `schedules`, `encodings`, and
`optional_sections`. Diagnostic LIN definitions are kept in their own
namespace: `diagnostic_frames`, `diagnostic_signals`, and `node_attributes`
expose the LDF `Diagnostic_signals`, `Diagnostic_frames`, and
`Node_attributes` sections without mixing them into ordinary scheduled
frames. Use `diagnostic_frame_by_id()`, `diagnostic_frame_by_name()`, and
`diagnostic_signal()` for indexed lookup. Diagnostic frames preserve their
classic checksum model, eight-byte payload definition, signal bit offsets, and
source locations. Node attribute dictionaries preserve configured and initial
NADs, product IDs, response-error signal names, LIN protocol, timing values in
milliseconds, configurable frames, and source locations. LIN node dictionaries
also preserve the master `timebase_ms` and `jitter_ms` values. Frame and signal dictionaries preserve checksum rules,
publishers, signal publishers and subscribers, bit layouts, initial values, scaling, limits, encoding names,
choice tables, physical ranges, and source locations. Use `frame_by_id()`,
`frame_by_name()`, and `signal()` for indexed lookup. `signal_names` and
`diagnostic_signal_names` provide sorted `Frame.Signal` names, while
`signal_count` and `diagnostic_signal_count` provide signal counts. Dynamic LIN groups are
available through `dynamic_frames`; use `dynamic_frames_for_id(frame_id)` or
`dynamic_frame_for_id(frame_id)` when you need only groups containing one
frame. These views contain copies of metadata and cannot mutate the Rust-owned
database used by signal queries.

`reader.signal()` resolves a qualified `Message.Signal` or `Frame.Signal`
within one explicit binding. This also applies to diagnostic names such as
`MasterReq.MasterReqB0`; LIN diagnostic frame IDs `0x3c` and `0x3d` use the
classic checksum rule. `reader.signals(name=...)` is the explicit
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
`value_text`, `status`, `error`, and owned `data` bytes. Samples also preserve
`object_type`, `object_class`, `direction`, `raw_direction`, `is_tx`, `is_rx`,
`dlc`, `payload_length`, `header_dlc`, `timestamp_source`,
`signal_timestamp_ns`, and `databyte_timestamps`. For LIN, `header_dlc` is the
protocol header declaration and `payload_length` is the number of response
bytes actually available; they are intentionally separate. `databyte_timestamps`
preserves Vector's raw per-byte values. Because their unit/base depends on the
BLF format variant, the reader does not invent an absolute
`signal_timestamp_ns`; it remains `None` until that metadata is explicit.
`value` contains the DBC/LDF choice text when a value table matches, otherwise
the physical number.
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

# One BLF scan can target multiple qualified signals. Filters remain lazy.
query = reader.signal_many(
    ["VehicleStatus.Speed", "VehicleStatus.Mode"],
    bus="can", channel=1,
    start_ns=1_000_000, end_ns=2_000_000, max_samples=10,
)
for sample in query:
    print(sample.signal_name, sample.timestamp_ns, sample.value)
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
database. A query captures the binding when it is created, so an existing query
is not changed by a later rebind. `signal_many()` and `signals_many()` share one
lazy BLF cursor for their target signals; `signals([...])` is an alias for the
aggregate form.

Signal queries accept lazy filters `start_ns`, `end_ns`, `frame_ids`,
`directions`, `object_types`, and `max_samples`. `directions` accepts the
normalized names (`Rx`, `Tx`, `TxRq`) or raw Vector direction values. For
classic CAN (`CanMessage`, `CanMessage2`, and the legacy `CanFdMessage`),
Vector stores direction in the low four bits of `flags`: `0` is `Rx` and `1`
is `Tx`; the remaining flag bits are RTR/WU/NERR protocol flags. `CanFdMessage64`
and LIN objects use their explicit `dir` field. The original `flags` and raw
direction are both preserved, and an undefined direction is reported as
`Unknown` rather than being treated as LIN `TxRq`. For classic CAN, `dlc` is
the number of valid payload bytes; for CAN-FD, `dlc` is the protocol data
length code and `payload_length` is the actual `valid_data_bytes` count.
Database parse failures raise `DatabaseParseError` with `path`, `line`,
`field`, and `reason` attributes.

For LIN, checksum validation uses valid BLF checksum metadata when present and
otherwise falls back to the LDF frame rule, including classic checksum for
diagnostic frame IDs `0x3C` and `0x3D`. Receive-error records preserve their
Vector BLF `stateReason` in the error text. The BLF record normally stores only
the six-bit frame ID and does not identify which receive-error cause occurred,
so PID parity is not guessed from that field. When an adapter supplies an
explicit protected identifier (`pid` or `protectedId`), classification
validates it before signal extraction.

Known diagnostic and vendor-prefixed (`Vendor_`) block-shaped LDF sections are
reported by name and source location; their balanced bodies remain opaque when
they are outside the native decoding model. `Sporadic_frames` and
`Event_triggered_frames` are additionally exposed as structured
`dynamic_frames` metadata with associated frame names and source locations.
Because a BLF response does not carry the schedule trigger context, querying a
frame from one of those groups returns a value only when its payload is safe,
with a `dynamic_frame` status and diagnostic instead of pretending the frame
was an unconditional response. Other unknown sections, unterminated optional
blocks, and malformed required LDF statements return structured parse errors.

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
- Unknown object IDs are skipped by default; `preserve_unknown_objects=True`
  retains them as `RawObject`, `skip_unknown_objects=True` explicitly filters
  them, while `strict=True` makes them an error.
- `max_container_size`, `max_object_size`, and `max_signature_scan` provide
  configurable safety limits for damaged or untrusted files. The default
  limits preserve the existing reader behavior. `BlfLimitError` and
  `BlfIoError` distinguish limit and I/O failures from format errors.
- The default tolerant reader skips a malformed object when it can validate a
  later object boundary, then continues reading subsequent messages. If a
  damaged object is at the end of a container, it can continue with the next
  container. `reader.skipped_object_count` reports how many malformed objects
  were skipped. Use `strict=True` when corruption must remain an error.

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

The Rust-native lazy signal path has a separate benchmark. It accepts one or
more qualified signal names; `--repeat` measures repeated independent scans:

```bash
PYTHONPATH=python python3 benchmarks/bench_signal_query.py \
  trace.blf vehicle.dbc Status.Speed Status.Mode \
  --bus can --channel 1 --repeat 5
```

One signal measures a single query, adding a second measures two queries over
the same BLF, and `--repeat` records repeated-query throughput. The benchmark
reports decoded samples per second and is intended as a release-mode smoke
measurement rather than a machine-specific threshold.

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
