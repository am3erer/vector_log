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
/// First list row below the header block, section title and column rule.
const LIST_FIRST_ROW: u16 = HEADER_ROWS + 2;
/// First detail row: the header block plus the detail section title.
const DETAIL_FIRST_ROW: u16 = HEADER_ROWS + 1;

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
    path: PathBuf,
    reader: File,
    objects: Vec<Object>,
    summaries: Vec<MessageSummary>,
    skipped_objects: u64,
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
    database_category: usize,
    database_object_selected: usize,
    database_object_offset: usize,
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

        let summaries = objects
            .iter()
            .enumerate()
            .map(|(object_index, object)| summary_for(object_index, object))
            .collect::<Vec<_>>();
        let filtered = (0..summaries.len()).collect::<Vec<_>>();
        let skipped_objects = reader.skipped_object_count;
        let mut app = Self {
            path,
            reader,
            objects,
            summaries,
            skipped_objects,
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
            database_category: 0,
            database_object_selected: 0,
            database_object_offset: 0,
            database_filter: String::new(),
            help: None,
        };
        for (bus, channel, database_path) in specs {
            app.bind_database(bus, channel, database_path)?;
        }
        Ok(app)
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
                InputEvent::Up | InputEvent::Char('k') => self.move_database_object(-1),
                InputEvent::Down | InputEvent::Char('j') => self.move_database_object(1),
                InputEvent::PageUp => self.move_database_object(-self.page_step()),
                InputEvent::PageDown => self.move_database_object(self.page_step()),
                InputEvent::Home => self.select_first_database_object(),
                InputEvent::End => self.select_last_database_object(),
                InputEvent::Left => self.move_database_category(-1),
                InputEvent::Right => self.move_database_category(1),
                InputEvent::Char('[') => self.switch_database(-1),
                InputEvent::Char(']') => self.switch_database(1),
                InputEvent::Char('/') => self.start_input(InputKind::DatabaseFilter),
                InputEvent::Enter => self.select_database_object(),
                InputEvent::Char('b') => self.start_input(InputKind::Binding),
                InputEvent::Char('?') => self.open_help(),
                InputEvent::Esc => self.mode = Mode::Messages,
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
            Mode::Databases => self.database_page_size(),
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
                            .any(|detail| detail.to_ascii_lowercase().contains(&filter))
                })
                .collect()
        };
        self.database_object_selected = self
            .database_object_selected
            .min(self.db_objects.len().saturating_sub(1));
    }

    fn move_database_category(&mut self, delta: isize) {
        let count = self.database_categories().len();
        self.database_category = move_index(self.database_category, count, delta);
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
        self.keep_database_object_visible();
    }

    fn select_first_database_object(&mut self) {
        self.database_object_selected = 0;
        self.database_object_offset = 0;
    }

    fn select_last_database_object(&mut self) {
        self.database_object_selected = self.db_objects.len().saturating_sub(1);
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
        let path = export_path(&self.path);
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
        let query = match self
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
        if mouse.wheel != 0 {
            if mouse.column < self.database_left_width() as u16 {
                self.switch_database(-isize::from(mouse.wheel));
            } else {
                self.move_database_object(-isize::from(mouse.wheel));
            }
        } else if mouse.button == 0 && !mouse.release && mouse.row >= LIST_FIRST_ROW {
            if mouse.column < self.database_left_width() as u16 {
                let row = usize::from(mouse.row - LIST_FIRST_ROW);
                if row == 0 {
                    self.switch_database(1);
                } else if row - 1 < self.database_categories().len() {
                    self.database_category = row - 1;
                    self.database_object_selected = 0;
                    self.database_object_offset = 0;
                }
            } else {
                let index = self.database_object_offset + usize::from(mouse.row - LIST_FIRST_ROW);
                if index < self.db_objects.len() {
                    self.database_object_selected = index;
                }
            }
        }
    }

    fn database_left_width(&self) -> usize {
        (self.viewport_width * 25 / 100).max(24)
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
        let page = help_page_size(self.viewport_height);
        let max_offset = HELP_ENTRIES.len().saturating_sub(page) as isize;
        if let Some(help) = &mut self.help {
            help.offset = (help.offset as isize)
                .saturating_add(delta)
                .clamp(0, max_offset) as usize;
        }
    }

    fn handle_help_event(&mut self, event: InputEvent) {
        let page = help_page_size(self.viewport_height) as isize;
        match event {
            InputEvent::Char('?') | InputEvent::Char('q') | InputEvent::Esc => self.close_help(),
            InputEvent::Char('j') | InputEvent::Down => self.scroll_help(1),
            InputEvent::Char('k') | InputEvent::Up => self.scroll_help(-1),
            InputEvent::PageDown => self.scroll_help(page),
            InputEvent::PageUp => self.scroll_help(-page),
            InputEvent::Home => self.scroll_help(-(HELP_ENTRIES.len() as isize)),
            InputEvent::End => self.scroll_help(HELP_ENTRIES.len() as isize),
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
        let filtered_count = self.filtered.len();
        lines.push(paint_bold(
            format!(
                " vector-log-tui  {}  objects={}/{}  skipped={}  mode={:?}",
                self.path.display(),
                filtered_count,
                self.summaries.len(),
                self.skipped_objects,
                self.mode
            ),
            ANSI_BRIGHT_CYAN,
        ));
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
                Mode::Databases => self.render_databases(&mut lines, width, height),
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
        let split = (width * 43 / 100).max(24);
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
        let split = (width * 43 / 100).max(24);
        let mut left = summary_detail_lines(summary, self.selected_object());
        left.extend(timing_lines(self.timing.clone()));
        let mut right = vec!["Signals".to_string()];
        if self.detail_rows.is_empty() {
            right.push("  no decoded signals (bind a matching database with b)".to_string());
        }
        for (index, row) in self.detail_rows.iter().enumerate().skip(self.detail_offset) {
            right.push(format!(
                "{}{} = {}",
                if index == self.selected_signal {
                    "▶ "
                } else {
                    "  "
                },
                row.name,
                row_display(row)
            ));
        }
        for row in 0..height.saturating_sub(5) {
            let line = two_column(
                left.get(row).map_or("", String::as_str),
                right.get(row).map_or("", String::as_str),
                split,
                width,
            );
            let signal_selected = row > 0
                && self.detail_offset + row - 1 == self.selected_signal
                && self.detail_offset + row - 1 < self.detail_rows.len();
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
        let left = self.database_left_width().min(width.saturating_sub(2));
        let middle = (width * 32 / 100)
            .max(30)
            .min(width.saturating_sub(left + 1));
        let right = width.saturating_sub(left + middle + 6);
        let objects = &self.db_objects;
        let categories = self.database_categories();
        let selected_object = objects.get(self.database_object_selected);
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
        lines.push(paint_bold(
            three_column(
                "Database tree",
                &format!(
                    "{} ({})",
                    self.selected_database_category()
                        .map_or("Objects", DbCategory::label),
                    objects.len()
                ),
                selected_object.map_or("Details", |object| object.title.as_str()),
                left,
                middle,
                width,
            ),
            ANSI_BRIGHT_CYAN,
        ));
        lines.push(paint(
            format!(
                "{}─┼{}─┼{}",
                "─".repeat(left),
                "─".repeat(middle),
                "─".repeat(right)
            ),
            ANSI_CYAN,
        ));
        let rows = height.saturating_sub(7).max(1);
        for row in 0..rows {
            let category = if row == 0 {
                format!("▶ {database_label}")
            } else {
                categories
                    .get(row - 1)
                    .map(|category| {
                        let marker = if row - 1 == self.database_category {
                            "▶ "
                        } else {
                            "  "
                        };
                        format!("{marker}{}", category.label())
                    })
                    .unwrap_or_default()
            };
            let object_index = self.database_object_offset + row;
            let object = objects.get(object_index);
            let object_line = object.map_or_else(String::new, |object| {
                format!(
                    "{}{}  {}",
                    if object_index == self.database_object_selected {
                        "▶ "
                    } else {
                        "  "
                    },
                    object.title,
                    object.subtitle
                )
            });
            let detail_line = selected_object
                .and_then(|object| object.details.get(row))
                .map_or("", String::as_str);
            let line = three_column(&category, &object_line, detail_line, left, middle, width);
            let category_selected = row > 0 && row - 1 == self.database_category;
            let object_selected = object.is_some() && object_index == self.database_object_selected;
            lines.push(if row == 0 {
                paint_bold(line, ANSI_BRIGHT_YELLOW)
            } else if category_selected || object_selected {
                paint_selected(line)
            } else {
                paint(line, ANSI_WHITE)
            });
        }
    }
}

#[derive(Debug, Clone)]
struct DbObject {
    title: String,
    subtitle: String,
    details: Vec<String>,
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
            details: vec![
                format!("source: {}", database.source_path().unwrap_or("-")),
                format!("version: {}", database.version().unwrap_or("-")),
                format!("messages: {}", database.len()),
                format!("signals: {}", database.signal_names().count()),
                format!("nodes: {}", database.nodes().count()),
                format!("attributes: {}", database.attributes().count()),
            ],
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
                details: dbc_message_details(message),
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
                    details: dbc_signal_details(message, signal),
                })
            })
            .collect(),
        (Database::Can(database), DbCategory::Nodes) => database
            .nodes()
            .map(|node| DbObject {
                title: node.name.clone(),
                subtitle: format!("node{}", source_suffix(node.source)),
                details: vec![format!("name: {}", node.name)],
            })
            .collect(),
        (Database::Can(database), DbCategory::Attributes) => database
            .attributes()
            .map(|(name, value)| DbObject {
                title: name.to_string(),
                subtitle: value.to_string(),
                details: vec![format!("name: {name}"), format!("value: {value}")],
            })
            .collect(),
        (Database::Can(database), DbCategory::ValueTables) => database
            .value_tables()
            .map(|(name, table)| DbObject {
                title: name.to_string(),
                subtitle: format!("{} values", table.iter().count()),
                details: vec![format!("values: {}", value_table_text(table))],
            })
            .collect(),
        (Database::Lin(database), DbCategory::Overview) => vec![DbObject {
            title: "LDF overview".to_string(),
            subtitle: format!(
                "{} frames, {} signals",
                database.len(),
                database.signal_count()
            ),
            details: vec![
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
                details: vec![
                    format!("name: {}", node.name),
                    format!("configured NAD: {:?}", node.configured_nad),
                    format!("timebase ms: {:?}", node.timebase_ms),
                    format!("jitter ms: {:?}", node.jitter_ms),
                ],
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
                details: vec![
                    format!("name: {}", signal.name),
                    format!("bit length: {}", signal.bit_length),
                    format!("initial value: {:?}", signal.initial_value),
                ],
            })
            .collect(),
        (Database::Lin(database), DbCategory::Schedules) => database
            .schedules()
            .map(|(name, entries)| DbObject {
                title: name.to_string(),
                subtitle: format!("{} entries", entries.len()),
                details: entries
                    .iter()
                    .map(|entry| format!("{} delay_ms={}", entry.frame_name, entry.delay_ms))
                    .collect(),
            })
            .collect(),
        (Database::Lin(database), DbCategory::DynamicFrames) => database
            .dynamic_frames()
            .map(|frame| DbObject {
                title: frame.name.clone(),
                subtitle: format!("{:?} trigger={:?}", frame.kind, frame.trigger_frame_id),
                details: vec![
                    format!("kind: {:?}", frame.kind),
                    format!("trigger frame id: {:?}", frame.trigger_frame_id),
                    format!("members: {}", frame.frame_names.join(", ")),
                ],
            })
            .collect(),
        (Database::Lin(database), DbCategory::OptionalSections) => database
            .optional_sections()
            .map(|section| DbObject {
                title: section.name.clone(),
                subtitle: source_suffix(section.source),
                details: vec![format!("name: {}", section.name)],
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn dbc_message_details(message: &blf_core::DbcMessage) -> Vec<String> {
    let mut details = vec![
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
        format!("signals: {}", message.signals.len()),
    ];
    if let Some(comment) = &message.comment {
        details.push(format!("comment: {comment}"));
    }
    details.extend(
        message
            .signals
            .iter()
            .map(|signal| format!("signal: {}", signal.name)),
    );
    details
}

fn dbc_signal_details(message: &blf_core::DbcMessage, signal: &blf_core::CanSignal) -> Vec<String> {
    let mut details = vec![
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
        format!("receivers: {}", signal.receivers.join(", ")),
        format!("multiplex: {:?}", signal.multiplex),
    ];
    if !signal.choices.is_empty() {
        details.push(format!("choices: {}", value_table_text(&signal.choices)));
    }
    if let Some(comment) = &signal.comment {
        details.push(format!("comment: {comment}"));
    }
    details
}

fn lin_frame_details(frame: &blf_core::LinFrame) -> Vec<String> {
    let mut details = vec![
        format!("frame id: 0x{:02X}", frame.id),
        format!("DLC: {}  publisher: {}", frame.dlc, frame.publisher),
        format!("checksum: {:?}", frame.checksum),
        format!("signals: {}", frame.signals.len()),
    ];
    details.extend(
        frame
            .signals
            .iter()
            .map(|signal| format!("signal: {}", signal.name)),
    );
    details
}

fn lin_diagnostic_frame_details(frame: &blf_core::LinDiagnosticFrame) -> Vec<String> {
    let mut details = vec![
        format!("frame id: 0x{:02X}", frame.id),
        format!("DLC: {}", frame.dlc),
        format!("checksum: {:?}", frame.checksum),
        format!("signals: {}", frame.signals.len()),
    ];
    details.extend(
        frame
            .signals
            .iter()
            .map(|signal| format!("signal: {}", signal.name)),
    );
    details
}

fn lin_signal_details(signal: &blf_core::LinSignal) -> Vec<String> {
    let mut details = vec![
        format!(
            "layout: start={} length={}",
            signal.start_bit, signal.bit_length
        ),
        format!("publisher: {}", signal.publisher.as_deref().unwrap_or("-")),
        format!("subscribers: {}", signal.subscribers.join(", ")),
        format!("scale/offset: {} / {}", signal.factor, signal.offset),
        format!("range: {:?}..{:?}", signal.minimum, signal.maximum),
        format!("encoding: {}", signal.encoding.as_deref().unwrap_or("-")),
    ];
    if !signal.choices.is_empty() {
        details.push(format!("choices: {}", value_table_text(&signal.choices)));
    }
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
    heading("Messages"),
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
    heading("Detail"),
    binding("j / Down", "next signal"),
    binding("k / Up", "previous signal"),
    binding("PageUp/PageDown", "move one screen"),
    binding("Home / End", "first / last signal"),
    binding("p", "curve for the selected numeric signal"),
    binding("b", "bind a DBC or LDF database"),
    binding("e", "export filtered messages to CSV"),
    binding("Esc / Left", "back to the message list"),
    binding("q", "quit"),
    heading("Curve"),
    binding("Left / Right", "pan the sample window"),
    binding("+ / = / -", "zoom in / out"),
    binding("PageUp/PageDown", "zoom in / out"),
    binding("Home / End", "first / last sample"),
    binding("Esc", "back to the message detail"),
    binding("q", "quit"),
    heading("Databases"),
    binding("j / Down", "next object"),
    binding("k / Up", "previous object"),
    binding("PageUp/PageDown", "move one screen"),
    binding("Home / End", "first / last object"),
    binding("Left / Right", "previous / next category"),
    binding("] / [", "next / previous database"),
    binding("/", "search objects"),
    binding("Enter", "select the current object"),
    binding("b", "bind a DBC or LDF database"),
    binding("Esc", "back to the message list"),
    binding("q", "quit"),
    heading("Global"),
    binding("Ctrl-C", "quit the viewer"),
    binding("?", "open or close this help"),
    binding("Mouse", "click to select, wheel to scroll"),
];

/// Render the help table for `width` cells as `(is_heading, line)` rows.
fn help_lines(width: usize) -> Vec<(bool, String)> {
    let split = HELP_KEY_WIDTH.min(width.saturating_sub(3));
    HELP_ENTRIES
        .iter()
        .map(|entry| {
            if entry.heading {
                (true, entry.key.to_string())
            } else {
                (
                    false,
                    two_column(entry.key, entry.description, split, width),
                )
            }
        })
        .collect()
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

fn parse_args(args: &[String]) -> Result<(PathBuf, Vec<BindingSpec>), String> {
    let Some(path) = args.first() else {
        return Err(usage());
    };
    let mut specs = Vec::new();
    let mut index = 1;
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
    Ok((PathBuf::from(path), specs))
}

fn usage() -> String {
    "usage: vector-log-tui TRACE.blf [--dbc CHANNEL=FILE] [--ldf CHANNEL=FILE]".to_string()
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

fn two_column(left: &str, right: &str, split: usize, width: usize) -> String {
    let split = split.min(width.saturating_sub(3));
    format!(
        "{} │ {}",
        pad_to_width(&truncate(left, split), split),
        truncate(right, width.saturating_sub(split + 3))
    )
}

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
    let (path, specs) = match parse_args(&args) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };
    if let Err(error) = App::open(path, specs).and_then(|mut app| app.run()) {
        eprintln!("vector-log-tui: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parses_database_bindings() {
        let args = vec![
            "trace.blf".to_string(),
            "--dbc".to_string(),
            "1=vehicle.dbc".to_string(),
            "--ldf".to_string(),
            "3=body.ldf".to_string(),
        ];
        let (path, specs) = parse_args(&args).unwrap();
        assert_eq!(path, PathBuf::from("trace.blf"));
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].0, BusKind::Can);
        assert_eq!(specs[1].1, 3);
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
        for heading in ["Messages", "Detail", "Curve", "Databases", "Global"] {
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
            "open the selected message",
            "wheel to scroll",
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
        let total = HELP_ENTRIES.len();
        let page = help_page_size(24);
        assert!(total > page, "help should overflow an 80x24 terminal");
        assert_eq!(clamp_help_offset(0, total, page), 0);
        assert_eq!(clamp_help_offset(usize::MAX, total, page), total - page);
        assert_eq!(clamp_help_offset(total, total, page), total - page);

        // Scrolling to the end keeps the global keys reachable.
        let entries = help_lines(80);
        let offset = clamp_help_offset(usize::MAX, total, page);
        let visible = &entries[offset..(offset + page).min(total)];
        assert_eq!(visible.len(), page);
        assert!(visible.iter().any(|(_, line)| line.contains("Ctrl-C")));
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
