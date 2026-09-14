//! The new-session form: a centered modal. Field order puts the repo first
//! so the common path is `n`, glance at the default, `Enter` to launch — or
//! `j`/`↓` to drop into the repo list, pick one, `Enter`.

use super::*;
use ratatui::widgets::{Clear, List, ListItem, ListState};
use rlord_core::git::sanitize_branch;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    Repo,
    Name,
    Model,
    Workspace,
    Branch,
    Prompt,
    Launch,
}

const FIELDS: [Field; 7] = [
    Field::Repo,
    Field::Name,
    Field::Model,
    Field::Workspace,
    Field::Branch,
    Field::Prompt,
    Field::Launch,
];

impl Field {
    fn is_text(self) -> bool {
        matches!(self, Field::Name | Field::Branch | Field::Prompt)
    }
    fn label(self) -> &'static str {
        match self {
            Field::Repo => "Repo",
            Field::Name => "Name",
            Field::Model => "Model",
            Field::Workspace => "Workspace",
            Field::Branch => "Branch",
            Field::Prompt => "Prompt",
            Field::Launch => "",
        }
    }
}

#[derive(Clone)]
pub(crate) enum Location {
    Repo(String),
    Folder(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    Repo,
    Model,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextPrompt {
    Folder,
}

/// The repo finder: every known + discovered repo, filtered as you type.
struct Finder {
    query: String,
    all: Vec<Value>,
    roots: Vec<String>,
    state: ListState,
}

impl Finder {
    /// Case-insensitive match: every query word must appear in the path.
    fn matches(&self) -> Vec<&Value> {
        let words: Vec<String> = self
            .query
            .split_whitespace()
            .map(|w| w.to_lowercase())
            .collect();
        self.all
            .iter()
            .filter(|r| {
                let p = r["path"].as_str().unwrap_or("").to_lowercase();
                words.iter().all(|w| p.contains(w.as_str()))
            })
            .collect()
    }

    /// The typed text itself, when it names a git repo not in the list.
    fn typed_repo(&self) -> Option<String> {
        let t = rlord_core::config::expand_tilde(self.query.trim());
        if t.is_empty() || !std::path::Path::new(&t).is_dir() {
            return None;
        }
        let root = rlord_core::git::main_repo_root(&t)?;
        if self
            .all
            .iter()
            .any(|r| r["path"].as_str() == Some(root.as_str()))
        {
            return None;
        }
        Some(root)
    }

    /// Rows shown: the typed path (if a repo) first, then matches.
    fn rows(&self) -> Vec<(String, String)> {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut rows = Vec::new();
        if let Some(t) = self.typed_repo() {
            rows.push((
                t.clone(),
                format!(
                    "↳ {}  (typed path)",
                    shorten(&t.replacen(&home, "~", 1), 50)
                ),
            ));
        }
        for r in self.matches() {
            let p = r["path"].as_str().unwrap_or("?").to_string();
            let label = shorten(&p.replacen(&home, "~", 1), 52);
            let tag = if r["default"].as_bool().unwrap_or(false) {
                "  ★ default".to_string()
            } else if let Some(t) = r["last_used_at"].as_i64() {
                format!("  ({})", rel_age(Some(t)))
            } else {
                String::new()
            };
            rows.push((p, format!("{label}{tag}")));
        }
        rows
    }
}

pub(crate) struct NewForm {
    focus: usize,
    location: Location,
    name: String,
    model: String,
    workspace: bool,
    branch: String,
    branch_touched: bool,
    prompt: String,
    repos: Vec<Value>,
    models: Vec<String>,
    picker: Option<(PickerKind, Vec<String>, ListState)>,
    finder: Option<Finder>,
    text_prompt: Option<(TextPrompt, String)>,
    error: Option<String>,
}

impl NewForm {
    fn current(&self) -> Field {
        FIELDS[self.focus]
    }

    fn location_label(&self) -> String {
        let home = std::env::var("HOME").unwrap_or_default();
        match &self.location {
            Location::Repo(p) => shorten(&p.replacen(&home, "~", 1), 58),
            Location::Folder(p) => format!("{} (no repo)", shorten(&p.replacen(&home, "~", 1), 48)),
        }
    }
}

impl App {
    /// Open the form, seeded from the daemon's session defaults and repo list.
    pub(crate) fn open_new_form(&mut self) {
        let defaults = self
            .call("session.defaults", Value::Null)
            .unwrap_or(json!({}));
        let repos = self
            .call("repo.list", Value::Null)
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let location = match repos.first().and_then(|r| r["path"].as_str()) {
            Some(p) if repos[0]["default"].as_bool().unwrap_or(false) => {
                Location::Repo(p.to_string())
            }
            _ => match rlord_core::git::main_repo_root(&cwd) {
                Some(root) => Location::Repo(root),
                None => match repos.first().and_then(|r| r["path"].as_str()) {
                    Some(p) => Location::Repo(p.to_string()),
                    None => Location::Folder(cwd),
                },
            },
        };
        let mut models: Vec<String> = defaults["models"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let model = defaults["model"].as_str().unwrap_or("").to_string();
        if !model.is_empty() && !models.contains(&model) {
            models.insert(0, model.clone());
        }
        self.mode = Mode::NewSession(Box::new(NewForm {
            focus: 0,
            location,
            name: String::new(),
            model,
            workspace: defaults["workspace"].as_bool().unwrap_or(true),
            branch: String::new(),
            branch_touched: false,
            prompt: String::new(),
            repos,
            models,
            picker: None,
            finder: None,
            text_prompt: None,
            error: None,
        }));
    }

    /// Returns the form to keep it open, or None once it closed.
    pub(crate) fn handle_form(
        &mut self,
        k: ratatui::crossterm::event::KeyEvent,
        mut f: Box<NewForm>,
    ) -> Result<Option<Box<NewForm>>> {
        let code = k.code;
        // --- a text prompt (other repo path / folder) is open ---
        if let Some((kind, mut buf)) = f.text_prompt.take() {
            match code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let path = rlord_core::config::expand_tilde(buf.trim());
                    match kind {
                        TextPrompt::Folder => {
                            if std::path::Path::new(&path).is_dir() {
                                f.location = Location::Folder(path);
                                f.workspace = false;
                                f.error = None;
                            } else {
                                f.error = Some(format!("{path} is not a directory"));
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    buf.pop();
                    f.text_prompt = Some((kind, buf));
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    buf.push(c);
                    f.text_prompt = Some((kind, buf));
                }
                _ => f.text_prompt = Some((kind, buf)),
            }
            return Ok(Some(f));
        }
        // --- the repo finder is open ---
        if let Some(mut finder) = f.finder.take() {
            let n = finder.rows().len();
            let i = finder
                .state
                .selected()
                .unwrap_or(0)
                .min(n.saturating_sub(1));
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            match code {
                KeyCode::Esc => {}
                KeyCode::Down | KeyCode::Tab => {
                    finder
                        .state
                        .select(Some(if n == 0 { 0 } else { (i + 1) % n }));
                    f.finder = Some(finder);
                }
                KeyCode::Char('n') if ctrl => {
                    finder
                        .state
                        .select(Some(if n == 0 { 0 } else { (i + 1) % n }));
                    f.finder = Some(finder);
                }
                KeyCode::Up | KeyCode::BackTab => {
                    finder
                        .state
                        .select(Some(if n == 0 { 0 } else { (i + n - 1) % n }));
                    f.finder = Some(finder);
                }
                KeyCode::Char('p') if ctrl => {
                    finder
                        .state
                        .select(Some(if n == 0 { 0 } else { (i + n - 1) % n }));
                    f.finder = Some(finder);
                }
                KeyCode::Enter => {
                    if let Some((path, _)) = finder.rows().get(i) {
                        f.location = Location::Repo(path.clone());
                        f.error = None;
                    } else {
                        f.finder = Some(finder); // nothing to pick; stay
                    }
                }
                KeyCode::Backspace => {
                    finder.query.pop();
                    finder.state.select(Some(0));
                    f.finder = Some(finder);
                }
                KeyCode::Char(c) if !ctrl => {
                    finder.query.push(c);
                    finder.state.select(Some(0));
                    f.finder = Some(finder);
                }
                _ => f.finder = Some(finder),
            }
            return Ok(Some(f));
        }
        // --- a picker list is open ---
        if let Some((kind, items, mut state)) = f.picker.take() {
            let n = items.len();
            let i = state.selected().unwrap_or(0);
            match code {
                KeyCode::Esc => {}
                KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => {
                    state.select(Some((i + 1) % n.max(1)));
                    f.picker = Some((kind, items, state));
                }
                KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => {
                    state.select(Some((i + n.max(1) - 1) % n.max(1)));
                    f.picker = Some((kind, items, state));
                }
                KeyCode::Enter => match kind {
                    PickerKind::Model => {
                        f.model = if items[i] == "(agent default)" {
                            String::new()
                        } else {
                            items[i].clone()
                        };
                    }
                    PickerKind::Repo => {
                        if items[i] == OTHER_REPO {
                            self.open_finder(&mut f);
                        } else if items[i] == NO_REPO {
                            f.text_prompt = Some((TextPrompt::Folder, String::new()));
                        } else {
                            let path = f.repos[i]["path"].as_str().unwrap_or("").to_string();
                            f.location = Location::Repo(path);
                            f.error = None;
                        }
                    }
                },
                _ => f.picker = Some((kind, items, state)),
            }
            return Ok(Some(f));
        }
        // --- form navigation / editing ---
        let field = f.current();
        let n = FIELDS.len();
        let next = |f: &mut NewForm| f.focus = (f.focus + 1) % n;
        let prev = |f: &mut NewForm| f.focus = (f.focus + n - 1) % n;
        match code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Tab => next(&mut f),
            KeyCode::BackTab => prev(&mut f),
            // On the repo field, "down" means "show me the list".
            KeyCode::Down | KeyCode::Char('j') if field == Field::Repo => {
                self.open_picker(&mut f, PickerKind::Repo)
            }
            KeyCode::Down => next(&mut f),
            KeyCode::Up => prev(&mut f),
            KeyCode::Char('j') if !field.is_text() => next(&mut f),
            KeyCode::Char('k') if !field.is_text() => prev(&mut f),
            KeyCode::Enter => match field {
                Field::Repo => self.open_picker(&mut f, PickerKind::Repo),
                Field::Model => self.open_picker(&mut f, PickerKind::Model),
                Field::Workspace => f.workspace = !f.workspace,
                Field::Launch => return self.submit_form(f),
                _ => next(&mut f),
            },
            KeyCode::Char(' ') if field == Field::Workspace => f.workspace = !f.workspace,
            KeyCode::Char('l') if field == Field::Launch => return self.submit_form(f),
            KeyCode::Backspace if field.is_text() => match field {
                Field::Name => {
                    f.name.pop();
                    if !f.branch_touched {
                        f.branch = sanitize_branch(&f.name);
                    }
                }
                Field::Branch => {
                    f.branch.pop();
                    f.branch_touched = true;
                }
                Field::Prompt => {
                    f.prompt.pop();
                }
                _ => {}
            },
            KeyCode::Char(c) if field.is_text() && !k.modifiers.contains(KeyModifiers::CONTROL) => {
                match field {
                    Field::Name => {
                        f.name.push(c);
                        if !f.branch_touched {
                            f.branch = sanitize_branch(&f.name);
                        }
                    }
                    Field::Branch => {
                        f.branch.push(c);
                        f.branch_touched = true;
                    }
                    Field::Prompt => f.prompt.push(c),
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(Some(f))
    }

    fn open_picker(&mut self, f: &mut NewForm, kind: PickerKind) {
        let items: Vec<String> = match kind {
            PickerKind::Repo => {
                let home = std::env::var("HOME").unwrap_or_default();
                let mut v: Vec<String> = f
                    .repos
                    .iter()
                    .map(|r| {
                        let p = shorten(
                            &r["path"].as_str().unwrap_or("?").replacen(&home, "~", 1),
                            56,
                        );
                        let age = rel_age(r["last_used_at"].as_i64());
                        if r["default"].as_bool().unwrap_or(false) {
                            format!("★ {p}  (default)")
                        } else {
                            format!("  {p}  ({age})")
                        }
                    })
                    .collect();
                v.push(OTHER_REPO.to_string());
                v.push(NO_REPO.to_string());
                v
            }
            PickerKind::Model => {
                let mut v = f.models.clone();
                v.push("(agent default)".to_string());
                v
            }
        };
        // Preselect the current value.
        let sel = match kind {
            PickerKind::Repo => match &f.location {
                Location::Repo(p) => f
                    .repos
                    .iter()
                    .position(|r| r["path"].as_str() == Some(p))
                    .unwrap_or(0),
                Location::Folder(_) => items.len() - 1,
            },
            PickerKind::Model => f.models.iter().position(|m| *m == f.model).unwrap_or(0),
        };
        f.picker = Some((kind, items, ListState::default().with_selected(Some(sel))));
    }

    fn open_finder(&mut self, f: &mut NewForm) {
        let v = self.call("repo.find", Value::Null).unwrap_or(json!({}));
        f.finder = Some(Finder {
            query: String::new(),
            all: v["repos"].as_array().cloned().unwrap_or_default(),
            roots: v["roots"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            state: ListState::default().with_selected(Some(0)),
        });
    }

    fn submit_form(&mut self, mut f: Box<NewForm>) -> Result<Option<Box<NewForm>>> {
        let mut params = json!({
            "name": if f.name.trim().is_empty() { None } else { Some(f.name.trim()) },
            "prompt": if f.prompt.trim().is_empty() { None } else { Some(f.prompt.trim()) },
            "model": f.model,
            "workspace": f.workspace,
            "branch": if f.branch.trim().is_empty() { None } else { Some(f.branch.trim()) },
        });
        match &f.location {
            Location::Repo(p) => params["repo"] = json!(p),
            Location::Folder(p) => {
                params["project"] = json!(p);
                params["workspace"] = json!(false);
            }
        }
        match client_call(&paths::socket_path(), "session.new", params) {
            Ok(v) => {
                self.status_line = match v["workspace_path"].as_str() {
                    Some(ws) => format!(
                        "launched {} in {}",
                        v["name"].as_str().unwrap_or("?"),
                        ws.replacen(&std::env::var("HOME").unwrap_or_default(), "~", 1)
                    ),
                    None => format!("launched {}", v["name"].as_str().unwrap_or("?")),
                };
                self.dirty = true;
                self.table.select(Some(0));
                Ok(None)
            }
            Err(e) => {
                f.error = Some(format!("{e:#}"));
                Ok(Some(f))
            }
        }
    }

    pub(crate) fn draw_form(&self, f: &mut Frame, form: &NewForm) {
        let area = centered(f.area(), 74, 16);
        f.render_widget(Clear, area);
        let block = Block::default().borders(Borders::ALL).title(
            " new session  (tab/↑↓ move · j/↓ on repo: list · enter: pick/launch · esc: cancel) ",
        );
        let inner = block.inner(area);
        f.render_widget(block, area);

        if let Some(finder) = &form.finder {
            let home = std::env::var("HOME").unwrap_or_default();
            let [q_area, list_area] =
                RLayout::vertical([Constraint::Length(2), Constraint::Min(3)]).areas(inner);
            let rows = finder.rows();
            let roots = finder
                .roots
                .iter()
                .map(|r| r.replacen(&home, "~", 1))
                .collect::<Vec<_>>()
                .join(", ");
            let hint = if rows.is_empty() {
                format!("no repos match — scanned {roots} (config: [session] repo_roots) · or type a repo path")
            } else {
                format!("{} repos · ↑↓/tab move · enter pick · esc back", rows.len())
            };
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        format!("find repo: {}▏", finder.query),
                        Style::default().fg(Color::Yellow),
                    )),
                    Line::from(Span::styled(hint, Style::default().fg(Color::Gray))),
                ]),
                q_area,
            );
            let list = List::new(rows.iter().map(|(_, label)| ListItem::new(label.as_str())))
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ");
            let mut st = finder.state.clone();
            f.render_stateful_widget(list, list_area, &mut st);
            return;
        }
        if let Some((kind, items, state)) = &form.picker {
            let title = match kind {
                PickerKind::Repo => " select repo  (j/k · enter · esc) ",
                PickerKind::Model => " select model  (j/k · enter · esc) ",
            };
            let list = List::new(items.iter().map(|s| ListItem::new(s.as_str())))
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ");
            let mut st = state.clone();
            f.render_stateful_widget(list, inner, &mut st);
            return;
        }

        let mut lines: Vec<Line> = Vec::new();
        for (i, field) in FIELDS.iter().enumerate() {
            let focused = i == form.focus;
            let marker = if focused { "▶ " } else { "  " };
            let value = match field {
                Field::Repo => form.location_label(),
                Field::Name => form.name.clone(),
                Field::Model => {
                    if form.model.is_empty() {
                        "(agent default)".into()
                    } else {
                        form.model.clone()
                    }
                }
                Field::Workspace => match (&form.location, form.workspace) {
                    (Location::Folder(_), _) => "n/a (no repo)".into(),
                    (_, true) => "[x] dedicated worktree".into(),
                    (_, false) => "[ ] work directly in the repo".into(),
                },
                Field::Branch => {
                    if !form.workspace || matches!(form.location, Location::Folder(_)) {
                        "-".into()
                    } else if form.branch.is_empty() {
                        "(from name)".into()
                    } else {
                        form.branch.clone()
                    }
                }
                Field::Prompt => form.prompt.clone(),
                Field::Launch => String::new(),
            };
            let style = if focused {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let cursor = if focused && field.is_text() {
                "▏"
            } else {
                ""
            };
            let line = if *field == Field::Launch {
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(
                        if focused { "[ Launch ]" } else { "  Launch  " },
                        style.fg(Color::Green),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(format!("{:<10}", field.label()), style),
                    Span::raw(format!("{value}{cursor}")),
                ])
            };
            lines.push(line);
            if *field == Field::Prompt {
                lines.push(Line::raw(""));
            }
        }
        if let Some(e) = &form.error {
            lines.push(Line::from(Span::styled(
                format!("error: {e}"),
                Style::default().fg(Color::Red),
            )));
        }
        if let Some((kind, buf)) = &form.text_prompt {
            let label = match kind {
                TextPrompt::Folder => "folder (no repo)",
            };
            lines.push(Line::from(Span::styled(
                format!("{label}: {buf}▏   (enter · esc)"),
                Style::default().fg(Color::Yellow),
            )));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

const OTHER_REPO: &str = "  find another repo…";
const NO_REPO: &str = "  no repo — plain folder…";

/// Keep the tail of a long path (the part that identifies it).
fn shorten(p: &str, max: usize) -> String {
    let n = p.chars().count();
    if n <= max {
        p.to_string()
    } else {
        let tail: String = p.chars().skip(n - (max - 1)).collect();
        format!("…{tail}")
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}
