//! Terminal dashboard: a thin client over the daemon API. Owns pixels only —
//! every action is one RPC, and the list refreshes on daemon push events.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout as RLayout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde_json::{json, Value};

use rlord_core::config::{Config, Keys, Layout};
use rlord_core::db::SortMode;
use rlord_core::paths;
use rlord_core::timefmt::{fmt_ts, rel_age};
use rlord_core::tmux;
use rlord_daemon::client_call;

pub fn run(config: Config) -> Result<()> {
    // Fail fast with a readable error before touching the terminal.
    client_call(&paths::socket_path(), "ping", Value::Null)?;
    let keys = KeyMap::from_config(&config.keys)?;
    let rx = spawn_subscriber();
    let mut terminal = ratatui::init();
    let result = App::new(rx, config, keys).run(&mut terminal);
    ratatui::restore();
    result
}

/// Background thread: hold an `events.subscribe` connection and forward each
/// push as a unit tick. Reconnects with backoff if the daemon restarts.
fn spawn_subscriber() -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        if let Ok(mut stream) = UnixStream::connect(paths::socket_path()) {
            let req = json!({"id": 1, "method": "events.subscribe", "params": {}});
            if writeln!(stream, "{req}").is_ok() {
                let reader = BufReader::new(stream);
                for line in reader.lines() {
                    if line.is_err() || tx.send(()).is_err() {
                        break;
                    }
                }
            }
        }
        if tx.send(()).is_err() {
            return; // app gone
        }
        std::thread::sleep(Duration::from_secs(1));
    });
    rx
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Preview {
    Pane,
    Log,
}

enum Mode {
    Normal,
    Filter(String),
    Rename(String),
    NewSession(String),
}

struct App {
    rx: mpsc::Receiver<()>,
    config: Config,
    keys: KeyMap,
    sessions: Vec<Value>,
    table: TableState,
    show_archived: bool,
    sort: SortMode,
    filter: String,
    mode: Mode,
    preview: Preview,
    preview_text: String,
    status_line: String,
    dirty: bool,
    last_preview: Instant,
}

impl App {
    fn new(rx: mpsc::Receiver<()>, config: Config, keys: KeyMap) -> Self {
        App {
            rx,
            config,
            keys,
            sessions: Vec::new(),
            table: TableState::default().with_selected(0),
            show_archived: false,
            sort: SortMode::Activity,
            filter: String::new(),
            mode: Mode::Normal,
            preview: Preview::Pane,
            preview_text: String::new(),
            status_line: String::new(),
            dirty: true,
            last_preview: Instant::now() - Duration::from_secs(10),
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Option<Value> {
        match client_call(&paths::socket_path(), method, params) {
            Ok(v) => Some(v),
            Err(e) => {
                self.status_line = format!("error: {e:#}");
                None
            }
        }
    }

    fn refresh(&mut self) {
        let q = json!({
            "archived": if self.show_archived { "only" } else { "live" },
            "sort": self.sort.label(),
            "name": if self.filter.is_empty() { None } else { Some(self.filter.clone()) },
        });
        if let Some(v) = self.call("session.list", q) {
            self.sessions = v.as_array().cloned().unwrap_or_default();
            let n = self.sessions.len();
            let sel = self.table.selected().unwrap_or(0);
            self.table
                .select(if n == 0 { None } else { Some(sel.min(n - 1)) });
        }
        self.dirty = false;
        self.refresh_preview();
    }

    fn selected(&self) -> Option<&Value> {
        self.table.selected().and_then(|i| self.sessions.get(i))
    }

    fn selected_id(&self) -> Option<String> {
        self.selected()
            .and_then(|s| s["id"].as_str())
            .map(str::to_string)
    }

    fn refresh_preview(&mut self) {
        self.last_preview = Instant::now();
        let Some(s) = self.selected().cloned() else {
            self.preview_text.clear();
            return;
        };
        self.preview_text = match self.preview {
            Preview::Pane => {
                let running = s["status"] != "stopped";
                match s["tmux_session"].as_str() {
                    Some(name) if running => tmux::capture_pane(name)
                        .map(|t| t.trim_end().to_string())
                        .unwrap_or_else(|e| format!("(no pane: {e})")),
                    _ => "(session stopped — press r to restart)".to_string(),
                }
            }
            Preview::Log => {
                let id = s["id"].as_str().unwrap_or("").to_string();
                match client_call(
                    &paths::socket_path(),
                    "session.log",
                    json!({"ref": id, "limit": 40}),
                ) {
                    Ok(v) => v
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .map(|e| {
                                    format!(
                                        "{}  {:<22} {}",
                                        fmt_ts(e["ts"].as_i64().unwrap_or(0)),
                                        e["kind"].as_str().unwrap_or("?"),
                                        e["payload"]
                                            .as_object()
                                            .filter(|m| !m.is_empty())
                                            .map(|_| e["payload"].to_string())
                                            .unwrap_or_default()
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default(),
                    Err(e) => format!("(log unavailable: {e})"),
                }
            }
        };
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            if self.dirty {
                self.refresh();
            } else if self.last_preview.elapsed()
                > Duration::from_millis(self.config.preview_refresh_ms)
            {
                self.refresh_preview();
            }
            terminal.draw(|f| self.draw(f))?;

            let tick = Duration::from_millis(self.config.preview_refresh_ms.clamp(50, 250));
            if event::poll(tick)? {
                if let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                        return Ok(());
                    }
                    match std::mem::replace(&mut self.mode, Mode::Normal) {
                        Mode::Normal => {
                            if self.handle_normal(k, terminal)? {
                                return Ok(());
                            }
                        }
                        Mode::Filter(buf) => self.handle_input(k.code, buf, InputKind::Filter),
                        Mode::Rename(buf) => self.handle_input(k.code, buf, InputKind::Rename),
                        Mode::NewSession(buf) => {
                            self.handle_input(k.code, buf, InputKind::NewSession)
                        }
                    }
                }
            }
            // Daemon pushes: coalesce everything queued into one refresh.
            let mut pushed = false;
            while self.rx.try_recv().is_ok() {
                pushed = true;
            }
            if pushed {
                self.dirty = true;
            }
        }
    }

    /// Returns true to quit.
    fn handle_normal(
        &mut self,
        k: ratatui::crossterm::event::KeyEvent,
        terminal: &mut DefaultTerminal,
    ) -> Result<bool> {
        let n = self.sessions.len();
        let Some(action) = self.keys.action(&k) else {
            return Ok(false);
        };
        match action {
            Action::Quit => return Ok(true),
            Action::Down => {
                if n > 0 {
                    let i = self.table.selected().unwrap_or(0);
                    self.table.select(Some((i + 1).min(n - 1)));
                    self.refresh_preview();
                }
            }
            Action::Up => {
                if n > 0 {
                    let i = self.table.selected().unwrap_or(0);
                    self.table.select(Some(i.saturating_sub(1)));
                    self.refresh_preview();
                }
            }
            Action::Top => {
                if n > 0 {
                    self.table.select(Some(0));
                    self.refresh_preview();
                }
            }
            Action::Bottom => {
                if n > 0 {
                    self.table.select(Some(n - 1));
                    self.refresh_preview();
                }
            }
            Action::Attach => self.attach(terminal)?,
            Action::Restart => self.action("session.restart", "restarted"),
            Action::Stop => self.action("session.stop", "stopped"),
            Action::Archive => self.action("session.archive", "archived"),
            Action::Unarchive => self.action("session.unarchive", "unarchived"),
            Action::Pin => {
                if let Some(s) = self.selected().cloned() {
                    let pinned = !s["pinned"].as_bool().unwrap_or(false);
                    let id = s["id"].as_str().unwrap_or("").to_string();
                    self.call("session.pin", json!({"ref": id, "pinned": pinned}));
                    self.dirty = true;
                }
            }
            Action::ToggleArchived => {
                self.show_archived = !self.show_archived;
                self.table.select(Some(0));
                self.dirty = true;
            }
            Action::Sort => {
                self.sort = self.sort.next();
                self.dirty = true;
            }
            Action::ToggleLog => {
                self.preview = match self.preview {
                    Preview::Pane => Preview::Log,
                    Preview::Log => Preview::Pane,
                };
                self.refresh_preview();
            }
            Action::Filter => self.mode = Mode::Filter(self.filter.clone()),
            Action::Rename => {
                if let Some(s) = self.selected() {
                    let cur = s["name"].as_str().unwrap_or("").to_string();
                    self.mode = Mode::Rename(cur);
                }
            }
            Action::New => {
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.mode = Mode::NewSession(cwd);
            }
            Action::Cancel => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.dirty = true;
                }
            }
        }
        Ok(false)
    }

    fn action(&mut self, method: &str, verb: &str) {
        if let Some(id) = self.selected_id() {
            let name = self
                .selected()
                .and_then(|s| s["name"].as_str())
                .unwrap_or("")
                .to_string();
            if self.call(method, json!({"ref": id})).is_some() {
                self.status_line = format!("{verb} {name}");
            }
            self.dirty = true;
        }
    }

    fn handle_input(&mut self, code: KeyCode, mut buf: String, kind: InputKind) {
        match code {
            KeyCode::Esc => {}
            KeyCode::Enter => match kind {
                InputKind::Filter => {
                    self.filter = buf;
                    self.dirty = true;
                }
                InputKind::Rename => {
                    if let Some(id) = self.selected_id() {
                        if !buf.trim().is_empty()
                            && self
                                .call("session.rename", json!({"ref": id, "name": buf.trim()}))
                                .is_some()
                        {
                            self.status_line = format!("renamed to {}", buf.trim());
                        }
                        self.dirty = true;
                    }
                }
                InputKind::NewSession => {
                    if let Some(v) = self.call("session.new", json!({"project": buf.trim()})) {
                        self.status_line =
                            format!("launched {}", v["name"].as_str().unwrap_or("?"));
                    }
                    self.dirty = true;
                }
            },
            KeyCode::Backspace => {
                buf.pop();
                self.mode = kind.mode(buf);
            }
            KeyCode::Char(c) => {
                buf.push(c);
                if kind == InputKind::Filter {
                    // Live filtering as you type.
                    self.filter = buf.clone();
                    self.dirty = true;
                }
                self.mode = kind.mode(buf);
            }
            _ => self.mode = kind.mode(buf),
        }
    }

    /// Leave the TUI, hand the terminal to tmux, come back when it detaches.
    fn attach(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(s) = self.selected().cloned() else {
            return Ok(());
        };
        if s["status"] == "stopped" {
            self.status_line = "session is stopped — press r to restart first".into();
            return Ok(());
        }
        let Some(name) = s["tmux_session"].as_str() else {
            return Ok(());
        };
        let target = format!("={name}");
        if std::env::var_os("TMUX").is_some() {
            // Already inside tmux: switch this client; the TUI keeps running
            // in its own pane, and `nav home` (the back key) returns here.
            if let Some(home) = tmux::current_session() {
                let _ = tmux::set_home(&home);
            }
            let out = std::process::Command::new("tmux")
                .args(["switch-client", "-t", &target])
                .output()?;
            if !out.status.success() {
                self.status_line = String::from_utf8_lossy(&out.stderr).trim().to_string();
            }
            return Ok(());
        }
        // Outside tmux there is no home session: the back key must detach.
        let _ = tmux::set_home("");
        ratatui::restore();
        let status = std::process::Command::new("tmux")
            .args(["attach-session", "-t", &target])
            .status();
        *terminal = ratatui::init();
        terminal.clear()?;
        if let Err(e) = status {
            self.status_line = format!("attach failed: {e}");
        }
        self.dirty = true;
        Ok(())
    }

    fn draw(&mut self, f: &mut Frame) {
        let [main_area, footer_area] =
            RLayout::vertical([Constraint::Min(5), Constraint::Length(2)]).areas(f.area());
        let (list_area, preview_area) = match self.config.layout {
            Layout::Side => {
                let pct = self.config.list_width_pct.clamp(20, 80);
                let [l, p] = RLayout::horizontal([
                    Constraint::Percentage(pct),
                    Constraint::Percentage(100 - pct),
                ])
                .areas(main_area);
                (l, p)
            }
            Layout::Stack => {
                let [l, p] = RLayout::vertical([Constraint::Percentage(55), Constraint::Min(5)])
                    .areas(main_area);
                (l, p)
            }
        };
        self.draw_list(f, list_area);
        self.draw_preview(f, preview_area);
        self.draw_footer(f, footer_area);
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        // Drop the wide, least-essential columns first as the list narrows.
        let width = area.width.saturating_sub(4) as usize; // borders + marker
        let show_project = width >= 88;
        let show_tags = width >= 72;
        let name_w = if width >= 60 { 26 } else { 18 };

        let mut header = vec!["", "NAME", "STATUS", "AGENT", "USER"];
        let mut widths = vec![
            Constraint::Length(1),
            Constraint::Length(name_w),
            Constraint::Length(13),
            Constraint::Length(6),
            Constraint::Length(6),
        ];
        if show_tags {
            header.push("TAGS");
            widths.push(Constraint::Length(16));
        }
        if show_project {
            header.push("PROJECT");
            widths.push(Constraint::Min(10));
        }
        let header = Row::new(header)
            .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
        let home = std::env::var("HOME").unwrap_or_default();
        let rows: Vec<Row> = self
            .sessions
            .iter()
            .map(|s| {
                let get = |k: &str| s[k].as_str().unwrap_or("-").to_string();
                let status = get("status");
                let mut cells = vec![
                    Cell::from(if s["pinned"].as_bool().unwrap_or(false) {
                        "*"
                    } else {
                        " "
                    }),
                    Cell::from(get("name")),
                    Cell::from(Span::styled(status.clone(), status_style(&status))),
                    Cell::from(rel_age(s["last_agent_msg_at"].as_i64())),
                    Cell::from(rel_age(s["last_user_msg_at"].as_i64())),
                ];
                if show_tags {
                    let tags = s["tags"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_default();
                    cells.push(Cell::from(tags));
                }
                if show_project {
                    cells.push(Cell::from(get("project_path").replacen(&home, "~", 1)).dim());
                }
                Row::new(cells)
            })
            .collect();
        let title = format!(
            " {} sessions · sort: {}{}{} ",
            if self.show_archived {
                "archived"
            } else {
                "live"
            },
            self.sort.label(),
            if self.filter.is_empty() {
                String::new()
            } else {
                format!(" · filter: {}", self.filter)
            },
            if self.sessions.is_empty() {
                " · (none)"
            } else {
                ""
            }
        );
        let table = Table::new(rows, widths)
            .header(header)
            .block(Block::default().borders(Borders::ALL).title(title))
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_preview(&self, f: &mut Frame, area: Rect) {
        let name = self
            .selected()
            .and_then(|s| s["name"].as_str())
            .unwrap_or("");
        let title = match self.preview {
            Preview::Pane => format!(" {name} · pane (l: log) "),
            Preview::Log => format!(" {name} · event log (l: pane) "),
        };
        // Show the tail that fits: the bottom of a pane is what matters.
        let inner_h = area.height.saturating_sub(2) as usize;
        let lines: Vec<&str> = self.preview_text.lines().collect();
        let start = lines.len().saturating_sub(inner_h);
        let text: Vec<Line> = lines[start..].iter().map(|l| Line::raw(*l)).collect();
        let p = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false });
        f.render_widget(p, area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let prompt = match &self.mode {
            Mode::Normal => None,
            Mode::Filter(b) => Some(format!("filter: {b}▏")),
            Mode::Rename(b) => Some(format!("rename: {b}▏")),
            Mode::NewSession(b) => Some(format!("new session in: {b}▏")),
        };
        let line1 = match prompt {
            Some(p) => Line::from(Span::styled(p, Style::default().fg(Color::Yellow))),
            None => Line::from(
                self.keys
                    .legend()
                    .into_iter()
                    .map(|(k, label)| key(k, label))
                    .collect::<Vec<_>>(),
            ),
        };
        let line2 = Line::from(Span::styled(
            self.status_line.clone(),
            Style::default().fg(Color::Gray),
        ));
        f.render_widget(Paragraph::new(vec![line1, line2]), area);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Filter,
    Rename,
    NewSession,
}

impl InputKind {
    fn mode(self, buf: String) -> Mode {
        match self {
            InputKind::Filter => Mode::Filter(buf),
            InputKind::Rename => Mode::Rename(buf),
            InputKind::NewSession => Mode::NewSession(buf),
        }
    }
}

fn key(k: String, label: &str) -> Span<'static> {
    Span::raw(format!("{k}:{label} ")).fg(Color::Cyan)
}

fn status_style(status: &str) -> Style {
    let color = match status {
        "working" => Color::Green,
        "waiting_input" => Color::Yellow,
        "idle" => Color::Cyan,
        "error" => Color::Red,
        "starting" => Color::Magenta,
        _ => Color::DarkGray,
    };
    Style::default().fg(color)
}

// ---- configurable keys ----

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Quit,
    Up,
    Down,
    Top,
    Bottom,
    Attach,
    New,
    Restart,
    Stop,
    Archive,
    Unarchive,
    Rename,
    Pin,
    ToggleArchived,
    Sort,
    Filter,
    ToggleLog,
    Cancel,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct KeyBinding {
    code: KeyCode,
    ctrl: bool,
    alt: bool,
}

impl KeyBinding {
    /// Parse `ctrl-q`, `alt-x`, `enter`, `f5`, `R` (uppercase = shift).
    fn parse(spec: &str) -> Result<Self> {
        let mut ctrl = false;
        let mut alt = false;
        let mut rest = spec.trim();
        loop {
            let lower = rest.to_ascii_lowercase();
            if let Some(r) = lower.strip_prefix("ctrl-").or(lower.strip_prefix("c-")) {
                ctrl = true;
                rest = &rest[rest.len() - r.len()..];
            } else if let Some(r) = lower.strip_prefix("alt-").or(lower.strip_prefix("m-")) {
                alt = true;
                rest = &rest[rest.len() - r.len()..];
            } else {
                break;
            }
        }
        let code = match rest.to_ascii_lowercase().as_str() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backspace" => KeyCode::Backspace,
            "space" => KeyCode::Char(' '),
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "delete" | "del" => KeyCode::Delete,
            f if f.starts_with('f')
                && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) =>
            {
                KeyCode::F(f[1..].parse().unwrap())
            }
            _ => {
                let mut chars = rest.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => KeyCode::Char(if ctrl { c.to_ascii_lowercase() } else { c }),
                    _ => anyhow::bail!("unrecognised key '{spec}'"),
                }
            }
        };
        Ok(KeyBinding { code, ctrl, alt })
    }

    fn matches(&self, k: &ratatui::crossterm::event::KeyEvent) -> bool {
        if k.modifiers.contains(KeyModifiers::CONTROL) != self.ctrl
            || k.modifiers.contains(KeyModifiers::ALT) != self.alt
        {
            return false;
        }
        match (self.code, k.code) {
            // Shift on letters shows up as the uppercase char; compare exactly.
            (KeyCode::Char(a), KeyCode::Char(b)) => a == b,
            (a, b) => a == b,
        }
    }

    fn label(&self) -> String {
        let name = match self.code {
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Enter => "enter".into(),
            KeyCode::Esc => "esc".into(),
            KeyCode::Tab => "tab".into(),
            KeyCode::F(n) => format!("f{n}"),
            other => format!("{other:?}").to_lowercase(),
        };
        format!(
            "{}{}{name}",
            if self.ctrl { "^" } else { "" },
            if self.alt { "M-" } else { "" }
        )
    }
}

struct KeyMap {
    bindings: Vec<(Action, KeyBinding)>,
}

impl KeyMap {
    fn from_config(k: &Keys) -> Result<Self> {
        let specs = [
            (Action::Quit, &k.quit),
            (Action::Up, &k.up),
            (Action::Down, &k.down),
            (Action::Top, &k.top),
            (Action::Bottom, &k.bottom),
            (Action::Attach, &k.attach),
            (Action::New, &k.new),
            (Action::Restart, &k.restart),
            (Action::Stop, &k.stop),
            (Action::Archive, &k.archive),
            (Action::Unarchive, &k.unarchive),
            (Action::Rename, &k.rename),
            (Action::Pin, &k.pin),
            (Action::ToggleArchived, &k.toggle_archived),
            (Action::Sort, &k.sort),
            (Action::Filter, &k.filter),
            (Action::ToggleLog, &k.toggle_log),
            (Action::Cancel, &k.cancel),
        ];
        let mut bindings = Vec::with_capacity(specs.len());
        for (a, spec) in specs {
            bindings.push((a, KeyBinding::parse(spec)?));
        }
        // Arrow keys always work for movement, alongside whatever is configured.
        bindings.push((Action::Up, KeyBinding::parse("up")?));
        bindings.push((Action::Down, KeyBinding::parse("down")?));
        Ok(KeyMap { bindings })
    }

    fn action(&self, k: &ratatui::crossterm::event::KeyEvent) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(_, b)| b.matches(k))
            .map(|(a, _)| *a)
    }

    fn label(&self, a: Action) -> String {
        self.bindings
            .iter()
            .find(|(x, _)| *x == a)
            .map(|(_, b)| b.label())
            .unwrap_or_default()
    }

    fn legend(&self) -> Vec<(String, &'static str)> {
        [
            (Action::Attach, "attach"),
            (Action::New, "new"),
            (Action::Restart, "restart"),
            (Action::Stop, "stop"),
            (Action::Archive, "archive"),
            (Action::Unarchive, "unarchive"),
            (Action::Rename, "rename"),
            (Action::Pin, "pin"),
            (Action::ToggleArchived, "archived"),
            (Action::Sort, "sort"),
            (Action::Filter, "filter"),
            (Action::ToggleLog, "log"),
            (Action::Quit, "quit"),
        ]
        .into_iter()
        .map(|(a, l)| (self.label(a), l))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_specs_parse() {
        assert_eq!(KeyBinding::parse("q").unwrap().code, KeyCode::Char('q'));
        let b = KeyBinding::parse("ctrl-Q").unwrap();
        assert!(b.ctrl && b.code == KeyCode::Char('q'));
        assert_eq!(KeyBinding::parse("R").unwrap().code, KeyCode::Char('R'));
        assert_eq!(KeyBinding::parse("enter").unwrap().code, KeyCode::Enter);
        assert_eq!(KeyBinding::parse("f5").unwrap().code, KeyCode::F(5));
        assert!(KeyBinding::parse("alt-tab").unwrap().alt);
        assert!(KeyBinding::parse("bogus-key").is_err());
        assert!(KeyMap::from_config(&Keys::default()).is_ok());
    }
}
