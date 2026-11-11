//! Terminal dashboard: a thin client over the daemon API. Owns pixels only —
//! every action is one RPC, and the list refreshes on daemon push events.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout as RLayout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, ListState, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};

mod groups;
mod newform;
mod overlay;
mod overlord;
mod theme;
use groups::Tab;
use newform::NewForm;
pub use overlay::run as run_overlay;
use serde_json::{json, Value};

use rlord_core::config::{Config, Keys, Layout};
use rlord_core::db::{SessionQuery, SortMode};
use rlord_core::paths;
use rlord_core::timefmt::{fmt_ts, rel_age};
use rlord_core::tmux;
use rlord_daemon::client_call;

pub fn run(config: Config) -> Result<()> {
    // Start (or upgrade) the daemon, failing with a readable error before
    // touching the terminal.
    rlord_daemon::autostart::ensure_daemon(&paths::socket_path())?;
    let keys = KeyMap::from_config(&config.keys)?;
    let rx = spawn_subscriber();
    let mut terminal = init_terminal();
    let result = App::new(rx, config, keys).run(&mut terminal);
    restore_terminal();
    result
}

/// `ratatui::init` plus xterm modifyOtherKeys (mode 1), so modified keys
/// such as Shift-Enter arrive distinct from plain ones (rlord's tmux has
/// `extended-keys on` and passes them through). Terminals that don't support
/// it ignore the request; Shift-Enter then reads as Enter.
fn init_terminal() -> DefaultTerminal {
    let t = ratatui::init();
    let _ = write!(std::io::stdout(), "\x1b[>4;1m");
    let _ = std::io::stdout().flush();
    t
}

fn restore_terminal() {
    let _ = write!(std::io::stdout(), "\x1b[>4;0m");
    let _ = std::io::stdout().flush();
    ratatui::restore();
}

/// A daemon push: a coarse "something changed" tick, or a UI command the
/// overlord issued.
enum Push {
    Changed,
    Ui(Value),
}

/// Background thread: hold an `events.subscribe` connection (as a TUI, so
/// the daemon knows UI commands have somewhere to land) and forward pushes.
/// Reconnects with backoff if the daemon restarts.
fn spawn_subscriber() -> mpsc::Receiver<Push> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        if let Ok(mut stream) = UnixStream::connect(paths::socket_path()) {
            let req = json!({"id": 1, "method": "events.subscribe", "params": {"client": "tui"}});
            if writeln!(stream, "{req}").is_ok() {
                let reader = BufReader::new(stream);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    let push = match v["notify"].as_str() {
                        Some("ui") => Push::Ui(v["cmd"].clone()),
                        _ => Push::Changed,
                    };
                    if tx.send(push).is_err() {
                        return; // app gone
                    }
                }
            }
        }
        if tx.send(Push::Changed).is_err() {
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
    Describe(String),
    /// Editing the selected session's tags (space-separated).
    Tags(String),
    /// Renaming a group (tag) on every session: (from, buffer).
    RenameGroup(String),
    NewSession(Box<NewForm>),
    OverlordSetup(Box<overlord::Setup>),
    OverlordUpgrade(Box<overlord::Upgrade>),
    RemoteOffer(Box<overlord::RemoteOffer>),
    /// "Quit rlord?" — quitting is deliberate (y/enter), anything else stays.
    ConfirmQuit,
}

struct App {
    rx: mpsc::Receiver<Push>,
    config: Config,
    keys: KeyMap,
    /// What the table shows: everything (sessions tab) or the selected
    /// group's members (groups tab).
    sessions: Vec<Value>,
    /// Every session the filter matches (both tabs' source).
    all: Vec<Value>,
    tab: Tab,
    /// Groups tab: (tag, count) column, its selection, and the selected tag
    /// ("" = untagged), kept by name across refreshes.
    groups: Vec<(String, usize)>,
    /// Favourite groups, in the user's order (daemon `tag.order`).
    group_order: Vec<String>,
    group_state: ListState,
    group_sel: Option<String>,
    /// Groups tab: keys go to the group column (else to the sessions).
    group_focus: bool,
    table: TableState,
    show_archived: bool,
    sort: SortMode,
    reverse: bool,
    filter: String,
    mode: Mode,
    preview: Preview,
    preview_text: String,
    preview_rich: Option<Text<'static>>,
    /// Sub-agents of the selected session (refreshed with the preview).
    agents: Vec<Value>,
    status_line: String,
    dirty: bool,
    last_preview: Instant,
    /// The highlight moved: refresh the preview once keys pause (moving
    /// itself must never wait on tmux or the daemon).
    preview_due: Option<Instant>,
    /// Which session the preview and sub-agents were loaded for: while the
    /// highlight moves they belong to another row and aren't shown.
    preview_for: Option<String>,
    /// The appearance watcher thread is running.
    watching_appearance: bool,
    /// The system's appearance as last applied (theme families follow it).
    dark: bool,
    /// Latest appearance from the watcher thread (asking macOS spawns a
    /// process, which the draw loop must never wait on).
    system_dark: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether row 0 is the overlord (shown first when not filtering).
    /// Fixed overlord rows at the top of the list (overlord, mastermind):
    /// unnumbered, so digits count the fleet below them.
    overlord_rows: usize,
    /// Session to select once the next refresh lands (set by the overlord).
    pending_select: Option<String>,
    /// Last view reported to the daemon (so the overlord sees what we show).
    reported_view: Value,
}

impl App {
    fn new(rx: mpsc::Receiver<Push>, config: Config, keys: KeyMap) -> Self {
        App {
            rx,
            config,
            keys,
            sessions: Vec::new(),
            all: Vec::new(),
            tab: Tab::Sessions,
            groups: Vec::new(),
            group_order: Vec::new(),
            group_state: ListState::default(),
            group_sel: None,
            group_focus: false,
            table: TableState::default().with_selected(0),
            show_archived: false,
            sort: SortMode::Activity,
            reverse: false,
            filter: String::new(),
            mode: Mode::Normal,
            preview: Preview::Pane,
            preview_text: String::new(),
            preview_rich: None,
            agents: Vec::new(),
            status_line: String::new(),
            dirty: true,
            last_preview: Instant::now() - Duration::from_secs(10),
            preview_due: None,
            preview_for: None,
            watching_appearance: false,
            dark: true,
            system_dark: Default::default(),
            overlord_rows: 0,
            pending_select: None,
            reported_view: Value::Null,
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

    fn query(&self) -> Value {
        let mut q = SessionQuery {
            sort: self.sort,
            reverse: self.reverse,
            ..Default::default()
        };
        q.apply_search_text(&self.filter);
        json!({
            "archived": if self.show_archived { "only" } else { "live" },
            "sort": q.sort.label(),
            "reverse": q.reverse,
            "status": q.status,
            "tag": q.tag,
            "agent": q.agent,
            "project": q.project,
            "group": q.group,
            "search": q.search,
        })
    }

    fn refresh(&mut self) {
        let q = self.query();
        if let Some(v) = self.call("session.list", q) {
            self.all = v.as_array().cloned().unwrap_or_default();
            self.sessions = self.all.clone();
            // The overlord heads the live list — but not while searching,
            // where it would only steal the top match — and isn't in groups.
            self.overlord_rows = 0;
            if self.tab == Tab::Groups {
                if let Some(o) = self.call("tag.order", json!({})) {
                    self.group_order = o["order"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                }
                self.regroup();
            } else if self.filter.trim().is_empty() && !self.show_archived {
                // The overlord, and under it the mastermind once it exists.
                let seats = self
                    .call("overlord.get", json!({"seat": "all"}))
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default();
                for o in seats {
                    self.sessions.insert(self.overlord_rows, o);
                    self.overlord_rows += 1;
                }
            }
            let n = self.sessions.len();
            let sel = self.table.selected().unwrap_or(0);
            self.table
                .select(if n == 0 { None } else { Some(sel.min(n - 1)) });
            if let Some(id) = self.pending_select.take() {
                if let Some(i) = self.sessions.iter().position(|s| s["id"] == id.as_str()) {
                    self.table.select(Some(i));
                }
            }
        }
        self.report_view();
        self.dirty = false;
        self.refresh_preview();
    }

    /// Tell the daemon what the list shows (and whether the overlord is up),
    /// when that changed.
    fn report_view(&mut self) {
        let view = self.current_view();
        if view != self.reported_view {
            self.call("ui.view.report", json!({"view": view}));
            self.reported_view = view;
        }
    }

    fn quit_hint(&mut self) {
        self.status_line = format!(
            "to quit rlord press {} (Shift-Q)",
            self.keys.label(Action::Quit)
        );
    }

    fn draw_confirm_quit(&self, f: &mut Frame) {
        let area = newform::centered(f.area(), 46, 7);
        f.render_widget(ratatui::widgets::Clear, area);
        let lines = vec![
            Line::from(""),
            Line::from("  Quit rlord? Your sessions keep running.").bold(),
            Line::from(""),
            keys_line(&[
                ("y/enter", "quit", Cap::Context),
                ("any key", "stay", Cap::Global),
            ]),
        ];
        f.render_widget(Paragraph::new(lines).block(theme::modal("Quit")), area);
    }

    fn selected_is_overlord(&self) -> bool {
        self.selected().is_some_and(is_overlord_role)
    }

    fn selected(&self) -> Option<&Value> {
        self.table.selected().and_then(|i| self.sessions.get(i))
    }

    fn selected_id(&self) -> Option<String> {
        self.selected()
            .and_then(|s| s["id"].as_str())
            .map(str::to_string)
    }

    /// The highlight moved: redraw now, load its preview and details when
    /// the keys pause.
    pub(crate) fn preview_soon(&mut self) {
        self.preview_due = Some(Instant::now() + PREVIEW_SETTLE);
    }

    fn refresh_preview(&mut self) {
        self.last_preview = Instant::now();
        self.preview_due = None;
        self.preview_for = self.selected_id();
        let Some(s) = self.selected().cloned() else {
            self.preview_text.clear();
            self.agents.clear();
            return;
        };
        self.agents = client_call(
            &paths::socket_path(),
            "session.subagents",
            json!({"ref": s["id"].as_str().unwrap_or("")}),
        )
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
        self.preview_rich = None;
        self.preview_text = match self.preview {
            Preview::Pane => {
                let running = s["status"] != "stopped";
                match s["tmux_session"].as_str() {
                    Some(name) if running => match tmux::capture_pane_ansi(name) {
                        Ok(raw) => {
                            // A detached window keeps its last client's size
                            // (often huge), so the pane's tail is blank rows:
                            // drop those and show the last rows with content.
                            let raw = raw.trim_end().to_string();
                            // Also collapse runs of blank rows: TUIs pin a
                            // footer to the bottom of a tall pane, leaving a
                            // void between content and footer.
                            self.preview_rich = raw.as_bytes().into_text().ok().map(|t| {
                                let blank = |l: &Line| {
                                    l.spans.iter().all(|sp| sp.content.trim().is_empty())
                                };
                                let mut out: Vec<Line> = Vec::with_capacity(t.lines.len());
                                for l in t.lines {
                                    if blank(&l) && out.last().is_some_and(blank) {
                                        continue;
                                    }
                                    out.push(l);
                                }
                                while out.last().is_some_and(blank) {
                                    out.pop();
                                }
                                Text::from(out)
                            });
                            raw
                        }
                        Err(e) => format!("(no pane: {e})"),
                    },
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
        self.startup();
        self.apply_theme();
        loop {
            self.follow_appearance();
            if self.dirty {
                self.refresh();
            } else if self.preview_due.is_some_and(|t| Instant::now() >= t)
                || (self.preview_due.is_none()
                    && self.last_preview.elapsed()
                        > Duration::from_millis(self.config.preview_refresh_ms))
            {
                self.refresh_preview();
            }
            terminal.draw(|f| self.draw(f))?;

            let mut tick = Duration::from_millis(self.config.preview_refresh_ms.clamp(50, 250));
            if let Some(due) = self.preview_due {
                tick = tick.min(due.saturating_duration_since(Instant::now()));
            }
            if event::poll(tick)? {
                if let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    // Quitting rlord takes the real quit key; the reflexive
                    // ones (Ctrl-C, and q in the list) just say what it is.
                    if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                        self.quit_hint();
                        continue;
                    }
                    match std::mem::replace(&mut self.mode, Mode::Normal) {
                        Mode::Normal => {
                            if self.handle_normal(k, terminal)? {
                                return Ok(());
                            }
                        }
                        Mode::Filter(buf) => {
                            self.handle_input(k, buf, InputKind::Filter, terminal)?
                        }
                        Mode::Rename(buf) => {
                            self.handle_input(k, buf, InputKind::Rename, terminal)?
                        }
                        Mode::Describe(buf) => {
                            self.handle_input(k, buf, InputKind::Describe, terminal)?
                        }
                        Mode::Tags(buf) => self.handle_input(k, buf, InputKind::Tags, terminal)?,
                        Mode::RenameGroup(buf) => {
                            self.handle_input(k, buf, InputKind::RenameGroup, terminal)?
                        }
                        Mode::NewSession(form) => {
                            if let Some(form) = self.handle_form(k, form, terminal)? {
                                self.mode = Mode::NewSession(form);
                            }
                        }
                        Mode::OverlordSetup(s) => self.handle_setup(k, s),
                        Mode::OverlordUpgrade(u) => self.handle_upgrade(k, u),
                        Mode::RemoteOffer(o) => self.handle_remote_offer(k, o),
                        Mode::ConfirmQuit => {
                            if matches!(
                                k.code,
                                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y')
                            ) {
                                return Ok(());
                            }
                            self.status_line.clear();
                        }
                    }
                }
            }
            // Daemon pushes: coalesce everything queued into one refresh.
            let mut pushed = false;
            while let Ok(p) = self.rx.try_recv() {
                pushed = true;
                if let Push::Ui(cmd) = p {
                    self.handle_ui_cmd(cmd);
                }
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
        if self.config.jump_digits {
            if let KeyCode::Char(c @ '1'..='9') = k.code {
                if k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT {
                    // Digits count the fleet; the overlord row is unnumbered.
                    let i = c as usize - '1' as usize + self.overlord_rows;
                    if i < n {
                        self.table.select(Some(i));
                        self.attach(terminal)?;
                    }
                    return Ok(false);
                }
            }
        }
        let action = self.keys.action(&k);
        if self.handle_groups_key(&k, action) {
            return Ok(false);
        }
        let Some(action) = action else {
            if k.code == KeyCode::Char('q') && k.modifiers.is_empty() {
                self.quit_hint();
            }
            return Ok(false);
        };
        match action {
            Action::Quit => self.mode = Mode::ConfirmQuit,
            Action::Down => {
                if n > 0 {
                    let i = self.table.selected().unwrap_or(0);
                    self.table.select(Some((i + 1).min(n - 1)));
                    self.preview_soon();
                }
            }
            Action::Up => {
                if n > 0 {
                    let i = self.table.selected().unwrap_or(0);
                    self.table.select(Some(i.saturating_sub(1)));
                    self.preview_soon();
                }
            }
            Action::Top => {
                if n > 0 {
                    self.table.select(Some(0));
                    self.preview_soon();
                }
            }
            Action::Bottom => {
                if n > 0 {
                    self.table.select(Some(n - 1));
                    self.preview_soon();
                }
            }
            Action::Attach => self.attach(terminal)?,
            Action::Restart => self.action("session.restart", "restarted"),
            Action::Stop => self.action("session.stop", "stopped"),
            // One rule for the overlord's row, mirrored by `context_legend`
            // (which never offers these keys for it) and enforced again by
            // the daemon.
            Action::Archive
            | Action::Rename
            | Action::Pin
            | Action::Describe
            | Action::Notes
            | Action::Tags
                if self.selected_is_overlord() =>
            {
                self.status_line =
                    "the overlord's row is fixed: it can only be attached, restarted or stopped"
                        .into();
            }
            Action::Archive => self.action("session.archive", "archived"),
            Action::Overlord => self.show_overlord(),
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
            Action::ReverseSort => {
                self.reverse = !self.reverse;
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
            Action::Describe => {
                if let Some(s) = self.selected() {
                    let cur = s["description"].as_str().unwrap_or("").to_string();
                    self.mode = Mode::Describe(cur);
                }
            }
            Action::Notes => self.edit_notes(terminal)?,
            Action::SessionsTab => self.tab_step(-1),
            Action::GroupsTab => self.tab_step(1),
            Action::SwitchTab => self.switch_tab(match self.tab {
                Tab::Sessions => Tab::Groups,
                Tab::Groups => Tab::Sessions,
            }),
            Action::Tags => {
                if let Some(s) = self.selected() {
                    let cur = s["tags"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    self.mode = Mode::Tags(cur);
                }
            }
            Action::New => self.open_new_form(newform::FormKind::Overlord),
            Action::NewManual => self.open_new_form(newform::FormKind::Manual),
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

    fn handle_input(
        &mut self,
        k: ratatui::crossterm::event::KeyEvent,
        mut buf: String,
        kind: InputKind,
        terminal: &mut DefaultTerminal,
    ) -> Result<()> {
        let code = k.code;
        match code {
            KeyCode::Esc => {
                if kind == InputKind::Filter {
                    self.filter.clear();
                    self.dirty = true;
                }
            }
            // Filter mode: Enter is the jump; Tab keeps the filter.
            KeyCode::Enter if kind == InputKind::Filter => {
                self.filter = buf;
                self.refresh();
                self.attach(terminal)?;
            }
            KeyCode::Tab if kind == InputKind::Filter => {
                self.filter = buf;
                self.dirty = true;
            }
            KeyCode::Up | KeyCode::Down if kind == InputKind::Filter => {
                let n = self.sessions.len();
                if n > 0 {
                    let i = self.table.selected().unwrap_or(0);
                    let j = if code == KeyCode::Down {
                        (i + 1).min(n - 1)
                    } else {
                        i.saturating_sub(1)
                    };
                    self.table.select(Some(j));
                    self.preview_soon();
                }
                self.mode = kind.mode(buf);
            }
            KeyCode::Enter => match kind {
                InputKind::Filter => unreachable!(),
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
                InputKind::Tags => self.save_tags(&buf),
                InputKind::RenameGroup => {
                    let to = buf.trim().trim_start_matches('#').to_string();
                    if let Some(from) = self.group_sel.clone().filter(|g| *g != to) {
                        if self
                            .call("tag.rename", json!({"from": from, "to": to}))
                            .is_some()
                        {
                            self.status_line = format!("group #{from} renamed to #{to}");
                            self.group_sel = Some(to);
                        }
                        self.dirty = true;
                    }
                }
                InputKind::Describe => {
                    if let Some(id) = self.selected_id() {
                        if self
                            .call(
                                "session.describe",
                                json!({"ref": id, "description": buf.trim()}),
                            )
                            .is_some()
                        {
                            self.status_line = "description saved".into();
                        }
                        self.dirty = true;
                    }
                }
            },
            KeyCode::Backspace => {
                buf.pop();
                if kind == InputKind::Filter {
                    self.filter = buf.clone();
                    self.dirty = true;
                }
                self.mode = kind.mode(buf);
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                buf.push(c);
                if kind == InputKind::Filter {
                    // Live filtering as you type; selection snaps to the top match.
                    self.filter = buf.clone();
                    self.table.select(Some(0));
                    self.dirty = true;
                }
                self.mode = kind.mode(buf);
            }
            _ => self.mode = kind.mode(buf),
        }
        Ok(())
    }

    /// Edit the selected session's notes in $EDITOR (vi fallback), then save.
    /// Replace the selected session's tags with `buf` (space/comma separated).
    fn save_tags(&mut self, buf: &str) {
        let Some(s) = self.selected().cloned() else {
            return;
        };
        let new: Vec<String> = buf
            .split([' ', ','])
            .map(|t| t.trim().trim_start_matches('#').to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        let old: Vec<String> = s["tags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let add: Vec<&String> = new.iter().filter(|t| !old.contains(t)).collect();
        let remove: Vec<&String> = old.iter().filter(|t| !new.contains(t)).collect();
        if add.is_empty() && remove.is_empty() {
            return;
        }
        let id = s["id"].as_str().unwrap_or("");
        if self
            .call(
                "session.tag",
                json!({"ref": id, "add": add, "remove": remove}),
            )
            .is_some()
        {
            // On the groups tab, follow the session if it left this group.
            if self.tab == Tab::Groups {
                let here = self.group_sel.clone().unwrap_or_default();
                let stays = if here == groups::UNTAGGED {
                    new.is_empty()
                } else {
                    new.contains(&here)
                };
                if !stays {
                    self.group_sel = Some(
                        new.first()
                            .cloned()
                            .unwrap_or_else(|| groups::UNTAGGED.into()),
                    );
                    self.pending_select = Some(id.to_string());
                }
            }
            self.status_line = if new.is_empty() {
                format!("{}: untagged", s["name"].as_str().unwrap_or("?"))
            } else {
                format!(
                    "{}: {}",
                    s["name"].as_str().unwrap_or("?"),
                    new.iter()
                        .map(|t| format!("#{t}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            };
        }
        self.dirty = true;
    }

    fn edit_notes(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(s) = self.selected().cloned() else {
            return Ok(());
        };
        let id = s["id"].as_str().unwrap_or("").to_string();
        let current = s["notes"].as_str().unwrap_or("").to_string();
        match edit_in_editor(terminal, &format!("notes-{id}.md"), &current) {
            Ok(new) => {
                if new.trim_end() != current.trim_end() {
                    self.call("session.notes", json!({"ref": id, "notes": new}));
                    self.status_line = "notes saved".into();
                } else {
                    self.status_line = "notes unchanged".into();
                }
            }
            Err(why) => self.status_line = format!("{why}; notes not saved"),
        }
        self.dirty = true;
        Ok(())
    }

    /// Leave the TUI, hand the terminal to tmux, come back when it detaches.
    fn attach(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(mut s) = self.selected().cloned() else {
            return Ok(());
        };
        if is_overlord_role(&s) {
            // Like any session, full screen — started/resumed if needed, and
            // its window handed back from whatever size an overlay set.
            let seat = rlord_daemon::overlord::seat(s["role"].as_str().unwrap_or(""));
            match self.call("overlord.ensure", json!({"seat": seat.key})) {
                Some(v) => s = v,
                None => return Ok(()),
            }
            let _ = tmux::auto_size_window(seat.tmux);
        } else if s["status"] == "stopped"
            || !s["tmux_session"].as_str().is_some_and(tmux::has_session)
        {
            // Enter on a stopped (or vanished) session resumes it, then attaches.
            let id = s["id"].as_str().unwrap_or("").to_string();
            match self.call("session.restart", json!({"ref": id})) {
                Some(v) => {
                    self.status_line = format!("resumed {}", v["name"].as_str().unwrap_or(""));
                    s = v;
                    self.dirty = true;
                }
                None => return Ok(()),
            }
        }
        let Some(name) = s["tmux_session"].as_str() else {
            return Ok(());
        };
        let target = format!("={name}");
        if tmux::inside_rlord() {
            // Inside rlord's tmux: switch this client; the TUI keeps running
            // in its own pane, and `nav home` (the back key) returns here.
            if let Some(home) = tmux::current_session() {
                let _ = tmux::set_home(&home);
            }
            if let Err(e) = tmux::switch_client(name) {
                self.status_line = format!("{e:#}");
            }
            return Ok(());
        }
        // Outside tmux there is no home session: the back key must detach.
        let _ = tmux::set_home("");
        restore_terminal();
        let status = tmux::command()
            .args(["attach-session", "-t", &target])
            .status();
        *terminal = init_terminal();
        terminal.clear()?;
        if let Err(e) = status {
            self.status_line = format!("attach failed: {e}");
        }
        self.dirty = true;
        Ok(())
    }

    /// Pick the palette for config `theme` and the system's appearance.
    fn apply_theme(&mut self) {
        use std::sync::atomic::Ordering;
        self.dark = rlord_core::themes::system_is_dark();
        self.system_dark.store(self.dark, Ordering::Relaxed);
        theme::set(rlord_core::themes::resolve(&self.config.theme, self.dark));
        // Theme families follow the system: a thread re-checks every few
        // seconds; the loop just reads what it found.
        if rlord_core::themes::follows_system(&self.config.theme) && !self.watching_appearance {
            self.watching_appearance = true;
            let flag = self.system_dark.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(3));
                flag.store(rlord_core::themes::system_is_dark(), Ordering::Relaxed);
            });
        }
    }

    /// Switch palettes when the system's appearance flipped (and recolor the
    /// overlord's bar).
    fn follow_appearance(&mut self) {
        let now = self.system_dark.load(std::sync::atomic::Ordering::Relaxed);
        if now != self.dark {
            self.dark = now;
            theme::set(rlord_core::themes::resolve(&self.config.theme, now));
            let _ = self.call("overlord.style", Value::Null);
        }
    }

    /// `H` / `L`: one step left / right along sessions tab ⇄ group list ⇄
    /// inside the highlighted group.
    pub(crate) fn tab_step(&mut self, dir: i8) {
        match (self.tab, self.group_focus, dir) {
            (Tab::Sessions, _, 1) => self.switch_tab(Tab::Groups),
            (Tab::Groups, true, 1) => self.group_focus = self.sessions.is_empty(),
            (Tab::Groups, true, -1) => self.switch_tab(Tab::Sessions),
            (Tab::Groups, false, -1) => self.group_focus = true,
            _ => {}
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        // Themes with their own background paint it under everything.
        if theme::bg().is_some() {
            f.render_widget(Block::default().style(theme::base()), f.area());
        }
        let [bars_area, main_area, footer_area] = RLayout::vertical([
            // The header: tabs, then the view, beside the 4-row logo.
            Constraint::Length(4),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .areas(f.area());
        self.draw_bars(f, bars_area);
        let (list_area, right) = match self.config.layout {
            Layout::Side => {
                // The list is what this view is for: it gets its width first
                // (configured share, but never below what its columns need);
                // the preview takes the remainder and simply drops out when
                // that remainder is too small to be useful.
                let pct = self.config.list_width_pct.clamp(20, 80) as u32;
                let want = (main_area.width as u32 * pct / 100) as u16;
                let list_w = want.max(60).min(main_area.width);
                let rest = main_area.width - list_w;
                if rest >= 32 {
                    let [l, r] =
                        RLayout::horizontal([Constraint::Length(list_w), Constraint::Min(1)])
                            .areas(main_area);
                    (l, Some(r))
                } else {
                    (main_area, None)
                }
            }
            Layout::Stack => {
                if main_area.height >= 16 {
                    let [l, r] =
                        RLayout::vertical([Constraint::Percentage(50), Constraint::Min(6)])
                            .areas(main_area);
                    (l, Some(r))
                } else {
                    (main_area, None)
                }
            }
        };
        match self.tab {
            Tab::Sessions => self.draw_list(f, list_area),
            Tab::Groups => self.draw_groups(f, list_area),
        }
        if let Some(right) = right {
            let details_h = self.config.details_height.clamp(4, 20);
            if right.height >= details_h + 6 {
                let [p, d] = RLayout::vertical([Constraint::Min(4), Constraint::Length(details_h)])
                    .areas(right);
                self.draw_preview(f, p);
                self.draw_details(f, d);
            } else {
                self.draw_preview(f, right);
            }
        }
        self.draw_footer(f, footer_area);
        match &self.mode {
            Mode::NewSession(form) => self.draw_form(f, form),
            Mode::OverlordSetup(s) => self.draw_setup(f, s),
            Mode::OverlordUpgrade(u) => self.draw_upgrade(f, u),
            Mode::RemoteOffer(o) => self.draw_remote_offer(f, o),
            Mode::ConfirmQuit => self.draw_confirm_quit(f),
            _ => {}
        }
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        // Drop the wide, least-essential columns first as the list narrows.
        let width = area.width.saturating_sub(6) as usize; // borders, padding, marker
        let show_project = width >= 96;
        let show_tags = width >= 62;
        let name_w = if width >= 60 { 26 } else { 18 };

        let mut header = vec!["", "", "Name", "Status", "Updated"];
        let mut widths = vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(name_w),
            Constraint::Length(13),
            Constraint::Length(8),
        ];
        if show_tags {
            header.push("Groups");
            widths.push(Constraint::Length(18));
        }
        if show_project {
            header.push("Project");
            widths.push(Constraint::Min(10));
        }
        let header = Row::new(header).style(theme::muted()).bottom_margin(1);
        let home = std::env::var("HOME").unwrap_or_default();
        let rows: Vec<Row> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let get = |k: &str| s[k].as_str().unwrap_or("-").to_string();
                let status = get("status");
                let is_overlord = is_overlord_role(s);
                let is_mastermind = s["role"] == "mastermind";
                let num = i.saturating_sub(self.overlord_rows);
                let mut cells = vec![
                    if is_overlord {
                        Cell::from("◆").fg(theme::overlord())
                    } else {
                        Cell::from(if num < 9 {
                            ((b'1' + num as u8) as char).to_string()
                        } else {
                            String::new()
                        })
                        .fg(theme::faint_c())
                    },
                    if s["pinned"].as_bool().unwrap_or(false) && !is_overlord {
                        Cell::from("★").fg(theme::yellow())
                    } else {
                        Cell::from(" ")
                    },
                    Cell::from(if is_mastermind {
                        format!("↳ {}", get("name"))
                    } else {
                        get("name")
                    }),
                    Cell::from(Line::from(vec![
                        // Not loaded yet (the daemon's warm-up): say so
                        // rather than show half-known data.
                        theme::status_chip(&status),
                        // Its sub-agents are still loading (warm-up): the
                        // status is known, the agent counts aren't yet.
                        Span::styled(if s["loading"] == true { " ◌" } else { "" }, theme::faint()),
                        // Remote Control: connected, connecting, or wanted
                        // but not to be had.
                        remote_chip(&s["remote"]),
                        Span::styled(
                            match s["agents_running"].as_i64().unwrap_or(0) {
                                0 => String::new(),
                                n => format!(" ⚙{n}"),
                            },
                            Style::default().fg(theme::overlord()),
                        ),
                    ])),
                    Cell::from(Span::styled(rel_age(updated_at(s)), theme::muted())),
                ];
                if show_tags {
                    let tags = s["tags"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                        .unwrap_or_default();
                    cells.push(Cell::from(Line::from(theme::tag_spans(tags))));
                }
                if show_project {
                    cells.push(if is_overlord {
                        Cell::from(format!(
                            "{} · {} to talk",
                            s["model"].as_str().unwrap_or("default model"),
                            // Only a key that works from the list: the
                            // mastermind's global key if bound, else enter.
                            if !is_mastermind {
                                self.keys.label(Action::Overlord)
                            } else if self.config.tmux.mastermind.trim().is_empty() {
                                self.keys.label(Action::Attach)
                            } else {
                                tmux::key_label(&self.config.tmux.mastermind)
                            }
                        ))
                        .fg(theme::faint_c())
                    } else {
                        Cell::from(get("project_path").replacen(&home, "~", 1)).fg(theme::faint_c())
                    });
                }
                let row = Row::new(cells);
                if is_overlord {
                    row.style(Style::default().fg(theme::overlord()).bold())
                } else {
                    row.style(Style::default().fg(theme::text()))
                }
            })
            .collect();
        // The list is the focused pane unless the group column has focus.
        let focused = !(self.tab == Tab::Groups && self.group_focus);
        let block = theme::panel(self.list_title(), focused);
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .column_spacing(2)
            .row_highlight_style(theme::selected())
            .highlight_symbol(Span::styled(
                theme::SELECT_BAR,
                Style::default().fg(theme::accent()),
            ))
            .highlight_spacing(ratatui::widgets::HighlightSpacing::Always);
        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_preview(&self, f: &mut Frame, area: Rect) {
        let name = self
            .selected()
            .and_then(|s| s["name"].as_str())
            .unwrap_or("");
        let log = self.keys.label(Action::ToggleLog);
        let (what, other) = match self.preview {
            Preview::Pane => ("live pane", "log"),
            Preview::Log => ("event log", "pane"),
        };
        let title = Line::from(vec![
            Span::raw(" "),
            Span::styled(name.to_string(), Style::default().fg(theme::text()).bold()),
            Span::styled(format!(" · {what} "), theme::muted()),
        ]);
        let switch =
            Line::from(Span::styled(format!(" {log} {other} "), theme::faint())).right_aligned();
        // Show the tail that fits: the bottom of a pane is what matters.
        let inner_h = area.height.saturating_sub(2) as usize;
        // Loaded for another row (the highlight is moving): show nothing
        // rather than the wrong session's screen.
        let current = self.preview_for == self.selected_id();
        let text: Text = match &self.preview_rich {
            _ if !current => Text::from(Span::styled("…", theme::faint())),
            Some(rich) if self.preview == Preview::Pane => {
                let start = rich.lines.len().saturating_sub(inner_h);
                Text::from(rich.lines[start..].to_vec())
            }
            _ => {
                let lines: Vec<&str> = self.preview_text.lines().collect();
                let start = lines.len().saturating_sub(inner_h);
                Text::from(
                    lines[start..]
                        .iter()
                        .map(|l| Line::raw(*l))
                        .collect::<Vec<_>>(),
                )
            }
        };
        let block = theme::panel(title, false).title(switch);
        f.render_widget(Paragraph::new(text).block(block), area);
    }

    /// The control-layer card: everything we know about the session that
    /// isn't the live pane, plus the human-authored description and notes.
    fn draw_details(&self, f: &mut Frame, area: Rect) {
        let block = theme::panel(theme::title("Details"), false);
        let Some(s) = self.selected() else {
            f.render_widget(block, area);
            return;
        };
        let home = std::env::var("HOME").unwrap_or_default();
        let tilde = |p: &str| p.replacen(&home, "~", 1);
        let get = |k: &str| s[k].as_str().unwrap_or("-").to_string();
        let key = |k: &str| Span::styled(format!("{k:<10}"), theme::faint());
        let status = get("status");
        let tags: Vec<&str> = s["tags"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let sep = || Span::styled("  ·  ", theme::faint());
        let desc = s["description"].as_str().unwrap_or("");
        let mut lines = vec![
            // Headline: name, status, then the description (what it's doing).
            Line::from(vec![
                Span::styled(get("name"), Style::default().fg(theme::text()).bold()),
                Span::raw("   "),
                theme::status_chip(&status),
                Span::styled(
                    match updated_at(s) {
                        Some(t) => format!("  ·  updated {} ago", rel_age(Some(t))),
                        None => String::new(),
                    },
                    theme::muted(),
                ),
            ]),
            if desc.is_empty() {
                Line::from(Span::styled(
                    format!(
                        "no description — {} to add",
                        self.keys.label(Action::Describe)
                    ),
                    theme::faint().italic(),
                ))
            } else {
                Line::from(Span::styled(
                    desc.to_string(),
                    Style::default().fg(theme::yellow()),
                ))
            },
            Line::raw(""),
            Line::from({
                let mut v = vec![key("groups")];
                if tags.is_empty() {
                    v.push(Span::styled("none", theme::faint()));
                } else {
                    v.extend(theme::tag_spans(tags));
                }
                v
            }),
            Line::from(vec![
                key("repo"),
                Span::styled(
                    s["repo_path"]
                        .as_str()
                        .map(tilde)
                        .unwrap_or_else(|| "none".into()),
                    Style::default().fg(theme::text()),
                ),
                Span::styled(
                    s["branch"]
                        .as_str()
                        .map(|b| format!("  ⎇ {b}"))
                        .unwrap_or_default(),
                    Style::default().fg(theme::cyan()),
                ),
            ]),
            Line::from(vec![
                key("folder"),
                Span::styled(tilde(&get("project_path")), theme::muted()),
            ]),
            Line::from(vec![
                key("agent"),
                Span::styled(get("agent"), Style::default().fg(theme::text())),
                sep(),
                Span::styled(
                    s["model"].as_str().unwrap_or("default model").to_string(),
                    Style::default().fg(theme::text()),
                ),
                sep(),
                Span::styled(format!("id {}", get("id")), theme::faint()),
            ]),
            Line::from(vec![
                key("activity"),
                Span::styled(
                    format!(
                        "you {} · agent {} · status {} · created {}",
                        rel_age(s["last_user_msg_at"].as_i64()),
                        rel_age(s["last_agent_msg_at"].as_i64()),
                        rel_age(s["last_state_change_at"].as_i64()),
                        rel_age(s["created_at"].as_i64()),
                    ),
                    theme::muted(),
                ),
            ]),
        ];
        if let Some(st) = s["remote"]["state"].as_str() {
            let (text, style) = match st {
                "on" => (
                    format!(
                        "on · {}",
                        s["remote"]["url"].as_str().unwrap_or("in the Claude app")
                    ),
                    Style::default().fg(theme::green()),
                ),
                "connecting" => ("connecting…".into(), theme::muted()),
                "pending" => (
                    format!(
                        "on · {}",
                        s["remote"]["reason"].as_str().unwrap_or("needs a restart")
                    ),
                    theme::muted(),
                ),
                "disconnecting" => ("off · disconnects at its restart".into(), theme::muted()),
                "stopped" => ("on · connects when it starts".into(), theme::muted()),
                other => (
                    format!(
                        "{other}: {}",
                        s["remote"]["reason"].as_str().unwrap_or("unknown")
                    ),
                    Style::default().fg(theme::red()),
                ),
            };
            lines.push(Line::from(vec![key("remote"), Span::styled(text, style)]));
        }
        if s["loading"] == true {
            lines.push(Line::from(vec![
                key("agents"),
                Span::styled("◌ loading sub-agents…", theme::faint()),
            ]));
        } else if !self.agents.is_empty() && self.preview_for == self.selected_id() {
            let running = self
                .agents
                .iter()
                .filter(|a| a["finished_at"].is_null())
                .count();
            lines.push(Line::from(vec![
                key("agents"),
                Span::styled(
                    format!("{} total · {} running", self.agents.len(), running),
                    Style::default().fg(theme::overlord()),
                ),
            ]));
            // Most recent first; the card has limited rows.
            for a in self.agents.iter().rev().take(4) {
                let started = a["started_at"].as_i64();
                let finished = a["finished_at"].as_i64();
                let (state, style) = match (started, finished) {
                    (Some(st), Some(fi)) => (
                        format!("done {}s", ((fi - st) as f64 / 1000.0).round()),
                        Style::default().fg(theme::faint_c()),
                    ),
                    (Some(st), None) => (
                        format!("running {}", rel_age(Some(st))),
                        Style::default().fg(theme::green()),
                    ),
                    _ => ("?".into(), Style::default()),
                };
                let result = a["result"]
                    .as_str()
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("");
                lines.push(Line::from(vec![
                    Span::styled("        ⚙ ", Style::default().fg(theme::overlord())),
                    Span::styled(
                        a["agent_type"].as_str().unwrap_or("?").to_string(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" {} · ", a["description"].as_str().unwrap_or(""))),
                    Span::styled(state, style),
                    Span::styled(
                        if result.is_empty() {
                            String::new()
                        } else {
                            format!(" ↳ {result}")
                        },
                        Style::default().fg(theme::faint_c()),
                    ),
                ]));
            }
        }
        let notes = s["notes"].as_str().unwrap_or("");
        if !notes.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled("notes", theme::faint())));
            for l in notes.lines() {
                lines.push(Line::from(Span::styled(l.to_string(), theme::muted())));
            }
        }
        f.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let prompt = match &self.mode {
            Mode::Normal => None,
            Mode::Filter(b) => Some(format!(
                "search: {b}▏   (enter: jump · tab: keep filter · esc: clear · status:/tag:/proj:)"
            )),
            Mode::Rename(b) => Some(format!("rename: {b}▏")),
            Mode::Describe(b) => Some(format!("description: {b}▏   (enter: save · esc: cancel)")),
            Mode::Tags(b) => Some(format!(
                "tags (= groups): {b}▏   (space-separated · enter: save · esc: cancel)"
            )),
            Mode::RenameGroup(b) => Some(format!(
                "rename group (on every session): #{b}▏   (enter: save · esc: cancel)"
            )),
            Mode::NewSession(_) => Some("new session — see form".to_string()),
            Mode::ConfirmQuit => {
                Some("quit rlord?  y / enter: quit · any other key: stay".to_string())
            }
            Mode::OverlordSetup(_) | Mode::OverlordUpgrade(_) | Mode::RemoteOffer(_) => {
                Some("overlord — see dialog".to_string())
            }
        };
        let line2 = if self.status_line.is_empty() {
            Line::raw("")
        } else {
            Line::from(vec![
                Span::styled(" › ", Style::default().fg(theme::accent())),
                Span::styled(self.status_line.clone(), theme::muted()),
            ])
        };
        let [line1_area, line2_area] =
            RLayout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
        f.render_widget(Paragraph::new(line2), line2_area);
        if let Some(p) = prompt {
            // `label: text   (hints)` → a label chip, the text, dim hints.
            let (label, rest) = p.split_once(": ").unwrap_or(("", p.as_str()));
            let (text, hints) = match rest.find("   (") {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, ""),
            };
            let mut spans = Vec::new();
            if !label.is_empty() {
                spans.push(Span::styled(
                    format!(" {label} "),
                    Style::default()
                        .fg(theme::on_accent())
                        .bg(theme::yellow())
                        .bold(),
                ));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                text.to_string(),
                Style::default().fg(theme::text()),
            ));
            spans.push(Span::styled(hints.to_string(), theme::faint()));
            f.render_widget(Paragraph::new(Line::from(spans)), line1_area);
            return;
        }
        // Keys that always work sit on the left; keys for what's highlighted
        // sit on the right after a divider, and only the ones that apply.
        let global = Line::from(
            self.keys
                .global_legend()
                .into_iter()
                .flat_map(|(k, label)| keycap(&k, label, Cap::Global))
                .collect::<Vec<_>>(),
        );
        let mut ctx = self.keys.labelled(&context_legend(
            self.tab == Tab::Groups && self.group_focus,
            self.group_sel.as_deref(),
            self.selected(),
        ));
        // Inside a group, say how to get back out.
        if self.tab == Tab::Groups && !self.group_focus {
            ctx.insert(0, (self.keys.label(Action::Cancel), "groups"));
        }
        // On the group list: favourite it, and reorder favourites.
        if self.tab == Tab::Groups && self.group_focus {
            if let Some(g) = self.group_sel.as_deref().filter(|g| *g != groups::UNTAGGED) {
                let fav = self.is_favourite(g);
                ctx.push((
                    self.keys.label(Action::Pin),
                    if fav { "unfavourite" } else { "favourite" },
                ));
                if fav {
                    ctx.push(("J/K".into(), "reorder"));
                }
            }
        }
        // The divider sits at a fixed column: the right slot is as wide as
        // the longest row-key set, and row keys fill it from the left, so
        // moving the highlight never shifts it.
        // Too narrow for both: the global keys keep their full width (quit,
        // new and the overlord must stay findable) and row keys get the rest.
        let slot = self.keys.slot_width;
        let global_w = u16::try_from(global.width()).unwrap_or(u16::MAX);
        let constraints = if line1_area.width >= global_w.saturating_add(slot) {
            [Constraint::Min(0), Constraint::Length(slot)]
        } else {
            [Constraint::Length(global_w), Constraint::Min(0)]
        };
        let [left, right] = RLayout::horizontal(constraints).areas(line1_area);
        f.render_widget(Paragraph::new(global), left);
        f.render_widget(Paragraph::new(context_line(ctx)), right);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Filter,
    Rename,
    Describe,
    Tags,
    RenameGroup,
}

impl InputKind {
    fn mode(self, buf: String) -> Mode {
        match self {
            InputKind::Filter => Mode::Filter(buf),
            InputKind::Rename => Mode::Rename(buf),
            InputKind::Describe => Mode::Describe(buf),
            InputKind::Tags => Mode::Tags(buf),
            InputKind::RenameGroup => Mode::RenameGroup(buf),
        }
    }
}

/// Keycap colour: neutral for keys that always work, the accent (the
/// active tab's cyan) for keys acting on the highlighted row.
#[derive(Clone, Copy)]
enum Cap {
    Global,
    Context,
}

/// The footer's right slot: divider, then the row keys as accent keycaps.
fn context_line(ctx: Vec<(String, &'static str)>) -> Line<'static> {
    let mut spans = vec![Span::raw("│  ").fg(theme::border())];
    spans.extend(
        ctx.into_iter()
            .flat_map(|(k, label)| keycap(&k, label, Cap::Context)),
    );
    Line::from(spans)
}

/// When a session last changed: its latest message either way or status
/// change (creation, failing all of those). The list's "Updated" column.
fn updated_at(s: &Value) -> Option<i64> {
    [
        "last_agent_msg_at",
        "last_user_msg_at",
        "last_state_change_at",
        "created_at",
    ]
    .iter()
    .filter_map(|k| s[*k].as_i64())
    .max()
}

/// The list's Remote Control mark: ⇄ connected (green), connecting (dim),
/// wanted but failing or unavailable (red); nothing when off.
fn remote_chip(r: &Value) -> Span<'static> {
    match r["state"].as_str() {
        Some("on") => Span::styled(" ⇄", Style::default().fg(theme::green())),
        Some("connecting" | "stopped" | "pending" | "disconnecting") => {
            Span::styled(" ⇄", theme::faint())
        }
        Some("failed") | Some("unavailable") => {
            Span::styled(" ⇄", Style::default().fg(theme::red()))
        }
        _ => Span::raw(""),
    }
}

/// Edit `text` in the user's `$VISUAL` / `$EDITOR` (vi if unset): the TUI
/// hands the terminal over and takes it back after. Ok(new text), or
/// Err(why it wasn't saved) when the editor failed or couldn't start.
fn edit_in_editor(
    terminal: &mut DefaultTerminal,
    file: &str,
    text: &str,
) -> std::result::Result<String, String> {
    // A unique file per edit: several TUIs (clients) may edit at once.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // rlord's own runtime dir (owned by the user), not the shared temp dir.
    let dir = paths::runtime_dir().join("edit");
    let next_path = || {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        dir.join(format!("{}-{n}-{file}", std::process::id()))
    };
    let mut path = next_path();
    // File errors are the user's to see (the caller keeps its text), not a
    // reason to quit the TUI.
    // Created fresh (never through an existing file or symlink).
    // A stale file left by a crashed TUI with the same pid: take the next name.
    let opened = std::fs::create_dir_all(&dir).and_then(|_| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let mut tries = 0;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => {
                    tries += 1;
                    path = next_path();
                }
                r => break r,
            }
        }
    });
    let written = match opened {
        Ok(mut f) => {
            use std::io::Write;
            let w = f.write_all(text.as_bytes());
            if w.is_err() {
                let _ = std::fs::remove_file(&path); // ours, partly written
            }
            w
        }
        Err(e) => Err(e), // not ours: leave whatever is there
    };
    if let Err(e) = written {
        return Err(format!("could not prepare {}: {e}", path.display()));
    }
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    restore_terminal();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("rlord")
        .arg(&path)
        .status();
    *terminal = init_terminal();
    let _ = terminal.clear();
    let r = match status {
        // Unreadable afterwards (deleted, renamed): keep the old text.
        Ok(st) if st.success() => std::fs::read_to_string(&path)
            .map_err(|e| format!("could not read the edited text back: {e}")),
        Ok(_) => Err("editor exited with an error".to_string()),
        Err(e) => Err(format!("could not run editor '{editor}': {e}")),
    };
    let _ = std::fs::remove_file(&path);
    r
}

/// The overlord's rows (overlord, mastermind): fixed, special rows.
fn is_overlord_role(s: &Value) -> bool {
    rlord_daemon::overlord::is_overlord_role(s["role"].as_str().unwrap_or(""))
}

/// How long the highlight must rest before its preview loads.
const PREVIEW_SETTLE: Duration = Duration::from_millis(80);

/// A dialog's row of key hints, indented to match its text.
fn keys_line(items: &[(&str, &str, Cap)]) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    for &(k, label, cap) in items {
        spans.extend(keycap(k, label, cap));
    }
    Line::from(spans)
}

/// A key hint as a padded keycap chip, then its label: ` n  new   `.
fn keycap(k: &str, label: &str, cap: Cap) -> [Span<'static>; 2] {
    let chip = match cap {
        Cap::Global => Style::default().fg(theme::text()).bg(theme::chip()),
        Cap::Context => Style::default().fg(theme::on_accent()).bg(theme::accent()),
    };
    [
        Span::styled(format!(" {k} "), chip.bold()),
        Span::styled(format!(" {label}  "), theme::muted()),
    ]
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
    NewManual,
    Restart,
    Stop,
    Archive,
    Unarchive,
    Rename,
    Pin,
    ToggleArchived,
    Sort,
    ReverseSort,
    Filter,
    ToggleLog,
    Describe,
    Notes,
    Overlord,
    SwitchTab,
    SessionsTab,
    GroupsTab,
    Tags,
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
            // With Ctrl, terminals report Ctrl-Shift-Q as Ctrl-Q (or Ctrl-'Q'):
            // treat Ctrl+letter case-insensitively.
            (KeyCode::Char(a), KeyCode::Char(b)) if self.ctrl => a.eq_ignore_ascii_case(&b),
            // Otherwise shift on letters shows up as the uppercase char; exact.
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
            if self.alt { "⌥" } else { "" }
        )
    }
}

struct KeyMap {
    bindings: Vec<(Action, KeyBinding)>,
    /// Width of the footer's right slot (see `context_slot_width`).
    slot_width: u16,
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
            (Action::NewManual, &k.new_manual),
            (Action::Restart, &k.restart),
            (Action::Stop, &k.stop),
            (Action::Archive, &k.archive),
            (Action::Unarchive, &k.unarchive),
            (Action::Rename, &k.rename),
            (Action::Pin, &k.pin),
            (Action::ToggleArchived, &k.toggle_archived),
            (Action::Sort, &k.sort),
            (Action::ReverseSort, &k.reverse_sort),
            (Action::Filter, &k.filter),
            (Action::ToggleLog, &k.toggle_log),
            (Action::Describe, &k.describe),
            (Action::Notes, &k.notes),
            (Action::Overlord, &k.overlord),
            (Action::SwitchTab, &k.switch_tab),
            (Action::SessionsTab, &k.sessions_tab),
            (Action::GroupsTab, &k.groups_tab),
            (Action::Tags, &k.tags),
            (Action::Cancel, &k.cancel),
        ];
        let mut bindings: Vec<(Action, KeyBinding)> = Vec::with_capacity(specs.len());
        for (a, spec) in specs {
            bindings.push((a, KeyBinding::parse(spec)?));
        }
        // H / L (the tab steps) are newer defaults: a user binding of the
        // same key to anything else wins, and the step key is dropped.
        let taken = |b: &KeyBinding, me: Action, all: &[(Action, KeyBinding)]| {
            all.iter()
                .any(|(a, o)| *a != me && o.code == b.code && o.ctrl == b.ctrl && o.alt == b.alt)
        };
        let snapshot = bindings.clone();
        let default_step = |a: &Action| {
            (*a == Action::SessionsTab && k.sessions_tab == "H")
                || (*a == Action::GroupsTab && k.groups_tab == "L")
        };
        // Only the defaults give way; a step key the user set is theirs.
        bindings.retain(|(a, b)| !default_step(a) || !taken(b, *a, &snapshot));
        // Arrow keys always work for movement, alongside whatever is configured.
        bindings.push((Action::Up, KeyBinding::parse("up")?));
        bindings.push((Action::Down, KeyBinding::parse("down")?));
        let mut km = KeyMap {
            bindings,
            slot_width: 0,
        };
        km.slot_width = km.context_slot_width();
        Ok(km)
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

    /// Keys that work whatever is highlighted.
    fn global_legend(&self) -> Vec<(String, &'static str)> {
        self.labelled(&[
            (Action::Overlord, "overlord"),
            (Action::SwitchTab, "sessions/groups"),
            (Action::New, "new"),
            (Action::NewManual, "new (manual)"),
            (Action::ReverseSort, "reverse"),
            (Action::ToggleLog, "log"),
            (Action::Quit, "quit"),
        ])
    }

    /// Width of the footer's right slot: the widest row-key set any row can
    /// produce. Measured over every row shape (configured keys can make
    /// either of archive/unarchive or pin/unpin the wider one), once.
    fn context_slot_width(&self) -> u16 {
        let mut w = 0;
        for status in ["working", "stopped"] {
            for archived in [Value::Null, json!(1)] {
                for pinned in [false, true] {
                    let s = json!({"status": status, "agent": "claude", "agent_session": "x",
                                   "archived_at": archived, "pinned": pinned, "role": ""});
                    let line = context_line(self.labelled(&context_legend(false, None, Some(&s))));
                    w = w.max(line.width());
                }
            }
        }
        // Inside a group the footer also leads with "esc groups".
        let back: usize = keycap(&self.label(Action::Cancel), "groups", Cap::Context)
            .iter()
            .map(|s| s.width())
            .sum();
        u16::try_from(w + back).unwrap_or(u16::MAX)
    }

    fn labelled(&self, actions: &[(Action, &'static str)]) -> Vec<(String, &'static str)> {
        actions.iter().map(|&(a, l)| (self.label(a), l)).collect()
    }
}

/// Keys that apply to what's highlighted, in footer order: a group (groups
/// tab, group column focused) or a session row. Only what the handler would
/// actually do for that row is listed: archive xor unarchive, stop only
/// while running, nothing the overlord refuses.
fn context_legend(
    group_focus: bool,
    group: Option<&str>,
    session: Option<&Value>,
) -> Vec<(Action, &'static str)> {
    if group_focus {
        let mut v = Vec::new();
        if let Some(g) = group {
            v.push((Action::Attach, "open"));
            if g != groups::UNTAGGED {
                v.push((Action::Rename, "rename group"));
            }
        }
        return v;
    }
    let Some(s) = session else {
        return Vec::new();
    };
    let overlord = is_overlord_role(s);
    let stopped = s["status"] == "stopped" || s["status"].is_null();
    let archived = !s["archived_at"].is_null();
    // Resuming (Enter on a stopped row) and restarting both need a
    // conversation the agent can resume.
    let resumable = s["agent"] == "claude" && !s["agent_session"].is_null();
    let mut v = Vec::new();
    if !stopped {
        v.push((Action::Attach, "attach"));
        if resumable {
            v.push((Action::Restart, "restart"));
        }
        v.push((Action::Stop, "stop"));
    } else if resumable {
        v.push((Action::Attach, "resume"));
    }
    if overlord {
        // Its row is fixed: no name, pin, tags, description, notes, archive.
        return v;
    }
    if archived {
        v.push((Action::Unarchive, "unarchive"));
    } else {
        v.push((Action::Archive, "archive"));
    }
    v.push((Action::Rename, "rename"));
    v.push((
        Action::Pin,
        if s["pinned"].as_bool().unwrap_or(false) {
            "unpin"
        } else {
            "pin"
        },
    ));
    v.push((Action::Tags, "tags"));
    v.push((Action::Describe, "describe"));
    v.push((Action::Notes, "notes"));
    v
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

    fn labels(v: &[(Action, &str)]) -> Vec<String> {
        v.iter().map(|(_, l)| l.to_string()).collect()
    }

    #[test]
    fn context_legend_lists_only_what_applies() {
        let running = json!({"status": "working", "agent": "claude", "agent_session": "x",
                             "archived_at": null, "pinned": false, "role": ""});
        assert_eq!(
            labels(&context_legend(false, None, Some(&running))),
            [
                "attach", "restart", "stop", "archive", "rename", "pin", "tags", "describe",
                "notes"
            ]
        );
        let archived = json!({"status": "stopped", "agent": "claude", "agent_session": "x",
                              "archived_at": 1, "pinned": true, "role": ""});
        assert_eq!(
            labels(&context_legend(false, None, Some(&archived))),
            [
                "resume",
                "unarchive",
                "rename",
                "unpin",
                "tags",
                "describe",
                "notes"
            ]
        );
        let overlord = json!({"status": "idle", "agent": "claude", "agent_session": "x",
                              "archived_at": null, "pinned": false, "role": "overlord"});
        assert_eq!(
            labels(&context_legend(false, None, Some(&overlord))),
            ["attach", "restart", "stop"]
        );
        // No conversation to resume: Enter and restart would only error.
        let fresh = json!({"status": "stopped", "agent": "codex", "archived_at": null,
                           "pinned": false, "role": ""});
        assert_eq!(
            labels(&context_legend(false, None, Some(&fresh)))[0],
            "archive"
        );
        assert!(context_legend(false, None, None).is_empty());
        assert_eq!(
            labels(&context_legend(true, Some("wip"), Some(&running))),
            ["open", "rename group"]
        );
        assert_eq!(
            labels(&context_legend(true, Some(groups::UNTAGGED), None)),
            ["open"]
        );
    }

    fn footer_row(app: &App, width: u16) -> String {
        use ratatui::backend::TestBackend;
        let mut term = ratatui::Terminal::new(TestBackend::new(width, 2)).unwrap();
        term.draw(|f| app.draw_footer(f, f.area())).unwrap();
        (0..width)
            .map(|x| term.backend().buffer()[(x, 0)].symbol().to_string())
            .collect()
    }

    /// Renders the whole screen; `SHOW_SCREEN=1 cargo test -p rlord-tui
    /// whole_screen -- --nocapture` prints it, for eyeballing layout changes.
    #[test]
    fn whole_screen_renders() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.sessions = vec![
            json!({"name": "overlord", "role": "overlord", "status": "idle", "agent": "claude",
                   "model": "opus", "agent_session": "o", "pinned": true, "archived_at": null}),
            json!({"name": "login-flake", "status": "working", "agent": "claude",
                   "agent_session": "a", "tags": ["web", "wip"], "pinned": true,
                   "project_path": "/home/x/src/web/.worktrees/login-flake", "repo_path": "/home/x/src/web",
                   "branch": "login-flake", "description": "fixing the flaky login test",
                   "archived_at": null, "agents_running": 2}),
            json!({"name": "myapp", "status": "waiting_input", "agent": "claude",
                   "agent_session": "b", "tags": ["myapp"], "project_path": "/home/x/src/myapp",
                   "archived_at": null}),
            json!({"name": "infra-bump", "status": "stopped", "agent": "claude",
                   "agent_session": "c", "tags": [], "project_path": "/home/x/src/infra",
                   "archived_at": null}),
        ];
        app.all = app.sessions.clone();
        app.overlord_rows = 1;
        app.table.select(Some(1));
        app.status_line = "launched login-flake".into();
        let (w, h) = (150, 34);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let out: Vec<String> = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        if std::env::var_os("SHOW_SCREEN").is_some() {
            println!("{}", out.join("\n"));
        }
        let all = out.join("\n");
        assert!(all.contains("┏▲▲▲┓") && all.contains("● working") && all.contains("#wip"));

        // The groups tab: group column focused.
        app.tab = Tab::Groups;
        app.groups = vec![
            ("web".into(), 1),
            ("wip".into(), 1),
            (groups::UNTAGGED.into(), 1),
        ];
        app.group_sel = Some("web".into());
        app.group_focus = true;
        app.group_state.select(Some(0));
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let out: Vec<String> = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        if std::env::var_os("SHOW_SCREEN").is_some() {
            println!("{}", out[..12].join("\n"));
        }
        assert!(out.join("\n").contains("Groups"));

        // Every theme renders; themes with a background paint all of it.
        for p in rlord_core::themes::THEMES {
            theme::set(p);
            term.draw(|f| app.draw(f)).unwrap();
            if let Some(bg) = p.bg {
                let corner = &term.backend().buffer()[(w - 1, h - 1)];
                assert_eq!(corner.bg, theme::color(bg), "{}", p.name);
            }
        }
        theme::set(rlord_core::themes::by_name("rlord-dark").unwrap());
    }

    #[test]
    fn footer_divider_is_fixed_and_lists_only_applicable_keys() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.sessions = vec![
            json!({"status": "stopped", "archived_at": 1, "pinned": false, "role": "",
                   "agent": "claude", "agent_session": "y"}),
            json!({"status": "working", "archived_at": null, "pinned": false, "role": "",
                   "agent": "claude", "agent_session": "x"}),
        ];
        let col = |row: &str| row.chars().position(|c| c == '│').expect("divider");
        app.table.select(Some(0));
        let archived = footer_row(&app, 240);
        app.table.select(Some(1));
        let running = footer_row(&app, 240);
        app.table.select(None);
        let none = footer_row(&app, 240);
        assert_eq!(col(&archived), col(&running), "\n{archived}\n{running}");
        assert_eq!(col(&archived), col(&none));

        let (left, right) = archived.split_once('│').unwrap();
        assert!(
            left.contains(" new ") && left.contains(" quit "),
            "{archived}"
        );
        assert!(
            right.contains(" unarchive ") && right.contains(" resume "),
            "{archived}"
        );
        assert!(
            !right.contains(" stop ") && !right.contains(" archive "),
            "{archived}"
        );
        let (_, right) = running.split_once('│').unwrap();
        assert!(
            right.contains(" stop ") && right.contains(" archive "),
            "{running}"
        );
        assert!(!right.contains("unarchive"), "{running}");

        // Narrower than both halves: the global keys stay whole.
        app.table.select(Some(1));
        let narrow = footer_row(&app, 120);
        assert!(
            narrow.contains(" new ") && narrow.contains(" quit ") && narrow.contains(" overlord "),
            "{narrow}"
        );
    }
}

#[cfg(test)]
mod ansi_tests {
    use ansi_to_tui::IntoText;
    use ratatui::style::Color;

    #[test]
    fn captured_ansi_parses_to_styled_spans() {
        let raw = b"\x1b[38;5;174m \xe2\x96\x90\x1b[48;5;16m\xe2\x96\x9b\x1b[39m\x1b[49m   \x1b[1mClaude Code\x1b[0m v2\n\x1b[2mdim line\x1b[0m\n";
        let text = raw.as_slice().into_text().expect("parse");
        assert_eq!(text.lines.len(), 2);
        let first = &text.lines[0];
        assert!(first
            .spans
            .iter()
            .any(|s| s.style.fg == Some(Color::Indexed(174))));
        assert!(first.spans.iter().any(|s| s
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)));
    }
}
