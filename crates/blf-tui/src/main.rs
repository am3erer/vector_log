use std::env;
use std::fmt::Write as _;
use std::fs::{File as StdFile, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use blf_core::{
    can_flags_direction, classify_lin_object, decode_lin_object, decode_message,
    object_timestamp_ns, object_timestamp_source, BusKind, Database, DatabaseBinding, DbcDatabase,
    FieldValue, File, LdfDatabase, Object,
};

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
    class_name: String,
    direction: Option<String>,
    dlc: Option<u8>,
    payload: Vec<u8>,
    status: String,
}

#[derive(Debug, Clone)]
struct SignalRow {
    name: String,
    raw: Option<i128>,
    physical: Option<f64>,
    choice: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct CurveData {
    title: String,
    samples: Vec<(u64, f64)>,
    error: Option<String>,
    window_start: usize,
    window_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Messages,
    Detail,
    Curve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Filter,
    Binding,
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
}

impl App {
    fn open(path: impl Into<PathBuf>, specs: Vec<(BusKind, u16, PathBuf)>) -> Result<Self, String> {
        let path = path.into();
        let mut reader = File::open(&path).map_err(|error| format!("open BLF: {error}"))?;
        let mut objects = Vec::new();
        loop {
            let batch = reader
                .read_batch(4096)
                .map_err(|error| format!("read BLF: {error}"))?;
            if batch.is_empty() {
                break;
            }
            objects.extend(batch);
        }

        let summaries = objects
            .iter()
            .enumerate()
            .map(|(object_index, object)| summary_for(object_index, object))
            .collect();
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
            self.render(terminal)?;
            if let Some(event) = terminal.read_event()? {
                if self.handle_event(event)? {
                    break;
                }
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }

    fn handle_event(&mut self, event: InputEvent) -> Result<bool, String> {
        if matches!(event, InputEvent::CtrlC | InputEvent::Eof) {
            return Ok(true);
        }
        if self.input.is_some() {
            return self.handle_input_event(event);
        }
        match self.mode {
            Mode::Messages => match event {
                InputEvent::Char('q') => return Ok(true),
                InputEvent::Up | InputEvent::Char('k') => self.move_selection(-1),
                InputEvent::Down | InputEvent::Char('j') => self.move_selection(1),
                InputEvent::PageUp => self.move_selection(-10),
                InputEvent::PageDown => self.move_selection(10),
                InputEvent::Home => self.select_first(),
                InputEvent::End => self.select_last(),
                InputEvent::Enter => self.enter_detail(),
                InputEvent::Char('/') => self.start_input(InputKind::Filter),
                InputEvent::Char('b') => self.start_input(InputKind::Binding),
                InputEvent::Char('p') => self.open_curve()?,
                InputEvent::Esc => self.clear_filter(),
                InputEvent::Mouse(mouse) => self.handle_message_mouse(mouse),
                _ => {}
            },
            Mode::Detail => match event {
                InputEvent::Char('q') => return Ok(true),
                InputEvent::Up | InputEvent::Char('k') => self.move_signal(-1),
                InputEvent::Down | InputEvent::Char('j') => self.move_signal(1),
                InputEvent::PageUp => self.move_signal(-10),
                InputEvent::PageDown => self.move_signal(10),
                InputEvent::Home => self.select_first_signal(),
                InputEvent::End => self.select_last_signal(),
                InputEvent::Char('p') => self.open_curve()?,
                InputEvent::Char('b') => self.start_input(InputKind::Binding),
                InputEvent::Esc | InputEvent::Left => self.mode = Mode::Messages,
                InputEvent::Mouse(mouse) => self.handle_detail_mouse(mouse),
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
                            self.status = format!("Filter: {}", display_filter(&self.filter));
                        }
                        InputKind::Binding => {
                            if let Err(error) = self.apply_binding_input(&value) {
                                self.status = error;
                                self.input = Some(InputKind::Binding);
                                self.input_buffer = value;
                            }
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
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.filtered_indices().len();
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

    fn select_last(&mut self) {
        self.selected = self.filtered_indices().len().saturating_sub(1);
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
        self.viewport_height.saturating_sub(7).max(1)
    }

    fn detail_page_size(&self) -> usize {
        self.viewport_height.saturating_sub(5).max(1)
    }

    fn clear_filter(&mut self) {
        self.filter.clear();
        self.selected = 0;
        self.list_offset = 0;
        self.status = "Filter cleared".to_string();
    }

    fn selected_summary(&self) -> Option<&MessageSummary> {
        let indices = self.filtered_indices();
        indices
            .get(self.selected)
            .and_then(|index| self.summaries.get(*index))
    }

    fn selected_object(&self) -> Option<&Object> {
        self.selected_summary()
            .and_then(|summary| self.objects.get(summary.object_index))
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let filter = self.filter.to_ascii_lowercase();
        self.summaries
            .iter()
            .enumerate()
            .filter(|(_, summary)| filter.is_empty() || summary_matches(summary, &filter))
            .map(|(index, _)| index)
            .collect()
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
        let Some(summary) = self.selected_summary().cloned() else {
            return;
        };
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

    fn open_curve(&mut self) -> Result<(), String> {
        if self.mode == Mode::Messages {
            self.enter_detail();
        }
        if self.detail_rows.is_empty() {
            self.status = "Select a decoded numeric signal first".to_string();
            return Ok(());
        }
        let Some(summary) = self.selected_summary().cloned() else {
            return Ok(());
        };
        let Some(binding) = self.binding_for_summary(&summary).cloned() else {
            self.status = "No database bound for this bus/channel".to_string();
            return Ok(());
        };
        let row = &self.detail_rows[self.selected_signal];
        let signal_name = qualified_signal_name(
            binding.database(),
            &row.name,
            &self.objects[summary.object_index],
        );
        let query = self
            .reader
            .signal_query(binding, signal_name.clone(), false)
            .map_err(|error| format!("start signal query: {error}"))?;
        let samples = query
            .collect()
            .map_err(|error| format!("read signal query: {error}"))?;
        let curve_samples = samples
            .iter()
            .filter_map(|sample| {
                sample
                    .physical_value
                    .map(|value| (sample.timestamp_ns, value))
            })
            .collect::<Vec<_>>();
        self.curve = Some(CurveData {
            title: signal_name,
            window_len: curve_samples.len().max(1),
            samples: curve_samples,
            error: samples.iter().find_map(|sample| sample.error.clone()),
            window_start: 0,
        });
        self.mode = Mode::Curve;
        Ok(())
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
        } else if mouse.button == 0 && !mouse.release && mouse.row >= 5 {
            let row = usize::from(mouse.row - 5);
            let index = self.list_offset + row;
            if index < self.filtered_indices().len() {
                self.selected = index;
                self.selected_signal = 0;
            }
        }
    }

    fn handle_detail_mouse(&mut self, mouse: MouseEvent) {
        if mouse.wheel != 0 {
            self.move_signal(-isize::from(mouse.wheel));
        } else if mouse.button == 0 && !mouse.release && mouse.row >= 4 {
            let split = (usize::from(mouse.column) * 43 / 100).max(24);
            if usize::from(mouse.column) > split {
                let index = self.detail_offset + usize::from(mouse.row - 4);
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

    fn render(&mut self, terminal: &mut RawTerminal) -> Result<(), String> {
        if self.last_size_check.elapsed() >= Duration::from_millis(250) {
            let (width, height) = terminal.size();
            self.viewport_width = width;
            self.viewport_height = height;
            self.last_size_check = Instant::now();
        }
        let width = self.viewport_width;
        let height = self.viewport_height;
        let mut lines = Vec::new();
        let filtered_count = self.filtered_indices().len();
        lines.push(format!(
            " vector-log-tui  {}  objects={}/{}  skipped={}  mode={:?}",
            self.path.display(),
            filtered_count,
            self.summaries.len(),
            self.skipped_objects,
            self.mode
        ));
        lines.push(
            " databases: ".to_string()
                + &if self.bindings.is_empty() {
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
                },
        );
        lines.push("─".repeat(width));
        match self.mode {
            Mode::Messages => self.render_messages(&mut lines, width, height),
            Mode::Detail => self.render_detail(&mut lines, width, height),
            Mode::Curve => self.render_curve(&mut lines, width, height),
        }
        lines.push("─".repeat(width));
        let footer = if let Some(kind) = self.input {
            let prompt = match kind {
                InputKind::Filter => "/",
                InputKind::Binding => "bind (can|lin CHANNEL PATH): ",
            };
            format!("{prompt}{}  [Enter] apply  [Esc] cancel", self.input_buffer)
        } else {
            match self.mode {
                Mode::Messages => {
                    "[j/k] move [Enter] detail [/] filter [b] bind [p] curve [q] quit"
                }
                Mode::Detail => "[j/k] signal [p] curve [b] bind [Esc] back [q] quit",
                Mode::Curve => "[Esc] back [q] quit",
            }
            .to_string()
        };
        lines.push(format!("{footer}  {}", self.status));
        let mut output = String::from("\x1b[2J\x1b[H");
        for line in lines.into_iter().take(height) {
            output.push_str(&truncate(&line, width));
            output.push('\n');
        }
        terminal.write(&output).map_err(|error| error.to_string())
    }

    fn render_messages(&self, lines: &mut Vec<String>, width: usize, height: usize) {
        let indices = self.filtered_indices();
        let split = (width * 43 / 100).max(24);
        lines.push(format!(
            "{:<split$} │ Preview",
            format!("Messages ({}/{})", indices.len(), self.summaries.len())
        ));
        lines.push(format!(
            "{}─┼{}",
            "─".repeat(split),
            "─".repeat(width.saturating_sub(split + 1))
        ));
        let available = height.saturating_sub(7);
        let preview = self
            .selected_summary()
            .map(|summary| summary_detail_lines(summary, self.selected_object()))
            .unwrap_or_else(|| vec!["No matching messages".to_string()]);
        for row in 0..available {
            let message_index = self.list_offset + row;
            let left = indices
                .get(message_index)
                .and_then(|index| self.summaries.get(*index))
                .map(|summary| {
                    format!(
                        "{}{}",
                        if message_index == self.selected {
                            "▶ "
                        } else {
                            "  "
                        },
                        summary_line(summary)
                    )
                })
                .unwrap_or_default();
            let right = preview.get(row).cloned().unwrap_or_default();
            lines.push(format!(
                "{:<split$} │ {}",
                truncate(&left, split),
                truncate(&right, width.saturating_sub(split + 3))
            ));
        }
    }

    fn render_detail(&self, lines: &mut Vec<String>, width: usize, height: usize) {
        let Some(summary) = self.selected_summary() else {
            lines.push("No message selected".to_string());
            return;
        };
        let split = (width * 43 / 100).max(24);
        let left = summary_detail_lines(summary, self.selected_object());
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
            lines.push(format!(
                "{:<split$} │ {}",
                truncate(left.get(row).map_or("", String::as_str), split),
                truncate(
                    right.get(row).map_or("", String::as_str),
                    width.saturating_sub(split + 3)
                )
            ));
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
        lines.push(format!(
            "Curve: {}  samples={}/{}  window={}-{}",
            curve.title,
            visible.len(),
            curve.samples.len(),
            start,
            end
        ));
        if let Some(error) = &curve.error {
            lines.push(format!("decode note: {error}"));
        }
        if visible.is_empty() {
            lines.push("No numeric samples available".to_string());
            return;
        }
        lines.extend(
            ascii_curve(visible, width.saturating_sub(2), height.saturating_sub(6))
                .lines()
                .map(str::to_string),
        );
    }
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
        Object::Lin(value) => lin_status(classify_lin_object(value).state).to_string(),
        Object::CanMessage(_)
        | Object::CanMessage2(_)
        | Object::CanFdMessage(_)
        | Object::CanFdMessage64(_) => "ok".to_string(),
        _ => "event".to_string(),
    };
    MessageSummary {
        object_index,
        timestamp_ns: object_timestamp_ns(object),
        timestamp_source: object_timestamp_source(object),
        bus,
        channel,
        frame_id,
        object_type_raw: object.object_type_raw(),
        class_name: object.object_type().name().to_string(),
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
        summary.direction.as_deref().unwrap_or("-"),
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

fn summary_line(summary: &MessageSummary) -> String {
    let time = format_timestamp(summary.timestamp_ns);
    let bus = summary.bus.map_or("--", BusKind::as_str);
    let channel = summary
        .channel
        .map_or_else(|| "-".to_string(), |value| value.to_string());
    let id = summary
        .frame_id
        .map_or_else(|| "------".to_string(), |value| format!("0x{value:03X}"));
    let direction = summary.direction.as_deref().unwrap_or("-");
    let payload = hex_bytes(&summary.payload);
    let mut text = format!("{time:>12} {bus:<3} {channel:>3} {id:>6} {direction:<3} {payload}");
    if text.len() > 120 {
        text.truncate(117);
        text.push_str("...");
    }
    text
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
    let mut columns = vec![Vec::new(); plot_width];
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
        if let Some(column) = columns.get_mut(x.min(plot_width - 1)) {
            column.push(y.min(plot_height - 1));
        }
    }
    let mut lines = Vec::new();
    for row in 0..plot_height {
        let mut line = format!(
            "{max:>8.3} ",
            max = max - range * row as f64 / plot_height as f64
        );
        for column in &columns {
            line.push(if column.contains(&row) { '*' } else { ' ' });
        }
        lines.push(line);
    }
    lines.push(format!("{min:>8.3} {}", "-".repeat(plot_width)));
    lines.join("\n")
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

fn summary_matches(summary: &MessageSummary, filter: &str) -> bool {
    let haystack = format!(
        "{} {} {} {} {} {} {} {} {}",
        summary.class_name,
        summary.bus.map_or("", BusKind::as_str),
        summary
            .channel
            .map_or_else(|| "".to_string(), |value| value.to_string()),
        summary
            .frame_id
            .map_or_else(|| "".to_string(), |value| format!("{value:X}")),
        summary.direction.as_deref().unwrap_or(""),
        hex_bytes(&summary.payload),
        summary.status,
        summary.object_type_raw,
        summary.class_name,
    )
    .to_ascii_lowercase();
    haystack.contains(filter)
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

fn move_index(index: usize, count: usize, delta: isize) -> usize {
    if count == 0 {
        return 0;
    }
    let index = index.min(count - 1) as isize + delta;
    index.clamp(0, count as isize - 1) as usize
}

fn can_direction(value: u8) -> String {
    match value {
        0 => "Rx",
        1 => "Tx",
        _ => "?",
    }
    .to_string()
}

fn lin_direction(value: u8) -> String {
    match value {
        0 => "Rx",
        1 => "Tx",
        2 => "TxRq",
        _ => "?",
    }
    .to_string()
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

fn truncate(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut chars = value.chars();
    let visible = chars.by_ref().take(width).collect::<String>();
    if chars.next().is_none() {
        return visible;
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let mut truncated = value.chars().take(width - 3).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
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
    fn truncates_by_character_without_splitting_utf8() {
        assert_eq!(truncate("abcdef", 0), "");
        assert_eq!(truncate("abcdef", 3), "...");
        assert_eq!(truncate("abcdef", 5), "ab...");
        assert_eq!(truncate("信号曲线", 3), "...");
        assert_eq!(truncate("信号曲线", 4), "信号曲线");
        assert_eq!(truncate("信号", 4), "信号");
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
    fn curve_window_bounds_are_stable() {
        let mut curve = CurveData {
            title: "Speed".to_string(),
            samples: (0..100).map(|index| (index, index as f64)).collect(),
            error: None,
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
