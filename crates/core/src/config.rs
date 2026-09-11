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
    pub filter: String,
    pub toggle_log: String,
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
            filter: "/".into(),
            toggle_log: "l".into(),
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
            next: "M-g".into(),
            prev: "M-s".into(),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub layout: Layout,
    /// Width of the session list in `side` layout, percent of the terminal.
    pub list_width_pct: u16,
    /// How often the pane preview re-captures, in milliseconds.
    pub preview_refresh_ms: u64,
    pub keys: Keys,
    pub tmux: TmuxKeys,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            layout: Layout::Side,
            list_width_pct: 50,
            preview_refresh_ms: 500,
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
filter = "/"
toggle_log = "l"
cancel = "esc"

# Global tmux bindings rlord installs (root key table, tmux key syntax) so they
# work from inside any agent pane. Set a key to "" to disable it.
# Defaults are left-hand home-row keys: Option/Alt + s d g (prev / home / next).
# (On macOS make Option send Alt — kitty: `macos_option_as_alt both`.)
# rlord will not override your tmux prefix or a key you already bind in the
# root table; it warns and skips that key instead.
[tmux]
back = "M-d"    # back to the rlord TUI (or detach if you attached from outside tmux)
next = "M-g"    # cycle to the next live session
prev = "M-s"    # cycle to the previous live session
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_defaults() {
        let c: Config = toml::from_str(&Config::default_toml()).unwrap();
        assert_eq!(c.keys.quit, Keys::default().quit);
        assert_eq!(c.tmux.back, "M-d");
        assert_eq!(c.layout, Layout::Side);
        assert_eq!(c.preview_refresh_ms, 500);
    }

    #[test]
    fn partial_config_fills_defaults_and_disables_keys() {
        let c: Config =
            toml::from_str("layout = \"stack\"\n[tmux]\nback = \"\"\n[keys]\nquit = \"Q\"\n")
                .unwrap();
        assert_eq!(c.layout, Layout::Stack);
        assert_eq!(c.keys.quit, "Q");
        assert_eq!(c.keys.attach, "enter");
        let b = c.tmux.bindings();
        assert_eq!(b.len(), 2);
        assert!(b.iter().all(|(_, v)| *v != "home"));
    }
}
