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
    pub new: String,
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
    pub cancel: String,
}

impl Default for Keys {
    fn default() -> Self {
        Keys {
            quit: "q".into(),
            up: "k".into(),
            down: "j".into(),
            top: "g".into(),
            bottom: "G".into(),
            attach: "enter".into(),
            new: "n".into(),
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
}

impl Default for TmuxKeys {
    fn default() -> Self {
        TmuxKeys {
            back: "M-d".into(),
            next: String::new(),
            prev: String::new(),
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
        ]
        .into_iter()
        .filter(|(k, _)| !k.trim().is_empty())
        .map(|(k, v)| (k.as_str(), v))
        .collect()
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
    pub session: SessionDefaults,
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
            session: SessionDefaults::default(),
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

# TUI keys. A character ("q", "R"), a named key ("enter", "esc", "tab", "up",
# "down", "space", "backspace", "f1".."f12"), optionally "ctrl-"/"alt-" prefixed.
[keys]
quit = "q"
up = "k"
down = "j"
top = "g"
bottom = "G"
attach = "enter"
new = "n"
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
cancel = "esc"

# Global tmux bindings rlord installs (root key table, tmux key syntax) so they
# work from inside any agent pane. Set a key to "" to disable it.
# `back` is the one you press constantly: Option/Alt + d, a home-row key.
# (On macOS make Option send Alt — kitty: `macos_option_as_alt both`.)
# next/prev cycle through live sessions in list order; off by default because
# jumping by search (`/` + Enter) or digit is usually what you actually want.
# rlord will not override your tmux prefix or a key you already bind in the
# root table; it warns and skips that key instead.
[tmux]
back = "M-d"    # back to the rlord TUI (or detach if you attached from outside tmux)
next = ""       # e.g. "M-g"
prev = ""       # e.g. "M-s"
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
    }

    #[test]
    fn partial_config_fills_defaults_and_disables_keys() {
        let c: Config =
            toml::from_str("layout = \"stack\"\n[tmux]\nback = \"\"\n[keys]\nquit = \"Q\"\n")
                .unwrap();
        assert_eq!(c.layout, Layout::Stack);
        assert_eq!(c.keys.quit, "Q");
        assert_eq!(c.keys.attach, "enter");
        assert!(c.tmux.bindings().is_empty()); // back disabled, next/prev off by default
    }
}
