//! The code-folder form: where the user keeps repos (`[session] code_dir`).
//! The repo finder scans it and new repos go there.
//!
//! Describe, then confirm: an optional description for the overlord, and
//! the folder and what the finder scans as hints with rlord's suggestion
//! filled in. Saving with no description (or after the overlord reopened
//! the form, `confirm_setup`) saves by plain code (`setup.apply`); a
//! description goes to the overlord (`overlord.setup_request`), which
//! reopens the form with its proposal.
//!
//! Opened on first run (after the overlord's own dialog), by `:setup`, and
//! by the overlord.

use super::*;
use ratatui::widgets::Clear;
use rlord_core::config::{contract_tilde, expand_tilde};

/// What the repo finder scans, as the form will save it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Roots {
    /// Only the code folder (`repo_roots` removed).
    Follow,
    /// The user's configured roots, as they are.
    Keep(Vec<String>),
    /// These (from the overlord).
    Set(Vec<String>),
}

/// A suggested folder: its path (with `~`) and what it rests on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Choice {
    path: String,
    projects: usize,
    of: usize,
    from_roots: bool,
    prompts: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Describe,
    Folder,
    Roots,
    Save,
}

pub(crate) struct SetupForm {
    /// The describe field. None: no overlord exists, the row is hidden.
    describe: Option<String>,
    /// The folder, shown with `~`.
    code_dir: String,
    /// The user edited or picked it (or the overlord set it).
    code_dir_chosen: bool,
    /// The suggestion, then alternatives.
    choices: Vec<Choice>,
    /// (source, projects) behind the suggestion.
    sources: Vec<(String, usize)>,
    roots: Roots,
    /// `repo_roots` as configured.
    configured: Vec<String>,
    /// The Roots row is shown: explicit roots other than the code folder,
    /// or roots the overlord set.
    roots_row: bool,
    /// From `confirm_setup`: the hints it changed, and why.
    changed: Vec<String>,
    note: Option<String>,
    /// Reopened by the overlord, description untouched since: saving
    /// saves what is shown.
    confirmed: bool,
    /// Opened by the first run: Esc is remembered (`setup.apply skip`).
    pub(crate) auto: bool,
    /// Open the overlord's overlay when this closes (first run).
    pub(crate) then_overlord: bool,
    row: usize,
    error: Option<String>,
    /// rlord's suggestion is still being worked out (off the UI thread;
    /// `fill` lands it).
    pub(crate) loading: bool,
    /// The user moved or typed since it opened: a late suggestion keeps
    /// their place.
    touched: bool,
    /// The folder the user was told doesn't exist; saving it again
    /// creates it.
    confirm_create: Option<String>,
    /// Which folders macOS would ask about (built once).
    guard: rlord_core::privacy::Guard,
    /// The line under the folder (`folder_hint`), worked out once per
    /// edit, not per frame (it may look at the disk).
    hint: String,
}

/// What Save does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SaveCall {
    /// `setup.apply` with these params.
    Apply(Value),
    /// `overlord.setup_request` with these params.
    Request(Value),
    /// The folder doesn't exist: ask before creating it.
    AskCreate(String),
}

impl SetupForm {
    /// The form for a `setup.suggest` answer. `overlord`: one exists (the
    /// describe row is shown).
    pub(crate) fn from_suggest(sugg: &Value, overlord: bool, auto: bool) -> SetupForm {
        let choice = |c: &Value| Choice {
            path: contract_tilde(c["path"].as_str().unwrap_or("")),
            projects: c["projects"].as_u64().unwrap_or(0) as usize,
            of: c["of"].as_u64().unwrap_or(0) as usize,
            from_roots: c["from"] == "repo_roots",
            prompts: c["prompts"] == true,
        };
        let choices: Vec<Choice> = std::iter::once(&sugg["suggestion"])
            .filter(|s| s.is_object())
            .chain(sugg["alternatives"].as_array().into_iter().flatten())
            .map(choice)
            .filter(|c| !c.path.is_empty())
            .collect();
        let sources = sugg["sources"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0) as usize))
                    .collect()
            })
            .unwrap_or_default();
        let configured: Vec<String> = sugg["roots"]["configured"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let current = sugg["code_dir"].as_str().unwrap_or("");
        let (code_dir, chosen) = if current.is_empty() {
            (
                choices.first().map(|c| c.path.clone()).unwrap_or_default(),
                false,
            )
        } else {
            (contract_tilde(current), true)
        };
        let roots = if configured.is_empty() || configured == ["~"] {
            Roots::Follow
        } else {
            Roots::Keep(configured.clone())
        };
        let mut f = SetupForm {
            describe: overlord.then(String::new),
            code_dir,
            code_dir_chosen: chosen,
            choices,
            sources,
            roots,
            configured,
            roots_row: false,
            changed: Vec::new(),
            note: None,
            confirmed: false,
            auto,
            then_overlord: false,
            row: 0,
            error: None,
            loading: false,
            touched: false,
            confirm_create: None,
            guard: rlord_core::privacy::Guard::new(&[]),
            hint: String::new(),
        };
        f.roots_row = f.roots_differ();
        // With a folder filled in, Enter saves it; with none, type one.
        let start = if f.code_dir.is_empty() {
            Row::Folder
        } else {
            Row::Save
        };
        f.row = f.rows().iter().position(|r| *r == start).unwrap_or(0);
        f.refresh_hint();
        f
    }

    /// Work out the line under the folder again (after an edit).
    pub(crate) fn refresh_hint(&mut self) {
        self.hint = folder_hint(self);
    }

    /// A form waiting on rlord's suggestion ("finding…").
    pub(crate) fn loading(overlord: bool, auto: bool) -> SetupForm {
        let mut f = SetupForm::from_suggest(&Value::Null, overlord, auto);
        f.loading = true;
        f.refresh_hint();
        f
    }

    /// The suggestion arrived (Null: none could be made). What the user
    /// already typed or picked stays.
    pub(crate) fn fill(&mut self, sugg: &Value) {
        let at = self.current();
        let mut new = SetupForm::from_suggest(sugg, self.describe.is_some(), self.auto);
        new.describe = self.describe.take();
        new.then_overlord = self.then_overlord;
        if self.code_dir_chosen {
            new.code_dir = std::mem::take(&mut self.code_dir);
            new.code_dir_chosen = true;
            new.roots_row = new.roots_differ();
        }
        if self.touched {
            new.row = new.rows().iter().position(|r| *r == at).unwrap_or(0);
            new.touched = true;
        }
        new.refresh_hint();
        *self = new;
    }

    /// The configured roots are explicit and aren't just the code folder.
    fn roots_differ(&self) -> bool {
        !self.configured.is_empty()
            && self
                .configured
                .iter()
                .map(|r| expand_tilde(r))
                .collect::<Vec<_>>()
                != [expand_tilde(&self.code_dir)]
    }

    /// Reopened by the overlord (`confirm_setup`): its proposal, the
    /// user's description, what it changed and why.
    pub(crate) fn prefill(&mut self, p: &Value) {
        if let Some(d) = p["code_dir"].as_str().filter(|d| !d.is_empty()) {
            self.code_dir = contract_tilde(d);
            self.code_dir_chosen = true;
        }
        if let Some(r) = p["repo_roots"].as_array() {
            let r: Vec<String> = r
                .iter()
                .filter_map(Value::as_str)
                .map(contract_tilde)
                .collect();
            if !r.is_empty() {
                self.roots = Roots::Set(r);
                self.roots_row = true;
            }
        } else {
            self.roots_row = self.roots_differ();
        }
        if let Some(d) = p["description"].as_str() {
            self.describe = Some(d.to_string());
        }
        self.changed = p["changed"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        self.note = p["note"]
            .as_str()
            .filter(|n| !n.is_empty())
            .map(str::to_string);
        self.refresh_hint();
        self.confirmed = true;
        self.error = None;
        self.row = self
            .rows()
            .iter()
            .position(|r| *r == Row::Save)
            .unwrap_or(0);
    }

    fn rows(&self) -> Vec<Row> {
        let mut v = Vec::new();
        if self.describe.is_some() {
            v.push(Row::Describe);
        }
        v.push(Row::Folder);
        if self.roots_row {
            v.push(Row::Roots);
        }
        v.push(Row::Save);
        v
    }

    fn current(&self) -> Row {
        self.rows()[self.row.min(self.rows().len() - 1)]
    }

    /// The description changed after the overlord reopened the form: it
    /// is a new request again.
    fn description_changed(&mut self) {
        self.confirmed = false;
        self.changed.clear();
        self.note = None;
    }

    fn describing(&self) -> bool {
        self.describe
            .as_deref()
            .is_some_and(|d| !d.trim().is_empty())
    }

    /// ← / → on the folder row: the next or previous suggestion.
    fn cycle(&mut self, forward: bool) {
        let n = self.choices.len();
        if n == 0 {
            return;
        }
        let at = self.choices.iter().position(|c| c.path == self.code_dir);
        let to = match (at, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        };
        self.code_dir = self.choices[to].path.clone();
        self.code_dir_chosen = true;
        self.refresh_hint();
        if !self.roots_row {
            self.roots_row = self.roots_differ();
        }
    }

    /// ← / → on the roots row: the code folder or the configured roots.
    fn toggle_roots(&mut self) {
        self.roots = match &self.roots {
            Roots::Follow if self.configured.is_empty() => Roots::Follow,
            Roots::Follow => Roots::Keep(self.configured.clone()),
            _ => Roots::Follow,
        };
    }

    /// The suggestion the folder field shows, if it is one.
    fn shown_choice(&self) -> Option<&Choice> {
        self.choices.iter().find(|c| c.path == self.code_dir)
    }

    /// What Save does, or why it can't. `exists`: whether a folder is
    /// there (None: can't tell without risking a macOS prompt).
    pub(crate) fn save_call(
        &self,
        exists: &dyn Fn(&str) -> Option<bool>,
    ) -> std::result::Result<SaveCall, String> {
        let dir = self.code_dir.trim();
        if self.describing() && !self.confirmed {
            let source = if dir.is_empty() {
                "not set"
            } else if self.code_dir_chosen {
                "picked by the user"
            } else {
                "suggested by rlord"
            };
            let (roots, roots_source) = match &self.roots {
                Roots::Follow => (json!("follow"), "user"),
                Roots::Keep(r) => (json!(r), "config"),
                Roots::Set(r) => (json!(r), "user"),
            };
            return Ok(SaveCall::Request(json!({
                "description": self.describe.as_deref().unwrap_or("").trim(),
                "code_dir": if dir.is_empty() { String::new() } else { expand_tilde(dir) },
                "code_dir_source": source,
                "roots": roots,
                "roots_source": roots_source,
            })));
        }
        if dir.is_empty() {
            return Err(if self.loading {
                "still finding a suggestion: type a folder, or wait a moment".into()
            } else {
                "type the folder you keep your repos in".into()
            });
        }
        let path = expand_tilde(dir);
        // A new folder is created only once the user said so (as the CLI
        // asks).
        let create = exists(&path) == Some(false);
        if create && self.confirm_create.as_deref() != Some(path.as_str()) {
            return Ok(SaveCall::AskCreate(path));
        }
        let roots = match &self.roots {
            Roots::Follow => json!("follow"),
            Roots::Keep(_) => json!("leave"),
            Roots::Set(r) => json!(r),
        };
        Ok(SaveCall::Apply(json!({
            "code_dir": path,
            "roots": roots,
            "create": create,
        })))
    }

    /// A click on row `i`: select it. True when it already was.
    pub(crate) fn click_row(&mut self, i: usize) -> bool {
        if self.row == i {
            return true;
        }
        self.row = i.min(self.rows().len() - 1);
        false
    }

    /// The Save row is focused (a click on it saves).
    pub(crate) fn on_save(&self) -> bool {
        self.current() == Row::Save
    }
}

impl App {
    /// First run: open the code-folder form when setup is needed. True
    /// when it opened.
    pub(crate) fn check_setup(&mut self) -> bool {
        // (A cheap look-up; the suggestion is worked out only if needed,
        // and off this thread.)
        let needed = self
            .call("setup.status", Value::Null)
            .is_some_and(|st| st["needed"] == true);
        needed && self.open_setup(true)
    }

    /// Open the code-folder form now, showing "finding…" while rlord's
    /// suggestion is worked out on another thread (it reads the agents'
    /// files). True when it opened.
    pub(crate) fn open_setup(&mut self, auto: bool) -> bool {
        let Some(o) = self.call("overlord.get", Value::Null) else {
            return false;
        };
        let launch = std::env::current_dir()
            .ok()
            .and_then(|d| rlord_core::git::main_repo_root(&d.to_string_lossy()));
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let v = client_call(
                &paths::socket_path(),
                "setup.suggest",
                json!({"launch_dir": launch}),
            )
            .unwrap_or(Value::Null);
            let _ = tx.send(v);
        });
        self.setup_rx = Some(rx);
        self.mode = Mode::Setup(Box::new(SetupForm::loading(!o.is_null(), auto)));
        true
    }

    /// The suggestion for an open form, if it has arrived (the event loop
    /// asks on each pass). True when the screen changed.
    pub(crate) fn poll_setup(&mut self) -> bool {
        let Some(rx) = &self.setup_rx else {
            return false;
        };
        let v = match rx.try_recv() {
            Ok(v) => v,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Value::Null,
        };
        self.setup_rx = None;
        match &mut self.mode {
            Mode::Setup(f) if f.loading => {
                f.fill(&v);
                true
            }
            _ => false,
        }
    }

    /// The overlord's `confirm_setup`: the form, reopened with its proposal.
    /// (It replaces any proposal parked for `:setup`.)
    pub(crate) fn open_setup_from(&mut self, cmd: &Value) {
        self.pending_setup = None;
        // Replacing the first-run form: it stays the first-run form (Esc,
        // then the overlay, as before).
        let (auto, then_overlord) = match &self.mode {
            Mode::Setup(f) => (f.auto, f.then_overlord),
            _ => (false, false),
        };
        let sugg = &cmd["suggest"];
        let mut f = SetupForm::from_suggest(sugg, true, auto);
        f.then_overlord = then_overlord;
        f.prefill(&cmd["prefill"]);
        self.mode = Mode::Setup(Box::new(f));
    }

    /// The form closed (saved, sent or put off).
    fn close_setup(&mut self, f: &SetupForm) {
        if f.then_overlord {
            self.show_overlord();
        }
    }

    pub(crate) fn handle_setup_form(&mut self, k: KeyEvent, mut f: Box<SetupForm>) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let row = f.current();
        let n = f.rows().len();
        if ctrl && k.code == KeyCode::Char('s') {
            return self.save_setup_form(f);
        }
        // Don't ask again (first run): recorded, so the form stops
        // opening; with no code folder the finder then lists only repos
        // already used (unless `repo_roots` is set).
        if ctrl && k.code == KeyCode::Char('d') && f.auto {
            let _ = self.call("setup.apply", json!({"skip": true}));
            self.status_line =
                "won't ask again · the finder lists only repos you've used · :setup any time"
                    .into();
            self.close_setup(&f);
            return;
        }
        if !matches!(k.code, KeyCode::Esc | KeyCode::Enter) {
            f.touched = true;
        }
        match k.code {
            // Later: nothing is recorded, so it asks again next start.
            KeyCode::Esc => {
                if f.auto {
                    self.status_line =
                        "no code folder yet · asks again next start · :setup any time".into();
                }
                self.close_setup(&f);
                return;
            }
            KeyCode::Tab | KeyCode::Down => f.row = (f.row + 1) % n,
            KeyCode::BackTab | KeyCode::Up => f.row = (f.row + n - 1) % n,
            KeyCode::Enter if row == Row::Describe => {
                if let Some(d) = &mut f.describe {
                    d.push('\n');
                }
                f.description_changed();
            }
            KeyCode::Enter => return self.save_setup_form(f),
            KeyCode::Left | KeyCode::Right if row == Row::Folder => {
                f.cycle(k.code == KeyCode::Right);
                f.error = None;
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if row == Row::Roots => {
                f.toggle_roots();
            }
            KeyCode::Backspace => match row {
                Row::Describe => {
                    if let Some(d) = &mut f.describe {
                        d.pop();
                    }
                    f.description_changed();
                }
                Row::Folder => {
                    f.code_dir.pop();
                    f.code_dir_chosen = true;
                    f.error = None;
                    f.refresh_hint();
                }
                _ => {}
            },
            KeyCode::Char(c) if !ctrl => match row {
                Row::Describe => {
                    if let Some(d) = &mut f.describe {
                        d.push(c);
                    }
                    f.description_changed();
                }
                Row::Folder => {
                    f.code_dir.push(c);
                    f.code_dir_chosen = true;
                    f.error = None;
                    if !f.roots_row {
                        f.roots_row = f.roots_differ();
                    }
                    f.refresh_hint();
                }
                _ => {}
            },
            _ => {}
        }
        self.mode = Mode::Setup(f);
    }

    fn save_setup_form(&mut self, mut f: Box<SetupForm>) {
        let guard = f.guard.clone();
        let exists = |p: &str| {
            let p = std::path::Path::new(p);
            guard.prompts(p).is_none().then(|| p.is_dir())
        };
        match f.save_call(&exists) {
            Ok(SaveCall::AskCreate(path)) => {
                f.error = Some(format!(
                    "{} doesn't exist: enter again to create it",
                    contract_tilde(&path)
                ));
                f.confirm_create = Some(path);
                f.row = f.rows().iter().position(|r| *r == Row::Save).unwrap_or(0);
                self.mode = Mode::Setup(f);
            }
            Err(e) => {
                f.error = Some(e);
                f.row = f.rows().iter().position(|r| *r == Row::Folder).unwrap_or(0);
                self.mode = Mode::Setup(f);
            }
            Ok(SaveCall::Apply(p)) => match client_call(&paths::socket_path(), "setup.apply", p) {
                Ok(r) => {
                    if let Ok(c) = Config::load() {
                        self.config = c;
                    }
                    let roots: Vec<String> = r["repo_roots"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(contract_tilde)
                                .collect()
                        })
                        .unwrap_or_default();
                    let dir = contract_tilde(r["code_dir"].as_str().unwrap_or(""));
                    // Saved: a proposal parked for `:setup` is stale now.
                    self.pending_setup = None;
                    self.status_line = if roots == [dir.clone()] {
                        format!("code folder: {dir} · the repo finder scans it")
                    } else {
                        format!(
                            "code folder: {dir} · the repo finder scans {}",
                            roots.join(", ")
                        )
                    };
                    self.close_setup(&f);
                }
                Err(e) => {
                    f.error = Some(format!("{e:#}"));
                    self.mode = Mode::Setup(f);
                }
            },
            Ok(SaveCall::Request(p)) => {
                let exists = self
                    .call("overlord.status", Value::Null)
                    .is_some_and(|st| st["exists"] == true);
                if !exists {
                    f.error =
                        Some("no overlord yet: clear the description to save as shown".into());
                    self.mode = Mode::Setup(f);
                    return;
                }
                // In the background: a stopped overlord can take a while.
                std::thread::spawn(move || {
                    let _ = client_call(&paths::socket_path(), "overlord.setup_request", p);
                });
                // Its answer reopens the form; nothing parked is current.
                self.pending_setup = None;
                self.status_line = "sent to the overlord".into();
                self.show_overlord();
            }
        }
    }

    pub(crate) fn draw_setup_form(&self, f: &mut Frame, s: &SetupForm) {
        let accent = Style::default().fg(theme::accent());
        let muted = theme::muted();
        let width = f.area().width.min(84).saturating_sub(8).max(10) as usize;
        let mut lines: Vec<Line> = vec![Line::from("")];
        if let Some(n) = &s.note {
            lines.push(Line::from(vec![
                Span::styled("  ✦ ", accent.bold()),
                Span::styled(n.clone(), accent),
            ]));
            lines.push(Line::from(Span::styled(
                "    Check it, then save: rlord saves it as shown.",
                muted,
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "  The folder you keep your repos in. The repo finder scans it,",
                Style::default().fg(theme::text()),
            )));
            lines.push(Line::from(Span::styled(
                "  and new repos go there.",
                Style::default().fg(theme::text()),
            )));
        }
        lines.push(Line::from(""));
        let rows = s.rows();
        let mut starts = Vec::new();
        let label = |focused: bool, name: &str| {
            vec![
                Span::styled(if focused { "▌ " } else { "  " }, accent),
                Span::styled(
                    format!("{name:<19}"),
                    if focused {
                        Style::default().fg(theme::yellow()).bold()
                    } else {
                        Style::default().bold()
                    },
                ),
            ]
        };
        let marker = |key: &str| s.changed.iter().any(|c| c == key);
        for (i, row) in rows.iter().enumerate() {
            let focused = s.row == i;
            starts.push(lines.len());
            match row {
                Row::Describe => {
                    let d = s.describe.as_deref().unwrap_or("");
                    let mut spans = label(focused, "Tell the overlord");
                    let mut body: Vec<String> = d
                        .lines()
                        .flat_map(|l| {
                            let chars: Vec<char> = l.chars().collect();
                            if chars.is_empty() {
                                return vec![String::new()];
                            }
                            chars
                                .chunks(width.saturating_sub(21).max(10))
                                .map(|c| c.iter().collect())
                                .collect::<Vec<String>>()
                        })
                        .collect();
                    if d.ends_with('\n') {
                        body.push(String::new());
                    }
                    if body.is_empty() {
                        spans.push(Span::styled(
                            if focused { "▏" } else { "" }.to_string(),
                            accent,
                        ));
                        spans.push(Span::styled(
                            "(optional) e.g. \"work in ~/ko, personal in ~/proj\"",
                            theme::faint(),
                        ));
                        lines.push(Line::from(spans));
                    } else {
                        let last = body.len() - 1;
                        for (j, b) in body.into_iter().enumerate() {
                            let mut sp = if j == 0 {
                                spans.clone()
                            } else {
                                vec![Span::raw(" ".repeat(21))]
                            };
                            let cursor = if focused && j == last { "▏" } else { "" };
                            sp.push(Span::raw(format!("{b}{cursor}")));
                            lines.push(Line::from(sp));
                        }
                    }
                }
                Row::Folder => {
                    let mut spans = label(focused, "Code folder");
                    let style = if marker("code_dir") {
                        accent
                    } else {
                        Style::default().fg(theme::text())
                    };
                    let shown = if focused && s.choices.len() > 1 {
                        format!("◀ {}▏ ▶", s.code_dir)
                    } else if focused {
                        format!("{}▏", s.code_dir)
                    } else {
                        s.code_dir.clone()
                    };
                    spans.push(Span::styled(shown, style));
                    if marker("code_dir") {
                        spans.push(Span::styled("  ✦ updated", accent));
                    }
                    lines.push(Line::from(spans));
                    let hint = s.hint.clone();
                    // (A long hint starts further left, to fit.)
                    let indent = if hint.chars().count() + 21 > width {
                        4
                    } else {
                        21
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{:indent$}{hint}", ""),
                        muted,
                    )));
                }
                Row::Roots => {
                    let mut spans = label(focused, "Repo finder scans");
                    let (value, was) = match &s.roots {
                        Roots::Follow => (
                            "the code folder".to_string(),
                            (!s.configured.is_empty())
                                .then(|| format!("  (was {})", s.configured.join(", "))),
                        ),
                        Roots::Keep(r) => (
                            format!("the code folder and {} (as configured)", r.join(", ")),
                            None,
                        ),
                        Roots::Set(r) => (format!("the code folder and {}", r.join(", ")), None),
                    };
                    let style = if marker("repo_roots") {
                        accent
                    } else {
                        Style::default().fg(theme::text())
                    };
                    spans.push(Span::styled(
                        if focused {
                            format!("◀ {value} ▶")
                        } else {
                            value
                        },
                        style,
                    ));
                    if let Some(w) = was {
                        spans.push(Span::styled(w, theme::faint()));
                    }
                    if marker("repo_roots") {
                        spans.push(Span::styled("  ✦ updated", accent));
                    }
                    lines.push(Line::from(spans));
                }
                Row::Save => {
                    lines.push(Line::from(""));
                    starts.pop();
                    starts.push(lines.len());
                    let text = if s.describing() && !s.confirmed {
                        "[ Send to the overlord ]"
                    } else {
                        "[ Save ]"
                    };
                    lines.push(Line::from(vec![
                        Span::styled(if focused { "▌ " } else { "  " }, accent),
                        Span::styled(
                            text,
                            if focused {
                                accent.bold()
                            } else {
                                Style::default().bold()
                            },
                        ),
                    ]));
                }
            }
            lines.push(Line::from(""));
        }
        lines.push(keys_line(&[
            ("enter", "save", Cap::Context),
            ("tab", "move", Cap::Global),
            ("←/→", "other suggestions", Cap::Global),
            ("esc", "ask me next start", Cap::Global),
        ]));
        if s.auto {
            lines.push(keys_line(&[("ctrl-d", "don't ask again", Cap::Global)]));
            lines.push(Line::from(Span::styled(
                "    then the finder lists only repos you've used, unless repo_roots is set",
                theme::faint(),
            )));
        }
        if let Some(e) = &s.error {
            lines.push(Line::from(Span::styled(
                format!("  {e}"),
                Style::default().fg(theme::red()),
            )));
        }
        // As tall as what it shows.
        let area = newform::centered(f.area(), 84, lines.len() as u16 + 3);
        f.render_widget(Clear, area);
        self.modal_hits(f.area(), area, &lines);
        for (i, at) in starts.iter().enumerate() {
            self.hits
                .row(area, area.y + 1 + *at as u16, mouse::Hit::Pick(i));
        }
        f.render_widget(
            Paragraph::new(lines).block(theme::modal("◆ Where do you keep your code?")),
            area,
        );
    }
}

/// The line under the folder: what the suggestion rests on, or what
/// saving will do.
fn folder_hint(s: &SetupForm) -> String {
    let dir = s.code_dir.trim();
    if dir.is_empty() && s.loading {
        return "finding where your projects are…".into();
    }
    if dir.is_empty() {
        return "rlord found no projects yet: type your repos' folder (created if missing)".into();
    }
    if let Some(c) = s.shown_choice() {
        if c.prompts {
            return "macOS will ask permission for this folder once".into();
        }
        let mut h = rlord_core::code_dir::basis(c.projects, c.of, &s.sources);
        if c.from_roots {
            h.push_str(" · in your repo_roots");
        }
        return h;
    }
    let path = expand_tilde(dir);
    if !std::path::Path::new(&path).is_absolute() {
        return "a full path, like ~/code".into();
    }
    if s.guard.prompts(std::path::Path::new(&path)).is_some() {
        return "macOS will ask permission for this folder once".into();
    }
    if !std::path::Path::new(&path).is_dir() {
        return "will be created".into();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sugg(best: Option<&str>, configured: &[&str]) -> Value {
        json!({
            "code_dir": "",
            "suggestion": best.map(|b| json!({"path": b, "projects": 3, "of": 4, "from": "vote", "prompts": false})),
            "alternatives": [{"path": "/elsewhere/work", "projects": 1, "of": 4, "from": "vote", "prompts": false}],
            "sources": {"claude": 3, "rlord": 1},
            "roots": {"configured": configured, "effective": []},
        })
    }

    #[test]
    fn focus_starts_on_save_with_a_suggestion() {
        let f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &[]), true, true);
        assert_eq!(f.current(), Row::Save);
        assert_eq!(f.rows(), [Row::Describe, Row::Folder, Row::Save]);
        assert_eq!(f.code_dir, "/elsewhere/ko");
        // No suggestion, no overlord: the folder field, no describe row.
        let mut s = sugg(None, &[]);
        s["alternatives"] = json!([]);
        let f = SetupForm::from_suggest(&s, false, true);
        assert_eq!(f.current(), Row::Folder);
        assert_eq!(f.rows(), [Row::Folder, Row::Save]);
        assert!(f.hint.starts_with("rlord found no projects yet"));
        assert!(f.save_call(&|_| Some(true)).is_err());
    }

    #[test]
    fn arrows_cycle_choices_and_roots_row_shows_when_it_should() {
        let mut f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &["~"]), true, true);
        // `["~"]`: the row shows, preselecting the code folder.
        assert!(f.roots_row && f.roots == Roots::Follow);
        f.row = 1;
        f.cycle(true);
        assert_eq!(
            (f.code_dir.as_str(), f.code_dir_chosen),
            ("/elsewhere/work", true)
        );
        f.cycle(true);
        assert_eq!(f.code_dir, "/elsewhere/ko");
        // Other explicit roots: kept by default.
        let f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &["/x"]), true, true);
        assert_eq!(f.roots, Roots::Keep(vec!["/x".into()]));
        // Roots that are the code folder: no row.
        let f =
            SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &["/elsewhere/ko"]), true, true);
        assert!(!f.roots_row);
    }

    #[test]
    fn prefill_from_the_overlord_and_editing_clears_confirmed() {
        let mut f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &[]), true, false);
        f.prefill(
            &json!({"code_dir": "/elsewhere/ko", "repo_roots": ["/elsewhere/ko", "/p"],
                          "description": "both", "changed": ["repo_roots"], "note": "Both"}),
        );
        assert!(f.confirmed && f.roots_row);
        assert_eq!(f.note.as_deref(), Some("Both"));
        assert_eq!(f.changed, ["repo_roots"]);
        assert_eq!(f.current(), Row::Save);
        // Confirmed: Save saves what is shown.
        assert_eq!(
            f.save_call(&|_| Some(true)).unwrap(),
            SaveCall::Apply(json!({"code_dir": "/elsewhere/ko",
                                   "roots": ["/elsewhere/ko", "/p"], "create": false}))
        );
        f.description_changed();
        assert!(!f.confirmed && f.changed.is_empty());
        assert!(matches!(
            f.save_call(&|_| Some(true)).unwrap(),
            SaveCall::Request(_)
        ));
    }

    #[test]
    fn save_applies_or_asks_the_overlord() {
        let mut f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &["~"]), true, true);
        assert_eq!(
            f.save_call(&|_| Some(true)).unwrap(),
            SaveCall::Apply(
                json!({"code_dir": "/elsewhere/ko", "roots": "follow", "create": false})
            )
        );
        // A folder that isn't there: asked first, created on the second save.
        assert_eq!(
            f.save_call(&|_| Some(false)).unwrap(),
            SaveCall::AskCreate("/elsewhere/ko".into())
        );
        f.confirm_create = Some("/elsewhere/ko".into());
        assert_eq!(
            f.save_call(&|_| Some(false)).unwrap(),
            SaveCall::Apply(
                json!({"code_dir": "/elsewhere/ko", "roots": "follow", "create": true})
            )
        );
        // Another folder typed since: asked again.
        f.code_dir = "/elsewhere/kode".into();
        assert!(matches!(
            f.save_call(&|_| Some(false)).unwrap(),
            SaveCall::AskCreate(_)
        ));
        // Can't tell (macOS would ask): saved as given, not created.
        assert_eq!(
            f.save_call(&|_| None).unwrap(),
            SaveCall::Apply(
                json!({"code_dir": "/elsewhere/kode", "roots": "follow", "create": false})
            )
        );
        f.code_dir = "/elsewhere/ko".into();
        f.describe = Some("mine are in ~/proj".into());
        let SaveCall::Request(p) = f.save_call(&|_| Some(true)).unwrap() else {
            panic!("not a request")
        };
        assert_eq!(p["code_dir_source"], "suggested by rlord");
        assert_eq!(p["roots"], "follow");
        assert_eq!(p["description"], "mine are in ~/proj");
    }

    /// The form opens at once; the suggestion lands later, keeping what
    /// the user typed meanwhile.
    #[test]
    fn a_late_suggestion_fills_the_form() {
        let mut f = SetupForm::loading(true, true);
        assert!(f.loading && f.code_dir.is_empty());
        assert_eq!(f.hint, "finding where your projects are…");
        assert!(f
            .save_call(&|_| Some(true))
            .unwrap_err()
            .contains("still finding"));
        f.fill(&sugg(Some("/elsewhere/ko"), &[]));
        assert!(!f.loading);
        assert_eq!(
            (f.code_dir.as_str(), f.current()),
            ("/elsewhere/ko", Row::Save)
        );
        // Typed before it arrived: kept, and the focus stays.
        let mut f = SetupForm::loading(true, true);
        f.row = 1;
        f.touched = true;
        f.code_dir = "/mine".into();
        f.code_dir_chosen = true;
        f.describe = Some("d".into());
        f.fill(&sugg(Some("/elsewhere/ko"), &[]));
        assert_eq!(f.code_dir, "/mine");
        assert_eq!(f.current(), Row::Folder);
        assert_eq!(f.describe.as_deref(), Some("d"));
        assert_eq!(f.choices.len(), 2);
        // No answer: an empty form, no longer loading.
        let mut f = SetupForm::loading(false, true);
        f.fill(&Value::Null);
        assert!(!f.loading);
        assert!(f.hint.starts_with("rlord found no projects yet"));
    }

    /// The hint is worked out on edits, not on every frame.
    #[test]
    fn the_hint_follows_edits() {
        let mut f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &[]), false, true);
        assert!(f.hint.starts_with("3 of 4 projects"), "{}", f.hint);
        f.cycle(true);
        assert!(f.hint.starts_with("1 of 4 projects"), "{}", f.hint);
        f.code_dir = "rel".into();
        f.refresh_hint();
        assert_eq!(f.hint, "a full path, like ~/code");
    }

    /// The overlord replacing the first-run form keeps it the first-run
    /// form.
    #[test]
    fn a_proposal_over_the_first_run_form_keeps_its_place() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        let mut first = SetupForm::from_suggest(&sugg(None, &[]), true, true);
        first.then_overlord = true;
        app.mode = Mode::Setup(Box::new(first));
        app.open_setup_from(&json!({"cmd": "setup", "prefill": {"code_dir": "/new"},
                                    "suggest": sugg(None, &[])}));
        let Mode::Setup(f) = &app.mode else {
            panic!("form not open")
        };
        assert!(f.auto && f.then_overlord);
    }

    /// A proposal parked for `:setup` is dropped once a newer one opens.
    #[test]
    fn a_newer_proposal_clears_the_parked_one() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.pending_setup = Some(json!({"cmd": "setup", "prefill": {"code_dir": "/old"}}));
        app.open_setup_from(&json!({"cmd": "setup", "prefill": {"code_dir": "/new"},
                                    "suggest": sugg(None, &[])}));
        assert!(app.pending_setup.is_none());
        let Mode::Setup(f) = &app.mode else {
            panic!("form not open")
        };
        assert_eq!(f.code_dir, "/new");
    }

    #[test]
    fn draws_the_form() {
        use ratatui::backend::TestBackend;
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let app = App::new(rx, Config::default(), keys);
        let mut s = sugg(None, &[]);
        s["alternatives"] = json!([]);
        let f = SetupForm::from_suggest(&s, true, true);
        let mut term = ratatui::Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|fr| app.draw_setup_form(fr, &f)).unwrap();
        let buf = term.backend().buffer();
        let out: String = (0..30)
            .map(|y| {
                (0..100)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        for want in [
            "Where do you keep your code?",
            "Tell the overlord",
            "Code folder",
            "rlord found no projects yet",
        ] {
            assert!(out.contains(want), "{want:?} missing:\n{out}");
        }
    }
}
