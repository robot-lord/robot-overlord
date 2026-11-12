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
mod keyscreen;
mod launcher;
mod mouse;
mod newform;
mod overlay;
mod overlord;
mod record;
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
use rlord_core::usage::{Usage, UsageMetric};
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
    // The mouse: clicks and the wheel reach the list (see mouse.rs).
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableMouseCapture
    );
    let _ = write!(std::io::stdout(), "\x1b[>4;1m");
    let _ = std::io::stdout().flush();
    t
}

fn restore_terminal() {
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableMouseCapture
    );
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
    /// The cheat sheet (`?`).
    Help,
    /// The command launcher (`:`).
    Command(Box<launcher::Launcher>),
    /// The key screen (`:keys`, or the overlord's `open_keys`).
    Keys(Box<keyscreen::KeyScreen>),
    /// The right-click menu of a row or a group.
    Menu(Box<mouse::Menu>),
    /// "Stop web-fix?", before a click does something hard to undo.
    Confirm(Box<mouse::Confirm>),
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
    /// What the Usage column shows (and the usage sort sorts by).
    usage: UsageMetric,
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
    /// The overlord's fixed row at the top of the list (1 when shown: not
    /// while filtering): unnumbered, so digits count the fleet below it.
    overlord_rows: usize,
    /// The overlord's conversations (`overlord.conversations`: the current
    /// one and the earlier ones), loaded when its row is selected and again
    /// on each daemon change notice (not on the preview's timer).
    convs: Value,
    /// `convs` needs reloading (something changed since it was loaded).
    convs_stale: bool,
    /// Which of them the preview shows: 0 the live pane, n the nth earlier
    /// conversation (newest first).
    conv_sel: usize,
    /// That conversation's text, by id (read once).
    conv_text: Option<(String, String)>,
    /// Session to select once the next refresh lands (set by the overlord).
    pending_select: Option<String>,
    /// The selection as last saved with the daemon (kept across restarts).
    saved_sel: Option<String>,
    /// Last view reported to the daemon (so the overlord sees what we show).
    reported_view: Value,
    /// What is where on screen, rebuilt each frame (see mouse.rs).
    hits: mouse::Hits,
    /// The last left click, for telling a double-click.
    last_click: Option<(Instant, String)>,
    /// The last event was pointer motion that changed nothing: no need to
    /// draw again for it (motion arrives in floods).
    idle_motion: bool,
    /// Preview: lines scrolled up from the bottom (0: following the end),
    /// how far it can go (set when drawn), and the text's length when last
    /// loaded (so new output doesn't move a scrolled view).
    preview_scroll: usize,
    preview_scroll_max: std::cell::Cell<usize>,
    preview_len: usize,
}

impl App {
    fn new(rx: mpsc::Receiver<Push>, config: Config, keys: KeyMap) -> Self {
        App {
            usage: config.usage,
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
            convs: Value::Null,
            convs_stale: true,
            conv_sel: 0,
            conv_text: None,
            pending_select: None,
            saved_sel: None,
            reported_view: Value::Null,
            hits: Default::default(),
            last_click: None,
            idle_motion: false,
            preview_scroll: 0,
            preview_scroll_max: Default::default(),
            preview_len: 0,
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

    /// Show `m` in the Usage column (the toolbar, its key, `:usage`), saved
    /// with the daemon so it's kept across restarts, over config `usage`;
    /// None forgets the pick (config `usage` again). Only once saved: if
    /// the daemon can't, nothing changes and the status line says why.
    pub(crate) fn set_usage(&mut self, m: Option<UsageMetric>) {
        if self
            .call("ui.usage", json!({"usage": m.map(UsageMetric::label)}))
            .is_none()
        {
            self.status_line = format!("Usage column not changed: {}", self.status_line);
            return;
        }
        self.usage = m.unwrap_or(self.config.usage);
        self.dirty = true;
        self.status_line = match m {
            Some(m) => format!("Usage column: {}", m.title()),
            None => format!("Usage column: {} (config.toml)", self.usage.title()),
        };
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
            "usage": self.usage.label(),
            "status": q.status,
            "tag": q.tag,
            "agent": q.agent,
            "project": q.project,
            "group": q.group,
            "search": q.search,
        })
    }

    fn refresh(&mut self) {
        // (A daemon change notice or an action: the overlord's
        // conversations may have changed too.)
        self.convs_stale = true;
        let q = self.query();
        // The selection follows the session, not the row: a re-sort or the
        // overlord rows appearing must not move it to another session.
        let keep = self.selected_id();
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
                if let Some(o) = self
                    .call("overlord.get", json!({}))
                    .filter(|o| !o.is_null())
                {
                    self.sessions.insert(0, o);
                    self.overlord_rows = 1;
                }
            }
            let n = self.sessions.len();
            let sel = self.table.selected().unwrap_or(0);
            let at = keep
                .as_deref()
                .and_then(|id| self.sessions.iter().position(|s| s["id"] == id));
            self.table.select(if n == 0 {
                None
            } else {
                Some(at.unwrap_or(sel.min(n - 1)))
            });
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
        self.status_line = format!("to quit rlord press {}", self.keys.label(Action::Quit));
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
        self.modal_hits(f.area(), area, &lines);
        f.render_widget(Paragraph::new(lines).block(theme::modal("Quit")), area);
    }

    /// For the mouse: a modal drawn in `area` over the screen, its text
    /// `lines` starting inside the border and padding. Nothing under it
    /// can be clicked; its keycaps can.
    pub(crate) fn modal_hits(&self, screen: Rect, area: Rect, lines: &[Line]) {
        self.hits.add(screen, mouse::Hit::Outside);
        self.hits.add(area, mouse::Hit::Inside);
        self.hits.keycaps(area.x + 2, area.y + 1, lines);
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

    /// The text of the overlord's earlier conversation the preview is on
    /// (read once: it no longer changes).
    fn conversation_text(&mut self) -> String {
        let Some(id) = self
            .earlier_convs()
            .get(self.conv_sel - 1)
            .and_then(|c| c["id"].as_str())
            .map(str::to_string)
        else {
            return String::new();
        };
        if let Some((have, text)) = &self.conv_text {
            if *have == id {
                return text.clone();
            }
        }
        let text = match client_call(
            &paths::socket_path(),
            "overlord.conversation",
            json!({"id": id}),
        ) {
            Ok(v) => v["messages"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|m| {
                            let who = match m["who"].as_str().unwrap_or("") {
                                "you" => "you",
                                "agent" => "overlord",
                                w => w,
                            };
                            format!("{who} ▸ {}\n", m["text"].as_str().unwrap_or(""))
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
            Err(e) => format!("(can't show it: {e:#})"),
        };
        self.conv_text = Some((id, text.clone()));
        text
    }

    /// The highlight moved: redraw now, load its preview and details when
    /// the keys pause.
    pub(crate) fn preview_soon(&mut self) {
        self.preview_due = Some(Instant::now() + PREVIEW_SETTLE);
    }

    fn refresh_preview(&mut self) {
        self.last_preview = Instant::now();
        self.preview_due = None;
        // Another session: its preview starts at the end again.
        if self.preview_for != self.selected_id() {
            self.preview_scroll = 0;
            self.convs_stale = true;
        }
        self.preview_for = self.selected_id();
        // Remember where the user is (once the highlight settles), so a
        // restarted list comes back to it.
        if self.preview_for.is_some() && self.preview_for != self.saved_sel {
            self.saved_sel = self.preview_for.clone();
            self.call("ui.selected", json!({"id": self.saved_sel}));
        }
        let Some(s) = self.selected().cloned() else {
            self.preview_text.clear();
            self.agents.clear();
            return;
        };
        // The overlord's conversations, while its row is selected: loaded
        // when it gets selected or something changed, not on every tick.
        if is_overlord_role(&s) {
            if self.convs_stale || self.convs.is_null() {
                self.convs =
                    client_call(&paths::socket_path(), "overlord.conversations", json!({}))
                        .unwrap_or(Value::Null);
                self.convs_stale = false;
            }
            self.conv_sel = self.conv_sel.min(self.earlier_convs().len());
        } else {
            self.convs = Value::Null;
            self.conv_sel = 0;
        }
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
            // One of the overlord's earlier conversations, not its pane.
            Preview::Pane if self.conv_sel > 0 => self.conversation_text(),
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
        // Scrolled up: new output arrives below, the view stays where it is.
        let len = match &self.preview_rich {
            Some(rich) => rich.lines.len(),
            None => self.preview_text.lines().count(),
        };
        if self.preview_scroll > 0 {
            self.preview_scroll += len.saturating_sub(self.preview_len);
        }
        self.preview_len = len;
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.startup();
        self.apply_theme();
        loop {
            self.follow_appearance();
            let refreshed = if self.dirty {
                self.refresh();
                true
            } else if self.preview_due.is_some_and(|t| Instant::now() >= t)
                || (self.preview_due.is_none()
                    && self.last_preview.elapsed()
                        > Duration::from_millis(self.config.preview_refresh_ms))
            {
                self.refresh_preview();
                true
            } else {
                false
            };
            // (Pointer motion that changed nothing: the screen is as drawn.)
            if refreshed || !std::mem::take(&mut self.idle_motion) {
                terminal.draw(|f| self.draw(f))?;
            }

            let mut tick = Duration::from_millis(self.config.preview_refresh_ms.clamp(50, 250));
            if let Some(due) = self.preview_due {
                tick = tick.min(due.saturating_duration_since(Instant::now()));
            }
            if event::poll(tick)? {
                let quit = match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        self.handle_key(k, terminal)?
                    }
                    Event::Mouse(m) => self.handle_mouse(m, terminal)?,
                    _ => false,
                };
                if quit {
                    return Ok(());
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

    /// One key press, in whatever mode the list is in (a click on something
    /// that names a key comes through here too). Returns true to quit.
    fn handle_key(&mut self, k: KeyEvent, terminal: &mut DefaultTerminal) -> Result<bool> {
        // Quitting rlord takes the real quit key; the reflexive
        // ones (Ctrl-C, and q in the list) just say what it is.
        let listening = matches!(&self.mode, Mode::Keys(ks) if ks.listening());
        if !listening && k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c')
        {
            self.quit_hint();
            return Ok(false);
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => return self.handle_normal(k, terminal),
            Mode::Filter(buf) => self.handle_input(k, buf, InputKind::Filter, terminal)?,
            Mode::Rename(buf) => self.handle_input(k, buf, InputKind::Rename, terminal)?,
            Mode::Describe(buf) => self.handle_input(k, buf, InputKind::Describe, terminal)?,
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
            Mode::Help => {}
            Mode::Command(l) => self.handle_launcher(k, l, terminal)?,
            Mode::Keys(ks) => self.handle_keys(k, ks),
            Mode::Menu(m) => return self.handle_menu(k, m, terminal),
            Mode::Confirm(c) => return self.handle_confirm(k, *c, terminal),
            Mode::ConfirmQuit => {
                if matches!(
                    k.code,
                    KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y')
                ) {
                    return Ok(true);
                }
                self.status_line.clear();
            }
        }
        Ok(false)
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
        // On the overlord's row, [ and ] step through its earlier
        // conversations in the preview (unless the user bound them).
        if action.is_none() && k.modifiers.is_empty() && self.selected_is_overlord() {
            match k.code {
                KeyCode::Char('[') => {
                    self.show_conversation(self.conv_sel + 1);
                    return Ok(false);
                }
                KeyCode::Char(']') => {
                    self.show_conversation(self.conv_sel.saturating_sub(1));
                    return Ok(false);
                }
                _ => {}
            }
        }
        let Some(action) = action else {
            if k.code == KeyCode::Char('q') && k.modifiers.is_empty() {
                self.quit_hint();
            }
            return Ok(false);
        };
        self.do_action(action, terminal)
    }

    /// Run one list action (from its key, or the command launcher).
    fn do_action(&mut self, action: Action, terminal: &mut DefaultTerminal) -> Result<bool> {
        let n = self.sessions.len();
        match action {
            Action::Help => self.mode = Mode::Help,
            Action::Command => self.mode = Mode::Command(Box::default()),
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
            // An earlier conversation of the overlord's is in the preview:
            // Enter continues it (asked first).
            Action::Attach if self.selected_is_overlord() && self.conv_sel > 0 => {
                self.confirm_continue()
            }
            Action::Attach => self.attach(terminal)?,
            Action::Restart | Action::Stop
                if self.selected().is_some_and(|s| !s["archived_at"].is_null()) =>
            {
                self.archived_hint()
            }
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
            // Too easy to hit by accident: asked first.
            Action::Archive => self.confirm_archive(),
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
                // A new sort starts in its natural direction (as a header
                // click or the launcher).
                self.sort = self.sort.next();
                self.reverse = false;
                self.dirty = true;
            }
            Action::ReverseSort => {
                self.reverse = !self.reverse;
                self.dirty = true;
            }
            Action::Usage => self.set_usage(Some(self.usage.next())),
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
            Action::Cancel if self.conv_sel > 0 => self.show_conversation(0),
            Action::Cancel => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.dirty = true;
                }
            }
        }
        Ok(false)
    }

    /// The overlord's earlier conversations, as last loaded.
    fn earlier_convs(&self) -> &[Value] {
        self.convs["earlier"].as_array().map_or(&[], |a| a)
    }

    /// Show the overlord's `n`th earlier conversation in the preview (0:
    /// its live pane again).
    pub(crate) fn show_conversation(&mut self, n: usize) {
        let max = self.earlier_convs().len();
        if max == 0 {
            self.status_line = "the overlord has no earlier conversations yet".into();
        }
        self.conv_sel = n.min(max);
        self.preview_scroll = 0;
        self.refresh_preview();
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
        if !s["archived_at"].is_null() {
            self.archived_hint();
            return Ok(());
        }
        if is_overlord_role(&s) {
            // Like any session, full screen — started/resumed if needed, and
            // its window handed back from whatever size an overlay set.
            match self.call("overlord.ensure", json!({})) {
                Some(v) => s = v,
                None => return Ok(()),
            }
            let _ = tmux::auto_size_window(rlord_daemon::overlord::TMUX_NAME);
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
        // What is where, for the mouse: rebuilt as the frame is drawn.
        self.hits.clear();
        // Themes with their own background paint it under everything.
        if theme::bg().is_some() {
            f.render_widget(Block::default().style(theme::base()), f.area());
        }
        // A short window gives up the header art first, then the toolbar
        // and the key hints, and only then list rows (`Chrome::for_height`).
        let chrome = Chrome::for_height(f.area().height);
        let [bars_area, main_area, footer_area] = RLayout::vertical([
            // The header: tabs, then the view, beside the 4-row logo (or
            // the toolbar alone, compact).
            Constraint::Length(chrome.header),
            Constraint::Min(1),
            Constraint::Length(chrome.footer),
        ])
        .areas(f.area());
        if chrome.header > 0 {
            self.draw_bars(f, bars_area);
        }
        let (list_area, right) = match self.config.layout {
            Layout::Side => {
                // The list is what this view is for: it gets its width first
                // (configured share, but never below what its columns need);
                // the preview takes the remainder and simply drops out when
                // that remainder is too small to be useful.
                // (Only the sessions list borrows width for its columns.)
                let list_w = side_list_width(
                    main_area.width,
                    self.config.list_width_pct,
                    self.tab == Tab::Sessions,
                );
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
            Mode::Help => self.draw_help(f),
            Mode::Command(l) => self.draw_launcher(f, l),
            Mode::Keys(ks) => self.draw_keys(f, ks),
            Mode::Menu(m) => self.draw_menu(f, m),
            Mode::Confirm(c) => self.draw_confirm(f, c),
            _ => {}
        }
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        // Which columns fit, and how wide: `ListCols::for_width`.
        let ListCols {
            compact,
            time_w,
            name_w,
            usage: show_usage,
            tags_w,
            project: show_project,
        } = ListCols::for_width(area.width);
        let show_tags = tags_w > 0;
        let sel_w = list_cols::sel_w();

        // The sort shows on its column: an arrow (▲ ascending, ▼
        // descending; `reverse` flips it), and the time column shows the time
        // being sorted by (the last message when the sort isn't a time).
        let arrow_on_name = self.sort == SortMode::Name;
        let arrow_on_usage = self.sort == SortMode::Usage;
        let time_label = match self.sort {
            SortMode::Activity | SortMode::Name | SortMode::Usage if compact => "Message",
            SortMode::User if compact => "Prompt",
            SortMode::State if compact => "Status",
            SortMode::Activity | SortMode::Name | SortMode::Usage => "Last message",
            SortMode::User => "Last prompt",
            SortMode::State => "Status change",
            SortMode::Created => "Created",
        };
        // Names run A→Z (ascending); times newest first, usage most first
        // (descending).
        let ascending = arrow_on_name != self.reverse;
        let arrow = if ascending { " ▲" } else { " ▼" };
        // The sorted column: accent, bold, with its arrow. A column whose
        // content can be switched (the time column follows the sort, Usage
        // its measure) otherwise reads in the quieter accent; fixed ones
        // (Name, Status, Groups) plain.
        let head = |label: &str, sorted: bool, variable: bool| -> Cell<'static> {
            if sorted {
                Cell::from(format!("{label}{arrow}"))
                    .style(Style::default().fg(theme::accent()).bold())
            } else if variable {
                Cell::from(label.to_string()).style(Style::default().fg(theme::accent_dim()))
            } else {
                Cell::from(label.to_string())
            }
        };
        // The header names what the column shows ("Usage" alone says
        // nothing); "Context ▼" wouldn't fit, so sorted it's "Ctx % ▼".
        let usage_label = match self.usage {
            UsageMetric::Turns => "Turns",
            UsageMetric::Tokens => "Tokens",
            UsageMetric::Context if arrow_on_usage => "Ctx %",
            UsageMetric::Context => "Context",
        };
        let mut header = vec![
            Cell::from(""),
            Cell::from(""),
            head("Name", arrow_on_name, false),
            Cell::from("Status"),
            head(time_label, !arrow_on_name && !arrow_on_usage, true),
        ];
        let mut widths = vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(name_w),
            Constraint::Length(13),
            Constraint::Length(time_w),
        ];
        if show_usage {
            header.push(head(usage_label, arrow_on_usage, true));
            widths.push(Constraint::Length(list_cols::USAGE_W));
        }
        if show_tags {
            // (Squeezed to a count, `#3`, the header too.)
            header.push(Cell::from(if tags_w >= 6 { "Groups" } else { "#" }));
            widths.push(Constraint::Length(tags_w));
        }
        if show_project {
            header.push(Cell::from("Project"));
            widths.push(Constraint::Min(10));
        }
        let header = Row::new(header).style(theme::muted()).bottom_margin(1);
        let sort = self.sort;
        let metric = self.usage;
        let home = std::env::var("HOME").unwrap_or_default();
        let rows: Vec<Row> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let get = |k: &str| s[k].as_str().unwrap_or("-").to_string();
                let status = get("status");
                let is_overlord = is_overlord_role(s);
                let hue = theme::overlord();
                let num = i.saturating_sub(self.overlord_rows);
                let mut cells = vec![
                    if is_overlord {
                        Cell::from("◆").fg(hue)
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
                    // Which agent runs it (its marker), then its name.
                    Cell::from(Line::from({
                        let mut spans = theme::agent_marker(s["agent"].as_str().unwrap_or(""));
                        spans.push(Span::raw(get("name")));
                        spans
                    })),
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
                    // The time the list is sorted by (activity otherwise).
                    Cell::from(Span::styled(
                        rel_age(match sort {
                            // (As the sort: created when there's none.)
                            SortMode::User => {
                                s["last_user_msg_at"].as_i64().or(s["created_at"].as_i64())
                            }
                            SortMode::State => s["last_state_change_at"]
                                .as_i64()
                                .or(s["created_at"].as_i64()),
                            SortMode::Created => s["created_at"].as_i64(),
                            _ => updated_at(s),
                        }),
                        theme::muted(),
                    )),
                ];
                if show_usage {
                    // 4 wide whatever the number, so the column never
                    // shifts.
                    let u = Usage::from_json(&s["usage"]);
                    let text = match &u {
                        Some(u) => u.cell(metric),
                        // (The daemon's warm-up is reading it.)
                        None if s["usage_loading"] == true => format!("{:>4}", "…"),
                        None => rlord_core::usage::cell(None, metric),
                    };
                    cells.push(
                        // (A fixed 5, under whatever label: values never
                        // move when the header changes.)
                        Cell::from(format!(" {text}")).fg(if u.is_some() {
                            theme::text()
                        } else {
                            theme::faint_c()
                        }),
                    );
                }
                if show_tags {
                    let tags = s["tags"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                        .unwrap_or_default();
                    cells.push(if is_overlord {
                        // Its conversation: when it started fresh, and how
                        // many came before (in full under Project, when
                        // that column shows).
                        Cell::from(if show_project {
                            String::new()
                        } else {
                            fresh_short(&s["conversations"], tags_w as usize)
                        })
                        .fg(theme::faint_c())
                    } else {
                        Cell::from(tags_cell(&tags, tags_w as usize))
                    });
                }
                if show_project {
                    cells.push(if is_overlord {
                        Cell::from(format!(
                            "{} · {} to talk{}",
                            s["model"].as_str().unwrap_or("default model"),
                            self.keys.label(Action::Overlord),
                            rlord_daemon::recall::fresh_label(&s["conversations"])
                                .map(|l| format!(" · {l}"))
                                .unwrap_or_default(),
                        ))
                        .fg(theme::faint_c())
                    } else {
                        Cell::from(get("project_path").replacen(&home, "~", 1)).fg(theme::faint_c())
                    });
                }
                // A line of space under the overlord's row: it's a
                // section of its own, not the top of the sorted list.
                let last_seat = self.overlord_rows > 0
                    && i + 1 == self.overlord_rows
                    && self.sessions.len() > self.overlord_rows;
                let row = Row::new(cells).bottom_margin(u16::from(last_seat));
                if is_overlord {
                    row.style(Style::default().fg(hue).bold())
                } else {
                    row.style(Style::default().fg(theme::text()))
                }
            })
            .collect();
        // The list is the focused pane unless the group column has focus.
        let focused = !(self.tab == Tab::Groups && self.group_focus);
        let block = theme::panel(self.list_title(), focused);
        let widths_for_hits = widths.clone();
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
        // For the mouse: the table (wheel), then each visible row. Rows
        // start under the border, the header and its margin.
        self.hits.add(area, mouse::Hit::List);
        let inner = Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(2),
            y: area.y + 3,
            height: area.height.saturating_sub(4),
        };
        let top = self.table.offset();
        let mut y = inner.y;
        // Rows past the bottom edge (all of them when none fit).
        let mut below = self.sessions.len().saturating_sub(top);
        for i in top..self.sessions.len() {
            if y >= inner.y + inner.height {
                break;
            }
            below -= 1;
            self.hits.row(inner, y, mouse::Hit::Row(i));
            // The gap under the last overlord row: a faint rule over it
            // (it also covers the selection highlight that spills into the
            // row's margin).
            if self.overlord_rows > 0
                && i + 1 == self.overlord_rows
                && self.sessions.len() > self.overlord_rows
            {
                y += 1;
                if y < inner.y + inner.height {
                    let rule = Rect::new(area.x + 2, y, area.width.saturating_sub(4), 1);
                    f.render_widget(
                        Paragraph::new("╌".repeat(rule.width as usize))
                            .style(Style::default().fg(theme::border())),
                        rule,
                    );
                }
            }
            y += 1;
        }
        // Rows off screen, counted on the box's border: the user can scroll
        // that way. (The border is there at any height.)
        let more = |n: usize, arrow: &str| {
            Line::from(Span::styled(format!(" {arrow} {n} more "), theme::muted()))
        };
        let more_at = |line: &Line, y: u16| {
            let w = (line.width() as u16).min(area.width.saturating_sub(4));
            Rect::new(area.x + area.width.saturating_sub(w + 2), y, w, 1)
        };
        if top > 0 && area.height >= 2 {
            let l = more(top, "↑");
            f.render_widget(Paragraph::new(l.clone()), more_at(&l, area.y));
        }
        if below > 0 && area.height >= 2 {
            let l = more(below, "↓");
            f.render_widget(
                Paragraph::new(l.clone()),
                more_at(&l, area.y + area.height - 1),
            );
        }
        // The headers of the sortable columns: Name and the time column,
        // placed by the same layout the table uses (inside the border and
        // padding, after the highlight symbol, two cells between columns).
        let cols_area = Rect::new(
            area.x + 2 + sel_w,
            area.y + 1,
            area.width.saturating_sub(4 + sel_w),
            1,
        );
        let cols = RLayout::horizontal(widths_for_hits.clone())
            .flex(ratatui::layout::Flex::Start)
            .spacing(2)
            .split(cols_area);
        let (name_x, name_w) = (cols[2].x, cols[2].width);
        let (time_x, time_hw) = (cols[4].x, cols[4].width);
        let time_sort = match self.sort {
            SortMode::Name | SortMode::Usage => SortMode::Activity,
            m => m,
        };
        let head_y = area.y + 1;
        self.hits.add(
            Rect::new(name_x, head_y, name_w, 1),
            mouse::Hit::SortBy(SortMode::Name),
        );
        self.hits.add(
            Rect::new(time_x, head_y, time_hw, 1),
            mouse::Hit::SortBy(time_sort),
        );
        if show_usage {
            self.hits.add(
                Rect::new(cols[5].x, head_y, cols[5].width, 1),
                mouse::Hit::SortBy(SortMode::Usage),
            );
        }
    }

    /// Archived sessions are a record: they open again only once unarchived.
    fn archived_hint(&mut self) {
        self.status_line = format!(
            "archived: {} unarchives it, then it can be opened",
            self.keys.label(Action::Unarchive)
        );
    }

    fn draw_preview(&self, f: &mut Frame, area: Rect) {
        // An archived session has no live pane: show its record instead.
        if let Some(s) = self.selected().filter(|s| !s["archived_at"].is_null()) {
            return self.draw_record(f, area, s);
        }
        let name = self
            .selected()
            .and_then(|s| s["name"].as_str())
            .unwrap_or("");
        let log = self.keys.label(Action::ToggleLog);
        let (what, other) = match self.preview {
            Preview::Pane => ("live pane", "log"),
            Preview::Log => ("event log", "pane"),
        };
        let what = if self.preview == Preview::Pane && self.conv_sel > 0 {
            format!(
                "earlier conversation {} of {} · [ ] step · {} continue it · {} back",
                self.conv_sel,
                self.convs["total_earlier"]
                    .as_u64()
                    .unwrap_or(self.conv_sel as u64),
                self.keys.label(Action::Attach),
                self.keys.label(Action::Cancel),
            )
        } else {
            what.to_string()
        };
        let title = Line::from(vec![
            Span::raw(" "),
            Span::styled(name.to_string(), Style::default().fg(theme::text()).bold()),
            Span::styled(format!(" · {what} "), theme::muted()),
        ]);
        let switch = Line::from(vec![
            // Scrolled up with the wheel: say so (it no longer follows).
            Span::styled(
                if self.preview_scroll > 0 {
                    " ↑ scrolled "
                } else {
                    ""
                },
                Style::default().fg(theme::yellow()),
            ),
            Span::styled(format!(" {log} {other} "), theme::faint()),
        ])
        .right_aligned();
        use unicode_width::UnicodeWidthStr;
        // The pane/log switch in the title, and the wheel over the pane.
        self.hits.add(area, mouse::Hit::Preview);
        if let Some(k) = self.keys.event(Action::ToggleLog) {
            let w = (log.width() + other.width() + 3) as u16;
            self.hits.add(
                Rect::new(area.right().saturating_sub(w + 1), area.y, w, 1),
                mouse::Hit::Key(k),
            );
        }
        // Show the tail that fits: the bottom of a pane is what matters
        // (the wheel scrolls back from it).
        let inner_h = area.height.saturating_sub(2) as usize;
        let window = |len: usize| {
            let max = len.saturating_sub(inner_h);
            self.preview_scroll_max.set(max);
            let start = max - self.preview_scroll.min(max);
            start..(start + inner_h).min(len)
        };
        // Loaded for another row (the highlight is moving): show nothing
        // rather than the wrong session's screen.
        let current = self.preview_for == self.selected_id();
        let text: Text = match &self.preview_rich {
            _ if !current => Text::from(Span::styled("…", theme::faint())),
            Some(rich) if self.preview == Preview::Pane && self.conv_sel == 0 => {
                Text::from(rich.lines[window(rich.lines.len())].to_vec())
            }
            _ => {
                let lines: Vec<&str> = self.preview_text.lines().collect();
                Text::from(
                    lines[window(lines.len())]
                        .iter()
                        .map(|l| Line::raw(*l))
                        .collect::<Vec<_>>(),
                )
            }
        };
        let block = theme::panel(title, false).title(switch);
        // A conversation is prose: wrapped (a pane is drawn as it is).
        let p = Paragraph::new(text).block(block);
        if self.preview == Preview::Pane && self.conv_sel > 0 {
            f.render_widget(p.wrap(Wrap { trim: false }), area);
        } else {
            f.render_widget(p, area);
        }
    }

    /// The control-layer card: everything we know about the session that
    /// isn't the live pane, plus the human-authored description and notes.
    fn draw_details(&self, f: &mut Frame, area: Rect) {
        let block = theme::panel(theme::title("Details"), false);
        let Some(s) = self.selected() else {
            f.render_widget(block, area);
            return;
        };
        if is_overlord_role(s) {
            return self.draw_overlord_details(f, area, s);
        }
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
            Line::from({
                let mut l = vec![key("agent")];
                l.extend(theme::agent_marker(&get("agent")));
                l.extend([
                    Span::styled(get("agent"), Style::default().fg(theme::text())),
                    sep(),
                    Span::styled(
                        s["model"].as_str().unwrap_or("default model").to_string(),
                        Style::default().fg(theme::text()),
                    ),
                    sep(),
                    Span::styled(format!("id {}", get("id")), theme::faint()),
                ]);
                l
            }),
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
            Line::from(vec![
                key("usage"),
                Span::styled(usage_details(&s["usage"]), theme::muted()),
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

    /// The overlord's card: what runs it, then its conversations. It
    /// restarts on a fresh one after sitting idle; the earlier ones are
    /// listed (newest first) and can be looked at in the preview.
    fn draw_overlord_details(&self, f: &mut Frame, area: Rect, s: &Value) {
        f.render_widget(theme::panel(theme::title("Details"), false), area);
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(2),
        };
        let get = |k: &str| s[k].as_str().unwrap_or("-").to_string();
        let key = |k: &str| Span::styled(format!("{k:<10}"), theme::faint());
        let sep = || Span::styled("  ·  ", theme::faint());
        let c = &s["conversations"];
        let mut lines = vec![
            Line::from(vec![
                Span::styled(get("name"), Style::default().fg(theme::overlord()).bold()),
                Span::raw("   "),
                theme::status_chip(&get("status")),
                Span::styled(
                    format!("  ·  {} to talk", self.keys.label(Action::Overlord)),
                    theme::muted(),
                ),
            ]),
            Line::from({
                let mut l = vec![key("agent")];
                l.extend(theme::agent_marker(&get("agent")));
                l.extend([
                    Span::styled(get("agent"), Style::default().fg(theme::text())),
                    sep(),
                    Span::styled(
                        s["model"].as_str().unwrap_or("default model").to_string(),
                        Style::default().fg(theme::text()),
                    ),
                    sep(),
                    Span::styled(
                        // What it was launched with, not the config's.
                        match s["effort"].as_str().unwrap_or("").trim() {
                            "" => "default effort".to_string(),
                            e => format!("effort {e}"),
                        },
                        theme::muted(),
                    ),
                ]);
                if let Some(st) = s["remote"]["state"].as_str() {
                    l.push(sep());
                    l.push(Span::styled(format!("remote {st}"), theme::muted()));
                }
                l
            }),
            Line::from(vec![
                key("fresh"),
                Span::styled(
                    match rlord_daemon::recall::fresh_label(c) {
                        Some(l) => format!(
                            "{}  —  {}",
                            l.trim_start_matches("fresh "),
                            c["why"].as_str().unwrap_or("a fresh conversation")
                        ),
                        None => match self.config.overlord.fresh_after_mins {
                            0 => "never restarted (fresh restarts are off)".to_string(),
                            m => format!(
                                "its first conversation (a fresh one starts after {m}m idle)"
                            ),
                        },
                    },
                    theme::muted(),
                ),
            ]),
        ];
        let earlier = self.earlier_convs();
        if !earlier.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(
                    "earlier conversations  ·  [ ] look  ·  {} continues the one shown",
                    self.keys.label(Action::Attach)
                ),
                theme::faint(),
            )));
        }
        let head = lines.len() as u16;
        f.render_widget(
            Paragraph::new(lines),
            Rect {
                height: head.min(inner.height),
                ..inner
            },
        );
        // The list, scrolled so the one in the preview stays in view.
        let list = Rect {
            y: inner.y + head,
            height: inner.height.saturating_sub(head),
            ..inner
        };
        if list.height == 0 {
            return;
        }
        let rows = list.height as usize;
        let sel = self.conv_sel.checked_sub(1);
        let top = sel.map_or(0, |i| i.saturating_sub(rows - 1));
        let mut out = Vec::new();
        for (i, c) in earlier.iter().enumerate().skip(top).take(rows) {
            let on = sel == Some(i);
            let turns = c["turns"].as_u64().unwrap_or(0);
            let ended = match c["ended"].as_str().unwrap_or("") {
                "idle" => "idle",
                "clear" => "/clear",
                "manual" => "restart",
                "continued" => "left",
                _ => "moved on",
            };
            out.push(Line::from(vec![
                Span::styled(
                    if on { "▸ " } else { "  " },
                    Style::default().fg(theme::accent()),
                ),
                Span::styled(
                    format!("{:>4} ago  ", rel_age(c["ended_at"].as_i64())),
                    theme::muted(),
                ),
                Span::styled(
                    format!(
                        "{turns:>3} {:<7}",
                        if turns == 1 { "turn" } else { "turns" }
                    ),
                    theme::faint(),
                ),
                Span::styled(format!("{ended:<9}"), theme::faint()),
                Span::styled(
                    match c["first_prompt"].as_str() {
                        Some(p) => p.to_string(),
                        None if c["on_disk"] == false => "(transcript gone)".to_string(),
                        None => "(nothing said)".to_string(),
                    },
                    if on {
                        Style::default().fg(theme::text()).bold()
                    } else {
                        Style::default().fg(theme::text())
                    },
                ),
            ]));
            self.hits
                .row(list, list.y + (i - top) as u16, mouse::Hit::Conv(i + 1));
        }
        f.render_widget(Paragraph::new(out), list);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let prompt = match &self.mode {
            Mode::Normal => None,
            Mode::Filter(b) => Some(format!(
                "search: {b}▏   (enter: jump · tab: keep filter · esc: clear · status:needs|turn|working · tag: · proj:)"
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
            Mode::Help => Some("help: any key closes".to_string()),
            Mode::Command(l) => Some(format!(
                "command: {}▏   (enter: run · tab: complete · ↑↓: choose · esc: close)",
                l.buf
            )),
            Mode::Keys(_) => Some("keys — see the key screen".to_string()),
            // The menu sits over the list; the footer stays as it is.
            Mode::Menu(_) | Mode::Confirm(_) => None,
        };
        let line2 = if self.status_line.is_empty() {
            Line::raw("")
        } else {
            Line::from(vec![
                Span::styled(" › ", Style::default().fg(theme::accent())),
                Span::styled(self.status_line.clone(), theme::muted()),
            ])
        };
        // One row (a short window): what's being typed, else the status
        // line; the key hints are dropped.
        let one_row = area.height < 2;
        if one_row && prompt.is_none() {
            f.render_widget(Paragraph::new(line2), area);
            return;
        }
        let [line1_area, line2_area] = if one_row {
            [area, Rect::default()]
        } else {
            RLayout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area)
        };
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
        // Each keycap, with its label, is also something to click: it
        // presses that key.
        let press = |a: Action| self.keys.event(a).map(mouse::Hit::Key);
        let mut global_hits = Vec::new();
        let global = Line::from(
            self.keys
                .global_legend()
                .into_iter()
                .flat_map(|(a, label)| {
                    let cap = keycap(&self.keys.label(a), label, Cap::Global);
                    global_hits.extend(keycap_hits(&cap, press(a)));
                    cap
                })
                .collect::<Vec<_>>(),
        );
        let mut ctx: Vec<(String, &'static str, Option<mouse::Hit>)> = context_legend(
            self.tab == Tab::Groups && self.group_focus,
            self.group_sel.as_deref(),
            self.selected(),
        )
        .into_iter()
        .map(|(a, label)| (self.keys.label(a), label, press(a)))
        .collect();
        // Inside a group, say how to get back out.
        if self.tab == Tab::Groups && !self.group_focus {
            ctx.insert(
                0,
                (
                    self.keys.label(Action::Cancel),
                    "groups",
                    press(Action::Cancel),
                ),
            );
        }
        // On the group list: favourite it, and reorder favourites.
        if self.tab == Tab::Groups && self.group_focus {
            if let Some(g) = self.group_sel.as_deref().filter(|g| *g != groups::UNTAGGED) {
                let fav = self.is_favourite(g);
                ctx.push((
                    self.keys.label(Action::Pin),
                    if fav { "unfavourite" } else { "favourite" },
                    press(Action::Pin),
                ));
                if fav {
                    ctx.push(("J/K".into(), "reorder", None));
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
        self.hits.spans(left.x, left.y, global_hits);
        // (After the divider, "│  ".)
        let mut ctx_hits = vec![(3, None)];
        let ctx_line = context_line(
            ctx.into_iter()
                .map(|(k, label, hit)| {
                    ctx_hits.extend(keycap_hits(&keycap(&k, label, Cap::Context), hit));
                    (k, label)
                })
                .collect(),
        );
        self.hits.spans(right.x, right.y, ctx_hits);
        f.render_widget(Paragraph::new(global), left);
        f.render_widget(Paragraph::new(ctx_line), right);
    }
}

/// What the list view keeps around the list at a window height: header
/// rows (4: the logo, name and toolbar; 1: the toolbar alone; 0: none) and
/// footer rows (2: key hints and the status line; 1: the status line or
/// what's being typed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Chrome {
    header: u16,
    footer: u16,
}

impl Chrome {
    /// The full header and footer.
    const HEADER: u16 = 4;
    const FOOTER: u16 = 2;
    /// The list box with about five sessions in view: its borders, the
    /// column header and its margin, the overlord's row and the rule under
    /// it, five rows.
    const LIST_ROOM: u16 = 2 + 2 + 2 + 5;

    /// The most that leaves the list `LIST_ROOM`: first the art goes (the
    /// toolbar keeps a row), then the key hints, then the toolbar. Below
    /// that the list itself shrinks.
    fn for_height(h: u16) -> Chrome {
        [
            (Self::HEADER, Self::FOOTER),
            (1, Self::FOOTER),
            (1, 1),
            (0, 1),
        ]
        .into_iter()
        .map(|(header, footer)| Chrome { header, footer })
        .find(|c| h >= c.header + c.footer + Self::LIST_ROOM)
        .unwrap_or(Chrome {
            header: 0,
            footer: u16::from(h > 2),
        })
    }
}

/// A keycap's two spans as hit parts: the chip and its label press the
/// key; the gap after the label doesn't.
fn keycap_hits(
    cap: &[Span<'static>; 2],
    hit: Option<mouse::Hit>,
) -> [(u16, Option<mouse::Hit>); 3] {
    use unicode_width::UnicodeWidthStr;
    let label = cap[1].content.trim_end().width() as u16;
    [
        (cap[0].width() as u16, hit.clone()),
        (label, hit),
        (cap[1].width() as u16 - label, None),
    ]
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

/// The list's "Last message" column: the latest message either way (not a
/// resume or a state flip), else when it was created.
fn updated_at(s: &Value) -> Option<i64> {
    ["last_agent_msg_at", "last_user_msg_at", "created_at"]
        .iter()
        .filter_map(|k| s[*k].as_i64())
        .max()
}

/// The details pane's usage line: all three, whatever the column shows
/// (`12 turns · 1.2M tokens · 42% of 200k context`).
fn usage_details(u: &Value) -> String {
    use rlord_core::usage::compact;
    let Some(u) = Usage::from_json(u) else {
        return "–".into();
    };
    let context = match (u.context_pct(), u.window) {
        (Some(p), Some(w)) => format!("{p}% of {} context", compact(w)),
        _ => format!("{} context (window unknown)", compact(u.context)),
    };
    format!(
        "{} turns · {} tokens · {context}",
        u.turns,
        compact(u.tokens)
    )
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

/// The session list's column widths, shared by the list (what to draw) and
/// the side layout (how much width the list needs).
mod list_cols {
    /// The Groups column at its widest; it shrinks to `TAGS_MIN` (room
    /// for a count, `#12`) before a name gives up its readable width.
    pub const TAGS_W: u16 = 15;
    pub const TAGS_MIN: u16 = 4;
    /// The Usage column, headed by its measure: "Tokens ▼" at most (fixed,
    /// so changing the measure moves nothing).
    pub const USAGE_W: u16 = 8;
    /// The narrowest a name gets (its agent marker and a few letters).
    pub const NAME_MIN: u16 = 10;
    /// A readable name: Groups show only while names get this much, and
    /// the side layout lends the list width to reach it.
    pub const NAME_OK: u16 = 18;
    /// The widest a name column gets.
    pub const NAME_MAX: u16 = 26;
    /// The time column: its full header ("Last message ▼"), or the short
    /// one ("Message ▼").
    pub const TIME_W: u16 = 15;
    pub const TIME_W_COMPACT: u16 = 10;
    /// The highlight symbol's width (drawn left of every row).
    pub fn sel_w() -> u16 {
        crate::theme::SELECT_BAR.chars().count() as u16
    }
    /// Everything but the name, Usage and the time column: borders and
    /// padding, the highlight symbol, number, pin, status, 4 gaps.
    pub fn base() -> u16 {
        2 + 2 + sel_w() + 1 + 1 + 13 + 2 * 4
    }
    /// What the list needs to show a readable name, Status, the full time
    /// column and Usage: the side layout widens the list to this when its
    /// configured share falls short.
    pub fn core_width() -> u16 {
        base() + TIME_W + NAME_OK + USAGE_W + 2
    }
}

/// Which list columns show at a width, and how wide. Usage comes first
/// after the fixed columns, then a readable name, then Groups, then the
/// rest of the name, then Project. As the list narrows the least
/// essential go first: Project, then the name back to readable, then
/// Groups shrinks (full tags, then `#N`) and goes, then the name shrinks
/// to its minimum (the time header shortens on the way), and Usage goes
/// last, only once a name would get less than its minimum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListCols {
    compact: bool,
    time_w: u16,
    name_w: u16,
    usage: bool,
    /// The Groups column's width (0: not shown).
    tags_w: u16,
    project: bool,
}

impl ListCols {
    fn for_width(width: u16) -> Self {
        use list_cols::*;
        let base = base();
        // The time column's header names its sort; where a readable name
        // wouldn't fit beside it and Usage (under `core_width`), the short
        // form.
        let compact = width < core_width();
        let time_w = if compact { TIME_W_COMPACT } else { TIME_W };
        let room = width.saturating_sub(base + time_w);
        let usage = room >= USAGE_W + 2 + NAME_MIN;
        let room = if usage { room - USAGE_W - 2 } else { room };
        let tags_w = if room >= NAME_OK + 2 + TAGS_MIN {
            (room - NAME_OK - 2).min(TAGS_W)
        } else {
            0
        };
        let room = if tags_w > 0 { room - tags_w - 2 } else { room };
        // A name never narrows as the list widens: it stays at the minimum
        // until Usage is in, at a readable width until Groups is full,
        // and only then takes the rest (the spare cells stay blank).
        let name_w = if !usage {
            NAME_MIN
        } else if tags_w < TAGS_W {
            room.clamp(NAME_MIN, NAME_OK)
        } else {
            room.clamp(NAME_MIN, NAME_MAX)
        };
        let project = room >= NAME_MAX + 2 + 12;
        ListCols {
            compact,
            time_w,
            name_w,
            usage,
            tags_w,
            project,
        }
    }
}

/// The list's width in the side layout. Its configured share
/// (`list_width_pct`) first, never under 60; where that's too narrow for
/// the core columns (`list_cols::core_width`), it borrows from the
/// preview up to that, leaving the preview at least 40 columns or 30%.
/// It never takes less than the configured share gives. `borrow` false
/// (the groups tab): the share alone.
fn side_list_width(total: u16, list_width_pct: u16, borrow: bool) -> u16 {
    let pct = list_width_pct.clamp(20, 80) as u32;
    let want = ((total as u32 * pct / 100) as u16).max(60).min(total);
    if !borrow {
        return want;
    }
    let right_min = 40u16.max((total as u32 * 30 / 100) as u16);
    let cap = total.saturating_sub(right_min);
    want.max(list_cols::core_width().min(cap))
}

/// The Groups column of a row: its tags when they fit in `width`, else how
/// many groups it is in, dimmed as a tag would read (`#3`), or "…" when not
/// even that fits; never a tag cut mid-word. All of them show in the
/// details pane and the groups tab.
fn tags_cell(tags: &[&str], width: usize) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;
    // `#a #b`: a `#` each, a space between.
    let need: usize =
        tags.iter().map(|t| t.width() + 1).sum::<usize>() + tags.len().saturating_sub(1);
    if need <= width {
        return Line::from(theme::tag_spans(tags.iter().copied()));
    }
    // Too many to list: how many, as a tag would read (`#3`), dim so it
    // isn't taken for a tag of its own.
    let text = [format!("#{}", tags.len()), "…".to_string()]
        .into_iter()
        .find(|t| t.width() <= width)
        .unwrap_or_default();
    Line::from(Span::styled(text, theme::muted()))
}

/// The overlord's conversation in a narrow column: "fresh 2h · 14" (when
/// it started, how many came before), or as short as the column needs
/// (`↻2h · 14`, `↻2h`); empty when it has never restarted. The details
/// card has it in full.
fn fresh_short(c: &Value, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    match c["earlier"].as_u64() {
        Some(n) if n > 0 => {
            let age = rel_age(c["fresh_at"].as_i64());
            [
                format!("fresh {age} · {n}"),
                format!("↻{age} · {n}"),
                format!("↻{age}"),
            ]
            .into_iter()
            .find(|t| t.width() <= width)
            .unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// The overlord's row: fixed, special.
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
    /// Cycle the Usage column's measure (turns → tokens → context).
    Usage,
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
    Help,
    Command,
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

    /// The key press this binding answers to.
    fn event(&self) -> KeyEvent {
        let mut mods = KeyModifiers::NONE;
        if self.ctrl {
            mods |= KeyModifiers::CONTROL;
        }
        if self.alt {
            mods |= KeyModifiers::ALT;
        }
        KeyEvent::new(self.code, mods)
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
        // The same labels as the key screen and tmux hints.
        format!(
            "{}{name}",
            rlord_core::keys::mods_label(self.ctrl, self.alt)
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
            (Action::Usage, &k.usage),
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
            (Action::Help, &k.help),
            (Action::Command, &k.command),
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
        // So do `?` and `:` (help, commands) and `U` (usage), newer still.
        let default_step = |a: &Action| {
            (*a == Action::SessionsTab && k.sessions_tab == "H")
                || (*a == Action::GroupsTab && k.groups_tab == "L")
                || (*a == Action::Help && k.help == "?")
                || (*a == Action::Command && k.command == ":")
                || (*a == Action::Usage && k.usage == "U")
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

    /// The key press that runs an action (a click on its keycap sends
    /// it); None when the action has no key.
    fn event(&self, a: Action) -> Option<KeyEvent> {
        self.bindings
            .iter()
            .find(|(x, _)| *x == a)
            .map(|(_, b)| b.event())
    }

    /// Keys that work whatever is highlighted.
    fn global_legend(&self) -> Vec<(Action, &'static str)> {
        // The everyday keys only: the rest are on the cheat sheet (`?`)
        // and in the launcher (`:`).
        vec![
            (Action::Overlord, "overlord"),
            (Action::SwitchTab, "sessions/groups"),
            (Action::New, "new"),
            (Action::Command, "commands"),
            (Action::Help, "help"),
            (Action::Quit, "quit"),
        ]
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
    // Archived: a record. Unarchive first; its notes can still be kept up.
    if archived {
        return vec![
            (Action::Unarchive, "unarchive"),
            (Action::Rename, "rename"),
            (Action::Tags, "tags"),
            (Action::Describe, "describe"),
            (Action::Notes, "notes"),
        ];
    }
    // Resuming (Enter on a stopped row) and restarting both need a
    // conversation the agent can resume.
    // (A Codex session gets its conversation id only once Codex reports
    // it; until then there is nothing to resume.)
    let known = s["agent"]
        .as_str()
        .is_some_and(|a| rlord_core::agents::Agent::parse(a).is_some());
    let resumable = known && !s["agent_session"].is_null();
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
            // A record: unarchive first, no resume.
            ["unarchive", "rename", "tags", "describe", "notes"]
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
                   "project_path": "/home/alice/src/web/.worktrees/login-flake", "repo_path": "/home/alice/src/web",
                   "branch": "login-flake", "description": "fixing the flaky login test",
                   "archived_at": null, "agents_running": 2}),
            json!({"name": "myapp", "status": "waiting_input", "agent": "claude",
                   "agent_session": "b", "tags": ["myapp"], "project_path": "/home/alice/src/myapp",
                   "archived_at": null}),
            json!({"name": "infra-bump", "status": "stopped", "agent": "claude",
                   "agent_session": "c", "tags": [], "project_path": "/home/alice/src/infra",
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
        assert!(all.contains("╔▲▲▲╗") && all.contains("● working") && all.contains("#wip"));

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
    fn groups_column_never_cuts_a_tag() {
        let text = |tags: &[&str]| -> String {
            tags_cell(tags, 15)
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect()
        };
        assert_eq!(text(&[]), "");
        assert_eq!(text(&["web", "wip"]), "#web #wip");
        // Exactly the column's width still fits.
        assert_eq!(text(&["release", "tools"]), "#release #tools");
        // Too wide: how many, as a tag reads (`#2`), not "#release #tooli".
        assert_eq!(text(&["release", "tooling"]), "#2");
        assert_eq!(text(&["release", "tooling", "wip"]), "#3");
        assert_eq!(text(&["a-very-long-group-name"]), "#1");
        // The count is dim, not a tag's colour.
        let cell = tags_cell(&["release", "tooling"], 15);
        assert_eq!(cell.spans[0].style, theme::muted());
        // Squeezed: the count fits 3 cells (4 for 10 or more), then "…".
        let narrow = |tags: &[&str], w| -> String {
            tags_cell(tags, w)
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect()
        };
        assert_eq!(narrow(&["release", "tooling"], 3), "#2");
        assert_eq!(narrow(&["a"; 12], 4), "#12");
        assert_eq!(narrow(&["a"; 12], 1), "…");
        assert_eq!(narrow(&["release"], 0), "");
        // The overlord's conversation: in full where it fits, then short.
        assert_eq!(fresh_short(&json!({"earlier": 0}), 15), "");
        let c = json!({"earlier": 3, "fresh_at": rlord_core::types::now_ms() - 7_200_000});
        assert_eq!(fresh_short(&c, 15), "fresh 2h · 3");
        assert_eq!(fresh_short(&c, 8), "↻2h · 3");
        assert_eq!(fresh_short(&c, 4), "↻2h");
        assert_eq!(fresh_short(&c, 2), "");
    }

    /// `x` (and so the menu and the launcher, which run the same action)
    /// asks before archiving: y / Enter archive, anything else doesn't.
    #[test]
    fn archive_asks_first() {
        use ratatui::backend::TestBackend;
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.sessions = vec![json!({"id": "01a", "name": "web-fix", "status": "idle",
            "archived_at": null, "pinned": false, "role": "", "agent": "claude"})];
        app.table.select(Some(0));
        let mut term = ratatui::Terminal::new(TestBackend::new(80, 24)).unwrap();
        // (Never drawn to: the key handlers only take it to hand the
        // screen to an editor or tmux. A fixed viewport asks no tty.)
        let mut real = ratatui::Terminal::with_options(
            ratatui::backend::CrosstermBackend::new(std::io::stdout()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 80, 24)),
            },
        )
        .unwrap();
        let x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        app.handle_key(x, &mut real).unwrap();
        assert!(matches!(app.mode, Mode::Confirm(_)), "x opens the confirm");
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let out: String = (0..24)
            .flat_map(|y| (0..80).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(out.contains("Archive web-fix?"), "{out}");
        // n / Esc: nothing happens.
        for no in [KeyCode::Char('n'), KeyCode::Esc] {
            app.mode = Mode::Normal;
            app.handle_key(x, &mut real).unwrap();
            app.handle_key(KeyEvent::new(no, KeyModifiers::NONE), &mut real)
                .unwrap();
            assert!(matches!(app.mode, Mode::Normal));
            assert_eq!(app.status_line, "cancelled");
        }
        // The overlord's row has nothing to archive.
        app.sessions[0]["role"] = json!("overlord");
        app.handle_key(x, &mut real).unwrap();
        assert!(matches!(app.mode, Mode::Normal));
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
            right.contains(" unarchive ") && !right.contains(" resume "),
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
