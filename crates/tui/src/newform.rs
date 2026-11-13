//! The new-session forms, as centered modals.
//!
//! - `n` — **describe it to the overlord**: what the session should do (as
//!   long as you like, including instructions for the overlord), plus
//!   optional hints (repo, agent, model, worktree, tags). The description wins over
//!   the hints. Sent as a `[rlord new-session request]` message; the overlord
//!   names the session, writes its prompt and launches it, or, when the
//!   description contradicts a hint, reopens this form with the hints
//!   corrected for the user to confirm (`confirm_new_session`). That
//!   describe → propose → confirm loop is rlord's standard for config-style
//!   input (see CLAUDE.md).
//! - `N` — the **manual form**: launch directly, no overlord involved. Field
//!   order puts the repo first. `Enter` on a field opens its picker; arrows
//!   only ever move between fields.

use super::*;
use ratatui::widgets::{Clear, List, ListItem, ListState};
use rlord_core::git::sanitize_branch;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    Repo,
    Name,
    Agent,
    Model,
    Workspace,
    Branch,
    Prompt,
    Tags,
    Launch,
}

/// Which form: the overlord request (`n`) or the manual launch (`N`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormKind {
    Overlord,
    Manual,
}

const OVERLORD_FIELDS: [Field; 7] = [
    Field::Prompt,
    Field::Repo,
    Field::Agent,
    Field::Model,
    Field::Workspace,
    Field::Tags,
    Field::Launch,
];

const FIELDS: [Field; 8] = [
    Field::Repo,
    Field::Name,
    Field::Agent,
    Field::Model,
    Field::Workspace,
    Field::Branch,
    Field::Prompt,
    Field::Launch,
];

impl Field {
    fn is_text(self) -> bool {
        matches!(
            self,
            Field::Name | Field::Branch | Field::Prompt | Field::Tags
        )
    }
    fn label(self, kind: FormKind) -> &'static str {
        match self {
            Field::Repo => "Repo",
            Field::Name => "Name",
            Field::Agent => "Agent",
            Field::Model => "Model",
            Field::Workspace if kind == FormKind::Overlord => "Worktree",
            Field::Workspace => "Workspace",
            Field::Branch => "Branch",
            Field::Prompt if kind == FormKind::Overlord => "What should this session do?",
            Field::Prompt => "Prompt",
            Field::Tags => "Tags",
            Field::Launch => "",
        }
    }
}

#[derive(Clone)]
pub(crate) enum Location {
    Repo(String),
    Folder(String),
    /// Overlord form only: the overlord picks the repo from the request.
    Auto,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    Repo,
    Agent,
    Model,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextPrompt {
    Folder,
    /// A model that isn't in the picker's list.
    Model,
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
    kind: FormKind,
    focus: usize,
    location: Location,
    name: String,
    /// The coding agent (`claude`, `codex`).
    agent: String,
    /// What each agent offers (`session.defaults`' `agents`): its label,
    /// default model and model choices.
    agents: Vec<Value>,
    model: String,
    workspace: bool,
    branch: String,
    branch_touched: bool,
    prompt: String,
    /// Overlord form: space-separated groups for the new session.
    tags: String,
    repos: Vec<Value>,
    models: Vec<String>,
    /// Overlord form: which hints the user set (vs. defaults), so the
    /// overlord knows what was a choice.
    repo_chosen: bool,
    agent_chosen: bool,
    model_chosen: bool,
    workspace_chosen: bool,
    tags_chosen: bool,
    /// The worktree default, restored when a plain folder (no worktree)
    /// gives way to a repo again...
    default_workspace: bool,
    /// ...unless there was a worktree choice before the folder: (value,
    /// the user's pick, the overlord's pick), put back on leaving it.
    before_folder: Option<(bool, bool, bool)>,
    /// Reopened by the overlord: the hints it set that the user hadn't picked
    /// (its resolved values: shown as such, sent as "your earlier pick").
    resolved: Vec<&'static str>,
    /// Opened by the overlord (confirm_new_session), not by the user.
    reopened: bool,
    /// Reopened by the overlord with corrected hints: submitting confirms
    /// them. `changed` names the hints it updated; `note` says why.
    confirmed: bool,
    changed: Vec<String>,
    note: Option<String>,
    picker: Option<(PickerKind, Vec<String>, ListState)>,
    finder: Option<Finder>,
    text_prompt: Option<(TextPrompt, String)>,
    error: Option<String>,
}

impl NewForm {
    /// Overlay fields proposed by the overlord (`repo`/`project`, `name`,
    /// `branch`, `model`, `workspace`, `prompt`); focus lands on Launch so
    /// confirming is one keypress.
    pub(crate) fn prefill(&mut self, p: &Value) {
        let get = |k: &str| p[k].as_str().map(str::to_string);
        if let Some(r) = get("repo") {
            self.location = Location::Repo(r);
        } else if let Some(d) = get("project") {
            self.location = Location::Folder(d);
            self.workspace = false;
        }
        if let Some(n) = get("name") {
            self.name = n;
        }
        if let Some(b) = get("branch") {
            self.branch = b;
            self.branch_touched = true;
        } else if !self.name.is_empty() && !self.branch_touched {
            self.branch = sanitize_branch(&self.name);
        }
        if let Some(a) = get("agent") {
            self.set_agent(&a);
        }
        if let Some(m) = get("model") {
            if !self.models.contains(&m) {
                self.models.insert(0, m.clone());
            }
            self.model = m;
        }
        if let Some(w) = p["workspace"].as_bool() {
            self.workspace = w && matches!(self.location, Location::Repo(_));
        }
        if let Some(pr) = get("prompt") {
            self.prompt = pr;
        }
        self.focus = self
            .fields()
            .iter()
            .position(|f| *f == Field::Launch)
            .unwrap_or(0);
    }

    /// The overlord's correction of an overlord-form request: the user's
    /// description, the hints as the overlord would launch them, which of
    /// them it changed and why. Submitting confirms it as is.
    pub(crate) fn prefill_confirm(&mut self, p: &Value) {
        let get = |k: &str| {
            p[k].as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        if let Some(d) = get("description") {
            self.prompt = d;
        }
        match (get("repo"), get("project")) {
            (Some(r), _) => self.location = Location::Repo(r),
            (None, Some(d)) => {
                self.location = Location::Folder(d);
                self.workspace = false;
            }
            (None, None) => self.location = Location::Auto,
        }
        if let Some(a) = get("agent") {
            self.set_agent(&a);
        }
        // "" is the agent default (the overlord reset the model).
        if let Some(m) = p["model"].as_str().map(|m| m.trim().to_string()) {
            if !m.is_empty() && !self.models.contains(&m) {
                self.models.insert(0, m.clone());
            }
            self.model = m;
        }
        if let Some(w) = p["workspace"].as_bool() {
            self.workspace = w && !matches!(self.location, Location::Folder(_));
        }
        // Only the tags it sent: not the group tab the form opened on.
        self.tags.clear();
        if let Some(t) = p["tags"].as_array() {
            self.tags = t
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
        }
        // Whose each hint is, as rlord worked it out from the request it
        // last delivered: `chosen` the user's, `resolved` the overlord's,
        // the rest defaults.
        let keys = ["repo", "agent", "model", "worktree", "tags"];
        let list = |name: &str| -> Vec<&'static str> {
            keys.into_iter()
                .filter(|k| {
                    p[name]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|c| c == *k))
                })
                .collect()
        };
        let chosen = list("chosen");
        self.repo_chosen = chosen.contains(&"repo");
        self.agent_chosen = chosen.contains(&"agent");
        self.model_chosen = chosen.contains(&"model");
        self.workspace_chosen = chosen.contains(&"worktree");
        self.tags_chosen = chosen.contains(&"tags");
        self.resolved = list("resolved");
        self.reopened = true;
        self.confirmed = true;
        self.changed = p["changed"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        self.note = get("note");
        self.focus = self
            .fields()
            .iter()
            .position(|f| *f == Field::Launch)
            .unwrap_or(0);
    }

    /// Set the location; leaving a plain folder brings the worktree default
    /// back (the folder turned it off, the user didn't).
    fn set_location(&mut self, loc: Location) {
        let was_folder = matches!(self.location, Location::Folder(_));
        if was_folder && !matches!(loc, Location::Folder(_)) {
            match self.before_folder.take() {
                Some((w, chosen, resolved)) => {
                    self.workspace = w;
                    self.workspace_chosen = chosen;
                    if resolved && !self.resolved.contains(&"worktree") {
                        self.resolved.push("worktree");
                    }
                }
                None if !self.workspace_chosen => self.workspace = self.default_workspace,
                None => {}
            }
        }
        self.location = loc;
    }

    /// Switch to another agent: its models are the picker's choices, and the
    /// model becomes its default (one agent's model means nothing to
    /// another). The same agent again changes nothing.
    fn set_agent(&mut self, name: &str) {
        if self.agent == name {
            return;
        }
        self.agent = name.to_string();
        let info = self.agents.iter().find(|a| a["name"] == name);
        self.models = info
            .and_then(|a| a["models"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        self.model = info
            .and_then(|a| a["model"].as_str())
            .unwrap_or("")
            .to_string();
        if !self.model.is_empty() && !self.models.contains(&self.model) {
            self.models.insert(0, self.model.clone());
        }
        self.model_chosen = false;
        self.resolved.retain(|k| *k != "model");
    }

    /// Why the chosen agent can't launch (rlord can't run it yet, or it
    /// isn't installed), and the install command to suggest. rlord never
    /// runs that command: the user does.
    fn agent_blocker(&self) -> Option<(String, Option<String>)> {
        let a = self
            .agents
            .iter()
            .find(|a| a["name"] == self.agent.as_str())?;
        let label = a["label"].as_str().unwrap_or(&self.agent);
        let cmd = a["install"]["command"].as_str().map(str::to_string);
        if a["supported"] == false {
            let have = if a["installed"] == false {
                "isn't installed, and "
            } else {
                ""
            };
            return Some((
                format!("{label} {have}can't run rlord sessions yet (support coming)"),
                cmd,
            ));
        }
        // Installed, but a version rlord can't run (OpenCode before v2).
        if let Some(why) = a["too_old"].as_str() {
            return Some((format!("{why}. Upgrade it yourself:"), cmd));
        }
        (a["installed"] == false).then(|| {
            (
                // (Only an agent rlord follows through hooks has that step.)
                if a["hooks"] == false {
                    format!("{label} isn't installed. Install it yourself:")
                } else {
                    format!(
                        "{label} isn't installed. Install it yourself, then `rlord install-hooks`:"
                    )
                },
                cmd,
            )
        })
    }

    /// The lines that say so on the form (empty when the agent is ready).
    fn agent_blocker_lines(&self, indent: &str) -> Vec<Line<'static>> {
        let Some((why, cmd)) = self.agent_blocker() else {
            return Vec::new();
        };
        let mut lines = vec![Line::from(Span::styled(
            format!("{indent}{why}"),
            Style::default().fg(theme::yellow()),
        ))];
        if let Some(cmd) = cmd {
            lines.push(Line::from(vec![
                Span::raw(format!("{indent}  ")),
                Span::styled(cmd, Style::default().fg(theme::text()).bold()),
                Span::styled("   ^Y copy", theme::faint()),
            ]));
        }
        lines
    }

    /// The agent's name as shown ("Claude Code", "Codex").
    fn agent_label(&self) -> String {
        self.agents
            .iter()
            .find(|a| a["name"] == self.agent.as_str())
            .and_then(|a| a["label"].as_str())
            .unwrap_or(&self.agent)
            .to_string()
    }

    /// The user picked a hint (`repo` / `agent` / `model` / `worktree` / `tags`). On a fresh form
    /// any pick is theirs (pinning the default counts); on a form the
    /// overlord reopened only a real change is, and it drops the overlord's
    /// label and confirmation.
    fn mark_picked(&mut self, key: &'static str, changed: bool) {
        if changed || !self.reopened {
            match key {
                "repo" => self.repo_chosen = true,
                "agent" => self.agent_chosen = true,
                "model" => self.model_chosen = true,
                "tags" => self.tags_chosen = true,
                "worktree" => self.workspace_chosen = true,
                _ => debug_assert!(false, "unknown hint {key}"),
            }
        }
        if changed {
            self.resolved.retain(|k| *k != key);
            self.description_changed();
        }
    }

    /// The description or a hint changed after the overlord reopened the
    /// form with corrected hints: its confirmation no longer holds, so it goes
    /// back to be checked against the hints again.
    fn description_changed(&mut self) {
        if self.confirmed {
            self.confirmed = false;
            self.changed.clear();
            self.note = None;
        }
    }

    /// A list is open over the form (a picker or the repo finder): the
    /// wheel moves in it.
    pub(crate) fn has_list(&self) -> bool {
        self.picker.is_some() || self.finder.is_some()
    }

    /// A click on item `i`: of the open list (selects it), else field `i`
    /// (focuses it). True when Enter should follow: a list item is chosen,
    /// a pick/toggle field or the send button is activated. A text field
    /// only takes the focus.
    pub(crate) fn click(&mut self, i: usize) -> bool {
        if let Some((_, items, state)) = &mut self.picker {
            state.select(Some(i.min(items.len().saturating_sub(1))));
            return true;
        }
        if let Some(finder) = &mut self.finder {
            finder.state.select(Some(i));
            return true;
        }
        if self.text_prompt.is_some() || i >= self.fields().len() {
            return false;
        }
        self.focus = i;
        !self.current().is_text()
    }

    fn fields(&self) -> &'static [Field] {
        match self.kind {
            FormKind::Overlord => &OVERLORD_FIELDS,
            FormKind::Manual => &FIELDS,
        }
    }

    fn current(&self) -> Field {
        self.fields()[self.focus]
    }

    fn location_label(&self) -> String {
        let home = std::env::var("HOME").unwrap_or_default();
        match &self.location {
            Location::Repo(p) => shorten(&p.replacen(&home, "~", 1), 58),
            Location::Folder(p) => format!("{} (no repo)", shorten(&p.replacen(&home, "~", 1), 48)),
            Location::Auto => "overlord picks".into(),
        }
    }
}

impl App {
    /// Open a form, seeded from the daemon's session defaults and repo list.
    pub(crate) fn open_new_form(&mut self, kind: FormKind) {
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
        // Each agent's default model and choices; the form opens on the
        // configured agent (`set_agent` below fills its models in).
        let default_agent = defaults["agent"].as_str().unwrap_or("claude").to_string();
        let agents = defaults["agents"].as_array().cloned().unwrap_or_else(|| {
            vec![
                json!({"name": default_agent, "label": default_agent, "installed": true,
                        "model": defaults["model"], "models": defaults["models"]}),
            ]
        });
        // On the groups tab, a new session joins the group being viewed.
        let tags = match (&self.tab, &self.group_sel) {
            (groups::Tab::Groups, Some(g)) if g != groups::UNTAGGED => g.clone(),
            _ => String::new(),
        };
        let from_group = !tags.is_empty();
        let mut form = Box::new(NewForm {
            kind,
            focus: 0,
            location,
            name: String::new(),
            agent: String::new(),
            agents,
            model: String::new(),
            workspace: defaults["workspace"].as_bool().unwrap_or(true),
            default_workspace: defaults["workspace"].as_bool().unwrap_or(true),
            before_folder: None,
            branch: String::new(),
            branch_touched: false,
            prompt: String::new(),
            tags,
            repos,
            models: Vec::new(),
            repo_chosen: false,
            agent_chosen: false,
            model_chosen: false,
            workspace_chosen: false,
            // Opened from a group: joining it is the user's choice.
            tags_chosen: from_group,
            resolved: Vec::new(),
            reopened: false,
            confirmed: false,
            changed: Vec::new(),
            note: None,
            picker: None,
            finder: None,
            text_prompt: None,
            error: None,
        });
        form.set_agent(&default_agent);
        self.mode = Mode::NewSession(form);
    }

    /// Returns the form to keep it open, or None once it closed.
    pub(crate) fn handle_form(
        &mut self,
        k: ratatui::crossterm::event::KeyEvent,
        mut f: Box<NewForm>,
        terminal: &mut DefaultTerminal,
    ) -> Result<Option<Box<NewForm>>> {
        let code = k.code;
        // --- a text prompt (other repo path / folder) is open ---
        if let Some((kind, mut buf)) = f.text_prompt.take() {
            match code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let path = rlord_core::config::expand_tilde(buf.trim());
                    match kind {
                        TextPrompt::Model => {
                            let m = buf.trim().to_string();
                            if !m.is_empty() {
                                f.mark_picked("model", m != f.model);
                                if !f.models.contains(&m) {
                                    f.models.insert(0, m.clone());
                                }
                                f.model = m;
                            }
                        }
                        TextPrompt::Folder => {
                            if std::path::Path::new(&path).is_dir() {
                                let moved =
                                    !matches!(&f.location, Location::Folder(p) if *p == path);
                                f.mark_picked("repo", moved);
                                if moved {
                                    // Kept for a change of mind back to a repo.
                                    if !matches!(f.location, Location::Folder(_)) {
                                        f.before_folder = Some((
                                            f.workspace,
                                            f.workspace_chosen,
                                            f.resolved.contains(&"worktree"),
                                        ));
                                    }
                                    f.location = Location::Folder(path);
                                    // A plain folder has no worktree: neither
                                    // the overlord's pick nor the user's.
                                    f.resolved.retain(|k| *k != "worktree");
                                    f.workspace_chosen = false;
                                    f.workspace = false;
                                }
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
            // One direction for every move key (plain j/k type here).
            let down = picker_step(&k).or(match code {
                KeyCode::Tab => Some(true),
                KeyCode::BackTab => Some(false),
                _ => None,
            });
            match code {
                KeyCode::Esc => {}
                _ if down.is_some() => {
                    finder
                        .state
                        .select(Some(wrap_step(i, n, down == Some(true))));
                    f.finder = Some(finder);
                }
                KeyCode::Enter => {
                    if let Some((path, _)) = finder.rows().get(i) {
                        f.mark_picked(
                            "repo",
                            !matches!(&f.location, Location::Repo(p) if p == path),
                        );
                        f.set_location(Location::Repo(path.clone()));
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
            // One direction for every move key (no text here: j/k move).
            let down = picker_step(&k).or(match code {
                KeyCode::Char('j') | KeyCode::Tab => Some(true),
                KeyCode::Char('k') | KeyCode::BackTab => Some(false),
                _ => None,
            });
            match code {
                KeyCode::Esc => {}
                _ if down.is_some() => {
                    state.select(Some(wrap_step(i, n, down == Some(true))));
                    f.picker = Some((kind, items, state));
                }
                KeyCode::Enter => match kind {
                    PickerKind::Agent => {
                        // One that isn't installed can be picked: the form
                        // then says how to get it (and won't send).
                        let picked = f.agents[i]["name"].as_str().unwrap_or("").to_string();
                        f.mark_picked("agent", picked != f.agent);
                        f.set_agent(&picked);
                        f.error = None;
                    }
                    PickerKind::Model if items[i] == OTHER_MODEL => {
                        f.text_prompt = Some((TextPrompt::Model, String::new()));
                    }
                    PickerKind::Model => {
                        let picked = if items[i] == "(agent default)" {
                            String::new()
                        } else {
                            items[i].clone()
                        };
                        // A real change only (re-picking keeps provenance).
                        f.mark_picked("model", picked != f.model);
                        f.model = picked;
                    }
                    PickerKind::Repo => {
                        // (Chosen once a repo or folder is actually set, not
                        // when a sub-prompt opens and may be cancelled.)
                        if items[i] == OTHER_REPO {
                            self.open_finder(&mut f);
                        } else if items[i] == NO_REPO {
                            f.text_prompt = Some((TextPrompt::Folder, String::new()));
                        } else if items[i] == AUTO_REPO {
                            f.mark_picked("repo", !matches!(f.location, Location::Auto));
                            f.set_location(Location::Auto);
                            f.error = None;
                        } else {
                            let path = f.repos[i]["path"].as_str().unwrap_or("").to_string();
                            f.mark_picked(
                                "repo",
                                !matches!(&f.location, Location::Repo(p) if *p == path),
                            );
                            f.set_location(Location::Repo(path));
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
        let n = f.fields().len();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // Shift-Enter (or ctrl-s, where the terminal can't tell Shift-Enter
        // from Enter) sends from any field.
        let shift_enter = code == KeyCode::Enter && k.modifiers.contains(KeyModifiers::SHIFT);
        if shift_enter || (ctrl && code == KeyCode::Char('s')) {
            return self.submit_form(f);
        }
        // ctrl-y: copy the suggested install command (the user runs it).
        if ctrl && code == KeyCode::Char('y') {
            if let Some((_, Some(cmd))) = f.agent_blocker() {
                overlay::copy_text("rlord-install", &cmd);
                self.status_line = format!("copied: {cmd}");
                f.error = None;
            }
            return Ok(Some(f));
        }
        // ctrl-e (the overlord form): edit the description in $EDITOR, e.g.
        // your vim; it comes back into the box.
        if ctrl && code == KeyCode::Char('e') && f.kind == FormKind::Overlord {
            match edit_in_editor(terminal, "new-session.md", &f.prompt) {
                Ok(text) => {
                    let text = text.trim_end().to_string();
                    // (Trailing whitespace isn't a change: quitting the
                    // editor without edits keeps a confirmation.)
                    if text != f.prompt.trim_end() {
                        f.prompt = text;
                        f.description_changed();
                    }
                    f.error = None;
                }
                Err(why) => f.error = Some(why),
            }
            f.focus = f
                .fields()
                .iter()
                .position(|x| *x == Field::Prompt)
                .unwrap_or(0);
            return Ok(Some(f));
        }
        let next = |f: &mut NewForm| f.focus = (f.focus + 1) % n;
        let prev = |f: &mut NewForm| f.focus = (f.focus + n - 1) % n;
        match code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Tab => next(&mut f),
            KeyCode::BackTab => prev(&mut f),
            KeyCode::Down => next(&mut f),
            KeyCode::Up => prev(&mut f),
            KeyCode::Char('j') if !field.is_text() => next(&mut f),
            KeyCode::Char('k') if !field.is_text() => prev(&mut f),
            // The request is free text: Enter is a new line (ctrl-s sends).
            KeyCode::Enter if field == Field::Prompt && f.kind == FormKind::Overlord => {
                f.prompt.push('\n');
                f.description_changed();
            }
            KeyCode::Enter => match field {
                Field::Repo => self.open_picker(&mut f, PickerKind::Repo),
                Field::Agent => self.open_picker(&mut f, PickerKind::Agent),
                Field::Model => self.open_picker(&mut f, PickerKind::Model),
                Field::Workspace => {
                    // A plain folder has no worktree to toggle.
                    if !matches!(f.location, Location::Folder(_)) {
                        f.workspace = !f.workspace;
                        f.mark_picked("worktree", true);
                    }
                }
                Field::Launch => return self.submit_form(f),
                _ => next(&mut f),
            },
            KeyCode::Char(' ') if field == Field::Workspace => {
                // A plain folder has no worktree to toggle.
                if !matches!(f.location, Location::Folder(_)) {
                    f.workspace = !f.workspace;
                    f.mark_picked("worktree", true);
                }
            }
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
                    f.description_changed();
                }
                Field::Tags => {
                    let removed = f.tags.pop().is_some();
                    f.mark_picked("tags", removed);
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
                    Field::Prompt => {
                        f.prompt.push(c);
                        f.description_changed();
                    }
                    Field::Tags => {
                        f.tags.push(c);
                        f.mark_picked("tags", true);
                    }
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
                if f.kind == FormKind::Overlord {
                    v.push(AUTO_REPO.to_string());
                }
                v
            }
            PickerKind::Agent => f
                .agents
                .iter()
                .map(|a| {
                    format!(
                        "{}{}",
                        a["label"].as_str().unwrap_or("?"),
                        if a["supported"] == false {
                            "  (rlord support coming)"
                        } else if a["too_old"].is_string() {
                            "  (too old for rlord: pick it to see how to upgrade)"
                        } else if a["installed"] == false {
                            "  (not installed: pick it to see how)"
                        } else {
                            ""
                        }
                    )
                })
                .collect(),
            // The agent's own choices (none known: type one).
            PickerKind::Model => {
                let mut v = f.models.clone();
                v.push("(agent default)".to_string());
                v.push(OTHER_MODEL.to_string());
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
                Location::Folder(_) => f.repos.len() + 1,
                Location::Auto => items.len() - 1,
            },
            PickerKind::Agent => f
                .agents
                .iter()
                .position(|a| a["name"] == f.agent.as_str())
                .unwrap_or(0),
            // No model set is "(agent default)", right after the choices.
            PickerKind::Model => f
                .models
                .iter()
                .position(|m| *m == f.model)
                .unwrap_or(f.models.len()),
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
        // Nothing to launch it with: say so here, not in a dead pane.
        if let Some((why, _)) = f.agent_blocker() {
            f.error = Some(format!(
                "{} (pick another agent, or install it and reopen this form)",
                why.trim_end_matches(':')
            ));
            return Ok(Some(f));
        }
        match f.kind {
            FormKind::Overlord => self.submit_request(f),
            FormKind::Manual => self.submit_manual(f),
        }
    }

    /// Hand the request to the overlord and open it, so the user sees any
    /// question it asks.
    fn submit_request(&mut self, mut f: Box<NewForm>) -> Result<Option<Box<NewForm>>> {
        if f.prompt.trim().is_empty() {
            f.error = Some("describe what the session should do first".into());
            f.focus = 0;
            return Ok(Some(f));
        }
        let chosen: Vec<&str> = [
            ("repo", f.repo_chosen),
            ("agent", f.agent_chosen),
            ("model", f.model_chosen),
            ("worktree", f.workspace_chosen),
            ("tags", f.tags_chosen && !f.tags.trim().is_empty()),
        ]
        .into_iter()
        .filter_map(|(k, c)| c.then_some(k))
        .collect();
        let mut params = json!({
            "description": f.prompt,
            "agent": f.agent,
            "model": f.model,
            "workspace": f.workspace,
            "tags": f.tags.split_whitespace().map(|t| t.trim_start_matches('#')).collect::<Vec<_>>(),
            "chosen": chosen,
            // The overlord's resolved values the user left as they were.
            "resolved": f.resolved,
            "confirmed": f.confirmed,
        });
        match &f.location {
            Location::Repo(p) => params["repo"] = json!(p),
            Location::Folder(p) => {
                params["repo"] = json!(format!("{p} (plain folder, not a repo)"));
                params["workspace"] = json!(false);
            }
            Location::Auto => {}
        }
        // Sent in the background: a stopped overlord can take a while
        // to start, and the list must not freeze meanwhile. Its overlay opens
        // now, so the user watches it pick the request up.
        // What can fail fast is checked here, so the form stays open with
        // the error (no overlord set up yet).
        let exists = self
            .call("overlord.status", Value::Null)
            .is_some_and(|st| st["exists"] == true);
        if !exists {
            f.error = Some(format!(
                "no overlord yet: press {} to set it up (or N for the manual form)",
                self.keys.label(Action::Overlord)
            ));
            return Ok(Some(f));
        }
        std::thread::spawn(move || {
            let _ = client_call(&paths::socket_path(), "overlord.new_session", params);
        });
        self.status_line = if f.confirmed {
            "confirmed — the overlord is launching it".into()
        } else {
            "sent to the overlord".into()
        };
        self.show_overlord();
        Ok(None)
    }

    fn submit_manual(&mut self, mut f: Box<NewForm>) -> Result<Option<Box<NewForm>>> {
        let mut params = json!({
            "name": if f.name.trim().is_empty() { None } else { Some(f.name.trim()) },
            "prompt": if f.prompt.trim().is_empty() { None } else { Some(f.prompt.trim()) },
            "agent": f.agent,
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
            Location::Auto => {
                f.error = Some("pick a repo".into());
                return Ok(Some(f));
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
        let (area, title) = match form.kind {
            FormKind::Overlord => (centered(f.area(), 82, 28), "New session"),
            FormKind::Manual => (centered(f.area(), 76, 20), "New session · manual"),
        };
        f.render_widget(Clear, area);
        self.hits.add(f.area(), mouse::Hit::Outside);
        self.hits.add(area, mouse::Hit::Inside);
        let block = theme::modal(title);
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
                        Style::default().fg(theme::yellow()),
                    )),
                    Line::from(Span::styled(hint, Style::default().fg(theme::muted_c()))),
                ]),
                q_area,
            );
            let list = List::new(rows.iter().map(|(_, label)| ListItem::new(label.as_str())))
                .highlight_style(theme::selected())
                .highlight_symbol(theme::SELECT_BAR);
            let mut st = finder.state;
            f.render_stateful_widget(list, list_area, &mut st);
            // Click a repo to pick it.
            for i in st.offset()..rows.len().min(st.offset() + list_area.height as usize) {
                self.hits.row(
                    list_area,
                    list_area.y + (i - st.offset()) as u16,
                    mouse::Hit::Pick(i),
                );
            }
            return;
        }
        if let Some((kind, items, state)) = &form.picker {
            let title = match kind {
                PickerKind::Repo => " Repo ",
                PickerKind::Agent => " Agent ",
                PickerKind::Model => " Model ",
            };
            let list = List::new(items.iter().map(|s| ListItem::new(s.as_str())))
                .block(theme::panel(title, true).title_bottom(
                    Line::from(" j/k move · enter pick · esc back ").style(theme::faint()),
                ))
                .highlight_style(theme::selected())
                .highlight_symbol(theme::SELECT_BAR);
            let mut st = *state;
            f.render_stateful_widget(list, inner, &mut st);
            // Click a choice to pick it (inside the list's own border).
            let body = Rect {
                x: inner.x + 1,
                y: inner.y + 1,
                width: inner.width.saturating_sub(2),
                height: inner.height.saturating_sub(2),
            };
            for i in st.offset()..items.len().min(st.offset() + body.height as usize) {
                self.hits
                    .row(body, body.y + (i - st.offset()) as u16, mouse::Hit::Pick(i));
            }
            return;
        }

        if form.kind == FormKind::Overlord {
            // The overlord's key from anywhere (tmux); the list's own key if
            // that one is turned off.
            let k = &self.config.tmux.overlord;
            let overlord_key = if k.trim().is_empty() {
                self.keys.label(Action::Overlord)
            } else {
                tmux::key_label(k)
            };
            draw_describe_form(f, inner, form, &overlord_key, &self.hits);
            return;
        }
        let mut lines: Vec<Line> = Vec::new();
        for (i, field) in form.fields().iter().enumerate() {
            // Click a field to focus it (a pick, toggle or Launch: to use it).
            self.hits
                .row(inner, inner.y + lines.len() as u16, mouse::Hit::Pick(i));
            let focused = i == form.focus;
            let marker = if focused { "▶ " } else { "  " };
            let value = match field {
                Field::Repo => form.location_label(),
                Field::Name => form.name.clone(),
                Field::Agent => form.agent_label(),
                Field::Model => {
                    if form.model.is_empty() {
                        "(agent default)".into()
                    } else {
                        form.model.clone()
                    }
                }
                Field::Workspace => match (&form.location, form.workspace) {
                    (Location::Folder(_), _) => "n/a (no repo)".into(),
                    (Location::Auto, true) => "[x] dedicated worktree".into(),
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
                Field::Tags => {
                    if form.tags.is_empty() && !focused {
                        "(overlord picks)".into()
                    } else {
                        form.tags.clone()
                    }
                }
                Field::Launch => String::new(),
            };
            let style = if focused {
                Style::default()
                    .fg(theme::yellow())
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
                let label = match form.kind {
                    FormKind::Overlord => "Send to overlord",
                    FormKind::Manual => "Launch",
                };
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(
                        if focused {
                            format!("[ {label} ]")
                        } else {
                            format!("  {label}  ")
                        },
                        style.fg(theme::green()),
                    ),
                ])
            } else if *field == Field::Prompt && form.kind == FormKind::Overlord {
                // Multi-line request: label on its own line, text indented.
                lines.push(Line::from(vec![
                    Span::raw(marker),
                    Span::styled(field.label(form.kind), style),
                ]));
                let text = format!("{value}{cursor}");
                let body: Vec<&str> = if text.is_empty() {
                    vec![""]
                } else {
                    text.split('\n').collect()
                };
                for l in body {
                    lines.push(Line::raw(format!("    {l}")));
                }
                lines.push(Line::raw(""));
                continue;
            } else {
                Line::from(vec![
                    Span::raw(marker),
                    Span::styled(format!("{:<10}", field.label(form.kind)), style),
                    Span::raw(format!("{value}{cursor}")),
                ])
            };
            lines.push(line);
            if *field == Field::Prompt {
                lines.push(Line::raw(""));
            }
        }
        lines.extend(form.agent_blocker_lines(""));
        if let Some(e) = &form.error {
            lines.push(Line::from(Span::styled(
                format!("error: {e}"),
                Style::default().fg(theme::red()),
            )));
        }
        if let Some((kind, buf)) = &form.text_prompt {
            let label = match kind {
                TextPrompt::Folder => "folder (no repo)",
                TextPrompt::Model => "model",
            };
            lines.push(Line::from(Span::styled(
                format!("{label}: {buf}▏   (enter · esc)"),
                Style::default().fg(theme::yellow()),
            )));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

/// The overlord form: a description box up top (the part that matters),
/// then the optional hints, visibly secondary, then send.
fn draw_describe_form(
    f: &mut Frame,
    inner: Rect,
    form: &NewForm,
    overlord_key: &str,
    hits: &mouse::Hits,
) {
    // The index of a field in this form (what a click on it picks).
    let pick = |fld: Field| {
        let i = OVERLORD_FIELDS.iter().position(|x| *x == fld).unwrap_or(0);
        mouse::Hit::Pick(i)
    };
    let dim = theme::faint();
    let gray = theme::muted();
    let accent = Style::default().fg(theme::accent());
    let focus = |fld: Field| form.current() == fld;
    // Text width inside the box: margins, borders and padding.
    let width = inner.width.saturating_sub(8).max(1) as usize;

    // Header: the overlord's note when it reopened the form, else what
    // happens on send.
    let mut head: Vec<Line> = vec![Line::raw("")];
    match (&form.note, form.confirmed) {
        (Some(note), _) => {
            head.push(Line::from(vec![
                Span::styled("  ✦ ", accent.bold()),
                Span::styled(note.clone(), accent),
            ]));
            head.push(Line::from(Span::styled(
                "    Check the hints below, then send to launch it as shown.",
                gray,
            )));
        }
        (None, true) => head.push(Line::from(Span::styled(
            "  ✦ Updated to match your description. Send to launch it as shown.",
            accent,
        ))),
        (None, false) => {
            // This form is a thin wrapper: the intended way to start a session
            // is to just tell the overlord. Say so, with an example.
            head.push(Line::from(vec![
                Span::styled("  Tip: ", Style::default().fg(theme::accent()).bold()),
                Span::styled("no form needed: press ", gray),
                Span::styled(
                    format!(" {overlord_key} "),
                    Style::default().fg(theme::text()).bg(theme::chip()).bold(),
                ),
                Span::styled(" and just say it, e.g.", gray),
            ]));
            head.push(Line::from(Span::styled(
                "  \"start a session in ai to fix the flaky login tests\"",
                Style::default().fg(theme::text()).italic(),
            )));
            head.push(Line::from(Span::styled(
                "  This form sends the same to the overlord, with optional hints.",
                gray,
            )));
        }
    }

    // The description: a bordered box that grows with the text.
    let text = format!(
        "{}{}",
        form.prompt,
        if focus(Field::Prompt) { "▏" } else { "" }
    );
    let wrapped: usize = text.split('\n').map(|l| wrapped_rows(l, width)).sum();
    let box_h = (wrapped as u16 + 2).clamp(5, 12);

    let [head_area, label_area, box_area, rest] = RLayout::vertical([
        Constraint::Length(head.len() as u16 + 1),
        Constraint::Length(1),
        Constraint::Length(box_h),
        Constraint::Min(0),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(head), head_area);
    let label_style = if focus(Field::Prompt) {
        Style::default().fg(theme::yellow()).bold()
    } else {
        Style::default().bold()
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(Field::Prompt.label(FormKind::Overlord), label_style),
        ])),
        label_area,
    );
    let box_area = Rect {
        x: box_area.x + 2,
        width: box_area.width.saturating_sub(4),
        ..box_area
    };
    hits.add(box_area, pick(Field::Prompt));
    let border = if focus(Field::Prompt) {
        Style::default().fg(theme::yellow())
    } else {
        dim
    };
    let body = if form.prompt.is_empty() && !focus(Field::Prompt) {
        Paragraph::new(Span::styled("e.g. fix the flaky login test in myapp", dim))
    } else {
        // Longer than the box: keep the end (where you type) in view.
        let visible = box_h.saturating_sub(2) as usize;
        let skip = wrapped.saturating_sub(visible) as u16;
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((skip, 0))
    };
    f.render_widget(
        body.block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(border)
                .padding(ratatui::widgets::Padding::horizontal(1)),
        ),
        box_area,
    );

    // Hints: secondary, and say so.
    let mut lines: Vec<Line> = vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled("  Optional hints", Style::default().bold()),
            Span::styled(
                if form.confirmed {
                    "  · what the overlord will use"
                } else {
                    "  · your description wins if they disagree"
                },
                dim,
            ),
        ]),
    ];
    // (Each hint is one line of `rest`; `row` is the line it will be on.)
    let hint = |row: usize, fld: Field, key: &str, value: String, chosen: bool| -> Line<'static> {
        hits.row(rest, rest.y + row as u16, pick(fld));
        let focused = focus(fld);
        let changed = form.changed.iter().any(|c| c == key);
        let label_style = if focused {
            Style::default().fg(theme::yellow()).bold()
        } else {
            gray
        };
        let value_style = if changed {
            accent
        } else if chosen {
            Style::default()
        } else {
            dim
        };
        let mut spans = vec![
            Span::raw(if focused { "  ▶ " } else { "    " }),
            Span::styled(
                format!("{:<10}", fld.label(FormKind::Overlord)),
                label_style,
            ),
            Span::styled(value.clone(), value_style),
        ];
        if changed {
            spans.push(Span::styled("  ✦ updated", accent));
        } else if !chosen && !value.starts_with("overlord picks") {
            spans.push(Span::styled(
                if form.resolved.contains(&key) {
                    "  · overlord's pick"
                } else {
                    "  · default"
                },
                dim,
            ));
        }
        Line::from(spans)
    };
    let auto = matches!(form.location, Location::Auto);
    lines.push(hint(
        lines.len(),
        Field::Repo,
        "repo",
        form.location_label(),
        form.repo_chosen && !auto,
    ));
    lines.push(hint(
        lines.len(),
        Field::Agent,
        "agent",
        form.agent_label(),
        form.agent_chosen,
    ));
    lines.push(hint(
        lines.len(),
        Field::Model,
        "model",
        if form.model.is_empty() {
            "agent default".into()
        } else {
            form.model.clone()
        },
        form.model_chosen,
    ));
    lines.push(hint(
        lines.len(),
        Field::Workspace,
        "worktree",
        match (&form.location, form.workspace) {
            (Location::Folder(_), _) => "n/a (no repo)".into(),
            (_, true) => "yes, its own git worktree".into(),
            (_, false) => "no, work in the repo directly".into(),
        },
        form.workspace_chosen,
    ));
    let tags_focused = focus(Field::Tags);
    let tags = if form.tags.is_empty() && !tags_focused {
        "overlord picks".to_string()
    } else {
        format!("{}{}", form.tags, if tags_focused { "▏" } else { "" })
    };
    lines.push(hint(
        lines.len(),
        Field::Tags,
        "tags",
        tags,
        form.tags_chosen && !form.tags.is_empty(),
    ));
    let blocker = form.agent_blocker_lines("    ");
    if blocker.is_empty() {
        lines.push(Line::raw(""));
    }
    lines.extend(blocker);

    let send_focused = focus(Field::Launch);
    let send = if form.confirmed {
        " Launch "
    } else {
        " Send to overlord "
    };
    hits.row(rest, rest.y + lines.len() as u16, pick(Field::Launch));
    lines.push(Line::from(vec![
        Span::raw(if send_focused { "  ▶ " } else { "    " }),
        Span::styled(
            send,
            if send_focused {
                Style::default()
                    .fg(theme::on_accent())
                    .bg(theme::green())
                    .bold()
            } else {
                Style::default().fg(theme::green()).bold()
            },
        ),
    ]));
    lines.push(Line::raw(""));
    let enter = match form.current() {
        Field::Prompt => "new line",
        Field::Repo | Field::Agent | Field::Model => "pick",
        Field::Workspace => "toggle",
        Field::Launch => "send",
        _ => "next",
    };
    lines.push(keys_line(&[
        ("shift-enter", "send", Cap::Context),
        ("enter", enter, Cap::Global),
        ("^E", "editor", Cap::Global),
        ("tab", "move", Cap::Global),
        ("esc", "cancel", Cap::Global),
    ]));
    if let Some(e) = &form.error {
        lines.push(Line::from(Span::styled(
            format!("  error: {e}"),
            Style::default().fg(theme::red()),
        )));
    }
    if let Some((kind, buf)) = &form.text_prompt {
        let label = match kind {
            TextPrompt::Folder => "folder (no repo)",
            TextPrompt::Model => "model",
        };
        lines.push(Line::from(Span::styled(
            format!("  {label}: {buf}▏   (enter · esc)"),
            Style::default().fg(theme::yellow()),
        )));
    }
    // The key hints under the form are buttons too.
    hits.keycaps(rest.x, rest.y, &lines);
    f.render_widget(Paragraph::new(lines), rest);
}

/// Rows a line takes when wrapped at word boundaries to `width` columns (as
/// the description box wraps), counting wide characters as two.
fn wrapped_rows(line: &str, width: usize) -> usize {
    let w = |t: &str| Span::raw(t).width();
    let (mut rows, mut col) = (1usize, 0usize);
    for word in line.split(' ') {
        let ww = w(word);
        let need = if col == 0 { ww } else { ww + 1 };
        if col > 0 && col + need > width {
            rows += 1;
            col = 0;
        }
        // A word longer than the line breaks across rows.
        let ww_rows = ww.max(1).div_ceil(width.max(1));
        if ww > width {
            rows += ww_rows - 1;
            col = ww - (ww_rows - 1) * width;
        } else {
            col += if col == 0 { ww } else { ww + 1 };
        }
    }
    rows
}

const OTHER_MODEL: &str = "  another model…";
const OTHER_REPO: &str = "  find another repo…";
const NO_REPO: &str = "  no repo — plain folder…";
const AUTO_REPO: &str = "  let the overlord pick (from the description)";

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

pub(crate) fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(app: &App, form: &NewForm) -> String {
        use ratatui::backend::TestBackend;
        let mut term = ratatui::Terminal::new(TestBackend::new(90, 30)).unwrap();
        term.draw(|f| app.draw_form(f, form)).unwrap();
        let buf = term.backend().buffer();
        (0..30)
            .map(|y| {
                (0..90)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn form(app: &mut App) -> Box<NewForm> {
        app.open_new_form(FormKind::Overlord);
        match std::mem::replace(&mut app.mode, Mode::Normal) {
            Mode::NewSession(f) => f,
            _ => panic!("no form"),
        }
    }

    #[test]
    fn describe_form_puts_the_description_first_and_hints_second() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let keys = KeyMap::from_config(&Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        let mut f = form(&mut app);
        f.prompt = "fix the flaky login test".into();
        let out = render(&app, &f);
        if std::env::var_os("SHOW_FORM").is_some() {
            println!("{out}");
        }
        let at = |s: &str| {
            out.find(s)
                .unwrap_or_else(|| panic!("{s:?} missing:\n{out}"))
        };
        assert!(at("What should this session do?") < at("Optional hints"));
        assert!(out.contains("your description wins"));
        assert!(out.contains("Send to overlord"));
        assert!(!out.contains("Request"));

        // Reopened by the overlord: its note, updated hints marked.
        f.prefill_confirm(&json!({
            "description": "fix the flaky login test in myapp",
            "repo": "/r/myapp", "model": "opus", "workspace": true,
            "changed": ["repo"], "note": "Updated to match your description: repo is myapp",
        }));
        let out = render(&app, &f);
        if std::env::var_os("SHOW_FORM").is_some() {
            println!("{out}");
        }
        assert!(out.contains("✦ Updated to match your description: repo is myapp"));
        let repo_line = out.lines().find(|l| l.contains("/r/myapp")).unwrap();
        assert!(repo_line.contains("updated"), "{repo_line}");
        assert!(out.contains("Launch"));
        assert!(f.confirmed && f.current() == Field::Launch);

        // Provenance: the user's own pick stays theirs unless the overlord
        // changed it; hints the overlord set are its picks; unset ones are
        // still defaults.
        f.prefill_confirm(&json!({
            "description": "x", "repo": "/r/a", "model": "",
            "chosen": ["model", "bogus"], "resolved": ["repo"], "changed": ["repo"], "note": "n",
        }));
        assert!(f.model_chosen && !f.repo_chosen && !f.workspace_chosen);
        assert_eq!(f.resolved, ["repo"]);
        assert_eq!(f.model, "", "reset to the agent default");
    }

    #[test]
    fn the_agent_hint_brings_its_own_models() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let keys = KeyMap::from_config(&Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        let mut f = form(&mut app);
        f.agents = vec![
            json!({"name": "claude", "label": "Claude Code", "installed": true,
                   "model": "opus", "models": ["opus", "sonnet"]}),
            json!({"name": "codex", "label": "Codex", "installed": true,
                   "model": "", "models": ["gpt-6-luna"]}),
        ];
        f.agent.clear();
        f.set_agent("claude");
        assert_eq!((f.model.as_str(), f.models.len()), ("opus", 2));
        let out = render(&app, &f);
        let line = out.lines().find(|l| l.contains("Agent")).unwrap();
        assert!(
            line.contains("Claude Code") && line.contains("default"),
            "{line}"
        );
        // Another agent: its models, its default; a model picked for the
        // first one doesn't carry over.
        f.model_chosen = true;
        f.set_agent("codex");
        assert_eq!(f.models, ["gpt-6-luna"]);
        assert!(f.model.is_empty() && !f.model_chosen);
        // The overlord reopens the form on Codex with a model.
        f.set_agent("claude");
        f.prefill_confirm(&json!({
            "description": "x", "agent": "codex", "model": "gpt-6-luna",
            "resolved": ["agent", "model"], "changed": ["agent", "model"], "note": "n",
        }));
        assert_eq!(
            (f.agent.as_str(), f.model.as_str()),
            ("codex", "gpt-6-luna")
        );
        assert_eq!(f.resolved, ["agent", "model"]);
        let out = render(&app, &f);
        let line = out.lines().find(|l| l.contains("Agent")).unwrap();
        assert!(line.contains("Codex") && line.contains("updated"), "{line}");
    }

    #[test]
    fn a_missing_agent_shows_how_to_install_it_and_blocks_sending() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let keys = KeyMap::from_config(&Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        let mut f = form(&mut app);
        f.prompt = "fix it".into();
        f.agents = vec![
            json!({"name": "claude", "label": "Claude Code", "installed": true,
                   "supported": true, "model": "opus", "models": ["opus"]}),
            json!({"name": "codex", "label": "Codex", "installed": false, "supported": true,
                   "model": "", "models": [],
                   "install": {"via": "homebrew", "command": "brew install --cask codex"}}),
            json!({"name": "opencode", "label": "OpenCode", "installed": true, "hooks": false,
                   "supported": true, "model": "", "models": [],
                   "too_old": "rlord needs OpenCode v2 or newer (this is 1.4.3)",
                   "install": {"via": "homebrew", "command": "brew install anomalyco/tap/opencode-v2"}}),
            json!({"name": "gemini", "label": "Gemini", "installed": true,
                   "supported": false, "model": "", "models": []}),
        ];
        f.agent.clear();
        f.set_agent("claude");
        assert!(f.agent_blocker().is_none());
        f.set_agent("codex");
        let out = render(&app, &f);
        assert!(
            out.contains("Codex isn't installed. Install it yourself"),
            "{out}"
        );
        assert!(out.contains("brew install --cask codex") && out.contains("^Y copy"));
        // Sending is refused, with the reason; the form stays open.
        let f = app.submit_form(f).unwrap().expect("still open");
        assert!(f.error.as_deref().unwrap().contains("isn't installed"));
        let mut f = f;
        // Installed, but too old to run: the command that upgrades it.
        f.set_agent("opencode");
        let (why, cmd) = f.agent_blocker().unwrap();
        assert!(
            why.contains("OpenCode v2") && why.contains("Upgrade"),
            "{why}"
        );
        assert_eq!(
            cmd.as_deref(),
            Some("brew install anomalyco/tap/opencode-v2")
        );
        // Missing, and one rlord follows without hooks: no install-hooks step.
        f.agents[2]["too_old"] = Value::Null;
        f.agents[2]["installed"] = json!(false);
        let (why, _) = f.agent_blocker().unwrap();
        assert!(
            why.contains("isn't installed") && !why.contains("install-hooks"),
            "{why}"
        );
        f.set_agent("gemini");
        let (why, cmd) = f.agent_blocker().unwrap();
        assert!(why.contains("support coming") && cmd.is_none(), "{why}");
    }

    #[test]
    fn wrapped_rows_counts_word_wrapping() {
        assert_eq!(wrapped_rows("", 10), 1);
        assert_eq!(wrapped_rows("aaaa bbbb", 10), 1);
        // "aaaa bbbb cc" is 12 wide: "cc" moves to the next row.
        assert_eq!(wrapped_rows("aaaa bbbb cc", 10), 2);
        // A word too long for a row breaks across rows.
        assert_eq!(wrapped_rows(&"x".repeat(25), 10), 3);
    }
}
