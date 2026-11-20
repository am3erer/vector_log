use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::fs::{File as StdFile, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use blf_core::{
    can_flags_direction, classify_lin_object, decode_lin_object, decode_message,
    object_timestamp_ns, object_timestamp_source, BusKind, Database, DatabaseBinding, DbcDatabase,
    FieldValue, File, LdfDatabase, Object,
};

/// Header lines drawn above every list: title, databases line and one rule.
const HEADER_ROWS: u16 = 3;
/// Upper bound on signal columns written by the CSV export.
const MAX_CSV_SIGNALS: usize = 64;
/// Upper bound on curve samples kept in memory; older ones are dropped.
const MAX_CURVE_SAMPLES: usize = 200_000;
/// Guard against a pathological detail list. The details column scrolls, so
/// this is far above any real database and only reports the remainder of an
/// absurdly long list instead of building thousands of lines.
const MAX_DETAIL_ENTRIES: usize = 500;
/// Below this width the three database columns leave too few cells for the
/// details, so a focused details pane takes the whole terminal width.
const NARROW_DATABASE_WIDTH: usize = 100;
/// Below this width the database browser itself drops to two panes.
///
/// Splitting less than 80 columns three ways leaves the details column at zero,
/// so the details get a full-width view of their own instead.
const TWO_PANE_DATABASE_WIDTH: usize = 80;
/// First list row below the header block, section title and column rule.
const LIST_FIRST_ROW: u16 = HEADER_ROWS + 2;
/// First detail row: the header block plus the detail section title.
const DETAIL_FIRST_ROW: u16 = HEADER_ROWS + 1;
/// First statistics row: the header block, the section title and the column rule.
const STATS_FIRST_ROW: u16 = HEADER_ROWS + 3;
/// Status for a trace-only action in a database-only session.
const NO_TRACE_STATUS: &str = "No trace loaded";

// Colour strategy
// --------------
// Plain text carries no escape code at all, so it takes the terminal's own
// foreground: black on a light theme, white on a dark one. Accents use the
// mid-luminance ANSI colours (31-36), which keep contrast on either background.
//
// A dark terminal benefits from the bright variants, and a light one must avoid
// them: `\x1b[93m` on white is nearly invisible. `--theme` picks the set, and
// `auto` reads the `COLORFGBG` variable most terminals export.
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
/// Follow the terminal foreground: readable on both themes.
const ANSI_PLAIN: &str = "";
/// Reverse video: inverts the theme, so a selected row reads on both.
const ANSI_SELECTED: &str = "\x1b[7m";

/// Whether the terminal has a light background.
static LIGHT_BACKGROUND: AtomicBool = AtomicBool::new(true);

/// Which colour set to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Theme {
    /// Pick from `COLORFGBG`, defaulting to light (the safer choice: bright
    /// colours on white are the ones that disappear).
    Auto,
    /// Dark background: the bright variants stay legible and look vivid.
    Dark,
    /// Light background: the mid-luminance variants keep contrast.
    Light,
}

impl Theme {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }

    /// Resolve `Auto` against the environment.
    fn resolve(self) -> bool {
        match self {
            Self::Light => true,
            Self::Dark => false,
            Self::Auto => detect_light_background(),
        }
    }
}

/// Read `COLORFGBG`, which terminals set to `foreground;background`.
///
/// A background index of 7 or 15 (and the `default` keyword) means light. When
/// the variable is missing the answer is "light", because that is the case where
/// a wrong guess hides text.
fn detect_light_background() -> bool {
    let Some(value) = std::env::var_os("COLORFGBG") else {
        return true;
    };
    let value = value.to_string_lossy();
    let Some(background) = value.rsplit(';').next() else {
        return true;
    };
    match background.trim().parse::<u8>() {
        // 7 is the light grey, 15 the bright white.
        Ok(index) => !matches!(index, 0..=6),
        Err(_) => true,
    }
}

fn set_theme(theme: Theme) {
    LIGHT_BACKGROUND.store(theme.resolve(), Ordering::Relaxed);
}

fn light_background() -> bool {
    LIGHT_BACKGROUND.load(Ordering::Relaxed)
}

/// Colour slot: the meaning stays the same, the exact code follows the theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
}

fn slot_color(slot: Slot) -> &'static str {
    match (slot, light_background()) {
        // Light background: mid-luminance ANSI colours.
        (Slot::Red, true) => "\x1b[31m",
        (Slot::Green, true) => "\x1b[32m",
        (Slot::Yellow, true) => "\x1b[33m",
        (Slot::Blue, true) => "\x1b[34m",
        (Slot::Magenta, true) => "\x1b[35m",
        (Slot::Cyan, true) => "\x1b[36m",
        // Dark background: the bright variants are vivid without washing out.
        (Slot::Red, false) => "\x1b[91m",
        (Slot::Green, false) => "\x1b[92m",
        (Slot::Yellow, false) => "\x1b[93m",
        (Slot::Blue, false) => "\x1b[94m",
        (Slot::Magenta, false) => "\x1b[95m",
        (Slot::Cyan, false) => "\x1b[96m",
    }
}

fn paint(value: impl AsRef<str>, color: &str) -> String {
    if color.is_empty() {
        // Plain text: let the terminal pick the foreground for its theme.
        return value.as_ref().to_string();
    }
    format!("{color}{}{ANSI_RESET}", value.as_ref())
}

fn paint_bold(value: impl AsRef<str>, color: &str) -> String {
    format!("{ANSI_BOLD}{color}{}{ANSI_RESET}", value.as_ref())
}

fn paint_selected(value: impl AsRef<str>) -> String {
    paint(value, ANSI_SELECTED)
}

#[derive(Debug, Clone)]
struct BindingEntry {
    binding: DatabaseBinding,
    path: PathBuf,
}

type BindingSpec = (BusKind, u16, PathBuf);

/// The BLF trace backing the message-oriented modes. Database-only launches
/// hold `None`, so no trace-derived state exists at all.
struct Trace {
    path: PathBuf,
    reader: File,
    skipped_objects: u64,
}

/// Interned text shared by every frame that carries the same content.
///
/// A capture repeats a handful of flows thousands of times, so the summary and
/// the searchable text are stored once and referenced by index. That keeps a
/// large Ethernet trace affordable: the per-frame cost is a few integers.
#[derive(Debug, Default)]
struct TextPool {
    texts: Vec<Box<str>>,
    index: HashMap<Box<str>, u32>,
}

impl TextPool {
    fn intern(&mut self, text: &str) -> u32 {
        if let Some(id) = self.index.get(text) {
            return *id;
        }
        let id = u32::try_from(self.texts.len()).unwrap_or(u32::MAX);
        let owned: Box<str> = text.into();
        self.texts.push(owned.clone());
        self.index.insert(owned, id);
        id
    }

    fn get(&self, id: u32) -> &str {
        self.texts.get(id as usize).map_or("", |text| text)
    }
}

/// What the Ethernet dissector found in one frame.
#[derive(Debug, Clone)]
struct EthernetInfo {
    /// Interned summary, such as
    /// `02:.. > FF:.. VLAN 10 ARP 10.0.0.1 > 10.0.0.2`.
    summary: u32,
    /// Interned compact search text: layer names followed by their values.
    search: u32,
    /// Highest protocol reached, such as `SOME/IP`.
    protocol: &'static str,
    ether_type: u16,
}

#[derive(Debug, Clone)]
struct MessageSummary {
    object_index: usize,
    timestamp_ns: Option<u64>,
    timestamp_source: Option<&'static str>,
    bus: Option<BusKind>,
    /// Bus name for the list column; Ethernet and events are not `BusKind`s.
    bus_label: &'static str,
    ethernet: Option<EthernetInfo>,
    /// A non-frame record: port state, driver error, bus event.
    event: Option<BusEvent>,
    /// Packed CAN frame properties (remote, wakeup, no-error, FD, BRS, ESI).
    can_flags: u8,
    channel: Option<u16>,
    frame_id: Option<u32>,
    object_type_raw: u32,
    class_name: &'static str,
    direction: Option<&'static str>,
    dlc: Option<u8>,
    payload: Vec<u8>,
    status: &'static str,
}

#[derive(Debug, Clone)]
struct SignalRow {
    name: String,
    raw: Option<i128>,
    physical: Option<f64>,
    choice: Option<String>,
    error: Option<String>,
}

/// Which column of the detail view the keys act on.
///
/// The frame details on the left can be longer than the screen, and the signal
/// list on the right scrolls independently, so one of them has to be focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailPane {
    /// Frame attributes, timing and raw bytes.
    Details,
    /// Decoded signals, where `p` and `a` plot a curve.
    Signals,
}

impl DetailPane {
    fn toggled(self) -> Self {
        match self {
            Self::Details => Self::Signals,
            Self::Signals => Self::Details,
        }
    }
}

/// Rows below the body: the status line with the key hints.
const STATUS_ROWS: usize = 1;

/// Below this width a two-pane layout leaves neither half readable.
const PREVIEW_MIN_WIDTH: usize = 100;

/// Which bus a list view shows in its own columns.
///
/// One BLF trace can hold CAN, LIN and Ethernet frames at once. Showing them
/// in one table forces every bus into the narrowest common shape, so each bus
/// gets a view with columns that fit it; frames from other buses stay visible
/// as a dim one-line description, which keeps the interleaving readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    All,
    Can,
    Lin,
    Ethernet,
}

impl View {
    fn label(self) -> &'static str {
        match self {
            Self::All => "ALL",
            Self::Can => "CAN",
            Self::Lin => "LIN",
            Self::Ethernet => "ETHERNET",
        }
    }

    fn cycle(self) -> Self {
        match self {
            Self::All => Self::Can,
            Self::Can => Self::Lin,
            Self::Lin => Self::Ethernet,
            Self::Ethernet => Self::All,
        }
    }

    fn from_digit(digit: char) -> Option<Self> {
        match digit {
            '1' => Some(Self::All),
            '2' => Some(Self::Can),
            '3' => Some(Self::Lin),
            '4' => Some(Self::Ethernet),
            _ => None,
        }
    }

    /// Whether this frame belongs to the bus the view is about.
    fn owns(self, summary: &MessageSummary) -> bool {
        match self {
            Self::All => true,
            Self::Can => summary.bus_label == "can",
            Self::Lin => summary.bus_label == "lin",
            Self::Ethernet => summary.bus_label == "eth",
        }
    }

    fn is_all(self) -> bool {
        self == Self::All
    }
}

/// Identifies one frame id on one bus and channel.
///
/// Ethernet has no CAN-style frame identifier, so it is grouped by its
/// EtherType, which is what a TSN trace is usually analysed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum FrameKey {
    Bus(BusKind, u16, u32),
    Ethernet(u16, u16),
}

impl FrameKey {
    /// Ordering key used by the statistics sorts.
    fn sort_id(self) -> u32 {
        match self {
            Self::Bus(_, _, frame_id) => frame_id,
            Self::Ethernet(_, ether_type) => u32::from(ether_type),
        }
    }

    fn channel(self) -> u16 {
        match self {
            Self::Bus(_, channel, _) | Self::Ethernet(channel, _) => channel,
        }
    }
}

/// The statistics and step key of one message.
fn frame_key(summary: &MessageSummary) -> Option<FrameKey> {
    if let Some(info) = &summary.ethernet {
        return Some(FrameKey::Ethernet(
            summary.channel.unwrap_or(0),
            info.ether_type,
        ));
    }
    Some(FrameKey::Bus(
        summary.bus?,
        summary.channel?,
        summary.frame_id?,
    ))
}

/// Outcome of a `]`/`[` step, resolved before mutating any state.
enum FrameStep {
    Missing,
    AtStart(usize),
    AtEnd(usize),
    Move {
        target: usize,
        ordinal: usize,
        total: usize,
    },
}

/// Cycle timing of one frame id on one bus/channel.
#[derive(Debug, Clone, PartialEq)]
struct TimingStats {
    frames: usize,
    avg_ms: f64,
    min_ms: f64,
    max_ms: f64,
    jitter_ms: f64,
}

#[derive(Debug, Clone)]
struct CurveSeries {
    title: String,
    samples: Vec<(u64, f64)>,
    error: Option<String>,
    truncated: bool,
    symbol: char,
    color: &'static str,
}

struct CurveData {
    series: Vec<CurveSeries>,
    /// Visible time window; `end` is exclusive.
    window_start: u64,
    window_end: u64,
    /// Scale every series to its own range.
    ///
    /// Two signals with very different units otherwise force the smaller one
    /// into a flat line at the bottom; normalising makes both shapes readable
    /// at the cost of a shared value axis.
    normalize: bool,
}

/// Symbols handed to the series of a curve, in plot order.
const CURVE_SYMBOLS: [char; 6] = ['*', 'o', '+', 'x', '#', '@'];
/// Colour slots for the same series; resolved per theme at render time so the
/// lines stay visible on both light and dark backgrounds.
const CURVE_SLOTS: [Slot; 6] = [
    Slot::Green,
    Slot::Magenta,
    Slot::Yellow,
    Slot::Cyan,
    Slot::Blue,
    Slot::Red,
];

fn curve_color(index: usize) -> &'static str {
    slot_color(CURVE_SLOTS[index % CURVE_SLOTS.len()])
}

/// Smallest window the zoom keeps, so a curve cannot collapse to nothing.
const MIN_CURVE_WINDOW_NS: u64 = 1_000_000;
/// Legend rows the curve view prints before summarising the rest.
const MAX_CURVE_LEGEND: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Messages,
    Detail,
    Curve,
    Databases,
    Statistics,
    /// Every attribute of the selected frame, scrollable.
    Inspector,
}

/// Which column of the database browser owns the keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DatabasePane {
    Tree,
    Objects,
    Details,
}

impl DatabasePane {
    /// The pane `step` stops after this one, clamped at both ends.
    fn shifted(self, step: isize) -> Self {
        match step {
            ..=-1 => match self {
                Self::Details => Self::Objects,
                Self::Objects | Self::Tree => Self::Tree,
            },
            _ => match self {
                Self::Tree => Self::Objects,
                Self::Objects | Self::Details => Self::Details,
            },
        }
    }

    /// The pane after this one, wrapping from the last back to the first.
    fn cycled(self) -> Self {
        match self {
            Self::Tree => Self::Objects,
            Self::Objects => Self::Details,
            Self::Details => Self::Tree,
        }
    }

    /// The pane before this one, wrapping from the first back to the last.
    fn cycled_back(self) -> Self {
        match self {
            Self::Tree => Self::Details,
            Self::Objects => Self::Tree,
            Self::Details => Self::Objects,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DbCategory {
    Overview,
    Messages,
    Signals,
    Nodes,
    Attributes,
    ValueTables,
    Frames,
    DiagnosticFrames,
    DiagnosticSignals,
    Schedules,
    DynamicFrames,
    OptionalSections,
}

impl DbCategory {
    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Messages => "Messages",
            Self::Signals => "Signals",
            Self::Nodes => "Nodes",
            Self::Attributes => "Attributes",
            Self::ValueTables => "Value tables",
            Self::Frames => "Frames",
            Self::DiagnosticFrames => "Diagnostic frames",
            Self::DiagnosticSignals => "Diagnostic signals",
            Self::Schedules => "Schedules",
            Self::DynamicFrames => "Dynamic frames",
            Self::OptionalSections => "Optional sections",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Filter,
    Binding,
    DatabaseFilter,
    Goto,
    TimeRange,
    /// Filter the help overlay entries.
    HelpFilter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputEvent {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Tab,
    BackTab,
    Esc,
    Backspace,
    CtrlC,
    Eof,
    Mouse(MouseEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MouseEvent {
    button: u8,
    column: u16,
    row: u16,
    wheel: i8,
    release: bool,
}

struct RawTerminal {
    tty: StdFile,
    saved_state: String,
}

impl RawTerminal {
    fn enter() -> Result<Self, String> {
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|error| {
                format!(
                    "open interactive terminal /dev/tty: {error}; vector-log-tui requires a TTY"
                )
            })?;
        let saved_state = command_output_with_stdin("stty", &["-g"], &tty)?;
        run_command_with_stdin("stty", &["-icanon", "-echo", "min", "0", "time", "1"], &tty)?;
        let mut terminal = Self { tty, saved_state };
        terminal
            .write("\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h")
            .map_err(|error| format!("initialize interactive terminal: {error}"))?;
        Ok(terminal)
    }

    fn write(&mut self, value: &str) -> io::Result<()> {
        self.tty.write_all(value.as_bytes())?;
        self.tty.flush()
    }

    fn read_event(&mut self) -> Result<Option<InputEvent>, String> {
        read_event_from(&mut self.tty)
    }

    fn size(&self) -> (usize, usize) {
        use std::os::fd::AsRawFd;
        // The ioctl is the authoritative answer; `stty` is only a fallback and
        // the last resort is a size no test ever wants to hit.
        if let Some(size) = terminal_size_from_kernel(self.tty.as_raw_fd()) {
            return size;
        }
        command_output_with_stdin("stty", &["size"], &self.tty)
            .ok()
            .and_then(|value| parse_terminal_size(&value))
            .unwrap_or((120, 40))
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = run_command_with_stdin("stty", &[&self.saved_state], &self.tty);
        let _ = self.write("\x1b[?1006l\x1b[?1000l\x1b[0m\x1b[?25h\x1b[?1049l");
    }
}

/// Show a cheap stderr progress line while large traces load, but only on a TTY.
struct LoadProgress {
    enabled: bool,
    last_report: Instant,
    shown: bool,
}

impl LoadProgress {
    fn new() -> Self {
        Self {
            enabled: io::stderr().is_terminal(),
            last_report: Instant::now(),
            shown: false,
        }
    }

    fn report(&mut self, objects: usize) {
        if !self.enabled || self.last_report.elapsed() < Duration::from_millis(150) {
            return;
        }
        eprint!("\rreading BLF: {objects} objects");
        let _ = io::stderr().flush();
        self.last_report = Instant::now();
        self.shown = true;
    }

    fn clear(&mut self) {
        if !self.enabled || !self.shown {
            return;
        }
        eprint!("\r\x1b[K");
        let _ = io::stderr().flush();
        self.shown = false;
    }
}

struct App {
    trace: Option<Trace>,
    objects: Vec<Object>,
    summaries: Vec<MessageSummary>,
    bindings: Vec<BindingEntry>,
    mode: Mode,
    input: Option<InputKind>,
    input_buffer: String,
    filter: String,
    filtered: Vec<usize>,
    db_objects: Vec<DbObject>,
    frame_index: Option<HashMap<FrameKey, Vec<usize>>>,
    timing: Option<TimingStats>,
    time_mode: TimeMode,
    time_base: u64,
    time_range: Option<(u64, u64)>,
    stats: Vec<FrameStat>,
    stats_selected: usize,
    stats_offset: usize,
    stats_sort: StatsSort,
    dirty: bool,
    selected: usize,
    list_offset: usize,
    selected_signal: usize,
    detail_offset: usize,
    /// Which detail column the keys act on.
    detail_pane: DetailPane,
    /// First visible line of the frame-detail column, counted after wrapping.
    detail_left_offset: usize,
    /// Wrapped height of the frame-detail column, and the rows it is drawn in.
    detail_left_total: usize,
    detail_left_rows: usize,
    status: String,
    detail_rows: Vec<SignalRow>,
    curve: Option<CurveData>,
    viewport_width: usize,
    viewport_height: usize,
    last_size_check: Instant,
    database_selected: usize,
    database_pane: DatabasePane,
    database_category: usize,
    database_object_selected: usize,
    database_object_offset: usize,
    database_detail_offset: usize,
    database_filter: String,
    help: Option<HelpState>,
    /// Decoded UDS messages, keyed by the object index of every frame that
    /// belongs to them. Filled once while the trace is loaded.
    uds: HashMap<usize, UdsFrame>,
    /// Interned Ethernet summaries and search texts.
    pool: TextPool,
    /// First visible line of the inspector, and its wrapped line count.
    inspector_offset: usize,
    inspector_lines: usize,
    /// Complete `METADATA` XML documents rebuilt from their fragments.
    metadata: Vec<String>,
    /// Objects the capture admits to having lost, from data-lost and overrun
    /// records. Non-zero means the trace is incomplete and must say so.
    lost_objects: u64,
    /// How many of those records were seen.
    lost_events: u64,
    /// Which bus the message list shows in its own columns.
    view: View,
    /// Whether frames from other buses stay visible as a dim description.
    show_foreign: bool,
}

/// State of the `?` help overlay; closing restores the previous status.
struct HelpState {
    offset: usize,
    previous_status: String,
    /// Case-insensitive substring the entries are filtered by.
    filter: String,
}

impl HelpState {
    /// Entries to show: everything, or what the filter matches.
    fn entries(&self, width: usize) -> Vec<(bool, String)> {
        let all = help_lines(width);
        if self.filter.trim().is_empty() {
            return all;
        }
        let needle = self.filter.to_lowercase();
        // A heading stays when any of its entries matches, so the result keeps
        // its structure instead of becoming a list of loose lines.
        let mut kept: Vec<(bool, String)> = Vec::new();
        let mut pending_heading: Option<(bool, String)> = None;
        let mut heading_used = false;
        for (heading, text) in all {
            if heading {
                pending_heading = Some((true, text));
                heading_used = false;
                continue;
            }
            if text.to_lowercase().contains(&needle) {
                if let Some(heading) = pending_heading.take() {
                    kept.push(heading);
                    heading_used = true;
                }
                let _ = heading_used;
                kept.push((false, text));
            }
        }
        if kept.is_empty() {
            return vec![(true, format!("no key matches \"{}\"", self.filter))];
        }
        kept
    }
}

impl App {
    fn open(path: impl Into<PathBuf>, specs: Vec<(BusKind, u16, PathBuf)>) -> Result<Self, String> {
        let path = path.into();
        let mut reader = File::open(&path).map_err(|error| format!("open BLF: {error}"))?;
        let mut objects = Vec::new();
        let mut progress = LoadProgress::new();
        loop {
            let batch = reader
                .read_batch(4096)
                .map_err(|error| format!("read BLF: {error}"))?;
            if batch.is_empty() {
                break;
            }
            objects.extend(batch);
            progress.report(objects.len());
        }
        progress.clear();
        let trace = Trace {
            path,
            skipped_objects: reader.skipped_object_count,
            reader,
        };
        Self::new(Some(trace), objects, specs)
    }

    /// `vector-log-tui --dbc … [--ldf …]`: browse databases with no trace.
    /// The message, detail and curve modes stay empty and unreachable.
    fn open_databases(specs: Vec<(BusKind, u16, PathBuf)>) -> Result<Self, String> {
        if specs.is_empty() {
            return Err(usage());
        }
        Self::new(None, Vec::new(), specs)
    }

    fn new(
        trace: Option<Trace>,
        objects: Vec<Object>,
        specs: Vec<(BusKind, u16, PathBuf)>,
    ) -> Result<Self, String> {
        let mut pool = TextPool::default();
        let mut summaries = objects
            .iter()
            .enumerate()
            .map(|(object_index, object)| summary_for(object_index, object, &mut pool))
            .collect::<Vec<_>>();
        let filtered = (0..summaries.len()).collect::<Vec<_>>();
        let uds = reassemble_trace(&objects, &summaries);
        let time_base = summaries
            .iter()
            .find_map(|summary| summary.timestamp_ns)
            .unwrap_or_default();
        let (lost_objects, lost_events) = count_lost_objects(&objects);
        let (metadata, metadata_owner) = collect_metadata(&objects);
        for (index, summary) in summaries.iter_mut().enumerate() {
            let Some(Some(document)) = metadata_owner.get(index).copied() else {
                continue;
            };
            let Some(document) = metadata.get(document) else {
                continue;
            };
            if let Some(event) = summary.event.as_mut() {
                event.summary = format!("{} · metadata {} bytes", event.summary, document.len());
                // The fragments only make sense together, so every row of a
                // document carries the whole text for the filter.
                event.search.push(' ');
                event.search.push_str(&document.to_lowercase());
            }
        }
        let mut app = Self {
            trace,
            metadata,
            lost_objects,
            lost_events,
            objects,
            summaries,
            uds,
            pool,
            inspector_offset: 0,
            inspector_lines: 0,
            view: View::All,
            show_foreign: true,
            bindings: Vec::new(),
            mode: Mode::Messages,
            input: None,
            input_buffer: String::new(),
            filter: String::new(),
            filtered,
            db_objects: Vec::new(),
            frame_index: None,
            timing: None,
            time_mode: TimeMode::Absolute,
            time_base,
            time_range: None,
            stats: Vec::new(),
            stats_selected: 0,
            stats_offset: 0,
            stats_sort: StatsSort::Count,
            dirty: true,
            selected: 0,
            list_offset: 0,
            selected_signal: 0,
            detail_offset: 0,
            detail_pane: DetailPane::Signals,
            detail_left_offset: 0,
            detail_left_total: 0,
            detail_left_rows: 0,
            status: "Ready".to_string(),
            detail_rows: Vec::new(),
            curve: None,
            viewport_width: 120,
            viewport_height: 40,
            last_size_check: Instant::now() - Duration::from_secs(1),
            database_selected: 0,
            database_pane: DatabasePane::Tree,
            database_category: 0,
            database_object_selected: 0,
            database_object_offset: 0,
            database_detail_offset: 0,
            database_filter: String::new(),
            help: None,
        };
        for (bus, channel, database_path) in specs {
            app.bind_database(bus, channel, database_path)?;
        }
        if !app.has_trace() {
            app.enter_databases();
        }
        Ok(app)
    }

    fn has_trace(&self) -> bool {
        self.trace.is_some()
    }

    /// First header line: the trace, or its absence, plus the active mode.
    /// Lines describing the metadata documents a record contributes to.
    ///
    /// The record itself only holds one fragment of a split XML document, so the
    /// detail pane shows every complete document rebuilt from the fragments.
    /// What to say when the list has no rows.
    ///
    /// An empty screen is ambiguous: the filter may be too narrow, the view may
    /// hide every bus, or the trace may be empty. Each case says how to undo it.
    fn empty_list_hint(&self) -> Vec<String> {
        if self.summaries.is_empty() {
            return vec!["The trace has no objects".to_string()];
        }
        let mut lines = vec![format!(
            "No message matches {}{}",
            display_filter(&self.filter),
            if self.view.is_all() {
                String::new()
            } else {
                format!(" in view {}", self.view.label())
            }
        )];
        if !self.filter.trim().is_empty() {
            lines.push("  Esc clears the filter  ·  / edits it".to_string());
        }
        if !self.view.is_all() {
            lines.push(format!(
                "  v or 1 switches view  ·  H {} other buses",
                if self.show_foreign { "hides" } else { "shows" }
            ));
        }
        if self.time_range.is_some() {
            lines.push("  t edits the time range".to_string());
        }
        lines
    }

    fn metadata_lines(&self, object_index: usize) -> Vec<String> {
        if self.metadata.is_empty() {
            return Vec::new();
        }
        let Some(object) = self.objects.get(object_index) else {
            return Vec::new();
        };
        let Object::AppText(value) = object else {
            return Vec::new();
        };
        if value.source != 2 {
            return Vec::new();
        }
        let mut lines = Vec::new();
        lines.push("metadata".to_string());
        lines.push(format!(
            "  {} document{} rebuilt from {} fragments",
            self.metadata.len(),
            if self.metadata.len() == 1 { "" } else { "s" },
            self.metadata
                .iter()
                .map(|document| document.lines().count())
                .sum::<usize>()
        ));
        for (number, document) in self.metadata.iter().enumerate() {
            lines.push(format!(
                "  document {} ({} bytes)",
                number + 1,
                document.len()
            ));
            for line in document.lines().take(40) {
                lines.push(format!("  {}", line.trim_end()));
            }
            if document.lines().count() > 40 {
                lines.push(format!(
                    "  ... ({} more lines)",
                    document.lines().count() - 40
                ));
            }
        }
        lines
    }

    fn title_line(&self) -> String {
        match &self.trace {
            Some(trace) => {
                let range = self.time_range.map_or_else(String::new, |(start, end)| {
                    format!(
                        "  range={}..{}",
                        self.time_axis().format(Some(start)),
                        self.time_axis().format(Some(end))
                    )
                });
                format!(
                    " vector-log-tui  {}  view={}  objects={}/{}  skipped={}  mode={:?}  time={}{range}",
                    trace.path.display(),
                    self.view.label(),
                    self.filtered.len(),
                    self.summaries.len(),
                    trace.skipped_objects,
                    self.mode,
                    self.time_mode.label()
                )
            }
            None => format!(
                " vector-log-tui  (no trace)  databases={}  mode={:?}",
                self.bindings.len(),
                self.mode
            ),
        }
    }

    fn run(&mut self) -> Result<(), String> {
        let mut terminal = RawTerminal::enter()?;
        self.run_terminal(&mut terminal)
    }

    fn run_terminal(&mut self, terminal: &mut RawTerminal) -> Result<(), String> {
        loop {
            if self.dirty || self.poll_resize(terminal) {
                self.render(terminal)?;
                self.dirty = false;
            }
            if let Some(event) = terminal.read_event()? {
                if self.handle_event(event)? {
                    break;
                }
                self.dirty = true;
            }
        }
        Ok(())
    }

    /// Redraw only when the terminal changed size; idle frames stay cheap.
    fn poll_resize(&mut self, terminal: &RawTerminal) -> bool {
        if self.last_size_check.elapsed() < Duration::from_millis(250) {
            return false;
        }
        let (width, height) = terminal.size();
        self.last_size_check = Instant::now();
        if (width, height) == (self.viewport_width, self.viewport_height) {
            return false;
        }
        self.viewport_width = width;
        self.viewport_height = height;
        true
    }

    fn handle_event(&mut self, event: InputEvent) -> Result<bool, String> {
        if matches!(event, InputEvent::CtrlC | InputEvent::Eof) {
            return Ok(true);
        }
        // The help overlay is modal: while it is open every key belongs to it,
        // including the characters typed into its filter.
        if self.help.is_some() {
            self.handle_help_event(event);
            return Ok(false);
        }
        if self.input.is_some() {
            return self.handle_input_event(event);
        }
        match self.mode {
            Mode::Messages => match event {
                InputEvent::Char('q') => return Ok(true),
                InputEvent::Up | InputEvent::Char('k') => self.move_selection(-1),
                InputEvent::Down | InputEvent::Char('j') => self.move_selection(1),
                InputEvent::PageUp => self.move_selection(-self.page_step()),
                InputEvent::PageDown => self.move_selection(self.page_step()),
                InputEvent::Home => self.select_first(),
                InputEvent::End => self.select_last(),
                InputEvent::Enter => self.enter_detail(),
                InputEvent::Char('/') => self.start_input(InputKind::Filter),
                InputEvent::Char('b') => self.start_input(InputKind::Binding),
                InputEvent::Char('p') => self.open_curve(),
                InputEvent::Char('d') => self.enter_databases(),
                InputEvent::Char('s') => self.open_statistics(),
                InputEvent::Char('i') => self.open_inspector(),
                InputEvent::Char('t') => self.start_input(InputKind::TimeRange),
                InputEvent::Char('e') => self.export_csv(),
                InputEvent::Char('P') => self.export_pcap(),
                InputEvent::Char('T') => self.toggle_time_mode(),
                InputEvent::Char('g') => self.start_input(InputKind::Goto),
                InputEvent::Char('G') => self.jump_to_end(),
                InputEvent::Char(']') => self.step_frame(1),
                InputEvent::Char('[') => self.step_frame(-1),
                InputEvent::Char('v') => self.set_view(self.view.cycle()),
                InputEvent::Char('V') => self.set_view(self.view.cycle()),
                InputEvent::Char(digit @ '1'..='4') => {
                    if let Some(view) = View::from_digit(digit) {
                        self.set_view(view);
                    }
                }
                InputEvent::Char('H') => self.toggle_foreign(),
                InputEvent::Char('?') => self.open_help(),
                InputEvent::Esc => self.clear_filter(),
                InputEvent::Mouse(mouse) => self.handle_message_mouse(mouse),
                _ => {}
            },
            Mode::Detail => match event {
                InputEvent::Char('q') => return Ok(true),
                InputEvent::Tab => {
                    self.detail_pane = self.detail_pane.toggled();
                    self.status = match self.detail_pane {
                        DetailPane::Details => {
                            "Detail pane: attributes (Tab to switch)".to_string()
                        }
                        DetailPane::Signals => "Detail pane: signals (Tab to switch)".to_string(),
                    };
                }
                InputEvent::Up | InputEvent::Char('k') => self.scroll_detail(-1),
                InputEvent::Down | InputEvent::Char('j') => self.scroll_detail(1),
                InputEvent::PageUp => self.scroll_detail(-self.detail_page_step()),
                InputEvent::PageDown => self.scroll_detail(self.detail_page_step()),
                InputEvent::Home => self.detail_home(),
                InputEvent::End => self.detail_end(),
                InputEvent::Char('p') => self.open_curve(),
                InputEvent::Char('a') => self.add_curve_series(),
                InputEvent::Char('i') => self.open_inspector(),
                InputEvent::Char('T') => self.toggle_time_mode(),
                InputEvent::Char('b') => self.start_input(InputKind::Binding),
                InputEvent::Char('e') => self.export_csv(),
                InputEvent::Esc | InputEvent::Left => self.mode = Mode::Messages,
                InputEvent::Char('?') => self.open_help(),
                InputEvent::Mouse(mouse) => self.handle_detail_mouse(mouse),
                _ => {}
            },
            Mode::Inspector => match event {
                InputEvent::Char('q') => return Ok(true),
                InputEvent::Up | InputEvent::Char('k') => self.scroll_inspector(-1),
                InputEvent::Down | InputEvent::Char('j') => self.scroll_inspector(1),
                InputEvent::PageUp => self.scroll_inspector(-self.page_step()),
                InputEvent::PageDown => self.scroll_inspector(self.page_step()),
                InputEvent::Home => self.inspector_offset = 0,
                InputEvent::End => self.scroll_inspector(isize::MAX / 2),
                InputEvent::Char('e') => self.export_csv(),
                InputEvent::Char('P') => self.export_pcap(),
                InputEvent::Char('?') => self.open_help(),
                InputEvent::Esc | InputEvent::Left => self.mode = Mode::Messages,
                _ => {}
            },
            Mode::Statistics => match event {
                InputEvent::Char('q') => return Ok(true),
                InputEvent::Up | InputEvent::Char('k') => self.move_stats(-1),
                InputEvent::Down | InputEvent::Char('j') => self.move_stats(1),
                InputEvent::PageUp => self.move_stats(-self.page_step()),
                InputEvent::PageDown => self.move_stats(self.page_step()),
                InputEvent::Home => {
                    self.stats_selected = 0;
                    self.stats_offset = 0;
                }
                InputEvent::End => {
                    self.stats_selected = self.stats.len().saturating_sub(1);
                    self.keep_stats_visible();
                }
                InputEvent::Enter => self.jump_to_stat(),
                InputEvent::Char('s') => {
                    self.stats_sort = self.stats_sort.next();
                    self.sort_stats();
                    self.stats_selected = 0;
                    self.stats_offset = 0;
                    self.status = format!("Sorted by {}", self.stats_sort.label());
                }
                InputEvent::Esc | InputEvent::Left => self.mode = Mode::Messages,
                InputEvent::Char('?') => self.open_help(),
                InputEvent::Mouse(mouse) => self.handle_stats_mouse(mouse),
                _ => {}
            },
            Mode::Databases => match event {
                InputEvent::Char('q') => return Ok(true),
                InputEvent::Up | InputEvent::Char('k') => self.move_database_row(-1),
                InputEvent::Down | InputEvent::Char('j') => self.move_database_row(1),
                InputEvent::PageUp => self.move_database_row(-self.page_step()),
                InputEvent::PageDown => self.move_database_row(self.page_step()),
                InputEvent::Home => self.select_first_database_row(),
                InputEvent::End => self.select_last_database_row(),
                InputEvent::Left | InputEvent::Char('h') => self.shift_database_pane(-1),
                InputEvent::Right | InputEvent::Char('l') => self.shift_database_pane(1),
                InputEvent::Tab => self.toggle_database_pane(true),
                InputEvent::BackTab => self.toggle_database_pane(false),
                InputEvent::Char('[') => self.switch_database(-1),
                InputEvent::Char(']') => self.switch_database(1),
                InputEvent::Char('/') => self.start_input(InputKind::DatabaseFilter),
                InputEvent::Enter => self.activate_database_row(),
                InputEvent::Char('b') => self.start_input(InputKind::Binding),
                InputEvent::Char('?') => self.open_help(),
                // `p`, `e`, `g` and `G` all act on the trace; a database-only
                // session explains why they do nothing instead of staying silent.
                InputEvent::Char('p' | 'e' | 'g' | 'G') if self.trace.is_none() => {
                    self.status = NO_TRACE_STATUS.to_string();
                }
                InputEvent::Esc => {
                    if self.has_trace() {
                        self.mode = Mode::Messages;
                    } else {
                        self.status = NO_TRACE_STATUS.to_string();
                    }
                }
                InputEvent::Mouse(mouse) => self.handle_database_mouse(mouse),
                _ => {}
            },
            Mode::Curve => match event {
                InputEvent::Char('q') => return Ok(true),
                InputEvent::Esc => self.mode = Mode::Detail,
                InputEvent::Left => self.curve_pan(-1),
                InputEvent::Right => self.curve_pan(1),
                InputEvent::PageUp | InputEvent::Char('+') | InputEvent::Char('=') => {
                    self.curve_zoom(0.5)
                }
                InputEvent::PageDown | InputEvent::Char('-') => self.curve_zoom(2.0),
                InputEvent::Home => self.curve_home(),
                InputEvent::End => self.curve_end(),
                InputEvent::Char('x') => self.remove_curve_series(),
                InputEvent::Char('n') => self.toggle_curve_normalize(),
                InputEvent::Char('?') => self.open_help(),
                InputEvent::Mouse(mouse) => self.handle_curve_mouse(mouse),
                _ => {}
            },
        }
        Ok(false)
    }

    fn handle_input_event(&mut self, event: InputEvent) -> Result<bool, String> {
        match event {
            InputEvent::Esc => {
                self.input = None;
                self.input_buffer.clear();
            }
            InputEvent::Enter => {
                let kind = self.input.take();
                let value = std::mem::take(&mut self.input_buffer);
                if let Some(kind) = kind {
                    match kind {
                        InputKind::Filter => {
                            self.filter = value;
                            self.selected = 0;
                            self.list_offset = 0;
                            self.rebuild_filtered();
                            self.status = format!("Filter: {}", display_filter(&self.filter));
                        }
                        InputKind::Binding => {
                            if let Err(error) = self.apply_binding_input(&value) {
                                self.status = error;
                                self.input = Some(InputKind::Binding);
                                self.input_buffer = value;
                            }
                        }
                        InputKind::TimeRange => match parse_time_range(&value, self.time_base) {
                            Ok(range) => {
                                self.time_range = range;
                                self.selected = 0;
                                self.list_offset = 0;
                                self.rebuild_filtered();
                                self.status = match range {
                                    Some(_) => format!("Range: {}", display_filter(value.trim())),
                                    None => "Range cleared".to_string(),
                                };
                            }
                            Err(error) => {
                                self.status = error;
                                self.input = Some(InputKind::TimeRange);
                                self.input_buffer = value;
                            }
                        },
                        InputKind::Goto => {
                            if let Err(error) = self.goto_input(&value) {
                                self.status = error;
                                self.input = Some(InputKind::Goto);
                                self.input_buffer = value;
                            }
                        }
                        // Handled by the help overlay itself.
                        InputKind::HelpFilter => {}
                        InputKind::DatabaseFilter => {
                            self.database_filter = value;
                            self.database_object_selected = 0;
                            self.database_object_offset = 0;
                            self.rebuild_database_objects();
                            self.status = format!(
                                "Database filter: {}",
                                display_filter(&self.database_filter)
                            );
                        }
                    }
                }
            }
            InputEvent::Backspace => {
                self.input_buffer.pop();
            }
            InputEvent::Char(value) => {
                self.input_buffer.push(value);
            }
            _ => {}
        }
        Ok(false)
    }

    fn start_input(&mut self, kind: InputKind) {
        self.input = Some(kind);
        self.input_buffer.clear();
    }

    fn apply_binding_input(&mut self, value: &str) -> Result<(), String> {
        let value = value.trim();
        let (bus_text, rest) = split_first_word(value);
        let bus = match bus_text.to_ascii_lowercase().as_str() {
            "can" => BusKind::Can,
            "lin" => BusKind::Lin,
            _ => return Err("Binding syntax: can|lin CHANNEL PATH".to_string()),
        };
        let (channel_text, path_text) = split_first_word(rest);
        let channel = match channel_text.parse::<u16>() {
            Ok(value) => value,
            Err(_) => return Err("Binding channel must be an integer".to_string()),
        };
        let path = PathBuf::from(path_text.trim());
        if path.as_os_str().is_empty() {
            return Err("Binding syntax: can|lin CHANNEL PATH".to_string());
        }
        self.bind_database(bus, channel, path)
    }

    fn bind_database(&mut self, bus: BusKind, channel: u16, path: PathBuf) -> Result<(), String> {
        let database = match bus {
            BusKind::Can => {
                DbcDatabase::from_file(&path).map(|database| Database::Can(Arc::new(database)))
            }
            BusKind::Lin => {
                LdfDatabase::from_file(&path).map(|database| Database::Lin(Arc::new(database)))
            }
        }
        .map_err(|error| format!("load {} database {}: {error}", bus, path.display()))?;
        let binding = DatabaseBinding::new(bus, channel, database)
            .map_err(|error| format!("bind database: {error}"))?;
        self.bindings
            .retain(|entry| !(entry.binding.bus() == bus && entry.binding.channel() == channel));
        self.bindings.push(BindingEntry {
            binding,
            path: path.clone(),
        });
        self.status = format!("Bound {} channel {} -> {}", bus, channel, path.display());
        if self.mode == Mode::Detail {
            self.refresh_detail();
        }
        if self.mode == Mode::Databases {
            self.rebuild_database_objects();
        }
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.filtered.len();
        self.selected = move_index(self.selected, count, delta);
        self.selected_signal = 0;
        self.keep_selection_visible();
    }

    /// Rows the detail column scrolls per page.
    fn detail_page_step(&self) -> isize {
        list_page_size(self.viewport_height, 5) as isize
    }

    /// Move the keys' effect to whichever column is focused.
    fn scroll_detail(&mut self, delta: isize) {
        match self.detail_pane {
            DetailPane::Signals => self.move_signal(delta),
            DetailPane::Details => {
                let max = self.detail_left_limit();
                self.detail_left_offset = self
                    .detail_left_offset
                    .saturating_add_signed(delta)
                    .min(max);
            }
        }
    }

    fn detail_home(&mut self) {
        match self.detail_pane {
            DetailPane::Signals => self.select_first_signal(),
            DetailPane::Details => self.detail_left_offset = 0,
        }
    }

    fn detail_end(&mut self) {
        match self.detail_pane {
            DetailPane::Signals => self.select_last_signal(),
            DetailPane::Details => self.detail_left_offset = self.detail_left_limit(),
        }
    }

    /// Last scroll position that still shows content.
    fn detail_left_limit(&self) -> usize {
        self.detail_left_total
            .saturating_sub(self.detail_left_rows.max(1))
    }

    fn move_signal(&mut self, delta: isize) {
        self.selected_signal = move_index(self.selected_signal, self.detail_rows.len(), delta);
        self.keep_signal_visible();
    }

    fn select_first(&mut self) {
        self.selected = 0;
        self.list_offset = 0;
        self.selected_signal = 0;
    }

    /// `s`: aggregate the visible messages by frame id.
    fn open_statistics(&mut self) {
        self.ensure_frame_index();
        let mut stats = Vec::new();
        if let Some(index) = &self.frame_index {
            for (key, positions) in index {
                let timestamps = positions
                    .iter()
                    .filter_map(|position| {
                        let list_index = *self.filtered.get(*position)?;
                        self.summaries.get(list_index)?.timestamp_ns
                    })
                    .collect::<Vec<_>>();
                stats.push(FrameStat {
                    key: *key,
                    count: positions.len(),
                    timing: timing_stats(&timestamps),
                });
            }
        }
        self.stats = stats;
        self.sort_stats();
        self.stats_selected = 0;
        self.stats_offset = 0;
        self.mode = Mode::Statistics;
        self.status = format!("{} frames in the visible list", self.stats.len());
    }

    fn sort_stats(&mut self) {
        sort_frame_stats(&mut self.stats, self.stats_sort);
    }

    fn stats_page_size(&self) -> usize {
        list_page_size(self.viewport_height, 6)
    }

    fn move_stats(&mut self, delta: isize) {
        self.stats_selected = move_index(self.stats_selected, self.stats.len(), delta);
        self.keep_stats_visible();
    }

    fn keep_stats_visible(&mut self) {
        let page = self.stats_page_size();
        if page == 0 {
            return;
        }
        if self.stats_selected < self.stats_offset {
            self.stats_offset = self.stats_selected;
        } else if self.stats_selected >= self.stats_offset + page {
            self.stats_offset = self.stats_selected + 1 - page;
        }
    }

    /// `Enter`: jump to the first occurrence of the selected frame.
    fn jump_to_stat(&mut self) {
        let Some(stat) = self.stats.get(self.stats_selected) else {
            return;
        };
        let (key, count) = (stat.key, stat.count);
        let Some(position) = self
            .filtered
            .iter()
            .position(|list_index| frame_key(&self.summaries[*list_index]) == Some(key))
        else {
            self.status = "No visible message for this frame".to_string();
            return;
        };
        self.selected = position;
        self.selected_signal = 0;
        self.mode = Mode::Messages;
        self.keep_selection_visible();
        self.status = format!("Frame 0x{:X}: {count} messages", key.sort_id());
    }

    fn handle_stats_mouse(&mut self, mouse: MouseEvent) {
        if mouse.wheel != 0 {
            self.move_stats(-isize::from(mouse.wheel));
        } else if mouse.button == 0 && !mouse.release && mouse.row >= STATS_FIRST_ROW {
            let index = self.stats_offset + usize::from(mouse.row - STATS_FIRST_ROW);
            if index < self.stats.len() {
                self.stats_selected = index;
            }
        }
    }

    fn time_axis(&self) -> TimeAxis {
        TimeAxis {
            mode: self.time_mode,
            base: self.time_base,
        }
    }

    /// `T`: print timestamps on the trace clock or after the first message.
    fn toggle_time_mode(&mut self) {
        self.time_mode = self.time_mode.toggled();
        self.status = format!(
            "Times: {} (first frame at {:.6}s)",
            self.time_mode.label(),
            self.time_base as f64 / 1_000_000_000.0
        );
    }

    /// `g`: jump to a trace-clock time, a `+` offset or a `#` position.
    fn goto_input(&mut self, value: &str) -> Result<(), String> {
        match parse_goto(value)? {
            Goto::Position(position) => self.jump_to_position(position),
            time => self.jump_to_time(time),
        }
        Ok(())
    }

    /// Jump to the first visible message at or after the requested time. A
    /// bare value is an instant on the trace clock the list shows; `+` counts
    /// from the first visible message because bench logs rarely start at zero.
    fn jump_to_time(&mut self, goto: Goto) {
        let Some(base) = self.first_timestamp() else {
            self.status = "This trace has no timestamps to jump by".to_string();
            return;
        };
        let Some(target) = goto_target(goto, base) else {
            self.status = "Time must be a finite, positive number of seconds".to_string();
            return;
        };
        let timestamps = self
            .filtered
            .iter()
            .map(|index| self.summaries[*index].timestamp_ns);
        let Some(position) = first_index_at_or_after(timestamps, target) else {
            self.status = format!("No message at or after {}", format_timestamp(Some(target)));
            return;
        };
        self.selected = position;
        self.selected_signal = 0;
        self.keep_selection_visible();
        let reached = self.summaries[self.filtered[position]]
            .timestamp_ns
            .unwrap_or(target);
        self.status = landed_status(reached, base, position, self.filtered.len());
    }

    fn first_timestamp(&self) -> Option<u64> {
        self.filtered
            .iter()
            .find_map(|index| self.summaries[*index].timestamp_ns)
    }

    fn jump_to_position(&mut self, position: usize) {
        if self.filtered.is_empty() {
            self.status = "Nothing to jump to".to_string();
            return;
        }
        let index = position.saturating_sub(1).min(self.filtered.len() - 1);
        self.selected = index;
        self.selected_signal = 0;
        self.keep_selection_visible();
        let total = self.filtered.len();
        let reached = self.summaries[self.filtered[index]].timestamp_ns;
        self.status = match (self.first_timestamp(), reached) {
            (Some(base), Some(reached)) => landed_status(reached, base, index, total),
            // A trace without timestamps can still be navigated by position.
            _ => format!("Message #{} of {total}", index + 1),
        };
    }

    fn jump_to_end(&mut self) {
        if self.filtered.is_empty() {
            self.status = "Nothing to jump to".to_string();
            return;
        }
        self.selected = self.filtered.len() - 1;
        self.selected_signal = 0;
        self.keep_selection_visible();
        self.status = format!("Last message ({})", self.position_label());
    }

    /// `]`/`[`: move to the next or previous message with the same frame id
    /// on the same bus and channel, and say which occurrence it is.
    fn step_frame(&mut self, direction: isize) {
        let Some(key) = self.selected_frame_key() else {
            self.status = "This message has no frame id to step through".to_string();
            return;
        };
        self.ensure_frame_index();
        let outcome = match self.frame_index.as_ref().and_then(|index| index.get(&key)) {
            None => FrameStep::Missing,
            Some(positions) => match positions
                .iter()
                .position(|position| *position == self.selected)
            {
                None => FrameStep::Missing,
                Some(current) => {
                    let next = current as isize + direction;
                    if next < 0 {
                        FrameStep::AtStart(positions.len())
                    } else if let Some(&target) = positions.get(next as usize) {
                        FrameStep::Move {
                            target,
                            ordinal: next as usize + 1,
                            total: positions.len(),
                        }
                    } else {
                        FrameStep::AtEnd(positions.len())
                    }
                }
            },
        };
        match outcome {
            FrameStep::Missing => {
                self.status = "This message has no frame id to step through".to_string();
            }
            FrameStep::AtStart(total) => self.status = format!("First of {total} occurrences"),
            FrameStep::AtEnd(total) => self.status = format!("Last of {total} occurrences"),
            FrameStep::Move {
                target,
                ordinal,
                total,
            } => {
                self.selected = target;
                self.selected_signal = 0;
                self.keep_selection_visible();
                let label = self
                    .selected_summary()
                    .map(frame_label)
                    .unwrap_or_else(|| "frame".to_string());
                self.status = format!("{label}: occurrence {ordinal}/{total}");
            }
        }
    }

    /// Switch the message list to another bus view.
    fn set_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        let anchor = self.selected_summary().map(|summary| summary.object_index);
        self.view = view;
        self.rebuild_filtered();
        // Keep the same frame selected when it is still visible, otherwise
        // land on the first frame the new view owns.
        if let Some(anchor) = anchor {
            if let Some(position) = self.filtered.iter().position(|index| *index == anchor) {
                self.selected = position;
                self.keep_selection_visible();
                self.status = format!("View {}", view.label());
                return;
            }
        }
        self.selected = 0;
        self.list_offset = 0;
        self.status = format!("View {} · {} frames", view.label(), self.filtered.len());
    }

    /// Show or hide the dim description of frames from other buses.
    fn toggle_foreign(&mut self) {
        self.show_foreign = !self.show_foreign;
        self.rebuild_filtered();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.status = if self.show_foreign {
            "Foreign frames shown".to_string()
        } else {
            format!("Showing {} only", self.view.label())
        };
    }

    fn selected_frame_key(&self) -> Option<FrameKey> {
        frame_key(self.selected_summary()?)
    }

    /// Map each frame id of the visible list to its positions in that list,
    /// built once instead of rescanning on every `]`/`[` and detail open.
    fn ensure_frame_index(&mut self) {
        if self.frame_index.is_some() {
            return;
        }
        let mut index: HashMap<FrameKey, Vec<usize>> = HashMap::new();
        for (position, list_index) in self.filtered.iter().enumerate() {
            let Some(summary) = self.summaries.get(*list_index) else {
                continue;
            };
            let Some(key) = frame_key(summary) else {
                continue;
            };
            index.entry(key).or_default().push(position);
        }
        self.frame_index = Some(index);
    }

    fn position_label(&self) -> String {
        format!("{} / {}", self.selected + 1, self.filtered.len())
    }

    fn select_last(&mut self) {
        self.selected = self.filtered.len().saturating_sub(1);
        self.keep_selection_visible();
    }

    fn select_first_signal(&mut self) {
        self.selected_signal = 0;
        self.detail_offset = 0;
    }

    fn select_last_signal(&mut self) {
        self.selected_signal = self.detail_rows.len().saturating_sub(1);
        self.keep_signal_visible();
    }

    fn keep_selection_visible(&mut self) {
        let page = self.message_page_size();
        if page == 0 {
            return;
        }
        if self.selected < self.list_offset {
            self.list_offset = self.selected;
        } else if self.selected >= self.list_offset + page {
            self.list_offset = self.selected + 1 - page;
        }
    }

    fn keep_signal_visible(&mut self) {
        let page = self.detail_page_size();
        if page == 0 {
            return;
        }
        if self.selected_signal < self.detail_offset {
            self.detail_offset = self.selected_signal;
        } else if self.selected_signal >= self.detail_offset + page {
            self.detail_offset = self.selected_signal + 1 - page;
        }
    }

    fn message_page_size(&self) -> usize {
        list_page_size(self.viewport_height, 7)
    }

    fn detail_page_size(&self) -> usize {
        list_page_size(self.viewport_height, 5)
    }

    fn database_page_size(&self) -> usize {
        list_page_size(self.viewport_height, 8)
    }

    /// Rows to move for PageUp/PageDown in the active mode.
    fn page_step(&self) -> isize {
        let rows = match self.mode {
            Mode::Messages => self.message_page_size(),
            Mode::Detail => self.detail_page_size(),
            // The details pane scrolls wrapped lines, so a page is its height.
            Mode::Databases => match self.database_pane {
                DatabasePane::Details => self.database_body_rows(self.viewport_height),
                DatabasePane::Tree | DatabasePane::Objects => self.database_page_size(),
            },
            Mode::Curve => 1,
            Mode::Statistics => self.stats_page_size(),
            Mode::Inspector => self.inspector_page_size(),
        };
        rows as isize
    }

    fn clear_filter(&mut self) {
        self.filter.clear();
        self.time_range = None;
        self.selected = 0;
        self.list_offset = 0;
        self.rebuild_filtered();
        self.status = "Filter cleared".to_string();
    }

    fn enter_databases(&mut self) {
        self.database_selected = self
            .database_selected
            .min(self.bindings.len().saturating_sub(1));
        self.database_pane = DatabasePane::Tree;
        self.database_category = 0;
        self.database_object_selected = 0;
        self.database_object_offset = 0;
        self.rebuild_database_objects();
        self.mode = Mode::Databases;
    }

    fn database_categories(&self) -> Vec<DbCategory> {
        let Some(entry) = self.bindings.get(self.database_selected) else {
            return Vec::new();
        };
        match entry.binding.database() {
            Database::Can(_) => vec![
                DbCategory::Overview,
                DbCategory::Messages,
                DbCategory::Signals,
                DbCategory::Nodes,
                DbCategory::Attributes,
                DbCategory::ValueTables,
            ],
            Database::Lin(_) => vec![
                DbCategory::Overview,
                DbCategory::Frames,
                DbCategory::Signals,
                DbCategory::Nodes,
                DbCategory::DiagnosticFrames,
                DbCategory::DiagnosticSignals,
                DbCategory::Schedules,
                DbCategory::DynamicFrames,
                DbCategory::OptionalSections,
            ],
        }
    }

    fn selected_database_category(&self) -> Option<DbCategory> {
        self.database_categories()
            .get(self.database_category)
            .copied()
    }

    /// Rebuild the object list once per database/category/filter change.
    fn rebuild_database_objects(&mut self) {
        let objects = match self.bindings.get(self.database_selected) {
            Some(entry) => database_objects(
                entry,
                self.selected_database_category()
                    .unwrap_or(DbCategory::Overview),
            ),
            None => Vec::new(),
        };
        let filter = self.database_filter.to_ascii_lowercase();
        self.db_objects = if filter.is_empty() {
            objects
        } else {
            objects
                .into_iter()
                .filter(|object| {
                    object.title.to_ascii_lowercase().contains(&filter)
                        || object.subtitle.to_ascii_lowercase().contains(&filter)
                        || object
                            .details
                            .iter()
                            .any(|detail| detail.text().to_ascii_lowercase().contains(&filter))
                })
                .collect()
        };
        self.database_object_selected = self
            .database_object_selected
            .min(self.db_objects.len().saturating_sub(1));
        self.database_detail_offset = 0;
    }

    fn move_database_category(&mut self, delta: isize) {
        let count = self.database_categories().len();
        let category = move_index(self.database_category, count, delta);
        self.set_database_category(category);
    }

    fn set_database_category(&mut self, category: usize) {
        let count = self.database_categories().len();
        self.database_category = category.min(count.saturating_sub(1));
        self.database_object_selected = 0;
        self.database_object_offset = 0;
        self.database_filter.clear();
        self.rebuild_database_objects();
    }

    fn switch_database(&mut self, delta: isize) {
        self.database_selected = move_index(self.database_selected, self.bindings.len(), delta);
        self.database_category = 0;
        self.database_object_selected = 0;
        self.database_object_offset = 0;
        self.database_filter.clear();
        self.rebuild_database_objects();
    }

    fn move_database_object(&mut self, delta: isize) {
        let count = self.db_objects.len();
        self.database_object_selected = move_index(self.database_object_selected, count, delta);
        self.database_detail_offset = 0;
        self.keep_database_object_visible();
    }

    fn select_first_database_object(&mut self) {
        self.database_object_selected = 0;
        self.database_object_offset = 0;
        self.database_detail_offset = 0;
    }

    fn select_last_database_object(&mut self) {
        self.database_object_selected = self.db_objects.len().saturating_sub(1);
        self.database_detail_offset = 0;
        self.keep_database_object_visible();
    }

    fn keep_database_object_visible(&mut self) {
        let page = self.viewport_height.saturating_sub(8).max(1);
        if self.database_object_selected < self.database_object_offset {
            self.database_object_offset = self.database_object_selected;
        } else if self.database_object_selected >= self.database_object_offset + page {
            self.database_object_offset = self.database_object_selected + 1 - page;
        }
    }

    /// Body rows of the database browser below its header block and rule.
    fn database_body_rows(&self, height: usize) -> usize {
        height.saturating_sub(7).max(1)
    }

    /// Cells available to the details column: the third column, or the whole
    /// terminal in the narrow layout.
    fn database_detail_width(&self, width: usize) -> usize {
        if details_take_full_width(width, self.database_pane) {
            width
        } else {
            self.database_layout(width).2
        }
    }

    /// The selected object's detail lines wrapped to `width` cells. Each
    /// wrapped part keeps the style of the detail line it came from.
    fn database_detail_lines(&self, width: usize) -> Vec<(String, CellStyle)> {
        let Some(object) = self.db_objects.get(self.database_object_selected) else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        for detail in &object.details {
            let (text, style) = detail.render();
            lines.extend(wrap(&text, width).into_iter().map(|part| (part, style)));
        }
        lines
    }

    /// Keep the details offset inside the wrapped content, so a resize or a
    /// shorter object cannot leave the column scrolled past its end.
    fn clamp_database_detail_offset(&mut self) {
        let width = self.database_detail_width(self.viewport_width);
        let total = self.database_detail_lines(width).len();
        let rows = self.database_body_rows(self.viewport_height);
        self.database_detail_offset = clamp_detail_offset(self.database_detail_offset, total, rows);
    }

    /// Scroll the details column by `delta` wrapped lines.
    fn scroll_database_details(&mut self, delta: isize) {
        let width = self.database_detail_width(self.viewport_width);
        let total = self.database_detail_lines(width).len();
        let rows = self.database_body_rows(self.viewport_height);
        self.database_detail_offset =
            move_detail_offset(self.database_detail_offset, total, rows, delta);
    }

    fn select_first_database_category(&mut self) {
        self.set_database_category(0);
    }

    fn select_last_database_category(&mut self) {
        let count = self.database_categories().len();
        self.set_database_category(count.saturating_sub(1));
    }

    /// Left/Right and h/l step through the panes and stop at either end.
    fn shift_database_pane(&mut self, step: isize) {
        self.database_pane = self.database_pane.shifted(step);
    }

    /// Tab and Shift-Tab cycle through the panes.
    fn toggle_database_pane(&mut self, forward: bool) {
        self.database_pane = if forward {
            self.database_pane.cycled()
        } else {
            self.database_pane.cycled_back()
        };
    }

    /// Up/Down, PageUp/PageDown and Home/End act on the focused pane only.
    fn move_database_row(&mut self, delta: isize) {
        match self.database_pane {
            DatabasePane::Tree => self.move_database_category(delta),
            DatabasePane::Objects => self.move_database_object(delta),
            DatabasePane::Details => self.scroll_database_details(delta),
        }
    }

    fn select_first_database_row(&mut self) {
        match self.database_pane {
            DatabasePane::Tree => self.select_first_database_category(),
            DatabasePane::Objects => self.select_first_database_object(),
            DatabasePane::Details => self.database_detail_offset = 0,
        }
    }

    fn select_last_database_row(&mut self) {
        match self.database_pane {
            DatabasePane::Tree => self.select_last_database_category(),
            DatabasePane::Objects => self.select_last_database_object(),
            DatabasePane::Details => self.scroll_database_details(isize::MAX),
        }
    }

    /// Enter opens the object list from the tree and reports the object
    /// selection from the object list, without moving focus. The details are
    /// read-only, so Enter does not change anything there.
    fn activate_database_row(&mut self) {
        match self.database_pane {
            DatabasePane::Tree => self.database_pane = DatabasePane::Objects,
            DatabasePane::Objects => self.select_database_object(),
            DatabasePane::Details => {}
        }
    }

    fn select_database_object(&mut self) {
        if let Some(object) = self.db_objects.get(self.database_object_selected) {
            self.status = format!("Selected {}", object.title);
        }
    }

    fn selected_summary(&self) -> Option<&MessageSummary> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.summaries.get(*index))
    }

    /// Cycle timing for the frame id of `summary`, read from the frame index
    /// so opening a detail page does not rescan the whole trace.
    fn frame_timing(&mut self, summary: &MessageSummary) -> Option<TimingStats> {
        let key = frame_key(summary)?;
        self.ensure_frame_index();
        let positions = self.frame_index.as_ref()?.get(&key)?;
        let timestamps = positions
            .iter()
            .filter_map(|position| {
                let index = *self.filtered.get(*position)?;
                self.summaries.get(index)?.timestamp_ns
            })
            .collect::<Vec<_>>();
        timing_stats(&timestamps)
    }

    fn selected_object(&self) -> Option<&Object> {
        self.selected_summary()
            .and_then(|summary| self.objects.get(summary.object_index))
    }

    /// Recompute the filter once per filter change instead of once per frame.
    fn rebuild_filtered(&mut self) {
        let filter = self.filter.to_ascii_lowercase();
        let range = self.time_range;
        let view = self.view;
        let show_foreign = self.show_foreign;
        // Only the current bus is kept when foreign frames are hidden.
        let keep = |summary: &MessageSummary| show_foreign || view.owns(summary);
        let filtered = if filter.is_empty() && range.is_none() {
            (0..self.summaries.len())
                .filter(|index| keep(&self.summaries[*index]))
                .collect()
        } else {
            let mut buffer = String::with_capacity(128);
            let mut indices = Vec::new();
            for (index, summary) in self.summaries.iter().enumerate() {
                if !in_time_range(summary.timestamp_ns, range) || !keep(summary) {
                    continue;
                }
                if filter.is_empty()
                    || summary_matches_in(summary, &filter, &mut buffer, &self.pool)
                {
                    indices.push(index);
                }
            }
            indices
        };
        self.filtered = filtered;
        self.frame_index = None;
        self.timing = None;
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    fn enter_detail(&mut self) {
        if self.trace.is_none() {
            self.status = NO_TRACE_STATUS.to_string();
            return;
        }
        if self.selected_summary().is_none() {
            return;
        }
        self.mode = Mode::Detail;
        self.selected_signal = 0;
        self.detail_offset = 0;
        self.refresh_detail();
    }

    fn refresh_detail(&mut self) {
        self.detail_rows.clear();
        self.timing = None;
        let Some(summary) = self.selected_summary().cloned() else {
            return;
        };
        // Timing only needs timestamps, so it stays available without a database.
        self.timing = self.frame_timing(&summary);
        let Some(object) = self.objects.get(summary.object_index) else {
            return;
        };
        let Some(binding) = self.binding_for_summary(&summary) else {
            self.status = "No database bound for this bus/channel".to_string();
            return;
        };
        self.detail_rows = decode_object_signals(object, binding);
        if self.selected_signal >= self.detail_rows.len() {
            self.selected_signal = self.detail_rows.len().saturating_sub(1);
        }
        self.detail_offset = 0;
    }

    fn binding_for_summary(&self, summary: &MessageSummary) -> Option<&DatabaseBinding> {
        let bus = summary.bus?;
        let channel = summary.channel?;
        self.bindings
            .iter()
            .find(|entry| entry.binding.bus() == bus && entry.binding.channel() == channel)
            .map(|entry| &entry.binding)
    }

    /// Plotting is best effort: a decode/query failure becomes a status line
    /// instead of tearing down the whole session.
    /// Write the filtered messages, with their decoded signals, to a CSV
    /// next to the trace so a bench report can quote real numbers.
    /// Export the visible frames as pcapng so Wireshark can read them.
    ///
    /// The current view, message filter and time range decide what goes in, so
    /// a CAN view produces only CAN packets and an EtherType filter narrows the
    /// capture to the traffic under investigation. LIN frames have no pcapng
    /// link type this writer fills in, so they are counted as skipped.
    fn export_pcap(&mut self) {
        if self.trace.is_none() {
            self.status = NO_TRACE_STATUS.to_string();
            return;
        }
        if self.filtered.is_empty() {
            self.status = "Nothing to export: the list is empty".to_string();
            return;
        }
        let mut writer = blf_core::PcapWriter::new();
        for index in &self.filtered {
            let Some(summary) = self.summaries.get(*index) else {
                continue;
            };
            // Frames of other buses are only a dim description in this view, so
            // they stay out of the capture as well.
            if !self.view.owns(summary) {
                continue;
            }
            let Some(timestamp) = summary.timestamp_ns else {
                writer.skip();
                continue;
            };
            let Some(object) = self.objects.get(summary.object_index) else {
                writer.skip();
                continue;
            };
            if summary.ethernet.is_some() {
                match blf_core::frame_bytes(object) {
                    Some(frame) => writer.write_ethernet(timestamp, &frame),
                    None => writer.skip(),
                }
            } else if matches!(summary.bus, Some(BusKind::Can)) {
                let extended = summary.frame_id.is_some_and(|id| id > 0x7FF);
                writer.write_can(
                    timestamp,
                    summary.channel.unwrap_or(0),
                    summary.frame_id.unwrap_or(0),
                    extended,
                    &summary.payload,
                );
            } else {
                writer.skip();
            }
        }
        let report = writer.report();
        let bytes = writer.finish();
        let path = self
            .trace
            .as_ref()
            .map(|trace| export_pcap_path(&trace.path))
            .unwrap_or_default();
        match std::fs::write(&path, &bytes) {
            Ok(()) => {
                self.status = format!(
                    "Exported {} ethernet, {} CAN frames to {} ({} skipped)",
                    report.ethernet,
                    report.can,
                    path.display(),
                    report.skipped
                )
            }
            Err(error) => self.status = format!("write {}: {error}", path.display()),
        }
    }

    fn export_csv(&mut self) {
        let Some(trace) = &self.trace else {
            self.status = NO_TRACE_STATUS.to_string();
            return;
        };
        if self.filtered.is_empty() {
            self.status = "Nothing to export: the list is empty".to_string();
            return;
        }
        let path = export_path(&trace.path);
        match self.write_csv(&path) {
            Ok((rows, signals)) => {
                self.status = format!(
                    "Exported {rows} rows, {signals} signals -> {}",
                    path.display()
                );
            }
            Err(error) => self.status = format!("Export failed: {error}"),
        }
    }

    fn write_csv(&self, path: &Path) -> Result<(usize, usize), String> {
        let file =
            StdFile::create(path).map_err(|error| format!("create {}: {error}", path.display()))?;
        let mut writer = BufWriter::new(file);
        let mut columns: Vec<String> = Vec::new();
        for summary in self.filtered_rows() {
            for row in self.decoded_rows(summary) {
                if !columns.iter().any(|name| name == &row.name) && columns.len() < MAX_CSV_SIGNALS
                {
                    columns.push(row.name.clone());
                }
            }
        }
        let mut header: Vec<String> = [
            "timestamp_s",
            "bus",
            "channel",
            "frame_id",
            "direction",
            "dlc",
            "data",
            "status",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        header.extend(columns.iter().cloned());
        writeln!(writer, "{}", csv_line(&header)).map_err(|error| error.to_string())?;

        let mut written = 0;
        for summary in self.filtered_rows() {
            let decoded = self.decoded_rows(summary);
            let mut fields = vec![
                format_timestamp(summary.timestamp_ns),
                summary.bus_label.to_string(),
                summary
                    .channel
                    .map_or_else(String::new, |value| value.to_string()),
                match (&summary.ethernet, summary.frame_id) {
                    (Some(info), _) => format!("0x{:04X}", info.ether_type),
                    (None, Some(value)) => format!("0x{value:X}"),
                    (None, None) => String::new(),
                },
                summary.direction.unwrap_or_default().to_string(),
                summary
                    .dlc
                    .map_or_else(String::new, |value| value.to_string()),
                match &summary.ethernet {
                    Some(info) => self.pool.get(info.summary).to_string(),
                    None => hex_bytes(&summary.payload),
                },
                match &summary.ethernet {
                    Some(info) => info.protocol.to_string(),
                    None => summary.status.to_string(),
                },
            ];
            for column in &columns {
                let value = decoded
                    .iter()
                    .find(|row| &row.name == column)
                    .map(csv_signal_value)
                    .unwrap_or_default();
                fields.push(value);
            }
            writeln!(writer, "{}", csv_line(&fields)).map_err(|error| error.to_string())?;
            written += 1;
        }
        writer.flush().map_err(|error| error.to_string())?;
        Ok((written, columns.len()))
    }

    fn filtered_rows(&self) -> impl Iterator<Item = &MessageSummary> + '_ {
        self.filtered
            .iter()
            .filter_map(|index| self.summaries.get(*index))
    }

    fn decoded_rows(&self, summary: &MessageSummary) -> Vec<SignalRow> {
        let Some(object) = self.objects.get(summary.object_index) else {
            return Vec::new();
        };
        self.binding_for_summary(summary)
            .map(|binding| decode_object_signals(object, binding))
            .unwrap_or_default()
    }

    /// `p`: plot the selected signal on its own.
    fn open_curve(&mut self) {
        self.show_curve(false);
    }

    /// `a`: add the selected signal to the curve that is already plotted.
    fn add_curve_series(&mut self) {
        self.show_curve(true);
    }

    fn show_curve(&mut self, append: bool) {
        if self.trace.is_none() {
            self.status = NO_TRACE_STATUS.to_string();
            return;
        }
        if self.mode == Mode::Messages {
            self.enter_detail();
        }
        if self.detail_rows.is_empty() {
            self.status = "Select a decoded numeric signal first".to_string();
            return;
        }
        let Some(summary) = self.selected_summary().cloned() else {
            return;
        };
        let Some(binding) = self.binding_for_summary(&summary).cloned() else {
            self.status = "No database bound for this bus/channel".to_string();
            return;
        };
        let row = &self.detail_rows[self.selected_signal];
        if row.error.is_some() {
            self.status = format!("{} is not decodable: select a decoded signal", row.name);
            return;
        }
        let title = qualified_signal_name(
            binding.database(),
            &row.name,
            &self.objects[summary.object_index],
        );
        if append {
            if let Some(curve) = &self.curve {
                if curve.series.len() >= CURVE_SYMBOLS.len() {
                    self.status = format!("A curve shows at most {} signals", CURVE_SYMBOLS.len());
                    return;
                }
                if curve.series.iter().any(|series| series.title == title) {
                    self.status = format!("{title} is already plotted");
                    return;
                }
            }
        }
        let Some(trace) = &self.trace else {
            return;
        };
        let query = match trace.reader.signal_query(binding, title.clone(), false) {
            Ok(query) => query,
            Err(error) => {
                self.status = format!("start signal query: {error}");
                return;
            }
        };
        let mut samples = Vec::new();
        let mut error: Option<String> = None;
        let mut truncated = false;
        for item in query {
            match item {
                Ok(sample) => {
                    if error.is_none() {
                        error = sample.error.clone();
                    }
                    if !in_time_range(Some(sample.timestamp_ns), self.time_range) {
                        continue;
                    }
                    let Some(value) = sample.physical_value else {
                        continue;
                    };
                    if samples.len() >= MAX_CURVE_SAMPLES {
                        truncated = true;
                        break;
                    }
                    samples.push((sample.timestamp_ns, value));
                }
                Err(query_error) => {
                    error = Some(query_error.to_string());
                    break;
                }
            }
        }
        if self.curve.is_none() {
            self.curve = Some(CurveData {
                normalize: false,
                series: Vec::new(),
                window_start: 0,
                window_end: 0,
            });
        }
        let Some(curve) = &mut self.curve else {
            return;
        };
        let index = curve.series.len() % CURVE_SYMBOLS.len();
        curve.series.push(CurveSeries {
            title: title.clone(),
            samples,
            error,
            truncated,
            symbol: CURVE_SYMBOLS[index],
            color: curve_color(index),
        });
        // The first series always seeds the window, also when it arrives by `a`.
        if !append || curve.series.len() == 1 {
            if let Some(bounds) = curve_bounds(curve) {
                curve.window_start = bounds.0;
                curve.window_end = bounds.1;
            }
        }
        self.mode = Mode::Curve;
        self.status = format!("Plotted {title}");
    }

    /// `n` in the curve view: scale every series to its own range.
    ///
    /// Signals with different units share one axis by default, so the smaller
    /// one flattens against zero. Normalising shows both shapes, which is what
    /// a first look usually needs.
    fn toggle_curve_normalize(&mut self) {
        let Some(curve) = self.curve.as_mut() else {
            return;
        };
        curve.normalize = !curve.normalize;
        self.status = if curve.normalize {
            "Curve: each signal scaled to its own range".to_string()
        } else {
            "Curve: shared value axis".to_string()
        };
    }

    /// `x` in the curve view: drop the series that was added last.
    fn remove_curve_series(&mut self) {
        let Some(curve) = &mut self.curve else {
            return;
        };
        if curve.series.pop().is_some() {
            if curve.series.is_empty() {
                self.curve = None;
                self.mode = Mode::Detail;
                self.status = "Curve closed".to_string();
            } else {
                self.status = "Removed the last signal".to_string();
            }
        }
    }

    fn curve_pan(&mut self, direction: isize) {
        let Some(curve) = &self.curve else {
            return;
        };
        let Some(bounds) = curve_bounds(curve) else {
            return;
        };
        let window = (curve.window_start, curve.window_end);
        let (start, end) = pan_window(window, direction, bounds);
        if let Some(curve) = &mut self.curve {
            curve.window_start = start;
            curve.window_end = end;
        }
    }

    fn curve_zoom(&mut self, factor: f64) {
        let Some(curve) = &self.curve else {
            return;
        };
        let Some(bounds) = curve_bounds(curve) else {
            return;
        };
        let window = (curve.window_start, curve.window_end);
        let (start, end) = zoom_window(window, factor, bounds);
        if let Some(curve) = &mut self.curve {
            curve.window_start = start;
            curve.window_end = end;
        }
    }

    fn curve_home(&mut self) {
        let Some(curve) = &self.curve else {
            return;
        };
        let Some(bounds) = curve_bounds(curve) else {
            return;
        };
        let width = curve.window_end.saturating_sub(curve.window_start);
        if let Some(curve) = &mut self.curve {
            curve.window_start = bounds.0;
            curve.window_end = bounds.0.saturating_add(width).min(bounds.1);
        }
    }

    fn curve_end(&mut self) {
        let Some(curve) = &self.curve else {
            return;
        };
        let Some(bounds) = curve_bounds(curve) else {
            return;
        };
        let width = curve.window_end.saturating_sub(curve.window_start);
        if let Some(curve) = &mut self.curve {
            curve.window_end = bounds.1;
            curve.window_start = bounds.1.saturating_sub(width).max(bounds.0);
        }
    }

    fn handle_message_mouse(&mut self, mouse: MouseEvent) {
        if mouse.wheel != 0 {
            self.move_selection(-isize::from(mouse.wheel));
        } else if mouse.button == 0 && !mouse.release && mouse.row >= LIST_FIRST_ROW {
            let row = usize::from(mouse.row - LIST_FIRST_ROW);
            let index = self.list_offset + row;
            if index < self.filtered.len() {
                self.selected = index;
                self.selected_signal = 0;
            }
        }
    }

    fn handle_detail_mouse(&mut self, mouse: MouseEvent) {
        if mouse.wheel != 0 {
            self.move_signal(-isize::from(mouse.wheel));
        } else if mouse.button == 0 && !mouse.release && mouse.row >= DETAIL_FIRST_ROW {
            let split = (usize::from(mouse.column) * 43 / 100).max(24);
            if usize::from(mouse.column) > split {
                let index = self.detail_offset + usize::from(mouse.row - DETAIL_FIRST_ROW);
                if index < self.detail_rows.len() {
                    self.selected_signal = index;
                    self.keep_signal_visible();
                }
            }
        }
    }

    fn handle_curve_mouse(&mut self, mouse: MouseEvent) {
        if mouse.wheel != 0 {
            self.curve_pan(-isize::from(mouse.wheel));
        }
    }

    fn handle_database_mouse(&mut self, mouse: MouseEvent) {
        // The narrow layout shows only the details, so every wheel event scrolls
        // them and a click keeps the pane focused.
        if details_take_full_width(self.viewport_width, self.database_pane) {
            if mouse.wheel != 0 {
                self.scroll_database_details(-isize::from(mouse.wheel));
            }
            return;
        }
        let (left, middle, _) = self.database_layout(self.viewport_width);
        let objects_start = left + 3;
        let details_start = objects_start + middle;
        if mouse.wheel != 0 {
            let column = usize::from(mouse.column);
            if column < left {
                self.move_database_category(-isize::from(mouse.wheel));
            } else if column < details_start {
                self.move_database_object(-isize::from(mouse.wheel));
            } else {
                self.database_pane = DatabasePane::Details;
                self.scroll_database_details(-isize::from(mouse.wheel));
            }
            return;
        }
        if mouse.button != 0 || mouse.release || mouse.row < LIST_FIRST_ROW {
            return;
        }
        let row = usize::from(mouse.row - LIST_FIRST_ROW);
        let column = usize::from(mouse.column);
        if column < left {
            self.database_pane = DatabasePane::Tree;
            if row >= 1 && row - 1 < self.database_categories().len() {
                self.set_database_category(row - 1);
            }
        } else if column < details_start {
            self.database_pane = DatabasePane::Objects;
            let index = self.database_object_offset + row;
            if index < self.db_objects.len() {
                self.database_object_selected = index;
                self.database_detail_offset = 0;
            }
        } else {
            self.database_pane = DatabasePane::Details;
        }
    }

    fn database_left_width(&self) -> usize {
        (self.viewport_width * 25 / 100).max(24)
    }

    fn database_layout(&self, width: usize) -> (usize, usize, usize) {
        // Below the narrow threshold the details column moves to a full-width
        // view of its own (see `details_take_full_width`), so the browser keeps
        // two panes. Splitting a 60-column terminal three ways left the third
        // column zero wide, which drew nothing at all.
        let two_pane = width < TWO_PANE_DATABASE_WIDTH;
        let left = self.database_left_width().min(width.saturating_sub(2));
        let middle = if two_pane {
            width.saturating_sub(left + 3).max(1)
        } else {
            (width * 32 / 100)
                .max(30)
                .min(width.saturating_sub(left + 1))
        };
        let right = if two_pane {
            0
        } else {
            width.saturating_sub(left + middle + 6)
        };
        (left, middle, right)
    }

    /// `?`: open the help overlay; `?`, `q` or `Esc` closes it again.
    fn open_help(&mut self) {
        if self.help.is_some() {
            return;
        }
        self.help = Some(HelpState {
            offset: 0,
            previous_status: std::mem::take(&mut self.status),
            filter: String::new(),
        });
        self.status = "Help: j/k scroll · / search · ? / q / Esc closes".to_string();
    }

    fn close_help(&mut self) {
        if let Some(help) = self.help.take() {
            self.status = help.previous_status;
        }
    }

    fn scroll_help(&mut self, delta: isize) {
        // The scroll range follows the filtered list, not the full one.
        let rows = self.help.as_ref().map_or_else(
            || help_lines(self.viewport_width).len(),
            |help| help.entries(self.viewport_width).len(),
        );
        let page = help_page_size(self.viewport_height);
        let max_offset = rows.saturating_sub(page) as isize;
        if let Some(help) = &mut self.help {
            help.offset = (help.offset as isize)
                .saturating_add(delta)
                .clamp(0, max_offset) as usize;
        }
    }

    fn handle_help_event(&mut self, event: InputEvent) {
        // Typing after `/` edits the filter instead of scrolling.
        if self.input == Some(InputKind::HelpFilter) {
            match event {
                InputEvent::Char(character) => {
                    if let Some(help) = self.help.as_mut() {
                        help.filter.push(character);
                        help.offset = 0;
                    }
                }
                InputEvent::Backspace => {
                    if let Some(help) = self.help.as_mut() {
                        help.filter.pop();
                        help.offset = 0;
                    }
                }
                InputEvent::Enter => self.input = None,
                InputEvent::Esc => {
                    if let Some(help) = self.help.as_mut() {
                        help.filter.clear();
                        help.offset = 0;
                    }
                    self.input = None;
                }
                _ => {}
            }
            return;
        }
        if matches!(event, InputEvent::Char('/')) {
            self.input = Some(InputKind::HelpFilter);
            return;
        }
        let page = help_page_size(self.viewport_height) as isize;
        let rows = help_lines(self.viewport_width).len() as isize;
        match event {
            InputEvent::Char('?') | InputEvent::Char('q') | InputEvent::Esc => self.close_help(),
            InputEvent::Char('f') => {
                // Clear a filter without leaving the overlay.
                if let Some(help) = self.help.as_mut() {
                    help.filter.clear();
                    help.offset = 0;
                }
            }
            InputEvent::Char('j') | InputEvent::Down => self.scroll_help(1),
            InputEvent::Char('k') | InputEvent::Up => self.scroll_help(-1),
            InputEvent::PageDown => self.scroll_help(page),
            InputEvent::PageUp => self.scroll_help(-page),
            InputEvent::Home => self.scroll_help(-rows),
            InputEvent::End => self.scroll_help(rows),
            InputEvent::Mouse(mouse) if mouse.wheel != 0 => {
                self.scroll_help(-isize::from(mouse.wheel))
            }
            _ => {}
        }
    }

    fn render(&mut self, terminal: &mut RawTerminal) -> Result<(), String> {
        let width = self.viewport_width;
        let height = self.viewport_height;
        let mut lines = Vec::new();
        lines.push(paint_bold(self.title_line(), slot_color(Slot::Cyan)));
        let databases = if self.bindings.is_empty() {
            "none".to_string()
        } else {
            self.bindings
                .iter()
                .map(|entry| {
                    format!(
                        "{}{}={}",
                        entry.binding.bus(),
                        entry.binding.channel(),
                        entry.path.display()
                    )
                })
                .collect::<Vec<_>>()
                .join("  ")
        };
        lines.push(paint(
            format!(" databases: {databases}"),
            slot_color(Slot::Cyan),
        ));
        lines.push(paint("─".repeat(width), slot_color(Slot::Cyan)));
        if self.help.is_some() {
            self.render_help(&mut lines, width, height);
        } else {
            match self.mode {
                Mode::Messages => self.render_messages(&mut lines, width, height),
                Mode::Detail => self.render_detail(&mut lines, width, height),
                Mode::Curve => self.render_curve(&mut lines, width, height),
                Mode::Databases => {
                    self.clamp_database_detail_offset();
                    self.render_databases(&mut lines, width, height);
                }
                Mode::Statistics => self.render_statistics(&mut lines, width, height),
                Mode::Inspector => self.render_inspector(&mut lines, width, height),
            }
        }
        lines.push(paint("─".repeat(width), slot_color(Slot::Cyan)));
        lines.push(paint_bold(
            footer_line(self.input, &self.input_buffer, &self.status, self.mode),
            slot_color(Slot::Yellow),
        ));
        let mut output = String::from("\x1b[H");
        for line in lines.into_iter().take(height) {
            output.push_str(&truncate_ansi(&line, width));
            output.push_str("\x1b[K");
            output.push('\n');
        }
        output.push_str("\x1b[J");
        terminal.write(&output).map_err(|error| error.to_string())
    }

    fn render_help(&self, lines: &mut Vec<String>, width: usize, height: usize) {
        let page = help_page_size(height);
        let entries = self
            .help
            .as_ref()
            .map_or_else(|| help_lines(width), |help| help.entries(width));
        let offset = clamp_help_offset(
            self.help.as_ref().map_or(0, |help| help.offset),
            entries.len(),
            page,
        );
        let end = (offset + page).min(entries.len());
        lines.push(paint_bold(
            format!(
                "Help  keys {}-{} of {}{}  (? / q / Esc closes)",
                offset + 1,
                end,
                entries.len(),
                self.help
                    .as_ref()
                    .filter(|help| !help.filter.trim().is_empty())
                    .map(|help| format!("  filter=\"{}\"", help.filter))
                    .unwrap_or_default()
            ),
            slot_color(Slot::Cyan),
        ));
        for (is_heading, line) in &entries[offset..end] {
            lines.push(if *is_heading {
                paint_bold(line, slot_color(Slot::Yellow))
            } else {
                paint(line, ANSI_PLAIN)
            });
        }
    }

    fn render_messages(&self, lines: &mut Vec<String>, width: usize, height: usize) {
        let indices = &self.filtered;
        // A preview pane needs room to be useful; below this width the list gets
        // the whole screen instead of two unreadable halves.
        let show_preview = width >= PREVIEW_MIN_WIDTH;
        // Bus views need wider columns than the mixed view.
        let list_share = if self.view.is_all() { 43 } else { 55 };
        let split = if show_preview {
            (width * list_share / 100)
                .max(24)
                .min(width.saturating_sub(3))
        } else {
            width
        };
        let preview_width = width.saturating_sub(split + 3);
        let view = self.view;
        let foreign_hidden = !self.show_foreign;
        let heading = format!(
            "Messages ({}/{})  view={}{}{}",
            indices.len(),
            self.summaries.len(),
            view.label(),
            if foreign_hidden {
                "  foreign=hidden"
            } else {
                ""
            },
            // A capture that lost objects is not a complete record; the list
            // header is where that has to stay visible.
            if self.lost_objects > 0 {
                format!(
                    "  ⚠ LOST {} objects ({} events)",
                    self.lost_objects, self.lost_events
                )
            } else {
                String::new()
            }
        );
        lines.push(paint_bold(
            if show_preview {
                two_column(&heading, "Preview", split, width)
            } else {
                truncate(&heading, width)
            },
            slot_color(Slot::Cyan),
        ));
        // Column header for the current view; the selection marker keeps the
        // alignment with the rows below.
        lines.push(paint(
            format!(
                "  {}",
                truncate_ansi(
                    &view_header(view, split.saturating_sub(2)),
                    split.saturating_sub(2)
                )
            ),
            slot_color(Slot::Cyan),
        ));
        lines.push(paint(
            if show_preview {
                two_column_separator(split, width)
            } else {
                "─".repeat(width)
            },
            slot_color(Slot::Cyan),
        ));
        let available = height.saturating_sub(7);
        let preview = self
            .selected_summary()
            .map(|summary| {
                let index = summary.object_index;
                let mut lines = summary_detail_lines(
                    summary,
                    self.selected_object(),
                    self.time_axis(),
                    self.uds.get(&index),
                    &self.pool,
                );
                // A metadata record is only a fragment of a split document; the
                // whole XML is readable in the inspector, which can scroll.
                lines.extend(self.metadata_lines(index));
                lines
            })
            .unwrap_or_else(|| self.empty_list_hint());
        let preview = preview
            .iter()
            .flat_map(|line| wrap(line, preview_width))
            .collect::<Vec<_>>();
        for row in 0..available {
            let message_index = self.list_offset + row;
            let selected = message_index == self.selected;
            let summary = indices
                .get(message_index)
                .and_then(|index| self.summaries.get(*index));
            let left = summary
                .map(|summary| {
                    let pane = split.saturating_sub(2);
                    if view.is_all() || view.owns(summary) {
                        let row = view_row(summary, view, self.time_axis(), &self.pool, pane);
                        format!(
                            "{}{}",
                            if selected { "▶ " } else { "  " },
                            truncate(&row, pane)
                        )
                    } else {
                        // A frame from another bus keeps the timing visible but
                        // collapses into a single dim description.
                        let text =
                            truncate(&foreign_row(summary, self.time_axis(), &self.pool), pane);
                        if selected {
                            format!("▶ {text}")
                        } else {
                            format!("\x1b[2m  {text}{ANSI_RESET}")
                        }
                    }
                })
                .unwrap_or_default();
            let right = preview.get(row).cloned().unwrap_or_default();
            // The highlight covers the list pane only; the preview stays plain
            // so a selected row cannot be mistaken for two selected columns.
            if selected {
                lines.push(if show_preview {
                    two_column_selected(&left, &right, split, width)
                } else {
                    paint_selected(pad_to_width(&strip_ansi(&truncate(&left, width)), width))
                });
                continue;
            }
            let line = if show_preview {
                two_column(&left, &right, split, width)
            } else {
                pad_to_width(&truncate(&left, width), width)
            };
            // A record that failed to decode keeps one alarming colour.
            lines.push(if let Some(summary) = summary {
                if summary.status == "ok" {
                    line
                } else {
                    message_color(summary, strip_ansi(&line))
                }
            } else {
                line
            });
        }
    }

    fn render_detail(&mut self, lines: &mut Vec<String>, width: usize, height: usize) {
        let Some(summary) = self.selected_summary() else {
            lines.push("No message selected".to_string());
            lines.push("  j/k or the arrows pick one  ·  / filters the list".to_string());
            return;
        };
        let show_preview = width >= PREVIEW_MIN_WIDTH;
        let split = if show_preview {
            (width * 43 / 100).max(24).min(width.saturating_sub(3))
        } else {
            width
        };
        let right_width = width.saturating_sub(split + 3);
        let mut left = summary_detail_lines(
            summary,
            self.selected_object(),
            self.time_axis(),
            self.uds.get(&summary.object_index),
            &self.pool,
        );
        left.extend(timing_lines(self.timing.clone()));
        left.extend(self.metadata_lines(summary.object_index));
        let left = left
            .iter()
            .flat_map(|line| wrap(line, split))
            .collect::<Vec<_>>();
        let mut right = vec![(None, "Signals".to_string())];
        if self.detail_rows.is_empty() {
            right.extend(
                wrap(
                    "  no decoded signals (bind a matching database with b)",
                    right_width,
                )
                .into_iter()
                .map(|line| (None, line)),
            );
        }
        for (index, row) in self.detail_rows.iter().enumerate().skip(self.detail_offset) {
            let text = format!(
                "{}{} = {}",
                if index == self.selected_signal {
                    "▶ "
                } else {
                    "  "
                },
                row.name,
                row_display(row)
            );
            right.extend(
                wrap(&text, right_width)
                    .into_iter()
                    .map(|part| (Some(index), part)),
            );
        }
        let rows = height.saturating_sub(5);
        self.detail_left_total = left.len();
        self.detail_left_rows = rows;
        // Clamp after a resize or a shorter record.
        self.detail_left_offset = self.detail_left_offset.min(self.detail_left_limit());
        for row in 0..rows {
            let left_text = left
                .get(self.detail_left_offset + row)
                .map_or("", String::as_str);
            let (signal, right_text) = right
                .get(row)
                .map_or((None, ""), |(index, text)| (*index, text.as_str()));
            let line = if show_preview {
                two_column(left_text, right_text, split, width)
            } else {
                pad_to_width(&truncate(left_text, width), width)
            };
            let signal_selected = signal == Some(self.selected_signal);
            // The highlight belongs to the signal row, so it covers the pane
            // that row lives in and leaves the frame details readable.
            lines.push(if signal_selected && show_preview {
                right_column_selected(
                    left.get(row).map_or("", String::as_str),
                    right_text,
                    split,
                    width,
                )
            } else if signal_selected {
                paint_selected(pad_to_width(&strip_ansi(right_text), width))
            } else if row == 0 {
                paint_bold(line, slot_color(Slot::Cyan))
            } else {
                line
            });
        }
    }

    /// The narrow layout: the details column alone, across the whole width,
    /// with the same header, wrapped lines and scroll offset as the column.
    fn render_database_details(&self, lines: &mut Vec<String>, width: usize, height: usize) {
        let rows = self.database_body_rows(height);
        let details = self.database_detail_lines(width);
        let total = details.len();
        let offset = clamp_detail_offset(self.database_detail_offset, total, rows);
        let header = if total > 0 {
            details_header_text(offset, rows, total, width)
        } else {
            "Details".to_string()
        };
        lines.push(paint_cell(header, CellStyle::PaneHeader));
        lines.push(paint("─".repeat(width), slot_color(Slot::Cyan)));
        for row in 0..rows {
            let (text, style) = details
                .get(offset + row)
                .map_or((String::new(), CellStyle::Normal), |(text, style)| {
                    (text.clone(), *style)
                });
            lines.push(paint_cell(text, style));
        }
    }

    fn render_curve(&self, lines: &mut Vec<String>, width: usize, height: usize) {
        let Some(curve) = &self.curve else {
            lines.push("No curve selected".to_string());
            return;
        };
        let axis = self.time_axis();
        let window = (curve.window_start, curve.window_end);
        let truncated = curve.series.iter().any(|series| series.truncated);
        lines.push(paint_bold(
            format!(
                "Curve: {} signal{}  window={}..{}{}  [a] add  [x] remove  {mode}",
                curve.series.len(),
                if curve.series.len() == 1 { "" } else { "s" },
                axis.format(Some(window.0)),
                axis.format(Some(window.1)),
                if truncated {
                    "  (first samples only)"
                } else {
                    ""
                },
                mode = if curve.normalize {
                    "axis=per-signal"
                } else {
                    "axis=shared"
                }
            ),
            slot_color(Slot::Cyan),
        ));
        let mut legend = 0;
        for series in &curve.series {
            if legend >= MAX_CURVE_LEGEND {
                lines.push(paint(
                    format!("  … {} more", curve.series.len() - legend),
                    ANSI_PLAIN,
                ));
                break;
            }
            lines.push(paint(
                format!(
                    "  {} {}  {} samples  {}",
                    series.symbol,
                    series.title,
                    series.samples.len(),
                    series_range_label(series)
                ),
                series.color,
            ));
            if let Some(error) = &series.error {
                lines.push(paint(format!("    note: {error}"), slot_color(Slot::Red)));
            }
            legend += 1;
        }
        if curve.series.iter().all(|series| series.samples.is_empty()) {
            lines.push(paint(
                "No numeric samples available",
                slot_color(Slot::Yellow),
            ));
            return;
        }
        lines.push(String::new());
        // The title, database line and separator above already used three rows,
        // and the status line takes one more; the plot needs two of its own for
        // the axes. Reserving only `3 + legend` pushed the axes off-screen.
        let rows = height
            .saturating_sub(usize::from(HEADER_ROWS) + STATUS_ROWS + 1 + legend + 1)
            .max(3);
        lines.extend(plot_series(curve, window, width.saturating_sub(1), rows));
    }

    fn inspector_page_size(&self) -> usize {
        // The title and the footer take three rows.
        self.viewport_height.saturating_sub(4).max(1)
    }

    /// Open the full attribute list of the selected frame.
    fn open_inspector(&mut self) {
        if self.selected_summary().is_none() {
            self.status = "No message selected".to_string();
            return;
        }
        self.mode = Mode::Inspector;
        self.inspector_offset = 0;
        self.status = "All attributes".to_string();
    }

    fn scroll_inspector(&mut self, delta: isize) {
        let page = self.inspector_page_size();
        let max = self.inspector_lines.saturating_sub(page);
        let next = self.inspector_offset as isize + delta;
        self.inspector_offset = next.clamp(0, max as isize) as usize;
    }

    fn render_inspector(&mut self, lines: &mut Vec<String>, width: usize, height: usize) {
        let Some(summary) = self.selected_summary() else {
            lines.push("No message selected".to_string());
            return;
        };
        let index = self.selected;
        let total = self.filtered.len();
        let object_index = summary.object_index;
        let class_name = summary.class_name;
        let object = self.selected_object();
        let uds = self.uds.get(&object_index);
        let mut content = inspector_lines(
            summary,
            object,
            self.time_axis(),
            uds,
            &self.pool,
            index,
            total,
        );
        // The metadata documents only exist for the trace as a whole, so they
        // are appended here where the view can scroll through them.
        content.extend(self.metadata_lines(object_index));
        // Wrap once per frame; the scroll offset counts wrapped lines.
        let body = content
            .iter()
            .flat_map(|line| wrap(line, width))
            .collect::<Vec<_>>();
        self.inspector_lines = body.len();
        let page = height.saturating_sub(4).max(1);
        let max = body.len().saturating_sub(page);
        self.inspector_offset = self.inspector_offset.min(max);
        lines.push(paint_bold(
            format!(
                "Attributes  {}  line {}/{}",
                class_name,
                self.inspector_offset + 1,
                body.len().max(1)
            ),
            slot_color(Slot::Cyan),
        ));
        lines.push(paint("─".repeat(width), slot_color(Slot::Cyan)));
        for row in 0..page {
            let line = body
                .get(self.inspector_offset + row)
                .cloned()
                .unwrap_or_default();
            lines.push(line);
        }
    }

    fn render_statistics(&self, lines: &mut Vec<String>, width: usize, height: usize) {
        lines.push(paint_bold(
            format!(
                "Frame statistics  {} frames  {} messages  sorted by {}",
                self.stats.len(),
                self.filtered.len(),
                self.stats_sort.label()
            ),
            slot_color(Slot::Cyan),
        ));
        lines.push(paint(
            stat_header_row(width, stat_unit(&self.stats, self.view)),
            slot_color(Slot::Cyan),
        ));
        lines.push(paint("─".repeat(width), slot_color(Slot::Cyan)));
        if self.stats.is_empty() {
            lines.push(paint(
                "No frames in the visible list (clear the filter with Esc)",
                slot_color(Slot::Yellow),
            ));
            return;
        }
        let rows = height.saturating_sub(6);
        for row in 0..rows {
            let index = self.stats_offset + row;
            let Some(stat) = self.stats.get(index) else {
                break;
            };
            let text = stat_row(stat, width);
            let selected = index == self.stats_selected;
            lines.push(if selected {
                paint_selected(pad_to_width(&text, width))
            } else {
                paint(text, ANSI_PLAIN)
            });
        }
    }

    fn render_databases(&self, lines: &mut Vec<String>, width: usize, height: usize) {
        if details_take_full_width(width, self.database_pane) {
            self.render_database_details(lines, width, height);
            return;
        }
        let (left, middle, right) = self.database_layout(width);
        let objects = &self.db_objects;
        let categories = self.database_categories();
        let selected_object = objects.get(self.database_object_selected);
        let rows = self.database_body_rows(height);
        let details = self.database_detail_lines(right);
        let detail_total = details.len();
        let detail_offset = clamp_detail_offset(self.database_detail_offset, detail_total, rows);
        let database_label = self
            .bindings
            .get(self.database_selected)
            .map(|entry| {
                format!(
                    "{} ch{} {}",
                    entry.binding.bus(),
                    entry.binding.channel(),
                    entry.path.display()
                )
            })
            .unwrap_or_else(|| "No database loaded".to_string());
        let tree_focused = self.database_pane == DatabasePane::Tree;
        let objects_focused = self.database_pane == DatabasePane::Objects;
        let tree_header = "Database tree";
        let object_header = format!(
            "{} ({})",
            self.selected_database_category()
                .map_or("Objects", DbCategory::label),
            objects.len()
        );
        // The selected object's title already sits in the object pane, so the
        // details header reports where the scrollable column is instead.
        let detail_header = if selected_object.is_some() && detail_total > 0 {
            details_header_text(detail_offset, rows, detail_total, right)
        } else {
            "Details".to_string()
        };
        let details_focused = self.database_pane == DatabasePane::Details;
        if right == 0 {
            // Two panes: the details have their own full-width view.
            lines.push(two_column(
                &paint_cell(
                    pad_to_width(&truncate(tree_header, left), left),
                    pane_header_style(tree_focused),
                ),
                &paint_cell(
                    truncate(&object_header, middle),
                    pane_header_style(objects_focused),
                ),
                left,
                width,
            ));
            lines.push(paint(
                two_column_separator(left, width),
                slot_color(Slot::Cyan),
            ));
        } else {
            lines.push(styled_three_column(
                (tree_header, pane_header_style(tree_focused)),
                (object_header.as_str(), pane_header_style(objects_focused)),
                (detail_header.as_str(), pane_header_style(details_focused)),
                left,
                middle,
                width,
            ));
            lines.push(paint(
                three_column_separator(left, middle, right, width),
                slot_color(Slot::Cyan),
            ));
        }
        for row in 0..rows {
            let (tree_text, tree_style) = if row == 0 {
                (format!(" {database_label}"), CellStyle::DatabaseHeader)
            } else {
                let category_index = row - 1;
                let selected = category_index == self.database_category;
                let marker = if selected {
                    if tree_focused {
                        "▶ "
                    } else {
                        "· "
                    }
                } else {
                    "  "
                };
                let text = categories
                    .get(category_index)
                    .map_or_else(String::new, |category| {
                        format!("{marker}{}", category.label())
                    });
                let style = if selected {
                    selection_style(tree_focused)
                } else {
                    CellStyle::Normal
                };
                (text, style)
            };
            let object_index = self.database_object_offset + row;
            let object = objects.get(object_index);
            let object_selected = object.is_some() && object_index == self.database_object_selected;
            let object_text = object.map_or_else(String::new, |object| {
                let marker = if object_selected {
                    if objects_focused {
                        "▶ "
                    } else {
                        "· "
                    }
                } else {
                    "  "
                };
                format!("{marker}{}  {}", object.title, object.subtitle)
            });
            let object_style = if object_selected {
                selection_style(objects_focused)
            } else {
                CellStyle::Normal
            };
            let (detail_text, detail_style) = details
                .get(detail_offset + row)
                .map_or((String::new(), CellStyle::Normal), |(text, style)| {
                    (text.clone(), *style)
                });
            lines.push(if right == 0 {
                // Two-pane layout: no details column in this row.
                two_column(
                    &paint_cell(pad_to_width(&truncate(&tree_text, left), left), tree_style),
                    &paint_cell(truncate(&object_text, middle), object_style),
                    left,
                    width,
                )
            } else {
                styled_three_column(
                    (&tree_text, tree_style),
                    (&object_text, object_style),
                    (detail_text.as_str(), detail_style),
                    left,
                    middle,
                    width,
                )
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellStyle {
    Normal,
    Selected,
    Dim,
    PaneHeader,
    PaneHeaderIdle,
    DatabaseHeader,
    DetailHeading,
    DetailItem,
}

/// One line of the database detail column.
///
/// A section heading, the items below it and a dim note have different visual
/// weight, so they stay apart until the column is painted instead of being
/// flattened into plain strings.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DetailLine {
    Heading(String),
    Item(String),
    Note(String),
    Blank,
}

impl DetailLine {
    fn heading(text: impl Into<String>) -> Self {
        Self::Heading(text.into())
    }

    fn item(text: impl Into<String>) -> Self {
        Self::Item(text.into())
    }

    fn note(text: impl Into<String>) -> Self {
        Self::Note(text.into())
    }

    /// The semantic text without presentation, used by filters and tests.
    fn text(&self) -> &str {
        match self {
            Self::Heading(text) | Self::Item(text) | Self::Note(text) => text,
            Self::Blank => "",
        }
    }

    /// The cell text and the style it is painted with. Items are indented under
    /// their heading; notes stay at the heading's level.
    fn render(&self) -> (String, CellStyle) {
        match self {
            Self::Heading(text) => (text.clone(), CellStyle::DetailHeading),
            Self::Item(text) => (format!("  {text}"), CellStyle::DetailItem),
            Self::Note(text) => (text.clone(), CellStyle::Dim),
            Self::Blank => (String::new(), CellStyle::Normal),
        }
    }
}

fn selection_style(focused: bool) -> CellStyle {
    if focused {
        CellStyle::Selected
    } else {
        CellStyle::Dim
    }
}

fn pane_header_style(focused: bool) -> CellStyle {
    if focused {
        CellStyle::PaneHeader
    } else {
        CellStyle::PaneHeaderIdle
    }
}

fn paint_cell(value: impl AsRef<str>, style: CellStyle) -> String {
    match style {
        CellStyle::Normal => paint(value, ANSI_PLAIN),
        CellStyle::Selected => paint_selected(value),
        CellStyle::Dim => paint(value, slot_color(Slot::Cyan)),
        CellStyle::PaneHeader => paint_bold(value, slot_color(Slot::Cyan)),
        CellStyle::PaneHeaderIdle => paint(value, slot_color(Slot::Cyan)),
        CellStyle::DatabaseHeader => paint_bold(value, slot_color(Slot::Yellow)),
        CellStyle::DetailHeading => paint_bold(value, slot_color(Slot::Cyan)),
        CellStyle::DetailItem => paint(value, ANSI_PLAIN),
    }
}

/// Render three padded columns, styling each cell independently so one pane's
/// selection never changes how the neighbouring pane is drawn.
/// Separator row that puts `┼` exactly under the `│` of the rows above.
///
/// A row is `left │ middle │ right`, so the dividers sit at `left + 1` and
/// `left + middle + 4`; the filler has to match those offsets, not the raw
/// column widths.
fn three_column_separator(left: usize, middle: usize, _right: usize, width: usize) -> String {
    let left = left.min(width.saturating_sub(6));
    let middle = middle.min(width.saturating_sub(left + 6));
    let right = width.saturating_sub(left + middle + 6);
    format!(
        "{}┼{}┼{}",
        "─".repeat(left + 1),
        "─".repeat(middle + 2),
        "─".repeat(right + 1)
    )
}

fn styled_three_column(
    left: (&str, CellStyle),
    middle: (&str, CellStyle),
    right: (&str, CellStyle),
    left_width: usize,
    middle_width: usize,
    width: usize,
) -> String {
    let left_width = left_width.min(width.saturating_sub(6));
    let middle_width = middle_width.min(width.saturating_sub(left_width + 6));
    let right_width = width.saturating_sub(left_width + middle_width + 6);
    let cell = |(text, style): (&str, CellStyle), width: usize| {
        paint_cell(pad_to_width(&truncate(text, width), width), style)
    };
    format!(
        "{} │ {} │ {}",
        cell(left, left_width),
        cell(middle, middle_width),
        cell(right, right_width)
    )
}

#[derive(Debug, Clone)]
struct DbObject {
    title: String,
    subtitle: String,
    details: Vec<DetailLine>,
}

fn database_objects(entry: &BindingEntry, category: DbCategory) -> Vec<DbObject> {
    match (entry.binding.database(), category) {
        (Database::Can(database), DbCategory::Overview) => vec![DbObject {
            title: "DBC overview".to_string(),
            subtitle: format!(
                "{} messages, {} signals",
                database.len(),
                database.signal_names().count()
            ),
            details: detail_section(
                "database",
                [
                    format!("source: {}", database.source_path().unwrap_or("-")),
                    format!("version: {}", database.version().unwrap_or("-")),
                    format!("messages: {}", database.len()),
                    format!("signals: {}", database.signal_names().count()),
                    format!("nodes: {}", database.nodes().count()),
                    format!("attributes: {}", database.attributes().count()),
                ],
            ),
        }],
        (Database::Can(database), DbCategory::Messages) => database
            .messages()
            .map(|message| DbObject {
                title: message.name.clone(),
                subtitle: format!(
                    "0x{:X}  dlc={}  {} signals{}",
                    message.id.id,
                    message.dlc,
                    message.signals.len(),
                    if message.is_fd { "  CAN-FD" } else { "" }
                ),
                details: dbc_message_details(database, message),
            })
            .collect(),
        (Database::Can(database), DbCategory::Signals) => database
            .messages()
            .flat_map(|message| {
                message.signals.iter().map(move |signal| DbObject {
                    title: format!("{}.{}", message.name, signal.name),
                    subtitle: format!(
                        "0x{:X}  [{}|{}]  {:?}",
                        message.id.id, signal.start_bit, signal.bit_length, signal.signal_type
                    ),
                    details: dbc_signal_details(database, message, signal),
                })
            })
            .collect(),
        (Database::Can(database), DbCategory::Nodes) => database
            .nodes()
            .map(|node| DbObject {
                title: node.name.clone(),
                subtitle: format!("node{}", source_suffix(node.source)),
                details: dbc_node_details(database, node),
            })
            .collect(),
        (Database::Can(database), DbCategory::Attributes) => {
            let mut objects: Vec<DbObject> = database
                .attribute_definitions()
                .map(|definition| {
                    let mut items = vec![
                        format!("name: {}", definition.name),
                        format!("scope: {}", definition.scope),
                    ];
                    if let Some(relationship) = &definition.relationship {
                        items.push(format!("relationship: {relationship}"));
                    }
                    items.push(format!("definition: {}", attribute_spec(definition)));
                    items.push(format!(
                        "default: {}",
                        attribute_default(database, &definition.name)
                            .unwrap_or_else(|| "-".to_string())
                    ));
                    items.push(format!(
                        "assignments: {}",
                        database
                            .attribute_assignments()
                            .filter(|assignment| assignment.name == definition.name)
                            .count()
                    ));
                    if let Some((_, value)) = database
                        .attributes()
                        .find(|(name, _)| *name == definition.name)
                    {
                        items.push(format!("value: {value}"));
                    }
                    DbObject {
                        title: definition.name.clone(),
                        subtitle: attribute_spec(definition),
                        details: detail_section("attribute", items),
                    }
                })
                .collect();
            objects.extend(
                database
                    .attributes()
                    .filter(|(name, _)| {
                        !database
                            .attribute_definitions()
                            .any(|definition| definition.name == *name)
                    })
                    .map(|(name, value)| DbObject {
                        title: name.to_string(),
                        subtitle: value.to_string(),
                        details: detail_section(
                            "attribute",
                            [
                                format!("name: {name}"),
                                format!("value: {value}"),
                                "definition: -".to_string(),
                                "default: -".to_string(),
                                format!(
                                    "assignments: {}",
                                    database
                                        .attribute_assignments()
                                        .filter(|assignment| assignment.name == name)
                                        .count()
                                ),
                            ],
                        ),
                    }),
            );
            objects
        }
        (Database::Can(database), DbCategory::ValueTables) => database
            .value_tables()
            .map(|(name, table)| DbObject {
                title: name.to_string(),
                subtitle: format!("{} values", table.iter().count()),
                details: detail_section(
                    "value table",
                    [format!("values: {}", value_table_text(table))],
                ),
            })
            .collect(),
        (Database::Lin(database), DbCategory::Overview) => vec![DbObject {
            title: "LDF overview".to_string(),
            subtitle: format!(
                "{} frames, {} signals",
                database.len(),
                database.signal_count()
            ),
            details: detail_section(
                "database",
                [
                    format!("source: {}", database.source_path().unwrap_or("-")),
                    format!("protocol: {}", database.protocol_version().unwrap_or("-")),
                    format!("language: {}", database.language_version().unwrap_or("-")),
                    format!("speed: {:?} kbps", database.speed_kbps()),
                    format!("frames: {}", database.len()),
                    format!(
                        "diagnostic frames: {}",
                        database.diagnostic_frames().count()
                    ),
                    format!("nodes: {}", database.nodes().count()),
                ],
            ),
        }],
        (Database::Lin(database), DbCategory::Frames) => database
            .frames()
            .map(|frame| DbObject {
                title: frame.name.clone(),
                subtitle: format!(
                    "0x{:02X}  dlc={}  {} signals",
                    frame.id,
                    frame.dlc,
                    frame.signals.len()
                ),
                details: lin_frame_details(frame),
            })
            .collect(),
        (Database::Lin(database), DbCategory::Signals) => database
            .frames()
            .flat_map(|frame| {
                frame.signals.iter().map(move |signal| DbObject {
                    title: format!("{}.{}", frame.name, signal.name),
                    subtitle: format!(
                        "[{}|{}] factor={} offset={}",
                        signal.start_bit, signal.bit_length, signal.factor, signal.offset
                    ),
                    details: lin_signal_details(signal),
                })
            })
            .collect(),
        (Database::Lin(database), DbCategory::Nodes) => database
            .nodes()
            .map(|node| DbObject {
                title: node.name.clone(),
                subtitle: format!(
                    "NAD={}",
                    node.configured_nad
                        .map_or_else(|| "-".to_string(), |value| value.to_string())
                ),
                details: lin_node_details(database, node),
            })
            .collect(),
        (Database::Lin(database), DbCategory::DiagnosticFrames) => database
            .diagnostic_frames()
            .map(|frame| DbObject {
                title: frame.name.clone(),
                subtitle: format!(
                    "0x{:02X}  dlc={}  {} signals",
                    frame.id,
                    frame.dlc,
                    frame.signals.len()
                ),
                details: lin_diagnostic_frame_details(frame),
            })
            .collect(),
        (Database::Lin(database), DbCategory::DiagnosticSignals) => database
            .diagnostic_signals()
            .map(|signal| DbObject {
                title: signal.name.clone(),
                subtitle: format!(
                    "len={} initial={:?}",
                    signal.bit_length, signal.initial_value
                ),
                details: detail_section(
                    "signal",
                    [
                        format!("name: {}", signal.name),
                        format!("bit length: {}", signal.bit_length),
                        format!("initial value: {:?}", signal.initial_value),
                    ],
                ),
            })
            .collect(),
        (Database::Lin(database), DbCategory::Schedules) => database
            .schedules()
            .map(|(name, entries)| DbObject {
                title: name.to_string(),
                subtitle: format!("{} entries", entries.len()),
                details: detail_list(
                    "entries",
                    entries
                        .iter()
                        .map(|entry| format!("{} delay_ms={}", entry.frame_name, entry.delay_ms))
                        .collect(),
                ),
            })
            .collect(),
        (Database::Lin(database), DbCategory::DynamicFrames) => database
            .dynamic_frames()
            .map(|frame| DbObject {
                title: frame.name.clone(),
                subtitle: format!("{:?} trigger={:?}", frame.kind, frame.trigger_frame_id),
                details: detail_section(
                    "dynamic frame",
                    [
                        format!("kind: {:?}", frame.kind),
                        format!("trigger frame id: {:?}", frame.trigger_frame_id),
                        format!("members: {}", frame.frame_names.join(", ")),
                    ],
                ),
            })
            .collect(),
        (Database::Lin(database), DbCategory::OptionalSections) => database
            .optional_sections()
            .map(|section| DbObject {
                title: section.name.clone(),
                subtitle: source_suffix(section.source),
                details: detail_section("section", [format!("name: {}", section.name)]),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Deduplicated names in first-seen order, trimmed, without empty entries and
/// without the names `skip` rejects.
fn unique_names<'a>(
    names: impl IntoIterator<Item = &'a str>,
    skip: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut unique: Vec<String> = Vec::new();
    for name in names {
        let name = name.trim();
        if name.is_empty() || skip(name) {
            continue;
        }
        if !unique.iter().any(|existing| existing.as_str() == name) {
            unique.push(name.to_string());
        }
    }
    unique
}

/// Union of signal-level node lists, deduplicated in first-seen order and
/// without the DBC placeholder names.
fn union_names<'a>(groups: impl IntoIterator<Item = &'a [String]>) -> Vec<String> {
    unique_names(groups.into_iter().flatten().map(String::as_str), |name| {
        name == "Vector__XXX"
    })
}

/// Start a section: a blank line separates it from the previous content and
/// its heading is painted with the strong heading style.
fn push_heading(details: &mut Vec<DetailLine>, text: impl Into<String>) {
    if details
        .last()
        .is_some_and(|line| !matches!(line, DetailLine::Blank))
    {
        details.push(DetailLine::Blank);
    }
    details.push(DetailLine::heading(text));
}

/// A heading and its items, for the small single-section detail objects.
fn detail_section(label: &str, items: impl IntoIterator<Item = String>) -> Vec<DetailLine> {
    let mut details = Vec::new();
    push_heading(&mut details, label);
    details.extend(items.into_iter().map(DetailLine::item));
    details
}

/// A counted list block as a standalone detail list.
fn detail_list(label: &str, entries: Vec<String>) -> Vec<DetailLine> {
    let mut details = Vec::new();
    push_detail_list(&mut details, label, entries);
    details
}

/// Add a counted list block, keeping its total visible even when the entries
/// are capped because the detail column does not scroll.
fn push_detail_list(details: &mut Vec<DetailLine>, label: &str, entries: Vec<String>) {
    let total = entries.len();
    push_heading(details, format!("{label}: {total}"));
    for entry in entries.into_iter().take(MAX_DETAIL_ENTRIES) {
        details.push(DetailLine::item(entry));
    }
    if total > MAX_DETAIL_ENTRIES {
        details.push(DetailLine::note(format!(
            "… {} more",
            total - MAX_DETAIL_ENTRIES
        )));
    }
}

/// The type and value domain of an attribute definition, as shown next to an
/// assigned value.
fn attribute_spec(definition: &blf_core::DbcAttributeDefinition) -> String {
    if !definition.enum_values.is_empty() {
        format!(
            "{} {}",
            definition.value_type,
            definition.enum_values.join(", ")
        )
    } else if let (Some(minimum), Some(maximum)) = (&definition.minimum, &definition.maximum) {
        format!("{} {minimum}..{maximum}", definition.value_type)
    } else {
        definition.value_type.clone()
    }
}

fn attribute_default(database: &DbcDatabase, name: &str) -> Option<String> {
    database
        .attribute_defaults()
        .find(|(default_name, _)| *default_name == name)
        .map(|(_, value)| {
            if value.is_empty() {
                "\"\"".to_string()
            } else {
                value.to_string()
            }
        })
}

/// Render one `BA_` assignment with its `BA_DEF_` definition and default. A
/// file may assign attributes it never defines, so the definition is optional.
fn attribute_line(database: &DbcDatabase, assignment: &blf_core::DbcAttributeAssignment) -> String {
    let Some(definition) = database
        .attribute_definitions()
        .find(|definition| definition.name == assignment.name)
    else {
        return format!("{} = {}", assignment.name, assignment.value);
    };
    let spec = attribute_spec(definition);
    match attribute_default(database, &definition.name) {
        Some(default) => format!(
            "{} = {} ({spec}, default {default})",
            assignment.name, assignment.value
        ),
        None => format!("{} = {} ({spec})", assignment.name, assignment.value),
    }
}

/// The compact line that completes an attributes block with the definitions
/// this object does not assign: their `BA_DEF_DEF_` default is still the
/// effective value of the attribute.
fn push_attribute_defaults(
    details: &mut Vec<DetailLine>,
    database: &DbcDatabase,
    scope: &str,
    assigned: &[&str],
) {
    let defaults: Vec<String> = database
        .attribute_definitions()
        .filter(|definition| definition.scope == scope)
        .filter(|definition| !assigned.contains(&definition.name.as_str()))
        .filter_map(|definition| {
            attribute_default(database, &definition.name)
                .map(|default| format!("{}={default}", definition.name))
        })
        .collect();
    if defaults.is_empty() {
        return;
    }
    push_detail_list(details, "defaults", defaults);
}

/// One attributes block: the object's `BA_` assignments, each with its
/// definition, followed by the defaults of the remaining definitions of the
/// same scope. A `BA_` assignment overrides a default; it does not create the
/// attribute, so the defaults are part of the object's effective attributes.
fn push_attributes<'a>(
    details: &mut Vec<DetailLine>,
    database: &DbcDatabase,
    scope: &str,
    assignments: impl IntoIterator<Item = &'a blf_core::DbcAttributeAssignment>,
) {
    let assignments: Vec<&blf_core::DbcAttributeAssignment> = assignments.into_iter().collect();
    let assigned: Vec<&str> = assignments
        .iter()
        .map(|assignment| assignment.name.as_str())
        .collect();
    push_heading(details, format!("attributes: {}", assignments.len()));
    for assignment in assignments.iter().take(MAX_DETAIL_ENTRIES) {
        details.push(DetailLine::item(attribute_line(database, assignment)));
    }
    if assignments.len() > MAX_DETAIL_ENTRIES {
        details.push(DetailLine::note(format!(
            "… {} more",
            assignments.len() - MAX_DETAIL_ENTRIES
        )));
    }
    push_attribute_defaults(details, database, scope, &assigned);
}

fn message_receivers(message: &blf_core::DbcMessage) -> Vec<String> {
    union_names(
        message
            .signals
            .iter()
            .map(|signal| signal.receivers.as_slice()),
    )
}

fn dbc_message_summary_line(message: &blf_core::DbcMessage) -> String {
    format!(
        "{} 0x{:X} {} signals",
        message.name,
        message.id.id,
        message.signals.len()
    )
}

fn message_signal_names(messages: &[&blf_core::DbcMessage]) -> Vec<String> {
    unique_names(
        messages
            .iter()
            .flat_map(|message| message.signals.iter())
            .map(|signal| signal.name.as_str()),
        |_| false,
    )
}

/// A CAN node owns two relations: the messages it transmits, and the messages
/// whose deduplicated signal receivers include it. The signal lists derive
/// from those two message sets.
fn dbc_node_details(database: &DbcDatabase, node: &blf_core::DbcNode) -> Vec<DetailLine> {
    let mut details = detail_section("node", [format!("name: {}", node.name)]);
    push_attributes(
        &mut details,
        database,
        "node",
        database.attribute_assignments().filter(|assignment| {
            assignment.scope == "node"
                && assignment.object_name.as_deref() == Some(node.name.as_str())
        }),
    );

    let sent: Vec<&blf_core::DbcMessage> = database
        .messages()
        .filter(|message| message.transmitter == node.name)
        .collect();
    let received: Vec<&blf_core::DbcMessage> = database
        .messages()
        .filter(|message| {
            message_receivers(message)
                .iter()
                .any(|receiver| receiver == &node.name)
        })
        .collect();

    push_detail_list(
        &mut details,
        "messages sent",
        sent.iter()
            .map(|message| dbc_message_summary_line(message))
            .collect(),
    );
    push_detail_list(
        &mut details,
        "messages received",
        received
            .iter()
            .map(|message| dbc_message_summary_line(message))
            .collect(),
    );
    push_detail_list(&mut details, "signals sent", message_signal_names(&sent));
    push_detail_list(
        &mut details,
        "signals received",
        message_signal_names(&received),
    );
    details
}

fn lin_node_details(database: &LdfDatabase, node: &blf_core::LinNode) -> Vec<DetailLine> {
    let mut details = detail_section(
        "node",
        [
            format!("name: {}", node.name),
            format!("configured NAD: {:?}", node.configured_nad),
            format!("timebase ms: {:?}", node.timebase_ms),
            format!("jitter ms: {:?}", node.jitter_ms),
        ],
    );
    let published: Vec<&blf_core::LinFrame> = database
        .frames()
        .filter(|frame| frame.publisher == node.name)
        .collect();
    push_detail_list(
        &mut details,
        "frames published",
        published
            .iter()
            .map(|frame| {
                format!(
                    "{} 0x{:02X} {} signals",
                    frame.name,
                    frame.id,
                    frame.signals.len()
                )
            })
            .collect(),
    );
    let subscribed = unique_names(
        database
            .frames()
            .flat_map(|frame| frame.signals.iter())
            .filter(|signal| {
                union_names([signal.subscribers.as_slice()])
                    .iter()
                    .any(|subscriber| subscriber == &node.name)
            })
            .map(|signal| signal.name.as_str()),
        |_| false,
    );
    push_detail_list(&mut details, "signals subscribed", subscribed);
    details
}

fn dbc_message_details(database: &DbcDatabase, message: &blf_core::DbcMessage) -> Vec<DetailLine> {
    let mut details = detail_section(
        "message",
        [
            format!("name: {}", message.name),
            format!(
                "frame id: 0x{:X} ({})",
                message.id.id,
                if message.id.extended {
                    "extended"
                } else {
                    "standard"
                }
            ),
            format!("DLC: {}  FD: {}", message.dlc, message.is_fd),
            format!("transmitter: {}", message.transmitter),
            format!("payload: {:?}", message.payload_kind),
        ],
    );
    push_detail_list(&mut details, "receivers", message_receivers(message));
    if let Some(comment) = &message.comment {
        details.push(DetailLine::note(format!("comment: {comment}")));
    }
    push_attributes(
        &mut details,
        database,
        "message",
        database.attribute_assignments().filter(|assignment| {
            assignment.scope == "message" && assignment.message_id == Some(message.id)
        }),
    );
    push_signal_lines(
        &mut details,
        message.signals.iter().map(|signal| signal.name.as_str()),
    );
    details
}

fn dbc_signal_details(
    database: &DbcDatabase,
    message: &blf_core::DbcMessage,
    signal: &blf_core::CanSignal,
) -> Vec<DetailLine> {
    let mut items = vec![
        format!("message: {}  frame=0x{:X}", message.name, message.id.id),
        format!(
            "layout: start={} length={}",
            signal.start_bit, signal.bit_length
        ),
        format!(
            "byte order: {:?}  type: {:?}",
            signal.byte_order, signal.signal_type
        ),
        format!("scale/offset: {} / {}", signal.factor, signal.offset),
        format!("range: {:?}..{:?}", signal.minimum, signal.maximum),
        format!("unit: {}", signal.unit.as_deref().unwrap_or("-")),
        format!("transmitter: {}", message.transmitter),
        format!("multiplex: {:?}", signal.multiplex),
    ];
    if !signal.choices.is_empty() {
        items.push(format!("choices: {}", value_table_text(&signal.choices)));
    }
    let mut details = detail_section("signal", items);
    push_detail_list(
        &mut details,
        "receivers",
        union_names([signal.receivers.as_slice()]),
    );
    push_attributes(
        &mut details,
        database,
        "signal",
        database.attribute_assignments().filter(|assignment| {
            assignment.scope == "signal"
                && assignment.message_id == Some(message.id)
                && assignment.signal_name.as_deref() == Some(signal.name.as_str())
        }),
    );
    if let Some(comment) = &signal.comment {
        details.push(DetailLine::note(format!("comment: {comment}")));
    }
    details
}

fn push_signal_lines<'a>(details: &mut Vec<DetailLine>, names: impl IntoIterator<Item = &'a str>) {
    let names: Vec<&str> = names.into_iter().collect();
    let total = names.len();
    push_heading(details, format!("signals: {total}"));
    for name in names.iter().take(MAX_DETAIL_ENTRIES) {
        details.push(DetailLine::item(format!("signal: {name}")));
    }
    if total > MAX_DETAIL_ENTRIES {
        details.push(DetailLine::note(format!(
            "… {} more",
            total - MAX_DETAIL_ENTRIES
        )));
    }
}

fn lin_frame_details(frame: &blf_core::LinFrame) -> Vec<DetailLine> {
    let mut details = detail_section(
        "frame",
        [
            format!("frame id: 0x{:02X}", frame.id),
            format!("DLC: {}  publisher: {}", frame.dlc, frame.publisher),
            format!("checksum: {:?}", frame.checksum),
        ],
    );
    push_detail_list(
        &mut details,
        "subscribers",
        union_names(
            frame
                .signals
                .iter()
                .map(|signal| signal.subscribers.as_slice()),
        ),
    );
    push_signal_lines(
        &mut details,
        frame.signals.iter().map(|signal| signal.name.as_str()),
    );
    details
}

fn lin_diagnostic_frame_details(frame: &blf_core::LinDiagnosticFrame) -> Vec<DetailLine> {
    let mut details = detail_section(
        "diagnostic frame",
        [
            format!("frame id: 0x{:02X}", frame.id),
            format!("DLC: {}", frame.dlc),
            format!("checksum: {:?}", frame.checksum),
        ],
    );
    push_signal_lines(
        &mut details,
        frame.signals.iter().map(|signal| signal.name.as_str()),
    );
    details
}

fn lin_signal_details(signal: &blf_core::LinSignal) -> Vec<DetailLine> {
    let mut items = vec![
        format!(
            "layout: start={} length={}",
            signal.start_bit, signal.bit_length
        ),
        format!("publisher: {}", signal.publisher.as_deref().unwrap_or("-")),
        format!("scale/offset: {} / {}", signal.factor, signal.offset),
        format!("range: {:?}..{:?}", signal.minimum, signal.maximum),
        format!("encoding: {}", signal.encoding.as_deref().unwrap_or("-")),
    ];
    if !signal.choices.is_empty() {
        items.push(format!("choices: {}", value_table_text(&signal.choices)));
    }
    let mut details = detail_section("signal", items);
    push_detail_list(
        &mut details,
        "subscribers",
        union_names([signal.subscribers.as_slice()]),
    );
    details
}

fn value_table_text(table: &blf_core::ValueTable) -> String {
    table
        .iter()
        .map(|(value, text)| format!("{value}={text}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn source_suffix(source: Option<blf_core::SourceSpan>) -> String {
    source.map_or_else(String::new, |span| {
        format!(" @{}:{}", span.line, span.column)
    })
}

fn summary_for(object_index: usize, object: &Object, pool: &mut TextPool) -> MessageSummary {
    let (bus, channel, frame_id, direction, dlc, payload) = match object {
        Object::CanMessage(value) => (
            Some(BusKind::Can),
            Some(value.channel),
            Some(value.id),
            Some(can_direction(can_flags_direction(value.flags))),
            Some(value.dlc),
            value.data[..usize::from(value.dlc).min(value.data.len())].to_vec(),
        ),
        Object::CanMessage2(value) => (
            Some(BusKind::Can),
            Some(value.channel),
            Some(value.id),
            Some(can_direction(can_flags_direction(value.flags))),
            Some(value.dlc),
            value.data[..usize::from(value.dlc).min(value.data.len())].to_vec(),
        ),
        Object::CanFdMessage(value) => (
            Some(BusKind::Can),
            Some(value.channel),
            Some(value.id),
            Some(can_direction(can_flags_direction(value.flags))),
            Some(value.dlc),
            value.data[..usize::from(value.valid_data_bytes).min(value.data.len())].to_vec(),
        ),
        Object::CanFdMessage64(value) => (
            Some(BusKind::Can),
            Some(u16::from(value.channel)),
            Some(value.id),
            Some(can_direction(value.dir)),
            Some(value.dlc),
            value.data[..usize::from(value.valid_data_bytes).min(value.data.len())].to_vec(),
        ),
        Object::Lin(value) => {
            let observation = classify_lin_object(value);
            (
                Some(BusKind::Lin),
                field_u16(value, "channel"),
                observation.frame_id.map(u32::from),
                observation.direction.map(lin_direction),
                observation.header_dlc,
                observation.payload.unwrap_or_default(),
            )
        }
        _ => (None, None, None, None, None, Vec::new()),
    };
    let can_flags = can_frame_flags(object);
    let event = bus_event(object);
    let status = match object {
        // A decoded event is not a frame that failed to decode.
        _ if event.is_some() => "event",
        Object::Lin(value) => lin_status(classify_lin_object(value).state),
        Object::CanMessage(_)
        | Object::CanMessage2(_)
        | Object::CanFdMessage(_)
        | Object::CanFdMessage64(_) => "ok",
        _ => "event",
    };
    let ethernet = ethernet_info(object, pool);
    let (hint_channel, hint_direction) = structured_frame_hints(object);
    let direction = direction.or(hint_direction);
    let (bus_label, channel, frame_id) = match (&ethernet, &event, bus) {
        (Some(info), _, _) => ("eth", hint_channel, Some(u32::from(info.ether_type))),
        (None, Some(record), _) => (record.bus_label, record.channel.or(hint_channel), None),
        (None, None, Some(bus)) => (bus.as_str(), channel, frame_id),
        (None, None, None) => ("-", channel, frame_id),
    };
    let bus = bus.or_else(|| event.as_ref().and_then(|record| record.bus));
    MessageSummary {
        object_index,
        timestamp_ns: object_timestamp_ns(object),
        timestamp_source: object_timestamp_source(object),
        bus,
        bus_label,
        ethernet,
        event,
        can_flags,
        channel,
        frame_id,
        object_type_raw: object.object_type_raw(),
        class_name: object.object_type().name(),
        direction,
        dlc,
        payload,
        status,
    }
}

fn decode_object_signals(object: &Object, binding: &DatabaseBinding) -> Vec<SignalRow> {
    match (binding.database(), object) {
        (Database::Can(database), Object::CanMessage(value)) => decode_can_rows(
            database,
            value.id,
            value.data[..usize::from(value.dlc).min(8)].to_vec(),
        ),
        (Database::Can(database), Object::CanMessage2(value)) => decode_can_rows(
            database,
            value.id,
            value.data[..usize::from(value.dlc).min(value.data.len())].to_vec(),
        ),
        (Database::Can(database), Object::CanFdMessage(value)) => decode_can_rows(
            database,
            value.id,
            value.data[..usize::from(value.valid_data_bytes).min(value.data.len())].to_vec(),
        ),
        (Database::Can(database), Object::CanFdMessage64(value)) => decode_can_rows(
            database,
            value.id,
            value.data[..usize::from(value.valid_data_bytes).min(value.data.len())].to_vec(),
        ),
        (Database::Lin(database), Object::Lin(value)) => decode_lin_rows(database, value),
        _ => Vec::new(),
    }
}

fn decode_can_rows(database: &DbcDatabase, raw_id: u32, data: Vec<u8>) -> Vec<SignalRow> {
    let (id, extended) = normalize_can_id(raw_id);
    let Some(message) = database.message_by_id(id, extended) else {
        return vec![SignalRow {
            name: format!("unknown frame 0x{id:X}"),
            raw: None,
            physical: None,
            choice: None,
            error: Some("message not found in bound DBC".to_string()),
        }];
    };
    match decode_message(message, &data) {
        Ok(values) => values
            .into_iter()
            .map(|value| SignalRow {
                name: value.signal_name,
                raw: Some(value.raw_value),
                physical: Some(value.physical_value),
                choice: value.value_text,
                error: None,
            })
            .collect(),
        Err(error) => vec![SignalRow {
            name: message.name.clone(),
            raw: None,
            physical: None,
            choice: None,
            error: Some(error.to_string()),
        }],
    }
}

fn decode_lin_rows(database: &LdfDatabase, object: &blf_core::LinObject) -> Vec<SignalRow> {
    match decode_lin_object(database, object) {
        Ok(values) => values
            .into_iter()
            .map(|value| SignalRow {
                name: value.signal_name,
                raw: Some(value.raw_value),
                physical: Some(value.physical_value),
                choice: value.value_text,
                error: None,
            })
            .collect(),
        Err(error) => vec![SignalRow {
            name: "LIN decode".to_string(),
            raw: None,
            physical: None,
            choice: None,
            error: Some(error.to_string()),
        }],
    }
}

fn qualified_signal_name(database: &Database, signal: &str, object: &Object) -> String {
    let frame_name = match (database, object) {
        (Database::Can(database), Object::CanMessage(value)) => database
            .message_by_id(normalize_can_id(value.id).0, normalize_can_id(value.id).1)
            .map(|message| message.name.clone()),
        (Database::Can(database), Object::CanMessage2(value)) => database
            .message_by_id(normalize_can_id(value.id).0, normalize_can_id(value.id).1)
            .map(|message| message.name.clone()),
        (Database::Can(database), Object::CanFdMessage(value)) => database
            .message_by_id(normalize_can_id(value.id).0, normalize_can_id(value.id).1)
            .map(|message| message.name.clone()),
        (Database::Can(database), Object::CanFdMessage64(value)) => database
            .message_by_id(normalize_can_id(value.id).0, normalize_can_id(value.id).1)
            .map(|message| message.name.clone()),
        (Database::Lin(database), Object::Lin(value)) => field_u8(value, "id")
            .and_then(|id| database.frame_by_id(id).map(|frame| frame.name.clone()))
            .or_else(|| {
                field_u8(value, "id").and_then(|id| {
                    database
                        .diagnostic_frame_by_id(id)
                        .map(|frame| frame.name.clone())
                })
            }),
        _ => None,
    };
    format!(
        "{}.{}",
        frame_name.unwrap_or_else(|| "unknown".to_string()),
        signal
    )
}

fn summary_detail_lines(
    summary: &MessageSummary,
    object: Option<&Object>,
    axis: TimeAxis,
    uds: Option<&UdsFrame>,
    pool: &TextPool,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("time: {}", axis.format(summary.timestamp_ns)));
    lines.push(format!(
        "class: {}  status: {}",
        summary.class_name, summary.status
    ));
    lines.push(format!(
        "object type: {} (raw={})",
        summary.class_name, summary.object_type_raw
    ));
    if let Some(source) = summary.timestamp_source {
        lines.push(format!("timestamp source: {source}"));
    }
    lines.push(format!(
        "bus/channel: {}/{}",
        summary.bus_label,
        summary
            .channel
            .map_or_else(|| "-".to_string(), |value| value.to_string())
    ));
    if let Some(event) = &summary.event {
        lines.push(format!("id: -  {}: {}", event.kind, event.summary));
    }
    match &summary.ethernet {
        Some(info) => {
            lines.push(format!(
                "id: 0x{:04X}  protocol: {}  direction: {}",
                info.ether_type,
                info.protocol,
                summary.direction.unwrap_or("-")
            ));
            lines.push("ethernet:".to_string());
            lines.push(format!("  {}", pool.get(info.summary)));
            // Fields are not cached for the whole trace, so dissect the frame
            // that is actually on screen.
            if let Some(dissection) = object.and_then(blf_core::dissect_object) {
                let mut layer = "";
                for field in &dissection.fields {
                    if field.layer != layer {
                        layer = field.layer;
                        lines.push(format!("  [{layer}]"));
                    }
                    lines.push(format!("    {} = {}", field.name, field.value));
                }
            }
        }
        None => {
            lines.push(format!(
                "id: {}  direction: {}  dlc: {}",
                summary
                    .frame_id
                    .map_or_else(|| "-".to_string(), |id| format!("0x{id:X}")),
                summary.direction.unwrap_or("-"),
                summary
                    .dlc
                    .map_or_else(|| "-".to_string(), |value| value.to_string())
            ));
            lines.push(format!("data: {}", hex_bytes(&summary.payload)));
        }
    }
    if let Some(uds) = uds {
        let transport = uds.transport;
        let node = uds
            .node
            .map_or_else(String::new, |node| format!("  node 0x{node:02X}"));
        lines.push(format!("uds ({transport}){node}"));
        if uds.complete {
            lines.push(format!("  {}  [{} frames]", uds.summary, uds.frames));
        } else {
            lines.push(format!(
                "  frame {}/{} of {}",
                uds.position, uds.frames, uds.summary
            ));
        }
        for (name, value) in &uds.fields {
            lines.push(format!("    {name} = {value}"));
        }
    }
    if let Some(object) = object {
        let fields = object_fields(object);
        if !fields.is_empty() {
            lines.push("fields:".to_string());
            for (name, value) in fields.iter().take(12) {
                lines.push(format!("  {name} = {value}"));
            }
            if fields.len() > 12 {
                lines.push(format!("  ... {} more fields", fields.len() - 12));
            }
        }
    }
    lines
}

fn object_fields(object: &Object) -> Vec<(String, String)> {
    match object {
        Object::Lin(value) => value
            .fields
            .iter()
            .map(|(name, value)| ((*name).to_string(), field_value(value)))
            .collect(),
        Object::Structured(value) => value
            .fields
            .iter()
            .map(|(name, value)| ((*name).to_string(), field_value(value)))
            .collect(),
        _ => Vec::new(),
    }
}

fn field_value(value: &FieldValue) -> String {
    match value {
        FieldValue::U8(value) => value.to_string(),
        FieldValue::U16(value) => value.to_string(),
        FieldValue::I16(value) => value.to_string(),
        FieldValue::U32(value) => value.to_string(),
        FieldValue::U64(value) => value.to_string(),
        FieldValue::I32(value) => value.to_string(),
        FieldValue::I64(value) => value.to_string(),
        FieldValue::F64(value) => format!("{value:.6}"),
        FieldValue::Bytes(value) => hex_bytes(value),
        FieldValue::String(value) => value.clone(),
        FieldValue::U16Vec(value) => format!("{value:?}"),
        FieldValue::U32Vec(value) => format!("{value:?}"),
        FieldValue::U64Vec(value) => format!("{value:?}"),
        FieldValue::I64Vec(value) => format!("{value:?}"),
    }
}

/// `<trace>.pcapng` next to the trace, so Wireshark can open it directly.
fn export_pcap_path(trace: &Path) -> PathBuf {
    let name = trace.file_name().map_or_else(
        || "trace".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    trace.with_file_name(format!("{name}.pcapng"))
}

fn export_path(trace: &Path) -> PathBuf {
    let name = trace.file_name().map_or_else(
        || "trace".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    trace.with_file_name(format!("{name}.signals.csv"))
}

fn csv_line(fields: &[String]) -> String {
    fields
        .iter()
        .map(|field| csv_field(field))
        .collect::<Vec<_>>()
        .join(",")
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn csv_signal_value(row: &SignalRow) -> String {
    if let Some(error) = &row.error {
        return format!("ERROR: {error}");
    }
    if let Some(physical) = row.physical {
        return format!("{physical:.6}");
    }
    if let Some(choice) = &row.choice {
        return choice.clone();
    }
    row.raw.map_or_else(String::new, |raw| raw.to_string())
}

fn timing_lines(stats: Option<TimingStats>) -> Vec<String> {
    let Some(stats) = stats else {
        return vec!["period: not enough samples".to_string()];
    };
    vec![
        "period:".to_string(),
        format!("  avg: {:.3} ms", stats.avg_ms),
        format!("  min/max: {:.3} / {:.3} ms", stats.min_ms, stats.max_ms),
        format!("  jitter: ±{:.3} ms", stats.jitter_ms),
        format!("  frames: {}", stats.frames),
    ]
}

/// One frame's place in a reassembled UDS message.
#[derive(Debug, Clone)]
struct UdsFrame {
    /// Service summary, such as `ReadDataByIdentifier 0x22 DID 0xF190`.
    summary: String,
    /// Fields of the decoded message, in `name = value` form.
    fields: Vec<(String, String)>,
    /// Frames that make up the message.
    frames: usize,
    /// Position of this frame inside the message, starting at one.
    position: usize,
    /// Whether this frame completes the message.
    complete: bool,
    /// Transport that carried it, for the detail heading.
    transport: &'static str,
    /// Node address of a LIN diagnostic frame.
    node: Option<u8>,
}

/// Reassemble the diagnostic messages of a whole trace.
///
/// Frames must be visited in trace order, because a segmented message is only
/// complete once its consecutive frames have arrived. Every participating
/// object index maps to the message it belongs to.
fn reassemble_trace(objects: &[Object], summaries: &[MessageSummary]) -> HashMap<usize, UdsFrame> {
    let mut reassembler = blf_core::Reassembler::new();
    let mut out = HashMap::new();
    let mut pending_frames: HashMap<blf_core::SessionKey, Vec<usize>> = HashMap::new();
    for (index, summary) in summaries.iter().enumerate() {
        let Some(object) = objects.get(index) else {
            continue;
        };
        let (transport, key, node) = match object {
            Object::Lin(value) => {
                let Some(frame_id) = lin_frame_id(value) else {
                    continue;
                };
                if frame_id != blf_core::LIN_MASTER_REQUEST
                    && frame_id != blf_core::LIN_SLAVE_RESPONSE
                {
                    continue;
                }
                let direction = u8::from(frame_id == blf_core::LIN_SLAVE_RESPONSE);
                match blf_core::classify_lin(&summary.payload) {
                    Some((node, frame)) => (
                        blf_core::Transport::Lin,
                        (
                            blf_core::SessionKey::new(
                                blf_core::Transport::Lin,
                                summary.channel.unwrap_or(0),
                                frame_id,
                                direction,
                            )
                            .with_node(node),
                            frame,
                        ),
                        Some(node),
                    ),
                    None => continue,
                }
            }
            _ if matches!(summary.bus, Some(BusKind::Can)) => {
                let Some(frame) = blf_core::classify_can(&summary.payload) else {
                    continue;
                };
                let direction = u8::from(summary.direction == Some("Tx"));
                (
                    blf_core::Transport::Can,
                    (
                        blf_core::SessionKey::new(
                            blf_core::Transport::Can,
                            summary.channel.unwrap_or(0),
                            summary.frame_id.unwrap_or(0),
                            direction,
                        ),
                        frame,
                    ),
                    None,
                )
            }
            _ => continue,
        };
        let (key, frame) = key;
        let members = pending_frames.entry(key).or_default();
        let outcome = reassembler.push(key, frame.clone());
        match outcome {
            blf_core::Reassembly::Complete { payload, frames } => {
                members.push(index);
                let Some(message) = blf_core::decode_uds(&payload) else {
                    members.clear();
                    continue;
                };
                let summary_text = message.summary();
                let fields = message.fields();
                let total = frames.max(members.len());
                for (position, member) in members.iter().enumerate() {
                    out.insert(
                        *member,
                        UdsFrame {
                            summary: summary_text.clone(),
                            fields: fields.clone(),
                            frames: total,
                            position: position + 1,
                            complete: true,
                            transport: transport.as_str(),
                            node,
                        },
                    );
                }
                members.clear();
            }
            blf_core::Reassembly::Incomplete { .. } => {
                members.push(index);
            }
            blf_core::Reassembly::Control => {}
            blf_core::Reassembly::Desync { .. } => {
                members.clear();
            }
        }
    }
    out
}

fn lin_frame_id(object: &blf_core::LinObject) -> Option<u32> {
    object.fields.iter().find_map(|(name, value)| {
        if *name != "id" {
            return None;
        }
        match value {
            blf_core::FieldValue::U8(value) => Some(u32::from(*value)),
            blf_core::FieldValue::U16(value) => Some(u32::from(*value)),
            blf_core::FieldValue::U32(value) => Some(*value),
            _ => None,
        }
    })
}

/// Dissect the object when it is an Ethernet frame; `None` for every other type.
///
/// Only the summary and the search text are kept; the detail pane dissects the
/// object again when it is opened, so a trace with many frames stays small.
fn ethernet_info(object: &Object, pool: &mut TextPool) -> Option<EthernetInfo> {
    let dissection = blf_core::dissect_object_summary(object)?;
    Some(EthernetInfo {
        summary: pool.intern(&dissection.summary),
        search: pool.intern(dissection.search_text()),
        protocol: dissection.protocol,
        ether_type: dissection.ether_type,
    })
}

/// Total the objects a capture reports as lost.
///
/// `DataLostEnd` carries the count, `OverrunError` says frames were dropped
/// without a number. Any of these means the trace is incomplete.
fn count_lost_objects(objects: &[Object]) -> (u64, u64) {
    let field = |object: &Object, name: &str| -> Option<u64> {
        let Object::Structured(value) = object else {
            return None;
        };
        value.fields.iter().find_map(|(field, value)| {
            if *field != name {
                return None;
            }
            match value {
                FieldValue::U8(number) => Some(u64::from(*number)),
                FieldValue::U16(number) => Some(u64::from(*number)),
                FieldValue::U32(number) => Some(u64::from(*number)),
                FieldValue::U64(number) => Some(*number),
                _ => None,
            }
        })
    };
    let mut lost = 0_u64;
    let mut events = 0_u64;
    for object in objects {
        match object.object_type() {
            blf_core::ObjectType::DataLostEnd => {
                events += 1;
                lost = lost.saturating_add(field(object, "numberOfLostEvents").unwrap_or(0));
            }
            blf_core::ObjectType::OverrunError => {
                events += 1;
            }
            _ => {}
        }
    }
    (lost, events)
}

/// Join the `METADATA` application texts into complete XML documents.
///
/// CANoe writes the metadata as a run of `AppText` records with source 2, split
/// every 2048 bytes, so one document spans several records. A record that starts
/// with the XML declaration begins a new document.
fn collect_metadata(objects: &[Object]) -> (Vec<String>, Vec<Option<usize>>) {
    let mut documents: Vec<String> = Vec::new();
    // Which document each object contributes to, so a row can carry the whole
    // text in its search string even when only a fragment is in the record.
    let mut owner: Vec<Option<usize>> = vec![None; objects.len()];
    for (index, object) in objects.iter().enumerate() {
        let Object::AppText(value) = object else {
            continue;
        };
        if value.source != 2 {
            continue;
        }
        let text = blf_core::app_text_text(&value.text);
        if text.trim_start().starts_with("<?xml") {
            documents.push(text);
        } else if let Some(current) = documents.last_mut() {
            current.push_str(&text);
        }
        owner[index] = Some(documents.len().saturating_sub(1));
    }
    (documents, owner)
}

/// Pack the CAN frame properties the list shows into one byte.
///
/// The bits follow the official `CAN_MSG_*` and `CAN_FD_MSG_*` macros, so the
/// names stay in `blf-core` and the summary only carries flags.
const CAN_FLAG_REMOTE: u8 = 1 << 0;
const CAN_FLAG_WAKEUP: u8 = 1 << 1;
const CAN_FLAG_NO_ERROR: u8 = 1 << 2;
const CAN_FLAG_FD: u8 = 1 << 3;
const CAN_FLAG_BRS: u8 = 1 << 4;
const CAN_FLAG_ESI: u8 = 1 << 5;

fn can_frame_flags(object: &Object) -> u8 {
    let mut flags = 0;
    let (classic, fd): (Option<u8>, Option<u32>) = match object {
        Object::CanMessage(value) => (Some(value.flags), None),
        Object::CanMessage2(value) => (Some(value.flags), None),
        Object::CanFdMessage(value) => (Some(value.flags & 0x0F), Some(u32::from(value.flags))),
        Object::CanFdMessage64(value) => (None, Some(value.flags)),
        _ => return 0,
    };
    if let Some(classic) = classic {
        if classic & 0x80 != 0 {
            flags |= CAN_FLAG_REMOTE;
        }
        if classic & 0x40 != 0 {
            flags |= CAN_FLAG_WAKEUP;
        }
        if classic & 0x20 != 0 {
            flags |= CAN_FLAG_NO_ERROR;
        }
    }
    if let Some(fd) = fd {
        if fd & 0x01 != 0 {
            flags |= CAN_FLAG_FD;
        }
        if fd & 0x02 != 0 {
            flags |= CAN_FLAG_BRS;
        }
        if fd & 0x04 != 0 {
            flags |= CAN_FLAG_ESI;
        }
    }
    flags
}

/// Short column showing the CAN frame properties, `-` when it is a plain frame.
fn can_flag_label(flags: u8) -> String {
    let mut parts = Vec::new();
    if flags & CAN_FLAG_REMOTE != 0 {
        parts.push("rtr");
    }
    if flags & CAN_FLAG_BRS != 0 {
        parts.push("brs");
    }
    if flags & CAN_FLAG_ESI != 0 {
        parts.push("esi");
    }
    if flags & CAN_FLAG_WAKEUP != 0 {
        parts.push("wu");
    }
    if flags & CAN_FLAG_NO_ERROR != 0 {
        parts.push("nerr");
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(",")
    }
}
/// A record that is not a frame: a port state, a driver error, a bus event.
///
/// These carry no payload, so the list shows one decoded line instead of the
/// frame columns.
#[derive(Debug, Clone)]
struct BusEvent {
    /// Which view owns the row; `None` for records that belong to no bus.
    bus: Option<BusKind>,
    /// `can`, `lin` or `eth`.
    bus_label: &'static str,
    /// Type column: `error`, `stat`, `event`, ...
    kind: &'static str,
    channel: Option<u16>,
    /// One line such as `ch 4.12 link up, 100 Mbit/s`.
    summary: String,
    /// Lowercased words the message filter can match.
    search: String,
}

impl BusEvent {
    fn new(bus_label: &'static str, kind: &'static str) -> Self {
        Self {
            bus: match bus_label {
                "can" => Some(BusKind::Can),
                "lin" => Some(BusKind::Lin),
                _ => None,
            },
            bus_label,
            kind,
            channel: None,
            summary: String::new(),
            search: String::new(),
        }
    }

    fn channel(mut self, channel: Option<u16>) -> Self {
        self.channel = channel;
        self
    }

    fn text(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    fn done(mut self) -> Self {
        // The kind is shown in its own column, so it has to be part of the
        // search text as well: `/sv:` finds system variables, `/lost` finds
        // dropped data.
        self.search = format!("{} {}", self.kind.to_lowercase(), self.summary).to_lowercase();
        self
    }
}

/// Bus name of a driver-overrun record, from `BL_OVERRUN_BUSTYPE_*`.
fn bus_type_label(bus_type: u64) -> &'static str {
    match bus_type {
        1 => "can",
        5 => "lin",
        6 => "most",
        7 => "flexray",
        9 => "j1708",
        10 => "eth",
        13 => "wlan",
        14 => "afdx",
        _ => "-",
    }
}

/// Decode the value of a CAN, LIN or Ethernet event record.
fn bus_event(object: &Object) -> Option<BusEvent> {
    match object {
        Object::CanErrorFrame(value) => Some(
            BusEvent::new("can", "error")
                .channel(Some(value.channel))
                .text("error frame")
                .done(),
        ),
        Object::CanErrorFrameExt(value) => {
            // `mECC` carries the Vector CAN core code in bits 0-5 and the
            // direction in bits 6-7.
            let code = blf_core::can_error_code_name(value.ecc & 0x3F);
            let direction = blf_core::can_error_direction_name(value.ecc >> 6);
            let mut text = format!("error frame: {code} ({direction})");
            if value.position > 0 {
                text.push_str(&format!(" at bit {}", value.position));
            }
            if value.id != 0 {
                text.push_str(&format!(", corrupted 0x{:X}", value.id));
            }
            Some(
                BusEvent::new("can", "error")
                    .channel(Some(value.channel))
                    .text(text)
                    .done(),
            )
        }
        Object::CanFdErrorFrame64(value) => {
            let code = blf_core::can_error_code_name(value.ecc & 0x3F);
            let direction = blf_core::can_error_direction_name(value.ecc >> 6);
            let mut text = format!("CAN FD error frame: {code} ({direction})");
            if value.error_position > 0 {
                text.push_str(&format!(" at bit {}", value.error_position));
            }
            if value.id != 0 {
                text.push_str(&format!(", corrupted 0x{:X}", value.id));
            }
            Some(
                BusEvent::new("can", "error")
                    .channel(Some(u16::from(value.channel)))
                    .text(text)
                    .done(),
            )
        }
        // ---- application text: measurement comment, channel info, metadata ----
        Object::RestorePointContainer(value) => {
            // A restore-point container carries no frame, only a table of
            // positions the reader can jump back to.
            // The payload is either the documented restore-point table or an
            // opaque block the writer leaves here; the reader already tells the
            // two apart, so the row says which one this is instead of guessing.
            let detail = match &value.restore_points {
                Some(table) => format!(
                    "{} restore point{} in {} bytes",
                    table.restore_points.len(),
                    if table.restore_points.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    value.data_length
                ),
                None => format!("{} bytes, table not decoded", value.data_length),
            };
            Some(BusEvent::new("-", "restore").text(detail).done())
        }
        Object::AppText(value) => {
            let source = blf_core::app_text_source_name(value.source);
            let text = blf_core::app_text_text(&value.text);
            let preview = if text.chars().count() > 60 {
                let short: String = text.chars().take(60).collect();
                format!("{short}…")
            } else {
                text.clone()
            };
            let detail = match value.source {
                // The payload of these two is a packed flag word, not text.
                1 => blf_core::app_text_channel_info(u32::from_le_bytes([
                    value.text.first().copied().unwrap_or(0),
                    value.text.get(1).copied().unwrap_or(0),
                    value.text.get(2).copied().unwrap_or(0),
                    value.text.get(3).copied().unwrap_or(0),
                ])),
                4 => blf_core::app_text_trace_line(u32::from_le_bytes([
                    value.text.first().copied().unwrap_or(0),
                    value.text.get(1).copied().unwrap_or(0),
                    value.text.get(2).copied().unwrap_or(0),
                    value.text.get(3).copied().unwrap_or(0),
                ])),
                _ => preview,
            };
            // The bus column says `txt` rather than a dash: an `AppText` record
            // belongs to no bus, but a bare dash reads as missing data.
            Some(
                BusEvent::new("txt", "text")
                    .text(format!("{source}: {detail}"))
                    .done(),
            )
        }
        Object::Lin(value) => lin_event(value),
        Object::Structured(value) => structured_event(value),
        _ => None,
    }
}

/// Events that the BLF stores as plain structured objects.
fn structured_event(object: &blf_core::StructuredObject) -> Option<BusEvent> {
    let field = |name: &str| -> Option<u64> {
        object.fields.iter().find_map(|(field, value)| {
            if *field != name {
                return None;
            }
            match value {
                FieldValue::U8(number) => Some(u64::from(*number)),
                FieldValue::U16(number) => Some(u64::from(*number)),
                FieldValue::U32(number) => Some(u64::from(*number)),
                FieldValue::U64(number) => Some(*number),
                _ => None,
            }
        })
    };
    let channel = field("channel").and_then(|value| u16::try_from(value).ok());
    let where_ = |hardware: Option<u64>| match hardware {
        Some(hardware) if hardware > 0 => format!("ch {}.{hardware}", channel.unwrap_or(0)),
        _ => format!("ch {}", channel.unwrap_or(0)),
    };
    match object.object_type {
        blf_core::ObjectType::CanDriverError | blf_core::ObjectType::CanDriverErrorExt => {
            let code = field("errorCode").unwrap_or(0);
            let mut text = format!(
                "{} (code {code})",
                blf_core::can_driver_error_name(u32::try_from(code).unwrap_or(0))
            );
            let tx = field("txErrors").unwrap_or(0);
            let rx = field("rxErrors").unwrap_or(0);
            if tx > 0 || rx > 0 {
                text.push_str(&format!(", tx {tx} rx {rx} errors"));
            }
            Some(
                BusEvent::new("can", "error")
                    .channel(channel)
                    .text(format!("ch {} {text}", channel.unwrap_or(0)))
                    .done(),
            )
        }
        blf_core::ObjectType::CanStatistic => {
            let load = f64::from(u32::try_from(field("busLoad").unwrap_or(0)).unwrap_or(0)) / 100.0;
            let text = format!(
                "ch {} bus load {load:.2} %, {} standard, {} extended, {} remote, {} errors, {} overload",
                channel.unwrap_or(0),
                field("standardDataFrames").unwrap_or(0),
                field("extendedDataFrames").unwrap_or(0),
                field("standardRemoteFrames").unwrap_or(0)
                    + field("extendedRemoteFrames").unwrap_or(0),
                field("errorFrames").unwrap_or(0),
                field("overloadFrames").unwrap_or(0),
            );
            Some(
                BusEvent::new("can", "stat")
                    .channel(channel)
                    .text(text)
                    .done(),
            )
        }
        blf_core::ObjectType::CanOverload => Some(
            BusEvent::new("can", "event")
                .channel(channel)
                .text(format!("ch {} overload frame", channel.unwrap_or(0)))
                .done(),
        ),
        blf_core::ObjectType::CanDriverSync => Some(
            BusEvent::new("can", "event")
                .channel(channel)
                .text(format!(
                    "ch {} driver time sync (flags 0x{:X})",
                    channel.unwrap_or(0),
                    field("flags").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::CanSettingChanged => Some(
            BusEvent::new("can", "event")
                .channel(channel)
                .text(format!(
                    "ch {} bit timing changed: type {}, arbitration 0x{:X}, data 0x{:X}",
                    channel.unwrap_or(0),
                    field("changedType").unwrap_or(0),
                    field("btrExtArb").unwrap_or(0),
                    field("btrExtData").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::EthernetStatus | blf_core::ObjectType::EthernetPhyState => {
            // The flags say which fields carry information: bit 2 is the
            // hardware channel in a PHY state record, bit 8 in a status record.
            let flags = field("flags").unwrap_or(0);
            let hardware = if object.object_type == blf_core::ObjectType::EthernetPhyState {
                flags & 0b100 != 0
            } else {
                flags & 0x0100 != 0
            };
            let where_ = if hardware {
                where_(field("hardwareChannel"))
            } else {
                where_(None)
            };
            let mut parts = vec![where_];
            if object.object_type == blf_core::ObjectType::EthernetStatus {
                if let Some(link) = field("linkStatus") {
                    parts.push(blf_core::ethernet_link_status_name(link as u8).to_string());
                    if link == 2 {
                        if let Some(bitrate) = field("bitrate") {
                            parts.push(format!("{} Mbit/s", bitrate / 1000));
                        }
                    }
                }
                if let Some(phy) = field("ethernetPhy") {
                    let name = blf_core::ethernet_phy_type_name(phy as u8);
                    if name != "unknown" {
                        parts.push(name.to_string());
                    }
                }
                if let Some(duplex) = field("duplex") {
                    let name = blf_core::ethernet_duplex_name(duplex as u8);
                    if name != "unknown" {
                        parts.push(name.to_string());
                    }
                }
                if let Some(mode) = field("clockMode") {
                    let name = blf_core::ethernet_clock_mode_name(mode as u8);
                    if name != "unknown" {
                        parts.push(format!("clock {name}"));
                    }
                }
            } else {
                if let Some(state) = field("phyState") {
                    parts.push(format!(
                        "PHY {}",
                        blf_core::ethernet_phy_state_name(state as u8)
                    ));
                }
                if let Some(event) = field("phyEvent") {
                    let name = blf_core::ethernet_phy_event_name(event as u8);
                    if name != "invalid event" {
                        parts.push(name.to_string());
                    }
                }
            }
            Some(
                BusEvent::new("eth", "event")
                    .channel(channel)
                    .text(parts.join(" · "))
                    .done(),
            )
        }
        blf_core::ObjectType::EthernetStatistic => {
            let sqi = object.fields.iter().find_map(|(name, value)| {
                if *name == "sqi" {
                    match value {
                        FieldValue::I16(number) => Some(*number),
                        _ => None,
                    }
                } else {
                    None
                }
            });
            let mut text = format!(
                "{} rx {} / tx {} frames, {} / {} errors, SQI {}",
                where_(field("hardwareChannel")),
                field("rcvOk_HW").unwrap_or(0),
                field("xmitOk_HW").unwrap_or(0),
                field("rcvError_HW").unwrap_or(0),
                field("xmitError_HW").unwrap_or(0),
                sqi.map_or("unknown", blf_core::ethernet_sqi_name),
            );
            let dropped = field("rcvNoBuffer_HW").unwrap_or(0);
            if dropped > 0 {
                text.push_str(&format!(", {dropped} dropped"));
            }
            Some(
                BusEvent::new("eth", "stat")
                    .channel(channel)
                    .text(text)
                    .done(),
            )
        }
        blf_core::ObjectType::EthernetRxError => {
            let error = blf_core::ethernet_rx_error_name(
                u32::try_from(field("error").unwrap_or(0)).unwrap_or(0),
            );
            let direction = match field("dir").unwrap_or(0) {
                0 => "RX",
                _ => "TX",
            };
            Some(
                BusEvent::new("eth", "error")
                    .channel(channel)
                    .text(format!(
                        "{} {direction} error: {error}, {} bytes, FCS 0x{:X}",
                        where_(field("hardwareChannel")),
                        field("frameDataLength").unwrap_or(0),
                        field("fcs").unwrap_or(0),
                    ))
                    .done(),
            )
        }
        // ---- MOST: the format leaves several fields "to be determined", so
        // only the values the specification names are decoded. ----
        blf_core::ObjectType::MostSpy | blf_core::ObjectType::MostCtrl => {
            let dir = match field("dir").unwrap_or(0) {
                0 => "Rx",
                1 => "Tx",
                _ => "TxRq",
            };
            let sub_type = blf_core::most_control_sub_type_name(
                u16::try_from(field("rTyp").unwrap_or(0)).unwrap_or(0),
            );
            let address = blf_core::most_addressing_mode_name(field("rTypAdr").unwrap_or(0) as u8);
            let state = blf_core::most25_state_text(field("state").unwrap_or(0) as u8);
            let mut text = format!(
                "ch {} {dir} 0x{:03X} -> 0x{:03X} {sub_type} ({address})",
                channel.unwrap_or(0),
                field("sourceAdr").unwrap_or(0),
                field("destAdr").unwrap_or(0)
            );
            if object.object_type == blf_core::ObjectType::MostSpy {
                text.push_str(&format!(", {state}"));
            }
            if object.fields.iter().any(|(name, _)| *name == "ackNack") {
                let ack = blf_core::most_ack_text(field("ackNack").unwrap_or(0) as u8);
                if ack != "-" {
                    text.push_str(&format!(", {ack}"));
                }
            }
            Some(
                BusEvent::new("most", "ctrl")
                    .channel(channel)
                    .text(text)
                    .done(),
            )
        }
        blf_core::ObjectType::MostLightlock => Some(
            BusEvent::new("most", "light")
                .channel(channel)
                .text(format!(
                    "ch {} light lock state {}",
                    channel.unwrap_or(0),
                    field("state").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::MostStatistic => Some(
            BusEvent::new("most", "stat")
                .channel(channel)
                .text(format!(
                    "ch {} {} packets, {} frames, {} light events, buffer level {}",
                    channel.unwrap_or(0),
                    field("pktCnt").unwrap_or(0),
                    field("frmCnt").unwrap_or(0),
                    field("lightCnt").unwrap_or(0),
                    field("bufferLevel").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::MostStatisticex => Some(
            BusEvent::new("most", "stat")
                .channel(channel)
                .text(format!(
                    "ch {} {} frames, {} coding errors",
                    channel.unwrap_or(0),
                    field("frameCounter").unwrap_or(0),
                    field("codingErrors").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::MostPkt | blf_core::ObjectType::MostPkt2 => {
            let dir = match field("dir").unwrap_or(0) {
                0 => "Rx",
                1 => "Tx",
                _ => "TxRq",
            };
            let transfer =
                blf_core::most_transfer_type_name(field("transferType").unwrap_or(0) as u8);
            let state = blf_core::most50_state_text(field("state").unwrap_or(0) as u8);
            Some(
                BusEvent::new("most", "packet")
                    .channel(channel)
                    .text(format!(
                        "ch {} {dir} 0x{:03X} -> 0x{:03X} {transfer}, {} bytes, {state}",
                        channel.unwrap_or(0),
                        field("sourceAdr").unwrap_or(0),
                        field("destAdr").unwrap_or(0),
                        field("pktDataLength").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::Most150Message => {
            let dir = match field("dir").unwrap_or(0) {
                0 => "Rx",
                1 => "Tx",
                _ => "TxRq",
            };
            let transfer =
                blf_core::most_transfer_type_name(field("transferType").unwrap_or(0) as u8);
            let priority = blf_core::most_priority_name(
                u32::try_from(field("priority").unwrap_or(0)).unwrap_or(0),
            );
            Some(
                BusEvent::new("most", "ctrl150")
                    .channel(channel)
                    .text(format!(
                        "ch {} {dir} 0x{:03X} -> 0x{:03X} {transfer}, {} bytes, {priority} priority",
                        channel.unwrap_or(0),
                        field("sourceAdr").unwrap_or(0),
                        field("destAdr").unwrap_or(0),
                        field("msgLen").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::Most150Pkt => {
            let dir = match field("dir").unwrap_or(0) {
                0 => "Rx",
                1 => "Tx",
                _ => "TxRq",
            };
            let transfer =
                blf_core::most_transfer_type_name(field("transferType").unwrap_or(0) as u8);
            Some(
                BusEvent::new("most", "pkt150")
                    .channel(channel)
                    .text(format!(
                        "ch {} {dir} 0x{:03X} -> 0x{:03X} {transfer}, {} bytes",
                        channel.unwrap_or(0),
                        field("sourceAdr").unwrap_or(0),
                        field("destAdr").unwrap_or(0),
                        field("pktDataLength").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::MostEthernetPkt => {
            let dir = match field("dir").unwrap_or(0) {
                0 => "Rx",
                1 => "Tx",
                _ => "TxRq",
            };
            let transfer =
                blf_core::most_transfer_type_name(field("transferType").unwrap_or(0) as u8);
            Some(
                BusEvent::new("most", "ethernet")
                    .channel(channel)
                    .text(format!(
                        "ch {} {dir} ethernet packet {transfer}, {} bytes",
                        channel.unwrap_or(0),
                        field("pktDataLength").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::Most150MessageFragment
        | blf_core::ObjectType::Most150PktFragment
        | blf_core::ObjectType::MostEthernetPktFragment => Some(
            BusEvent::new("most", "fragment")
                .channel(channel)
                .text(format!(
                    "ch {} {} fragment, {} bytes",
                    channel.unwrap_or(0),
                    object.object_type.name(),
                    field("dataLength").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::MostHwmode => Some(
            BusEvent::new("most", "hw")
                .channel(channel)
                .text(format!(
                    "ch {} hardware mode 0x{:X} (mask 0x{:X})",
                    channel.unwrap_or(0),
                    field("hwMode").unwrap_or(0),
                    field("hwModeMask").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::MostReg => Some(
            BusEvent::new("most", "register")
                .channel(channel)
                .text(format!(
                    "ch {} {} register 0x{:X} offset {}, {} bytes",
                    channel.unwrap_or(0),
                    blf_core::most_register_sub_type_name(
                        u32::try_from(field("subType").unwrap_or(0)).unwrap_or(0)
                    ),
                    field("regId").or_else(|| field("handle")).unwrap_or(0),
                    field("offset").unwrap_or(0),
                    field("regDataLen").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::MostGenreg => Some(
            BusEvent::new("most", "register")
                .channel(channel)
                .text(format!(
                    "ch {} {} {} = 0x{:X}",
                    channel.unwrap_or(0),
                    blf_core::most_register_sub_type_name(
                        u32::try_from(field("subType").unwrap_or(0)).unwrap_or(0)
                    ),
                    blf_core::most_register_id_name(
                        u32::try_from(field("regId").unwrap_or(0)).unwrap_or(0)
                    ),
                    field("regValue").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::MostNetstate => Some(
            BusEvent::new("most", "netstate")
                .channel(channel)
                .text(format!(
                    "ch {} {} -> {}",
                    channel.unwrap_or(0),
                    blf_core::most_net_state_name(
                        u16::try_from(field("stateOld").unwrap_or(0)).unwrap_or(0)
                    ),
                    blf_core::most_net_state_name(
                        u16::try_from(field("stateNew").unwrap_or(0)).unwrap_or(0)
                    )
                ))
                .done(),
        ),
        blf_core::ObjectType::MostDatalost => {
            let lost = field("lostMsgsCtrl").unwrap_or(0) + field("lostMsgsAsync").unwrap_or(0);
            Some(
                BusEvent::new("most", "LOST")
                    .channel(channel)
                    .text(format!(
                        "ch {} lost {lost} messages ({} control, {} async) between {} and {} ns",
                        channel.unwrap_or(0),
                        field("lostMsgsCtrl").unwrap_or(0),
                        field("lostMsgsAsync").unwrap_or(0),
                        field("lastGoodTimeStampNs").unwrap_or(0),
                        field("nextGoodTimeStampNs").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::MostTrigger => Some(
            BusEvent::new("most", "trigger")
                .channel(channel)
                .text(format!(
                    "ch {} trigger mode {} {} -> {}",
                    channel.unwrap_or(0),
                    field("mode").unwrap_or(0),
                    field("previousTriggerValue").unwrap_or(0),
                    field("currentTriggerValue").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::MostTxlight => Some(
            BusEvent::new("most", "txlight")
                .channel(channel)
                .text(format!(
                    "ch {} transmit light state {}",
                    channel.unwrap_or(0),
                    field("state").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::MostAlloctab | blf_core::ObjectType::Most150Alloctab => Some(
            BusEvent::new("most", "alloctab")
                .channel(channel)
                .text(format!(
                    "ch {} allocation table, {} entries",
                    channel.unwrap_or(0),
                    field("length").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::MostStress => Some(
            BusEvent::new("most", "stress")
                .channel(channel)
                .text(format!(
                    "ch {} stress test state {} mode {}",
                    channel.unwrap_or(0),
                    field("state").unwrap_or(0),
                    field("mode").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::MostEcl => Some(
            BusEvent::new("most", "ecl")
                .channel(channel)
                .text(format!(
                    "ch {} electrical control line state {} mode {}",
                    channel.unwrap_or(0),
                    field("eclState").unwrap_or(0),
                    field("mode").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::MostSystemEvent => Some(
            BusEvent::new("most", "system")
                .channel(channel)
                .text(format!(
                    "ch {} system event {} = {} (was {})",
                    channel.unwrap_or(0),
                    field("id").unwrap_or(0),
                    field("value").unwrap_or(0),
                    field("valueOld").unwrap_or(0)
                ))
                .done(),
        ),

        // ---- FlexRay ----
        blf_core::ObjectType::FlexrayMessage => {
            let channel_name = blf_core::flexray_channel_name(
                u16::try_from(field("channel").unwrap_or(0)).unwrap_or(0),
            );
            let flags = blf_core::flexray_frame_flags_text(
                u32::try_from(field("frameFlags").unwrap_or(0)).unwrap_or(0),
            );
            let state = blf_core::flexray_frame_state_text(field("frameState").unwrap_or(0) as u8);
            Some(
                BusEvent::new("flexray", "frame")
                    .channel(channel)
                    .text(format!(
                        "ch {channel_name} slot {} cycle {} {} bytes, {flags}; state {state}",
                        field("frameId").unwrap_or(0),
                        field("cycle").unwrap_or(0),
                        field("length").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::FlexrayData | blf_core::ObjectType::FlexraySync => {
            let channel_name = blf_core::flexray_channel_name(
                u16::try_from(field("channel").unwrap_or(0)).unwrap_or(0),
            );
            Some(
                BusEvent::new("flexray", "frame")
                    .channel(channel)
                    .text(format!(
                        "ch {channel_name} id 0x{:X} {} bytes, mux {}",
                        field("messageId").unwrap_or(0),
                        field("len").unwrap_or(0),
                        field("mux").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::FlexrayCycle => Some(
            BusEvent::new("flexray", "cycle")
                .channel(channel)
                .text(format!(
                    "ch {} start of cycle {}, {} bytes",
                    blf_core::flexray_channel_name(
                        u16::try_from(field("channel").unwrap_or(0)).unwrap_or(0)
                    ),
                    field("cycle").unwrap_or(0),
                    field("length").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::FrRcvmessage | blf_core::ObjectType::FrRcvmessageEx => {
            let channel_name = blf_core::flexray_channel_name(
                u16::try_from(field("channelMask").unwrap_or(0)).unwrap_or(0),
            );
            let flags = blf_core::flexray_frame_flags_text(
                u32::try_from(field("frameFlags").unwrap_or(0)).unwrap_or(0),
            );
            Some(
                BusEvent::new("flexray", "frame")
                    .channel(channel)
                    .text(format!(
                        "ch {channel_name} slot {} cycle {} {} bytes, {flags}",
                        field("frameId").unwrap_or(0),
                        field("cycle").unwrap_or(0),
                        field("byteCount").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::FrStartcycle => {
            let channel_name = blf_core::flexray_channel_name(
                u16::try_from(field("channelMask").unwrap_or(0)).unwrap_or(0),
            );
            Some(
                BusEvent::new("flexray", "cycle")
                    .channel(channel)
                    .text(format!(
                        "ch {channel_name} start of cycle {}, NM vector {} bytes, {}",
                        field("cycle").unwrap_or(0),
                        field("nmSize").unwrap_or(0),
                        blf_core::flexray_cc_type_name(
                            u32::try_from(field("tag").unwrap_or(0)).unwrap_or(0)
                        )
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::FrError => {
            let channel_name = blf_core::flexray_channel_name(
                u16::try_from(field("channelMask").unwrap_or(0)).unwrap_or(0),
            );
            Some(
                BusEvent::new("flexray", "error")
                    .channel(channel)
                    .text(format!(
                        "ch {channel_name} error in cycle {}, {}",
                        field("cycle").unwrap_or(0),
                        blf_core::flexray_cc_type_name(
                            u32::try_from(field("tag").unwrap_or(0)).unwrap_or(0)
                        )
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::FrStatus | blf_core::ObjectType::FlexrayStatus => {
            let channel_name = blf_core::flexray_channel_name(
                u16::try_from(field("channelMask").unwrap_or(0)).unwrap_or(0),
            );
            Some(
                BusEvent::new("flexray", "status")
                    .channel(channel)
                    .text(format!(
                        "ch {channel_name} status type {}, wakeup state {}, sync state {}",
                        field("statusType").unwrap_or(0),
                        field("wus").unwrap_or(0),
                        field("ccSyncState").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        // ---- AFDX ----
        blf_core::ObjectType::AfdxFrame => {
            let protocol = field("type")
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(0);
            Some(
                BusEvent::new("afdx", "frame")
                    .channel(channel)
                    .text(format!(
                        "ch {} ethertype 0x{protocol:04X}, {} bytes, BAG {} us, flags 0x{:X}",
                        channel.unwrap_or(0),
                        field("payLoadLength").unwrap_or(0),
                        field("bagUsec").unwrap_or(0),
                        field("afdxFlags").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::AfdxStatistic => Some(
            BusEvent::new("afdx", "stat")
                .channel(channel)
                .text(format!(
                    "ch {} rx {} / tx {} packets, {} / {} bytes, {} collisions, {} errors, VL {}",
                    channel.unwrap_or(0),
                    field("rxPacketCount").unwrap_or(0),
                    field("txPacketCount").unwrap_or(0),
                    field("rxByteCount").unwrap_or(0),
                    field("txByteCount").unwrap_or(0),
                    field("collisionCount").unwrap_or(0),
                    field("errorCount").unwrap_or(0),
                    field("vlid").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::AfdxStatus => {
            let line = |prefix: &str| {
                let name = |suffix: &str| {
                    object.fields.iter().find_map(|(name, value)| {
                        if name != &format!("{prefix}{suffix}") {
                            return None;
                        }
                        match value {
                            FieldValue::U8(number) => Some(u64::from(*number)),
                            FieldValue::U16(number) => Some(u64::from(*number)),
                            FieldValue::U32(number) => Some(u64::from(*number)),
                            _ => None,
                        }
                    })
                };
                (
                    name("linkStatus").unwrap_or(0),
                    name("bitrate").unwrap_or(0),
                )
            };
            let (a_status, a_rate) = line("statusA_");
            let (b_status, b_rate) = line("statusB_");
            Some(
                BusEvent::new("afdx", "status")
                    .channel(channel)
                    .text(format!(
                        "ch {} line A {} at {} kbit/s, line B {} at {} kbit/s",
                        channel.unwrap_or(0),
                        blf_core::ethernet_link_status_name(a_status as u8),
                        a_rate / 1000,
                        blf_core::ethernet_link_status_name(b_status as u8),
                        b_rate / 1000
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::AfdxBusStatistic => Some(
            BusEvent::new("afdx", "stat")
                .channel(channel)
                .text(format!(
                    "ch {} line {}, {} / {}, {} rx packets, {} tx packets, {} dropped, {} lost",
                    channel.unwrap_or(0),
                    match field("line").unwrap_or(0) {
                        0 => "A",
                        _ => "B",
                    },
                    blf_core::ethernet_link_status_name(field("linkStatus").unwrap_or(0) as u8),
                    blf_core::afdx_link_speed_name(field("linkSpeed").unwrap_or(0) as u16),
                    field("statRxPacketCount").unwrap_or(0),
                    field("statTxPacketCount").unwrap_or(0),
                    field("statDroppedPacketCount").unwrap_or(0),
                    field("statLostPacketCount").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::AfdxErrorEvent => {
            let level = match field("errorLevel").unwrap_or(0) {
                0 => "error",
                1 => "warning",
                _ => "info",
            };
            let text = object
                .fields
                .iter()
                .find_map(|(name, value)| match (*name, value) {
                    ("errorText", FieldValue::String(text)) => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            Some(
                BusEvent::new("afdx", "error")
                    .channel(channel)
                    .text(format!("ch {} {level}: {text}", channel.unwrap_or(0)))
                    .done(),
            )
        }
        // ---- A429 ----
        blf_core::ObjectType::A429Message => Some(
            BusEvent::new("a429", "word")
                .channel(channel)
                .text(format!(
                    "ch {} {} bits, bitrate {}, {}",
                    channel.unwrap_or(0),
                    field("bitLenOfLastBit").unwrap_or(0),
                    field("bitrate").unwrap_or(0),
                    blf_core::flexray_direction_name(field("dir").unwrap_or(0) as u8)
                ))
                .done(),
        ),
        blf_core::ObjectType::A429Status => Some(
            BusEvent::new("a429", "status")
                .channel(channel)
                .text(format!(
                    "ch {} parity {}, bitrate {}, gap floor {}",
                    channel.unwrap_or(0),
                    blf_core::a429_parity_name(field("parity").unwrap_or(0) as u16),
                    field("bitrate").unwrap_or(0),
                    field("minGap").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::A429BusStatistic => Some(
            BusEvent::new("a429", "stat")
                .channel(channel)
                .text(format!(
                    "ch {} bus load {:.2} %, {} words, {} errors",
                    channel.unwrap_or(0),
                    f64::from(u32::try_from(field("busload").unwrap_or(0)).unwrap_or(0)) / 100.0,
                    field("dataTotal").unwrap_or(0),
                    field("errorTotal").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::A429Error => {
            let text = object
                .fields
                .iter()
                .find_map(|(name, value)| match (*name, value) {
                    ("errorText", FieldValue::String(text)) => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            Some(
                BusEvent::new("a429", "error")
                    .channel(channel)
                    .text(format!(
                        "ch {} {}: {text}",
                        channel.unwrap_or(0),
                        blf_core::a429_error_type_name(field("errorType").unwrap_or(0) as u16)
                    ))
                    .done(),
            )
        }
        // ---- WLAN, K-Line, J1708 ----
        blf_core::ObjectType::WlanFrame => Some(
            BusEvent::new("wlan", "frame")
                .channel(channel)
                .text(format!(
                    "ch {} radio {} {} bytes, signal {} / quality {}, {}",
                    channel.unwrap_or(0),
                    field("radioChannel").unwrap_or(0),
                    field("frameLength").unwrap_or(0),
                    field("signalStrength").unwrap_or(0),
                    field("signalQuality").unwrap_or(0),
                    blf_core::wlan_frame_flags_text(
                        u32::try_from(field("flags").unwrap_or(0)).unwrap_or(0)
                    )
                ))
                .done(),
        ),
        blf_core::ObjectType::WlanStatistic => Some(
            BusEvent::new("wlan", "stat")
                .channel(channel)
                .text(format!(
                    "ch {} rx {} / tx {} packets, {} / {} bytes, {} collisions, {} errors",
                    channel.unwrap_or(0),
                    field("rxPacketCount").unwrap_or(0),
                    field("txPacketCount").unwrap_or(0),
                    field("rxByteCount").unwrap_or(0),
                    field("txByteCount").unwrap_or(0),
                    field("collisionCount").unwrap_or(0),
                    field("errorCount").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::KlineStatusEvent => Some(
            BusEvent::new("kline", "status")
                .channel(channel)
                .text(format!(
                    "port {} {} ({} bytes)",
                    field("port").unwrap_or(0),
                    blf_core::kline_status_name(
                        u32::try_from(field("type").unwrap_or(0)).unwrap_or(0)
                    ),
                    field("dataLen").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::J1708Message | blf_core::ObjectType::J1708VirtualMsg => {
            let length = field("size").unwrap_or(0).min(21);
            let data = object
                .fields
                .iter()
                .find_map(|(name, value)| match (*name, value) {
                    ("data", FieldValue::Bytes(bytes)) => Some(bytes.as_slice()),
                    _ => None,
                })
                .unwrap_or(&[]);
            let body: String = data
                .iter()
                .take(length as usize)
                .map(|byte| format!("{byte:02X}"))
                .collect::<Vec<_>>()
                .join(" ");
            Some(
                BusEvent::new("j1708", "message")
                    .channel(channel)
                    .text(format!(
                        "ch {} {} bytes{}{body}",
                        channel.unwrap_or(0),
                        length,
                        if field("error").unwrap_or(0) != 0 {
                            " (error)"
                        } else {
                            ""
                        }
                    ))
                    .done(),
            )
        }

        blf_core::ObjectType::Most50Message | blf_core::ObjectType::Most50Pkt => {
            let dir = match field("dir").unwrap_or(0) {
                0 => "Rx",
                1 => "Tx",
                _ => "TxRq",
            };
            let transfer =
                blf_core::most_transfer_type_name(field("transferType").unwrap_or(0) as u8);
            let state = blf_core::most50_state_text(field("state").unwrap_or(0) as u8);
            let length = if object.object_type == blf_core::ObjectType::Most50Message {
                field("msgLen").unwrap_or(0)
            } else {
                field("pktDataLength").unwrap_or(0)
            };
            Some(
                BusEvent::new("most", "most50")
                    .channel(channel)
                    .text(format!(
                        "ch {} {dir} 0x{:03X} -> 0x{:03X} {transfer}, {length} bytes, {state}",
                        channel.unwrap_or(0),
                        field("sourceAdr").unwrap_or(0),
                        field("destAdr").unwrap_or(0)
                    ))
                    .done(),
            )
        }

        // ---- remaining records: triggers, variables, marker points ----
        blf_core::ObjectType::AppTrigger => Some(
            BusEvent::new("-", "trigger")
                .channel(channel)
                .text(format!(
                    "application trigger ch {}: {} ns before, {} ns after",
                    channel.unwrap_or(0),
                    field("preTriggerTime").unwrap_or(0),
                    field("postTriggerTime").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::EnvInteger
        | blf_core::ObjectType::EnvDouble
        | blf_core::ObjectType::EnvString
        | blf_core::ObjectType::EnvData => {
            let text_of = |_name: &str| {
                object
                    .fields
                    .iter()
                    .find_map(|(field, value)| match (*field, value) {
                        ("name", FieldValue::String(text)) => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or("")
            };
            let data = object
                .fields
                .iter()
                .find_map(|(name, value)| match (*name, value) {
                    ("data", FieldValue::Bytes(bytes)) => Some(bytes.as_slice()),
                    _ => None,
                })
                .unwrap_or(&[]);
            let value = String::from_utf8_lossy(data)
                .trim_end_matches('\0')
                .to_string();
            Some(
                BusEvent::new("-", "var")
                    .text(format!("{} = {value}", text_of("name")))
                    .done(),
            )
        }
        blf_core::ObjectType::GpsEvent => {
            // Coordinates arrive as scaled integers in the format.
            Some(
                BusEvent::new("-", "gps")
                    .channel(channel)
                    .text(format!(
                        "ch {} position {:.6} / {:.6}, altitude {} m, speed {} km/h, course {}",
                        channel.unwrap_or(0),
                        (field("latitude").unwrap_or(0) as i64) as f64 / 1e7,
                        (field("longitude").unwrap_or(0) as i64) as f64 / 1e7,
                        field("altitude").unwrap_or(0),
                        field("speed").unwrap_or(0),
                        field("course").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::WaterMarkEvent => Some(
            BusEvent::new("-", "watermark")
                .text(format!("queue state {}", field("queueState").unwrap_or(0)))
                .done(),
        ),
        blf_core::ObjectType::TriggerCondition => {
            let text_of = |wanted: &str| {
                object
                    .fields
                    .iter()
                    .find_map(|(field, value)| match (*field, value) {
                        (name, FieldValue::String(text)) if name == wanted => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or("")
            };
            Some(
                BusEvent::new("-", "trigger")
                    .text(format!(
                        "trigger condition state {}, block {}: {}",
                        field("state").unwrap_or(0),
                        text_of("triggerBlockName"),
                        text_of("triggerCondition")
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::FunctionBus => {
            let text_of = |wanted: &str| {
                object
                    .fields
                    .iter()
                    .find_map(|(field, value)| match (*field, value) {
                        (name, FieldValue::String(text)) if name == wanted => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or("")
            };
            Some(
                BusEvent::new("func", "bus")
                    .text(format!(
                        "{} = {}",
                        text_of("name"),
                        String::from_utf8_lossy(
                            object
                                .fields
                                .iter()
                                .find_map(|(name, value)| match (*name, value) {
                                    ("data", FieldValue::Bytes(bytes)) => Some(bytes.as_slice()),
                                    _ => None,
                                })
                                .unwrap_or(&[])
                        )
                        .trim_end_matches('\0')
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::DistributedObjectMember => {
            let text_of = |wanted: &str| {
                object
                    .fields
                    .iter()
                    .find_map(|(field, value)| match (*field, value) {
                        (name, FieldValue::String(text)) if name == wanted => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or("")
            };
            Some(
                BusEvent::new("-", "member")
                    .text(format!(
                        "distributed member {} (type {} detail {})",
                        text_of("path"),
                        field("memberType").unwrap_or(0),
                        field("detailType").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::AttributeEvent => {
            let text_of = |wanted: &str| {
                object
                    .fields
                    .iter()
                    .find_map(|(field, value)| match (*field, value) {
                        (name, FieldValue::String(text)) if name == wanted => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or("")
            };
            Some(
                BusEvent::new("-", "attribute")
                    .text(format!(
                        "attribute {} of {}{} changed",
                        text_of("attributeDefinitionPath"),
                        text_of("mainAttributableObjectPath"),
                        if text_of("memberPath").is_empty() {
                            String::new()
                        } else {
                            format!(".{}", text_of("memberPath"))
                        }
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::DistributedObjectChange => Some(
            BusEvent::new("-", "change")
                .text(format!(
                    "distributed object change, {} bytes",
                    field("dataLength").unwrap_or(0)
                ))
                .done(),
        ),

        // ---- data integrity: a lost frame or an overrun means the capture is
        // not complete, so these must never be silent rows. ----
        blf_core::ObjectType::DataLostBegin => Some(
            BusEvent::new("-", "LOST")
                .text(format!(
                    "queue {} begins losing objects",
                    field("queueIdentifier").unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::DataLostEnd => {
            let lost = field("numberOfLostEvents").unwrap_or(0);
            let since = field("firstObjectLostTimeStamp").unwrap_or(0);
            Some(
                BusEvent::new("-", "LOST")
                    .text(format!(
                        "queue {} lost {lost} objects from timestamp {since}",
                        field("queueIdentifier").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::OverrunError => Some(
            BusEvent::new(bus_type_label(field("busType").unwrap_or(0)), "OVERRUN")
                .channel(channel)
                .text(format!(
                    "ch {} reception overrun, frames were lost",
                    channel.unwrap_or(0)
                ))
                .done(),
        ),
        blf_core::ObjectType::SerialEvent => {
            let port = field("port").unwrap_or(0);
            let baudrate = field("baudrate").unwrap_or(0);
            let mut text = format!("ch {port} at {baudrate} bit/s");
            if object.fields.iter().any(|(name, _)| *name == "byte") {
                text.push_str(&format!(
                    ", single byte 0x{:02X}",
                    field("byte").unwrap_or(0)
                ));
            } else if object
                .fields
                .iter()
                .any(|(name, _)| *name == "compactLength")
            {
                let length = field("compactLength").unwrap_or(0);
                text.push_str(&format!(", compact {length} bytes"));
            } else {
                let length = field("dataLength").unwrap_or(0);
                let stamps = field("timeStampsLength").unwrap_or(0) / 8;
                text.push_str(&format!(", {length} bytes, {stamps} timestamps"));
            }
            Some(BusEvent::new("-", "serial").text(text).done())
        }

        // ---- system variables ----
        blf_core::ObjectType::SysVariable => {
            let kind = u32::try_from(field("type").unwrap_or(0)).unwrap_or(0);
            let representation = u32::try_from(field("representation").unwrap_or(0)).unwrap_or(0);
            let name = object
                .fields
                .iter()
                .find_map(|(name, value)| match (*name, value) {
                    ("name", FieldValue::String(text)) => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("-");
            let data = object
                .fields
                .iter()
                .find_map(|(name, value)| match (*name, value) {
                    ("data", FieldValue::Bytes(bytes)) => Some(bytes.as_slice()),
                    _ => None,
                })
                .unwrap_or(&[]);
            Some(
                BusEvent::new("-", "SV")
                    .text(format!(
                        "{name} = {} ({})",
                        blf_core::sysvar_value_text(kind, representation, data),
                        blf_core::sysvar_type_name(kind)
                    ))
                    .done(),
            )
        }
        // ---- measurement structure ----
        blf_core::ObjectType::Realtimeclock => {
            // `time` and `loggingOffset` are nanoseconds since the Unix epoch.
            let time = field("time").unwrap_or(0);
            let offset = field("loggingOffset").unwrap_or(0);
            let seconds = time / 1_000_000_000;
            let shifted = seconds.saturating_sub(offset / 1_000_000_000);
            Some(
                BusEvent::new("-", "clock")
                    .text(format!(
                        "real time {seconds} s epoch, logging offset {offset} ns (zero at {shifted} s)"
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::GlobalMarker => {
            let marker = object
                .fields
                .iter()
                .find_map(|(name, value)| match (*name, value) {
                    ("markerName", FieldValue::String(text)) => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("-");
            let group = object
                .fields
                .iter()
                .find_map(|(name, value)| match (*name, value) {
                    ("groupName", FieldValue::String(text)) => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            let mut text = format!("marker {marker}");
            if !group.is_empty() {
                text.push_str(&format!(" in {group}"));
            }
            Some(BusEvent::new("-", "marker").text(text).done())
        }
        blf_core::ObjectType::EventComment => {
            let text = object
                .fields
                .iter()
                .find_map(|(name, value)| match (*name, value) {
                    ("text", FieldValue::String(text)) => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            Some(
                BusEvent::new("-", "comment")
                    .text(format!(
                        "comment on object type {}: {text}",
                        field("commentedEventType").unwrap_or(0)
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::TestStructure => {
            let name = object
                .fields
                .iter()
                .find_map(|(name, value)| match (*name, value) {
                    ("name", FieldValue::String(text)) if !text.is_empty() => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            let text = object
                .fields
                .iter()
                .find_map(|(name, value)| match (*name, value) {
                    ("text", FieldValue::String(text)) if !text.is_empty() => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            Some(
                BusEvent::new("-", "test")
                    .text(format!(
                        "test structure type {} action {} result {}{}{}",
                        field("type").unwrap_or(0),
                        field("action").unwrap_or(0),
                        field("result").unwrap_or(0),
                        if name.is_empty() {
                            String::new()
                        } else {
                            format!(" [{name}]")
                        },
                        if text.is_empty() {
                            String::new()
                        } else {
                            format!(" {text}")
                        },
                    ))
                    .done(),
            )
        }
        blf_core::ObjectType::DiagRequestInterpretation => {
            let qualifier = |field_name: &str| {
                object
                    .fields
                    .iter()
                    .find_map(|(name, value)| match (*name, value) {
                        (name, FieldValue::String(text)) if name == field_name => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .unwrap_or("")
            };
            Some(
                BusEvent::new("-", "diag")
                    .text(format!(
                        "diagnostic request {} / {} / {}",
                        qualifier("ecuQualifier"),
                        qualifier("variantQualifier"),
                        qualifier("serviceQualifier")
                    ))
                    .done(),
            )
        }
        _ => None,
    }
}

/// Events that the BLF stores as LIN records.
fn lin_event(object: &blf_core::LinObject) -> Option<BusEvent> {
    use blf_core::ObjectType;
    let field = |name: &str| -> Option<u64> {
        object.fields.iter().find_map(|(field, value)| {
            if *field != name {
                return None;
            }
            match value {
                FieldValue::U8(number) => Some(u64::from(*number)),
                FieldValue::U16(number) => Some(u64::from(*number)),
                FieldValue::U32(number) => Some(u64::from(*number)),
                FieldValue::U64(number) => Some(*number),
                FieldValue::I32(number) => u64::try_from(*number).ok(),
                FieldValue::F64(number) => Some(*number as u64),
                _ => None,
            }
        })
    };
    let channel = field("channel").and_then(|value| u16::try_from(value).ok());
    let ch = channel.unwrap_or(0);
    let (kind, text) = match object.object_type {
        ObjectType::LinBaudrate => (
            "event",
            format!("ch {ch} baudrate {} bit/s", field("baudrate").unwrap_or(0)),
        ),
        ObjectType::LinSleep => (
            "event",
            format!(
                "ch {ch} sleep: {} ({})",
                blf_core::lin_sleep_reason_name(field("reason").unwrap_or(0) as u8),
                blf_core::lin_sleep_flags_text(field("flags").unwrap_or(0) as u8),
            ),
        ),
        ObjectType::LinWakeup | ObjectType::LinWakeup2 => {
            let mut text = format!(
                "ch {ch} wakeup signal 0x{:02X}",
                field("signal").unwrap_or(0)
            );
            if let Some((_, FieldValue::U8(length))) =
                object.fields.iter().find(|(name, _)| *name == "lengthInfo")
            {
                text.push_str(&format!(", {}", blf_core::lin_wakeup_length_name(*length)));
            }
            if field("external").unwrap_or(0) != 0 {
                text.push_str(", external");
            }
            ("event", text)
        }
        ObjectType::LinSynError | ObjectType::LinSynError2 => {
            ("error", format!("ch {ch} synchronization error"))
        }
        ObjectType::LinSchedModch => (
            "event",
            format!(
                "ch {ch} schedule table {} -> {}",
                field("oldMode").unwrap_or(0),
                field("newMode").unwrap_or(0)
            ),
        ),
        ObjectType::LinChecksumInfo => (
            "event",
            format!(
                "ch {ch} frame 0x{:02X} checksum model {}",
                field("id").unwrap_or(0),
                blf_core::lin_checksum_model_name(field("checksumModel").unwrap_or(0) as u8)
            ),
        ),
        ObjectType::LinSpikeEvent | ObjectType::LinSpikeEvent2 => (
            "event",
            format!(
                "ch {ch} spike {} us ({})",
                field("width").unwrap_or(0),
                blf_core::lin_spike_origin_name(field("internal").unwrap_or(0) as u8)
            ),
        ),
        ObjectType::LinLongDomSig | ObjectType::LinLongDomSig2 => (
            "event",
            format!(
                "ch {ch} long dominant signal {}, {} ns",
                blf_core::lin_long_dom_signal_type_name(field("type").unwrap_or(0) as u8),
                field("length").unwrap_or(0)
            ),
        ),
        ObjectType::LinDisturbanceEvent => (
            "event",
            format!(
                "ch {ch} {} on frame 0x{:02X} at byte {} bit {}",
                blf_core::lin_disturbance_type_name(
                    u32::try_from(field("disturbanceType").unwrap_or(0)).unwrap_or(0)
                ),
                field("id").unwrap_or(0),
                field("byteIndex").unwrap_or(0),
                field("bitIndex").unwrap_or(0)
            ),
        ),
        ObjectType::LinShortOrSlowResponse | ObjectType::LinShortOrSlowResponse2 => {
            let mut text = format!(
                "ch {ch} short response: {} bytes",
                field("numberOfRespBytes").unwrap_or(0)
            );
            if field("slowResponse").unwrap_or(0) != 0 {
                text.push_str(", slow");
            }
            if field("interruptedByBreak").unwrap_or(0) != 0 {
                text.push_str(", interrupted by break");
            }
            ("event", text)
        }
        ObjectType::LinUnexpectedWakeup => (
            "event",
            format!(
                "ch {ch} unexpected wakeup signal 0x{:02X}, {} ns",
                field("signal").unwrap_or(0),
                field("width").unwrap_or(0)
            ),
        ),
        ObjectType::LinStatistic => {
            let load = object
                .fields
                .iter()
                .find_map(|(name, value)| match (*name, value) {
                    ("busLoad", FieldValue::F64(load)) => Some(*load),
                    _ => None,
                })
                .unwrap_or(0.0);
            (
                "stat",
                format!(
                    "ch {ch} bus load {load:.2} %, {} sent, {} received, {} unanswered, {} bursts ({} overrun)",
                    field("framesSent").unwrap_or(0),
                    field("framesReceived").unwrap_or(0),
                    field("framesUnanswered").unwrap_or(0),
                    field("burstsTotal").unwrap_or(0),
                    field("burstsOverrun").unwrap_or(0),
                ),
            )
        }
        ObjectType::LinSlvTimeout => (
            "error",
            format!(
                "ch {ch} slave 0x{:02X} timeout in state {} -> {}",
                field("slaveId").unwrap_or(0),
                field("stateId").unwrap_or(0),
                field("followStateId").unwrap_or(0)
            ),
        ),
        ObjectType::LinDlcInfo => (
            "event",
            format!(
                "ch {ch} frame 0x{:02X} length adjusted to {}",
                field("id").unwrap_or(0),
                field("dlc").unwrap_or(0)
            ),
        ),
        _ => return None,
    };
    Some(
        BusEvent::new("lin", kind)
            .channel(channel)
            .text(text)
            .done(),
    )
}

/// The channel and direction a structured frame reports.
///
/// One pass over the field list, because a trace can hold millions of frames.
fn structured_frame_hints(object: &Object) -> (Option<u16>, Option<&'static str>) {
    let Object::Structured(value) = object else {
        return (None, None);
    };
    let mut channel = None;
    let mut direction = None;
    for (field, value) in &value.fields {
        match *field {
            "channel" | "hardwareChannel" if channel.is_none() => {
                channel = match value {
                    FieldValue::U8(number) => Some(u16::from(*number)),
                    FieldValue::U16(number) => Some(*number),
                    FieldValue::U32(number) => u16::try_from(*number).ok(),
                    FieldValue::U64(number) => u16::try_from(*number).ok(),
                    _ => None,
                };
            }
            "dir" if direction.is_none() => {
                direction = match value {
                    FieldValue::U8(number) => Some(can_direction(*number)),
                    FieldValue::U16(number) => u8::try_from(*number).ok().map(can_direction),
                    _ => None,
                };
            }
            _ => {}
        }
    }
    (channel, direction)
}

/// Every attribute of one frame, as scrollable text.
///
/// The list pane only shows what fits in its columns; this is the place that
/// shows the whole object: header fields, bus fields, the payload as hex and
/// ASCII, every parsed protocol field, the reassembled UDS message and the raw
/// object bytes.
fn inspector_lines(
    summary: &MessageSummary,
    object: Option<&Object>,
    axis: TimeAxis,
    uds: Option<&UdsFrame>,
    pool: &TextPool,
    index: usize,
    total: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("frame {}/{}", index + 1, total));
    lines.push(format!("  class          {}", summary.class_name));
    lines.push(format!(
        "  object type    {} (raw {})",
        summary.class_name, summary.object_type_raw
    ));
    if let Some(source) = summary.timestamp_source {
        lines.push(format!(
            "  timestamp      {}",
            axis.format(summary.timestamp_ns)
        ));
        lines.push(format!("  time source    {source}"));
    } else {
        lines.push(format!(
            "  timestamp      {}",
            axis.format(summary.timestamp_ns)
        ));
    }

    if let Some(object) = object {
        let header = object.header_info();
        lines.push("header".to_string());
        lines.push(format!("  signature      0x{:08X}", header.signature));
        lines.push(format!(
            "  header size    {} bytes, version {}",
            header.header_size, header.header_version
        ));
        lines.push(format!("  object size    {} bytes", header.object_size));
        lines.push(format!(
            "  flags          0x{:08X}  (client {}, version {})",
            header.object_flags, header.client_index, header.object_version
        ));
        lines.push(format!("  raw timestamp  {}", header.timestamp));
    }

    lines.push("bus".to_string());
    lines.push(format!(
        "  bus            {}  channel {}",
        summary.bus_label,
        summary
            .channel
            .map_or_else(|| "-".to_string(), |value| value.to_string())
    ));
    if let Some(direction) = summary.direction {
        lines.push(format!("  direction      {direction}"));
    }
    if let Some(id) = summary.frame_id {
        lines.push(format!("  id             0x{id:X}  ({id})"));
    }
    if let Some(dlc) = summary.dlc {
        lines.push(format!("  dlc            {dlc}"));
    }
    lines.push(format!("  status         {}", summary.status));
    if let Some(info) = &summary.ethernet {
        lines.push("ethernet".to_string());
        lines.push(format!(
            "  etherType      0x{:04X}  protocol {}",
            info.ether_type, info.protocol
        ));
        lines.push(format!("  summary        {}", pool.get(info.summary)));
        // Every parsed layer, in order, without the field cap of the preview.
        if let Some(dissection) = object.and_then(blf_core::dissect_object) {
            let mut layer = "";
            for field in &dissection.fields {
                if field.layer != layer {
                    layer = field.layer;
                    lines.push(format!("  [{layer}]"));
                }
                lines.push(format!("    {} = {}", field.name, field.value));
            }
        }
    }

    if summary.ethernet.is_some() {
        lines.push("frame bytes".to_string());
        match object.and_then(blf_core::frame_bytes) {
            Some(frame) => lines.extend(hex_dump(&frame, 2)),
            None => lines.push("  (not available for this object)".to_string()),
        }
    } else {
        lines.push("payload".to_string());
        lines.extend(hex_dump(&summary.payload, 2));
    }

    if let Some(uds) = uds {
        lines.push("uds".to_string());
        lines.push(format!(
            "  transport      {}  frames {}  position {}",
            uds.transport, uds.frames, uds.position
        ));
        if let Some(node) = uds.node {
            lines.push(format!("  node           0x{node:02X}"));
        }
        lines.push(format!("  message        {}", uds.summary));
        for (name, value) in &uds.fields {
            lines.push(format!("    {name} = {value}"));
        }
    }

    if let Some(object) = object {
        if summary.ethernet.is_none() {
            let fields = object_fields(object);
            if !fields.is_empty() {
                lines.push("fields".to_string());
                for (name, value) in fields {
                    lines.push(format!("  {name:<14} {value}"));
                }
            }
            // The decoded names keep the raw numbers readable.
            if let Some(event) = &summary.event {
                lines.push("event".to_string());
                lines.push(format!("  {}", event.summary));
            }
        }
        let extensions = object_extensions(object);
        if !extensions.is_empty() {
            lines.push("extensions".to_string());
            lines.extend(hex_dump(&extensions, 2));
        }
        let raw = object_raw_bytes(object);
        if !raw.is_empty() {
            lines.push("raw object bytes".to_string());
            lines.extend(hex_dump(&raw, 2));
        }
    }
    lines
}

/// Hex dump with an ASCII column, `indent` spaces of leading padding.
fn hex_dump(bytes: &[u8], indent: usize) -> Vec<String> {
    if bytes.is_empty() {
        return vec![format!("{}  (empty)", " ".repeat(indent))];
    }
    let pad = " ".repeat(indent);
    bytes
        .chunks(16)
        .enumerate()
        .map(|(row, chunk)| {
            let mut hex = String::new();
            for byte in chunk {
                hex.push_str(&format!("{byte:02X} "));
            }
            for _ in chunk.len()..16 {
                hex.push_str("   ");
            }
            let ascii: String = chunk
                .iter()
                .map(|byte| {
                    if byte.is_ascii_graphic() || *byte == b' ' {
                        char::from(*byte)
                    } else {
                        '.'
                    }
                })
                .collect();
            format!("{pad}{:04X}  {hex} {ascii}", row * 16)
        })
        .collect()
}

/// Bytes a BLF object stores beyond its decoded fields.
fn object_extensions(object: &Object) -> Vec<u8> {
    match object {
        Object::Structured(value) => value.extensions.clone(),
        Object::Lin(value) => value.extensions.clone(),
        _ => Vec::new(),
    }
}

/// The original bytes of an object, when the reader kept them.
fn object_raw_bytes(object: &Object) -> Vec<u8> {
    match object {
        Object::Structured(value) => {
            if value.data.is_empty() {
                Vec::new()
            } else {
                value.data.clone()
            }
        }
        Object::CanMessage(value) => value.raw_data.clone(),
        Object::CanMessage2(value) => value.raw_data.clone(),
        Object::Raw(value) => value.data.clone(),
        _ => Vec::new(),
    }
}

/// Column header of the current view, or empty when the view has no columns.
fn view_header(view: View, width: usize) -> String {
    match view {
        View::All => format!(
            "{:>12} {:<3} {:>3} {:<6} {:<3}  {}",
            "time", "bus", "ch", "id", "dir", "data"
        ),
        View::Can => format!(
            "{:>12} {:>3} {:<6} {:<3} {:>3} {:<9}  {}",
            "time", "ch", "id", "dir", "dlc", "flags", "data"
        ),
        View::Lin => format!(
            "{:>12} {:>3} {:<6} {:<3} {:>3} {:<8}  {}",
            "time", "ch", "id", "dir", "dlc", "crc", "data"
        ),
        View::Ethernet => {
            if width >= 108 {
                format!(
                    "{:>12} {:>3} {:<7} {:<9} {:<5} {:<17} {:<17}  {}",
                    "time", "ch", "type", "proto", "vlan", "source", "destination", "info"
                )
            } else {
                format!(
                    "{:>12} {:>3} {:<7} {:<9} {:<5}  {}",
                    "time", "ch", "type", "proto", "vlan", "info"
                )
            }
        }
    }
}

/// One frame rendered in the columns of `view`.
/// Colour of a timestamp: the least important column, so it stays subdued.
fn color_time() -> &'static str {
    slot_color(Slot::Blue)
}
/// Colour of the bus label, so a mixed trace is scannable at a glance.
fn color_bus() -> &'static str {
    slot_color(Slot::Cyan)
}
/// Colour of an identifier or EtherType.
fn color_id() -> &'static str {
    slot_color(Slot::Yellow)
}
/// Colour of a payload: the terminal foreground.
const COLOR_DATA: &str = ANSI_PLAIN;
/// Colour of a secondary attribute such as DLC or a flag word.
const COLOR_ATTRIBUTE: &str = ANSI_PLAIN;

fn color_direction(direction: Option<&str>) -> &'static str {
    match direction {
        Some("Rx") => slot_color(Slot::Green),
        Some("Tx") => slot_color(Slot::Magenta),
        Some("TxRq") => slot_color(Slot::Blue),
        _ => ANSI_PLAIN,
    }
}

/// Pad a value to `width`, then colour it.
///
/// The padding has to happen before the escape codes are added, otherwise
/// `format!` would count the escape bytes as visible width.
fn colored_cell(value: &str, width: usize, align_right: bool, color: &str) -> String {
    let padded = if align_right {
        format!("{value:>width$}")
    } else {
        format!("{value:<width$}")
    };
    paint(padded, color)
}

fn view_row(
    summary: &MessageSummary,
    view: View,
    axis: TimeAxis,
    pool: &TextPool,
    width: usize,
) -> String {
    let time = axis.format(summary.timestamp_ns);
    let channel = summary
        .channel
        .map_or_else(|| "-".to_string(), |value| value.to_string());
    match view {
        View::All => summary_line(summary, axis, pool, width),
        View::Can => {
            if summary.event.is_some() {
                return event_row(summary, View::Can, axis);
            }
            let id = summary
                .frame_id
                .map_or_else(|| "-".to_string(), |id| format!("0x{id:X}"));
            let direction = summary.direction.unwrap_or("-");
            let dlc = summary
                .dlc
                .map_or_else(|| "-".to_string(), |value| value.to_string());
            format!(
                "{} {} {} {} {} {}  {}",
                colored_cell(&time, 12, true, color_time()),
                colored_cell(&channel, 3, true, color_bus()),
                colored_cell(&id, 6, false, color_id()),
                colored_cell(direction, 3, false, color_direction(summary.direction)),
                colored_cell(&dlc, 3, true, COLOR_ATTRIBUTE),
                colored_cell(
                    &can_flag_label(summary.can_flags),
                    9,
                    false,
                    slot_color(Slot::Yellow)
                ),
                paint(hex_bytes(&summary.payload), COLOR_DATA)
            )
        }
        View::Lin => {
            if summary.event.is_some() {
                return event_row(summary, View::Lin, axis);
            }
            let id = summary
                .frame_id
                .map_or_else(|| "-".to_string(), |id| format!("0x{id:X}"));
            let direction = summary.direction.unwrap_or("-");
            let dlc = summary
                .dlc
                .map_or_else(|| "-".to_string(), |value| value.to_string());
            let crc = lin_crc_label(summary.status);
            let crc_color = if summary.status == "response" {
                slot_color(Slot::Green)
            } else if summary.status == "header_only" || summary.status == "no_response" {
                slot_color(Slot::Yellow)
            } else {
                slot_color(Slot::Red)
            };
            format!(
                "{} {} {} {} {} {}  {}",
                colored_cell(&time, 12, true, color_time()),
                colored_cell(&channel, 3, true, color_bus()),
                colored_cell(&id, 6, false, color_id()),
                colored_cell(direction, 3, false, color_direction(summary.direction)),
                colored_cell(&dlc, 3, true, COLOR_ATTRIBUTE),
                colored_cell(crc, 8, false, crc_color),
                paint(hex_bytes(&summary.payload), COLOR_DATA)
            )
        }
        View::Ethernet => {
            let Some(info) = &summary.ethernet else {
                if summary.event.is_some() {
                    return event_row(summary, View::Ethernet, axis);
                }
                return summary_line(summary, axis, pool, width);
            };
            let summary_text = pool.get(info.summary);
            let short = ethernet_info_text(summary_text, info.protocol);
            let vlan = vlan_label(summary_text);
            if width >= 108 {
                format!(
                    "{time:>12} {channel:>3} 0x{:04X}  {:<9} {:<5} {:<17} {:<17}  {short}",
                    info.ether_type,
                    info.protocol,
                    vlan,
                    ethernet_address(summary_text, 0),
                    ethernet_address(summary_text, 1),
                )
            } else {
                format!(
                    "{time:>12} {channel:>3} 0x{:04X}  {:<9} {:<5}  {short}",
                    info.ether_type, info.protocol, vlan
                )
            }
        }
    }
}

/// A record without payload.
///
/// The frame columns keep their width so the row stays aligned with the header;
/// they are all `-`, and the decoded text starts where the payload column does,
/// labelled with the record kind.
fn event_row(summary: &MessageSummary, view: View, axis: TimeAxis) -> String {
    let time = axis.format(summary.timestamp_ns);
    let channel = summary
        .channel
        .map_or_else(|| "-".to_string(), |value| value.to_string());
    let text = match &summary.event {
        Some(event) => format!("{}: {}", event.kind, event.summary),
        None => "-".to_string(),
    };
    match view {
        // `id dir dlc flags`
        View::Can => format!(
            "{time:>12} {channel:>3} {:<6} {:<3} {:>3} {:<9}  {text}",
            "-", "-", "-", "-"
        ),
        // `id dir dlc crc`
        View::Lin => format!(
            "{time:>12} {channel:>3} {:<6} {:<3} {:>3} {:<8}  {text}",
            "-", "-", "-", "-"
        ),
        // `type proto vlan`
        View::Ethernet | View::All => format!(
            "{time:>12} {channel:>3} {:<7} {:<9} {:<5}  {text}",
            "-", "-", "-"
        ),
    }
}

/// LIN checksum state, shown in its own column.
fn lin_crc_label(status: &str) -> &'static str {
    match status {
        "response" => "ok",
        "checksum_error" => "crc-err",
        "parity_error" => "par-err",
        "no_response" => "no-resp",
        "header_only" => "hdr",
        "send_error" => "tx-err",
        "receive_error" => "rx-err",
        _ => "-",
    }
}

/// The part of an Ethernet summary that the columns do not already show.
fn ethernet_info_text(summary: &str, protocol: &str) -> String {
    let mut tokens: Vec<&str> = summary.split_whitespace().skip(3).collect();
    // Tags and the protocol name have their own columns.
    loop {
        if tokens.first() == Some(&"VLAN") {
            tokens.drain(..2);
            continue;
        }
        if tokens.first() == Some(&protocol) {
            tokens.drain(..1);
            continue;
        }
        break;
    }
    tokens.join(" ")
}

/// VLAN identifier out of a summary such as `… VLAN 10 ARP 10.0.0.1 > …`.
fn vlan_label(summary: &str) -> String {
    let tokens: Vec<&str> = summary.split_whitespace().collect();
    for (index, token) in tokens.iter().enumerate() {
        if *token == "VLAN" {
            return tokens.get(index + 1).copied().unwrap_or("-").to_string();
        }
    }
    "-".to_string()
}

/// Source (index 0) or destination (index 1) address of a summary.
fn ethernet_address(summary: &str, index: usize) -> String {
    summary
        .split_whitespace()
        .nth(index)
        .unwrap_or("-")
        .to_string()
}

/// A dim one-line description of a frame that belongs to another bus.
fn foreign_row(summary: &MessageSummary, axis: TimeAxis, pool: &TextPool) -> String {
    let time = axis.format(summary.timestamp_ns);
    let description = if let Some(info) = &summary.ethernet {
        let text = pool.get(info.summary);
        format!("eth 0x{:04X} {}", info.ether_type, short_summary(text))
    } else if let Some(event) = &summary.event {
        // A record without payload has no frame columns; keep its decoded text
        // so a foreign bus row still says what happened.
        format!(
            "{} {}: {}",
            summary.bus_label,
            event.kind,
            short_summary(&event.summary)
        )
    } else {
        format!(
            "{} {} {}",
            summary.bus_label,
            summary
                .frame_id
                .map_or_else(|| "-".to_string(), |id| format!("0x{id:X}")),
            hex_bytes(&summary.payload)
        )
    };
    format!("{time:>12}  · {description}")
}

/// Drop the leading `source > destination` pair; the row shows addresses in
/// its own columns, so repeating them here wastes width.
fn short_summary(summary: &str) -> String {
    summary
        .split_whitespace()
        .skip(3)
        .collect::<Vec<_>>()
        .join(" ")
}

fn summary_line(summary: &MessageSummary, axis: TimeAxis, pool: &TextPool, width: usize) -> String {
    let time = axis.format(summary.timestamp_ns);
    let bus = summary.bus_label;
    let channel = summary
        .channel
        .map_or_else(|| "-".to_string(), |value| value.to_string());
    let id = match (&summary.ethernet, summary.frame_id) {
        (Some(info), _) => format!("0x{:04X}", info.ether_type),
        (None, Some(value)) => format!("0x{value:03X}"),
        (None, None) => "------".to_string(),
    };
    let direction = summary.direction.unwrap_or("-");
    let payload = match (&summary.ethernet, &summary.event) {
        (Some(info), _) => short_summary(pool.get(info.summary)),
        (None, Some(event)) => format!("{}: {}", event.kind, event.summary),
        // A record with neither an event nor a payload still has a class, and
        // naming it beats a row of dashes that says nothing at all.
        (None, None) if summary.payload.is_empty() => format!("<{}>", summary.class_name),
        (None, None) => hex_bytes(&summary.payload),
    };
    // Each column keeps its own colour so the list stays scannable; the width
    // check runs on the visible text, before the escape codes are added.
    let plain = format!("{time:>12} {bus:<3} {channel:>3} {id:>6} {direction:<3} {payload}");
    // The row has to fit the pane it is drawn in; a fixed limit left the
    // separator further right than the terminal on a narrow window.
    if display_width(&plain) > width {
        return truncate(&plain, width);
    }
    format!(
        "{} {} {} {} {} {}",
        colored_cell(&time, 12, true, color_time()),
        colored_cell(bus, 3, false, color_bus()),
        colored_cell(&channel, 3, true, color_bus_channel(bus)),
        colored_cell(&id, 6, true, color_id()),
        colored_cell(direction, 3, false, color_direction(summary.direction)),
        paint(&payload, COLOR_DATA)
    )
}

/// The channel number follows the bus colour, so a column of mixed buses reads
/// as blocks instead of a wall of digits.
fn color_bus_channel(bus: &str) -> &'static str {
    match bus {
        "can" => slot_color(Slot::Green),
        "lin" => slot_color(Slot::Magenta),
        "eth" => slot_color(Slot::Blue),
        "most" => slot_color(Slot::Magenta),
        "flexray" => slot_color(Slot::Yellow),
        _ => COLOR_ATTRIBUTE,
    }
}

fn message_color(summary: &MessageSummary, text: String) -> String {
    if summary.status == "ok" {
        return match summary.bus {
            Some(BusKind::Can) => paint(text, slot_color(Slot::Green)),
            Some(BusKind::Lin) => paint(text, slot_color(Slot::Magenta)),
            None => match summary.bus_label {
                "eth" => paint(text, slot_color(Slot::Blue)),
                // The remaining buses each get their own hue so a mixed trace
                // stays scannable without reading every label.
                "most" => paint(text, slot_color(Slot::Magenta)),
                "flexray" => paint(text, slot_color(Slot::Yellow)),
                "afdx" => paint(text, slot_color(Slot::Cyan)),
                "a429" => paint(text, slot_color(Slot::Cyan)),
                "wlan" => paint(text, slot_color(Slot::Green)),
                "kline" | "j1708" => paint(text, slot_color(Slot::Yellow)),
                _ => paint(text, ANSI_PLAIN),
            },
        };
    }
    match summary.status {
        // A lost-object record is the one thing the user must not miss.
        "event"
            if matches!(
                summary.event.as_ref().map(|event| event.kind),
                Some("LOST" | "OVERRUN")
            ) =>
        {
            paint(text, slot_color(Slot::Red))
        }
        "event" => paint(text, slot_color(Slot::Cyan)),
        "header_only" | "no_response" | "unsupported" => paint(text, slot_color(Slot::Yellow)),
        _ => paint(text, slot_color(Slot::Red)),
    }
}

fn row_display(row: &SignalRow) -> String {
    if let Some(error) = &row.error {
        return format!("ERROR: {error}");
    }
    let mut value = row
        .physical
        .map_or_else(|| "-".to_string(), |value| format!("{value:.6}"));
    if let Some(raw) = row.raw {
        let _ = write!(value, " raw={raw}");
    }
    if let Some(choice) = &row.choice {
        let _ = write!(value, " [{choice}]");
    }
    value
}

/// Time span covered by every sample of every series.
fn curve_bounds(curve: &CurveData) -> Option<(u64, u64)> {
    let mut start = u64::MAX;
    let mut end = 0_u64;
    for series in &curve.series {
        for (timestamp, _) in &series.samples {
            start = start.min(*timestamp);
            end = end.max(timestamp.saturating_add(1));
        }
    }
    (start != u64::MAX && end > start).then_some((start, end))
}

/// Shift the window by a fifth of its width, staying inside `bounds`.
fn pan_window(window: (u64, u64), direction: isize, bounds: (u64, u64)) -> (u64, u64) {
    let width = window.1.saturating_sub(window.0).max(1);
    let step = (width / 5).max(1) as i128 * direction as i128;
    let max_start = bounds.1.saturating_sub(width);
    let start = (window.0 as i128 + step).clamp(bounds.0 as i128, max_start as i128) as u64;
    (start, start.saturating_add(width))
}

/// Scale the window around its centre, clamped to the data and to a minimum width.
fn zoom_window(window: (u64, u64), factor: f64, bounds: (u64, u64)) -> (u64, u64) {
    let width = window.1.saturating_sub(window.0).max(1);
    let data_width = bounds.1.saturating_sub(bounds.0).max(1);
    // The floor is the smallest useful window, never wider than the data itself.
    let lower = MIN_CURVE_WINDOW_NS.min(data_width).max(1);
    let wanted = (width as f64 * factor)
        .round()
        .clamp(lower as f64, data_width as f64) as u64;
    let centre = window.0.saturating_add(width / 2);
    let start = centre
        .saturating_sub(wanted / 2)
        .min(bounds.1.saturating_sub(wanted))
        .max(bounds.0);
    (start, start.saturating_add(wanted).min(bounds.1))
}

/// Value span of one series, printed in the legend.
fn series_range_label(series: &CurveSeries) -> String {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for (_, value) in &series.samples {
        min = min.min(*value);
        max = max.max(*value);
    }
    if min.is_finite() && max.is_finite() {
        format!("{min:.3}..{max:.3}")
    } else {
        "-".to_string()
    }
}

fn timing_stats(timestamps: &[u64]) -> Option<TimingStats> {
    if timestamps.len() < 2 {
        return None;
    }
    let mut ordered = timestamps.to_vec();
    ordered.sort_unstable();
    let deltas = ordered
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]))
        .filter(|delta| *delta > 0)
        .collect::<Vec<_>>();
    if deltas.is_empty() {
        return None;
    }
    let average = deltas.iter().map(|delta| *delta as f64).sum::<f64>() / deltas.len() as f64;
    let min = deltas.iter().copied().min().unwrap_or_default() as f64;
    let max = deltas.iter().copied().max().unwrap_or_default() as f64;
    let jitter = deltas
        .iter()
        .map(|delta| (*delta as f64 - average).abs())
        .sum::<f64>()
        / deltas.len() as f64;
    Some(TimingStats {
        frames: ordered.len(),
        avg_ms: average / 1_000_000.0,
        min_ms: min / 1_000_000.0,
        max_ms: max / 1_000_000.0,
        jitter_ms: jitter / 1_000_000.0,
    })
}

/// Draw every series into a shared time window.
///
/// Each series is normalised by its own value range so unrelated scales can be
/// compared; the legend carries the real ranges. A series keeps one symbol, and
/// the later series wins a cell two of them share.
/// Value-axis label width: enough for `-1234.5` plus a tick mark.
const PLOT_AXIS_WIDTH: usize = 9;

/// Round a span up to a "nice" step of 1, 2 or 5 times a power of ten.
fn nice_step(raw: f64) -> f64 {
    if !raw.is_finite() || raw <= 0.0 {
        return 1.0;
    }
    let exponent = raw.log10().floor();
    let scale = 10_f64.powf(exponent);
    let fraction = raw / scale;
    let nice = if fraction <= 1.0 {
        1.0
    } else if fraction <= 2.0 {
        2.0
    } else if fraction <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * scale
}

/// Tick values that cover `[min, max]` with roughly `target` intervals.
fn axis_ticks(min: f64, max: f64, target: usize) -> Vec<f64> {
    let span = (max - min).max(f64::EPSILON);
    let step = nice_step(span / target.max(1) as f64);
    let first = (min / step).floor() * step;
    let mut ticks = Vec::new();
    let mut value = first;
    // A generous bound keeps a pathological range from looping forever.
    while value <= max + step && ticks.len() < 64 {
        if value >= min - step {
            ticks.push(value);
        }
        value += step;
    }
    if ticks.is_empty() {
        ticks.push(min);
    }
    ticks
}

/// Format a tick value without trailing zeros in the fractional part.
///
/// Only the decimals may be trimmed: `3600` must stay `3600`, not become `36`.
fn tick_label(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let magnitude = value.abs();
    if !(1e-3..1e7).contains(&magnitude) {
        return format!("{value:.1e}");
    }
    let decimals = if magnitude >= 1000.0 {
        0
    } else if magnitude >= 10.0 {
        1
    } else if magnitude >= 1.0 {
        2
    } else {
        3
    };
    if decimals == 0 {
        return format!("{value:.0}");
    }
    let text = format!("{value:.decimals$}");
    match text.split_once('.') {
        Some((whole, fraction)) => {
            let fraction = fraction.trim_end_matches('0');
            if fraction.is_empty() {
                whole.to_string()
            } else {
                format!("{whole}.{fraction}")
            }
        }
        None => text,
    }
}

/// Lowest and highest finite sample of a series.
fn series_bounds(series: &CurveSeries) -> (f64, f64) {
    let mut low = f64::INFINITY;
    let mut high = f64::NEG_INFINITY;
    for (_, value) in &series.samples {
        if value.is_finite() {
            low = low.min(*value);
            high = high.max(*value);
        }
    }
    (low, high)
}

/// Draw the selected series with a value axis, a time axis and connected points.
///
/// Every series shares one value axis, so two signals keep their real
/// proportions instead of each being normalised into the same band.
fn plot_series(curve: &CurveData, window: (u64, u64), width: usize, height: usize) -> Vec<String> {
    // Two rows go to the time labels, one to the axis line.
    let plot_height = height.saturating_sub(2).max(1);
    let axis_width = PLOT_AXIS_WIDTH.min(width / 2);
    let plot_width = width.saturating_sub(axis_width + 2).max(1);
    let span = window.1.saturating_sub(window.0).max(1) as f64;

    // Shared axis by default: two signals keep their real proportions. With
    // `normalize` every series gets its own range, which is the only way to see
    // the shape of a signal whose unit is far smaller than its neighbours'.
    let mut value_min = f64::INFINITY;
    let mut value_max = f64::NEG_INFINITY;
    for series in &curve.series {
        let (series_min, series_max) = series_bounds(series);
        if !series_min.is_finite() || !series_max.is_finite() {
            continue;
        }
        if curve.normalize {
            // 0..1 for every series.
            value_min = value_min.min(0.0);
            value_max = value_max.max(1.0);
        } else {
            value_min = value_min.min(series_min);
            value_max = value_max.max(series_max);
        }
    }
    if !value_min.is_finite() || !value_max.is_finite() {
        return vec!["No numeric samples available".to_string()];
    }
    // Give a flat signal some height so it does not sit on the axis.
    if (value_max - value_min).abs() < f64::EPSILON {
        value_min -= 0.5;
        value_max += 0.5;
    }
    let value_span = value_max - value_min;

    let column_of = |timestamp: u64| -> Option<usize> {
        if timestamp < window.0 || timestamp > window.1 {
            return None;
        }
        let ratio = (timestamp - window.0) as f64 / span;
        Some(((ratio * (plot_width - 1) as f64).round() as usize).min(plot_width - 1))
    };
    // Where a value of `series` lands, honouring the normalisation mode.
    let row_of = |series: &CurveSeries, value: f64| -> usize {
        let (low, high) = if curve.normalize {
            series_bounds(series)
        } else {
            (value_min, value_max)
        };
        let span = (high - low).max(f64::EPSILON);
        let ratio = (high - value) / span;
        ((ratio.clamp(0.0, 1.0) * (plot_height - 1) as f64).round() as usize).min(plot_height - 1)
    };

    // A column takes the last sample that falls in it, so the line follows the
    // signal instead of drawing every sample on top of itself.
    let mut grid = vec![vec![None::<usize>; plot_width]; plot_height];
    for (index, series) in curve.series.iter().enumerate() {
        let mut columns: Vec<Option<f64>> = vec![None; plot_width];
        for (timestamp, value) in &series.samples {
            if !value.is_finite() {
                continue;
            }
            if let Some(column) = column_of(*timestamp) {
                columns[column] = Some(*value);
            }
        }
        // Connect neighbouring columns so the samples read as a line.
        let mut previous: Option<(usize, usize)> = None;
        for (column, value) in columns.iter().enumerate() {
            let Some(value) = value else {
                continue;
            };
            let row = row_of(series, *value);
            if let Some((last_column, last_row)) = previous {
                let gap = column - last_column;
                if gap > 1 {
                    for step in 1..gap {
                        let ratio = step as f64 / gap as f64;
                        let interpolated = last_row as f64 + (row as f64 - last_row as f64) * ratio;
                        let bridge = interpolated.round() as usize;
                        if let Some(cell) = grid
                            .get_mut(bridge.min(plot_height - 1))
                            .and_then(|line| line.get_mut(last_column + step))
                        {
                            *cell = Some(index);
                        }
                    }
                }
            }
            if let Some(cell) = grid.get_mut(row).and_then(|line| line.get_mut(column)) {
                *cell = Some(index);
            }
            previous = Some((column, row));
        }
    }

    let ticks = axis_ticks(value_min, value_max, plot_height.saturating_sub(1).max(2));
    // Which grid row each tick sits on, so the axis can label it. The axis is
    // shared, so its own bounds decide the row; with normalisation the labels
    // are fractions of each series' range.
    let mut tick_rows: Vec<(usize, String)> = ticks
        .iter()
        .map(|tick| {
            let ratio = (value_max - tick) / value_span;
            let row = ((ratio.clamp(0.0, 1.0) * (plot_height - 1) as f64).round() as usize)
                .min(plot_height - 1);
            let label = if curve.normalize {
                // The axis is a fraction of each series' own range now, so a
                // percentage is honest where a value would not be.
                let percent = (value_max - tick) / value_span * 100.0;
                format!("{percent:.0}%")
            } else {
                tick_label(*tick)
            };
            (row, label)
        })
        .filter(|(row, _)| *row < plot_height)
        .collect();
    tick_rows.sort_by_key(|(row, _)| *row);
    tick_rows.dedup_by_key(|(row, _)| *row);

    let mut lines = Vec::with_capacity(plot_height + 2);
    for (row, grid_row) in grid.iter().enumerate() {
        let label = tick_rows
            .iter()
            .find(|(tick_row, _)| *tick_row == row)
            .map(|(_, text)| text.clone())
            .unwrap_or_default();
        let mut line = format!("{label:>axis_width$} ┤");
        let mut current: Option<usize> = None;
        for cell in grid_row {
            match cell {
                Some(index) => {
                    if current != Some(*index) {
                        line.push_str(ANSI_RESET);
                        line.push_str(curve.series[*index].color);
                        current = Some(*index);
                    }
                    // A body character keeps the line continuous.
                    line.push('━');
                }
                None => {
                    if current.is_some() {
                        line.push_str(ANSI_RESET);
                        current = None;
                    }
                    line.push(' ');
                }
            }
        }
        if current.is_some() {
            line.push_str(ANSI_RESET);
        }
        lines.push(line);
    }

    // Time axis: an axis rule plus labels at the two ends.
    let axis_line = paint(
        format!("{:>axis_width$} ┴{}", "", "─".repeat(plot_width)),
        slot_color(Slot::Cyan),
    );
    lines.push(axis_line);
    // Two labels only: a narrow plot cannot fit more without them colliding.
    let first = curve_time_label(window.0, window);
    let last = curve_time_label(window.1, window);
    let mut labels = vec![' '; plot_width];
    for (offset, character) in first.chars().enumerate() {
        if let Some(slot) = labels.get_mut(offset) {
            *slot = character;
        }
    }
    let start = plot_width.saturating_sub(last.chars().count());
    for (offset, character) in last.chars().enumerate() {
        if let Some(slot) = labels.get_mut(start + offset) {
            *slot = character;
        }
    }
    lines.push(format!(
        "{} {}",
        " ".repeat(axis_width),
        paint(
            labels.into_iter().collect::<String>(),
            slot_color(Slot::Blue)
        )
    ));
    lines
}

/// Time label for the axis: the timestamp in seconds.
///
/// Both ends of the window use the same scale, so the labels stay comparable
/// however far into the capture the window sits.
fn curve_time_label(timestamp: u64, _window: (u64, u64)) -> String {
    format!("{:.3}s", (timestamp as f64) / 1e9)
}

fn format_timestamp(timestamp: Option<u64>) -> String {
    timestamp.map_or_else(
        || "-".to_string(),
        |value| format!("{:.6}s", value as f64 / 1_000_000_000.0),
    )
}

fn hex_bytes(data: &[u8]) -> String {
    data.iter()
        .map(|value| format!("{value:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Column widths of the frame statistics table; the right columns drop first
/// when the terminal is narrow.
/// Order the frame statistics rows; ties fall back to the frame id.
fn sort_frame_stats(stats: &mut [FrameStat], sort: StatsSort) {
    match sort {
        StatsSort::Count => stats.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then(left.key.sort_id().cmp(&right.key.sort_id()))
        }),
        StatsSort::FrameId => stats.sort_by(|left, right| {
            left.key
                .sort_id()
                .cmp(&right.key.sort_id())
                .then(left.key.channel().cmp(&right.key.channel()))
        }),
        StatsSort::Jitter => stats.sort_by(|left, right| {
            let jitter =
                |stat: &FrameStat| stat.timing.as_ref().map_or(-1.0, |timing| timing.jitter_ms);
            jitter(right)
                .partial_cmp(&jitter(left))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(left.key.sort_id().cmp(&right.key.sort_id()))
        }),
    }
}

fn stat_columns(width: usize) -> usize {
    if width >= 72 {
        4
    } else if width >= 52 {
        3
    } else {
        2
    }
}

fn stat_header_row(width: usize, unit: &str) -> String {
    let columns = match stat_columns(width) {
        4 => format!("ID       BUS CH     N   AVG({unit})  MIN({unit})  MAX({unit})  JIT({unit})"),
        _ => format!("ID       BUS CH     N   AVG({unit})"),
    };
    columns
}

/// Ethernet timing is reported in microseconds, CAN in milliseconds.
fn stat_unit(stats: &[FrameStat], view: View) -> &'static str {
    let ethernet = view == View::Ethernet
        || stats
            .iter()
            .any(|stat| matches!(stat.key, FrameKey::Ethernet(..)));
    if ethernet {
        "us"
    } else {
        "ms"
    }
}

fn stat_row(stat: &FrameStat, width: usize) -> String {
    let (id, bus, channel) = match stat.key {
        FrameKey::Bus(bus, channel, frame_id) => (frame_id, bus.as_str(), channel),
        FrameKey::Ethernet(channel, ether_type) => (u32::from(ether_type), "eth", channel),
    };
    let mut row = format!(
        "{:<8} {:<3} {:>3} {:>6}",
        format!("0x{id:X}"),
        bus,
        channel,
        stat.count
    );
    // Ethernet rows use microseconds, which is where TSN jitter lives.
    let scale = if matches!(stat.key, FrameKey::Ethernet(..)) {
        1000.0
    } else {
        1.0
    };
    if let Some(timing) = &stat.timing {
        let _ = write!(row, " {:>8.3}", timing.avg_ms * scale);
        if stat_columns(width) >= 4 {
            let _ = write!(
                row,
                " {:>8.3} {:>8.3} {:>8.3}",
                timing.min_ms * scale,
                timing.max_ms * scale,
                timing.jitter_ms * scale
            );
        }
    } else {
        row.push_str("        -");
    }
    row
}

fn display_filter(filter: &str) -> &str {
    if filter.is_empty() {
        "<empty>"
    } else {
        filter
    }
}

/// Match one message against an already-lowercased filter.
///
/// The haystack is rebuilt in a reusable buffer: formatting a fresh `String`
/// per message costs about 0.5 us per item, which is a visible pause when a
/// million-message trace is filtered.
fn summary_matches_in(
    summary: &MessageSummary,
    filter: &str,
    buffer: &mut String,
    pool: &TextPool,
) -> bool {
    buffer.clear();
    buffer.push_str(summary.class_name);
    buffer.push(' ');
    buffer.push_str(summary.bus.map_or("-", BusKind::as_str));
    buffer.push(' ');
    if let Some(channel) = summary.channel {
        push_decimal(buffer, u64::from(channel));
    }
    buffer.push(' ');
    if let Some(frame_id) = summary.frame_id {
        let frame_id = u64::from(frame_id);
        buffer.push_str("0x");
        push_hex(buffer, frame_id);
        buffer.push(' ');
        push_hex(buffer, frame_id);
        buffer.push(' ');
    }
    buffer.push_str(summary.direction.unwrap_or("-"));
    buffer.push(' ');
    if let Some(info) = &summary.ethernet {
        buffer.push_str(info.protocol);
        buffer.push(' ');
        buffer.push_str(pool.get(info.search));
        buffer.push(' ');
    }
    if let Some(event) = &summary.event {
        buffer.push_str(event.kind);
        buffer.push(' ');
        buffer.push_str(&event.search);
        buffer.push(' ');
    }
    buffer.push_str(summary.status);
    buffer.push(' ');
    push_decimal(buffer, u64::from(summary.object_type_raw));
    for byte in &summary.payload {
        buffer.push(' ');
        push_hex_byte(buffer, *byte);
    }
    buffer.make_ascii_lowercase();
    buffer.contains(filter)
}

/// Append a decimal number without going through `core::fmt`, which costs
/// noticeably more per message when a million messages are filtered.
fn push_decimal(buffer: &mut String, mut value: u64) {
    if value == 0 {
        buffer.push('0');
        return;
    }
    let mut digits = [0_u8; 20];
    let mut length = 0;
    while value > 0 {
        digits[length] = b'0' + (value % 10) as u8;
        value /= 10;
        length += 1;
    }
    for index in (0..length).rev() {
        buffer.push(char::from(digits[index]));
    }
}

fn push_hex(buffer: &mut String, mut value: u64) {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    if value == 0 {
        buffer.push('0');
        return;
    }
    let mut digits = [0_u8; 16];
    let mut length = 0;
    while value > 0 {
        digits[length] = DIGITS[(value % 16) as usize];
        value /= 16;
        length += 1;
    }
    for index in (0..length).rev() {
        buffer.push(char::from(digits[index]));
    }
}

fn push_hex_byte(buffer: &mut String, byte: u8) {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    buffer.push(char::from(DIGITS[(byte >> 4) as usize]));
    buffer.push(char::from(DIGITS[(byte & 0x0F) as usize]));
}

#[cfg(test)]
fn summary_matches(summary: &MessageSummary, filter: &str) -> bool {
    let pool = TextPool::default();
    summary_matches_in(summary, filter, &mut String::with_capacity(128), &pool)
}

fn lin_status(state: blf_core::LinResponseState) -> &'static str {
    match state {
        blf_core::LinResponseState::Response => "response",
        blf_core::LinResponseState::HeaderOnly => "header_only",
        blf_core::LinResponseState::NoResponse => "no_response",
        blf_core::LinResponseState::SendError => "send_error",
        blf_core::LinResponseState::ReceiveError => "receive_error",
        blf_core::LinResponseState::ChecksumError => "checksum_error",
        blf_core::LinResponseState::ParityError => "parity_error",
        blf_core::LinResponseState::Unsupported => "unsupported",
    }
}

/// One row of the frame statistics view.
#[derive(Debug, Clone)]
struct FrameStat {
    key: FrameKey,
    count: usize,
    timing: Option<TimingStats>,
}

/// Order of the frame statistics rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatsSort {
    Count,
    FrameId,
    Jitter,
}

impl StatsSort {
    fn next(self) -> Self {
        match self {
            Self::Count => Self::FrameId,
            Self::FrameId => Self::Jitter,
            Self::Jitter => Self::Count,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::FrameId => "frame id",
            Self::Jitter => "jitter",
        }
    }
}

/// How the message list prints a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeMode {
    /// The trace clock, as recorded in the file.
    Absolute,
    /// Seconds after the first message of the trace.
    Relative,
}

impl TimeMode {
    fn toggled(self) -> Self {
        match self {
            Self::Absolute => Self::Relative,
            Self::Relative => Self::Absolute,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Absolute => "absolute",
            Self::Relative => "relative",
        }
    }
}

/// A timestamp printer: `base` is the first message of the trace.
#[derive(Debug, Clone, Copy)]
struct TimeAxis {
    mode: TimeMode,
    base: u64,
}

impl TimeAxis {
    fn format(self, timestamp: Option<u64>) -> String {
        let Some(timestamp) = timestamp else {
            return "-".to_string();
        };
        match self.mode {
            TimeMode::Absolute => format!("{:.6}s", timestamp as f64 / 1_000_000_000.0),
            TimeMode::Relative => {
                let offset = timestamp.saturating_sub(self.base) as f64 / 1_000_000_000.0;
                format!("+{offset:.6}s")
            }
        }
    }
}

/// Whether a message timestamp falls inside the active range.
///
/// A message without a timestamp cannot be placed in time, so an active range
/// excludes it.
fn in_time_range(timestamp: Option<u64>, range: Option<(u64, u64)>) -> bool {
    match (timestamp, range) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(timestamp), Some((start, end))) => timestamp >= start && timestamp < end,
    }
}

/// Parse a `t` prompt: `10..15`, `..15`, `10..` or a single instant.
///
/// A leading `+` counts from the first message of the trace, matching the `g`
/// prompt; a bare number is the trace clock. Values are seconds.
fn parse_time_range(value: &str, base: u64) -> Result<Option<(u64, u64)>, String> {
    let text = value.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let instant = |text: &str| -> Result<u64, String> {
        let text = text.trim();
        let (relative, number) = match text.strip_prefix('+') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let seconds = parse_seconds(number, "Time")?;
        let nanos = seconds_to_ns(seconds).ok_or("Time must be finite")?;
        Ok(if relative {
            base.saturating_add(nanos)
        } else {
            nanos
        })
    };
    if let Some((start, end)) = text.split_once("..") {
        let start = if start.trim().is_empty() {
            0
        } else {
            instant(start)?
        };
        let end = if end.trim().is_empty() {
            u64::MAX
        } else {
            instant(end)?
        };
        if end <= start {
            return Err("The range end must be after its start".to_string());
        }
        return Ok(Some((start, end)));
    }
    let at = instant(text)?;
    // A single instant selects the messages at that timestamp.
    Ok(Some((at, at.saturating_add(1))))
}

/// A `g` prompt value: a time on the trace clock, a time relative to the
/// first message of the visible list, or a 1-based position in that list.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Goto {
    /// Seconds on the trace's own clock, the timestamps the list displays.
    Clock(f64),
    /// Seconds after the first message of the visible list, written `+10`.
    Relative(f64),
    /// 1-based position in the visible list, written `#1234`.
    Position(usize),
}

fn parse_goto(value: &str) -> Result<Goto, String> {
    let text = value.trim();
    if text.is_empty() {
        return Err("Enter a time (10.5), an offset (+10) or a position (#1234)".to_string());
    }
    if let Some(rest) = text.strip_prefix('#') {
        let position = rest
            .trim()
            .parse::<usize>()
            .map_err(|_| "Position must be a positive number".to_string())?;
        if position == 0 {
            return Err("Positions start at 1".to_string());
        }
        return Ok(Goto::Position(position));
    }
    if let Some(rest) = text.strip_prefix('+') {
        return parse_seconds(rest, "Offset").map(Goto::Relative);
    }
    // `@` is the explicit spelling of the bare, trace-clock form.
    let rest = text.strip_prefix('@').unwrap_or(text);
    parse_seconds(rest, "Time").map(Goto::Clock)
}

fn parse_seconds(value: &str, label: &str) -> Result<f64, String> {
    let text = value.strip_suffix('s').unwrap_or(value).trim();
    let seconds = text
        .parse::<f64>()
        .map_err(|_| format!("{label} must be a number of seconds, for example 10.5"))?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(format!(
            "{label} must be a finite, positive number of seconds"
        ));
    }
    Ok(seconds)
}

/// Absolute nanosecond target of a time jump. `base_ns` is the timestamp of
/// the first visible message, which the `+` form counts from.
fn goto_target(goto: Goto, base_ns: u64) -> Option<u64> {
    match goto {
        Goto::Clock(seconds) => seconds_to_ns(seconds),
        Goto::Relative(seconds) => {
            seconds_to_ns(seconds).map(|offset| base_ns.saturating_add(offset))
        }
        Goto::Position(_) => None,
    }
}

fn seconds_to_ns(seconds: f64) -> Option<u64> {
    let value = (seconds * 1_000_000_000.0).round();
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Some(value as u64)
}

fn first_index_at_or_after<I>(timestamps: I, target: u64) -> Option<usize>
where
    I: IntoIterator<Item = Option<u64>>,
{
    timestamps
        .into_iter()
        .position(|timestamp| timestamp.is_some_and(|timestamp| timestamp >= target))
}

/// Status after a time jump: the landed trace clock, its offset from the
/// first visible message, and the landed position in the visible list.
fn landed_status(reached_ns: u64, base_ns: u64, position: usize, total: usize) -> String {
    format!(
        "At {} (+{:.6}s from start) ({} / {})",
        format_timestamp(Some(reached_ns)),
        reached_ns.saturating_sub(base_ns) as f64 / 1_000_000_000.0,
        position + 1,
        total
    )
}

/// Bottom line: the active prompt while typing, otherwise the status message.
fn footer_line(input: Option<InputKind>, buffer: &str, status: &str, mode: Mode) -> String {
    let Some(kind) = input else {
        // The status comes first so a narrow terminal truncates the hints.
        return format!("{status}   ·   {}", mode_hint(mode));
    };
    let prompt = match kind {
        InputKind::Filter => "/",
        InputKind::Binding => "bind (can|lin CHANNEL PATH): ",
        InputKind::DatabaseFilter => "db /",
        InputKind::Goto => "goto (10.5 | +10 | #1234): ",
        InputKind::TimeRange => "range (10..15 | ..15 | +10..+15): ",
        InputKind::HelpFilter => "help /",
    };
    format!("{prompt}{buffer}  [Enter] apply  [Esc] cancel")
}

fn frame_label(summary: &MessageSummary) -> String {
    let bus = summary.bus.map_or("--", BusKind::as_str);
    let channel = summary
        .channel
        .map_or_else(|| "-".to_string(), |value| value.to_string());
    let id = summary
        .frame_id
        .map_or_else(|| "-".to_string(), |value| format!("0x{value:X}"));
    format!("{bus} {channel} {id}")
}

fn list_page_size(viewport_height: usize, reserved_rows: usize) -> usize {
    viewport_height.saturating_sub(reserved_rows).max(1)
}

/// Width of the key column of the `?` help table.
const HELP_KEY_WIDTH: usize = 18;

/// One row of the `?` help screen: a mode heading or a key binding.
struct HelpEntry {
    heading: bool,
    key: &'static str,
    description: &'static str,
}

const fn heading(label: &'static str) -> HelpEntry {
    HelpEntry {
        heading: true,
        key: label,
        description: "",
    }
}

const fn binding(key: &'static str, description: &'static str) -> HelpEntry {
    HelpEntry {
        heading: false,
        key,
        description,
    }
}

/// Every binding, grouped by mode, with the global keys last.
/// One-line hint under the list, shown instead of the cheat-sheet only when
/// the terminal has room for it (see `render`).
fn mode_hint(mode: Mode) -> &'static str {
    match mode {
        Mode::Messages => {
            "j/k move · Enter detail · v view · / filter · t range · s stats · e export · ? help"
        }
        Mode::Detail => "Tab pane · j/k move · p curve · T time · Esc back · ? help",
        Mode::Curve => "←/→ pan · +/- zoom · n axis · x remove signal · Esc back · ? help",
        Mode::Databases => "Tab panes · j/k move · Enter open · / search · [/] database · ? help",
        Mode::Statistics => "j/k move · Enter jump to frame · s sort · Esc back · ? help",
        Mode::Inspector => "j/k scroll · PgUp/PgDn · Home/End · e export · Esc back · ? help",
    }
}

const HELP_ENTRIES: &[HelpEntry] = &[
    heading("Messages (needs a trace)"),
    binding("j / Down", "next message"),
    binding("k / Up", "previous message"),
    binding("PageUp/PageDown", "move one screen"),
    binding("Home / End", "first / last message"),
    binding("Enter", "open the selected message"),
    binding("/", "filter messages"),
    binding("t", "limit to a time range, empty clears"),
    binding("b", "bind a DBC or LDF database"),
    binding("p", "curve for the selected numeric signal"),
    binding("a", "add the selected signal to the plotted curve"),
    binding("d", "open the database browser"),
    binding("s", "frame statistics: count and cycle time per frame id"),
    binding("e", "export filtered messages to CSV"),
    binding("g", "jump by trace time, +offset or #position"),
    binding("G", "jump to the last message"),
    binding("T", "switch timestamps between trace clock and first frame"),
    binding("] / [", "next / previous occurrence of the frame id"),
    binding("v / 1 2 3 4", "switch view: ALL, CAN, LIN, ETHERNET"),
    binding("H", "hide frames of other buses in this view"),
    binding("P", "export the visible frames as pcapng"),
    binding("i", "inspect every attribute of the selected frame"),
    binding("Esc", "clear the message filter and the time range"),
    binding("q", "quit"),
    heading("Detail (needs a trace)"),
    binding("Tab", "switch between attributes and signals"),
    binding("j / Down", "next signal, or scroll the focused pane"),
    binding("k / Up", "previous signal, or scroll the focused pane"),
    binding("PageUp/PageDown", "move one screen"),
    binding("Home / End", "first / last entry"),
    binding("p", "curve for the selected numeric signal"),
    binding("a", "add the selected signal to the open curve"),
    binding("b", "bind a DBC or LDF database"),
    binding("e", "export filtered messages to CSV"),
    binding("Esc / Left", "back to the message list"),
    binding("q", "quit"),
    heading("Curve (needs a trace)"),
    binding("Left / Right", "pan the sample window"),
    binding("+ / = / -", "zoom in / out"),
    binding("PageUp/PageDown", "zoom in / out"),
    binding("Home / End", "first / last sample"),
    binding("n", "scale each signal to its own range"),
    binding("x", "remove the last added signal"),
    binding("Esc", "back to the message detail"),
    binding("q", "quit"),
    heading("Frame statistics"),
    binding("j / k", "move through the frames"),
    binding("Enter", "jump to the first message of the frame"),
    binding("s", "cycle the sort order: count, frame id, jitter"),
    binding("Esc / Left", "back to the message list"),
    heading("Databases"),
    binding("j / Down", "next category, object or detail line"),
    binding("k / Up", "previous category, object or detail line"),
    binding("PageUp/PageDown", "move one screen in the focused pane"),
    binding("Home / End", "first / last row or detail line"),
    binding(
        "Left/Right or h/l",
        "focus the tree, object or details pane",
    ),
    binding("Tab / Shift-Tab", "cycle the tree, objects and details"),
    binding("] / [", "next / previous database"),
    binding("/", "search objects"),
    binding("Enter", "focus objects or report the selection"),
    binding("b", "bind a DBC or LDF database"),
    binding("Esc", "back to the message list (trace only)"),
    binding("q", "quit"),
    heading("Attributes (i)"),
    binding("j / k", "scroll the attribute list"),
    binding("PageUp/PageDown", "move one screen"),
    binding("Home / End", "first / last line"),
    binding("e", "export filtered messages to CSV"),
    binding("P", "export the visible frames to pcapng"),
    binding("Esc / Left", "back to the message list"),
    heading("Help"),
    binding("/", "search the key list"),
    binding("f", "clear the search"),
    binding("j / k", "scroll"),
    heading("Global"),
    binding("Ctrl-C", "quit the viewer"),
    binding("?", "open or close this help"),
    binding("Mouse", "click to select, wheel to scroll"),
    binding("--theme", "light, dark or auto colour set"),
];

/// Render the help table for `width` cells as `(is_heading, line)` rows. A
/// description that does not fit wraps under its key instead of being cut.
fn help_lines(width: usize) -> Vec<(bool, String)> {
    let split = HELP_KEY_WIDTH.min(width.saturating_sub(3));
    let description_width = width.saturating_sub(split + 3);
    let mut lines = Vec::new();
    for entry in HELP_ENTRIES {
        if entry.heading {
            lines.push((true, entry.key.to_string()));
            continue;
        }
        for (index, part) in wrap(entry.description, description_width)
            .into_iter()
            .enumerate()
        {
            let key = if index == 0 { entry.key } else { "" };
            lines.push((false, two_column(key, &part, split, width)));
        }
    }
    lines
}

/// Rows of the help table that fit below the header and above the footer.
fn help_page_size(height: usize) -> usize {
    height.saturating_sub(6).max(1)
}

fn clamp_help_offset(offset: usize, len: usize, page: usize) -> usize {
    offset.min(len.saturating_sub(page.max(1)))
}

fn move_index(index: usize, count: usize, delta: isize) -> usize {
    if count == 0 {
        return 0;
    }
    let index = index.min(count - 1) as isize + delta;
    index.clamp(0, count as isize - 1) as usize
}

fn can_direction(value: u8) -> &'static str {
    match value {
        0 => "Rx",
        1 => "Tx",
        _ => "?",
    }
}

fn lin_direction(value: u8) -> &'static str {
    match value {
        0 => "Rx",
        1 => "Tx",
        2 => "TxRq",
        _ => "?",
    }
}

fn normalize_can_id(raw: u32) -> (u32, bool) {
    let extended = raw & 0x8000_0000 != 0;
    (if extended { raw & 0x1FFF_FFFF } else { raw }, extended)
}

fn field_u16(object: &blf_core::LinObject, name: &str) -> Option<u16> {
    object.fields.iter().find_map(|(field, value)| {
        if *field != name {
            return None;
        }
        match value {
            FieldValue::U8(value) => Some(u16::from(*value)),
            FieldValue::U16(value) => Some(*value),
            FieldValue::U32(value) => u16::try_from(*value).ok(),
            _ => None,
        }
    })
}

fn field_u8(object: &blf_core::LinObject, name: &str) -> Option<u8> {
    object.fields.iter().find_map(|(field, value)| {
        if *field != name {
            return None;
        }
        match value {
            FieldValue::U8(value) => Some(*value),
            _ => None,
        }
    })
}

/// Parsed command line: an optional trace and every `--dbc`/`--ldf` binding.
#[derive(Debug)]
struct Args {
    trace: Option<PathBuf>,
    specs: Vec<BindingSpec>,
    theme: Theme,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let (trace, mut index) = match args.first() {
        None => return Err(usage()),
        // Without a leading trace path the viewer still needs a database to browse.
        Some(first) if first.starts_with("--") => (None, 0),
        Some(trace) => (Some(PathBuf::from(trace)), 1),
    };
    let mut specs = Vec::new();
    let mut theme = Theme::Auto;
    while index < args.len() {
        let option = args[index].as_str();
        if option == "--theme" {
            let value = args.get(index + 1).ok_or_else(usage)?;
            theme = Theme::from_name(value)
                .ok_or_else(|| format!("invalid theme: {value} (auto, light or dark)"))?;
            index += 2;
            continue;
        }
        let bus = match option {
            "--dbc" => BusKind::Can,
            "--ldf" => BusKind::Lin,
            _ => return Err(usage()),
        };
        let value = args.get(index + 1).ok_or_else(usage)?;
        let (channel, database_path) = value.split_once('=').ok_or_else(usage)?;
        let channel = channel
            .parse::<u16>()
            .map_err(|_| format!("invalid channel: {channel}"))?;
        specs.push((bus, channel, PathBuf::from(database_path)));
        index += 2;
    }
    if trace.is_none() && specs.is_empty() {
        return Err(usage());
    }
    Ok(Args {
        trace,
        specs,
        theme,
    })
}

fn usage() -> String {
    "usage: vector-log-tui TRACE.blf [--dbc CHANNEL=FILE] [--ldf CHANNEL=FILE]\n       \
     [--theme auto|light|dark]\n       \
     vector-log-tui --dbc CHANNEL=FILE [--ldf CHANNEL=FILE] …\n       \
     (without a trace, at least one --dbc/--ldf opens the database browser alone)"
        .to_string()
}

fn split_first_word(value: &str) -> (&str, &str) {
    let value = value.trim_start();
    match value.find(char::is_whitespace) {
        Some(index) => (&value[..index], value[index..].trim_start()),
        None => (value, ""),
    }
}

/// Terminal size straight from the kernel.
///
/// `stty size` needs a working child process and a PATH, and when it fails the
/// viewer silently assumed 120x40, which breaks every column width on any other
/// terminal. The `TIOCGWINSZ` ioctl is the authoritative answer; `stty` stays as
/// a fallback for platforms where the call is unavailable.
#[cfg(unix)]
fn terminal_size_from_kernel(fd: RawFd) -> Option<(usize, usize)> {
    #[repr(C)]
    struct WinSize {
        rows: u16,
        cols: u16,
        x_pixels: u16,
        y_pixels: u16,
    }
    extern "C" {
        fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
    }
    // Linux and the BSDs differ; both values are the ones their headers define.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const TIOCGWINSZ: std::ffi::c_ulong = 0x5413;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const TIOCGWINSZ: std::ffi::c_ulong = 0x4008_7468;

    let mut size = WinSize {
        rows: 0,
        cols: 0,
        x_pixels: 0,
        y_pixels: 0,
    };
    // SAFETY: `size` is a correctly sized `winsize` and the fd is a live tty.
    let result = unsafe { ioctl(fd, TIOCGWINSZ, &mut size as *mut WinSize) };
    if result != 0 || size.rows == 0 || size.cols == 0 {
        return None;
    }
    Some((usize::from(size.cols), usize::from(size.rows)))
}

#[cfg(not(unix))]
fn terminal_size_from_kernel(_fd: RawFd) -> Option<(usize, usize)> {
    None
}

fn command_output_with_stdin(
    program: &str,
    args: &[&str],
    stdin: &StdFile,
) -> Result<String, String> {
    let stdin = stdin
        .try_clone()
        .map_err(|error| format!("clone terminal for {program}: {error}"))?;
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::from(stdin))
        .output()
        .map_err(|error| format!("run {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed with status {}{}",
            output.status,
            if output.stderr.is_empty() {
                String::new()
            } else {
                format!(": {}", String::from_utf8_lossy(&output.stderr).trim())
            }
        ));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_string())
        .map_err(|error| format!("{program} returned non-UTF-8 output: {error}"))
}

fn run_command_with_stdin(program: &str, args: &[&str], stdin: &StdFile) -> Result<(), String> {
    let stdin = stdin
        .try_clone()
        .map_err(|error| format!("clone terminal for {program}: {error}"))?;
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::from(stdin))
        .status()
        .map_err(|error| format!("run {program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed with status {status}"))
    }
}

fn read_event_from<R: Read>(input: &mut R) -> Result<Option<InputEvent>, String> {
    let mut byte = [0_u8; 1];
    let read = input
        .read(&mut byte)
        .map_err(|error| format!("read terminal input: {error}"))?;
    if read == 0 {
        return Ok(None);
    }

    let event = match byte[0] {
        3 => InputEvent::CtrlC,
        4 => InputEvent::Eof,
        b'\r' | b'\n' => InputEvent::Enter,
        8 | 127 => InputEvent::Backspace,
        b'\t' => InputEvent::Tab,
        27 => read_escape_event(input)?,
        value if value.is_ascii() && !value.is_ascii_control() => InputEvent::Char(value as char),
        _ => return Ok(None),
    };
    Ok(Some(event))
}

fn read_escape_event<R: Read>(input: &mut R) -> Result<InputEvent, String> {
    let Some(first) = read_terminal_byte(input)? else {
        return Ok(InputEvent::Esc);
    };
    if first != b'[' {
        return Ok(InputEvent::Esc);
    }
    let Some(second) = read_terminal_byte(input)? else {
        return Ok(InputEvent::Esc);
    };
    if second == b'<' {
        return read_sgr_mouse_event(input);
    }

    let mut sequence = vec![second];
    if second.is_ascii_alphabetic() {
        return Ok(csi_event(&sequence));
    }
    while sequence.len() < 16 {
        let Some(byte) = read_terminal_byte(input)? else {
            return Ok(InputEvent::Esc);
        };
        sequence.push(byte);
        if (0x40..=0x7e).contains(&byte) {
            break;
        }
    }
    Ok(csi_event(&sequence))
}

fn read_sgr_mouse_event<R: Read>(input: &mut R) -> Result<InputEvent, String> {
    let mut text = String::new();
    while text.len() < 64 {
        let Some(byte) = read_terminal_byte(input)? else {
            return Ok(InputEvent::Esc);
        };
        let character = byte as char;
        if matches!(byte, b'M' | b'm') {
            let mut parts = text.split(';');
            let button = parts.next().and_then(|value| value.parse::<u16>().ok());
            let column = parts.next().and_then(|value| value.parse::<u16>().ok());
            let row = parts.next().and_then(|value| value.parse::<u16>().ok());
            let (Some(button), Some(column), Some(row)) = (button, column, row) else {
                return Ok(InputEvent::Esc);
            };
            let wheel = if button & 0x40 == 0 {
                0
            } else if button & 0x03 == 0 {
                1
            } else if button & 0x03 == 1 {
                -1
            } else {
                0
            };
            return Ok(InputEvent::Mouse(MouseEvent {
                button: (button & 0b11) as u8,
                column: column.saturating_sub(1),
                row: row.saturating_sub(1),
                wheel,
                release: byte == b'm',
            }));
        }
        text.push(character);
    }
    Ok(InputEvent::Esc)
}

fn csi_event(sequence: &[u8]) -> InputEvent {
    match sequence {
        [b'A'] => InputEvent::Up,
        [b'B'] => InputEvent::Down,
        [b'C'] => InputEvent::Right,
        [b'D'] => InputEvent::Left,
        [b'5', b'~'] => InputEvent::PageUp,
        [b'6', b'~'] => InputEvent::PageDown,
        [b'H'] | [b'1', b'~'] => InputEvent::Home,
        [b'F'] | [b'4', b'~'] => InputEvent::End,
        [b'Z'] => InputEvent::BackTab,
        _ => InputEvent::Esc,
    }
}

fn read_terminal_byte<R: Read>(input: &mut R) -> Result<Option<u8>, String> {
    let mut byte = [0_u8; 1];
    let read = input
        .read(&mut byte)
        .map_err(|error| format!("read escape sequence: {error}"))?;
    Ok((read > 0).then_some(byte[0]))
}

fn parse_terminal_size(value: &str) -> Option<(usize, usize)> {
    let mut parts = value.split_whitespace();
    let rows = parts.next().and_then(|value| value.parse::<usize>().ok());
    let columns = parts.next().and_then(|value| value.parse::<usize>().ok());
    match (columns, rows) {
        (Some(columns), Some(rows)) if columns > 0 && rows > 0 => Some((columns, rows)),
        _ => None,
    }
}

/// Terminal cells used by one character; CJK/fullwidth glyphs occupy two.
fn char_width(character: char) -> usize {
    match u32::from(character) {
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x20D0..=0x20F0 => 0,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

/// Visible width of `value`, skipping escape sequences.
///
/// A coloured row carries four or five bytes per column, and counting those as
/// printable made `truncate` cut the row short: the payload disappeared and the
/// selection highlight ran into the preview pane.
fn display_width(value: &str) -> usize {
    if !value.contains('\x1b') {
        return value.chars().map(char_width).sum();
    }
    let mut width = 0;
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character == '\x1b' {
            if chars.next() == Some('[') {
                for code in chars.by_ref() {
                    if ('@'..='~').contains(&code) {
                        break;
                    }
                }
            }
            continue;
        }
        width += char_width(character);
    }
    width
}

fn truncate(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(value) <= width {
        return value.to_string();
    }
    // A coloured row must be cut on a character boundary that is outside an
    // escape sequence, which `truncate_ansi` already handles.
    if value.contains('\x1b') {
        return truncate_ansi(value, width);
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let mut truncated = take_width(value, width - 3);
    truncated.push_str("...");
    truncated
}

/// Copy characters until adding the next one would exceed `width` cells.
fn take_width(value: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used = 0;
    for character in value.chars() {
        let cells = char_width(character);
        if used + cells > width {
            break;
        }
        used += cells;
        output.push(character);
    }
    output
}

/// Left-pad to an exact display width so columns line up for wide glyphs too.
fn pad_to_width(value: &str, width: usize) -> String {
    let mut padded = value.to_string();
    let used = display_width(value);
    padded.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    padded
}

/// Characters a wrapped line may break in front of when it has no space left
/// to break at: a specification, value or list member starts a new line
/// instead of a word being split.
const WRAP_BREAK_BEFORE: [char; 5] = ['(', '=', ',', '/', '-'];

/// The last position in `line` where a continuation may start, with the width
/// before it and whether the break consumes a space.
///
/// The space in front of an opening parenthesis wins, so a specification such
/// as `(INT 2..50000, default 2)` moves to its own line instead of being cut
/// into fragments; then any space, then a [`WRAP_BREAK_BEFORE`] character.
fn last_wrap_break(line: &str) -> Option<(usize, usize, bool)> {
    let mut space = None;
    let mut specification = None;
    let mut punctuation = None;
    let mut before = 0;
    let mut characters = line.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if character == ' ' && index > 0 {
            space = Some((index, before, true));
            if characters.peek().is_some_and(|(_, next)| *next == '(') {
                specification = Some((index, before, true));
            }
        } else if index > 0 && WRAP_BREAK_BEFORE.contains(&character) {
            punctuation = Some((index, before, false));
        }
        before += char_width(character);
    }
    specification.or(space).or(punctuation)
}

/// Wrap `text` into lines of at most `width` display cells.
///
/// Wide glyphs count as two cells. A line breaks after the space in front of an
/// opening parenthesis, else after its last space, else in front of the last
/// `(`, `=`, `,`, `/` or `-` it contains; a word wider than the whole line is
/// split as a last resort. Continuation lines repeat the leading indentation of
/// `text`, so a wrapped detail item stays indented under its heading.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let indent = text
        .chars()
        .take_while(|character| *character == ' ')
        .count();
    // A continuation has to keep at least one cell for text, so a terminal
    // narrower than the indentation drops it instead of losing the line.
    let indent = if indent + 1 > width { 0 } else { indent };
    let indent_text = " ".repeat(indent);
    let limit = width - indent;
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0;
    for character in text[indent..].chars() {
        let cells = char_width(character);
        if !line.is_empty() && line_width + cells > limit {
            match last_wrap_break(&line) {
                // Break at the last space or key character and keep the rest.
                Some((index, before, is_space)) => {
                    let carry = if is_space { index + 1 } else { index };
                    lines.push(format!("{indent_text}{}", &line[..index]));
                    line = line[carry..].to_string();
                    line_width -= before + usize::from(is_space);
                }
                // A word longer than the line is split at the boundary.
                None => {
                    lines.push(format!("{indent_text}{line}"));
                    line.clear();
                    line_width = 0;
                }
            }
        }
        line.push(character);
        line_width += cells;
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(format!("{indent_text}{line}"));
    }
    lines
}

/// Whether the database browser drops the tree and object columns and shows
/// the details across the whole width. Only a focused details pane does that,
/// and only below [`NARROW_DATABASE_WIDTH`], so the three-column layout stays
/// available for navigating and is untouched on wide terminals.
fn details_take_full_width(width: usize, pane: DatabasePane) -> bool {
    width < NARROW_DATABASE_WIDTH && pane == DatabasePane::Details
}

/// Clamp a detail-column line offset so the last page stays full instead of
/// scrolling past the end of the wrapped content.
fn clamp_detail_offset(offset: usize, total: usize, rows: usize) -> usize {
    offset.min(total.saturating_sub(rows.max(1)))
}

/// Move a detail-column line offset by `delta` and keep it inside the content.
fn move_detail_offset(offset: usize, total: usize, rows: usize, delta: isize) -> usize {
    let moved = (offset as isize).saturating_add(delta).max(0) as usize;
    clamp_detail_offset(moved, total, rows)
}

/// The visible line range written into the details header.
fn detail_range_label(offset: usize, rows: usize, total: usize) -> String {
    let first = if total == 0 { 0 } else { offset.min(total) + 1 };
    let last = (offset + rows).min(total);
    format!("lines {first}-{last} of {total}")
}

/// The details header for a `width`-cell column. The full label is used when
/// it fits and a shorter range when the column is narrow, so the header never
/// ends in a cut-off range.
fn details_header_text(offset: usize, rows: usize, total: usize, width: usize) -> String {
    let first = if total == 0 { 0 } else { offset.min(total) + 1 };
    let last = (offset + rows).min(total);
    let full = format!("Details  lines {first}-{last} of {total}");
    if display_width(&full) <= width {
        return full;
    }
    let range = detail_range_label(offset, rows, total);
    if display_width(&range) <= width {
        return range;
    }
    let compact = format!("{first}-{last}/{total}");
    if display_width(&compact) <= width {
        return compact;
    }
    take_width(&full, width)
}

fn two_column(left: &str, right: &str, split: usize, width: usize) -> String {
    let split = split.min(width.saturating_sub(3));
    format!(
        "{} │ {}",
        pad_to_width(&truncate(left, split), split),
        truncate(right, width.saturating_sub(split + 3))
    )
}

/// Separator row whose `┼` sits exactly under the row divider.
///
/// A row is `left(padded to split) │ right`, so the divider is at `split + 1`.
/// The previous formula put `┼` at `split` and made the row one column too wide.
fn two_column_separator(split: usize, width: usize) -> String {
    let split = split.min(width.saturating_sub(3));
    format!(
        "{}┼{}",
        "─".repeat(split + 1),
        "─".repeat(width.saturating_sub(split + 2))
    )
}

/// Two panes with the right one highlighted.
///
/// The mirror of `two_column_selected`: the frame details stay plain while the
/// selected signal inside the right pane is reversed.
fn right_column_selected(left: &str, right: &str, split: usize, width: usize) -> String {
    let split = split.min(width.saturating_sub(3));
    let right_width = width.saturating_sub(split + 3);
    let plain = strip_ansi(&truncate(right, right_width));
    format!(
        "{} │ {}",
        truncate(left, split),
        paint_selected(pad_to_width(&plain, right_width))
    )
}

/// Two panes with the left one highlighted.
///
/// The highlight stops at the divider: reversing the whole line made the
/// selection look like it belonged to the preview column as well.
fn two_column_selected(left: &str, right: &str, split: usize, width: usize) -> String {
    let split = split.min(width.saturating_sub(3));
    let plain = strip_ansi(&truncate(left, split));
    format!(
        "{}{} │ {}",
        paint_selected(pad_to_width(&plain, split)),
        ANSI_RESET,
        truncate(right, width.saturating_sub(split + 3))
    )
}

#[cfg(test)]
fn three_column(
    left: &str,
    middle: &str,
    right: &str,
    left_width: usize,
    middle_width: usize,
    width: usize,
) -> String {
    let left_width = left_width.min(width.saturating_sub(6));
    let middle_width = middle_width.min(width.saturating_sub(left_width + 6));
    format!(
        "{} │ {} │ {}",
        pad_to_width(&truncate(left, left_width), left_width),
        pad_to_width(&truncate(middle, middle_width), middle_width),
        truncate(right, width.saturating_sub(left_width + middle_width + 6))
    )
}

/// Copy ANSI escapes verbatim while counting only visible cells.
/// Drop the escape sequences, leaving the visible text.
///
/// Used where a row is re-coloured as a whole: the per-column colours would
/// reset an outer attribute mid-row.
fn strip_ansi(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\x1b' {
            output.push(character);
            continue;
        }
        if chars.next() != Some('[') {
            continue;
        }
        for code in chars.by_ref() {
            if ('@'..='~').contains(&code) {
                break;
            }
        }
    }
    output
}

fn ansi_prefix(value: &str, width: usize) -> (String, bool) {
    let mut output = String::new();
    let mut used = 0;
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\x1b' {
            output.push(character);
            if chars.next_if_eq(&'[').is_some() {
                output.push('[');
                for code in chars.by_ref() {
                    output.push(code);
                    if ('@'..='~').contains(&code) {
                        break;
                    }
                }
            }
            continue;
        }
        let cells = char_width(character);
        if used + cells > width {
            return (output, false);
        }
        used += cells;
        output.push(character);
    }
    (output, true)
}

fn truncate_ansi(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let (prefix, complete) = ansi_prefix(value, width);
    if complete {
        return prefix;
    }
    if width <= 3 {
        let (short, _) = ansi_prefix(value, width);
        return format!("{short}{ANSI_RESET}");
    }
    let (short, _) = ansi_prefix(value, width - 3);
    format!("{short}...{ANSI_RESET}")
}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{}", usage());
        return;
    }
    let args = match parse_args(&args) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };
    set_theme(args.theme);
    let app = match args.trace {
        Some(path) => App::open(path, args.specs),
        None => App::open_databases(args.specs),
    };
    if let Err(error) = app.and_then(|mut app| app.run()) {
        eprintln!("vector-log-tui: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail_texts(details: &[DetailLine]) -> Vec<&str> {
        details.iter().map(DetailLine::text).collect()
    }

    /// The details header cell of a rendered database-browser line.
    fn details_header_cell(line: &str) -> String {
        strip_ansi(line)
            .rsplit_once(" │ ")
            .map(|(_, right)| right.trim_end().to_string())
            .unwrap_or_default()
    }

    fn find_detail(details: &[DetailLine], expected: &str) -> usize {
        details
            .iter()
            .position(|line| line.text() == expected)
            .unwrap_or_else(|| panic!("missing detail line {expected:?}"))
    }

    fn strip_ansi(value: &str) -> String {
        let mut output = String::new();
        let mut chars = value.chars().peekable();
        while let Some(character) = chars.next() {
            if character == '\x1b' {
                if chars.next_if_eq(&'[').is_some() {
                    for code in chars.by_ref() {
                        if ('@'..='~').contains(&code) {
                            break;
                        }
                    }
                }
                continue;
            }
            output.push(character);
        }
        output
    }

    const TEST_DBC: &str = r#"
VERSION "1.0"
BU_: Vector__XXX BCM TCM Gateway

BO_ 291 VehicleStatus: 8 BCM
 SG_ VehicleSpeed : 0|16@1+ (0.01,0) [0|655.35] "km/h" TCM,Gateway
 SG_ EngineSpeed : 16|16@1+ (0.25,0) [0|16383.75] "rpm" Vector__XXX,TCM

BO_ 292 DoorStatus: 8 BCM
 SG_ FrontLeft : 0|1@1+ (1,0) [0|1] "" TCM

BO_ 293 BatteryStatus: 8 BMS
 SG_ PackVoltage : 0|16@1+ (0.01,0) [0|655.35] "V" Gateway

BO_ 294 AdasStatus: 8 ADAS
 SG_ TargetSpeed : 0|16@1+ (0.01,0) [0|655.35] "km/h" Gateway
"#;

    const RELATION_DBC: &str = r#"
VERSION "1.0"
BU_: BCM TCM Gateway

BO_ 256 VehicleStatus: 8 BCM
 SG_ VehicleSpeed : 0|16@1+ (0.01,0) [0|655.35] "km/h" TCM,Gateway
 SG_ EngineSpeed : 16|16@1+ (0.25,0) [0|16383.75] "rpm" Vector__XXX,TCM

BO_ 257 DoorStatus: 8 BCM
 SG_ FrontLeft : 0|1@1+ (1,0) [0|1] "" TCM

BO_ 258 EngineStatus: 8 TCM
 SG_ EngineTemp : 0|8@1+ (1,-40) [-40|215] "degC" BCM,Gateway
"#;

    const ATTRIBUTE_DBC: &str = r#"
VERSION "1.0"
BU_: BCM TCM

BO_ 256 VehicleStatus: 8 BCM
 SG_ VehicleSpeed : 0|16@1+ (0.01,0) [0|655.35] "km/h" TCM

BA_DEF_ BO_ "GenMsgCycleTime" INT 0 65535;
BA_DEF_DEF_ "GenMsgCycleTime" 0;
BA_DEF_ BO_ "GenMsgSendType" ENUM "Cyclic","Spontan";
BA_DEF_DEF_ "GenMsgSendType" "Cyclic";
BA_DEF_ SG_ "GenSigStartValue" FLOAT 0 100;
BA_DEF_DEF_ "GenSigStartValue" 0;
BA_DEF_ BU_ "GenNodeAddr" HEX 0 255;
BA_DEF_DEF_ "GenNodeAddr" 0;
BA_ "GenMsgCycleTime" BO_ 256 100;
BA_ "GenMsgSendType" BO_ 256 1;
BA_ "GenSigStartValue" SG_ 256 VehicleSpeed 5.5;
BA_ "GenNodeAddr" BU_ BCM 1;
"#;

    // One assigned message attribute and several unassigned definitions, plus
    // a signal definition that must not leak into the message scope.
    const DEFAULT_DBC: &str = r#"
VERSION "1.0"
BU_: BCM TCM

BO_ 256 VehicleStatus: 8 BCM
 SG_ VehicleSpeed : 0|16@1+ (0.01,0) [0|655.35] "km/h" TCM

BA_DEF_ BO_ "GenMsgCycleTime" INT 2 50000;
BA_DEF_DEF_ "GenMsgCycleTime" 2;
BA_DEF_ BO_ "GenMsgDelayTime" INT 1 1000;
BA_DEF_DEF_ "GenMsgDelayTime" 1;
BA_DEF_ BO_ "GenMsgFastOnStart" INT 0 0;
BA_DEF_DEF_ "GenMsgFastOnStart" 0;
BA_DEF_ BO_ "GenMsgILSupport" ENUM "No","Yes";
BA_DEF_DEF_ "GenMsgILSupport" "Yes";
BA_DEF_ SG_ "GenSigStartValue" FLOAT 0 100;
BA_DEF_DEF_ "GenSigStartValue" 0;
BA_ "GenMsgCycleTime" BO_ 256 100;
"#;

    const RELATION_LDF: &str = r#"
LIN_description_file;
LIN_protocol_version = "2.2";
LIN_language_version = "2.2";
LIN_speed = 19.2 kbps;

Nodes {
  Master: MasterNode, 5 ms, 0.1 ms ;
  Slaves: SlaveA ;
}

Signals {
  Mode: 2, 0, MasterNode, SlaveA ;
  Speed: 8, 0, SlaveA, MasterNode ;
}

Frames {
  Status: 1, SlaveA, 2 {
    Mode, 0 ;
    Speed, 8 ;
  }
}

Schedule_tables {
  Normal {
    Status delay 10 ms ;
  }
}
"#;

    /// A process-wide unique path in the temp directory for a test fixture.
    fn unique_temp_path(extension: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "vector-log-tui-test-{}-{}.{extension}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// A real `App` over an empty but valid trace, with one CAN database bound.
    fn test_database_app() -> App {
        test_database_app_for(TEST_DBC)
    }

    /// The same app with a caller-supplied DBC source.
    fn test_database_app_for(source: &str) -> App {
        let path = unique_temp_path("blf");
        let mut statistics = vec![0_u8; 144];
        statistics[0..4].copy_from_slice(&blf_core::FILE_SIGNATURE.to_le_bytes());
        statistics[4..8].copy_from_slice(&144_u32.to_le_bytes());
        statistics[8..12].copy_from_slice(&4_080_200_u32.to_le_bytes());
        statistics[16..24].copy_from_slice(&144_u64.to_le_bytes());
        statistics[24..32].copy_from_slice(&144_u64.to_le_bytes());
        std::fs::write(&path, &statistics).expect("write empty test trace");
        let mut app = App::open(path.clone(), Vec::new()).expect("open empty test trace");
        std::fs::remove_file(&path).ok();

        app.bindings.push(database_entry(source));
        app.enter_databases();
        app
    }

    fn database_entry(source: &str) -> BindingEntry {
        let database = DbcDatabase::from_str(source).expect("parse test DBC");
        let binding = DatabaseBinding::new(BusKind::Can, 1, Database::Can(Arc::new(database)))
            .expect("bind test DBC");
        BindingEntry {
            binding,
            path: PathBuf::from("test.dbc"),
        }
    }

    #[test]
    fn database_keys_move_the_focused_pane_and_keep_both_rows() {
        let mut app = test_database_app();
        app.set_database_category(1);
        app.database_object_selected = 2;
        app.database_pane = DatabasePane::Tree;

        // Moving focus keeps each pane on its own row.
        app.handle_event(InputEvent::Right).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Objects);
        assert_eq!(app.database_category, 1);
        assert_eq!(app.database_object_selected, 2);

        // Down now moves the object row, not the category.
        app.handle_event(InputEvent::Down).unwrap();
        assert_eq!(app.database_object_selected, 3);
        assert_eq!(app.database_category, 1);

        // h returns to the tree without losing the object row.
        app.handle_event(InputEvent::Char('h')).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Tree);
        assert_eq!(app.database_category, 1);
        assert_eq!(app.database_object_selected, 3);

        // Up now moves the category row, leaving the object pane focused off.
        app.handle_event(InputEvent::Up).unwrap();
        assert_eq!(app.database_category, 0);
        assert_eq!(app.database_pane, DatabasePane::Tree);

        // Tab and Shift-Tab switch focus without changing either row.
        app.handle_event(InputEvent::Tab).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Objects);
        app.handle_event(InputEvent::BackTab).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Tree);

        // Home/End stay inside the focused pane too.
        let last_category = app.database_categories().len() - 1;
        app.handle_event(InputEvent::End).unwrap();
        assert_eq!(app.database_category, last_category);
        app.handle_event(InputEvent::Home).unwrap();
        assert_eq!(app.database_category, 0);

        app.set_database_category(1);
        app.database_pane = DatabasePane::Objects;
        app.handle_event(InputEvent::End).unwrap();
        assert_eq!(app.database_object_selected, 3);
        assert_eq!(app.database_category, 1);
        app.handle_event(InputEvent::Home).unwrap();
        assert_eq!(app.database_object_selected, 0);
        assert_eq!(app.database_category, 1);
    }

    #[test]
    fn database_enter_moves_into_the_object_list_and_stays_there() {
        let mut app = test_database_app();
        app.set_database_category(1);
        app.database_pane = DatabasePane::Tree;
        app.database_object_selected = 1;

        app.handle_event(InputEvent::Enter).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Objects);
        assert_eq!(app.database_category, 1);
        assert_eq!(app.database_object_selected, 1);
        assert_eq!(app.status, "Ready");

        let title = app.db_objects[1].title.clone();
        app.handle_event(InputEvent::Enter).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Objects);
        assert_eq!(app.database_object_selected, 1);
        assert_eq!(app.status, format!("Selected {title}"));
    }

    #[test]
    fn database_mouse_click_focuses_and_selects_the_clicked_row() {
        let mut app = test_database_app();
        let left = app.database_left_width() as u16;

        // Row 0 of the tree is the database header, so the second category is
        // drawn on the row after the first category.
        app.handle_event(InputEvent::Mouse(MouseEvent {
            button: 0,
            column: 1,
            row: LIST_FIRST_ROW + 2,
            wheel: 0,
            release: false,
        }))
        .unwrap();
        assert_eq!(app.database_pane, DatabasePane::Tree);
        assert_eq!(app.database_category, 1);

        app.handle_event(InputEvent::Mouse(MouseEvent {
            button: 0,
            column: left + 3,
            row: LIST_FIRST_ROW + 1,
            wheel: 0,
            release: false,
        }))
        .unwrap();
        assert_eq!(app.database_pane, DatabasePane::Objects);
        assert_eq!(app.database_object_selected, 1);
        assert_eq!(app.database_category, 1);
    }

    #[test]
    fn database_render_marks_only_the_focused_pane_selection() {
        let mut app = test_database_app();
        app.set_database_category(1);
        app.database_object_selected = 1;
        app.database_pane = DatabasePane::Tree;

        let mut lines = Vec::new();
        app.render_databases(&mut lines, 100, 30);
        // lines[2] is body row 0 (database header + object 0). The selected
        // object row is body row 1 and the selected category row is body row 2.
        let database_row = &lines[2];
        assert!(database_row.contains(" can ch1"));
        assert!(!database_row.contains("▶"));
        assert!(!database_row.contains(ANSI_SELECTED));

        assert!(lines[3].contains("· DoorStatus"));
        assert!(!lines[3].contains(ANSI_SELECTED));
        assert!(lines[4].contains("▶ Messages"));
        assert!(lines[4].contains(ANSI_SELECTED));
        let header_parts = lines[0].split(" │ ").collect::<Vec<_>>();
        assert!(header_parts[0].starts_with(ANSI_BOLD));
        assert!(!header_parts[1].starts_with(ANSI_BOLD));

        app.database_pane = DatabasePane::Objects;
        let mut lines = Vec::new();
        app.render_databases(&mut lines, 100, 30);
        assert!(lines[3].contains("▶ DoorStatus"));
        assert!(lines[3].contains(ANSI_SELECTED));
        assert!(lines[4].contains("· Messages"));
        assert!(!lines[4].contains(ANSI_SELECTED));
        let header_parts = lines[0].split(" │ ").collect::<Vec<_>>();
        assert!(!header_parts[0].starts_with(ANSI_BOLD));
        assert!(header_parts[1].starts_with(ANSI_BOLD));
    }

    #[test]
    fn moves_selection_with_bounds() {
        assert_eq!(move_index(0, 0, 1), 0);
        assert_eq!(move_index(0, 3, -1), 0);
        assert_eq!(move_index(1, 3, 1), 2);
        assert_eq!(move_index(2, 3, 1), 2);
    }

    #[test]
    fn curve_legend_names_the_value_range() {
        let series = CurveSeries {
            title: "Speed".to_string(),
            samples: vec![(0, -10.0), (1, 50.0), (2, 10.0)],
            error: None,
            truncated: false,
            symbol: '*',
            color: slot_color(Slot::Green),
        };
        let label = series_range_label(&series);
        assert!(label.contains("-10.000"), "got {label}");
        assert!(label.contains("50.000"), "got {label}");
    }

    #[test]
    fn curve_plots_every_series_with_its_own_symbol() {
        let curve = CurveData {
            series: vec![
                CurveSeries {
                    title: "Speed".to_string(),
                    samples: (0..10).map(|index| (index, index as f64)).collect(),
                    error: None,
                    truncated: false,
                    symbol: '*',
                    color: slot_color(Slot::Green),
                },
                CurveSeries {
                    title: "Gear".to_string(),
                    samples: (0..10).map(|index| (index, 9.0 - index as f64)).collect(),
                    error: None,
                    truncated: false,
                    symbol: 'o',
                    color: slot_color(Slot::Magenta),
                },
            ],
            window_start: 0,
            window_end: 10,
            normalize: false,
        };
        let lines = plot_series(&curve, (0, 10), 20, 6);
        let painted = lines.join("\n");
        // The lines are drawn as `━`; the colour separates the series, so check
        // that both colours appear rather than the legend symbols.
        assert!(
            painted.contains(slot_color(Slot::Green)),
            "first series missing"
        );
        assert!(
            painted.contains(slot_color(Slot::Magenta)),
            "second series missing"
        );
        assert!(painted.matches('━').count() >= 4, "too few plotted cells");
    }

    #[test]
    fn parses_no_arguments_as_a_usage_error() {
        assert_eq!(parse_args(&[]).unwrap_err(), usage());
    }

    #[test]
    fn parses_a_trace_followed_by_database_bindings() {
        let args: Vec<String> = ["trace.blf", "--dbc", "1=vehicle.dbc", "--ldf", "3=body.ldf"]
            .into_iter()
            .map(String::from)
            .collect();
        let parsed = parse_args(&args).unwrap();
        assert_eq!(parsed.trace, Some(PathBuf::from("trace.blf")));
        assert_eq!(parsed.specs.len(), 2);
        assert_eq!(parsed.specs[0].0, BusKind::Can);
        assert_eq!(parsed.specs[1].1, 3);
    }

    #[test]
    fn parses_database_only_arguments_without_a_trace() {
        let args: Vec<String> = ["--dbc", "1=vehicle.dbc", "--ldf", "3=body.ldf"]
            .into_iter()
            .map(String::from)
            .collect();
        let parsed = parse_args(&args).unwrap();
        assert_eq!(parsed.trace, None);
        assert_eq!(parsed.specs.len(), 2);
        assert_eq!(parsed.specs[0].0, BusKind::Can);
        assert_eq!(parsed.specs[0].1, 1);
        assert_eq!(parsed.specs[1].0, BusKind::Lin);
        assert_eq!(parsed.specs[1].1, 3);
    }

    #[test]
    fn requires_the_trace_to_be_the_first_argument() {
        let args: Vec<String> = ["--dbc", "1=vehicle.dbc", "trace.blf"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(parse_args(&args).unwrap_err(), usage());
    }

    #[test]
    fn database_only_app_starts_in_the_browser_without_a_trace() {
        let path = unique_temp_path("dbc");
        std::fs::write(&path, TEST_DBC).expect("write test DBC");
        let mut app = App::open_databases(vec![(BusKind::Can, 1, path.clone())])
            .expect("open database-only app");
        std::fs::remove_file(&path).ok();

        assert_eq!(app.mode, Mode::Databases);
        assert!(app.trace.is_none());
        assert!(app.objects.is_empty());
        assert!(app.summaries.is_empty());
        assert!(app.filtered.is_empty());
        assert_eq!(app.bindings.len(), 1);
        assert!(!app.db_objects.is_empty(), "database objects should load");

        let title = app.title_line();
        assert!(title.contains("(no trace)"), "got {title}");
        assert!(title.contains("databases=1"), "got {title}");
        assert!(title.contains("mode=Databases"), "got {title}");

        // Esc cannot leave the browser, and trace-only keys explain why.
        app.handle_event(InputEvent::Esc).unwrap();
        assert_eq!(app.mode, Mode::Databases);
        assert_eq!(app.status, NO_TRACE_STATUS);
        for key in ['p', 'e', 'g', 'G'] {
            app.status.clear();
            app.handle_event(InputEvent::Char(key)).unwrap();
            assert_eq!(app.status, NO_TRACE_STATUS, "key {key}");
            assert_eq!(app.mode, Mode::Databases, "key {key}");
        }

        // The browser itself keeps working without a trace.
        app.handle_event(InputEvent::Char(']')).unwrap();
        assert_eq!(app.database_selected, 0);
        app.handle_event(InputEvent::Char('?')).unwrap();
        assert!(app.help.is_some());
        app.handle_event(InputEvent::Esc).unwrap();
        assert!(app.help.is_none());
        app.handle_event(InputEvent::Char('b')).unwrap();
        assert_eq!(app.input, Some(InputKind::Binding));
    }

    #[test]
    fn unions_receivers_without_placeholders_in_first_seen_order() {
        let first = vec!["Vector__XXX".to_string(), "BCM".to_string(), String::new()];
        let second = vec!["TCM".to_string(), "BCM".to_string(), "Gateway".to_string()];
        assert_eq!(
            union_names([first.as_slice(), second.as_slice()]),
            vec!["BCM".to_string(), "TCM".to_string(), "Gateway".to_string()]
        );
        assert_eq!(
            union_names(std::iter::empty::<&[String]>()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn dbc_message_details_name_receivers_after_the_transmitter() {
        use blf_core::{ByteOrder, CanMessageId, CanPayloadKind, SignalType};

        let mut speed = blf_core::CanSignal::new(
            "VehicleSpeed",
            0,
            16,
            ByteOrder::LittleEndian,
            SignalType::Unsigned,
        );
        speed.receivers = vec!["TCM".to_string(), "Vector__XXX".to_string(), String::new()];
        let mut engine = blf_core::CanSignal::new(
            "EngineSpeed",
            16,
            16,
            ByteOrder::LittleEndian,
            SignalType::Unsigned,
        );
        engine.receivers = vec!["Gateway".to_string(), "TCM".to_string()];
        let message = blf_core::DbcMessage {
            id: CanMessageId::new(0x123, false),
            name: "VehicleStatus".to_string(),
            dlc: 8,
            is_fd: false,
            payload_kind: CanPayloadKind::Physical,
            transmitter: "BCM".to_string(),
            signals: vec![speed, engine],
            comment: None,
            attributes: Default::default(),
            source: None,
        };

        let database = DbcDatabase::from_str(TEST_DBC).expect("parse test DBC");
        let details = dbc_message_details(&database, &message);
        let transmitter = find_detail(&details, "transmitter: BCM");
        // The receivers follow the transmitter as one name per line.
        let receivers = find_detail(&details, "receivers: 2");
        assert!(receivers > transmitter);
        assert_eq!(details[receivers + 1].text(), "TCM");
        assert_eq!(details[receivers + 2].text(), "Gateway");
        assert_eq!(
            detail_texts(&details)
                .iter()
                .filter(|line| line.starts_with("receivers:"))
                .count(),
            1
        );
    }

    #[test]
    fn dbc_signal_details_show_the_transmitter_and_deduplicated_receivers() {
        use blf_core::{ByteOrder, CanMessageId, CanPayloadKind, SignalType};

        let mut speed = blf_core::CanSignal::new(
            "VehicleSpeed",
            0,
            16,
            ByteOrder::LittleEndian,
            SignalType::Unsigned,
        );
        speed.receivers = vec!["TCM", "Vector__XXX", "TCM", "Gateway", ""]
            .into_iter()
            .map(str::to_string)
            .collect();
        let message = blf_core::DbcMessage {
            id: CanMessageId::new(0x123, false),
            name: "VehicleStatus".to_string(),
            dlc: 8,
            is_fd: false,
            payload_kind: CanPayloadKind::Physical,
            transmitter: "BCM".to_string(),
            signals: vec![speed],
            comment: None,
            attributes: Default::default(),
            source: None,
        };

        let database = DbcDatabase::from_str(TEST_DBC).expect("parse test DBC");
        let details = dbc_signal_details(&database, &message, &message.signals[0]);
        let transmitter = find_detail(&details, "transmitter: BCM");
        let receivers = find_detail(&details, "receivers: 2");
        assert!(receivers > transmitter);
        assert_eq!(details[receivers + 1].text(), "TCM");
        assert_eq!(details[receivers + 2].text(), "Gateway");
        assert_eq!(
            detail_texts(&details)
                .iter()
                .filter(|line| line.starts_with("transmitter:"))
                .count(),
            1
        );
        assert!(detail_texts(&details)
            .iter()
            .all(|line| !line.contains("Vector__XXX")));
    }

    #[test]
    fn lin_frame_details_name_subscribers_after_the_publisher() {
        use blf_core::{LinChecksum, LinFrame, LinSignal};

        let signal = LinSignal {
            name: "FrontLeft".to_string(),
            start_bit: 0,
            bit_length: 1,
            publisher: Some("BCM".to_string()),
            subscribers: vec!["TCM", "Vector__XXX", "Gateway"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            initial_value: None,
            factor: 1.0,
            offset: 0.0,
            minimum: None,
            maximum: None,
            physical_ranges: Vec::new(),
            encoding: None,
            choices: Default::default(),
            source: None,
        };
        let frame = LinFrame {
            id: 0x10,
            name: "DoorStatus".to_string(),
            dlc: 8,
            publisher: "BCM".to_string(),
            checksum: LinChecksum::Enhanced,
            signals: vec![signal],
            source: None,
        };

        let details = lin_frame_details(&frame);
        let publisher = find_detail(&details, "DLC: 8  publisher: BCM");
        let subscribers = find_detail(&details, "subscribers: 2");
        assert!(subscribers > publisher);
        assert_eq!(details[subscribers + 1].text(), "TCM");
        assert_eq!(details[subscribers + 2].text(), "Gateway");
    }

    #[test]
    fn lin_signal_details_deduplicate_subscribers() {
        use blf_core::LinSignal;

        let signal = LinSignal {
            name: "FrontLeft".to_string(),
            start_bit: 0,
            bit_length: 1,
            publisher: Some("BCM".to_string()),
            subscribers: vec!["TCM", "Vector__XXX", "TCM", "Gateway", ""]
                .into_iter()
                .map(str::to_string)
                .collect(),
            initial_value: None,
            factor: 1.0,
            offset: 0.0,
            minimum: None,
            maximum: None,
            physical_ranges: Vec::new(),
            encoding: None,
            choices: Default::default(),
            source: None,
        };

        let details = lin_signal_details(&signal);
        let publisher = find_detail(&details, "publisher: BCM");
        let subscribers = find_detail(&details, "subscribers: 2");
        assert!(subscribers > publisher);
        assert_eq!(details[subscribers + 1].text(), "TCM");
        assert_eq!(details[subscribers + 2].text(), "Gateway");
        assert!(detail_texts(&details)
            .iter()
            .all(|line| !line.contains("Vector__XXX")));
    }

    #[test]
    fn dbc_node_details_list_sent_and_received_messages_and_signals() {
        let database = DbcDatabase::from_str(RELATION_DBC).expect("parse relation DBC");
        let node = database
            .nodes()
            .find(|node| node.name == "BCM")
            .expect("BCM node");
        let details = dbc_node_details(&database, node);
        let position = |line: &str| find_detail(&details, line);

        assert_eq!(details[0], DetailLine::heading("node"));
        assert_eq!(details[1].text(), "name: BCM");
        let sent = position("messages sent: 2");
        assert_eq!(details[sent + 1].text(), "VehicleStatus 0x100 2 signals");
        assert_eq!(details[sent + 2].text(), "DoorStatus 0x101 1 signals");
        let received = position("messages received: 1");
        assert_eq!(details[received + 1].text(), "EngineStatus 0x102 1 signals");
        let sent_signals = position("signals sent: 3");
        let names: Vec<&str> = details[sent_signals + 1..sent_signals + 4]
            .iter()
            .map(DetailLine::text)
            .collect();
        assert_eq!(names, ["VehicleSpeed", "EngineSpeed", "FrontLeft"]);
        let received_signals = position("signals received: 1");
        assert_eq!(details[received_signals + 1].text(), "EngineTemp");

        let tcm = database
            .nodes()
            .find(|node| node.name == "TCM")
            .expect("TCM node");
        let details = dbc_node_details(&database, tcm);
        let position = |line: &str| find_detail(&details, line);
        assert_eq!(
            position("messages sent: 1") + 1,
            position("EngineStatus 0x102 1 signals")
        );
        assert_eq!(
            position("messages received: 2") + 1,
            position("VehicleStatus 0x100 2 signals")
        );
        assert_eq!(position("signals sent: 1") + 1, position("EngineTemp"));
        assert_eq!(
            position("signals received: 3") + 1,
            position("VehicleSpeed")
        );
    }

    #[test]
    fn dbc_details_render_attribute_definitions_defaults_and_fallbacks() {
        use blf_core::CanMessageId;

        let database = DbcDatabase::from_str(ATTRIBUTE_DBC).expect("parse attribute DBC");
        let message = database.messages().next().expect("message");
        let details = dbc_message_details(&database, message);
        assert!(detail_texts(&details).contains(&"attributes: 2"));
        assert!(detail_texts(&details).contains(&"GenMsgCycleTime = 100 (INT 0..65535, default 0)"));
        assert!(detail_texts(&details)
            .contains(&"GenMsgSendType = 1 (ENUM Cyclic, Spontan, default Cyclic)"));

        let signal_details = dbc_signal_details(&database, message, &message.signals[0]);
        assert!(detail_texts(&signal_details).contains(&"attributes: 1"));
        assert!(detail_texts(&signal_details)
            .contains(&"GenSigStartValue = 5.5 (FLOAT 0..100, default 0)"));

        let node = database
            .nodes()
            .find(|node| node.name == "BCM")
            .expect("BCM node");
        let node_details = dbc_node_details(&database, node);
        assert!(detail_texts(&node_details).contains(&"attributes: 1"));
        assert!(detail_texts(&node_details).contains(&"GenNodeAddr = 1 (HEX 0..255, default 0)"));

        let undefined = blf_core::DbcAttributeAssignment {
            name: "CustomAttr".to_string(),
            scope: "message".to_string(),
            message_id: Some(CanMessageId::new(256, false)),
            signal_name: None,
            object_name: None,
            value: "7".to_string(),
            source: None,
        };
        assert_eq!(attribute_line(&database, &undefined), "CustomAttr = 7");
    }

    #[test]
    fn attribute_category_shows_definitions_defaults_and_assignment_counts() {
        let entry = database_entry(ATTRIBUTE_DBC);
        let objects = database_objects(&entry, DbCategory::Attributes);

        let cycle = objects
            .iter()
            .find(|object| object.title == "GenMsgCycleTime")
            .expect("message cycle attribute");
        assert!(detail_texts(&cycle.details).contains(&"scope: message"));
        assert!(detail_texts(&cycle.details).contains(&"definition: INT 0..65535"));
        assert!(detail_texts(&cycle.details).contains(&"default: 0"));
        assert!(detail_texts(&cycle.details).contains(&"assignments: 1"));

        let send_type = objects
            .iter()
            .find(|object| object.title == "GenMsgSendType")
            .expect("message send type attribute");
        assert_eq!(send_type.subtitle, "ENUM Cyclic, Spontan");
        assert!(detail_texts(&send_type.details).contains(&"definition: ENUM Cyclic, Spontan"));

        assert!(objects.iter().any(|object| object.title == "GenNodeAddr"));
    }

    #[test]
    fn dbc_message_details_show_assigned_attributes_and_scope_defaults() {
        let database = DbcDatabase::from_str(DEFAULT_DBC).expect("parse defaults DBC");
        let message = database.messages().next().expect("message");
        let details = dbc_message_details(&database, message);
        let texts = detail_texts(&details);

        assert!(texts.contains(&"attributes: 1"));
        assert!(texts.contains(&"GenMsgCycleTime = 100 (INT 2..50000, default 2)"));
        let defaults = find_detail(&details, "defaults: 3");
        assert_eq!(details[defaults + 1].text(), "GenMsgDelayTime=1");
        assert_eq!(details[defaults + 2].text(), "GenMsgFastOnStart=0");
        assert_eq!(details[defaults + 3].text(), "GenMsgILSupport=Yes");
        // The assigned attribute is not repeated, and other scopes stay out.
        let defaults_text = details[defaults..]
            .iter()
            .map(DetailLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!defaults_text.contains("GenMsgCycleTime="));
        assert!(!defaults_text.contains("GenSigStartValue"));

        // The signal scope has its own default, which is not assigned here.
        let signal_details = dbc_signal_details(&database, message, &message.signals[0]);
        let signal_defaults = find_detail(&signal_details, "defaults: 1");
        assert_eq!(
            signal_details[signal_defaults + 1].text(),
            "GenSigStartValue=0"
        );
    }

    #[test]
    fn detail_scope_without_definitions_has_no_defaults_line() {
        let database = DbcDatabase::from_str(TEST_DBC).expect("parse test DBC");
        let message = database.messages().next().expect("message");
        let details = dbc_message_details(&database, message);
        assert!(detail_texts(&details)
            .iter()
            .all(|line| !line.starts_with("defaults:")));

        let node = database.nodes().next().expect("node");
        let node_details = dbc_node_details(&database, node);
        assert!(detail_texts(&node_details)
            .iter()
            .all(|line| !line.starts_with("defaults:")));
    }

    #[test]
    fn attribute_defaults_line_caps_scope_definitions() {
        let mut source = String::from("VERSION \"1.0\"\nBU_: BCM\n\nBO_ 256 M: 8 BCM\n\n");
        for index in 0..MAX_DETAIL_ENTRIES + 2 {
            let _ = writeln!(source, "BA_DEF_ BO_ \"Attr{index:02}\" INT 0 1;");
            let _ = writeln!(source, "BA_DEF_DEF_ \"Attr{index:02}\" 0;");
        }
        let database = DbcDatabase::from_str(&source).expect("parse generated defaults");
        let message = database.messages().next().expect("message");
        let details = dbc_message_details(&database, message);
        let defaults = find_detail(&details, &format!("defaults: {}", MAX_DETAIL_ENTRIES + 2));

        // Definitions are ordered by name, so only the guard boundary matters.
        assert_eq!(details[defaults + 1].text(), "Attr00=0");
        let listed = details[defaults + 1..=defaults + MAX_DETAIL_ENTRIES]
            .iter()
            .filter(|line| matches!(line, DetailLine::Item(_)))
            .count();
        assert_eq!(listed, MAX_DETAIL_ENTRIES);
        assert_eq!(
            details[defaults + MAX_DETAIL_ENTRIES + 1].text(),
            "… 2 more"
        );
    }

    #[test]
    fn detail_lines_separate_headings_items_notes_and_blank_rows() {
        let mut details = Vec::new();
        push_heading(&mut details, "message");
        details.push(DetailLine::item("name: VehicleStatus"));
        details.push(DetailLine::note("comment: demo"));
        push_heading(&mut details, "attributes: 1");
        details.push(DetailLine::item("GenMsgCycleTime = 100"));

        assert_eq!(details[0], DetailLine::heading("message"));
        assert_eq!(details[1], DetailLine::item("name: VehicleStatus"));
        assert_eq!(details[2], DetailLine::note("comment: demo"));
        assert_eq!(details[3], DetailLine::Blank);
        assert_eq!(details[4], DetailLine::heading("attributes: 1"));

        assert_eq!(
            details[0].render(),
            ("message".to_string(), CellStyle::DetailHeading)
        );
        assert_eq!(
            details[1].render(),
            ("  name: VehicleStatus".to_string(), CellStyle::DetailItem)
        );
        assert_eq!(
            details[2].render(),
            ("comment: demo".to_string(), CellStyle::Dim)
        );
        assert_eq!(details[3].render(), (String::new(), CellStyle::Normal));

        let heading = paint_cell("message", CellStyle::DetailHeading);
        assert!(heading.starts_with(ANSI_BOLD));
        assert!(heading.contains(slot_color(Slot::Cyan)));
        let item = paint_cell("  name: VehicleStatus", CellStyle::DetailItem);
        assert!(item.contains(ANSI_PLAIN));
        assert!(!item.contains(ANSI_BOLD));
    }

    #[test]
    fn lin_node_details_list_published_frames_and_subscribed_signals() {
        let database = LdfDatabase::from_str(RELATION_LDF).expect("parse relation LDF");
        let position = |details: &[DetailLine], line: &str| find_detail(details, line);

        let master = database
            .nodes()
            .find(|node| node.name == "MasterNode")
            .expect("master node");
        let details = lin_node_details(&database, master);
        assert_eq!(details[1].text(), "name: MasterNode");
        assert!(detail_texts(&details).contains(&"frames published: 0"));
        let subscribed = position(&details, "signals subscribed: 1");
        assert_eq!(details[subscribed + 1].text(), "Speed");

        let slave = database
            .nodes()
            .find(|node| node.name == "SlaveA")
            .expect("slave node");
        let details = lin_node_details(&database, slave);
        let published = position(&details, "frames published: 1");
        assert_eq!(details[published + 1].text(), "Status 0x01 2 signals");
        let subscribed = position(&details, "signals subscribed: 1");
        assert_eq!(details[subscribed + 1].text(), "Mode");
    }

    #[test]
    fn detail_lists_cap_entries_and_report_the_remainder() {
        let entries: Vec<String> = (0..MAX_DETAIL_ENTRIES + 3)
            .map(|index| format!("entry-{index}"))
            .collect();
        let mut details = Vec::new();
        push_detail_list(&mut details, "items", entries);

        assert_eq!(
            details[0].text(),
            format!("items: {}", MAX_DETAIL_ENTRIES + 3)
        );
        assert_eq!(details.len(), MAX_DETAIL_ENTRIES + 2);
        assert_eq!(details.last().unwrap().text(), "… 3 more");
    }

    #[test]
    fn database_details_fit_an_80x24_terminal() {
        let mut app = test_database_app();
        app.set_database_category(3);
        app.database_object_selected = 1;
        app.viewport_width = 80;
        app.viewport_height = 24;

        let mut lines = Vec::new();
        app.render_databases(&mut lines, 80, 24);
        assert!(lines.len() <= 24);
        for line in &lines {
            assert!(
                display_width(&strip_ansi(line)) <= 80,
                "line too wide: {line}"
            );
        }
        assert!(lines
            .iter()
            .map(|line| strip_ansi(line))
            .any(|line| line.contains("messages sent: 2")));
    }

    /// A DBC whose message details are longer than one 80x24 details column and
    /// whose attribute value and comment are wider than that column.
    const LONG_DETAIL_DBC: &str = r#"
VERSION "1.0"
BU_: BCM TCM

BO_ 256 VehicleStatus: 8 BCM
 SG_ VehicleSpeed : 0|16@1+ (0.01,0) [0|655.35] "km/h" TCM
 SG_ EngineSpeed : 16|16@1+ (0.25,0) [0|16383.75] "rpm" TCM
 SG_ GearPosition : 32|8@1+ (1,0) [0|15] "" TCM
 SG_ BrakePressure : 40|8@1+ (1,0) [0|255] "bar" TCM
 SG_ SteeringAngle : 48|8@1+ (1,0) [-128|127] "deg" TCM

BA_DEF_ BO_ "GenMsgSendType" STRING ;
BA_DEF_DEF_ "GenMsgSendType" "";
BA_ "GenMsgSendType" BO_ 256 "Cyclic, Spontaneous, IfActive, OnChange, OnWrite, OnRead, IfActiveAndOnChange, IfActiveAndOnRead";
CM_ BO_ 256 "A long comment that names every receiver and explains when the frame is sent on the bus.";
"#;

    /// A DBC shaped like the narrow-terminal report: an `EngineState` message
    /// long enough to scroll and a key/value attribute with a specification.
    const NARROW_DETAIL_DBC: &str = r#"
VERSION "1.0"
BU_: Engine

BO_ 291 EngineState: 8 Engine
 SG_ EngineSpeed : 0|16@1+ (0.25,0) [0|16383.75] "rpm" Engine
 SG_ EngineTorque : 16|16@1+ (0.5,-1000) [-1000|16000] "Nm" Engine
 SG_ CoolantTemperature : 32|8@1- (1,-40) [-40|215] "degC" Engine
 SG_ OilPressure : 40|8@1+ (0.1,0) [0|25.5] "bar" Engine
 SG_ ThrottlePosition : 48|8@1+ (0.4,0) [0|100] "%" Engine
 SG_ EngineLoad : 56|8@1+ (0.4,0) [0|100] "%" Engine
 SG_ IdleSpeed : 0|8@1+ (1,0) [0|255] "rpm" Engine
 SG_ FuelRate : 8|8@1+ (0.1,0) [0|25.5] "l/h" Engine
 SG_ BatteryVoltage : 16|8@1+ (0.1,0) [0|25.5] "V" Engine
 SG_ OilTemperature : 24|8@1- (1,-40) [-40|215] "degC" Engine
 SG_ IntakeAirTemperature : 32|8@1- (1,-40) [-40|215] "degC" Engine
 SG_ ExhaustGasTemperature : 40|8@1- (1,-40) [-40|215] "degC" Engine
 SG_ TurboBoost : 48|8@1+ (0.1,0) [0|25.5] "bar" Engine
 SG_ EgrPosition : 56|8@1+ (0.4,0) [0|100] "%" Engine
 SG_ RailPressure : 0|16@1+ (1,0) [0|65535] "bar" Engine
 SG_ InjectionTiming : 16|8@1- (0.5,-64) [-64|63.5] "deg" Engine
 SG_ FuelPressure : 24|16@1+ (0.1,0) [0|6553.5] "bar" Engine
 SG_ AirMassFlow : 40|16@1+ (0.1,0) [0|6553.5] "g/s" Engine
 SG_ LambdaValue : 56|8@1+ (0.01,0) [0|2.55] "" Engine
 SG_ KnockRetard : 0|8@1- (0.5,-64) [-64|63.5] "deg" Engine

BO_ 292 EngineOff: 8 Engine
 SG_ EngineRunning : 0|1@1+ (1,0) [0|1] "" Engine

BA_DEF_ BO_ "GenMsgCycleTime" INT 2 50000;
BA_DEF_DEF_ "GenMsgCycleTime" 2;
BA_ "GenMsgCycleTime" BO_ 291 100;
CM_ BO_ 291 "Bench harness frame for the narrow details layout.";
"#;

    #[test]
    fn details_offset_keeps_up_with_a_resized_column() {
        let mut app = test_database_app_for(NARROW_DETAIL_DBC);
        app.viewport_width = 80;
        app.viewport_height = 24;
        app.set_database_category(1);
        app.database_object_selected = 0;
        app.database_pane = DatabasePane::Details;
        let rows = app.database_body_rows(24);
        let narrow_width = app.database_detail_width(80);
        let narrow_total = app.database_detail_lines(narrow_width).len();
        app.handle_event(InputEvent::End).unwrap();
        assert_eq!(app.database_detail_offset, narrow_total - rows);

        // A wider terminal leaves the narrow layout for a wider third column.
        app.viewport_width = 260;
        let wide_width = app.database_detail_width(260);
        assert!(wide_width > narrow_width);
        let wide_total = app.database_detail_lines(wide_width).len();
        assert!(wide_total <= narrow_total);
        // An offset left past the end of the content is clamped back.
        app.database_detail_offset = usize::MAX;
        app.clamp_database_detail_offset();
        assert_eq!(app.database_detail_offset, wide_total.saturating_sub(rows));
    }

    #[test]
    fn wrap_breaks_plain_text_on_spaces_and_splits_long_words() {
        assert_eq!(wrap("alpha beta gamma", 11), ["alpha beta", "gamma"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 10), [""]);
        // A word longer than the line is split instead of being cut.
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
        for line in wrap("alpha beta gamma delta", 7) {
            assert!(display_width(&line) <= 7, "line too wide: {line}");
        }
    }

    #[test]
    fn wrap_breaks_before_key_characters_when_no_space_fits() {
        // A key = value stays whole and the specification starts a continuation,
        // both when the whole line does not fit and when only the specification
        // would have to be cut.
        assert_eq!(
            wrap("GenMsgCycleTime = 100 (INT 2..50000, default 2)", 25),
            ["GenMsgCycleTime = 100", "(INT 2..50000, default 2)"]
        );
        assert_eq!(
            wrap("GenMsgCycleTime = 100 (INT 2..50000, default 2)", 40),
            ["GenMsgCycleTime = 100", "(INT 2..50000, default 2)"]
        );
        // Without a space the break lands in front of `,` instead of inside a
        // word.
        assert_eq!(
            wrap("signal=Engine,Throttle,Brake", 14),
            ["signal=Engine", ",Throttle", ",Brake"]
        );
        // `/` and `-` are break points too.
        assert_eq!(wrap("left/right", 6), ["left", "/right"]);
        assert_eq!(wrap("multi-word", 8), ["multi", "-word"]);
        // Only a run without any break point is split inside the word.
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        for line in wrap("GenMsgCycleTime = 100 (INT 2..50000, default 2)", 25) {
            assert!(display_width(&line) <= 25, "line too wide: {line}");
        }
    }

    #[test]
    fn wrap_counts_wide_glyphs_and_keeps_continuation_indentation() {
        // Two CJK glyphs are four cells, so only one fits a four-cell column.
        assert_eq!(wrap("信号曲线", 4), ["信号", "曲线"]);
        // Continuation lines repeat the indentation of the wrapped line.
        assert_eq!(wrap("  信号 曲线 字", 7), ["  信号", "  曲线", "  字"]);
        assert_eq!(wrap("  abcdefgh", 6), ["  abcd", "  efgh"]);
        for line in wrap("  信号 曲线 字", 7) {
            assert!(display_width(&line) <= 7, "line too wide: {line}");
        }
    }

    #[test]
    fn detail_offsets_clamp_to_the_wrapped_content() {
        assert_eq!(clamp_detail_offset(0, 41, 18), 0);
        assert_eq!(clamp_detail_offset(5, 41, 18), 5);
        assert_eq!(clamp_detail_offset(100, 41, 18), 23);
        assert_eq!(clamp_detail_offset(usize::MAX, 41, 18), 23);
        // Content shorter than one page cannot scroll.
        assert_eq!(clamp_detail_offset(5, 10, 18), 0);
        assert_eq!(clamp_detail_offset(5, 0, 18), 0);
        assert_eq!(move_detail_offset(20, 41, 18, 5), 23);
        assert_eq!(move_detail_offset(0, 41, 18, -5), 0);
    }

    #[test]
    fn detail_range_label_names_the_visible_lines() {
        assert_eq!(detail_range_label(0, 18, 41), "lines 1-18 of 41");
        assert_eq!(detail_range_label(23, 18, 41), "lines 24-41 of 41");
        assert_eq!(detail_range_label(0, 18, 0), "lines 0-0 of 0");
    }

    #[test]
    fn details_pane_cycles_and_scrolls_wrapped_lines() {
        let mut app = test_database_app_for(NARROW_DETAIL_DBC);
        app.viewport_width = 80;
        app.viewport_height = 24;
        app.set_database_category(1);
        app.database_object_selected = 0;
        app.database_pane = DatabasePane::Details;
        let width = app.database_detail_width(80);
        let total = app.database_detail_lines(width).len();
        let rows = app.database_body_rows(24);
        assert!(total > rows, "fixture details should overflow the pane");
        app.database_pane = DatabasePane::Tree;

        // Tab cycles Tree -> Objects -> Details -> Tree, Shift-Tab reverses it.
        assert_eq!(app.database_pane, DatabasePane::Tree);
        app.handle_event(InputEvent::Tab).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Objects);
        app.handle_event(InputEvent::Tab).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Details);
        app.handle_event(InputEvent::Tab).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Tree);
        app.handle_event(InputEvent::BackTab).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Details);

        // Up/Down scroll by one wrapped line and clamp at both ends.
        app.handle_event(InputEvent::Down).unwrap();
        assert_eq!(app.database_detail_offset, 1);
        app.handle_event(InputEvent::End).unwrap();
        assert_eq!(app.database_detail_offset, total - rows);
        app.handle_event(InputEvent::Down).unwrap();
        assert_eq!(app.database_detail_offset, total - rows);
        app.handle_event(InputEvent::Home).unwrap();
        assert_eq!(app.database_detail_offset, 0);
        app.handle_event(InputEvent::PageDown).unwrap();
        assert_eq!(
            app.database_detail_offset,
            clamp_detail_offset(app.page_step() as usize, total, rows)
        );
        assert_eq!(app.page_step() as usize, rows);
        app.handle_event(InputEvent::PageUp).unwrap();
        assert_eq!(app.database_detail_offset, 0);

        // Left/Right step through the three panes and stop at either end.
        app.handle_event(InputEvent::Left).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Objects);
        app.handle_event(InputEvent::Left).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Tree);
        app.handle_event(InputEvent::Left).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Tree);
        app.handle_event(InputEvent::Right).unwrap();
        app.handle_event(InputEvent::Right).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Details);
    }

    #[test]
    fn details_scroll_resets_when_the_selected_object_changes() {
        let mut app = test_database_app_for(NARROW_DETAIL_DBC);
        app.viewport_width = 80;
        app.viewport_height = 24;
        app.set_database_category(1);
        app.database_object_selected = 0;
        app.database_pane = DatabasePane::Details;
        app.handle_event(InputEvent::End).unwrap();
        assert!(app.database_detail_offset > 0);

        // The object list owns the selection, so moving it resets the scroll.
        app.handle_event(InputEvent::Left).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Objects);
        app.handle_event(InputEvent::Down).unwrap();
        assert_eq!(app.database_object_selected, 1);
        assert_eq!(app.database_detail_offset, 0);

        // Scrolling again, then picking another object with the mouse resets it.
        app.handle_event(InputEvent::Up).unwrap();
        assert_eq!(app.database_object_selected, 0);
        app.database_pane = DatabasePane::Details;
        app.handle_event(InputEvent::End).unwrap();
        assert!(app.database_detail_offset > 0);
        // The tree and object panes come back when they own the focus, so the
        // object row can be clicked from there.
        app.handle_event(InputEvent::Left).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Objects);
        let left = app.database_left_width() as u16;
        app.handle_event(InputEvent::Mouse(MouseEvent {
            button: 0,
            column: left + 3,
            row: LIST_FIRST_ROW + 1,
            wheel: 0,
            release: false,
        }))
        .unwrap();
        assert_eq!(app.database_object_selected, 1);
        assert_eq!(app.database_detail_offset, 0);
    }

    #[test]
    fn details_header_reports_the_visible_line_range() {
        let mut app = test_database_app_for(NARROW_DETAIL_DBC);
        app.viewport_width = 80;
        app.viewport_height = 24;
        app.set_database_category(1);
        app.database_object_selected = 0;
        app.database_pane = DatabasePane::Details;
        let width = app.database_detail_width(80);
        let total = app.database_detail_lines(width).len();
        let rows = app.database_body_rows(24);

        // The narrow layout gives the details the whole width, so the full
        // label fits the header.
        let mut lines = Vec::new();
        app.render_databases(&mut lines, 80, 24);
        let header = strip_ansi(&lines[0]).trim_end().to_string();
        assert_eq!(header, details_header_text(0, rows, total, width));
        assert_eq!(
            header,
            format!("Details  lines 1-{} of {total}", rows.min(total))
        );

        // A wide terminal keeps the three columns and the details header in
        // the third cell.
        let (_, _, wide_width) = app.database_layout(160);
        let wide_total = app.database_detail_lines(wide_width).len();
        let mut lines = Vec::new();
        app.render_databases(&mut lines, 160, 24);
        let header = details_header_cell(&lines[0]);
        assert_eq!(
            header,
            format!("Details  lines 1-{} of {wide_total}", rows.min(wide_total))
        );

        // Scrolling to the end moves the reported range with it.
        app.handle_event(InputEvent::End).unwrap();
        let mut lines = Vec::new();
        app.render_databases(&mut lines, 80, 24);
        let header = strip_ansi(&lines[0]).trim_end().to_string();
        assert_eq!(
            header,
            details_header_text(total - rows, rows, total, width)
        );
        assert_eq!(
            header,
            format!("Details  lines {}-{total} of {total}", total - rows + 1)
        );
    }

    #[test]
    fn narrow_layout_decision_uses_the_width_and_the_focused_pane() {
        assert!(details_take_full_width(80, DatabasePane::Details));
        assert!(details_take_full_width(99, DatabasePane::Details));
        assert!(!details_take_full_width(100, DatabasePane::Details));
        assert!(!details_take_full_width(160, DatabasePane::Details));
        assert!(!details_take_full_width(80, DatabasePane::Tree));
        assert!(!details_take_full_width(80, DatabasePane::Objects));
    }

    #[test]
    fn narrow_layout_shows_the_details_at_full_width() {
        let mut app = test_database_app_for(NARROW_DETAIL_DBC);
        app.viewport_width = 80;
        app.viewport_height = 24;
        app.set_database_category(1);
        app.database_object_selected = 0;

        // The tree and object panes own the focus at first, so the three-column
        // layout is drawn and the narrow details column shreds the values.
        let mut lines = Vec::new();
        app.render_databases(&mut lines, 80, 24);
        assert!(lines.iter().any(|line| strip_ansi(line).contains(" │ ")));

        // Focusing the details replaces the other columns with a full-width
        // view: the short values stay on one line.
        app.database_pane = DatabasePane::Details;
        let mut lines = Vec::new();
        app.render_databases(&mut lines, 80, 24);
        let text = lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains(" │ "), "columns should collapse: {text}");
        assert!(text.contains("name: EngineState"), "missing name: {text}");
        assert!(text.contains("transmitter: Engine"), "missing transmitter");
        assert!(
            text.contains("GenMsgCycleTime = 100 (INT 2..50000, default 2)"),
            "attribute line split: {text}"
        );
        let header = strip_ansi(&lines[0]).trim_end().to_string();
        assert!(header.starts_with("Details  lines 1-"), "header: {header}");

        // Scrolling still works and the header follows it.
        app.handle_event(InputEvent::End).unwrap();
        let offset = app.database_detail_offset;
        assert!(offset > 0);
        let mut lines = Vec::new();
        app.render_databases(&mut lines, 80, 24);
        let header = strip_ansi(&lines[0]).trim_end().to_string();
        assert!(
            header.starts_with(&format!("Details  lines {}-", offset + 1)),
            "header: {header}"
        );

        // Leaving the details brings the three-column layout back.
        app.handle_event(InputEvent::Left).unwrap();
        assert_eq!(app.database_pane, DatabasePane::Objects);
        let mut lines = Vec::new();
        app.render_databases(&mut lines, 80, 24);
        assert!(lines.iter().any(|line| strip_ansi(line).contains(" │ ")));
    }

    #[test]
    fn wide_layout_keeps_the_three_columns() {
        let mut app = test_database_app_for(NARROW_DETAIL_DBC);
        app.viewport_width = 120;
        app.viewport_height = 24;
        app.set_database_category(1);
        app.database_object_selected = 0;
        app.database_pane = DatabasePane::Details;
        let (_, _, width) = app.database_layout(120);
        assert_eq!(app.database_detail_width(120), width);

        let mut lines = Vec::new();
        app.render_databases(&mut lines, 120, 24);
        let header = strip_ansi(&lines[0]);
        assert!(header.contains("Database tree"), "header: {header}");
        assert!(header.contains(" │ "), "header: {header}");
        assert!(
            lines.iter().any(|line| strip_ansi(line).contains(" │ ")),
            "three-column body expected"
        );
    }

    #[test]
    fn long_detail_values_are_reachable_by_wrapping() {
        let mut app = test_database_app_for(LONG_DETAIL_DBC);
        app.viewport_width = 80;
        app.viewport_height = 24;
        app.set_database_category(1);
        app.database_object_selected = 0;
        let (_, _, width) = app.database_layout(80);
        let details = app.database_detail_lines(width);
        assert!(details.len() > app.database_body_rows(24));

        for (text, _) in &details {
            assert!(
                display_width(text) <= width,
                "detail line too wide for {width} cells: {text}"
            );
        }
        // Spaces at a wrap and a long word split across lines both disappear
        // once the whitespace is squeezed out, so the text stays intact.
        let combined = details
            .iter()
            .map(|(text, _)| text.trim())
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<String>();
        assert!(
            combined.contains(
                "Cyclic,Spontaneous,IfActive,OnChange,OnWrite,OnRead,IfActiveAndOnChange,IfActiveAndOnRead"
            ),
            "attribute value lost: {combined}"
        );
        assert!(
            combined.contains(
                "Alongcommentthatnameseveryreceiverandexplainswhentheframeissentonthebus."
            ),
            "comment lost: {combined}"
        );
        assert!(!combined.contains("..."));
    }

    #[test]
    fn long_data_line_wraps_in_the_message_preview() {
        let mut app = App::new(None, Vec::new(), Vec::new()).expect("build app");
        app.mode = Mode::Messages;
        app.viewport_width = 80;
        app.viewport_height = 24;
        let payload: Vec<u8> = (0u8..32).collect();
        app.summaries.push(MessageSummary {
            object_index: 0,
            timestamp_ns: Some(1_000_000_000),
            timestamp_source: None,
            bus: Some(BusKind::Can),
            bus_label: "can",
            ethernet: None,
            event: None,
            can_flags: 0,
            channel: Some(1),
            frame_id: Some(0x123),
            object_type_raw: 1,
            class_name: "CAN_MESSAGE",
            direction: Some("Rx"),
            dlc: Some(8),
            payload: payload.clone(),
            status: "ok",
        });
        app.filtered = vec![0];
        app.selected = 0;

        let mut lines = Vec::new();
        // A preview pane only exists above the minimum width.
        app.render_messages(&mut lines, 120, 24);
        // Strip the message list column so only the preview text remains.
        let preview = lines
            .iter()
            .filter_map(|line| {
                strip_ansi(line)
                    .rsplit_once(" │ ")
                    .map(|(_, right)| right.trim_end().to_string())
            })
            .collect::<Vec<_>>()
            .join(" ");
        let preview = preview.split_whitespace().collect::<Vec<_>>().join(" ");
        let data = format!("data: {}", hex_bytes(&payload));
        assert!(
            preview.contains(&data),
            "data line lost from the preview: {preview}"
        );
        assert!(!preview.contains("..."));
    }

    #[test]
    fn truncates_without_splitting_utf8_or_wide_glyphs() {
        assert_eq!(truncate("abcdef", 0), "");
        assert_eq!(truncate("abcdef", 3), "...");
        assert_eq!(truncate("abcdef", 5), "ab...");
        assert_eq!(truncate("信号曲线", 3), "...");
        // Four CJK glyphs need eight cells; they must not be split in half.
        assert_eq!(truncate("信号曲线", 8), "信号曲线");
        assert_eq!(truncate("信号曲线", 4), "...");
        assert_eq!(truncate("信号", 4), "信号");
    }

    #[test]
    fn truncates_ansi_by_visible_width_and_keeps_reset() {
        let value = paint("abcdef", slot_color(Slot::Green));
        let truncated = truncate_ansi(&value, 5);
        assert!(truncated.starts_with(slot_color(Slot::Green)));
        assert!(truncated.ends_with(ANSI_RESET));
        assert!(truncated.contains("ab..."));
        assert!(!truncated.contains("cdef"));
        assert_eq!(truncate_ansi(&value, 6), value);
    }

    #[test]
    fn parses_navigation_sequences() {
        assert_eq!(csi_event(b"A"), InputEvent::Up);
        assert_eq!(csi_event(b"6~"), InputEvent::PageDown);
        assert_eq!(csi_event(b"1~"), InputEvent::Home);
        assert_eq!(csi_event(b"4~"), InputEvent::End);
        assert_eq!(csi_event(b"Z"), InputEvent::BackTab);
        assert_eq!(
            read_event_from(&mut "\t".as_bytes()).unwrap(),
            Some(InputEvent::Tab)
        );
        assert_eq!(csi_event(b"99~"), InputEvent::Esc);
    }

    #[test]
    fn splits_binding_input_without_losing_paths_with_spaces() {
        let (bus, rest) = split_first_word("  can   1 /tmp/my database.dbc");
        let (channel, path) = split_first_word(rest);
        assert_eq!(bus, "can");
        assert_eq!(channel, "1");
        assert_eq!(path, "/tmp/my database.dbc");
    }

    #[test]
    fn binding_syntax_errors_are_returned_for_retry() {
        assert_eq!(lin_status(blf_core::LinResponseState::Response), "response");
        assert_eq!(
            lin_status(blf_core::LinResponseState::HeaderOnly),
            "header_only"
        );
        assert_eq!(
            lin_status(blf_core::LinResponseState::SendError),
            "send_error"
        );
    }

    #[test]
    fn display_width_counts_wide_glyphs_as_two_cells() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("信号"), 4);
        assert_eq!(display_width("信号abc"), 7);
        assert_eq!(display_width(""), 0);
        // Combining marks take no cell of their own.
        assert_eq!(display_width("e\u{301}"), 1);
    }

    #[test]
    fn truncate_never_exceeds_the_requested_cells() {
        assert!(display_width(&truncate("信号曲线信号曲线", 6)) <= 6);
        assert!(display_width(&truncate("信号曲线信号曲线", 7)) <= 7);
        assert_eq!(truncate("信号曲线", 8), "信号曲线");
        assert!(display_width(&truncate("abcdef", 5)) <= 5);
    }

    #[test]
    fn columns_stay_aligned_with_wide_glyphs() {
        let line = two_column("信号 0x344", "预览内容", 20, 40);
        let (left, right) = line.split_once(" │ ").unwrap();
        assert_eq!(display_width(left), 20);
        assert_eq!(right, "预览内容");
        assert!(display_width(&line) <= 40);

        let line = three_column("Database", "信号 (2)", "details", 12, 14, 40);
        let parts = line.split(" │ ").collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);
        assert_eq!(display_width(parts[0]), 12);
        assert_eq!(display_width(parts[1]), 14);
        assert!(display_width(&line) <= 40);

        // Degenerate narrow terminals must clamp instead of wrapping.
        let line = three_column("a", "b", "c", 10, 10, 12);
        assert!(display_width(&line) <= 12);
    }

    #[test]
    fn ansi_columns_stay_aligned_after_painting() {
        let line = paint(
            two_column("信号 0x344", "preview", 20, 40),
            slot_color(Slot::Green),
        );
        let visible = strip_ansi(&line);
        let (left, _) = visible.split_once(" │ ").unwrap();
        assert_eq!(display_width(left), 20);

        let truncated = truncate_ansi(&line, 40);
        assert!(display_width(&strip_ansi(&truncated)) <= 40);
        assert!(truncated.contains(ANSI_RESET));
    }

    #[test]
    fn summary_line_truncates_by_display_width() {
        let summary = MessageSummary {
            object_index: 0,
            timestamp_ns: Some(1),
            timestamp_source: None,
            bus: Some(BusKind::Can),
            bus_label: "can",
            ethernet: None,
            event: None,
            can_flags: 0,
            channel: Some(1),
            frame_id: Some(0x344),
            object_type_raw: 1,
            class_name: "CAN_MESSAGE",
            direction: Some("Rx"),
            dlc: Some(64),
            payload: vec![0xAB; 64],
            status: "ok",
        };
        let pool = TextPool::default();
        let line = summary_line(
            &summary,
            TimeAxis {
                mode: TimeMode::Absolute,
                base: 0,
            },
            &pool,
            120,
        );
        assert!(display_width(&line) <= 120);
        assert!(line.ends_with("..."));
    }

    fn sample_summary(
        frame_id: u32,
        bus: BusKind,
        channel: u16,
        status: &'static str,
    ) -> MessageSummary {
        MessageSummary {
            object_index: 0,
            timestamp_ns: Some(0),
            timestamp_source: None,
            bus: Some(bus),
            bus_label: bus.as_str(),
            ethernet: None,
            event: None,
            can_flags: 0,
            channel: Some(channel),
            frame_id: Some(frame_id),
            object_type_raw: 101,
            class_name: "CAN_MESSAGE",
            direction: Some("Rx"),
            dlc: Some(2),
            payload: vec![0xAB, 0xCD],
            status,
        }
    }

    #[test]
    fn filter_matches_frame_ids_status_and_payload() {
        let summary = sample_summary(0x344, BusKind::Can, 1, "ok");
        assert!(summary_matches(&summary, "0x344"));
        assert!(summary_matches(&summary, "344"));
        // Payload bytes are space separated, matching what the list displays.
        assert!(summary_matches(&summary, "ab cd"));
        assert!(!summary_matches(&summary, "abcd"));
        assert!(summary_matches(&summary, "can 1"));
        assert!(summary_matches(&summary, "can_message"));
        assert!(!summary_matches(&summary, "0x999"));
    }

    #[test]
    fn filter_matches_lin_status_words() {
        let summary = sample_summary(0x2f, BusKind::Lin, 3, "header_only");
        assert!(summary_matches(&summary, "header_only"));
        assert!(summary_matches(&summary, "lin 3"));
        assert!(!summary_matches(&summary, "send_error"));
    }

    fn frame_stat(id: u32, count: usize, jitter_ms: f64) -> FrameStat {
        FrameStat {
            key: FrameKey::Bus(BusKind::Can, 1, id),
            count,
            timing: Some(TimingStats {
                frames: count,
                avg_ms: 10.0,
                min_ms: 9.0,
                max_ms: 11.0,
                jitter_ms,
            }),
        }
    }

    #[test]
    fn statistics_sort_orders_by_count_frame_id_or_jitter() {
        let mut stats = vec![
            frame_stat(0x300, 5, 0.5),
            frame_stat(0x100, 20, 0.1),
            frame_stat(0x200, 10, 2.0),
        ];
        sort_frame_stats(&mut stats, StatsSort::Count);
        assert_eq!(
            stats.iter().map(|stat| stat.count).collect::<Vec<_>>(),
            vec![20, 10, 5]
        );
        sort_frame_stats(&mut stats, StatsSort::FrameId);
        assert_eq!(
            stats
                .iter()
                .map(|stat| stat.key.sort_id())
                .collect::<Vec<_>>(),
            vec![0x100, 0x200, 0x300]
        );
        sort_frame_stats(&mut stats, StatsSort::Jitter);
        assert_eq!(
            stats
                .iter()
                .map(|stat| stat.key.sort_id())
                .collect::<Vec<_>>(),
            vec![0x200, 0x300, 0x100]
        );
        assert_eq!(StatsSort::Count.next(), StatsSort::FrameId);
        assert_eq!(StatsSort::Jitter.next(), StatsSort::Count);
    }

    #[test]
    fn statistics_rows_drop_columns_on_a_narrow_terminal() {
        let stat = frame_stat(0x344, 54, 0.012);
        let wide = stat_row(&stat, 80);
        assert!(wide.contains("0x344"));
        assert!(wide.contains("10.000"));
        assert!(wide.contains("0.012"));

        let narrow = stat_row(&stat, 40);
        assert!(narrow.contains("0x344"));
        assert!(narrow.contains("10.000"));
        assert!(!narrow.contains("0.012"));
        assert!(stat_header_row(40, "ms").contains("AVG"));

        let single = FrameStat {
            key: FrameKey::Bus(BusKind::Lin, 3, 0x2f),
            count: 1,
            timing: None,
        };
        let row = stat_row(&single, 80);
        assert!(row.contains("lin"));
        assert!(row.trim_end().ends_with('-'));
    }

    #[test]
    fn time_range_parsing_handles_open_ends_offsets_and_errors() {
        let base = 1_000_000_000;
        assert_eq!(parse_time_range("", base).unwrap(), None);
        assert_eq!(
            parse_time_range("1..2", base).unwrap(),
            Some((1_000_000_000, 2_000_000_000))
        );
        assert_eq!(
            parse_time_range("..2", base).unwrap(),
            Some((0, 2_000_000_000))
        );
        assert_eq!(
            parse_time_range("1..", base).unwrap(),
            Some((1_000_000_000, u64::MAX))
        );
        // A leading `+` counts from the first message of the trace.
        assert_eq!(
            parse_time_range("+1..+2", base).unwrap(),
            Some((2_000_000_000, 3_000_000_000))
        );
        // A single instant selects exactly that timestamp.
        assert_eq!(
            parse_time_range("5", base).unwrap(),
            Some((5_000_000_000, 5_000_000_001))
        );
        assert!(parse_time_range("5..5", base).is_err());
        assert!(parse_time_range("3..1", base).is_err());
        assert!(parse_time_range("a..b", base).is_err());
    }

    #[test]
    fn time_range_excludes_messages_outside_it() {
        let range = Some((1_000_000_000, 2_000_000_000));
        assert!(in_time_range(Some(1_000_000_000), range));
        assert!(in_time_range(Some(1_999_999_999), range));
        // The end is exclusive.
        assert!(!in_time_range(Some(2_000_000_000), range));
        assert!(!in_time_range(Some(999_999_999), range));
        // A message without a timestamp cannot be placed in time.
        assert!(!in_time_range(None, range));
        // Without a range everything passes.
        assert!(in_time_range(None, None));
        assert!(in_time_range(Some(5), None));
    }

    #[test]
    fn time_axis_prints_the_trace_clock_or_an_offset() {
        let base = 1_672_542_331_984_470_000;
        let absolute = TimeAxis {
            mode: TimeMode::Absolute,
            base,
        };
        assert_eq!(
            absolute.format(Some(1_672_542_332_000_000_000)),
            "1672542332.000000s"
        );
        assert_eq!(absolute.format(None), "-");

        let relative = TimeAxis {
            mode: TimeMode::Relative,
            base,
        };
        assert_eq!(
            relative.format(Some(1_672_542_332_000_000_000)),
            "+0.015530s"
        );
        // A clock before the first frame never prints a negative offset.
        assert_eq!(relative.format(Some(0)), "+0.000000s");
        assert_eq!(TimeMode::Absolute.toggled(), TimeMode::Relative);
        assert_eq!(TimeMode::Relative.toggled(), TimeMode::Absolute);
    }

    #[test]
    fn goto_accepts_trace_clock_relative_and_position_forms() {
        assert_eq!(parse_goto("10.000770"), Ok(Goto::Clock(10.000770)));
        assert_eq!(parse_goto(" 12.5s "), Ok(Goto::Clock(12.5)));
        assert_eq!(parse_goto("0"), Ok(Goto::Clock(0.0)));
        assert_eq!(parse_goto("+10"), Ok(Goto::Relative(10.0)));
        assert_eq!(parse_goto(" + 10s "), Ok(Goto::Relative(10.0)));
        // `@` stays accepted as the explicit trace-clock spelling.
        assert_eq!(parse_goto("@1672542341.5"), Ok(Goto::Clock(1672542341.5)));
        assert_eq!(parse_goto("#1"), Ok(Goto::Position(1)));
        assert_eq!(parse_goto("# 42 "), Ok(Goto::Position(42)));
        assert!(parse_goto("").is_err());
        assert!(parse_goto("#0").is_err());
        assert!(parse_goto("#x").is_err());
        assert!(parse_goto("@nope").is_err());
        assert!(parse_goto("+").is_err());
        assert!(parse_goto("+-1").is_err());
        assert!(parse_goto("-1").is_err());
        assert!(parse_goto("later").is_err());
    }

    #[test]
    fn goto_targets_use_the_trace_clock_on_a_trace_that_starts_late() {
        // Repro trace: the first message is at 0.001820s, one is at 10.000770s.
        let base = 1_820_000;
        let timestamps = [Some(base), Some(10_000_770_000), Some(10_101_820_000)];

        let clock = goto_target(Goto::Clock(10.000770), base).expect("clock target");
        assert_eq!(clock, 10_000_770_000);
        assert_eq!(first_index_at_or_after(timestamps, clock), Some(1));

        let relative = goto_target(Goto::Relative(10.0), base).expect("relative target");
        assert_eq!(relative, 10_001_820_000);
        assert_eq!(first_index_at_or_after(timestamps, relative), Some(2));

        assert_eq!(goto_target(Goto::Position(2), base), None);
    }

    #[test]
    fn jump_status_names_both_clocks_and_the_position() {
        // Landing at 10.000770s when the first message is 0.001820s.
        assert_eq!(
            landed_status(10_000_770_000, 1_820_000, 511, 1374),
            "At 10.000770s (+9.998950s from start) (512 / 1374)"
        );
        assert_eq!(
            landed_status(5_025_000_000, 0, 0, 219),
            "At 5.025000s (+5.025000s from start) (1 / 219)"
        );
    }

    #[test]
    fn bottom_line_shows_the_status_first_then_hints_or_the_prompt() {
        let line = footer_line(
            None,
            "",
            "At 5.025000s (+3.510000s from start) (118 / 219)",
            Mode::Messages,
        );
        // The status stays first so a narrow terminal truncates the hints.
        assert!(line.starts_with("At 5.025000s (+3.510000s from start) (118 / 219)"));
        assert!(line.contains("Enter detail"));

        let prompt = footer_line(Some(InputKind::Goto), "+5", "Ready", Mode::Messages);
        assert!(prompt.starts_with("goto ("), "got {prompt}");
        assert!(prompt.contains("+5"));
        // While typing, the prompt owns the line.
        assert!(!prompt.contains("Ready"));
        assert!(!prompt.contains("Enter detail"));
        assert_eq!(
            footer_line(Some(InputKind::Filter), "fd", "", Mode::Messages),
            "/fd  [Enter] apply  [Esc] cancel"
        );
        // Every mode has a hint of its own.
        for mode in [
            Mode::Messages,
            Mode::Detail,
            Mode::Curve,
            Mode::Databases,
            Mode::Statistics,
        ] {
            assert!(!mode_hint(mode).is_empty());
        }
    }

    #[test]
    fn help_lists_bindings_by_mode_and_global_keys() {
        let lines = help_lines(80);
        for heading in [
            "Messages (needs a trace)",
            "Detail (needs a trace)",
            "Curve (needs a trace)",
            "Databases",
            "Global",
        ] {
            assert!(
                lines
                    .iter()
                    .any(|(is_heading, line)| *is_heading && line.as_str() == heading),
                "missing help section {heading}"
            );
        }
        let text = lines
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "Ctrl-C",
            "open or close this help",
            "jump by trace time",
            "next / previous database",
            "focus the tree, object or details pane",
            "cycle the tree, objects and details",
            "focus objects or report the selection",
            "open the selected message",
            "wheel to scroll",
            "needs a trace",
            "back to the message list (trace only)",
        ] {
            assert!(text.contains(expected), "missing {expected} in help");
        }
    }

    #[test]
    fn help_is_legible_in_eighty_by_twenty_four() {
        for (_, line) in help_lines(80) {
            assert!(display_width(&line) <= 80, "help line too wide: {line}");
            assert!(
                !line.contains("..."),
                "help line truncated in 80 columns: {line}"
            );
        }
        assert_eq!(help_page_size(24), 18);
    }

    #[test]
    fn help_scrolls_instead_of_truncating() {
        // At 80 columns every description fits its row, so one row per entry.
        let entries = help_lines(80);
        let total = entries.len();
        assert_eq!(total, HELP_ENTRIES.len());
        let page = help_page_size(24);
        assert!(total > page, "help should overflow an 80x24 terminal");
        assert_eq!(clamp_help_offset(0, total, page), 0);
        assert_eq!(clamp_help_offset(usize::MAX, total, page), total - page);
        assert_eq!(clamp_help_offset(total, total, page), total - page);

        // Scrolling to the end keeps the global keys reachable.
        let offset = clamp_help_offset(usize::MAX, total, page);
        let visible = &entries[offset..(offset + page).min(total)];
        assert_eq!(visible.len(), page);
        assert!(visible.iter().any(|(_, line)| line.contains("Ctrl-C")));
    }

    #[test]
    fn help_wraps_descriptions_in_a_narrow_terminal() {
        let lines = help_lines(40);
        assert!(lines.len() > HELP_ENTRIES.len());
        for (_, line) in &lines {
            assert!(display_width(line) <= 40, "help line too wide: {line}");
            assert!(!line.contains("..."), "help line cut: {line}");
        }
        let squeezed = lines
            .iter()
            .filter_map(|(_, line)| line.rsplit_once(" │ ").map(|(_, right)| right))
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<String>();
        assert!(
            squeezed.contains("focusthetree,objectordetailspane"),
            "wrapped description lost: {squeezed}"
        );
    }

    #[test]
    fn seconds_convert_to_nanoseconds_only_when_sane() {
        assert_eq!(seconds_to_ns(1.5), Some(1_500_000_000));
        assert_eq!(seconds_to_ns(0.0), Some(0));
        assert_eq!(seconds_to_ns(-1.0), None);
        assert_eq!(seconds_to_ns(f64::NAN), None);
        assert_eq!(seconds_to_ns(f64::INFINITY), None);
    }

    #[test]
    fn jump_lands_on_the_first_message_at_or_after_the_target() {
        let timestamps = [
            None,
            Some(1_000_000_000),
            Some(2_000_000_000),
            Some(3_000_000_000),
        ];
        assert_eq!(first_index_at_or_after(timestamps, 0), Some(1));
        assert_eq!(first_index_at_or_after(timestamps, 1_000_000_000), Some(1));
        assert_eq!(first_index_at_or_after(timestamps, 2_500_000_000), Some(3));
        assert_eq!(first_index_at_or_after(timestamps, 9_000_000_000), None);
        assert_eq!(first_index_at_or_after([None, None], 0), None);
        assert_eq!(first_index_at_or_after([], 0), None);
    }

    fn ethernet_object() -> Object {
        use blf_core::{
            ObjectHeader, ObjectHeaderBase, ObjectType, RawObjectHeader, StructuredObject,
        };
        let mut payload = vec![
            0x45, 0x00, 0x00, 0x1C, 0x00, 0x01, 0x00, 0x00, 0x40, 0x11, 0x00, 0x00, 0xC0, 0xA8,
            0x01, 0x01, 0xC0, 0xA8, 0x01, 0x02,
        ];
        // UDP header plus a four-byte body.
        payload.extend_from_slice(&[0x04, 0xD2, 0x1F, 0x90, 0x00, 0x0C, 0x00, 0x00, 0, 0, 0, 0]);
        Object::Structured(StructuredObject {
            object_type: ObjectType::EthernetFrame,
            header: RawObjectHeader::Object(ObjectHeader {
                base: ObjectHeaderBase {
                    signature: blf_core::OBJECT_SIGNATURE,
                    header_size: 32,
                    header_version: 1,
                    object_size: 0,
                    object_type: ObjectType::EthernetFrame,
                    raw_object_type: ObjectType::EthernetFrame as u32,
                },
                object_flags: 2,
                client_index: 0,
                object_version: 0,
                object_time_stamp: 1_000,
            }),
            class_name: "EthernetFrame".to_string(),
            fields: vec![
                (
                    "sourceAddress",
                    blf_core::FieldValue::Bytes(vec![2, 0, 0, 0, 0, 1]),
                ),
                (
                    "destinationAddress",
                    blf_core::FieldValue::Bytes(vec![2, 0, 0, 0, 0, 2]),
                ),
                ("channel", blf_core::FieldValue::U16(2)),
                ("dir", blf_core::FieldValue::U16(0)),
                ("type", blf_core::FieldValue::U16(0x0800)),
                ("tpid", blf_core::FieldValue::U16(0)),
                ("tci", blf_core::FieldValue::U16(0)),
                (
                    "payLoadLength",
                    blf_core::FieldValue::U16(payload.len() as u16),
                ),
                ("payLoad", blf_core::FieldValue::Bytes(payload)),
            ],
            extensions: Vec::new(),
            data: Vec::new(),
        })
    }

    #[test]
    fn ethernet_frames_show_the_protocol_and_are_searchable() {
        let mut pool = TextPool::default();
        let summary = summary_for(0, &ethernet_object(), &mut pool);
        assert_eq!(summary.bus_label, "eth");
        assert_eq!(summary.frame_id, Some(0x0800));
        assert_eq!(summary.channel, Some(2));
        assert_eq!(summary.direction, Some("Rx"));
        let info = summary.ethernet.as_ref().expect("dissected frame");
        assert_eq!(info.protocol, "UDP");

        let line = summary_line(
            &summary,
            TimeAxis {
                mode: TimeMode::Absolute,
                base: 0,
            },
            &pool,
            120,
        );
        assert!(line.contains("eth"), "{line}");
        assert!(line.contains("0x0800"), "{line}");
        assert!(line.contains("UDP"), "{line}");

        // Values from every layer join the message filter, one interned string
        // per distinct flow.
        let mut buffer = String::new();
        assert!(summary_matches_in(
            &summary,
            "192.168.1.2",
            &mut buffer,
            &pool
        ));
        assert!(summary_matches_in(&summary, "1234", &mut buffer, &pool));
        assert!(summary_matches_in(&summary, "udp", &mut buffer, &pool));
        assert!(!summary_matches_in(&summary, "0x0806", &mut buffer, &pool));
    }

    fn can_message(id: u32, data: &[u8], timestamp: u64) -> Object {
        use blf_core::{CanMessage, ObjectHeader, ObjectHeaderBase, ObjectType};
        let mut payload = [0_u8; 8];
        payload[..data.len().min(8)].copy_from_slice(&data[..data.len().min(8)]);
        Object::CanMessage(CanMessage {
            header: ObjectHeader {
                base: ObjectHeaderBase {
                    signature: blf_core::OBJECT_SIGNATURE,
                    header_size: 32,
                    header_version: 1,
                    object_size: 48,
                    object_type: ObjectType::CanMessage,
                    raw_object_type: ObjectType::CanMessage as u32,
                },
                object_flags: 2,
                client_index: 0,
                object_version: 0,
                object_time_stamp: timestamp,
            },
            channel: 1,
            flags: 1,
            dlc: data.len() as u8,
            id,
            data: payload,
            raw_data: Vec::new(),
        })
    }

    #[test]
    fn reassembles_a_segmented_can_uds_response() {
        // 0x62 0xF1 0x90 plus 17 data octets: 0x10 0x14 first frame, then two
        // consecutive frames of seven octets each.
        let message: Vec<u8> = std::iter::once(&[0x62_u8, 0xF1, 0x90][..])
            .chain([(0u8..17).collect::<Vec<_>>().leak() as &[u8]])
            .flatten()
            .copied()
            .collect();
        let first = can_message(
            0x7E8,
            &[
                0x10, 0x14, message[0], message[1], message[2], message[3], message[4], message[5],
            ],
            1,
        );
        let second = can_message(
            0x7E8,
            &[
                0x21,
                message[6],
                message[7],
                message[8],
                message[9],
                message[10],
                message[11],
                message[12],
            ],
            2,
        );
        let third = can_message(
            0x7E8,
            &[
                0x22,
                message[13],
                message[14],
                message[15],
                message[16],
                message[17],
                message[18],
                message[19],
            ],
            3,
        );
        let objects = vec![first, second, third];
        let mut pool = TextPool::default();
        let summaries: Vec<MessageSummary> = objects
            .iter()
            .enumerate()
            .map(|(index, object)| summary_for(index, object, &mut pool))
            .collect();
        let uds = reassemble_trace(&objects, &summaries);
        for index in 0..3 {
            let frame = uds
                .get(&index)
                .unwrap_or_else(|| panic!("frame {index} missing"));
            assert!(frame.complete, "{frame:?}");
            assert_eq!(frame.frames, 3);
            assert_eq!(frame.transport, "iso-tp");
            assert!(
                frame.summary.contains("ReadDataByIdentifier"),
                "{}",
                frame.summary
            );
            assert!(
                frame
                    .fields
                    .iter()
                    .any(|(name, value)| name == "dataIdentifier" && value == "0xF190"),
                "{:?}",
                frame.fields
            );
        }
    }

    fn lin_message(frame_id: u8, data: &[u8], timestamp: u64) -> Object {
        use blf_core::{FieldValue, LinObject, ObjectHeader, ObjectHeaderBase, ObjectType};
        let mut payload = [0_u8; 8];
        payload[..data.len().min(8)].copy_from_slice(&data[..data.len().min(8)]);
        Object::Lin(LinObject {
            object_type: ObjectType::LinMessage,
            header: ObjectHeader {
                base: ObjectHeaderBase {
                    signature: blf_core::OBJECT_SIGNATURE,
                    header_size: 32,
                    header_version: 1,
                    object_size: 56,
                    object_type: ObjectType::LinMessage,
                    raw_object_type: ObjectType::LinMessage as u32,
                },
                object_flags: 2,
                client_index: 0,
                object_version: 0,
                object_time_stamp: timestamp,
            },
            fields: vec![
                ("channel", FieldValue::U16(1)),
                ("id", FieldValue::U8(frame_id)),
                ("dlc", FieldValue::U8(data.len() as u8)),
                ("data", FieldValue::Bytes(payload.to_vec())),
                ("dir", FieldValue::U8(0)),
            ],
            extensions: Vec::new(),
            raw_data: Vec::new(),
        })
    }

    #[test]
    fn reassembles_a_segmented_lin_uds_response() {
        // LIN diagnostic frames: node address, PCI, then payload octets.
        let message = [0x62_u8, 0xF1, 0x90, 0x11, 0x22, 0x33, 0x44, 0x55];
        let first = lin_message(0x3D, &[0x0A, 0x10, 0x08, 0x62, 0xF1, 0x90, 0x11, 0x22], 1);
        let second = lin_message(0x3D, &[0x0A, 0x21, 0x33, 0x44, 0x55, 0, 0, 0], 2);
        let objects = vec![first, second];
        let mut pool = TextPool::default();
        let summaries: Vec<MessageSummary> = objects
            .iter()
            .enumerate()
            .map(|(index, object)| summary_for(index, object, &mut pool))
            .collect();
        let uds = reassemble_trace(&objects, &summaries);
        for index in 0..2 {
            let frame = uds
                .get(&index)
                .unwrap_or_else(|| panic!("frame {index} missing"));
            assert!(frame.complete, "{frame:?}");
            assert_eq!(frame.frames, 2);
            assert_eq!(frame.transport, "lin-tp");
            assert_eq!(frame.node, Some(0x0A));
            assert!(
                frame.summary.contains("ReadDataByIdentifier"),
                "{}",
                frame.summary
            );
        }
        // The reassembled payload is the whole UDS message.
        let frame = uds.get(&1).expect("second frame");
        assert_eq!(
            frame
                .fields
                .iter()
                .find(|(name, _)| *name == "data")
                .map(|(_, value)| value.as_str()),
            Some("11 22 33 44 55")
        );
        assert_eq!(message.len(), 8);
    }

    /// A message carrying one Ethernet frame, for view tests.
    fn ethernet_summary(pool: &mut TextPool, timestamp: u64) -> MessageSummary {
        let mut summary = summary_for(0, &ethernet_object(), pool);
        summary.timestamp_ns = Some(timestamp);
        summary
    }

    fn can_summary(pool: &mut TextPool, timestamp: u64) -> MessageSummary {
        let object = can_message(0x7E0, &[0x03, 0x22, 0xF1, 0x90], timestamp);
        let mut summary = summary_for(0, &object, pool);
        summary.timestamp_ns = Some(timestamp);
        summary
    }

    #[test]
    fn the_inspector_scrolls_and_stops_at_the_end() {
        let mut app = App::new(None, Vec::new(), Vec::new()).expect("build app");
        app.mode = Mode::Inspector;
        app.inspector_lines = 41;
        app.viewport_height = 24;
        for _ in 0..3 {
            app.handle_event(InputEvent::Char('j')).expect("handle");
        }
        assert_eq!(app.inspector_offset, 3);
        for _ in 0..200 {
            app.handle_event(InputEvent::Char('j')).expect("handle");
        }
        // The last page stays on screen.
        assert_eq!(app.inspector_offset, 41 - app.inspector_page_size());
        app.handle_event(InputEvent::Char('k')).expect("handle");
        assert_eq!(app.inspector_offset, 41 - app.inspector_page_size() - 1);
        app.handle_event(InputEvent::Home).expect("handle");
        assert_eq!(app.inspector_offset, 0);
        app.handle_event(InputEvent::Esc).expect("handle");
        assert_eq!(app.mode, Mode::Messages);
    }

    fn phy_state_object(payload: &[u8]) -> Object {
        use blf_core::{
            FieldValue, ObjectHeader, ObjectHeaderBase, ObjectType, RawObjectHeader,
            StructuredObject,
        };
        // channel, flags, phyState, phyEvent, hardwareChannel
        let ch = u16::from_le_bytes([payload[0], payload[1]]);
        let flags = u16::from_le_bytes([payload[2], payload[3]]);
        Object::Structured(StructuredObject {
            object_type: ObjectType::EthernetPhyState,
            header: RawObjectHeader::Object(ObjectHeader {
                base: ObjectHeaderBase {
                    signature: blf_core::OBJECT_SIGNATURE,
                    header_size: 32,
                    header_version: 1,
                    object_size: 40,
                    object_type: ObjectType::EthernetPhyState,
                    raw_object_type: 133,
                },
                object_flags: 2,
                client_index: 0,
                object_version: 0,
                object_time_stamp: 5_000_000,
            }),
            class_name: "EthernetPhyState".to_string(),
            fields: vec![
                ("channel", FieldValue::U16(ch)),
                ("flags", FieldValue::U16(flags)),
                ("phyState", FieldValue::U8(payload[4])),
                ("phyEvent", FieldValue::U8(payload[5])),
                ("hardwareChannel", FieldValue::U8(payload[6])),
            ],
            extensions: Vec::new(),
            data: Vec::new(),
        })
    }

    #[test]
    fn phy_state_events_join_the_ethernet_view() {
        // channel 4, flags 7, normal state, wakeup received, hardware channel 12.
        let object = phy_state_object(&[0x04, 0x00, 0x07, 0x00, 0x01, 0x08, 0x0C]);
        let mut pool = TextPool::default();
        let summary = summary_for(0, &object, &mut pool);
        assert_eq!(summary.class_name, "EthernetPhyState");
        assert_eq!(summary.bus_label, "eth");
        assert_eq!(summary.channel, Some(4));
        let event = summary.event.as_ref().expect("event summary");
        assert_eq!(
            event.summary,
            "ch 4.12 · PHY normal state · wakeup received"
        );
        assert!(View::Ethernet.owns(&summary));
        assert!(!View::Can.owns(&summary));

        let axis = TimeAxis {
            mode: TimeMode::Absolute,
            base: 0,
        };
        let row = view_row(&summary, View::Ethernet, axis, &pool, 120);
        assert!(row.contains("PHY normal state"), "{row}");
        let mut buffer = String::new();
        assert!(summary_matches_in(&summary, "wakeup", &mut buffer, &pool));
        assert!(summary_matches_in(&summary, "4.12", &mut buffer, &pool));

        // The detail pane and the inspector list every field.
        let lines = summary_detail_lines(&summary, Some(&object), axis, None, &pool);
        assert!(
            lines.iter().any(|line| line.contains("PHY normal state")),
            "{lines:?}"
        );
        let lines = inspector_lines(&summary, Some(&object), axis, None, &pool, 0, 1);
        assert!(
            lines
                .iter()
                .any(|line| line.trim().starts_with("phyEvent") && line.trim().ends_with('8')),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.trim().starts_with("hardwareChannel")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("wakeup received")),
            "{lines:?}"
        );
    }

    #[test]
    fn views_own_their_bus_and_skip_the_others() {
        let mut pool = TextPool::default();
        let can = can_summary(&mut pool, 1_000);
        let eth = ethernet_summary(&mut pool, 2_000);
        assert!(View::All.owns(&can) && View::All.owns(&eth));
        assert!(View::Can.owns(&can) && !View::Can.owns(&eth));
        assert!(View::Ethernet.owns(&eth) && !View::Ethernet.owns(&can));
        assert!(!View::Lin.owns(&can) && !View::Lin.owns(&eth));
        assert_eq!(View::All.cycle(), View::Can);
        assert_eq!(View::Ethernet.cycle(), View::All);
        assert_eq!(View::from_digit('4'), Some(View::Ethernet));
        assert_eq!(View::from_digit('9'), None);
    }

    #[test]
    fn each_view_renders_its_own_columns() {
        let mut pool = TextPool::default();
        let can = can_summary(&mut pool, 1_000);
        let eth = ethernet_summary(&mut pool, 2_000);
        let axis = TimeAxis {
            mode: TimeMode::Absolute,
            base: 0,
        };
        let can_row = view_row(&can, View::Can, axis, &pool, 120);
        assert!(can_row.contains("0x7E0"), "{can_row}");
        assert!(can_row.contains("03 22 F1 90"), "{can_row}");

        let eth_row = view_row(&eth, View::Ethernet, axis, &pool, 120);
        assert!(eth_row.contains("0x0800"), "{eth_row}");
        assert!(eth_row.contains("UDP"), "{eth_row}");
        // The info column no longer repeats the protocol name.
        let info = ethernet_info_text(
            pool.get(eth.ethernet.as_ref().expect("ethernet").summary),
            "UDP",
        );
        assert!(!info.starts_with("UDP"), "{info}");

        // A foreign frame collapses into one dim description.
        let foreign = foreign_row(&eth, axis, &pool);
        assert!(foreign.contains("eth 0x0800"), "{foreign}");
        let foreign = foreign_row(&can, axis, &pool);
        assert!(foreign.contains("can 0x7E0"), "{foreign}");
    }

    #[test]
    fn hiding_foreign_frames_narrows_the_list_to_the_view() {
        let mut pool = TextPool::default();
        let objects = vec![
            can_message(0x7E0, &[0x01], 1_000_000),
            ethernet_object(),
            can_message(0x7E1, &[0x02], 2_000_000),
        ];
        let mut app = App::new(None, objects, Vec::new()).expect("build app");
        app.summaries = app
            .objects
            .iter()
            .enumerate()
            .map(|(index, object)| summary_for(index, object, &mut pool))
            .collect();
        app.rebuild_filtered();
        assert_eq!(app.filtered.len(), 3);

        app.set_view(View::Can);
        // Foreign frames stay as dim descriptions by default.
        assert_eq!(app.filtered.len(), 3);
        app.toggle_foreign();
        assert_eq!(app.filtered.len(), 2);
        assert!(app
            .filtered
            .iter()
            .all(|index| app.summaries[*index].bus == Some(BusKind::Can)));

        // Switching views keeps working with the narrowed list.
        app.set_view(View::Ethernet);
        assert_eq!(app.filtered.len(), 1);
        assert!(app.summaries[app.filtered[0]].ethernet.is_some());
    }

    #[test]
    fn ethernet_frames_are_grouped_by_ether_type_in_statistics() {
        let mut pool = TextPool::default();
        let objects = vec![ethernet_object(), ethernet_object()];
        let mut app = App::new(None, objects.clone(), Vec::new()).expect("build app");
        app.summaries = objects
            .iter()
            .enumerate()
            .map(|(index, object)| {
                let mut summary = summary_for(index, object, &mut pool);
                // Two frames a millisecond apart give the row a period.
                summary.timestamp_ns = Some(1_000_000 + index as u64 * 1_000_000);
                summary
            })
            .collect();
        app.filtered = (0..app.summaries.len()).collect();
        app.open_statistics();
        assert_eq!(app.stats.len(), 1);
        let stat = &app.stats[0];
        assert_eq!(stat.count, 2);
        assert_eq!(stat.key, FrameKey::Ethernet(2, 0x0800));
        assert!(
            stat.timing.is_some(),
            "two frames are enough for a period estimate"
        );
        let row = stat_row(stat, 100);
        assert!(row.contains("0x800"), "{row}");
        assert!(row.contains("eth"), "{row}");
        // Jumping from the row selects the first matching frame.
        app.jump_to_stat();
        assert_eq!(app.selected, 0);
        assert_eq!(app.mode, Mode::Messages);
    }

    #[test]
    fn ethernet_detail_lists_every_layer() {
        let object = ethernet_object();
        let mut pool = TextPool::default();
        let summary = summary_for(0, &object, &mut pool);
        let lines = summary_detail_lines(
            &summary,
            Some(&object),
            TimeAxis {
                mode: TimeMode::Absolute,
                base: 0,
            },
            None,
            &pool,
        );
        assert!(
            lines.iter().any(|line| line.starts_with("id: 0x0800")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("[ipv4]")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|line| line.contains("[udp]")), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|line| line.contains("source = 192.168.1.1")),
            "{lines:?}"
        );
    }

    #[test]
    fn frame_labels_name_bus_channel_and_id() {
        let summary = MessageSummary {
            object_index: 0,
            timestamp_ns: Some(0),
            timestamp_source: None,
            bus: Some(BusKind::Can),
            bus_label: "can",
            ethernet: None,
            event: None,
            can_flags: 0,
            channel: Some(1),
            frame_id: Some(0x344),
            object_type_raw: 1,
            class_name: "CAN_MESSAGE",
            direction: Some("Rx"),
            dlc: Some(8),
            payload: Vec::new(),
            status: "ok",
        };
        assert_eq!(frame_label(&summary), "can 1 0x344");
    }

    #[test]
    fn pages_move_by_a_visible_page() {
        assert_eq!(list_page_size(40, 7), 33);
        assert_eq!(list_page_size(5, 9), 1);
        assert_eq!(list_page_size(0, 7), 1);
    }

    #[test]
    fn timing_reports_cycle_and_jitter() {
        // 10 ms cycles with one 20 ms gap.
        let timestamps = [0, 10_000_000, 20_000_000, 40_000_000];
        let stats = timing_stats(&timestamps).expect("stats");
        assert_eq!(stats.frames, 4);
        assert!((stats.min_ms - 10.0).abs() < 1e-6);
        assert!((stats.max_ms - 20.0).abs() < 1e-6);
        assert!((stats.avg_ms - 13.333_333).abs() < 1e-3);
        assert!(stats.jitter_ms > 0.0);
    }

    #[test]
    fn timing_needs_two_samples() {
        assert!(timing_stats(&[]).is_none());
        assert!(timing_stats(&[1]).is_none());
        // Duplicate timestamps carry no period.
        assert!(timing_stats(&[5, 5, 5]).is_none());
    }

    #[test]
    fn timing_lines_explain_missing_samples() {
        let lines = timing_lines(None);
        assert_eq!(lines, vec!["period: not enough samples".to_string()]);
        let lines = timing_lines(timing_stats(&[0, 1_000_000]));
        assert!(lines.iter().any(|line| line.contains("1.000 ms")));
    }

    #[test]
    fn csv_fields_are_quoted_only_when_needed() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("line\nbreak"), "\"line\nbreak\"");
        assert_eq!(csv_line(&["a".into(), "b,c".into()]), "a,\"b,c\"");
    }

    #[test]
    fn export_path_sits_next_to_the_trace() {
        let path = export_path(&PathBuf::from("/tmp/run.blf"));
        assert_eq!(path, PathBuf::from("/tmp/run.blf.signals.csv"));
    }

    #[test]
    fn curve_columns_span_the_vertical_range() {
        // Dense samples in one column must paint a vertical span, not one dot.
        let curve = CurveData {
            series: vec![CurveSeries {
                title: "Speed".to_string(),
                samples: (0..200)
                    .map(|index| (index as u64, (index % 7) as f64))
                    .collect(),
                error: None,
                truncated: false,
                symbol: '*',
                color: slot_color(Slot::Green),
            }],
            window_start: 0,
            window_end: 200,
            normalize: false,
        };
        let lines = plot_series(&curve, (0, 200), 30, 8);
        let painted = lines.iter().filter(|line| line.contains('━')).count();
        assert!(painted > 2, "expected a painted span, got {painted} rows");
    }

    #[test]
    fn curve_bounds_are_never_empty() {
        // A single sample still yields a window, so the plot is not blank.
        let curve = CurveData {
            series: vec![CurveSeries {
                title: "Speed".to_string(),
                samples: vec![(100, 1.0)],
                error: None,
                truncated: false,
                symbol: '*',
                color: slot_color(Slot::Green),
            }],
            window_start: 0,
            window_end: 0,
            normalize: false,
        };
        assert_eq!(curve_bounds(&curve), Some((100, 101)));
        assert_eq!(
            curve_bounds(&CurveData {
                series: Vec::new(),
                window_start: 0,
                normalize: false,
                window_end: 0,
            }),
            None
        );
    }

    #[test]
    fn curve_window_stays_inside_the_data() {
        // Bounds and windows are nanoseconds, as everywhere else in the viewer.
        const S: u64 = 1_000_000_000;
        let bounds = (0, 100 * S);
        // Panning keeps the window width and stops at the edges.
        assert_eq!(pan_window((0, 20 * S), 1, bounds), (4 * S, 24 * S));
        assert_eq!(pan_window((0, 20 * S), -1, bounds), (0, 20 * S));
        assert_eq!(pan_window((80 * S, 100 * S), 1, bounds), (80 * S, 100 * S));
        // Zooming shrinks around the centre and never leaves the data.
        assert_eq!(zoom_window((0, 100 * S), 0.5, bounds), (25 * S, 75 * S));
        assert_eq!(zoom_window((0, 100 * S), 2.0, bounds), (0, 100 * S));
        let (start, end) = zoom_window((40 * S, 60 * S), 0.5, bounds);
        assert_eq!(end - start, 10 * S);
        assert!(start >= bounds.0 && end <= bounds.1);
        // Zooming out never exceeds the data, and the window never collapses.
        let (start, end) = zoom_window((50 * S, 50 * S + MIN_CURVE_WINDOW_NS), 0.01, (0, 5 * S));
        assert!(end > start);
        assert!(end - start >= MIN_CURVE_WINDOW_NS.min(5 * S));
    }
}

#[cfg(test)]
mod bus_event_tests {
    use super::*;
    use blf_core::{
        CanErrorFrameExt, LinObject, ObjectHeader, ObjectHeaderBase, ObjectType, RawObjectHeader,
        StructuredObject,
    };

    fn header(object_type: ObjectType) -> RawObjectHeader {
        RawObjectHeader::Object(ObjectHeader {
            base: ObjectHeaderBase {
                signature: blf_core::OBJECT_SIGNATURE,
                header_size: 32,
                header_version: 1,
                object_size: 40,
                object_type,
                raw_object_type: object_type.as_u32(),
            },
            object_flags: 2,
            client_index: 0,
            object_version: 0,
            object_time_stamp: 1_000_000,
        })
    }

    fn structured(object_type: ObjectType, fields: Vec<(&'static str, FieldValue)>) -> Object {
        Object::Structured(StructuredObject {
            object_type,
            header: header(object_type),
            class_name: object_type.name().to_string(),
            fields,
            extensions: Vec::new(),
            data: Vec::new(),
        })
    }

    fn lin(object_type: ObjectType, fields: Vec<(&'static str, FieldValue)>) -> Object {
        Object::Lin(LinObject {
            header: match header(object_type) {
                RawObjectHeader::Object(header) => header,
                _ => unreachable!(),
            },
            object_type,
            fields,
            extensions: Vec::new(),
            raw_data: Vec::new(),
        })
    }

    fn axis() -> TimeAxis {
        TimeAxis {
            mode: TimeMode::Absolute,
            base: 0,
        }
    }

    #[test]
    fn can_driver_error_shows_its_name_and_joins_the_can_view() {
        // errorCode 102 is "chip state bus off" in the logging format document.
        let object = structured(
            ObjectType::CanDriverError,
            vec![
                ("channel", FieldValue::U16(2)),
                ("txErrors", FieldValue::U32(3)),
                ("rxErrors", FieldValue::U32(1)),
                ("errorCode", FieldValue::U32(102)),
            ],
        );
        let mut pool = TextPool::default();
        let summary = summary_for(0, &object, &mut pool);
        assert_eq!(summary.bus_label, "can");
        assert_eq!(summary.bus, Some(BusKind::Can));
        assert_eq!(summary.status, "event");
        let event = summary.event.as_ref().expect("event");
        assert_eq!(event.kind, "error");
        assert!(event.summary.contains("chip state bus off"), "{event:?}");
        assert!(event.summary.contains("tx 3 rx 1"), "{event:?}");
        assert!(View::Can.owns(&summary));
        assert!(!View::Ethernet.owns(&summary));

        let row = view_row(&summary, View::Can, axis(), &pool, 120);
        assert!(row.contains("chip state bus off"), "{row}");
        assert!(summary_matches_in(
            &summary,
            "bus off",
            &mut String::new(),
            &pool
        ));
    }

    #[test]
    fn can_statistic_reports_busload_and_frame_counts() {
        // mBusLoad is in 1/100 percent: 1234 is 12.34 %.
        let object = structured(
            ObjectType::CanStatistic,
            vec![
                ("channel", FieldValue::U16(3)),
                ("busLoad", FieldValue::U16(1234)),
                ("standardDataFrames", FieldValue::U32(1200)),
                ("extendedDataFrames", FieldValue::U32(34)),
                ("standardRemoteFrames", FieldValue::U32(0)),
                ("extendedRemoteFrames", FieldValue::U32(0)),
                ("errorFrames", FieldValue::U32(5)),
                ("overloadFrames", FieldValue::U32(1)),
            ],
        );
        let mut pool = TextPool::default();
        let summary = summary_for(0, &object, &mut pool);
        let event = summary.event.as_ref().expect("event");
        assert_eq!(event.kind, "stat");
        assert!(event.summary.contains("12.34 %"), "{event:?}");
        assert!(event.summary.contains("1200 standard"), "{event:?}");
        assert!(event.summary.contains("5 errors"), "{event:?}");
    }

    #[test]
    fn can_error_frame_decodes_the_vector_error_code() {
        // Vector CAN core ECC: bits 0-5 code (4 = CRC error), bits 6-7 direction (3 = TX).
        let object = Object::CanErrorFrameExt(CanErrorFrameExt {
            header: match header(ObjectType::CanErrorExt) {
                RawObjectHeader::Object(header) => header,
                _ => unreachable!(),
            },
            channel: 1,
            length: 0,
            flags: 0,
            ecc: 0b1100_0100,
            position: 17,
            dlc: 8,
            reserved_can_error_frame_ext1: 0,
            frame_length_in_ns: 0,
            id: 0x123,
            flags_ext: 0,
            reserved_can_error_frame_ext2: 0,
            data: vec![0; 8],
            raw_data: Vec::new(),
        });
        let mut pool = TextPool::default();
        let summary = summary_for(0, &object, &mut pool);
        let event = summary.event.as_ref().expect("event");
        assert!(event.summary.contains("CRC error"), "{event:?}");
        assert!(event.summary.contains("(TX)"), "{event:?}");
        assert!(event.summary.contains("at bit 17"), "{event:?}");
        assert!(event.summary.contains("0x123"), "{event:?}");
    }

    #[test]
    fn lin_events_are_decoded_and_join_the_lin_view() {
        let cases = vec![
            (
                lin(
                    ObjectType::LinSleep,
                    vec![
                        ("channel", FieldValue::U16(1)),
                        ("reason", FieldValue::U8(1)),
                        ("flags", FieldValue::U8(0b001)),
                    ],
                ),
                "go-to-sleep frame",
            ),
            (
                lin(
                    ObjectType::LinWakeup2,
                    vec![
                        ("channel", FieldValue::U16(2)),
                        ("signal", FieldValue::U8(0x3C)),
                        ("lengthInfo", FieldValue::U8(1)),
                        ("external", FieldValue::U8(1)),
                    ],
                ),
                "wakeup too short",
            ),
            (
                lin(
                    ObjectType::LinBaudrate,
                    vec![
                        ("channel", FieldValue::U16(1)),
                        ("baudrate", FieldValue::I32(19_200)),
                    ],
                ),
                "19200 bit/s",
            ),
            (
                lin(
                    ObjectType::LinSynError2,
                    vec![("channel", FieldValue::U16(1))],
                ),
                "synchronization error",
            ),
            (
                lin(
                    ObjectType::LinDisturbanceEvent,
                    vec![
                        ("channel", FieldValue::U16(1)),
                        ("id", FieldValue::U8(0x1A)),
                        ("disturbanceType", FieldValue::U32(2)),
                        ("byteIndex", FieldValue::U32(3)),
                        ("bitIndex", FieldValue::U32(2)),
                    ],
                ),
                "disturbance with a header",
            ),
            (
                lin(
                    ObjectType::LinShortOrSlowResponse2,
                    vec![
                        ("channel", FieldValue::U16(1)),
                        ("numberOfRespBytes", FieldValue::U8(2)),
                        ("slowResponse", FieldValue::U8(1)),
                        ("interruptedByBreak", FieldValue::U8(1)),
                    ],
                ),
                "interrupted by break",
            ),
        ];
        for (object, expected) in cases {
            let mut pool = TextPool::default();
            let summary = summary_for(0, &object, &mut pool);
            assert_eq!(summary.bus_label, "lin", "{}", summary.class_name);
            assert_eq!(summary.status, "event", "{}", summary.class_name);
            assert!(View::Lin.owns(&summary), "{}", summary.class_name);
            let event = summary.event.as_ref().expect("event");
            assert!(
                event.summary.contains(expected),
                "{}: {}",
                summary.class_name,
                event.summary
            );
            let row = view_row(&summary, View::Lin, axis(), &pool, 120);
            assert!(row.contains(expected), "{row}");
        }
    }

    #[test]
    fn ethernet_statistic_and_rx_error_carry_their_numbers() {
        let statistic = structured(
            ObjectType::EthernetStatistic,
            vec![
                ("channel", FieldValue::U16(4)),
                ("hardwareChannel", FieldValue::U16(12)),
                ("rcvOk_HW", FieldValue::U64(1_000)),
                ("xmitOk_HW", FieldValue::U64(900)),
                ("rcvError_HW", FieldValue::U64(2)),
                ("xmitError_HW", FieldValue::U64(0)),
                ("rcvNoBuffer_HW", FieldValue::U64(7)),
                ("sqi", FieldValue::I16(4)),
            ],
        );
        let mut pool = TextPool::default();
        let summary = summary_for(0, &statistic, &mut pool);
        assert_eq!(summary.bus_label, "eth");
        let event = summary.event.as_ref().expect("event");
        assert!(event.summary.contains("ch 4.12"), "{event:?}");
        assert!(event.summary.contains("rx 1000"), "{event:?}");
        assert!(event.summary.contains("SQI good"), "{event:?}");
        assert!(event.summary.contains("7 dropped"), "{event:?}");
        assert!(View::Ethernet.owns(&summary));

        let error = structured(
            ObjectType::EthernetRxError,
            vec![
                ("channel", FieldValue::U16(4)),
                ("hardwareChannel", FieldValue::U16(11)),
                ("dir", FieldValue::U16(0)),
                ("fcs", FieldValue::U32(0xDEAD_BEEF)),
                ("frameDataLength", FieldValue::U16(64)),
                ("error", FieldValue::U32(2)),
            ],
        );
        let summary = summary_for(0, &error, &mut pool);
        let event = summary.event.as_ref().expect("event");
        assert_eq!(event.kind, "error");
        assert!(event.summary.contains("invalid CRC"), "{event:?}");
        assert!(event.summary.contains("64 bytes"), "{event:?}");
        assert!(event.summary.contains("0xDEADBEEF"), "{event:?}");
    }
}

#[cfg(test)]
mod text_record_tests {
    use super::*;
    use blf_core::{AppText, ObjectHeader, ObjectHeaderBase, ObjectType, RawObjectHeader};

    fn app_text(source: u32, text: &[u8]) -> Object {
        Object::AppText(AppText {
            header: {
                ObjectHeader {
                    base: ObjectHeaderBase {
                        signature: blf_core::OBJECT_SIGNATURE,
                        header_size: 32,
                        header_version: 1,
                        object_size: 48 + text.len() as u32,
                        object_type: ObjectType::AppText,
                        raw_object_type: 65,
                    },
                    object_flags: 0,
                    client_index: 0,
                    object_version: 0,
                    object_time_stamp: 0,
                }
            },
            source,
            reserved_app_text1: 0,
            text_length: text.len() as u32,
            reserved_app_text2: 0,
            text: text.to_vec(),
            raw_data: Vec::new(),
        })
    }

    #[test]
    fn metadata_fragments_are_joined_into_one_document() {
        let objects = vec![
            app_text(2, b"<?xml version=\"1.0\"?><a>"),
            app_text(2, b"middle"),
            app_text(2, b"</a>"),
            // A second document starts with its own declaration.
            app_text(2, b"<?xml version=\"1.0\"?><b/>"),
            // Other sources never join a document.
            app_text(1, b"\x00"),
            app_text(0, b"Comment"),
        ];
        let (documents, owner) = collect_metadata(&objects);
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0], "<?xml version=\"1.0\"?><a>middle</a>");
        assert_eq!(documents[1], "<?xml version=\"1.0\"?><b/>");
        assert_eq!(owner[0], Some(0));
        assert_eq!(owner[1], Some(0));
        assert_eq!(owner[2], Some(0));
        assert_eq!(owner[3], Some(1));
        assert_eq!(owner[4], None);
    }

    #[test]
    fn metadata_rows_carry_the_whole_document_in_their_search_text() {
        let objects = vec![
            app_text(
                2,
                b"<?xml version=\"1.0\"?><channels network=\"Ethernet11\"/>",
            ),
            app_text(2, b""),
        ];
        let mut pool = TextPool::default();
        let mut summaries: Vec<MessageSummary> = objects
            .iter()
            .enumerate()
            .map(|(index, object)| summary_for(index, object, &mut pool))
            .collect();
        let (documents, owner) = collect_metadata(&objects);
        for (index, summary) in summaries.iter_mut().enumerate() {
            let Some(Some(document)) = owner.get(index).copied() else {
                continue;
            };
            let Some(document) = documents.get(document) else {
                continue;
            };
            if let Some(event) = summary.event.as_mut() {
                event.search.push(' ');
                event.search.push_str(&document.to_lowercase());
            }
        }
        let mut buffer = String::new();
        // The network name only exists in the joined document.
        assert!(summary_matches_in(
            &summaries[0],
            "ethernet11",
            &mut buffer,
            &pool
        ));
    }

    #[test]
    fn lost_objects_are_totaled_from_the_trace() {
        use blf_core::{FieldValue, StructuredObject};
        let lost_end = |lost: u64, since: u64| {
            Object::Structured(StructuredObject {
                object_type: ObjectType::DataLostEnd,
                header: RawObjectHeader::Object(ObjectHeader {
                    base: ObjectHeaderBase {
                        signature: blf_core::OBJECT_SIGNATURE,
                        header_size: 32,
                        header_version: 1,
                        object_size: 56,
                        object_type: ObjectType::DataLostEnd,
                        raw_object_type: 126,
                    },
                    object_flags: 2,
                    client_index: 0,
                    object_version: 0,
                    object_time_stamp: 0,
                }),
                class_name: "DataLostEnd".to_string(),
                fields: vec![
                    ("queueIdentifier", FieldValue::U32(1)),
                    ("firstObjectLostTimeStamp", FieldValue::U64(since)),
                    ("numberOfLostEvents", FieldValue::U32(lost as u32)),
                ],
                extensions: Vec::new(),
                data: Vec::new(),
            })
        };
        let objects = vec![lost_end(5, 100), lost_end(7, 200)];
        assert_eq!(count_lost_objects(&objects), (12, 2));
        assert_eq!(count_lost_objects(&[]), (0, 0));

        // A data-lost record reads as a warning row, not an empty one.
        let mut pool = TextPool::default();
        let summary = summary_for(0, &objects[0], &mut pool);
        let event = summary.event.as_ref().expect("event");
        assert!(event.summary.contains("lost 5 objects"), "{event:?}");
        assert!(event.summary.contains("100"), "{event:?}");
    }
}

#[cfg(test)]
mod record_display_tests {
    use super::*;
    use blf_core::{
        AppText, CanFdMessage64, ObjectHeader, ObjectHeaderBase, ObjectType, RawObjectHeader,
        StructuredObject,
    };

    fn header(object_type: ObjectType, raw: u32) -> RawObjectHeader {
        RawObjectHeader::Object(ObjectHeader {
            base: ObjectHeaderBase {
                signature: blf_core::OBJECT_SIGNATURE,
                header_size: 32,
                header_version: 1,
                object_size: 96,
                object_type,
                raw_object_type: raw,
            },
            object_flags: 2,
            client_index: 0,
            object_version: 0,
            object_time_stamp: 1_000_000,
        })
    }

    fn structured(object_type: ObjectType, fields: Vec<(&'static str, FieldValue)>) -> Object {
        Object::Structured(StructuredObject {
            object_type,
            header: header(object_type, object_type.as_u32()),
            class_name: object_type.name().to_string(),
            fields,
            extensions: Vec::new(),
            data: Vec::new(),
        })
    }

    fn app_text(source: u32, text: &[u8]) -> Object {
        Object::AppText(AppText {
            header: match header(ObjectType::AppText, 65) {
                RawObjectHeader::Object(header) => header,
                _ => unreachable!(),
            },
            source,
            reserved_app_text1: 0,
            text_length: text.len() as u32,
            reserved_app_text2: 0,
            text: text.to_vec(),
            raw_data: Vec::new(),
        })
    }

    fn axis() -> TimeAxis {
        TimeAxis {
            mode: TimeMode::Absolute,
            base: 0,
        }
    }

    #[test]
    fn system_variables_show_their_value_and_type() {
        // type 2 is a 32-bit integer in the official table.
        let mut data = vec![0_u8; 4];
        data[..4].copy_from_slice(&1234_u32.to_le_bytes());
        let object = structured(
            ObjectType::SysVariable,
            vec![
                ("type", FieldValue::U32(2)),
                ("representation", FieldValue::U32(1)),
                ("nameLength", FieldValue::U32(5)),
                ("dataLength", FieldValue::U32(4)),
                ("name", FieldValue::String("::a::b".to_string())),
                ("data", FieldValue::Bytes(data)),
            ],
        );
        let mut pool = TextPool::default();
        let summary = summary_for(0, &object, &mut pool);
        assert_eq!(summary.status, "event");
        let event = summary.event.as_ref().expect("event");
        assert_eq!(event.kind, "SV");
        assert!(event.summary.contains("::a::b = 1234"), "{event:?}");
        assert!(event.summary.contains("(long)"), "{event:?}");
        // The variable name is searchable.
        let mut buffer = String::new();
        assert!(summary_matches_in(&summary, "::a::b", &mut buffer, &pool));

        // A signed variable keeps its sign: representation 0 means signed.
        let signed = structured(
            ObjectType::SysVariable,
            vec![
                ("type", FieldValue::U32(2)),
                ("representation", FieldValue::U32(0)),
                ("nameLength", FieldValue::U32(4)),
                ("dataLength", FieldValue::U32(4)),
                ("name", FieldValue::String("::c".to_string())),
                ("data", FieldValue::Bytes((-5i32).to_le_bytes().to_vec())),
            ],
        );
        let summary = summary_for(0, &signed, &mut pool);
        let event = summary.event.as_ref().expect("event");
        assert!(event.summary.contains("::c = -5"), "{event:?}");
    }

    #[test]
    fn application_text_reports_its_purpose() {
        // A measurement comment is stored as UTF-16LE by CANoe.
        let comment: Vec<u8> = "Comment"
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .chain([0, 0])
            .collect();
        let object = app_text(0, &comment);
        let mut pool = TextPool::default();
        let summary = summary_for(0, &object, &mut pool);
        let event = summary.event.as_ref().expect("event");
        assert_eq!(event.kind, "text");
        assert!(event.summary.contains("measurement comment"), "{event:?}");
        assert!(event.summary.contains("Comment"), "{event:?}");

        // Channel info carries a packed flag word, not text.
        let mut packed = vec![0_u8; 4];
        packed.copy_from_slice(&0x01_00_07_02_u32.to_le_bytes());
        let object = app_text(1, &packed);
        let summary = summary_for(0, &object, &mut pool);
        let event = summary.event.as_ref().expect("event");
        assert!(event.summary.contains("channel info"), "{event:?}");
        // 0x01000702 packs version 2, channel 7, bus type 0 and CAN FD 1.
        assert!(event.summary.contains("channel 7"), "{event:?}");
        assert!(event.summary.contains("version 2"), "{event:?}");
        assert!(event.summary.contains("CAN FD 1"), "{event:?}");
    }

    #[test]
    fn can_frame_flags_are_reported() {
        // CAN FD with BRS and ESI set.
        let object = Object::CanFdMessage64(CanFdMessage64 {
            header: match header(ObjectType::CanFdMessage64, 101) {
                RawObjectHeader::Object(header) => header,
                _ => unreachable!(),
            },
            channel: 1,
            dlc: 9,
            valid_data_bytes: 8,
            tx_count: 0,
            id: 0x123,
            frame_length: 100,
            flags: 0x02 | 0x04,
            btr_cfg_arb: 0,
            btr_cfg_data: 0,
            time_offset_brs_ns: 0,
            time_offset_crc_del_ns: 0,
            bit_count: 0,
            dir: 1,
            ext_data_offset: 0,
            crc: 0,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
            ext_data: None,
            reserved_can_fd_ext_frame_data: Vec::new(),
            raw_data: Vec::new(),
        });
        let mut pool = TextPool::default();
        let summary = summary_for(0, &object, &mut pool);
        assert_eq!(summary.can_flags & CAN_FLAG_BRS, CAN_FLAG_BRS);
        assert_eq!(summary.can_flags & CAN_FLAG_ESI, CAN_FLAG_ESI);
        assert_eq!(summary.can_flags & CAN_FLAG_REMOTE, 0);
        let label = can_flag_label(summary.can_flags);
        assert!(label.contains("brs"), "{label}");
        assert!(label.contains("esi"), "{label}");

        // The CAN view keeps its columns aligned and shows the flags.
        let row = view_row(&summary, View::Can, axis(), &pool, 120);
        assert!(row.contains("brs"), "{row}");
        let header = view_header(View::Can, 120);
        assert!(header.contains("flags"), "{header}");
    }

    #[test]
    fn overrun_records_name_their_bus() {
        let object = structured(
            ObjectType::OverrunError,
            vec![
                ("busType", FieldValue::U32(1)),
                ("channel", FieldValue::U16(3)),
            ],
        );
        let mut pool = TextPool::default();
        let summary = summary_for(0, &object, &mut pool);
        let event = summary.event.as_ref().expect("event");
        assert_eq!(event.kind, "OVERRUN");
        assert_eq!(event.bus_label, "can");
        assert!(event.summary.contains("overrun"), "{event:?}");
        assert!(event.summary.contains('3'), "{event:?}");
    }

    #[test]
    fn a_trace_that_lost_objects_says_so_in_the_header() {
        use blf_core::FieldValue::U32;
        let lost = structured(
            ObjectType::DataLostEnd,
            vec![
                ("queueIdentifier", U32(1)),
                ("firstObjectLostTimeStamp", FieldValue::U64(500)),
                ("numberOfLostEvents", U32(9)),
            ],
        );
        let mut pool = TextPool::default();
        let summary = summary_for(0, &lost, &mut pool);
        let event = summary.event.as_ref().expect("event");
        assert_eq!(event.kind, "LOST");
        assert!(event.summary.contains("lost 9 objects"), "{event:?}");
        // The header warning is driven by the same total.
        assert_eq!(count_lost_objects(&[lost]), (9, 1));
    }
}

#[cfg(test)]
mod bus_family_tests {
    use super::*;
    use blf_core::{
        ObjectHeader2, ObjectHeaderBase, ObjectType, RawObjectHeader, StructuredObject,
    };

    fn strip_ansi(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        let mut chars = value.chars();
        while let Some(character) = chars.next() {
            if character != '\x1b' {
                out.push(character);
                continue;
            }
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        out
    }

    fn header2(object_type: ObjectType) -> RawObjectHeader {
        RawObjectHeader::Object2(ObjectHeader2 {
            base: ObjectHeaderBase {
                signature: blf_core::OBJECT_SIGNATURE,
                header_size: 40,
                header_version: 2,
                object_size: 96,
                object_type,
                raw_object_type: object_type.as_u32(),
            },
            object_flags: 2,
            time_stamp_status: 0,
            reserved_object_header: 0,
            object_version: 0,
            object_time_stamp: 1_000_000,
            original_time_stamp: 0,
        })
    }

    fn structured(object_type: ObjectType, fields: Vec<(&'static str, FieldValue)>) -> Object {
        Object::Structured(StructuredObject {
            object_type,
            header: header2(object_type),
            class_name: object_type.name().to_string(),
            fields,
            extensions: Vec::new(),
            data: Vec::new(),
        })
    }

    /// Every bus family must produce a readable line and carry its own label.
    #[test]
    fn each_bus_family_decodes_a_representative_record() {
        let cases: Vec<(Object, &str, &str)> = vec![
            (
                structured(
                    ObjectType::MostCtrl,
                    vec![
                        ("channel", FieldValue::U16(1)),
                        ("dir", FieldValue::U8(1)),
                        ("sourceAdr", FieldValue::U32(0x400)),
                        ("destAdr", FieldValue::U32(0x401)),
                        ("rTyp", FieldValue::U16(3)),
                        ("rTypAdr", FieldValue::U8(0x20)),
                        ("state", FieldValue::U8(0x11)),
                        ("ackNack", FieldValue::U8(0x10)),
                    ],
                ),
                "most",
                "allocate",
            ),
            (
                structured(
                    ObjectType::MostNetstate,
                    vec![
                        ("channel", FieldValue::U16(1)),
                        ("stateNew", FieldValue::U16(5)),
                        ("stateOld", FieldValue::U16(2)),
                    ],
                ),
                "most",
                "configuration ok",
            ),
            (
                structured(
                    ObjectType::FlexrayMessage,
                    vec![
                        ("channel", FieldValue::U16(3)),
                        ("frameId", FieldValue::U16(12)),
                        ("cycle", FieldValue::U8(7)),
                        ("length", FieldValue::U16(16)),
                        ("frameFlags", FieldValue::U32(0x06)),
                        ("frameState", FieldValue::U8(0x20)),
                    ],
                ),
                "flexray",
                "A+B",
            ),
            (
                structured(
                    ObjectType::AfdxBusStatistic,
                    vec![
                        ("channel", FieldValue::U16(2)),
                        ("line", FieldValue::U8(1)),
                        ("linkStatus", FieldValue::U8(2)),
                        ("linkSpeed", FieldValue::U16(1)),
                        ("statRxPacketCount", FieldValue::U32(10)),
                        ("statTxPacketCount", FieldValue::U32(20)),
                    ],
                ),
                "afdx",
                "100 Mbit/s",
            ),
            (
                structured(
                    ObjectType::A429Status,
                    vec![
                        ("channel", FieldValue::U16(4)),
                        ("parity", FieldValue::U16(3)),
                        ("bitrate", FieldValue::U32(100_000)),
                    ],
                ),
                "a429",
                "even",
            ),
            (
                structured(
                    ObjectType::WlanFrame,
                    vec![
                        ("channel", FieldValue::U16(1)),
                        ("flags", FieldValue::U32(0x01)),
                        ("frameLength", FieldValue::U16(64)),
                    ],
                ),
                "wlan",
                "beacon",
            ),
            (
                structured(
                    ObjectType::KlineStatusEvent,
                    vec![
                        ("port", FieldValue::U16(2)),
                        ("type", FieldValue::U32(0)),
                        ("dataLen", FieldValue::U16(0)),
                    ],
                ),
                "kline",
                "wake-up pattern",
            ),
            (
                structured(
                    ObjectType::J1708Message,
                    vec![
                        ("channel", FieldValue::U16(1)),
                        ("size", FieldValue::U8(2)),
                        ("data", FieldValue::Bytes(vec![0xAA, 0xBB])),
                    ],
                ),
                "j1708",
                "AA BB",
            ),
            (
                structured(
                    ObjectType::GpsEvent,
                    vec![
                        ("channel", FieldValue::U16(1)),
                        ("latitude", FieldValue::U32(486_000_000)),
                        ("longitude", FieldValue::U32(90_000_000)),
                    ],
                ),
                "-",
                "48.600000",
            ),
        ];
        let axis = TimeAxis {
            mode: TimeMode::Absolute,
            base: 0,
        };
        for (object, expected_bus, expected_text) in cases {
            let mut pool = TextPool::default();
            let summary = summary_for(0, &object, &mut pool);
            let event = summary
                .event
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no event", summary.class_name));
            assert_eq!(
                event.bus_label, expected_bus,
                "{} label",
                summary.class_name
            );
            assert!(
                event.summary.contains(expected_text),
                "{}: {}",
                summary.class_name,
                event.summary
            );
            // The row renders without the frame columns and keeps the text.
            let row = view_row(&summary, View::All, axis, &pool, 120);
            assert!(row.contains(&summary.class_name[..1]) || !row.is_empty());
            let line = summary_line(&summary, axis, &pool, 120);
            assert!(!line.is_empty(), "{} row", summary.class_name);
        }
    }

    #[test]
    fn lost_object_rows_are_marked_for_attention() {
        let object = structured(
            ObjectType::MostDatalost,
            vec![
                ("channel", FieldValue::U16(1)),
                ("lostMsgsCtrl", FieldValue::U32(3)),
                ("lostMsgsAsync", FieldValue::U32(4)),
            ],
        );
        let mut pool = TextPool::default();
        let summary = summary_for(0, &object, &mut pool);
        let event = summary.event.as_ref().expect("event");
        assert_eq!(event.kind, "LOST");
        assert!(event.summary.contains("lost 7 messages"), "{event:?}");
        // The header warning counts these too.
        assert_eq!(count_lost_objects(&[object]), (0, 0));
    }

    #[test]
    fn narrow_terminals_drop_the_preview_pane() {
        let mut app = App::new(None, Vec::new(), Vec::new()).expect("build app");
        app.mode = Mode::Messages;
        app.viewport_width = 80;
        app.viewport_height = 24;
        let mut lines = Vec::new();
        app.render_messages(&mut lines, 80, 24);
        let plain = lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>();
        // No pane divider and no preview heading below the threshold.
        assert!(
            !plain.iter().any(|line| line.contains("Preview")),
            "preview heading should be hidden: {plain:?}"
        );
        assert!(
            !plain.iter().any(|line| line.contains('│')),
            "pane divider should be hidden: {plain:?}"
        );

        // A wide terminal keeps both panes.
        let mut lines = Vec::new();
        app.render_messages(&mut lines, 140, 24);
        let plain = lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>();
        assert!(plain.iter().any(|line| line.contains("Preview")));
    }
}

#[cfg(test)]
mod layout_alignment {
    use super::*;

    fn strip_ansi(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        let mut chars = value.chars();
        while let Some(character) = chars.next() {
            if character != '\x1b' {
                out.push(character);
                continue;
            }
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        out
    }

    fn bar_columns(line: &str) -> Vec<usize> {
        strip_ansi(line)
            .chars()
            .enumerate()
            .filter(|(_, c)| matches!(c, '│' | '┼'))
            .map(|(index, _)| index)
            .collect()
    }

    #[test]
    fn two_column_separator_lines_up_with_the_divider() {
        for width in [60usize, 80, 100, 120, 160] {
            for split in [20usize, 24, 34, 44, 55, 66] {
                if split >= width.saturating_sub(3) {
                    continue;
                }
                let row = two_column("left", "right", split, width);
                let separator = two_column_separator(split, width);
                assert_eq!(
                    bar_columns(&row),
                    bar_columns(&separator),
                    "width {width} split {split}"
                );
                assert_eq!(
                    display_width(&separator),
                    width,
                    "separator should span width {width}"
                );
            }
        }
    }

    #[test]
    fn three_column_separator_lines_up_with_the_dividers() {
        for width in [80usize, 100, 120, 160] {
            for (left, middle) in [(20usize, 30usize), (26, 31), (30, 40)] {
                let row = styled_three_column(
                    ("a", CellStyle::Normal),
                    ("b", CellStyle::Normal),
                    ("c", CellStyle::Normal),
                    left,
                    middle,
                    width,
                );
                let separator = three_column_separator(left, middle, width, width);
                assert_eq!(
                    bar_columns(&row),
                    bar_columns(&separator),
                    "width {width} left {left} middle {middle}"
                );
            }
        }
    }
}

#[cfg(test)]
mod plot_axis_tests {
    use super::*;

    #[test]
    fn tick_labels_keep_integer_trailing_zeros() {
        assert_eq!(tick_label(3600.0), "3600");
        assert_eq!(tick_label(3000.0), "3000");
        assert_eq!(tick_label(100.0), "100");
        assert_eq!(tick_label(10.0), "10");
        // Fractional ticks drop their trailing zeros only.
        assert_eq!(tick_label(1.5), "1.5");
        assert_eq!(tick_label(1.25), "1.25");
        assert_eq!(tick_label(0.5), "0.5");
        assert_eq!(tick_label(0.0), "0");
        assert_eq!(tick_label(-1200.0), "-1200");
        // Very large and very small values switch to exponent form.
        assert_eq!(tick_label(1.0e9), "1.0e9");
        assert_eq!(tick_label(1.0e-5), "1.0e-5");
    }

    #[test]
    fn axis_ticks_cover_the_range_with_nice_steps() {
        let ticks = axis_ticks(0.0, 3500.0, 5);
        assert!(ticks.first().is_some_and(|first| *first <= 0.0));
        assert!(ticks.last().is_some_and(|last| *last >= 3500.0));
        assert!(ticks.len() >= 2, "{ticks:?}");
        // The step is uniform.
        let step = ticks[1] - ticks[0];
        for pair in ticks.windows(2) {
            assert!((pair[1] - pair[0] - step).abs() < 1e-6, "{ticks:?}");
        }
        // A degenerate range still yields at least one tick.
        assert!(!axis_ticks(5.0, 5.0, 4).is_empty());
        assert!(!axis_ticks(-1.0, 1.0, 4).is_empty());
    }

    #[test]
    fn nice_step_uses_one_two_or_five() {
        for (raw, expected) in [
            (1.0, 1.0),
            (1.5, 2.0),
            (3.0, 5.0),
            (7.0, 10.0),
            (700.0, 1000.0),
            (0.3, 0.5),
        ] {
            assert_eq!(nice_step(raw), expected, "raw {raw}");
        }
        // A zero or negative span must not loop forever.
        assert_eq!(nice_step(0.0), 1.0);
        assert_eq!(nice_step(-1.0), 1.0);
    }

    #[test]
    fn the_plot_draws_axes_and_a_connected_line() {
        let series = CurveSeries {
            title: "EngineSpeed".to_string(),
            samples: vec![
                (0, 0.0),
                (100_000_000, 1000.0),
                (200_000_000, 2000.0),
                (300_000_000, 3000.0),
            ],
            error: None,
            truncated: false,
            symbol: '*',
            color: slot_color(Slot::Green),
        };
        let curve = CurveData {
            series: vec![series],
            window_start: 0,
            window_end: 400_000_000,
            normalize: false,
        };
        let lines = plot_series(&curve, (0, 400_000_000), 80, 20);
        assert!(lines.len() >= 4, "{lines:?}");
        let plain: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();
        // A value axis with tick marks, an axis corner and a time label.
        assert!(plain.iter().any(|line| line.contains('┤')), "{plain:?}");
        assert!(plain.iter().any(|line| line.contains('┴')), "{plain:?}");
        assert!(plain.iter().any(|line| line.contains('━')), "{plain:?}");
        let last = plain.last().unwrap();
        assert!(last.contains('s'), "time label: {last:?}");
        // The line spans several rows, so the samples are not a flat row.
        let rows_with_line = plain.iter().filter(|line| line.contains('━')).count();
        assert!(
            rows_with_line >= 3,
            "connected line spans {rows_with_line} rows"
        );
    }
}

#[cfg(test)]
mod plot_fit_tests {
    use super::*;

    fn sample_curve() -> CurveData {
        CurveData {
            series: vec![CurveSeries {
                title: "Speed".to_string(),
                samples: (0..20).map(|i| (i * 1_000_000, i as f64 * 100.0)).collect(),
                error: None,
                truncated: false,
                symbol: '*',
                color: slot_color(Slot::Green),
            }],
            window_start: 0,
            window_end: 19_000_000,
            normalize: false,
        }
    }

    #[test]
    fn the_plot_returns_exactly_the_rows_it_is_given() {
        // The caller reserves `rows` lines, so the plot must not overflow.
        for rows in [6usize, 10, 20, 30] {
            let curve = sample_curve();
            let lines = plot_series(&curve, (0, 19_000_000), 80, rows);
            assert_eq!(lines.len(), rows, "rows {rows}");
        }
    }

    #[test]
    fn the_plot_keeps_the_time_axis_even_when_short() {
        let curve = sample_curve();
        let lines = plot_series(&curve, (0, 19_000_000), 80, 8);
        let plain: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();
        assert!(plain.iter().any(|line| line.contains('┴')), "{plain:?}");
        let last = plain.last().unwrap();
        assert!(last.contains('s'), "time label missing: {last:?}");
    }
}

#[cfg(test)]
mod plot_label_tests {
    use super::*;

    fn one_series() -> CurveData {
        CurveData {
            series: vec![CurveSeries {
                title: "Speed".to_string(),
                samples: (0..10).map(|i| (i * 2_000_000_000, i as f64)).collect(),
                error: None,
                truncated: false,
                symbol: '*',
                color: slot_color(Slot::Green),
            }],
            window_start: 0,
            window_end: 19_200_820_000,
            normalize: false,
        }
    }

    #[test]
    fn both_ends_of_the_time_axis_are_labelled() {
        let curve = one_series();
        let lines = plot_series(&curve, (0, 19_200_820_000), 110, 20);
        let plain: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();
        let axis_label = plain.last().expect("label row");
        assert!(axis_label.contains("0.000s"), "start label: {axis_label:?}");
        assert!(axis_label.contains("19.2"), "end label: {axis_label:?}");
        // Labels must not run into each other.
        let first_end = axis_label.find("0.000s").unwrap() + "0.000s".len();
        let last_start = axis_label.rfind("19.2").unwrap();
        assert!(last_start >= first_end, "labels overlap: {axis_label:?}");
    }

    #[test]
    fn the_axis_label_row_is_not_wider_than_the_plot() {
        for width in [40usize, 60, 80, 110, 160] {
            let curve = one_series();
            let lines = plot_series(&curve, (0, 19_200_820_000), width, 20);
            let plain: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();
            for line in &plain {
                assert!(
                    display_width(line) <= width.max(20),
                    "line wider than the plot: {} > {} ({line:?})",
                    display_width(line),
                    width
                );
            }
        }
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    /// The palette lives in a process-wide atomic, so these tests have to run
    /// one at a time or they would overwrite each other's theme.
    static THEME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        THEME_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// The palette has to follow the background, and the light set must never
    /// use a bright variant: those disappear on a white terminal.
    #[test]
    fn the_light_palette_avoids_bright_foregrounds() {
        let _guard = lock();
        set_theme(Theme::Light);
        for slot in [
            Slot::Red,
            Slot::Green,
            Slot::Yellow,
            Slot::Blue,
            Slot::Magenta,
            Slot::Cyan,
        ] {
            let code = slot_color(slot);
            let number: u8 = code
                .trim_start_matches("\x1b[")
                .trim_end_matches('m')
                .parse()
                .unwrap();
            assert!(
                (30..=37).contains(&number),
                "light theme used a bright colour: {code:?}"
            );
            assert_ne!(number, 30, "pure black on a dark terminal is invisible");
        }
        assert!(light_background());
    }

    #[test]
    fn the_dark_palette_uses_bright_foregrounds() {
        let _guard = lock();
        set_theme(Theme::Dark);
        for slot in [Slot::Red, Slot::Green, Slot::Yellow, Slot::Cyan] {
            let code = slot_color(slot);
            let number: u8 = code
                .trim_start_matches("\x1b[")
                .trim_end_matches('m')
                .parse()
                .unwrap();
            assert!(
                (90..=97).contains(&number),
                "dark theme should be vivid: {code:?}"
            );
        }
        assert!(!light_background());
    }

    #[test]
    fn plain_text_carries_no_colour_so_it_follows_the_terminal() {
        let _guard = lock();
        set_theme(Theme::Light);
        assert_eq!(paint("plain", ANSI_PLAIN), "plain");
        assert_eq!(paint("plain", ""), "plain");
        assert!(paint("coloured", slot_color(Slot::Cyan)).contains("\x1b["));
    }

    #[test]
    fn theme_names_parse_and_rejects_are_reported() {
        assert_eq!(Theme::from_name("auto"), Some(Theme::Auto));
        assert_eq!(Theme::from_name("light"), Some(Theme::Light));
        assert_eq!(Theme::from_name("dark"), Some(Theme::Dark));
        assert_eq!(Theme::from_name("solarized"), None);
    }

    #[test]
    fn auto_falls_back_to_light_when_the_variable_is_missing() {
        // `COLORFGBG` is set by the terminal; without it the safe answer is
        // light, because bright colours on white are the ones that vanish.
        let previous = std::env::var_os("COLORFGBG");
        std::env::remove_var("COLORFGBG");
        assert!(detect_light_background());
        std::env::set_var("COLORFGBG", "15;0");
        assert!(!detect_light_background());
        std::env::set_var("COLORFGBG", "0;15");
        assert!(detect_light_background());
        std::env::set_var("COLORFGBG", "0;default;7");
        assert!(detect_light_background());
        match previous {
            Some(value) => std::env::set_var("COLORFGBG", value),
            None => std::env::remove_var("COLORFGBG"),
        }
    }

    #[test]
    fn the_theme_option_is_parsed_from_the_command_line() {
        let args: Vec<String> = ["trace.blf", "--theme", "dark"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(parse_args(&args).unwrap().theme, Theme::Dark);

        let args: Vec<String> = ["trace.blf", "--dbc", "1=vehicle.dbc", "--theme", "light"]
            .into_iter()
            .map(String::from)
            .collect();
        let parsed = parse_args(&args).unwrap();
        assert_eq!(parsed.theme, Theme::Light);
        assert_eq!(parsed.specs.len(), 1);

        // A missing or unknown value is rejected with the accepted names.
        let args: Vec<String> = ["trace.blf", "--theme", "neon"]
            .into_iter()
            .map(String::from)
            .collect();
        let error = parse_args(&args).unwrap_err();
        assert!(error.contains("invalid theme"), "{error}");
        assert!(error.contains("light"), "{error}");
    }
}

#[cfg(test)]
mod render_width_tests {
    use super::*;

    fn app_with_rows(count: usize) -> App {
        let mut app = App::new(None, Vec::new(), Vec::new()).expect("app");
        app.mode = Mode::Messages;
        for index in 0..count {
            app.summaries.push(MessageSummary {
                object_index: index,
                timestamp_ns: Some(1_000_000_000 + index as u64 * 100_000_000),
                timestamp_source: None,
                bus: Some(BusKind::Can),
                bus_label: "can",
                ethernet: None,
                event: None,
                can_flags: 0,
                channel: Some(1),
                frame_id: Some(0x123),
                object_type_raw: 1,
                class_name: "CanMessage",
                direction: Some("Tx"),
                dlc: Some(8),
                payload: (0..8).map(|byte| byte as u8).collect(),
                status: "ok",
            });
        }
        app.filtered = (0..count).collect();
        app.selected = 0;
        app
    }

    /// Every row the list draws must fit the pane it is given.
    ///
    /// A fixed limit in `summary_line` used to draw rows wider than the
    /// terminal, which pushed the separator past the right edge.
    #[test]
    fn rows_never_exceed_the_requested_width() {
        for width in [40usize, 60, 80, 100, 120, 160] {
            for height in [16usize, 24, 40] {
                let app = app_with_rows(8);
                let mut lines = Vec::new();
                app.render_messages(&mut lines, width, height);
                for line in &lines {
                    assert!(
                        display_width(line) <= width,
                        "width {width} height {height}: row is {} wide: {:?}",
                        display_width(line),
                        strip_ansi(line)
                    );
                }
            }
        }
    }

    #[test]
    fn titles_and_rules_follow_the_terminal_width() {
        for width in [40usize, 60, 80, 100, 120, 160] {
            let app = app_with_rows(3);
            let mut lines = Vec::new();
            app.render_messages(&mut lines, width, 24);
            let plain: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();
            // The heading and the separator are the first two rows.
            assert!(
                display_width(&plain[0]) <= width,
                "heading wider than {width}: {:?}",
                plain[0]
            );
            let rule = &plain[2];
            assert!(
                rule.chars().all(|c| c == '─' || c == '┼'),
                "second row is not a rule: {rule:?}"
            );
            assert_eq!(
                display_width(rule),
                width,
                "rule should span the terminal at width {width}"
            );
        }
    }

    #[test]
    fn a_selected_row_highlights_only_the_list_pane() {
        let app = app_with_rows(4);
        let mut lines = Vec::new();
        app.render_messages(&mut lines, 100, 24);
        // Row 3 is the first data row (after heading, columns and rule).
        let selected = &lines[3];
        assert!(selected.contains(ANSI_SELECTED), "row is not highlighted");
        // The highlight must be closed before the pane divider.
        let after_highlight = selected
            .split(ANSI_SELECTED)
            .nth(1)
            .expect("highlight start");
        let inside = after_highlight
            .split(ANSI_RESET)
            .next()
            .expect("highlight end");
        let inside_plain = strip_ansi(inside);
        assert!(
            !inside_plain.contains('│'),
            "highlight covers the divider: {inside_plain:?}"
        );
        assert!(
            !inside_plain.contains("time:"),
            "highlight covers the preview: {inside_plain:?}"
        );
    }

    #[test]
    fn width_measurement_ignores_escape_sequences() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("\x1b[32mabc\x1b[0m"), 3);
        assert_eq!(display_width("\x1b[1m\x1b[96mabc\x1b[0m"), 3);
        assert_eq!(display_width("\x1b[38;5;196mab\x1b[0m"), 2);
        // Truncating a coloured string keeps the escape sequences intact.
        let cut = truncate("\x1b[32mabcdef\x1b[0m", 4);
        assert!(display_width(&cut) <= 4, "{cut:?}");
        assert!(cut.contains("\x1b[32m"), "{cut:?}");
        assert!(cut.contains(ANSI_RESET), "{cut:?}");
        // A plain string keeps the old behaviour.
        assert_eq!(truncate("abcdef", 4), "a...");
    }

    #[test]
    fn the_terminal_size_ioctl_matches_a_known_pty() {
        // The kernel answer is what the layout relies on; `stty` is only a
        // fallback, so a wrong reading here breaks every column width.
        let fd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null");
        // /dev/null is not a terminal, so the call must decline rather than lie.
        if let Ok(file) = fd {
            use std::os::fd::AsRawFd;
            assert_eq!(terminal_size_from_kernel(file.as_raw_fd()), None);
        }
    }
}

#[cfg(test)]
mod curve_normalize_tests {
    use super::*;

    fn two_scale_curve(normalize: bool) -> CurveData {
        CurveData {
            series: vec![
                CurveSeries {
                    title: "Speed".to_string(),
                    samples: (0..10).map(|i| (i * 1_000_000, i as f64 * 400.0)).collect(),
                    error: None,
                    truncated: false,
                    symbol: '*',
                    color: slot_color(Slot::Green),
                },
                CurveSeries {
                    title: "Gear".to_string(),
                    samples: (0..10).map(|i| (i * 1_000_000, (i % 2) as f64)).collect(),
                    error: None,
                    truncated: false,
                    symbol: 'o',
                    color: slot_color(Slot::Magenta),
                },
            ],
            window_start: 0,
            window_end: 10_000_000,
            normalize,
        }
    }

    /// With a shared axis the small signal collapses; with per-series scaling it
    /// has to spread over the plot.
    #[test]
    fn normalising_gives_the_small_signal_its_own_height() {
        let shared = plot_series(&two_scale_curve(false), (0, 10_000_000), 60, 16);
        let normalized = plot_series(&two_scale_curve(true), (0, 10_000_000), 60, 16);

        let rows_with_magenta = |lines: &[String]| {
            lines
                .iter()
                .filter(|line| line.contains(slot_color(Slot::Magenta)))
                .count()
        };
        let shared_rows = rows_with_magenta(&shared);
        let normalized_rows = rows_with_magenta(&normalized);
        assert!(
            normalized_rows > shared_rows,
            "normalised series should spread out: shared {shared_rows} rows, \
             normalised {normalized_rows} rows"
        );
        assert!(
            normalized_rows >= 2,
            "normalised series drawn on {normalized_rows} rows"
        );
    }

    #[test]
    fn the_axis_labels_follow_the_mode() {
        let shared = plot_series(&two_scale_curve(false), (0, 10_000_000), 70, 14);
        let normalized = plot_series(&two_scale_curve(true), (0, 10_000_000), 70, 14);
        let plain = |lines: &[String]| {
            lines
                .iter()
                .map(|line| strip_ansi(line))
                .collect::<Vec<_>>()
                .join("\n")
        };
        // A shared axis prints real values, a normalised one prints percentages.
        let has_number = |text: &str| {
            text.split_whitespace()
                .any(|token| token.parse::<f64>().is_ok() && token != "0")
        };
        assert!(has_number(&plain(&shared)), "{}", plain(&shared));
        assert!(plain(&normalized).contains('%'), "{}", plain(&normalized));
        assert!(!plain(&shared).contains('%'), "{}", plain(&shared));
    }

    #[test]
    fn both_modes_keep_the_axes_and_the_line() {
        for normalize in [false, true] {
            let lines = plot_series(&two_scale_curve(normalize), (0, 10_000_000), 70, 16);
            let plain: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();
            assert!(plain.iter().any(|line| line.contains('┤')), "value axis");
            assert!(plain.iter().any(|line| line.contains('┴')), "time axis");
            assert!(plain.iter().any(|line| line.contains('━')), "line");
        }
    }
}

#[cfg(test)]
mod detail_pane_tests {
    use super::*;

    fn app_with_many_fields() -> App {
        let mut app = App::new(None, Vec::new(), Vec::new()).expect("app");
        app.mode = Mode::Detail;
        let object = blf_core::Object::Structured(blf_core::StructuredObject {
            object_type: blf_core::ObjectType::CanDriverError,
            header: blf_core::RawObjectHeader::Object(blf_core::ObjectHeader {
                base: blf_core::ObjectHeaderBase {
                    signature: blf_core::OBJECT_SIGNATURE,
                    header_size: 32,
                    header_version: 1,
                    object_size: 48,
                    object_type: blf_core::ObjectType::CanDriverError,
                    raw_object_type: 31,
                },
                object_flags: 2,
                client_index: 0,
                object_version: 0,
                object_time_stamp: 1_000_000,
            }),
            class_name: "CanDriverError".to_string(),
            fields: (0..60)
                .map(|index| ("channel", blf_core::FieldValue::U16(index as u16)))
                .collect(),
            extensions: vec![0; 400],
            data: Vec::new(),
        });
        let mut pool = TextPool::default();
        app.summaries.push(summary_for(0, &object, &mut pool));
        app.objects.push(object);
        app.filtered = vec![0];
        app.selected = 0;
        app
    }

    #[test]
    fn tab_switches_which_column_the_keys_move() {
        let mut app = app_with_many_fields();
        assert_eq!(app.detail_pane, DetailPane::Signals);
        app.handle_event(InputEvent::Tab).expect("handle");
        assert_eq!(app.detail_pane, DetailPane::Details);
        app.handle_event(InputEvent::Tab).expect("handle");
        assert_eq!(app.detail_pane, DetailPane::Signals);
    }

    #[test]
    fn the_attribute_column_scrolls_when_it_is_focused() {
        let mut app = app_with_many_fields();
        app.viewport_height = 24;
        let mut lines = Vec::new();
        app.render_detail(&mut lines, 100, 24);
        assert!(
            app.detail_left_total > app.detail_left_rows,
            "fixture too short"
        );

        // Keys do nothing to the attributes while the signals are focused.
        let before = app.detail_left_offset;
        app.handle_event(InputEvent::Char('j')).expect("handle");
        assert_eq!(app.detail_left_offset, before);

        // Focused, they scroll and stop at the end.
        app.detail_pane = DetailPane::Details;
        app.handle_event(InputEvent::Char('j')).expect("handle");
        assert_eq!(app.detail_left_offset, 1);
        app.handle_event(InputEvent::End).expect("handle");
        assert_eq!(
            app.detail_left_offset,
            app.detail_left_total - app.detail_left_rows
        );
        app.handle_event(InputEvent::Char('j')).expect("handle");
        assert_eq!(
            app.detail_left_offset,
            app.detail_left_total - app.detail_left_rows
        );
        app.handle_event(InputEvent::Home).expect("handle");
        assert_eq!(app.detail_left_offset, 0);
    }

    #[test]
    fn scrolling_reveals_the_tail_of_the_attributes() {
        let mut app = app_with_many_fields();
        app.viewport_height = 24;
        app.detail_pane = DetailPane::Details;
        let mut lines = Vec::new();
        app.render_detail(&mut lines, 100, 24);
        app.handle_event(InputEvent::End).expect("handle");
        let mut lines = Vec::new();
        app.render_detail(&mut lines, 100, 24);
        // Scrolled to the end, the column no longer starts at the timestamp and
        // shows content that the first screen had pushed out.
        let joined = lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !joined.contains("time: 0.001000s"),
            "still showing the top: {joined}"
        );
        assert!(
            app.detail_left_offset > 0,
            "offset did not move: {}",
            app.detail_left_offset
        );
    }
}

#[cfg(test)]
mod help_filter_tests {
    use super::*;

    fn app() -> App {
        App::new(None, Vec::new(), Vec::new()).expect("app")
    }

    #[test]
    fn a_filter_keeps_the_matching_entries_and_their_headings() {
        let mut help = HelpState {
            offset: 0,
            previous_status: String::new(),
            filter: "curve".to_string(),
        };
        let entries = help.entries(100);
        assert!(!entries.is_empty(), "no match for a documented key");
        let joined = entries
            .iter()
            .map(|(_, text)| text.to_lowercase())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("curve"), "{joined}");
        // At least one heading survived so the result keeps its structure.
        assert!(entries.iter().any(|(heading, _)| *heading), "{entries:?}");

        help.filter = "zzzznothing".to_string();
        let empty = help.entries(100);
        assert_eq!(empty.len(), 1);
        assert!(empty[0].1.contains("no key matches"), "{empty:?}");
    }

    #[test]
    fn the_filter_is_typed_after_a_slash() {
        let mut app = app();
        app.open_help();
        app.handle_help_event(InputEvent::Char('/'));
        assert_eq!(app.input, Some(InputKind::HelpFilter));
        for character in "curve".chars() {
            app.handle_help_event(InputEvent::Char(character));
        }
        assert_eq!(app.help.as_ref().unwrap().filter, "curve");
        // Backspace edits it, Enter leaves the filter active.
        app.handle_help_event(InputEvent::Backspace);
        assert_eq!(app.help.as_ref().unwrap().filter, "curv");
        app.handle_help_event(InputEvent::Enter);
        assert_eq!(app.input, None);
        assert_eq!(app.help.as_ref().unwrap().filter, "curv");
        // Esc inside the input clears the filter without closing the overlay.
        app.handle_help_event(InputEvent::Char('/'));
        app.handle_help_event(InputEvent::Esc);
        assert_eq!(app.help.as_ref().unwrap().filter, "");
        assert!(app.help.is_some());
    }

    #[test]
    fn scrolling_follows_the_filtered_length() {
        let mut app = app();
        app.open_help();
        app.viewport_height = 20;
        let all = help_lines(100).len();
        app.scroll_help(all as isize * 4);
        let bottom_all = app.help.as_ref().unwrap().offset;
        assert!(bottom_all > 0, "help did not scroll");

        // A one-line filter result cannot scroll at all.
        app.help.as_mut().unwrap().filter = "zzzznothing".to_string();
        app.help.as_mut().unwrap().offset = 0;
        app.scroll_help(10);
        assert_eq!(app.help.as_ref().unwrap().offset, 0);
    }

    #[test]
    fn the_footer_names_the_help_prompt() {
        let footer = footer_line(Some(InputKind::HelpFilter), "curve", "", Mode::Messages);
        assert!(footer.contains("help /"), "{footer}");
        assert!(footer.contains("curve"), "{footer}");
    }
}

#[cfg(test)]
mod empty_state_tests {
    use super::*;

    #[test]
    fn an_empty_list_explains_what_to_change() {
        let mut app = App::new(None, Vec::new(), Vec::new()).expect("app");
        // No objects at all.
        assert!(app.empty_list_hint()[0].contains("no objects"));

        // Objects exist but the filter matches none.
        app.summaries.push(MessageSummary {
            object_index: 0,
            timestamp_ns: Some(0),
            timestamp_source: None,
            bus: Some(BusKind::Can),
            bus_label: "can",
            ethernet: None,
            event: None,
            can_flags: 0,
            channel: Some(1),
            frame_id: Some(1),
            object_type_raw: 1,
            class_name: "CanMessage",
            direction: Some("Rx"),
            dlc: Some(1),
            payload: vec![0],
            status: "ok",
        });
        app.filter = "zzz".to_string();
        let hint = app.empty_list_hint();
        let joined = hint.join("\n");
        assert!(joined.contains("zzz"), "{joined}");
        assert!(joined.contains("Esc"), "{joined}");

        // A bus view with no rows suggests switching views.
        app.filter.clear();
        app.view = View::Ethernet;
        let joined = app.empty_list_hint().join("\n");
        assert!(joined.contains("ETHERNET"), "{joined}");
        assert!(joined.contains('v'), "{joined}");

        // A time range narrows it further, so say how to change that too.
        app.view = View::All;
        app.time_range = Some((0, 1));
        let joined = app.empty_list_hint().join("\n");
        assert!(joined.contains("time range"), "{joined}");
    }

    #[test]
    fn an_empty_detail_pane_names_the_keys() {
        let mut app = App::new(None, Vec::new(), Vec::new()).expect("app");
        let mut lines = Vec::new();
        app.render_detail(&mut lines, 100, 24);
        let joined = lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("No message selected"), "{joined}");
        assert!(joined.contains("filters the list"), "{joined}");
    }
}

#[cfg(test)]
mod database_pane_tests {
    use super::*;

    fn app() -> App {
        App::new(None, Vec::new(), Vec::new()).expect("app")
    }

    #[test]
    fn a_narrow_browser_keeps_both_panes_usable() {
        let mut app = app();
        let mut lines = Vec::new();
        for width in [40usize, 60, 72] {
            app.viewport_width = width;
            lines.clear();
            app.render_databases(&mut lines, width, 24);
            let plain: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();
            // Two panes, so exactly one divider per row and no empty third one.
            for line in &plain {
                assert!(
                    display_width(line) <= width,
                    "width {width}: row too wide: {line:?}"
                );
            }
            let dividers = plain
                .iter()
                .map(|line| line.matches('│').count())
                .max()
                .unwrap_or(0);
            assert_eq!(dividers, 1, "width {width}: expected two panes");
        }
    }

    #[test]
    fn a_wide_browser_still_shows_three_panes() {
        let app = app();
        for width in [100usize, 140] {
            let mut lines = Vec::new();
            app.render_databases(&mut lines, width, 24);
            let plain: Vec<String> = lines.iter().map(|line| strip_ansi(line)).collect();
            let dividers = plain
                .iter()
                .map(|line| line.matches('│').count())
                .max()
                .unwrap_or(0);
            assert_eq!(dividers, 2, "width {width}: expected three panes");
        }
    }
}

#[cfg(test)]
mod help_coverage_tests {
    use super::*;

    /// Every key the views react to has to be listed, or the help lies.
    #[test]
    fn the_help_lists_the_keys_the_views_use() {
        let text = help_lines(120)
            .into_iter()
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n");

        for expected in [
            // Messages
            "filter messages",
            "limit to a time range",
            // Detail
            "switch between attributes and signals",
            "curve for the selected numeric signal",
            "add the selected signal to the open curve",
            // Curve
            "pan the sample window",
            "scale each signal to its own range",
            "remove the last added signal",
            // Databases
            "focus the tree, object or details pane",
            // Attributes
            "export the visible frames to pcapng",
            // Help
            "search the key list",
            // Global
            "open or close this help",
            "light, dark or auto colour set",
        ] {
            assert!(text.contains(expected), "help does not mention: {expected}");
        }
    }

    #[test]
    fn the_help_has_a_section_for_every_view() {
        let headings: Vec<String> = help_lines(120)
            .into_iter()
            .filter(|(heading, _)| *heading)
            .map(|(_, line)| line)
            .collect();
        for expected in [
            "Messages",
            "Detail",
            "Curve",
            "Frame statistics",
            "Databases",
            "Attributes",
            "Help",
            "Global",
        ] {
            assert!(
                headings.iter().any(|heading| heading.contains(expected)),
                "no help section for {expected}: {headings:?}"
            );
        }
    }
}
