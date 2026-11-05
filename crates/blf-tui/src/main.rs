use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::fs::{File as StdFile, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
/// First list row below the header block, section title and column rule.
const LIST_FIRST_ROW: u16 = HEADER_ROWS + 2;
/// First detail row: the header block plus the detail section title.
const DETAIL_FIRST_ROW: u16 = HEADER_ROWS + 1;
/// Status for a trace-only action in a database-only session.
const NO_TRACE_STATUS: &str = "No trace loaded";

// Match the vivid ANSI palette used by the other terminal tools in this workspace.
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_MAGENTA: &str = "\x1b[35m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_WHITE: &str = "\x1b[37m";
const ANSI_BRIGHT_GREEN: &str = "\x1b[92m";
const ANSI_BRIGHT_CYAN: &str = "\x1b[96m";
const ANSI_BRIGHT_YELLOW: &str = "\x1b[93m";
const ANSI_SELECTED: &str = "\x1b[1;97;44m";

fn paint(value: impl AsRef<str>, color: &str) -> String {
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

#[derive(Debug, Clone)]
struct MessageSummary {
    object_index: usize,
    timestamp_ns: Option<u64>,
    timestamp_source: Option<&'static str>,
    bus: Option<BusKind>,
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

/// Identifies one frame id on one bus and channel.
type FrameKey = (BusKind, u16, u32);

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
struct CurveData {
    title: String,
    samples: Vec<(u64, f64)>,
    error: Option<String>,
    truncated: bool,
    window_start: usize,
    window_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Messages,
    Detail,
    Curve,
    Databases,
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
    dirty: bool,
    selected: usize,
    list_offset: usize,
    selected_signal: usize,
    detail_offset: usize,
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
}

/// State of the `?` help overlay; closing restores the previous status.
struct HelpState {
    offset: usize,
    previous_status: String,
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
        let summaries = objects
            .iter()
            .enumerate()
            .map(|(object_index, object)| summary_for(object_index, object))
            .collect::<Vec<_>>();
        let filtered = (0..summaries.len()).collect::<Vec<_>>();
        let mut app = Self {
            trace,
            objects,
            summaries,
            bindings: Vec::new(),
            mode: Mode::Messages,
            input: None,
            input_buffer: String::new(),
            filter: String::new(),
            filtered,
            db_objects: Vec::new(),
            frame_index: None,
            timing: None,
            dirty: true,
            selected: 0,
            list_offset: 0,
            selected_signal: 0,
            detail_offset: 0,
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
    fn title_line(&self) -> String {
        match &self.trace {
            Some(trace) => format!(
                " vector-log-tui  {}  objects={}/{}  skipped={}  mode={:?}",
                trace.path.display(),
                self.filtered.len(),
                self.summaries.len(),
                trace.skipped_objects,
                self.mode
            ),
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
        if self.input.is_some() {
            return self.handle_input_event(event);
        }
        if self.help.is_some() {
            self.handle_help_event(event);
            return Ok(false);
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
                InputEvent::Char('e') => self.export_csv(),
                InputEvent::Char('g') => self.start_input(InputKind::Goto),
                InputEvent::Char('G') => self.jump_to_end(),
                InputEvent::Char(']') => self.step_frame(1),
                InputEvent::Char('[') => self.step_frame(-1),
                InputEvent::Char('?') => self.open_help(),
                InputEvent::Esc => self.clear_filter(),
                InputEvent::Mouse(mouse) => self.handle_message_mouse(mouse),
                _ => {}
            },
            Mode::Detail => match event {
                InputEvent::Char('q') => return Ok(true),
                InputEvent::Up | InputEvent::Char('k') => self.move_signal(-1),
                InputEvent::Down | InputEvent::Char('j') => self.move_signal(1),
                InputEvent::PageUp => self.move_signal(-self.page_step()),
                InputEvent::PageDown => self.move_signal(self.page_step()),
                InputEvent::Home => self.select_first_signal(),
                InputEvent::End => self.select_last_signal(),
                InputEvent::Char('p') => self.open_curve(),
                InputEvent::Char('b') => self.start_input(InputKind::Binding),
                InputEvent::Char('e') => self.export_csv(),
                InputEvent::Esc | InputEvent::Left => self.mode = Mode::Messages,
                InputEvent::Char('?') => self.open_help(),
                InputEvent::Mouse(mouse) => self.handle_detail_mouse(mouse),
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
                        InputKind::Goto => {
                            if let Err(error) = self.goto_input(&value) {
                                self.status = error;
                                self.input = Some(InputKind::Goto);
                                self.input_buffer = value;
                            }
                        }
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

    fn move_signal(&mut self, delta: isize) {
        self.selected_signal = move_index(self.selected_signal, self.detail_rows.len(), delta);
        self.keep_signal_visible();
    }

    fn select_first(&mut self) {
        self.selected = 0;
        self.list_offset = 0;
        self.selected_signal = 0;
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

    fn selected_frame_key(&self) -> Option<FrameKey> {
        let summary = self.selected_summary()?;
        Some((summary.bus?, summary.channel?, summary.frame_id?))
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
            let (Some(bus), Some(channel), Some(frame_id)) =
                (summary.bus, summary.channel, summary.frame_id)
            else {
                continue;
            };
            index
                .entry((bus, channel, frame_id))
                .or_default()
                .push(position);
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
        };
        rows as isize
    }

    fn clear_filter(&mut self) {
        self.filter.clear();
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
        let (_, _, width) = self.database_layout(self.viewport_width);
        let total = self.database_detail_lines(width).len();
        let rows = self.database_body_rows(self.viewport_height);
        self.database_detail_offset = clamp_detail_offset(self.database_detail_offset, total, rows);
    }

    /// Scroll the details column by `delta` wrapped lines.
    fn scroll_database_details(&mut self, delta: isize) {
        let (_, _, width) = self.database_layout(self.viewport_width);
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
        let key = (summary.bus?, summary.channel?, summary.frame_id?);
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
        let filtered = if filter.is_empty() {
            (0..self.summaries.len()).collect()
        } else {
            let mut buffer = String::with_capacity(128);
            let mut indices = Vec::new();
            for (index, summary) in self.summaries.iter().enumerate() {
                if summary_matches_in(summary, &filter, &mut buffer) {
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
    fn export_csv(&mut self) {
        let Some(trace) = &self.trace else {
            self.status = NO_TRACE_STATUS.to_string();
            return;
        };
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
        let mut header = vec![
            "timestamp_s".to_string(),
            "bus".to_string(),
            "channel".to_string(),
            "frame_id".to_string(),
            "direction".to_string(),
            "dlc".to_string(),
            "data".to_string(),
            "status".to_string(),
        ];
        header.extend(columns.iter().cloned());
        writeln!(writer, "{}", csv_line(&header)).map_err(|error| error.to_string())?;

        let mut written = 0;
        for summary in self.filtered_rows() {
            let decoded = self.decoded_rows(summary);
            let mut fields = vec![
                format_timestamp(summary.timestamp_ns),
                summary
                    .bus
                    .map_or(String::new(), |bus| bus.as_str().to_string()),
                summary
                    .channel
                    .map_or_else(String::new, |value| value.to_string()),
                summary
                    .frame_id
                    .map_or_else(String::new, |value| format!("0x{value:X}")),
                summary.direction.unwrap_or_default().to_string(),
                summary
                    .dlc
                    .map_or_else(String::new, |value| value.to_string()),
                hex_bytes(&summary.payload),
                summary.status.to_string(),
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

    fn open_curve(&mut self) {
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
        let signal_name = qualified_signal_name(
            binding.database(),
            &row.name,
            &self.objects[summary.object_index],
        );
        let Some(trace) = &self.trace else {
            return;
        };
        let query = match trace
            .reader
            .signal_query(binding, signal_name.clone(), false)
        {
            Ok(query) => query,
            Err(error) => {
                self.status = format!("start signal query: {error}");
                return;
            }
        };
        let mut curve_samples = Vec::new();
        let mut decode_error: Option<String> = None;
        let mut truncated = false;
        for item in query {
            match item {
                Ok(sample) => {
                    if decode_error.is_none() {
                        decode_error = sample.error.clone();
                    }
                    let Some(value) = sample.physical_value else {
                        continue;
                    };
                    if curve_samples.len() >= MAX_CURVE_SAMPLES {
                        truncated = true;
                        break;
                    }
                    curve_samples.push((sample.timestamp_ns, value));
                }
                Err(error) => {
                    decode_error = Some(error.to_string());
                    break;
                }
            }
        }
        self.curve = Some(CurveData {
            title: signal_name,
            window_len: curve_samples.len().max(1),
            samples: curve_samples,
            error: decode_error,
            truncated,
            window_start: 0,
        });
        self.mode = Mode::Curve;
    }

    fn curve_pan(&mut self, direction: isize) {
        let Some(curve) = &mut self.curve else {
            return;
        };
        let step = (curve.window_len / 5).max(1) as isize;
        let max_start = curve.samples.len().saturating_sub(curve.window_len) as isize;
        curve.window_start =
            (curve.window_start as isize + direction * step).clamp(0, max_start) as usize;
    }

    fn curve_zoom(&mut self, factor: f64) {
        let Some(curve) = &mut self.curve else {
            return;
        };
        let old_len = curve.window_len.max(1);
        let max_len = curve.samples.len().max(1);
        let min_len = max_len.min(2);
        let new_len = ((old_len as f64 * factor).round() as usize).clamp(min_len, max_len);
        let center = curve.window_start + old_len / 2;
        curve.window_len = new_len;
        curve.window_start = center
            .saturating_sub(new_len / 2)
            .min(curve.samples.len().saturating_sub(new_len));
    }

    fn curve_home(&mut self) {
        if let Some(curve) = &mut self.curve {
            curve.window_start = 0;
        }
    }

    fn curve_end(&mut self) {
        if let Some(curve) = &mut self.curve {
            curve.window_start = curve.samples.len().saturating_sub(curve.window_len);
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
        let left = self.database_left_width().min(width.saturating_sub(2));
        let middle = (width * 32 / 100)
            .max(30)
            .min(width.saturating_sub(left + 1));
        let right = width.saturating_sub(left + middle + 6);
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
        });
        self.status = "Help: j/k or arrows scroll, ? / q / Esc closes".to_string();
    }

    fn close_help(&mut self) {
        if let Some(help) = self.help.take() {
            self.status = help.previous_status;
        }
    }

    fn scroll_help(&mut self, delta: isize) {
        let rows = help_lines(self.viewport_width).len();
        let page = help_page_size(self.viewport_height);
        let max_offset = rows.saturating_sub(page) as isize;
        if let Some(help) = &mut self.help {
            help.offset = (help.offset as isize)
                .saturating_add(delta)
                .clamp(0, max_offset) as usize;
        }
    }

    fn handle_help_event(&mut self, event: InputEvent) {
        let page = help_page_size(self.viewport_height) as isize;
        let rows = help_lines(self.viewport_width).len() as isize;
        match event {
            InputEvent::Char('?') | InputEvent::Char('q') | InputEvent::Esc => self.close_help(),
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
        lines.push(paint_bold(self.title_line(), ANSI_BRIGHT_CYAN));
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
        lines.push(paint(format!(" databases: {databases}"), ANSI_CYAN));
        lines.push(paint("─".repeat(width), ANSI_BRIGHT_CYAN));
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
            }
        }
        lines.push(paint("─".repeat(width), ANSI_BRIGHT_CYAN));
        lines.push(paint_bold(
            footer_line(self.input, &self.input_buffer, &self.status),
            ANSI_BRIGHT_YELLOW,
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
        let entries = help_lines(width);
        let offset = clamp_help_offset(
            self.help.as_ref().map_or(0, |help| help.offset),
            entries.len(),
            page,
        );
        let end = (offset + page).min(entries.len());
        lines.push(paint_bold(
            format!(
                "Help  keys {}-{} of {}  (? / q / Esc closes)",
                offset + 1,
                end,
                entries.len()
            ),
            ANSI_BRIGHT_CYAN,
        ));
        for (is_heading, line) in &entries[offset..end] {
            lines.push(if *is_heading {
                paint_bold(line, ANSI_BRIGHT_YELLOW)
            } else {
                paint(line, ANSI_WHITE)
            });
        }
    }

    fn render_messages(&self, lines: &mut Vec<String>, width: usize, height: usize) {
        let indices = &self.filtered;
        let split = (width * 43 / 100).max(24).min(width.saturating_sub(3));
        let preview_width = width.saturating_sub(split + 3);
        lines.push(paint_bold(
            two_column(
                &format!("Messages ({}/{})", indices.len(), self.summaries.len()),
                "Preview",
                split,
                width,
            ),
            ANSI_BRIGHT_CYAN,
        ));
        lines.push(paint(
            format!(
                "{}─┼{}",
                "─".repeat(split),
                "─".repeat(width.saturating_sub(split + 1))
            ),
            ANSI_CYAN,
        ));
        let available = height.saturating_sub(7);
        let preview = self
            .selected_summary()
            .map(|summary| summary_detail_lines(summary, self.selected_object()))
            .unwrap_or_else(|| vec!["No matching messages".to_string()]);
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
                    format!(
                        "{}{}",
                        if selected { "▶ " } else { "  " },
                        summary_line(summary)
                    )
                })
                .unwrap_or_default();
            let right = preview.get(row).cloned().unwrap_or_default();
            let line = two_column(&left, &right, split, width);
            lines.push(if selected {
                paint_selected(line)
            } else if let Some(summary) = summary {
                message_color(summary, line)
            } else {
                paint(line, ANSI_WHITE)
            });
        }
    }

    fn render_detail(&self, lines: &mut Vec<String>, width: usize, height: usize) {
        let Some(summary) = self.selected_summary() else {
            lines.push("No message selected".to_string());
            return;
        };
        let split = (width * 43 / 100).max(24).min(width.saturating_sub(3));
        let right_width = width.saturating_sub(split + 3);
        let mut left = summary_detail_lines(summary, self.selected_object());
        left.extend(timing_lines(self.timing.clone()));
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
        for row in 0..height.saturating_sub(5) {
            let (signal, right_text) = right
                .get(row)
                .map_or((None, ""), |(index, text)| (*index, text.as_str()));
            let line = two_column(
                left.get(row).map_or("", String::as_str),
                right_text,
                split,
                width,
            );
            let signal_selected = signal == Some(self.selected_signal);
            lines.push(if signal_selected {
                paint_selected(line)
            } else if row == 0 {
                paint_bold(line, ANSI_BRIGHT_CYAN)
            } else {
                paint(line, ANSI_WHITE)
            });
        }
    }

    fn render_curve(&self, lines: &mut Vec<String>, width: usize, height: usize) {
        let Some(curve) = &self.curve else {
            lines.push("No curve selected".to_string());
            return;
        };
        let start = curve.window_start.min(curve.samples.len());
        let end = start
            .saturating_add(curve.window_len)
            .min(curve.samples.len());
        let visible = &curve.samples[start..end];
        lines.push(paint_bold(
            format!(
                "Curve: {}  samples={}/{}  window={}-{}{}",
                curve.title,
                visible.len(),
                curve.samples.len(),
                start,
                end,
                if curve.truncated {
                    "  (first samples only)"
                } else {
                    ""
                }
            ),
            ANSI_BRIGHT_CYAN,
        ));
        if let Some(error) = &curve.error {
            lines.push(paint(format!("decode note: {error}"), ANSI_RED));
        }
        if visible.is_empty() {
            lines.push(paint("No numeric samples available", ANSI_BRIGHT_YELLOW));
            return;
        }
        lines.extend(
            ascii_curve(visible, width.saturating_sub(2), height.saturating_sub(6))
                .lines()
                .map(|line| paint(line, ANSI_BRIGHT_GREEN)),
        );
    }

    fn render_databases(&self, lines: &mut Vec<String>, width: usize, height: usize) {
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
        lines.push(styled_three_column(
            (tree_header, pane_header_style(tree_focused)),
            (object_header.as_str(), pane_header_style(objects_focused)),
            (detail_header.as_str(), pane_header_style(details_focused)),
            left,
            middle,
            width,
        ));
        lines.push(paint(
            format!(
                "{}─┼{}─┼{}",
                "─".repeat(left),
                "─".repeat(middle),
                "─".repeat(width.saturating_sub(left + middle + 6))
            ),
            ANSI_CYAN,
        ));
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
            lines.push(styled_three_column(
                (&tree_text, tree_style),
                (&object_text, object_style),
                (detail_text.as_str(), detail_style),
                left,
                middle,
                width,
            ));
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
        CellStyle::Normal => paint(value, ANSI_WHITE),
        CellStyle::Selected => paint_selected(value),
        CellStyle::Dim => paint(value, ANSI_CYAN),
        CellStyle::PaneHeader => paint_bold(value, ANSI_BRIGHT_CYAN),
        CellStyle::PaneHeaderIdle => paint(value, ANSI_BRIGHT_CYAN),
        CellStyle::DatabaseHeader => paint_bold(value, ANSI_BRIGHT_YELLOW),
        CellStyle::DetailHeading => paint_bold(value, ANSI_BRIGHT_CYAN),
        CellStyle::DetailItem => paint(value, ANSI_WHITE),
    }
}

/// Render three padded columns, styling each cell independently so one pane's
/// selection never changes how the neighbouring pane is drawn.
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

fn summary_for(object_index: usize, object: &Object) -> MessageSummary {
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
    let status = match object {
        Object::Lin(value) => lin_status(classify_lin_object(value).state),
        Object::CanMessage(_)
        | Object::CanMessage2(_)
        | Object::CanFdMessage(_)
        | Object::CanFdMessage64(_) => "ok",
        _ => "event",
    };
    MessageSummary {
        object_index,
        timestamp_ns: object_timestamp_ns(object),
        timestamp_source: object_timestamp_source(object),
        bus,
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

fn summary_detail_lines(summary: &MessageSummary, object: Option<&Object>) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("time: {}", format_timestamp(summary.timestamp_ns)));
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
        summary.bus.map_or("-", BusKind::as_str),
        summary
            .channel
            .map_or_else(|| "-".to_string(), |value| value.to_string())
    ));
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
            .map(|(name, value)| (name.clone(), field_value(value)))
            .collect(),
        Object::Structured(value) => value
            .fields
            .iter()
            .map(|(name, value)| (name.clone(), field_value(value)))
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

fn summary_line(summary: &MessageSummary) -> String {
    let time = format_timestamp(summary.timestamp_ns);
    let bus = summary.bus.map_or("--", BusKind::as_str);
    let channel = summary
        .channel
        .map_or_else(|| "-".to_string(), |value| value.to_string());
    let id = summary
        .frame_id
        .map_or_else(|| "------".to_string(), |value| format!("0x{value:03X}"));
    let direction = summary.direction.unwrap_or("-");
    let payload = hex_bytes(&summary.payload);
    let text = format!("{time:>12} {bus:<3} {channel:>3} {id:>6} {direction:<3} {payload}");
    if display_width(&text) > 120 {
        return truncate(&text, 120);
    }
    text
}

fn message_color(summary: &MessageSummary, text: String) -> String {
    if summary.status == "ok" {
        return match summary.bus {
            Some(BusKind::Can) => paint(text, ANSI_BRIGHT_GREEN),
            Some(BusKind::Lin) => paint(text, ANSI_MAGENTA),
            None => paint(text, ANSI_WHITE),
        };
    }
    match summary.status {
        "event" => paint(text, ANSI_CYAN),
        "header_only" | "no_response" | "unsupported" => paint(text, ANSI_BRIGHT_YELLOW),
        _ => paint(text, ANSI_RED),
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

fn ascii_curve(samples: &[(u64, f64)], width: usize, height: usize) -> String {
    let plot_width = width.saturating_sub(10).max(1);
    let plot_height = height.saturating_sub(2).max(1);
    let mut columns_low = vec![usize::MAX; plot_width];
    let mut columns_high = vec![0_usize; plot_width];
    let first = samples.first().map_or(0, |sample| sample.0);
    let last = samples
        .last()
        .map_or(first + 1, |sample| sample.0.max(first + 1));
    let min = samples
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::INFINITY, f64::min);
    let max = samples
        .iter()
        .map(|(_, value)| *value)
        .fold(f64::NEG_INFINITY, f64::max);
    let range = (max - min).max(f64::EPSILON);
    for &(timestamp, value) in samples {
        let x = (((timestamp.saturating_sub(first)) as f64 / (last - first) as f64)
            * (plot_width - 1) as f64)
            .round() as usize;
        let y = (((max - value) / range) * (plot_height - 1) as f64).round() as usize;
        let x = x.min(plot_width - 1);
        let y = y.min(plot_height - 1);
        columns_low[x] = columns_low[x].min(y);
        columns_high[x] = columns_high[x].max(y);
    }
    let mut lines = Vec::new();
    for row in 0..plot_height {
        let mut line = format!(
            "{max:>8.3} ",
            max = max - range * row as f64 / plot_height as f64
        );
        for column in 0..plot_width {
            let drawn = columns_low[column] != usize::MAX
                && columns_low[column] <= row
                && row <= columns_high[column];
            line.push(if drawn { '*' } else { ' ' });
        }
        lines.push(line);
    }
    lines.push(format!("{min:>8.3} {}", "-".repeat(plot_width)));
    lines.join("\n")
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
fn summary_matches_in(summary: &MessageSummary, filter: &str, buffer: &mut String) -> bool {
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
    summary_matches_in(summary, filter, &mut String::with_capacity(128))
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
fn footer_line(input: Option<InputKind>, buffer: &str, status: &str) -> String {
    let Some(kind) = input else {
        return status.to_string();
    };
    let prompt = match kind {
        InputKind::Filter => "/",
        InputKind::Binding => "bind (can|lin CHANNEL PATH): ",
        InputKind::DatabaseFilter => "db /",
        InputKind::Goto => "goto (10.5 | +10 | #1234): ",
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
const HELP_ENTRIES: &[HelpEntry] = &[
    heading("Messages (needs a trace)"),
    binding("j / Down", "next message"),
    binding("k / Up", "previous message"),
    binding("PageUp/PageDown", "move one screen"),
    binding("Home / End", "first / last message"),
    binding("Enter", "open the selected message"),
    binding("/", "filter messages"),
    binding("b", "bind a DBC or LDF database"),
    binding("p", "curve for the selected numeric signal"),
    binding("d", "open the database browser"),
    binding("e", "export filtered messages to CSV"),
    binding("g", "jump by trace time, +offset or #position"),
    binding("G", "jump to the last message"),
    binding("] / [", "next / previous occurrence of the frame id"),
    binding("Esc", "clear the message filter"),
    binding("q", "quit"),
    heading("Detail (needs a trace)"),
    binding("j / Down", "next signal"),
    binding("k / Up", "previous signal"),
    binding("PageUp/PageDown", "move one screen"),
    binding("Home / End", "first / last signal"),
    binding("p", "curve for the selected numeric signal"),
    binding("b", "bind a DBC or LDF database"),
    binding("e", "export filtered messages to CSV"),
    binding("Esc / Left", "back to the message list"),
    binding("q", "quit"),
    heading("Curve (needs a trace)"),
    binding("Left / Right", "pan the sample window"),
    binding("+ / = / -", "zoom in / out"),
    binding("PageUp/PageDown", "zoom in / out"),
    binding("Home / End", "first / last sample"),
    binding("Esc", "back to the message detail"),
    binding("q", "quit"),
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
    heading("Global"),
    binding("Ctrl-C", "quit the viewer"),
    binding("?", "open or close this help"),
    binding("Mouse", "click to select, wheel to scroll"),
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
        if field != name {
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
        if field != name {
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
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let (trace, mut index) = match args.first() {
        None => return Err(usage()),
        // Without a leading trace path the viewer still needs a database to browse.
        Some(first) if first.starts_with("--") => (None, 0),
        Some(trace) => (Some(PathBuf::from(trace)), 1),
    };
    let mut specs = Vec::new();
    while index < args.len() {
        let option = args[index].as_str();
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
    Ok(Args { trace, specs })
}

fn usage() -> String {
    "usage: vector-log-tui TRACE.blf [--dbc CHANNEL=FILE] [--ldf CHANNEL=FILE]\n       \
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

fn display_width(value: &str) -> usize {
    value.chars().map(char_width).sum()
}

fn truncate(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(value) <= width {
        return value.to_string();
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

/// Wrap `text` into lines of at most `width` display cells.
///
/// Wide glyphs count as two cells, the break lands on a space when the current
/// line has one, and a word longer than the width is split instead of dropped.
/// Continuation lines repeat the leading indentation of `text`, so a wrapped
/// detail item stays indented under its heading.
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
    let mut last_space = None;
    for character in text[indent..].chars() {
        let cells = char_width(character);
        if !line.is_empty() && line_width + cells > limit {
            match last_space.take() {
                // Break at the last space and keep the rest of the line.
                Some((index, before)) => {
                    lines.push(format!("{indent_text}{}", &line[..index]));
                    line = line[index + 1..].to_string();
                    line_width -= before + 1;
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
        if character == ' ' {
            last_space = Some((line.len() - 1, line_width - 1));
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(format!("{indent_text}{line}"));
    }
    lines
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
    fn curve_keeps_extrema_visible() {
        let chart = ascii_curve(&[(0, -10.0), (1, 50.0), (2, 10.0)], 30, 8);
        assert!(chart.contains("50.000"));
        assert!(chart.contains("-10.000"));
        assert!(chart.contains('*'));
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
        speed.receivers = vec![
            "TCM".to_string(),
            "Vector__XXX".to_string(),
            "TCM".to_string(),
            "Gateway".to_string(),
            String::new(),
        ];
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
            subscribers: vec![
                "TCM".to_string(),
                "Vector__XXX".to_string(),
                "Gateway".to_string(),
            ],
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
            subscribers: vec![
                "TCM".to_string(),
                "Vector__XXX".to_string(),
                "TCM".to_string(),
                "Gateway".to_string(),
                String::new(),
            ],
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
        assert!(heading.contains(ANSI_BRIGHT_CYAN));
        let item = paint_cell("  name: VehicleStatus", CellStyle::DetailItem);
        assert!(item.contains(ANSI_WHITE));
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

    #[test]
    fn details_offset_keeps_up_with_a_resized_column() {
        let mut app = test_database_app();
        app.viewport_width = 80;
        app.viewport_height = 24;
        app.set_database_category(1);
        app.database_object_selected = 0;
        app.database_pane = DatabasePane::Details;
        app.handle_event(InputEvent::End).unwrap();
        let narrow = app.database_detail_offset;
        assert!(narrow > 0);

        // A wider terminal wraps the same details into fewer lines.
        app.viewport_width = 200;
        let (_, _, width) = app.database_layout(200);
        let total = app.database_detail_lines(width).len();
        let rows = app.database_body_rows(24);
        assert!(total.saturating_sub(rows) < narrow);
        app.clamp_database_detail_offset();
        assert_eq!(app.database_detail_offset, total.saturating_sub(rows));
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
        let mut app = test_database_app();
        app.viewport_width = 80;
        app.viewport_height = 24;
        app.set_database_category(1);
        app.database_object_selected = 0;
        let (_, _, width) = app.database_layout(80);
        let total = app.database_detail_lines(width).len();
        let rows = app.database_body_rows(24);
        assert!(total > rows, "fixture details should overflow the pane");

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
        let mut app = test_database_app();
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
        let mut app = test_database_app();
        app.viewport_width = 80;
        app.viewport_height = 24;
        app.set_database_category(1);
        app.database_object_selected = 0;
        let (_, _, width) = app.database_layout(80);
        let total = app.database_detail_lines(width).len();
        let rows = app.database_body_rows(24);

        // 80 columns leave a narrow details column, so the range is shortened
        // instead of being cut off.
        let mut lines = Vec::new();
        app.render_databases(&mut lines, 80, 24);
        let header = details_header_cell(&lines[0]);
        assert_eq!(header, details_header_text(0, rows, total, width));
        assert_eq!(header, format!("lines 1-{} of {total}", rows.min(total)));

        // A wide column has room for the full label.
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
        app.database_pane = DatabasePane::Details;
        app.handle_event(InputEvent::End).unwrap();
        let mut lines = Vec::new();
        app.render_databases(&mut lines, 80, 24);
        let header = details_header_cell(&lines[0]);
        assert_eq!(
            header,
            details_header_text(total - rows, rows, total, width)
        );
        assert_eq!(
            header,
            format!("lines {}-{total} of {total}", total - rows + 1)
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
        app.render_messages(&mut lines, 80, 24);
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
        let value = paint("abcdef", ANSI_BRIGHT_GREEN);
        let truncated = truncate_ansi(&value, 5);
        assert!(truncated.starts_with(ANSI_BRIGHT_GREEN));
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
            ANSI_BRIGHT_GREEN,
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
            channel: Some(1),
            frame_id: Some(0x344),
            object_type_raw: 1,
            class_name: "CAN_MESSAGE",
            direction: Some("Rx"),
            dlc: Some(64),
            payload: vec![0xAB; 64],
            status: "ok",
        };
        let line = summary_line(&summary);
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
    fn bottom_line_shows_only_the_prompt_or_the_status() {
        assert_eq!(
            footer_line(None, "", "At 5.025000s (+3.510000s from start) (118 / 219)"),
            "At 5.025000s (+3.510000s from start) (118 / 219)"
        );
        let prompt = footer_line(Some(InputKind::Goto), "+5", "Ready");
        assert!(prompt.starts_with("goto ("), "got {prompt}");
        assert!(prompt.contains("+5"));
        // The old key cheat-sheet and the status no longer share the line.
        assert!(!prompt.contains("Ready"));
        assert!(!prompt.contains("[j/k]"));
        assert_eq!(
            footer_line(Some(InputKind::Filter), "fd", ""),
            "/fd  [Enter] apply  [Esc] cancel"
        );
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

    #[test]
    fn frame_labels_name_bus_channel_and_id() {
        let summary = MessageSummary {
            object_index: 0,
            timestamp_ns: Some(0),
            timestamp_source: None,
            bus: Some(BusKind::Can),
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
        let samples = (0..200)
            .map(|index| (index as u64, (index % 7) as f64))
            .collect::<Vec<_>>();
        let chart = ascii_curve(&samples, 30, 8);
        assert!(chart.contains('*'));
        let painted = chart.lines().filter(|line| line.contains('*')).count();
        assert!(painted > 2, "expected a painted span, got {painted} rows");
    }

    #[test]
    fn curve_window_bounds_are_stable() {
        let mut curve = CurveData {
            title: "Speed".to_string(),
            samples: (0..100).map(|index| (index, index as f64)).collect(),
            error: None,
            truncated: false,
            window_start: 0,
            window_len: 100,
        };
        let max_start = curve.samples.len().saturating_sub(curve.window_len);
        assert_eq!(max_start, 0);
        curve.window_len = 20;
        curve.window_start = 80;
        assert_eq!(curve.window_start + curve.window_len, 100);
        curve.window_start = curve.window_start.saturating_add(20).min(max_start);
        assert_eq!(curve.window_start, 0);
    }
}
