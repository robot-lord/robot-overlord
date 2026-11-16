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
    /// The code folder (`repo_roots` removed).
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
}

/// What Save does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SaveCall {
    /// `setup.apply` with these params.
    Apply(Value),
    /// `overlord.setup_request` with these params.
    Request(Value),
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
        };
        f.roots_row = f.roots_differ();
        // With a folder filled in, Enter saves it; with none, type one.
        let start = if f.code_dir.is_empty() {
            Row::Folder
        } else {
            Row::Save
        };
        f.row = f.rows().iter().position(|r| *r == start).unwrap_or(0);
        f
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

    /// What Save does, or why it can't.
    pub(crate) fn save_call(&self) -> std::result::Result<SaveCall, String> {
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
            return Err("type the folder you keep your repos in".into());
        }
        let roots = match &self.roots {
            Roots::Follow => json!("follow"),
            Roots::Keep(_) => json!("leave"),
            Roots::Set(r) => json!(r),
        };
        Ok(SaveCall::Apply(json!({
            "code_dir": expand_tilde(dir),
            "roots": roots,
            "create": true,
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
        let needed = self
            .call("setup.status", Value::Null)
            .is_some_and(|st| st["needed"] == true);
        needed && self.open_setup(true)
    }

    /// Open the code-folder form, filled with rlord's suggestion. True when
    /// it opened.
    pub(crate) fn open_setup(&mut self, auto: bool) -> bool {
        let launch = std::env::current_dir()
            .ok()
            .and_then(|d| rlord_core::git::main_repo_root(&d.to_string_lossy()));
        let Some(sugg) = self.call("setup.suggest", json!({"launch_dir": launch})) else {
            return false;
        };
        let overlord = self
            .call("overlord.get", Value::Null)
            .is_some_and(|o| !o.is_null());
        self.mode = Mode::Setup(Box::new(SetupForm::from_suggest(&sugg, overlord, auto)));
        true
    }

    /// The overlord's `confirm_setup`: the form, reopened with its proposal.
    pub(crate) fn open_setup_from(&mut self, cmd: &Value) {
        let sugg = &cmd["suggest"];
        let mut f = SetupForm::from_suggest(sugg, true, false);
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
        match k.code {
            KeyCode::Esc => {
                if f.auto {
                    let _ = self.call("setup.apply", json!({"skip": true}));
                    self.status_line = "no code folder set · :setup any time".into();
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
                }
                _ => {}
            },
            _ => {}
        }
        self.mode = Mode::Setup(f);
    }

    fn save_setup_form(&mut self, mut f: Box<SetupForm>) {
        match f.save_call() {
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
                    let hint = folder_hint(s);
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
                        Roots::Keep(r) => (format!("{} (as configured)", r.join(", ")), None),
                        Roots::Set(r) => (r.join(", "), None),
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
            ("esc", "later (:setup)", Cap::Global),
        ]));
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
    if dir.is_empty() {
        return "rlord found no projects yet: type your repos' folder (created if missing)".into();
    }
    if let Some(c) = s.shown_choice() {
        if c.prompts {
            return "macOS will ask permission for this folder once".into();
        }
        let mut h = rlord_core::code_dir::basis(c.projects, c.of, &s.sources);
        if c.from_roots {
            h.push_str(" · your repo_roots");
        }
        return h;
    }
    let path = expand_tilde(dir);
    if !std::path::Path::new(&path).is_absolute() {
        return "a full path, like ~/code".into();
    }
    if rlord_core::privacy::Guard::new(&[])
        .prompts(std::path::Path::new(&path))
        .is_some()
    {
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
        assert!(folder_hint(&f).starts_with("rlord found no projects yet"));
        assert!(f.save_call().is_err());
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
            f.save_call().unwrap(),
            SaveCall::Apply(json!({"code_dir": "/elsewhere/ko",
                                   "roots": ["/elsewhere/ko", "/p"], "create": true}))
        );
        f.description_changed();
        assert!(!f.confirmed && f.changed.is_empty());
        assert!(matches!(f.save_call().unwrap(), SaveCall::Request(_)));
    }

    #[test]
    fn save_applies_or_asks_the_overlord() {
        let mut f = SetupForm::from_suggest(&sugg(Some("/elsewhere/ko"), &["~"]), true, true);
        assert_eq!(
            f.save_call().unwrap(),
            SaveCall::Apply(
                json!({"code_dir": "/elsewhere/ko", "roots": "follow", "create": true})
            )
        );
        f.describe = Some("mine are in ~/proj".into());
        let SaveCall::Request(p) = f.save_call().unwrap() else {
            panic!("not a request")
        };
        assert_eq!(p["code_dir_source"], "suggested by rlord");
        assert_eq!(p["roots"], "follow");
        assert_eq!(p["description"], "mine are in ~/proj");
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
