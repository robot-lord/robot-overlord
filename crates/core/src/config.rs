//! User configuration (`config.toml`). Every key is optional; missing keys
//! take the defaults below, so an empty or absent file is a valid config.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// Session list on the left, preview on the right (other-tool style).
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
    /// Show/hide the overlord overlay. Defaults to Option-O, the same key
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
    /// The same for the mastermind (the overlord's second, deeper session).
    pub mastermind: String,
    /// Agent sessions use your imported tmux profile (`rlord tmux import`):
    /// your prefix, bindings and status bar, in that session only. Turn it
    /// on/off per session with `rlord tmux profile` or `T` in the list.
    pub profile: bool,
}

impl Default for TmuxKeys {
    fn default() -> Self {
        TmuxKeys {
            back: "M-d".into(),
            next: String::new(),
            prev: String::new(),
            overlord: "M-o".into(),
            mastermind: "M-m".into(),
            profile: true,
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
            (&self.mastermind, "mastermind"),
        ]
        .into_iter()
        .filter(|(k, _)| !k.trim().is_empty())
        .map(|(k, v)| (k.as_str(), v))
        .collect()
    }
}

/// The overlord: the long-running session you talk to in order to drive
/// rlord itself (launch sessions, search, navigate, set views).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlordConfig {
    /// Coding tool. Only `claude` today.
    pub agent: String,
    /// Model for the overlord: the quick front desk you summon with ⌥O.
    /// It does mechanical work itself and hands anything that needs thought
    /// to the mastermind, so a fast model fits. Empty = the agent's default.
    pub model: String,
    /// Its effort level (`low`..`max`; empty = the agent's default).
    pub effort: String,
    /// Model for the mastermind: the second overlord session that launches
    /// sessions and takes one-off questions — where judgment pays.
    pub mastermind_model: String,
    pub mastermind_effort: String,
    /// Restart an idle mastermind on a fresh conversation after this many
    /// minutes without activity (while you're not looking at it), so its
    /// context doesn't keep growing. 0 = never.
    pub mastermind_fresh_after_mins: u64,
    /// Size of the overlord overlay in the TUI: `90%` of the screen, or a
    /// cell count.
    #[serde(alias = "popup_width")]
    pub width: String,
    #[serde(alias = "popup_height")]
    pub height: String,
}

impl Default for OverlordConfig {
    fn default() -> Self {
        OverlordConfig {
            agent: "claude".into(),
            model: "sonnet".into(),
            effort: "low".into(),
            mastermind_model: "opus".into(),
            mastermind_effort: "medium".into(),
            mastermind_fresh_after_mins: 15,
            width: "90%".into(),
            height: "85%".into(),
        }
    }
}

/// Defaults applied to every session created through rlord (TUI or CLI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionDefaults {
    /// Coding tool. Only `claude` today.
    pub agent: String,
    /// Model passed to the agent (`--model`). Empty = the agent's own default.
    pub model: String,
    /// Choices offered by the model picker.
    pub models: Vec<String>,
    /// Create each new session in its own git worktree under `workspace_dir`.
    pub workspace: bool,
    /// Directory (relative to the repo root) that holds session worktrees.
    pub workspace_dir: String,
    /// Repo preselected in the new-session form. Empty = the current directory.
    pub default_repo: String,
    /// Directories scanned (to `repo_scan_depth`) by the repo finder.
    pub repo_roots: Vec<String>,
    pub repo_scan_depth: u8,
}

impl Default for SessionDefaults {
    fn default() -> Self {
        SessionDefaults {
            agent: "claude".into(),
            model: "opus".into(),
            models: vec!["opus".into(), "sonnet".into(), "haiku".into()],
            workspace: true,
            workspace_dir: ".worktrees".into(),
            default_repo: String::new(),
            repo_roots: vec!["~".into()],
            repo_scan_depth: 3,
        }
    }
}

impl SessionDefaults {
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
    /// Color theme: a family (follows the system's light/dark mode) or one
    /// variant. See `rlord_core::themes`.
    pub theme: String,
    pub session: SessionDefaults,
    pub overlord: OverlordConfig,
    pub keys: Keys,
    pub tmux: TmuxKeys,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            layout: Layout::Side,
            list_width_pct: 50,
            preview_refresh_ms: 500,
            jump_digits: true,
            details_height: 12,
            theme: "rlord".into(),
            session: SessionDefaults::default(),
            overlord: OverlordConfig::default(),
            keys: Keys::default(),
            tmux: TmuxKeys::default(),
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

    /// Commented template written by `rlord config init`; equals the defaults.
    pub fn default_toml() -> String {
        DEFAULT_TOML.to_string()
    }
}

const DEFAULT_TOML: &str = r##"# Robot Overlord configuration. Every key is optional.

# "side": session list left, live preview right.  "stack": list above preview.
layout = "side"
list_width_pct = 50
preview_refresh_ms = 500
jump_digits = true      # 1-9 attach to the Nth listed session
details_height = 12     # rows for the details pane under the preview

# Color theme. A family follows your system's light/dark mode:
#   rlord, catppuccin, tokyonight, gruvbox, solarized
# or pin one variant: rlord-dark, rlord-light, catppuccin-mocha,
# catppuccin-latte, tokyonight-night, tokyonight-day, gruvbox-dark,
# gruvbox-light, solarized-dark, solarized-light, nord, dracula.
# The named themes need a truecolor terminal (kitty, iTerm2, Ghostty, WezTerm).
theme = "rlord"

# Defaults for every session you create (TUI form or `rlord new`).
[session]
agent = "claude"
model = "opus"                  # passed as --model; "" = the agent's default
models = ["opus", "sonnet", "haiku"]   # model picker choices
workspace = true                # each session gets its own git worktree...
workspace_dir = ".worktrees"    # ...under <repo>/.worktrees/<branch>
default_repo = ""               # e.g. "~/src/myrepo"; "" = current directory
repo_roots = ["~"]              # where the repo finder looks for git repos...
repo_scan_depth = 3             # ...and how deep (hidden/heavy dirs skipped)

# The overlord: a long-running session you talk to in order to drive rlord —
# launch sessions, search, jump around, set the list's sort/filter. It gets
# rlord's MCP tools; add your own standing instructions in
# ~/.config/robot-overlord/overlord.md.
# The overlord is two sessions. The overlord (Option-O) is a quick front
# desk: it navigates, organizes and answers from rlord's tools itself, and
# hands anything that needs thought to the mastermind (Option-M), which
# launches sessions and takes one-off questions. Fast model up front, a
# strong one behind.
[overlord]
agent = "claude"
model = "sonnet"
effort = "low"
mastermind_model = "opus"
mastermind_effort = "medium"
mastermind_fresh_after_mins = 15   # fresh conversation after this idle; 0 = never
width = "90%"           # size of the overlord overlay in the TUI
height = "85%"          # (percent of the screen, or a cell count)

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
overlord = "M-o"        # show/hide the overlord (same as Option-O everywhere)
switch_tab = "tab"      # sessions tab (flat list) <-> groups tab (sessions by tag)
sessions_tab = "H"      # step left:  sessions tab <- group list <- inside a group
groups_tab = "L"        # step right: sessions tab -> group list -> inside a group
tags = "#"              # edit the selected session's tags (tags are the groups)
cancel = "esc"

# Keys bound in rlord's own tmux server (tmux key syntax), so they work from
# inside any agent pane. rlord runs a private tmux (`tmux -L rlord`, no status
# bar, no prefix): these are the only keys it binds, and your own tmux is
# never touched. Set a key to "" to disable it.
# (On macOS make Option send Alt — kitty: `macos_option_as_alt both`.)
[tmux]
back = "M-d"    # back to the rlord TUI (or detach if you attached from outside tmux)
next = ""       # e.g. "M-g"
prev = ""       # e.g. "M-s"
overlord = "M-o" # from any pane: the overlord (quick front desk) over it
mastermind = "M-m" # from any pane: the mastermind (launches, deeper questions)
profile = true   # agent sessions use your tmux (after `rlord tmux import`)
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        std::env::set_var("HOME", "/h");
        assert_eq!(expand_tilde("~"), "/h");
        assert_eq!(expand_tilde("~/ko"), "/h/ko");
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
        assert_eq!(c.overlord.model, "sonnet");
        assert_eq!(c.overlord.effort, "low");
        assert_eq!(c.overlord.mastermind_model, "opus");
        assert_eq!(c.overlord.mastermind_effort, "medium");
        assert_eq!(c.tmux.mastermind, "M-m");
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
        // back disabled, next/prev off by default; the overlord keys remain
        assert_eq!(
            c.tmux.bindings(),
            [("M-o", "overlord"), ("M-m", "mastermind")]
        );
    }
}
