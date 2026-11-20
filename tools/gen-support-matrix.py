#!/usr/bin/env python3
"""Generate doc/blf-object-support.md from the parser sources.

The object type enum is the source of truth for which ids exist; the reader and
the TUI tell which of them have a field schema and a decoded list row. Running
this after adding a type keeps the matrix honest.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def read(path):
    return (ROOT / path).read_text()


types_rs = read("crates/blf-core/src/types.rs")
reader_rs = read("crates/blf-core/src/reader.rs")
tui_rs = read("crates/blf-tui/src/main.rs")

# Enum variants: `Name = 12,`
variant_by_id = {}
for match in re.finditer(r"^\s+(\w+) = (\d+),", types_rs, re.M):
    variant_by_id[int(match.group(2))] = match.group(1)

# Names come from `object_type_name` only; other tables in the same file reuse
# the `N => "name"` shape for services and sub-functions.
name_body = re.search(
    r"pub fn object_type_name\(value: u32\) -> &'static str \{(.*?)\n\}", types_rs, re.S
).group(1)
name_by_id = {}
for match in re.finditer(r'^\s*(\d+)(?:\.\.=(\d+))? => "(\w+)"', name_body, re.M):
    start = int(match.group(1))
    end = int(match.group(2)) if match.group(2) else start
    for value in range(start, end + 1):
        name_by_id[value] = match.group(3)

canvas = re.findall(r"ObjectType::(\w+)", reader_rs)
# The LIN records go through `parse_lin_object`, so the predicate lists them.
lin_body = re.search(r"fn is_lin_type\(.*?\n\}", reader_rs, re.S)
lin_variants = set(re.findall(r"ObjectType::(\w+)", lin_body.group(0))) if lin_body else set()
typed = {
    "CanMessage", "CanMessage2", "CanFdMessage", "CanFdMessage64", "CanError",
    "CanErrorExt", "CanFdError64", "AppText", "Unknown115",
}

schema_types = set(canvas)

shown = set()
for function in ("fn structured_event", "fn bus_event", "fn lin_event"):
    body = re.search(function + r"\(.*?\n\}\n", tui_rs, re.S)
    if body:
        shown |= set(re.findall(r"ObjectType::(\w+)", body.group(0)))
shown |= set(re.findall(r"Object::(\w+)\(", tui_rs))
# LIN records share one row renderer, so every LIN type is shown.
shown |= lin_variants
# The CAN error frames have their own display branches; the enum variants use
# short names while the matrix rows use the long ones.
shown |= {"CanError", "CanErrorExt", "CanFdError64", "CanMessage", "CanMessage2"}
shown |= set(re.findall(r"ObjectType::(\w+)", tui_rs))
# The Ethernet frames are rendered through the protocol dissection, so they have
# no event branch of their own.
shown |= {
    "EthernetFrame", "EthernetFrameEx", "EthernetFrameForwarded",
    "EthernetErrorEx", "EthernetErrorForwarded",
}

BUS_PREFIXES = [
    ("CAN_", "CAN"), ("CANFD", "CAN"), ("LIN_", "LIN"), ("ETHERNET_", "Ethernet"),
    ("MOST", "MOST"), ("FLEXRAY", "FlexRay"), ("FR_", "FlexRay"), ("AFDX", "AFDX"),
    ("A429", "A429"), ("WLAN", "WLAN"), ("KLINE", "K-Line"), ("J1708", "J1708"),
]


def bus_of(name):
    for prefix, label in BUS_PREFIXES:
        if name.startswith(prefix):
            return label
    return "General"


SKIP = {"Unknown", "LogContainer", "Unknown115", "Unknown115"}
rows = []
for type_id in sorted(name_by_id):
    name = name_by_id[type_id]
    variant = variant_by_id.get(type_id, "")
    if name.startswith("Reserved") or variant in SKIP:
        continue
    parsed = variant in typed or variant in lin_variants or variant in schema_types
    rows.append((type_id, name, bus_of(name), parsed, variant in shown))

lines = [
    "# BLF object support matrix",
    "",
    "Which object types this reader understands, and which of them produce a",
    "decoded row in the message list. Regenerate with:",
    "",
    "```sh",
    "python3 tools/gen-support-matrix.py",
    "```",
    "",
    "| id | type | bus | fields parsed | list row |",
    "|---:|---|---|---|---|",
]
for type_id, name, bus, parsed, visible in rows:
    lines.append(
        f"| {type_id} | `{name}` | {bus} | {'yes' if parsed else '—'} | "
        f"{'yes' if visible else '—'} |"
    )
parsed_total = sum(1 for row in rows if row[3])
visible_total = sum(1 for row in rows if row[4])
lines += [
    "",
    f"{len(rows)} object types: {parsed_total} with parsed fields, "
    f"{visible_total} with a decoded list row.",
]
(ROOT / "doc/blf-object-support.md").write_text("\n".join(lines) + "\n")

gaps = [row for row in rows if not (row[3] and row[4])]
print(f"{len(rows)} types, {parsed_total} parsed, {visible_total} shown")
if gaps:
    print("gaps:", ", ".join(f"{row[0]} {row[1]}" for row in gaps))
    sys.exit(1)
