# Design
## Source of truth
Status: Active. Date: 2026-11-04. Product surface: the Rust terminal UI in
`crates/blf-tui`, backed by `blf-core`. Evidence reviewed: the BLF reader,
DBC/LDF database APIs, lazy signal queries, Python workflows, and the existing
README.

## Brand
The tool is a focused engineering instrument: calm, dense, trustworthy, and
easy to inspect over SSH. Trust comes from preserving raw bytes, timestamps,
decode status, and database binding context. Avoid decorative dashboards,
hidden automatic database selection, and visual noise that competes with data.

## Product goals
- Open a BLF and locate messages quickly.
- Inspect raw protocol fields and decoded DBC/LDF signals together.
- Bind different databases to different CAN/LIN channels from the UI.
- Explore signal changes over time without leaving the terminal.
- Keep keyboard workflows complete; mouse input is an enhancement.

Non-goals for the first release: BLF writing, editing databases, live bus
capture, and a full replacement for a desktop measurement tool.

Success signals: a user can open a trace, filter to a channel or identifier,
load a database, expand a matching message, and open a useful signal plot in
under a minute.

## Personas and jobs
Primary users are automotive test engineers and developers investigating a
recorded CAN/LIN trace over a local terminal, SSH session, or tmux. Their jobs
are to correlate a raw frame with a database definition, understand errors,
and check whether a signal changes at the expected times.

## Information architecture
The first screen is a three-band analysis surface:
- Header: file name, object count, and current database bindings.
- Main split: scrollable message list on the left and selected-message detail
  on the right.
- Footer: mode, status, and discoverable key commands.

Modes are `Messages`, `Detail`, `Curve`, and transient `Input`. `b` opens a
database binding prompt, `/` opens filtering, `Enter` enters detail, `p` opens
the curve for the selected signal, and `Esc` returns to the previous mode.

## Design principles
- Show protocol truth before interpretation: timestamp, bus, channel, ID,
  direction, DLC, payload, and status stay visible.
- Make database context explicit in the header and signal detail.
- Prefer bounded, predictable rendering for very large traces.
- Preserve an escape route: raw message detail remains useful when decoding
  fails or no database is loaded.
- Use one core data path; the TUI must not reimplement BLF parsing.

## Visual language
Use a restrained terminal palette: cyan for navigation and bindings, green for
successful decoded values, yellow for warnings, and red for decode errors.
Use compact monospace text, one-character borders, and no animation. The
layout should remain useful at 100 columns and degrade by clipping long raw
values rather than overlapping panels.

## Components
- `MessageList`: filtered, timestamp-ordered BLF object summaries.
- `MessageDetail`: raw metadata, payload, and decoded signal rows.
- `CurveView`: ASCII min/max envelope with a shared time axis.
- `BindingHeader`: per-bus/channel database state.
- `InputPrompt`: filter and database-path entry.
- `StatusBar`: mode, errors, and keyboard commands.

## Accessibility
Keyboard navigation is the primary contract. Every mouse action must have a
keyboard equivalent. Do not rely on color alone: statuses include text, and
selected rows use a marker. Avoid rapid motion and keep focus visible.

## Responsive behavior
At narrow terminal widths, the detail panel becomes the full-width view after
`Enter`; the message list remains available through `Esc`. At normal widths,
the list receives 42 percent of the main area and detail receives the rest.
Long payloads and paths are truncated at the viewport edge.

## Interaction states
Startup shows a progress-free deterministic scan and a readable error if the
BLF cannot be opened. Empty filters show `No matching messages`. A missing
database shows raw fields and `database: none`. Decode failures remain visible
as status text. Curve view shows an explicit empty/error state when no numeric
samples are available.

## Content voice
Use short, factual labels: `CAN`, `LIN`, `Rx`, `Tx`, `header-only`,
`checksum-error`, `database`, and `signal`. Errors should identify the path,
channel, or signal that needs attention.

## Three-stage delivery

The TUI is delivered in three independently usable stages:

### Stage 1: trace inspection

- Open a BLF and scan message summaries into a predictable navigable view.
- Filter the complete summary set and keep selection visible while scrolling.
- Support `j/k`, arrows, PageUp/PageDown, Home/End, Enter, and Esc.
- Inspect raw protocol metadata, payload, object fields, and decode failures.
- Keep the raw view useful without a database.

Status: complete. The stage is covered by selection, filtering, navigation, and
UTF-8 rendering tests.

### Stage 2: database and signal analysis

- Bind separate DBC/LDF files to explicit bus/channel pairs at startup or with
  the `b` prompt.
- Decode CAN, CAN-FD, ordinary LIN, and diagnostic LIN signals in message detail.
- Preserve raw value, physical value, choice text, and decode errors.
- Open a lazy-query-backed numeric signal curve.
- Pan the curve with Left/Right, zoom with `+`/`-`, and jump with Home/End.

Status: complete for the current terminal viewer. The curve remains a single
selected-signal plot; multi-signal overlays and export are later extensions.

### Stage 3: terminal ergonomics and scale

- Enable ANSI alternate-screen, cursor, and SGR mouse reporting.
- Support mouse row selection, signal selection, and wheel navigation while
  retaining keyboard equivalents.
- Bound terminal-size probing and keep rendering deterministic at narrow widths.
- Preserve explicit status text and visible selection markers for color-blind
  and SSH/tmux use.

Status: complete for the current MVP. Disk-backed indexing/background loading
is still a future scale optimization for very large BLF files.

## Implementation constraints
`blf-core` owns BLF parsing, database parsing, decoding, timestamps, and lazy
queries. `blf-tui` owns only application state and terminal rendering. The
current implementation uses Rust's standard library plus ANSI control
sequences so the binary remains dependency-light and independent from Python.
The first implementation scans objects into memory for predictable navigation;
future work may add a disk-backed index and background loading. Every feature
must have Rust tests where practical and pass workspace format, test, and
Clippy checks.

## Open questions
- [ ] Should large-file navigation move to a disk-backed index after real
  traces establish a memory threshold? Owner: maintainer. Impact: performance.
- [ ] Should curve export support CSV or only terminal inspection initially?
  Owner: maintainer. Impact: analysis workflow.
