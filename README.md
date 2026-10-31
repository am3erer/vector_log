# vector_log

Rust-backed Vector BLF parser with Python bindings.

Current P0 scope:

- `FileStatistics`
- `LogContainer` and zlib decompression
- `ObjectHeaderBase`, `ObjectHeader`, `ObjectHeader2`, `VarObjectHeader`
- `CanMessage`, `CanMessage2`, `CanFdMessage`, `CanFdMessage64`
- `CanErrorFrame`, `CanErrorFrameExt`, `CanFdErrorFrame64`
- `AppText`, `RestorePointContainer`

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
    print(obj["class"], obj["objectType"])
reader.close()
```

The Python binding currently returns dictionaries. Typed Python classes matching the C++ class names are a follow-up after the P0 core is stable.
