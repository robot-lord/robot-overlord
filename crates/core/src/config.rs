//! User configuration (`config.toml`). Every key is optional; missing keys
//! take the defaults below, so an empty or absent file is a valid config.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// Session list on the left, preview on the right.
    #[default]
    Side,
    /// List on top, preview below.
    Stack,
}

/// TUI key bindings, as strings: a single character (`q`, `R`), a named key
/// (`enter`, `esc`, `tab`, `up`, `f5`, `space`), optionally prefixed with
/// `ctrl-` / `alt-`. Shift on letters is expressed by the uppercase letter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Keys {
    pub quit: String,
    pub up: String,
    pub down: String,
    pub top: String,
    pub bottom: String,
    pub attach: String,
    /// New session via the overlord: a request plus a few defaults.
    pub new: String,
    /// New session via the manual form (no overlord).
    pub new_manual: String,
    pub restart: String,
    pub stop: String,
    pub archive: String,
    pub unarchive: String,
    pub rename: String,
    pub pin: String,
    pub toggle_archived: String,
    pub sort: String,
    pub reverse_sort: String,
    pub filter: String,
    pub toggle_log: String,
    pub describe: String,
    pub notes: String,
    /// Show/hide the overlord overlay. Defaults to Alt-O (Option-O on a
    /// Mac), the same key
    /// that summons it from any pane.
    pub overlord: String,
    /// Switch between the sessions tab (flat list) and the groups tab.
    pub switch_tab: String,
    /// Step left / right along sessions tab ⇄ group list ⇄ inside a group.
    /// A user binding of the same key to another action wins.
    pub sessions_tab: String,
    pub groups_tab: String,
    /// Edit the selected session's tags (tags are the groups).
    pub tags: String,
    pub cancel: String,
    /// The cheat sheet: every key, and what to ask the overlord.
    pub help: String,
    /// The command launcher: the less frequent things, by name.
    pub command: String,
}

impl Default for Keys {
    fn default() -> Self {
        Keys {
            quit: "Q".into(),
            up: "k".into(),
            down: "j".into(),
            top: "g".into(),
            bottom: "G".into(),
            attach: "enter".into(),
            new: "n".into(),
            new_manual: "N".into(),
            restart: "r".into(),
            stop: "s".into(),
            archive: "x".into(),
            unarchive: "u".into(),
            rename: "R".into(),
            pin: "p".into(),
            toggle_archived: "a".into(),
            sort: "o".into(),
            reverse_sort: "O".into(),
            filter: "/".into(),
            toggle_log: "l".into(),
            describe: "D".into(),
            notes: "E".into(),
            overlord: "M-o".into(),
            switch_tab: "tab".into(),
            sessions_tab: "H".into(),
            groups_tab: "L".into(),
            tags: "#".into(),
            cancel: "esc".into(),
            help: "?".into(),
            command: ":".into(),
        }
    }
}

/// tmux root-table bindings rlord installs server-wide so they work inside
/// any agent pane. tmux key syntax (`C-q`, `M-]`, `F5`). Empty = disabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TmuxKeys {
    /// Jump back to the rlord TUI (or detach, if you attached from outside tmux).
    /// Default `M-d`: Option/Alt + a home-row key, since this is pressed constantly.
    pub back: String,
    /// Cycle to the next / previous live session without going home.
    pub next: String,
    pub prev: String,
    /// From any pane: jump to rlord with the overlord overlay open; in rlord:
    /// toggle the overlay.
    pub overlord: String,
    /// Agent sessions use your imported tmux profile (`rlord tmux import`):
    /// your prefix, bindings and status bar, in that session only. Turn it
    /// on/off per session with `rlord tmux profile` or `T` in the list.
    pub profile: bool,
    /// Mouse support in rlord's tmux server: clicks and the wheel reach
    /// the list, the overlay and agent panes (the wheel scrolls a pane's
    /// scrollback). With it on, selecting text by dragging needs Shift
    /// (Option in some macOS terminals). An imported profile's own `mouse`
    /// setting wins in agent sessions.
    pub mouse: bool,
}

impl Default for TmuxKeys {
    fn default() -> Self {
        TmuxKeys {
            back: "M-d".into(),
            next: String::new(),
            prev: String::new(),
            overlord: "M-o".into(),
            profile: true,
            mouse: true,
        }
    }
}

impl TmuxKeys {
    /// (key, nav verb) pairs for every enabled binding.
    pub fn bindings(&self) -> Vec<(&str, &'static str)> {
        [
            (&self.back, "home"),
            (&self.next, "next"),
            (&self.prev, "prev"),
            (&self.overlord, "overlord"),
        ]
        .into_iter()
        .filter(|(k, _)| !k.trim().is_empty())
        .map(|(k, v)| (k.as_str(), v))
        .collect()
    }
}

/// The overlord: the long-running session you talk to in order to drive
/// rlord itself (launch sessions, search, navigate, set views). One session:
/// a strong model at medium effort, which does light work itself and sends
/// anything heavier to a subagent or a new session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "OverlordToml")]
pub struct OverlordConfig {
    /// Coding tool. Only `claude` today.
    pub agent: String,
    /// Its model. Empty = the agent's own default.
    pub model: String,
    /// Its effort level (`low`..`max`; empty = the agent's default).
    pub effort: String,
    /// Restart an idle overlord on a fresh conversation after this many
    /// minutes without activity (while you're not looking at it), so its
    /// context doesn't keep growing. 0 = never.
    pub fresh_after_mins: u64,
    /// Size of the overlord overlay in the TUI: `90%` of the screen, or a
    /// cell count.
    pub width: String,
    pub height: String,
    /// Ask the user before each `send_to_session` (the overlord typing into
    /// another session). Off by default: like rlord's other tools, it runs
    /// unasked.
    pub confirm_sends: bool,
}

impl OverlordConfig {
    /// The overlord's default (model, effort) on `agent`: a strong model at
    /// medium effort. Claude: Opus / medium. Codex: its own default model with
    /// `model_reasoning_effort = "medium"`. OpenCode: its default model; it has
    /// no effort to set.
    pub fn defaults_for(agent: &str) -> (&'static str, &'static str) {
        match agent.trim() {
            "claude" | "" => ("opus", "medium"),
            "codex" => ("", "medium"),
            _ => ("", ""),
        }
    }

    /// The effort levels offered for `agent` ("" = the agent's default);
    /// empty when it has none to set.
    pub fn efforts_for(agent: &str) -> &'static [&'static str] {
        match agent.trim() {
            "claude" | "" => &["low", "medium", "high", "max"],
            "codex" => &["low", "medium", "high"],
            _ => &[],
        }
    }
}

/// `[overlord]` as written, which may be from when the overlord was two
/// sessions (a fast front desk on `model` / `effort`, and a mastermind on
/// `mastermind_model` / `mastermind_effort`). The mastermind is the overlord
/// now, so its keys carry over; see `From<OverlordToml>`. The daemon moves
/// them onto the plain keys once (`fold_mastermind_keys_in`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct OverlordToml {
    agent: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    mastermind_model: Option<String>,
    mastermind_effort: Option<String>,
    fresh_after_mins: Option<u64>,
    mastermind_fresh_after_mins: Option<u64>,
    #[serde(alias = "popup_width")]
    width: Option<String>,
    #[serde(alias = "popup_height")]
    height: Option<String>,
    confirm_sends: Option<bool>,
}

impl From<OverlordToml> for OverlordConfig {
    /// What the user wrote wins; only the defaults changed. An old
    /// `mastermind_*` key wins over its plain counterpart, whatever that
    /// says: the mastermind's settings are what the migrated overlord
    /// already runs on (the plain keys were the removed front desk's).
    fn from(t: OverlordToml) -> Self {
        let agent = t.agent.unwrap_or_else(|| "claude".into());
        let (model_dflt, effort_dflt) = OverlordConfig::defaults_for(&agent);
        OverlordConfig {
            model: t
                .mastermind_model
                .or(t.model)
                .unwrap_or_else(|| model_dflt.into()),
            effort: t
                .mastermind_effort
                .or(t.effort)
                .unwrap_or_else(|| effort_dflt.into()),
            fresh_after_mins: t
                .mastermind_fresh_after_mins
                .or(t.fresh_after_mins)
                .unwrap_or(15),
            width: t.width.unwrap_or_else(|| "90%".into()),
            height: t.height.unwrap_or_else(|| "85%".into()),
            confirm_sends: t.confirm_sends.unwrap_or(false),
            agent,
        }
    }
}

/// The two-session overlord's keys and the overlord's own they become.
const MASTERMIND_KEYS: &[(&str, &str)] = &[
    ("mastermind_model", "model"),
    ("mastermind_effort", "effort"),
    ("mastermind_fresh_after_mins", "fresh_after_mins"),
];

/// Move the old `mastermind_*` keys in the config file at `path` onto the
/// overlord's own (`model`, `effort`, `fresh_after_mins`), replacing those,
/// so later edits to them take effect. Keeps the rest of the file as
/// written; writes only when something moved, and only if what loads is
/// unchanged. Returns the keys moved (none: nothing to do, or no file).
pub fn fold_mastermind_keys_in(path: &std::path::Path) -> Result<Vec<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    match fold_mastermind_keys(&text)? {
        Some((out, moved)) => {
            write_config(path, &out)?;
            Ok(moved)
        }
        None => Ok(Vec::new()),
    }
}

/// `text` with the `mastermind_*` keys moved (see `fold_mastermind_keys_in`);
/// None when it has none.
fn fold_mastermind_keys(text: &str) -> Result<Option<(String, Vec<String>)>> {
    let mut doc: toml_edit::DocumentMut = text.parse().context("parsing config.toml")?;
    let has_old =
        |t: &dyn toml_edit::TableLike| MASTERMIND_KEYS.iter().any(|(o, _)| t.contains_key(o));
    if !doc
        .get("overlord")
        .and_then(|i| i.as_table_like())
        .is_some_and(has_old)
    {
        return Ok(None);
    }
    if let Some(t) = doc
        .get("overlord")
        .and_then(|i| i.as_inline_table())
        .cloned()
    {
        doc["overlord"] = toml_edit::Item::Table(t.into_table());
    }
    let table = doc["overlord"]
        .as_table_mut()
        .context("[overlord] isn't a table")?;
    let mut moved = Vec::new();
    for (old, new) in MASTERMIND_KEYS {
        let Some(mut v) = table.remove(old).and_then(|i| i.into_value().ok()) else {
            continue;
        };
        match table.get_mut(new).and_then(|i| i.as_value_mut()) {
            // Keep the line's own comment.
            Some(cur) => {
                *v.decor_mut() = cur.decor().clone();
                *cur = v;
            }
            None => table[new] = toml_edit::Item::Value(v),
        }
        moved.push(format!("{old} -> {new}"));
    }
    let out = doc.to_string();
    let before = toml::from_str::<Config>(text).context("config.toml doesn't load")?;
    let after = toml::from_str::<Config>(&out).context("the edited config doesn't load")?;
    let key = |c: &Config| {
        (
            c.overlord.model.clone(),
            c.overlord.effort.clone(),
            c.overlord.fresh_after_mins,
        )
    };
    if key(&before) != key(&after) {
        anyhow::bail!("moving the mastermind_* keys would change the overlord's settings");
    }
    Ok(Some((out, moved)))
}

impl Default for OverlordConfig {
    fn default() -> Self {
        OverlordToml::default().into()
    }
}

/// Defaults applied to every session created through rlord (TUI or CLI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionDefaults {
    /// Coding tool new sessions run: `claude`, `codex` or `opencode`.
    pub agent: String,
    /// Model passed to Claude Code (`--model`). Empty = its own default.
    pub model: String,
    /// Choices offered by the model picker for Claude Code.
    pub models: Vec<String>,
    /// Model passed to Codex (`--model`). Empty = Codex's own default. (Its
    /// picker choices come from Codex's own model catalog.)
    pub codex_model: String,
    /// Model set on OpenCode sessions (`provider/model`). Empty = whatever
    /// OpenCode picks. (Its picker choices come from `opencode models`.)
    pub opencode_model: String,
    /// Create each new session in its own git worktree under `workspace_dir`.
    pub workspace: bool,
    /// Directory (relative to the repo root) that holds session worktrees.
    pub workspace_dir: String,
    /// Repo preselected in the new-session form. Empty = the current directory.
    pub default_repo: String,
    /// Directories scanned (to `repo_scan_depth`) by the repo finder.
    pub repo_roots: Vec<String>,
    pub repo_scan_depth: u8,
    /// Claude Code's renderer for rlord's sessions (`--settings
    /// {"tui": …}`): "default" draws inline, so tmux keeps the scrollback
    /// (copy-mode scrolling and search work); "fullscreen" keeps it inside
    /// Claude. "" leaves Claude's own setting.
    pub claude_tui: String,
}

impl Default for SessionDefaults {
    fn default() -> Self {
        SessionDefaults {
            agent: "claude".into(),
            model: "opus".into(),
            models: vec!["opus".into(), "sonnet".into(), "haiku".into()],
            codex_model: String::new(),
            opencode_model: String::new(),
            workspace: true,
            workspace_dir: ".worktrees".into(),
            default_repo: String::new(),
            repo_roots: vec!["~".into()],
            repo_scan_depth: 3,
            claude_tui: "default".into(),
        }
    }
}

impl SessionDefaults {
    /// The default model for sessions of `agent` ("" = the agent's own).
    pub fn model_for(&self, agent: crate::agents::Agent) -> &str {
        match agent {
            crate::agents::Agent::Claude => &self.model,
            crate::agents::Agent::Codex => &self.codex_model,
            crate::agents::Agent::OpenCode => &self.opencode_model,
        }
    }

    /// The model picker's choices for `agent`.
    pub fn models_for(&self, agent: crate::agents::Agent) -> Vec<String> {
        match agent {
            crate::agents::Agent::Claude => self.models.clone(),
            crate::agents::Agent::Codex => {
                crate::agents::codex::models(&crate::agents::codex::home())
            }
            // (As last listed: see `install::list_models`.)
            crate::agents::Agent::OpenCode => crate::agents::opencode::models(),
        }
    }

    /// `default_repo` with `~` expanded; None when unset.
    pub fn default_repo_path(&self) -> Option<String> {
        let r = self.default_repo.trim();
        if r.is_empty() {
            return None;
        }
        Some(expand_tilde(r))
    }
}

pub fn expand_tilde(p: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return p.to_string();
    };
    let home = home.to_string_lossy();
    if p == "~" {
        return home.into_owned();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    p.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub layout: Layout,
    /// Width of the session list in `side` layout, percent of the terminal.
    pub list_width_pct: u16,
    /// How often the pane preview re-captures, in milliseconds.
    pub preview_refresh_ms: u64,
    /// Digits 1–9 attach straight to the Nth listed session.
    pub jump_digits: bool,
    /// Height of the details pane under the preview (rows).
    pub details_height: u16,
    /// What the list's Usage column shows (and the usage sort sorts by):
    /// turns, tokens or context (% of the context window).
    pub usage: crate::usage::UsageMetric,
    /// Color theme: a family (follows the system's light/dark mode) or one
    /// variant. See `rlord_core::themes`.
    pub theme: String,
    pub session: SessionDefaults,
    pub overlord: OverlordConfig,
    pub keys: Keys,
    pub tmux: TmuxKeys,
    pub archive: ArchiveConfig,
    pub update: UpdateConfig,
}

/// `rlord update`: which releases it installs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// `stable` (X.Y.Z, every platform) or `dev`: the newest release of
    /// either kind, development builds (X.Y.Z-dev.N, macOS only) included.
    pub channel: String,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig {
            channel: "stable".into(),
        }
    }
}

/// Archiving: a summary of what the session did, made once.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ArchiveConfig {
    /// Make a summary when a session is archived (in the background).
    pub summary: bool,
    /// The model that writes a Claude Code session's: a cheap one is plenty.
    pub summary_model: String,
    /// The model that writes a Codex session's (a session's own agent
    /// summarizes it). Empty = Codex's default.
    pub codex_summary_model: String,
    /// The model that writes an OpenCode session's when it is summarized
    /// from excerpts (`provider/model`; empty = OpenCode's default). A
    /// conversation that fits is summarized by the session's own model.
    pub opencode_summary_model: String,
    /// A conversation bigger than this (tokens) is summarized from excerpts
    /// instead of in full.
    pub summary_max_tokens: u64,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        ArchiveConfig {
            summary: true,
            summary_model: "haiku".into(),
            codex_summary_model: String::new(),
            opencode_summary_model: String::new(),
            summary_max_tokens: 150_000,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            layout: Layout::Side,
            list_width_pct: 50,
            preview_refresh_ms: 500,
            jump_digits: true,
            details_height: 12,
            usage: Default::default(),
            theme: "rlord".into(),
            session: SessionDefaults::default(),
            overlord: OverlordConfig::default(),
            keys: Keys::default(),
            tmux: TmuxKeys::default(),
            archive: ArchiveConfig::default(),
            update: UpdateConfig::default(),
        }
    }
}

impl Config {
    /// Load the user config; a missing file is the default config, a
    /// malformed one is an error (never silently ignored).
    pub fn load() -> Result<Self> {
        let path = paths::config_path();
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Set one string value (`[section] field = value`) in the user's config
    /// file, keeping everything else in it as written: comments, order,
    /// other values. The result must still load, or nothing is written.
    pub fn set_value(section: &str, field: &str, value: &str) -> Result<()> {
        let path = paths::config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        write_config(&path, &set_in(&text, section, field, value)?)
    }

    /// Save the overlord's agent, model and effort (the setup dialog's
    /// choice) under `[overlord]`, keeping the rest of the file as written.
    /// Keys from the two-session overlord (`mastermind_model`,
    /// `mastermind_effort`) are dropped: the values chosen here replace them.
    pub fn set_overlord(agent: &str, model: &str, effort: &str) -> Result<()> {
        let path = paths::config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        write_config(&path, &set_overlord_in(&text, agent, model, effort)?)
    }

    /// Commented template written by `rlord config init`; equals the defaults.
    pub fn default_toml() -> String {
        DEFAULT_TOML.to_string()
    }
}

/// Write the config file's new text (already checked to load).
fn write_config(path: &std::path::Path, out: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // A symlinked config (a dotfiles repo) is written where it points.
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, out)?;
    // Keep the file's own permissions (a 0600 config stays 0600).
    if let Ok(m) = std::fs::metadata(&path) {
        let _ = std::fs::set_permissions(&tmp, m.permissions());
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// `text` with the overlord's agent, model and effort set, and the old
/// two-session keys they replace removed (see `Config::set_overlord`).
fn set_overlord_in(text: &str, agent: &str, model: &str, effort: &str) -> Result<String> {
    let mut out = text.to_string();
    for (field, value) in [("agent", agent), ("model", model), ("effort", effort)] {
        out = set_in(&out, "overlord", field, value)?;
    }
    let mut doc: toml_edit::DocumentMut = out.parse().context("parsing the edited config")?;
    if let Some(t) = doc["overlord"].as_table_mut() {
        t.remove("mastermind_model");
        t.remove("mastermind_effort");
    }
    let out = doc.to_string();
    let c = toml::from_str::<Config>(&out).context("the edited config doesn't load")?;
    // What was chosen is what loads (nothing left over outranks it).
    if (
        c.overlord.agent.as_str(),
        c.overlord.model.as_str(),
        c.overlord.effort.as_str(),
    ) != (agent, model, effort)
    {
        anyhow::bail!("the overlord's settings didn't take in config.toml");
    }
    Ok(out)
}

/// `text` with `[section] field = value` set (see `Config::set_value`).
fn set_in(text: &str, section: &str, field: &str, value: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", paths::config_path().display()))?;
    // `keys = { … }` becomes a `[keys]` table with the same values.
    if let Some(t) = doc.get(section).and_then(|i| i.as_inline_table()).cloned() {
        doc[section] = toml_edit::Item::Table(t.into_table());
    }
    if !doc.contains_table(section) {
        doc[section] = toml_edit::table();
    }
    let table = doc[section]
        .as_table_mut()
        .with_context(|| format!("[{section}] isn't a table"))?;
    match table.get_mut(field).and_then(|i| i.as_value_mut()) {
        // Keep the line's own comment.
        Some(v) => {
            let decor = v.decor().clone();
            *v = toml_edit::Value::from(value);
            *v.decor_mut() = decor;
        }
        None => table[field] = toml_edit::value(value),
    }
    let out = doc.to_string();
    toml::from_str::<Config>(&out).context("the edited config doesn't load")?;
    Ok(out)
}

const DEFAULT_TOML: &str = r##"# Robot Overlord configuration. Every key is optional.

# "side": session list left, live preview right.  "stack": list above preview.
layout = "side"
list_width_pct = 50
preview_refresh_ms = 500
jump_digits = true      # 1-9 attach to the Nth listed session
details_height = 12     # rows for the details pane under the preview
usage = "turns"         # the list's Usage column: "turns" (prompts you typed),
                        # "tokens", or "context" (% of the context window)

# Color theme. A family follows your system's light/dark mode:
#   rlord, catppuccin, tokyonight, gruvbox, solarized
# or pin one variant: rlord-dark, rlord-light, catppuccin-mocha,
# catppuccin-latte, tokyonight-night, tokyonight-day, gruvbox-dark,
# gruvbox-light, solarized-dark, solarized-light, nord, dracula.
# The named themes need a truecolor terminal (kitty, iTerm2, Ghostty, WezTerm).
theme = "rlord"

# Defaults for every session you create (TUI form or `rlord new`).
[session]
agent = "claude"                # what new sessions run: "claude", "codex"
                                # or "opencode"
model = "opus"                  # Claude Code's --model; "" = its default
models = ["opus", "sonnet", "haiku"]   # model picker choices (Claude Code)
codex_model = ""                # Codex's --model; "" = its default (the
                                # picker offers Codex's own model list)
opencode_model = ""             # OpenCode's model, as provider/model; "" =
                                # its default (the picker: `opencode models`)
workspace = true                # each session gets its own git worktree...
workspace_dir = ".worktrees"    # ...under <repo>/.worktrees/<branch>
default_repo = ""               # e.g. "~/src/myrepo"; "" = current directory
repo_roots = ["~"]              # where the repo finder looks for git repos...
repo_scan_depth = 3             # ...and how deep (hidden/heavy dirs skipped)
claude_tui = "default"          # Claude's renderer here: "default" (inline: tmux
                                # scrollback and copy-mode search work),
                                # "fullscreen", or "" for Claude's own setting

# The overlord: a long-running session you talk to in order to drive rlord —
# launch sessions, search, jump around, set the list's sort/filter. It gets
# rlord's MCP tools; add your own standing instructions in
# ~/.config/robot-overlord/overlord.md.
# One session (Alt-O; Option-O on a Mac): a strong model at medium effort.
# It does light work itself (look-ups, small edits, short answers) and sends
# anything heavier to a subagent or a new session.
[overlord]
agent = "claude"
model = "opus"
effort = "medium"
fresh_after_mins = 15   # fresh conversation after this idle; 0 = never
width = "90%"           # size of the overlord overlay in the TUI
height = "85%"          # (percent of the screen, or a cell count)
confirm_sends = false   # ask you before it types into another session

# Easiest: ask your overlord to change a key (or `:keys` in the list). It
# opens the key screen, which tests that the key reaches rlord and checks it
# against your tmux, shell and Claude Code before saving it here.
#
# TUI keys. A character ("q", "R"), a named key ("enter", "esc", "tab", "up",
# "down", "space", "backspace", "f1".."f12"), optionally "ctrl-"/"alt-" prefixed.
[keys]
quit = "Q"              # Shift-Q, then confirm; plain q just shows a hint
up = "k"
down = "j"
top = "g"
bottom = "G"
attach = "enter"
new = "n"               # new session: describe it, the overlord launches it
new_manual = "N"        # new session: the manual form
restart = "r"
stop = "s"
archive = "x"
unarchive = "u"
rename = "R"
pin = "p"
toggle_archived = "a"
sort = "o"
reverse_sort = "O"
filter = "/"
toggle_log = "l"
describe = "D"          # edit the one-line description
notes = "E"             # edit notes in $EDITOR
overlord = "M-o"        # show/hide the overlord (same as Alt-O everywhere)
switch_tab = "tab"      # sessions tab (flat list) <-> groups tab (sessions by tag)
sessions_tab = "H"      # step left:  sessions tab <- group list <- inside a group
groups_tab = "L"        # step right: sessions tab -> group list -> inside a group
tags = "#"              # edit the selected session's tags (tags are the groups)
cancel = "esc"
help = "?"              # cheat sheet: every key, and what to ask the overlord
command = ":"           # command launcher (keys, sort, log, archived, …)

# Keys bound in rlord's own tmux server (tmux key syntax), so they work from
# inside any agent pane. rlord runs a private tmux (`tmux -L rlord`, no status
# bar, no prefix): these are the only keys it binds, and your own tmux is
# never touched. Set a key to "" to disable it.
# (On macOS make Option send Alt — kitty: `macos_option_as_alt both`.)
[tmux]
back = "M-d"    # back to the rlord TUI (or detach if you attached from outside tmux)
next = ""       # e.g. "M-g"
prev = ""       # e.g. "M-s"
overlord = "M-o" # from any pane: the overlord over it
profile = true   # agent sessions use your tmux (after `rlord tmux import`)
mouse = true     # click and scroll in the list, the overlay and agent panes
                 # (drag-select then needs Shift; Option in some macOS
                 # terminals). An imported profile's own setting wins in
                 # agent sessions.

# Archiving a session writes a short summary of what it did (once, in the
# background, on a cheap model), shown and searched with archived sessions.
# A session's own agent writes it.
[archive]
summary = true
summary_model = "haiku"         # for Claude Code sessions
codex_summary_model = ""        # for Codex sessions; "" = Codex's default
opencode_summary_model = ""     # for OpenCode sessions too big to summarize
                                # in full (those that fit use their own model)
summary_max_tokens = 150000   # bigger conversations: summarized from excerpts

# `rlord update`: "stable" releases, or "dev" for the newest of any kind
# (development builds, macOS only, included).
[update]
channel = "stable"
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_in_keeps_the_file_as_written() {
        let t = "# mine\n[tmux]\nback = \"M-d\"   # home\nnext = \"\"\n";
        let out = set_in(t, "tmux", "back", "M-h").unwrap();
        assert_eq!(
            out,
            "# mine\n[tmux]\nback = \"M-h\"   # home\nnext = \"\"\n"
        );
        let out = set_in("", "keys", "help", "F1").unwrap();
        assert!(out.contains("[keys]") && out.contains("help = \"F1\""));
        // Commented-out template sections gain a real table.
        let out = set_in(&Config::default_toml(), "tmux", "next", "M-g").unwrap();
        let c: Config = toml::from_str(&out).unwrap();
        assert_eq!(c.tmux.next, "M-g");
        assert_eq!(c.tmux.back, "M-d");
        assert!(set_in(t, "keys", "quit", "x").is_ok());
        // An inline table keeps its values.
        let out = set_in(
            "keys = { quit = \"x\", new = \"a\" }\n",
            "keys",
            "help",
            "F1",
        )
        .unwrap();
        let c: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            (
                c.keys.quit.as_str(),
                c.keys.new.as_str(),
                c.keys.help.as_str()
            ),
            ("x", "a", "F1")
        );
    }

    #[test]
    fn tilde_expansion() {
        std::env::set_var("HOME", "/h");
        assert_eq!(expand_tilde("~"), "/h");
        assert_eq!(expand_tilde("~/src"), "/h/src");
        assert_eq!(expand_tilde("/abs"), "/abs");
        assert_eq!(expand_tilde("~user"), "~user");
    }

    #[test]
    fn template_parses_to_defaults() {
        let c: Config = toml::from_str(&Config::default_toml()).unwrap();
        assert_eq!(c.keys.quit, Keys::default().quit);
        assert_eq!(c.tmux.back, "M-d");
        assert_eq!(c.layout, Layout::Side);
        assert_eq!(c.preview_refresh_ms, 500);
        assert!(c.session.workspace);
        assert_eq!(c.session.workspace_dir, ".worktrees");
        assert_eq!(c.overlord.model, "opus");
        assert_eq!(c.overlord.effort, "medium");
        assert_eq!(c.overlord.fresh_after_mins, 15);
        assert_eq!(c.tmux.overlord, "M-o");
        assert_eq!(c.keys.overlord, "M-o");
    }

    #[test]
    fn partial_config_fills_defaults_and_disables_keys() {
        let c: Config =
            toml::from_str("layout = \"stack\"\n[tmux]\nback = \"\"\n[keys]\nquit = \"Q\"\n")
                .unwrap();
        assert_eq!(c.layout, Layout::Stack);
        assert_eq!(c.keys.quit, "Q");
        assert_eq!(c.keys.attach, "enter");
        // back disabled, next/prev off by default; the overlord remains
        assert_eq!(c.tmux.bindings(), [("M-o", "overlord")]);
    }

    /// A config from the two-seat overlord: the mastermind's keys become
    /// the overlord's; what the user set for the overlord itself wins.
    #[test]
    fn old_overlord_keys_map_onto_the_one_overlord() {
        let o = |t: &str| toml::from_str::<Config>(t).unwrap().overlord;
        // Nothing set: the new defaults.
        let d = o("");
        assert_eq!((d.model.as_str(), d.effort.as_str()), ("opus", "medium"));
        // The old template, as written: the mastermind carries over.
        let t = o("[overlord]\nmodel = \"sonnet\"\neffort = \"low\"\n\
                   mastermind_model = \"opus\"\nmastermind_effort = \"medium\"\n\
                   mastermind_fresh_after_mins = 30\n");
        assert_eq!((t.model.as_str(), t.effort.as_str()), ("opus", "medium"));
        assert_eq!(t.fresh_after_mins, 30);
        // Only the old keys.
        let m = o("[overlord]\nmastermind_model = \"fable\"\nmastermind_effort = \"high\"\n");
        assert_eq!((m.model.as_str(), m.effort.as_str()), ("fable", "high"));
        // An old key wins over the plain one, whatever that says: it is
        // what the migrated overlord runs on.
        let e = o("[overlord]\nmodel = \"haiku\"\neffort = \"max\"\n\
                   mastermind_model = \"opus\"\nmastermind_effort = \"medium\"\n\
                   fresh_after_mins = 0\nmastermind_fresh_after_mins = 30\n");
        assert_eq!((e.model.as_str(), e.effort.as_str()), ("opus", "medium"));
        assert_eq!(e.fresh_after_mins, 30);
        // On its own, sonnet is a choice.
        assert_eq!(o("[overlord]\nmodel = \"sonnet\"\n").model, "sonnet");
        // Another agent: its own defaults; an old tmux key is ignored.
        let c = o("[overlord]\nagent = \"codex\"\n[tmux]\nmastermind = \"M-m\"\n");
        assert_eq!((c.model.as_str(), c.effort.as_str()), ("", "medium"));
        assert_eq!(OverlordConfig::defaults_for("opencode"), ("", ""));
        assert!(OverlordConfig::efforts_for("opencode").is_empty());
    }

    /// The daemon moves the old keys onto the plain ones once; after that,
    /// editing `model` / `effort` takes effect.
    #[test]
    fn mastermind_keys_are_folded_in_once() {
        let old = "# mine\n[overlord]\nmodel = \"sonnet\"   # front desk\neffort = \"low\"\n\
                   mastermind_model = \"opus\"\nmastermind_effort = \"medium\"\n\
                   mastermind_fresh_after_mins = 30\nwidth = \"80%\"\n";
        let (out, moved) = fold_mastermind_keys(old).unwrap().expect("keys moved");
        assert_eq!(moved.len(), 3);
        assert!(!out.contains("mastermind"), "{out}");
        assert!(
            out.starts_with("# mine\n") && out.contains("# front desk"),
            "{out}"
        );
        let c: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            (c.overlord.model.as_str(), c.overlord.effort.as_str()),
            ("opus", "medium")
        );
        assert_eq!(
            (c.overlord.fresh_after_mins, c.overlord.width.as_str()),
            (30, "80%")
        );
        // Nothing left to move.
        assert!(fold_mastermind_keys(&out).unwrap().is_none());
        assert!(fold_mastermind_keys("").unwrap().is_none());
        // Afterwards an edit to `model` takes.
        let edited = set_in(&out, "overlord", "model", "sonnet").unwrap();
        assert_eq!(
            toml::from_str::<Config>(&edited).unwrap().overlord.model,
            "sonnet"
        );
        // Only an old key, no plain one: it is added.
        let (out, _) = fold_mastermind_keys("[overlord]\nmastermind_effort = \"high\"\n")
            .unwrap()
            .unwrap();
        assert_eq!(
            toml::from_str::<Config>(&out).unwrap().overlord.effort,
            "high"
        );
        // On disk: written in place; a missing file is nothing to do.
        let dir = std::env::temp_dir().join(format!("rlord-fold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        assert!(fold_mastermind_keys_in(&p).unwrap().is_empty());
        std::fs::write(&p, old).unwrap();
        assert_eq!(fold_mastermind_keys_in(&p).unwrap().len(), 3);
        assert!(!std::fs::read_to_string(&p).unwrap().contains("mastermind"));
        assert!(fold_mastermind_keys_in(&p).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The setup dialog's choice is what loads afterwards, whatever old
    /// keys the file had; the rest of the file stays as written.
    #[test]
    fn the_overlords_choice_is_saved_over_old_keys() {
        let old = "# mine\n[overlord]\nmodel = \"sonnet\"   # front desk\neffort = \"low\"\n\
                   mastermind_model = \"opus\"\nmastermind_effort = \"medium\"\nwidth = \"80%\"\n";
        // Low, picked over the medium that carried over: it must stick.
        let out = set_overlord_in(old, "claude", "opus", "low").unwrap();
        let c: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            (c.overlord.model.as_str(), c.overlord.effort.as_str()),
            ("opus", "low")
        );
        assert_eq!(c.overlord.width, "80%");
        assert!(out.starts_with("# mine\n") && out.contains("# front desk"));
        assert!(!out.contains("mastermind"));
        // An empty file gains the section.
        let out = set_overlord_in("", "claude", "sonnet", "").unwrap();
        let c: Config = toml::from_str(&out).unwrap();
        assert_eq!(
            (c.overlord.model.as_str(), c.overlord.effort.as_str()),
            ("sonnet", "")
        );
    }
}
