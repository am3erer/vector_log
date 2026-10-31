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

Unknown object types are skipped by `objectSize`, matching `vector_blf`.

## Rust core

```bash
~/.cargo/bin/cargo test -p blf-core
```

## Python wheel

```bash
python3 -m pip install maturin
PATH="$HOME/.cargo/bin:$PATH" python3 -m maturin build --release
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

## Inspect a BLF file

```bash
python3 print_blf_info.py trace.blf
python3 print_blf_info.py trace.blf --limit 100
python3 print_blf_info.py trace.blf --limit 0
```
