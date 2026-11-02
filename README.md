# vector_log

Rust-backed Vector BLF parser with Python bindings.

Current P0 scope:

- `FileStatistics`
- `LogContainer` and zlib decompression
- `ObjectHeaderBase`, `ObjectHeader`, `ObjectHeader2`, `VarObjectHeader`
- `CanMessage`, `CanMessage2`, `CanFdMessage`, `CanFdMessage64`
- `CanErrorFrame`, `CanErrorFrameExt`, `CanFdErrorFrame64`
- `AppText`, `RestorePointContainer`
- LIN objects, including `LinMessage`, `LinMessage2`, `LinSendError2`,
  `LinSleepModeEvent`, `LinWakeupEvent2`, `LinSchedulerModeChange`
- All non-reserved `vector_blf` object type IDs are decoded into named fields.
  The original object bytes and any unrecognised tail bytes are retained so
  newer BLF extensions remain inspectable.

Reserved object type IDs are skipped by `objectSize`, matching `vector_blf`.
Truly unknown object IDs are retained as `UnknownObject` in default mode and
rejected in strict mode; they are the only objects returned as raw objects.

## Rust core

```bash
~/.cargo/bin/cargo test -p blf-core
```

## Python wheel

```bash
python3 -m pip install maturin
PATH="$HOME/.cargo/bin:$PATH" python3 -m maturin build --release
```

For running the repository scripts directly, install the extension into the
active Python environment first:

```bash
python3 -m pip install maturin
python3 -m maturin develop --release
```

```python
from vector_log.blf_rs import open

reader = open("trace.blf")
while True:
    obj = reader.read()
    if obj is None:
        break
    print(type(obj).__name__, obj.object_type)
reader.close()
```

The Python binding now returns typed classes named after the C++ objects, such as `CanMessage`, `CanMessage2`, `CanFdMessage64`, and LIN classes. Convenience APIs include:

- `reader.read_batch(max_objects)`
- `reader.can_messages()`
- `reader.iter_objects()` and direct iteration over a reader
- `obj.fields` for named wire fields
- `obj.raw_data` for the complete encoded BLF object, including its header
- `obj.payload` and `obj.extensions` for structured objects

Typed legacy objects keep their protocol `data` field unchanged. Use
`raw_data` when the complete object bytes are needed; this avoids confusing a
CAN payload or an Ethernet frame with the enclosing BLF object.

## Inspect a BLF file

```bash
python3 print_blf_info.py trace.blf
python3 print_blf_info.py trace.blf --limit 100
python3 print_blf_info.py trace.blf --limit 0
python3 print_blf_info.py trace.blf --messages-only
```

`print_blf_info.py` also discovers the repository's local `python/` source
directory when launched from a checkout. `--messages-only` prints every object
line without file statistics or the final object summary. LIN directions are
shown as `Rx`, `Tx`, or `TxRq`; LIN `headerDlc` is the frame length declared by
the LIN message descriptor. `LinSendError2` has no `dir` field in the BLF
format because its object type already means a transmitted header received no
response; the script shows this as `event=TxHeaderNoResponse`.
