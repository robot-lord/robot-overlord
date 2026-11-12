//! Every key rlord binds, and what stands in a key's way.
//!
//! The key screen (TUI) and the overlord's `keys_list` tool share this:
//! one catalogue of rlord's bindings (`SLOTS`, read from and written to
//! `config.toml`), one key notation that reads both of the config's
//! spellings (`alt-x` in `[keys]`, tmux's `M-x` in `[tmux]`), and the checks
//! that say whether a key will do what the user wants:
//!
//! - **taken**: another rlord key in the same place, the user's tmux prefix,
//!   or the user's own tmux binding in rlord's root table. Saving it would
//!   break something, so the key screen won't.
//! - **shadows**: a global key also means something to Claude Code, the
//!   shell or the user's tmux config inside agent sessions; rlord takes it
//!   from them. Worth knowing, not wrong.
//!
//! What can't be seen from here (the terminal, macOS, other apps' global
//! hotkeys) is what the key screen's live test is for: a key that never
//! arrives is taken by one of them.

use std::path::Path;

use serde_json::Value;

use crate::config::Config;

/// Where a key works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Bound in rlord's tmux server: works in every agent session and the
    /// list (tmux sees it first).
    Global,
    /// The session list's own keys.
    List,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::Global => "anywhere",
            Scope::List => "list",
        }
    }
}

/// One of rlord's bindings: `[section] field` in `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub section: &'static str,
    pub field: &'static str,
    pub does: &'static str,
    pub scope: Scope,
    /// May be empty (off).
    pub optional: bool,
}

impl Slot {
    /// `tmux.back`, `keys.quit`.
    pub fn id(&self) -> String {
        format!("{}.{}", self.section, self.field)
    }

    /// The key set for it now (as written in the config).
    pub fn current(&self, cfg: &Config) -> String {
        field_of(cfg, self)
    }

    pub fn default_spec(&self) -> String {
        field_of(&Config::default(), self)
    }

    /// The key written in this slot's section's notation.
    pub fn spell(&self, k: &Key) -> String {
        match self.section {
            "tmux" => k.tmux(),
            _ => k.list(),
        }
    }
}

fn field_of(cfg: &Config, s: &Slot) -> String {
    serde_json::to_value(cfg)
        .ok()
        .and_then(|v| v[s.section][s.field].as_str().map(str::to_string))
        .unwrap_or_default()
}

const fn g(field: &'static str, does: &'static str, optional: bool) -> Slot {
    Slot {
        section: "tmux",
        field,
        does,
        scope: Scope::Global,
        optional,
    }
}

const fn l(field: &'static str, does: &'static str) -> Slot {
    Slot {
        section: "keys",
        field,
        does,
        scope: Scope::List,
        optional: false,
    }
}

/// Every key rlord binds, in the order the key screen lists them.
pub const SLOTS: &[Slot] = &[
    g("back", "go home to the list (closes the overlay)", true),
    g("overlord", "open the overlord", true),
    g("next", "next session", true),
    g("prev", "previous session", true),
    l("overlord", "show/hide the overlord"),
    l("new", "new session (ask the overlord)"),
    l("new_manual", "new session (form)"),
    l("attach", "attach / resume"),
    l("up", "up"),
    l("down", "down"),
    l("top", "top"),
    l("bottom", "bottom"),
    l("filter", "search"),
    l("switch_tab", "sessions ⇄ groups"),
    l("sessions_tab", "step left (tabs)"),
    l("groups_tab", "step right (tabs)"),
    l("restart", "restart"),
    l("stop", "stop"),
    l("archive", "archive"),
    l("unarchive", "unarchive"),
    l("rename", "rename"),
    l("pin", "pin"),
    l("tags", "tags"),
    l("describe", "describe"),
    l("notes", "notes"),
    l("toggle_archived", "show archived"),
    l("sort", "sort"),
    l("reverse_sort", "reverse sort"),
    l("toggle_log", "log preview"),
    l("help", "cheat sheet"),
    l("command", "command launcher"),
    l("cancel", "cancel / back"),
    l("quit", "quit"),
];

pub fn slot(id: &str) -> Option<&'static Slot> {
    SLOTS.iter().find(|s| s.id() == id)
}

/// A key, whichever way it was spelled. `name` is one character (case
/// matters: `R` is Shift-r) or a lowercase key name (`enter`, `f5`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub name: String,
    pub ctrl: bool,
    pub alt: bool,
}

/// (our name, tmux's name)
const NAMES: &[(&str, &str)] = &[
    ("enter", "Enter"),
    ("esc", "Escape"),
    ("tab", "Tab"),
    ("space", "Space"),
    ("backspace", "BSpace"),
    ("up", "Up"),
    ("down", "Down"),
    ("left", "Left"),
    ("right", "Right"),
    ("home", "Home"),
    ("end", "End"),
    ("pageup", "PPage"),
    ("pagedown", "NPage"),
    ("delete", "DC"),
    ("insert", "IC"),
];

impl Key {
    pub fn char(c: char, ctrl: bool, alt: bool) -> Key {
        let c = if ctrl { c.to_ascii_lowercase() } else { c };
        let name = if c == ' ' {
            "space".into()
        } else {
            c.to_string()
        };
        Key { name, ctrl, alt }
    }

    pub fn named(name: &str, ctrl: bool, alt: bool) -> Key {
        Key {
            name: name.to_string(),
            ctrl,
            alt,
        }
    }

    /// `ctrl-q`, `alt-x`, `C-q`, `M-x`, `M-C-x`, `enter`, `Enter`, `f5`,
    /// `F5`, `PPage`, `R`. None for an empty or unknown spec.
    pub fn parse(spec: &str) -> Option<Key> {
        let mut rest = spec.trim();
        let (mut ctrl, mut alt) = (false, false);
        loop {
            let lower = rest.to_ascii_lowercase();
            let strip = |p: &str| {
                lower
                    .strip_prefix(p)
                    .filter(|r| !r.is_empty())
                    .map(str::len)
            };
            if let Some(n) = strip("ctrl-").or_else(|| strip("c-")) {
                ctrl = true;
                rest = &rest[rest.len() - n..];
            } else if let Some(n) = strip("alt-")
                .or_else(|| strip("meta-"))
                .or_else(|| strip("m-"))
            {
                alt = true;
                rest = &rest[rest.len() - n..];
            } else {
                break;
            }
        }
        if rest.is_empty() {
            return None;
        }
        let mut chars = rest.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            return Some(Key::char(c, ctrl, alt));
        }
        let lower = rest.to_ascii_lowercase();
        let name = NAMES
            .iter()
            .find(|(ours, theirs)| lower == *ours || lower == theirs.to_ascii_lowercase())
            .map(|(ours, _)| ours.to_string())
            .or(match lower.as_str() {
                "return" => Some("enter".into()),
                "escape" => Some("esc".into()),
                "del" => Some("delete".into()),
                f if f.starts_with('f')
                    && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) =>
                {
                    Some(f.to_string())
                }
                _ => None,
            })?;
        Some(Key::named(&name, ctrl, alt))
    }

    /// tmux's spelling: `M-d`, `C-q`, `Enter`, `F5`.
    pub fn tmux(&self) -> String {
        let name = if self.name.chars().count() == 1 {
            self.name.clone()
        } else if let Some((_, t)) = NAMES.iter().find(|(o, _)| *o == self.name) {
            t.to_string()
        } else {
            self.name.to_ascii_uppercase()
        };
        format!(
            "{}{}{name}",
            if self.ctrl { "C-" } else { "" },
            if self.alt { "M-" } else { "" }
        )
    }

    /// The list's spelling: `alt-d`, `ctrl-q`, `enter`, `R`.
    pub fn list(&self) -> String {
        format!(
            "{}{}{}",
            if self.ctrl { "ctrl-" } else { "" },
            if self.alt { "alt-" } else { "" },
            self.name
        )
    }

    /// How it reads: `⌥d`, `⌃q`, `enter` on a Mac; `Alt-d`, `Ctrl-q`
    /// elsewhere.
    pub fn label(&self) -> String {
        format!("{}{}", mods_label(self.ctrl, self.alt), self.name)
    }
}

/// What the Alt key is called on this system: `Option` on a Mac, `Alt`
/// elsewhere. For user-facing words ("press Option-O").
pub fn alt_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Option"
    } else {
        "Alt"
    }
}

/// The modifier part of a key label: `⌃⌥` on a Mac, `Ctrl-Alt-` elsewhere.
pub fn mods_label(ctrl: bool, alt: bool) -> String {
    let mac = cfg!(target_os = "macos");
    let mut s = String::new();
    if ctrl {
        s.push_str(if mac { "⌃" } else { "Ctrl-" });
    }
    if alt {
        s.push_str(if mac { "⌥" } else { "Alt-" });
    }
    s
}

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Worth knowing.
    Note,
    /// rlord takes the key from something that used it.
    Shadows,
    /// Won't work, or breaks another key: not saved.
    Taken,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Note => "note",
            Level::Shadows => "shadows",
            Level::Taken => "taken",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub text: String,
}

/// What else is on the keyboard: read once per check.
#[derive(Debug, Default, Clone)]
pub struct Env {
    /// The user's tmux prefixes (their profile's; rlord's server has none).
    pub prefixes: Vec<Key>,
    /// Bindings in rlord's root table that aren't rlord's: the user's own
    /// (the list page and sessions without the profile).
    pub root: Vec<(Key, String)>,
    /// The user's imported tmux bindings (agent sessions with the profile).
    pub profile: Vec<(Key, String)>,
    /// Claude Code's keys: its defaults, then the user's keybindings.json.
    pub claude: Vec<(Key, String)>,
}

impl Env {
    /// Read the live tmux server and Claude Code's key file.
    pub fn read() -> Env {
        let (root, profile, prefixes) = crate::tmux::key_env();
        let parse = |v: Vec<(String, String)>| {
            v.into_iter()
                .filter(|(_, cmd)| !cmd.contains("rlord") || !cmd.contains(" nav "))
                .filter_map(|(k, cmd)| Some((Key::parse(&k)?, cmd)))
                .collect()
        };
        Env {
            prefixes: prefixes.iter().filter_map(|p| Key::parse(p)).collect(),
            root: parse(root),
            profile: parse(profile),
            claude: claude_keys(&crate::agents::claude::config_dir().join("keybindings.json")),
        }
    }
}

/// Claude Code's default keys (the ones a global rlord key would take).
const CLAUDE_DEFAULTS: &[(&str, &str)] = &[
    ("C-c", "cancel / exit"),
    ("C-d", "exit"),
    ("C-l", "redraw the screen"),
    ("C-o", "the transcript view"),
    ("C-r", "history search"),
    ("C-t", "the todo list"),
    ("C-g", "editing the prompt in $EDITOR"),
    ("C-b", "backgrounding a command"),
    ("C-s", "stashing the prompt"),
    ("C-_", "undo"),
    ("C-v", "pasting an image"),
    ("C-z", "suspend"),
    ("M-p", "switching model"),
    ("M-t", "toggling thinking"),
];

/// What the shell's line editor (bash/zsh, emacs keys) does with a key.
const SHELL: &[(&str, &str)] = &[
    ("C-a", "start of line"),
    ("C-e", "end of line"),
    ("C-b", "back a character"),
    ("C-f", "forward a character"),
    ("C-k", "delete to end of line"),
    ("C-u", "delete the line"),
    ("C-w", "delete a word back"),
    ("C-y", "paste"),
    ("C-p", "previous command"),
    ("C-n", "next command"),
    ("C-r", "history search"),
    ("C-t", "swap characters"),
    ("C-l", "clear the screen"),
    ("C-d", "delete / exit"),
    ("C-c", "interrupt"),
    ("C-z", "suspend"),
    ("M-b", "back a word"),
    ("M-f", "forward a word"),
    ("M-d", "delete a word"),
    ("M-.", "last argument"),
    ("M-u", "uppercase a word"),
    ("M-l", "lowercase a word"),
    ("M-c", "capitalize a word"),
    ("M-t", "swap words"),
];

/// Claude Code's keys: defaults, then `keybindings.json` on top (a key set
/// to null there is unbound).
pub fn claude_keys(path: &Path) -> Vec<(Key, String)> {
    let mut out: Vec<(Key, String)> = CLAUDE_DEFAULTS
        .iter()
        .filter_map(|(k, d)| Some((Key::parse(k)?, d.to_string())))
        .collect();
    let Some(v) = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return out;
    };
    let mut user = Vec::new();
    walk_bindings(&v, &mut user);
    for (chord, action) in user {
        let Some(k) = claude_chord(&chord) else {
            continue;
        };
        out.retain(|(o, _)| *o != k);
        if let Some(a) = action {
            out.push((k, a));
        }
    }
    out
}

/// Every `"bindings": {chord: action}` object, however nested.
fn walk_bindings(v: &Value, out: &mut Vec<(String, Option<String>)>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k == "bindings" {
                    if let Value::Object(b) = x {
                        for (chord, action) in b {
                            out.push((chord.clone(), action.as_str().map(str::to_string)));
                        }
                        continue;
                    }
                }
                walk_bindings(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| walk_bindings(x, out)),
        _ => {}
    }
}

/// `ctrl+shift+k`, `alt+p`, `meta+t` → a key (the first of a chord sequence).
fn claude_chord(s: &str) -> Option<Key> {
    let first = s.split_whitespace().next()?;
    let parts: Vec<&str> = first.split('+').collect();
    let (name, mods) = parts.split_last()?;
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    for m in mods {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "meta" | "option" | "opt" => alt = true,
            "shift" => shift = true,
            _ => return None,
        }
    }
    let name = if name.chars().count() == 1 && shift && !ctrl {
        name.to_uppercase()
    } else {
        name.to_string()
    };
    let mut k = Key::parse(&name)?;
    k.ctrl |= ctrl;
    k.alt |= alt;
    if k.ctrl && k.name.chars().count() == 1 {
        k.name = k.name.to_ascii_lowercase();
    }
    Some(k)
}

/// The keys every slot holds now (`None` when off or unreadable).
fn bound(cfg: &Config) -> Vec<(&'static Slot, Option<Key>)> {
    let v = serde_json::to_value(cfg).unwrap_or_default();
    SLOTS
        .iter()
        .map(|s| (s, v[s.section][s.field].as_str().and_then(Key::parse)))
        .collect()
}

/// What stands in the way of `key` for `slot` (worst first).
pub fn check(cfg: &Config, env: &Env, slot: &Slot, key: &Key) -> Vec<Finding> {
    let mut out = findings(cfg, env, slot, key);
    out.sort_by_key(|f| std::cmp::Reverse(f.level));
    out
}

/// Keys the list always uses, whatever the config says.
const LIST_BUILTIN: &[(&str, &str)] = &[
    ("up", "the list always moves up with ↑"),
    ("down", "the list always moves down with ↓"),
    ("left", "← moves to the group list (groups tab)"),
    ("right", "→ opens a group (groups tab)"),
    ("C-c", "Ctrl-C in the list only says how to quit"),
];

/// Keys no global binding may take: every pane needs them.
const PANE_ESSENTIAL: &[(&str, &str)] = &[
    ("C-c", "interrupt"),
    ("C-d", "end of input"),
    ("C-z", "suspend"),
    ("C-\\", "quit"),
];

fn findings(cfg: &Config, env: &Env, slot: &Slot, key: &Key) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut add = |level, text: String| out.push(Finding { level, text });
    let label = key.label();
    // The list's overlord key matches the global one on purpose.
    let pair = |a: &Slot, b: &Slot| {
        a.field == "overlord" && b.field == "overlord" && a.section != b.section
    };
    for (other, k) in bound(cfg) {
        if other == slot || k.as_ref() != Some(key) || pair(slot, other) {
            continue;
        }
        let text = match (slot.scope, other.scope) {
            (a, b) if a == b => format!("{label} already does \"{}\" ({})", other.does, other.id()),
            (Scope::List, Scope::Global) => format!(
                "{label} is rlord's key to \"{}\" everywhere: tmux takes it before the list sees it",
                other.does
            ),
            _ => format!(
                "{label} is the list's key to \"{}\": that would stop working",
                other.does
            ),
        };
        add(Level::Taken, text);
    }
    if slot.scope == Scope::List
        && cfg.jump_digits
        && !key.ctrl
        && !key.alt
        && matches!(
            key.name.as_str(),
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"
        )
    {
        add(
            Level::Taken,
            "digits jump to the Nth session (jump_digits = true)".into(),
        );
    }
    if env.prefixes.contains(key) {
        let level = if slot.scope == Scope::Global {
            Level::Taken
        } else {
            Level::Note
        };
        add(level, format!("{label} is your tmux prefix"));
    }
    if let Some((_, cmd)) = env.root.iter().find(|(k, _)| k == key) {
        add(
            Level::Taken,
            format!("your tmux config binds {label} on rlord's server: {cmd}"),
        );
    }
    if slot.scope == Scope::List {
        let own = matches!(
            (slot.field, key.name.as_str()),
            ("up", "up") | ("down", "down")
        );
        if let Some((_, why)) = LIST_BUILTIN
            .iter()
            .find(|(k, _)| !own && Key::parse(k).as_ref() == Some(key))
        {
            add(Level::Taken, why.to_string());
        }
    }
    if slot.scope != Scope::Global {
        return out;
    }
    if let Some((_, what)) = PANE_ESSENTIAL
        .iter()
        .find(|(k, _)| Key::parse(k).as_ref() == Some(key))
    {
        add(
            Level::Taken,
            format!("{label} is {what} in every pane: it can't be rlord's"),
        );
    }
    if let Some((_, cmd)) = env.profile.iter().find(|(k, _)| k == key) {
        add(
            Level::Shadows,
            format!("in agent sessions rlord takes {label} from your tmux config ({cmd})"),
        );
    }
    if let Some((_, what)) = env.claude.iter().find(|(k, _)| k == key) {
        add(
            Level::Shadows,
            format!("Claude Code uses {label} for {what}; it won't reach Claude"),
        );
    }
    if let Some((_, what)) = SHELL
        .iter()
        .find(|(k, _)| Key::parse(k).as_ref() == Some(key))
    {
        add(
            Level::Shadows,
            format!("your shell's line editing uses {label} ({what}); it won't reach the shell"),
        );
    }
    // Without a modifier (bar F-keys) a global key is a key people type.
    let fkey = key.name.len() > 1 && key.name.starts_with('f');
    if !key.ctrl && !key.alt && !fkey {
        add(
            Level::Taken,
            format!("a plain {label} everywhere would stop you typing it"),
        );
    }
    out
}

/// macOS types these for Option+letter when the terminal doesn't send
/// Option as Meta (US layout): the key arrived as a character. Only
/// consulted on macOS (the key screen's hint).
pub fn option_char(c: char) -> Option<char> {
    const MAP: &[(char, char)] = &[
        ('å', 'a'),
        ('∫', 'b'),
        ('ç', 'c'),
        ('∂', 'd'),
        ('ƒ', 'f'),
        ('©', 'g'),
        ('˙', 'h'),
        ('∆', 'j'),
        ('˚', 'k'),
        ('¬', 'l'),
        ('µ', 'm'),
        ('ø', 'o'),
        ('π', 'p'),
        ('œ', 'q'),
        ('®', 'r'),
        ('ß', 's'),
        ('†', 't'),
        ('√', 'v'),
        ('∑', 'w'),
        ('≈', 'x'),
        ('¥', 'y'),
        ('Ω', 'z'),
    ];
    MAP.iter().find(|(o, _)| *o == c).map(|(_, l)| *l)
}

/// How to make Option send Meta in the user's terminal (`term` is tmux's
/// `client_termtype`, e.g. `iTerm2 3.5.0`, `ghostty 1.1.0`).
pub fn option_as_meta_fix(term: &str) -> String {
    let t = term.to_ascii_lowercase();
    if t.contains("iterm") {
        "iTerm2: Settings → Profiles → Keys → Left Option key: Esc+".into()
    } else if t.contains("ghostty") {
        "Ghostty: add `macos-option-as-alt = true` to its config".into()
    } else if t.contains("kitty") {
        "kitty: add `macos_option_as_alt both` to kitty.conf".into()
    } else if t.contains("wezterm") {
        "WezTerm: set `send_composed_key_when_left_alt_is_pressed = false`".into()
    } else if t.contains("alacritty") {
        "Alacritty: set `option_as_alt = \"Both\"` under [window]".into()
    } else if t.contains("apple_terminal") || t.contains("terminal") {
        "Terminal: Settings → Profiles → Keyboard → Use Option as Meta key".into()
    } else {
        "Turn on \"Option as Meta\" (or Alt / Esc+) in your terminal's settings".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_spellings_are_one_key() {
        let k = |s| Key::parse(s).unwrap();
        assert_eq!(k("M-d"), k("alt-d"));
        assert_eq!(k("C-q"), k("ctrl-Q"));
        assert_eq!(k("Enter"), k("enter"));
        assert_eq!(k("PPage"), k("pageup"));
        assert_eq!(k("F5"), k("f5"));
        assert_ne!(k("R"), k("r"));
        assert_eq!(k("M-d").tmux(), "M-d");
        assert_eq!(k("alt-d").list(), "alt-d");
        assert_eq!(k("enter").tmux(), "Enter");
        assert_eq!(k("-").name, "-");
        assert_eq!(k("M--").tmux(), "M--");
        assert_eq!(k("?").list(), "?");
        assert!(Key::parse("").is_none());
        assert!(Key::parse("bogus").is_none());
    }

    #[test]
    fn checks_find_whats_in_the_way() {
        let cfg = Config::default();
        let env = Env {
            prefixes: vec![Key::parse("C-a").unwrap()],
            root: vec![],
            profile: vec![(Key::parse("M-a").unwrap(), "copy-mode".into())],
            claude: claude_keys(Path::new("/nonexistent")),
        };
        let back = slot("tmux.back").unwrap();
        let lv = |k: &str, s: &Slot| {
            check(&cfg, &env, s, &Key::parse(k).unwrap())
                .first()
                .map(|f| f.level)
        };
        // The default Option-D: the shell's delete-word, nothing worse.
        assert_eq!(lv("M-d", back), Some(Level::Shadows));
        assert_eq!(lv("M-o", back), Some(Level::Taken)); // the overlord's
        assert_eq!(lv("C-a", back), Some(Level::Taken)); // their prefix
        assert_eq!(lv("M-a", back), Some(Level::Shadows)); // their binding
        assert_eq!(lv("C-o", back), Some(Level::Shadows)); // Claude's
        assert_eq!(lv("x", back), Some(Level::Taken)); // plain letter
        assert_eq!(lv("M-h", back), None);
        assert_eq!(lv("enter", back), Some(Level::Taken)); // typed everywhere
        assert_eq!(lv("F5", back), None);
        assert_eq!(lv("C-c", back), Some(Level::Taken)); // interrupt
        assert_eq!(lv("up", slot("keys.quit").unwrap()), Some(Level::Taken)); // built-in arrow
        assert_eq!(lv("C-c", slot("keys.quit").unwrap()), Some(Level::Taken));
        assert_eq!(lv("up", slot("keys.up").unwrap()), None);
        let quit = slot("keys.quit").unwrap();
        assert_eq!(lv("j", quit), Some(Level::Taken)); // down
        assert_eq!(lv("M-d", quit), Some(Level::Taken)); // tmux's first
        assert_eq!(lv("3", quit), Some(Level::Taken)); // digits
        assert_eq!(lv("C-o", quit), None); // Claude isn't in the list
                                           // The two overlord keys match on purpose.
        let lo = slot("keys.overlord").unwrap();
        assert_eq!(lv("M-o", lo), None);
    }

    #[test]
    fn claude_keybindings_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("keybindings.json");
        std::fs::write(
            &p,
            r#"{"bindings": [{"context": "Chat", "bindings":
                {"ctrl+e": "chat:externalEditor", "ctrl+o": null, "alt+shift+x": "x"}}]}"#,
        )
        .unwrap();
        let ks = claude_keys(&p);
        let has = |s: &str| ks.iter().any(|(k, _)| *k == Key::parse(s).unwrap());
        assert!(has("C-e") && has("M-X") && has("C-r"));
        assert!(!has("C-o"));
    }

    #[test]
    fn slots_read_the_config() {
        let cfg = Config::default();
        assert_eq!(slot("tmux.back").unwrap().current(&cfg), "M-d");
        assert_eq!(slot("keys.help").unwrap().current(&cfg), "?");
        // Every slot names a real string field.
        for s in SLOTS {
            assert!(
                serde_json::to_value(&cfg).unwrap()[s.section][s.field].is_string(),
                "{}",
                s.id()
            );
        }
    }
}
