//! Which coding agents are on this machine, and how to get one that isn't.
//!
//! rlord never installs anything: it suggests the command for this machine
//! and the user runs it. Everything here is plain look-ups (PATH, a few
//! files, `--version`).
//!
//! One table, `TOOLS`, has a row per agent: its name, its marker in the
//! list, and its install commands. Update it when a
//! vendor changes theirs; each entry names the vendor page it was checked
//! against.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::{codex, opencode, Agent};

/// A piece of a marker: its text and colour (RGB; None: the theme's text).
pub type MarkerPiece = (&'static str, Option<(u8, u8, u8)>);

/// A coding agent rlord knows about, and its vendor's install commands.
pub struct Tool {
    /// Name in rlord (and its command on PATH).
    pub name: &'static str,
    pub label: &'static str,
    /// The agent rlord runs it as; None: known, not runnable in rlord yet.
    pub agent: Option<Agent>,
    /// Its marker in the session list: at most two columns wide (the list
    /// pads it and adds a space), as
    /// coloured pieces (text, RGB; None: the theme's text colour).
    pub marker: &'static [MarkerPiece],
    /// The vendor's install page.
    pub docs: &'static str,
    /// The vendor's install script (macOS and Linux).
    pub script: &'static str,
    /// With Homebrew.
    pub brew: &'static str,
    /// With npm, globally.
    pub npm: &'static str,
    /// With an AUR helper on Arch Linux, when the vendor documents one.
    pub aur: Option<&'static str>,
}

/// Checked against the vendors' pages on 2026-11-11.
pub const TOOLS: [Tool; 3] = [
    // https://code.claude.com/docs/en/setup (the script is "recommended";
    // apt/dnf/apk repositories exist too: several steps, see the page).
    Tool {
        name: "claude",
        label: "Claude Code",
        marker: &[("✻", Some((217, 119, 87)))],
        agent: Some(Agent::Claude),
        docs: "https://code.claude.com/docs/en/setup",
        script: "curl -fsSL https://claude.ai/install.sh | bash",
        brew: "brew install --cask claude-code",
        npm: "npm install -g @anthropic-ai/claude-code",
        aur: None,
    },
    // https://github.com/openai/codex#readme
    Tool {
        name: "codex",
        label: "Codex",
        marker: &[(">", Some((59, 130, 246))), ("_", None)],
        agent: Some(Agent::Codex),
        docs: "https://github.com/openai/codex#readme",
        script: "curl -fsSL https://chatgpt.com/codex/install.sh | sh",
        brew: "brew install --cask codex",
        npm: "npm install -g @openai/codex",
        aur: None,
    },
    // https://opencode.ai/docs/ (rlord runs OpenCode v2: `opencode::MIN_MAJOR`)
    Tool {
        name: "opencode",
        label: "OpenCode",
        marker: &[("▣", Some((20, 184, 166)))],
        agent: Some(Agent::OpenCode),
        docs: "https://opencode.ai/docs/",
        script: "curl -fsSL https://opencode.ai/v2/install | bash",
        brew: "brew install anomalyco/tap/opencode-v2",
        npm: "npm install -g @opencode/cli",
        aur: Some("paru -S opencode-beta"),
    },
];

pub fn tool(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.name == name.trim())
}

/// What about this machine decides the suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Machine {
    pub macos: bool,
    pub brew: bool,
    pub npm: bool,
    /// An AUR helper (`paru`) is installed.
    pub paru: bool,
}

impl Machine {
    pub fn detect() -> Machine {
        Machine {
            macos: cfg!(target_os = "macos"),
            brew: on_path("brew").is_some(),
            npm: on_path("npm").is_some(),
            paru: on_path("paru").is_some(),
        }
    }
}

/// One way to install a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Method {
    /// `homebrew`, `install script`, `npm`, `aur`.
    pub via: &'static str,
    pub command: &'static str,
}

/// The command to suggest on this machine, and the other ways that would
/// work here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Suggestion {
    pub via: &'static str,
    pub command: &'static str,
    pub alternatives: Vec<Method>,
    pub docs: &'static str,
}

/// macOS with Homebrew: the brew command. Arch with an AUR helper, when
/// the vendor has a package: that. Otherwise the vendor's install script
/// (its official method on macOS and Linux). npm, when present, and the
/// script are offered as alternatives.
pub fn suggest(t: &Tool, m: Machine) -> Suggestion {
    let script = Method {
        via: "install script",
        command: t.script,
    };
    let mut ways: Vec<Method> = Vec::new();
    if m.brew && m.macos {
        ways.push(Method {
            via: "homebrew",
            command: t.brew,
        });
    }
    if let Some(aur) = t.aur.filter(|_| m.paru && !m.macos) {
        ways.push(Method {
            via: "aur",
            command: aur,
        });
    }
    ways.push(script);
    // (Linuxbrew: works, but it isn't what a Linux user reaches for first.)
    if m.brew && !m.macos {
        ways.push(Method {
            via: "homebrew",
            command: t.brew,
        });
    }
    if m.npm {
        ways.push(Method {
            via: "npm",
            command: t.npm,
        });
    }
    let first = ways.remove(0);
    Suggestion {
        via: first.via,
        command: first.command,
        alternatives: ways,
        docs: t.docs,
    }
}

/// Where a command is on PATH (a file that can be executed).
pub fn on_path(bin: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// What the user's login shell last said about each command.
type Asked = std::collections::HashMap<String, (std::time::Instant, Option<PathBuf>)>;
static ASKED: std::sync::LazyLock<std::sync::Mutex<Asked>> =
    std::sync::LazyLock::new(Default::default);

/// How long a login-shell answer is taken as current by `resolve`.
const ASK_AGAIN: std::time::Duration = std::time::Duration::from_secs(60);

fn asked(bin: &str, max_age: Option<std::time::Duration>) -> Option<Option<PathBuf>> {
    let mut asked = ASKED.lock().ok()?;
    let (at, found) = asked.get(bin)?;
    // A path that has since gone (uninstalled, moved) is no answer: forget
    // it, so the shell is asked again.
    if found.as_ref().is_some_and(|p| !p.is_file()) {
        asked.remove(bin);
        return None;
    }
    max_age
        .is_none_or(|max| at.elapsed() < max)
        .then(|| found.clone())
}

/// Where a command is: on this process's PATH, else where the user's
/// login shell resolves it (`$SHELL -lc 'command -v <bin>'`: the daemon
/// may run with a shorter PATH than the user's shell has). **May start
/// that shell** (three seconds at most; its answer is remembered for a
/// minute): never call it while holding a lock others wait on. Use
/// `resolved` there.
pub fn resolve(bin: &str) -> Option<PathBuf> {
    if let Some(p) = on_path(bin) {
        return Some(p);
    }
    if let Some(found) = asked(bin, Some(ASK_AGAIN)) {
        return found;
    }
    let found = login_shell_path(bin, std::time::Duration::from_secs(3));
    if let Ok(mut asked) = ASKED.lock() {
        asked.insert(bin.to_string(), (std::time::Instant::now(), found.clone()));
    }
    found
}

/// `resolve` without ever starting anything: PATH, else the login shell's
/// last answer (however old; `warm` keeps it fresh). Safe under a lock.
pub fn resolved(bin: &str) -> Option<PathBuf> {
    on_path(bin).or_else(|| asked(bin, None).flatten())
}

/// Ask the login shell about every agent rlord runs that PATH doesn't
/// have (outside any lock: at daemon start, and before a launch), so
/// `resolved` knows. Also what else a launch or the form needs that means
/// running the agent: OpenCode's version (rlord needs v2) and its model
/// list, each remembered (`version_known`, `opencode::models`).
pub fn warm() {
    // Only the ones PATH lacks need the shell, and those at the same time:
    // a slow shell costs its limit once, not once per agent.
    let ask: Vec<&str> = TOOLS
        .iter()
        .filter(|t| t.agent.is_some() && on_path(t.name).is_none())
        .map(|t| t.name)
        .collect();
    std::thread::scope(|s| {
        for name in ask {
            s.spawn(move || resolve(name));
        }
    });
    if let Some(bin) = resolved(Agent::OpenCode.name()) {
        let usable = version(&bin).is_none_or(|v| opencode::too_old(&v).is_none());
        if usable {
            opencode::refresh_models(&bin);
        }
    }
}

/// The program a session's pane should run for `bin`: None when the bare
/// name works (it is on this process's PATH, which rlord's tmux panes
/// share); else the path the login shell found, so the launch finds what
/// the "is it installed" check found.
pub fn pane_program(bin: &str) -> Option<PathBuf> {
    if on_path(bin).is_some() {
        return None;
    }
    resolved(bin)
}

/// `$SHELL -lc 'command -v <bin>'`: an absolute path, or nothing. The
/// shell and reading its answer both end at `limit`.
fn login_shell_path(bin: &str, limit: std::time::Duration) -> Option<PathBuf> {
    // Off for demos, tests and scratch setups: there only what is on PATH
    // exists, so a stand-in can never be passed over for a real agent the
    // user's own shell knows.
    if login_shell_off(std::env::var("RLORD_NO_LOGIN_SHELL").ok().as_deref()) {
        return None;
    }
    // (Only ever rlord's own agent names: never text from outside.)
    if !bin.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    shell_answer(&shell, &format!("command -v {bin}"), limit)
}

/// `RLORD_NO_LOGIN_SHELL`: set to anything but empty or `0`, the login
/// shell is never asked where an agent is.
fn login_shell_off(var: Option<&str>) -> bool {
    var.is_some_and(|v| !v.is_empty() && v != "0")
}

/// Run `shell -lc script` and take the last line it prints as a path.
fn shell_answer(shell: &str, script: &str, limit: std::time::Duration) -> Option<PathBuf> {
    use std::io::Read;
    let mut child = std::process::Command::new(shell)
        .args(["-lc", script])
        .env_remove("RLORD_SESSION_ID")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // Read on a thread: something the shell's rc left running can hold the
    // pipe open long after the shell is gone.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        let _ = tx.send(out);
    });
    let start = std::time::Instant::now();
    let ok = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.success(),
            Ok(None) if start.elapsed() < limit => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    if !ok {
        return None;
    }
    let out = rx
        .recv_timeout(
            limit
                .saturating_sub(start.elapsed())
                .max(std::time::Duration::from_millis(200)),
        )
        .ok()?;
    // (A shell rc may print before it: the answer is the last line.)
    let p = PathBuf::from(out.lines().next_back()?.trim());
    (p.is_absolute() && p.is_file()).then_some(p)
}

/// What `<bin> --version` prints (its first line), remembered until the
/// binary changes. Bounded: a command that doesn't answer in two seconds
/// has no version.
pub fn version(bin: &Path) -> Option<String> {
    use std::time::{Duration, Instant};
    // (Through symlinks: an upgrade replaces what the link points at.)
    let modified = std::fs::metadata(bin).and_then(|m| m.modified()).ok()?;
    if let Some(known) = version_seen(bin, modified) {
        return known;
    }
    let mut child = std::process::Command::new(bin)
        .arg("--version")
        .env_remove("RLORD_SESSION_ID")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let start = Instant::now();
    let done = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(2) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let v = done
        .then(|| {
            use std::io::Read;
            let mut out = String::new();
            child.stdout.take()?.read_to_string(&mut out).ok()?;
            let line = out.lines().next()?.trim().to_string();
            (!line.is_empty()).then_some(line)
        })
        .flatten();
    if let Ok(mut seen) = SEEN.lock() {
        seen.insert(bin.to_path_buf(), (modified, v.clone()));
    }
    v
}

/// Per binary: when it was last modified, and what `--version` said then.
type Seen = std::collections::HashMap<PathBuf, (std::time::SystemTime, Option<String>)>;
static SEEN: std::sync::LazyLock<std::sync::Mutex<Seen>> =
    std::sync::LazyLock::new(Default::default);

/// What `version` remembers for a binary last modified at `modified`.
fn version_seen(bin: &Path, modified: std::time::SystemTime) -> Option<Option<String>> {
    let seen = SEEN.lock().ok()?;
    let (at, v) = seen.get(bin)?;
    (*at == modified).then(|| v.clone())
}

/// `version` without running anything: what the binary last answered, if
/// it was asked since it last changed (`warm` asks). Safe under a lock.
pub fn version_known(bin: &Path) -> Option<String> {
    let modified = std::fs::metadata(bin).and_then(|m| m.modified()).ok()?;
    version_seen(bin, modified).flatten()
}

/// Why rlord can't run this install of `agent` (it is older than rlord
/// supports), as far as is known without running it.
fn unusable(agent: Agent, bin: &Path) -> Option<String> {
    match agent {
        Agent::OpenCode => opencode::too_old(&version_known(bin)?),
        Agent::Claude | Agent::Codex => None,
    }
}

/// The events rlord's hook should be installed for and isn't, in the
/// agent's hooks file (empty: all in place). A newer rlord listens for
/// more events than an older one installed.
pub fn hooks_missing(agent: Agent, file: &Path) -> Vec<&'static str> {
    let ours = our_hooks(file);
    agent
        .hook_events()
        .iter()
        .map(|(event, _)| *event)
        .filter(|event| !ours.iter().any(|(e, _, _)| e == event))
        .collect()
}

/// Where rlord's hook sits in a hooks file: (event, group index, hook
/// index) for each event that has it.
fn our_hooks(file: &Path) -> Vec<(String, usize, usize)> {
    let Some(v) = std::fs::read_to_string(file)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (event, groups) in v["hooks"].as_object().into_iter().flatten() {
        for (gi, g) in groups.as_array().into_iter().flatten().enumerate() {
            for (hi, h) in g["hooks"].as_array().into_iter().flatten().enumerate() {
                let ours = h["command"]
                    .as_str()
                    .is_some_and(|c| c.contains("hook emit") && c.contains("rlord"));
                if ours {
                    out.push((event.clone(), gi, hi));
                }
            }
        }
    }
    out
}

/// Whether the user has trusted rlord's hooks in Codex (`/hooks`): Codex
/// records each reviewed hook in `config.toml` under `[hooks.state]`, keyed
/// `<hooks file>:<event>:<group>:<hook>`, with a `trusted_hash`. None when
/// rlord's hooks aren't installed. (A hash from before the command last
/// changed still reads as trusted here: Codex decides for real.)
pub fn codex_hooks_trusted(home: &Path) -> Option<bool> {
    let file = home.join("hooks.json");
    let ours = our_hooks(&file);
    if ours.is_empty() {
        return None;
    }
    let state = std::fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|s| s.parse::<toml::Table>().ok())
        .and_then(|t| t.get("hooks")?.get("state")?.as_table().cloned())
        .unwrap_or_default();
    let trusted = |event: &str, gi: usize, hi: usize| {
        let key = format!("{}:{}:{gi}:{hi}", file.display(), snake(event));
        state
            .get(&key)
            .and_then(|e| e.get("trusted_hash"))
            .and_then(|h| h.as_str())
            .is_some_and(|h| !h.is_empty())
    };
    Some(ours.iter().all(|(e, gi, hi)| trusted(e, *gi, *hi)))
}

/// `SessionStart` → `session_start`.
fn snake(event: &str) -> String {
    let mut out = String::new();
    for (i, c) in event.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// One agent's state on this machine.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub name: &'static str,
    pub label: &'static str,
    /// rlord can run sessions on it (false: known, support coming).
    pub supported: bool,
    pub installed: bool,
    /// Where its command is.
    pub path: Option<String>,
    /// From `--version` (only when asked for).
    pub version: Option<String>,
    /// Every hook rlord needs is in its hooks file (None: rlord doesn't
    /// run it, or follows it without hooks, as it does OpenCode).
    pub hooks_installed: Option<bool>,
    /// Some are, some aren't: installed by an older rlord.
    pub hooks_outdated: bool,
    /// Codex only: the user trusted rlord's hooks (`/hooks`).
    pub hooks_trusted: Option<bool>,
    /// Installed, but a version rlord can't run: why.
    pub too_old: Option<String>,
    /// How to install it, when it isn't (or to upgrade it, when too old).
    pub install: Option<Suggestion>,
    /// What's left to do, in a line (None: ready).
    pub next: Option<String>,
}

/// Every known agent's state. `full` (`rlord agents`, `list_agents`: never
/// under a lock): ask the login shell where PATH doesn't have one, and run
/// each one's `--version`. Otherwise nothing is started: agents rlord runs
/// come from PATH or the shell's last answer, the others from PATH alone.
pub fn status(full: bool) -> Vec<Status> {
    let versions = full;
    let machine = Machine::detect();
    TOOLS
        .iter()
        .map(|t| {
            let path = match (full, t.agent) {
                (true, _) => resolve(t.name),
                (false, Some(_)) => resolved(t.name),
                (false, None) => on_path(t.name),
            };
            let installed = path.is_some();
            let hooked = t.agent.filter(|a| a.uses_hooks());
            let missing = hooked.map(|a| hooks_missing(a, &a.hooks_file()));
            // (Run first when asked for versions, so the check below knows.)
            let said = path.as_deref().filter(|_| versions).and_then(version);
            let too_old = t
                .agent
                .zip(path.as_deref())
                .and_then(|(a, p)| unusable(a, p));
            let hooks_installed = missing.as_ref().map(Vec::is_empty);
            let hooks_outdated = hooked
                .zip(missing.as_ref())
                .is_some_and(|(a, m)| !m.is_empty() && m.len() < a.hook_events().len());
            let hooks_trusted = match t.agent {
                Some(Agent::Codex) => codex_hooks_trusted(&codex::home()),
                _ => None,
            };
            let next = match (installed, t.agent, hooks_installed, hooks_trusted) {
                _ if too_old.is_some() => too_old
                    .as_ref()
                    .map(|why| format!("{why}: upgrade it yourself with the install command")),
                (false, _, _, _) => Some(format!(
                    "not installed: run the install command yourself{}",
                    if hooked.is_some() {
                        ", then `rlord install-hooks`"
                    } else {
                        ""
                    }
                )),
                (true, None, _, _) => Some("installed; rlord support coming".to_string()),
                (true, Some(_), Some(false), _) if hooks_outdated => Some(
                    "rlord's hooks are out of date (this rlord listens for more events): \
                     run `rlord install-hooks` again"
                        .to_string(),
                ),
                (true, Some(_), Some(false), _) => Some("run `rlord install-hooks`".to_string()),
                (true, Some(_), _, Some(false)) => Some(
                    "trust rlord's hooks: start `codex`, type `/hooks`, trust the \
                     `rlord hook emit codex` entries"
                        .to_string(),
                ),
                _ => None,
            };
            Status {
                name: t.name,
                label: t.label,
                supported: t.agent.is_some(),
                installed,
                version: said,
                path: path.map(|p| p.to_string_lossy().into_owned()),
                hooks_installed,
                hooks_outdated,
                hooks_trusted,
                install: (!installed || too_old.is_some()).then(|| suggest(t, machine)),
                too_old,
                next,
            }
        })
        .collect()
}

/// Why sessions on `agent` can't launch here, with what to do about it;
/// None when its command can be found (on PATH, or by the user's login
/// shell as last asked: see `warm`). Starts nothing.
pub fn missing(agent: Agent) -> Option<String> {
    let found = resolved(agent.name());
    let old = found.as_deref().and_then(|bin| unusable(agent, bin));
    if found.is_some() && old.is_none() {
        return None;
    }
    let t = tool(agent.name())?;
    let s = suggest(t, Machine::detect());
    Some(match old {
        Some(why) => format!(
            "{why}. rlord doesn't upgrade it for you; run this yourself: {}  (other ways: {}).",
            s.command, s.docs
        ),
        None => format!(
            "{} isn't installed (`{}` isn't on PATH). rlord doesn't install it for you; run \
             this yourself: {}  (other ways: {}).{}",
            t.label,
            t.name,
            s.command,
            s.docs,
            if agent.uses_hooks() {
                " Then `rlord install-hooks`."
            } else {
                ""
            }
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_BREW: Machine = Machine {
        macos: true,
        brew: true,
        npm: true,
        paru: false,
    };

    #[test]
    fn the_suggestion_fits_the_machine() {
        let codex = tool("codex").unwrap();
        let s = suggest(codex, MAC_BREW);
        assert_eq!(
            (s.via, s.command),
            ("homebrew", "brew install --cask codex")
        );
        let alt: Vec<&str> = s.alternatives.iter().map(|m| m.via).collect();
        assert_eq!(alt, ["install script", "npm"]);
        // A Mac without Homebrew, or Linux: the vendor's script.
        let bare = Machine {
            macos: true,
            ..Default::default()
        };
        let s = suggest(codex, bare);
        assert_eq!(s.via, "install script");
        assert!(s.alternatives.is_empty());
        let linux = Machine {
            npm: true,
            brew: true,
            ..Default::default()
        };
        let s = suggest(tool("claude").unwrap(), linux);
        assert_eq!(s.command, "curl -fsSL https://claude.ai/install.sh | bash");
        let alt: Vec<&str> = s.alternatives.iter().map(|m| m.command).collect();
        assert_eq!(
            alt,
            [
                "brew install --cask claude-code",
                "npm install -g @anthropic-ai/claude-code"
            ]
        );
        // Arch with an AUR helper: OpenCode's package; Codex has none.
        let arch = Machine {
            paru: true,
            ..Default::default()
        };
        assert_eq!(suggest(tool("opencode").unwrap(), arch).via, "aur");
        assert_eq!(suggest(codex, arch).via, "install script");
    }

    #[test]
    fn every_agent_rlord_runs_has_install_commands() {
        for a in Agent::ALL {
            let t = tool(a.name()).expect("in the table");
            assert_eq!(t.agent, Some(a));
        }
        // Every agent in the table is one rlord runs.
        assert!(TOOLS.iter().all(|t| t.agent.is_some()));
        // Nothing here installs with elevated rights or runs unprompted.
        for t in &TOOLS {
            for c in [t.script, t.brew, t.npm] {
                assert!(!c.contains("sudo"), "{c}");
            }
        }
    }

    #[test]
    fn an_opencode_older_than_v2_is_not_run() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let install = |name: &str, says: &str| {
            let bin = dir.path().join(name);
            std::fs::write(&bin, format!("#!/bin/sh\necho '{says}'\n")).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            bin
        };
        let old = install("opencode-old", "1.4.3");
        // Not asked yet: nothing is known, nothing is refused.
        assert_eq!(version_known(&old), None);
        assert_eq!(unusable(Agent::OpenCode, &old), None);
        assert_eq!(version(&old).as_deref(), Some("1.4.3"));
        assert_eq!(version_known(&old).as_deref(), Some("1.4.3"));
        let why = unusable(Agent::OpenCode, &old).unwrap();
        assert!(why.contains("v2") && why.contains("1.4.3"), "{why}");
        let new = install("opencode-new", "opencode v2.2.7");
        version(&new);
        assert_eq!(unusable(Agent::OpenCode, &new), None);
        // Only OpenCode has a floor.
        assert_eq!(unusable(Agent::Codex, &old), None);
    }

    #[test]
    fn codex_trust_is_read_from_its_config() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        assert_eq!(codex_hooks_trusted(home), None, "no hooks installed");
        // Another tool's hook first on Stop: ours is the second group there.
        std::fs::write(
            home.join("hooks.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"other"}]}]}}"#,
        )
        .unwrap();
        Agent::Codex
            .install_hooks_in(&home.join("hooks.json"), "/bin/rlord")
            .unwrap();
        assert_eq!(codex_hooks_trusted(home), Some(false));
        let file = home.join("hooks.json");
        let entry = |event: &str, gi: usize| {
            format!(
                "[hooks.state.\"{}:{event}:{gi}:0\"]\ntrusted_hash = \"sha256:abc\"\n",
                file.display()
            )
        };
        let mut cfg = String::from("model = \"gpt-6-luna\"\n[hooks.state]\n");
        for e in [
            "session_start",
            "user_prompt_submit",
            "permission_request",
            "session_end",
            "post_tool_use",
        ] {
            cfg += &entry(e, 0);
        }
        // Only the other tool's Stop hook is trusted: not all of ours yet.
        std::fs::write(
            home.join("config.toml"),
            format!("{cfg}{}", entry("stop", 0)),
        )
        .unwrap();
        assert_eq!(codex_hooks_trusted(home), Some(false));
        std::fs::write(
            home.join("config.toml"),
            format!("{cfg}{}", entry("stop", 1)),
        )
        .unwrap();
        assert_eq!(codex_hooks_trusted(home), Some(true));
    }

    #[test]
    fn hooks_from_an_older_rlord_are_out_of_date() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hooks.json");
        assert_eq!(
            hooks_missing(Agent::Codex, &file).len(),
            Agent::Codex.hook_events().len(),
            "none installed"
        );
        // As an older rlord left it: no PostToolUse.
        std::fs::write(
            &file,
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"/bin/rlord hook emit codex"}]}],
                "UserPromptSubmit":[{"hooks":[{"type":"command","command":"/bin/rlord hook emit codex"}]}],
                "Stop":[{"hooks":[{"type":"command","command":"/bin/rlord hook emit codex"}]}],
                "PermissionRequest":[{"hooks":[{"type":"command","command":"/bin/rlord hook emit codex"}]}],
                "SessionEnd":[{"hooks":[{"type":"command","command":"/bin/rlord hook emit codex"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(hooks_missing(Agent::Codex, &file), ["PostToolUse"]);
        Agent::Codex.install_hooks_in(&file, "/bin/rlord").unwrap();
        assert!(hooks_missing(Agent::Codex, &file).is_empty());
    }

    #[test]
    fn a_file_that_cannot_run_is_not_the_command() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("rlord-test-agent-bin");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        let exec = |p: &Path| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        };
        assert!(!exec(&bin));
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(exec(&bin));
        // The login shell's answer: a path that exists, within the limit
        // (a shell that hangs, or leaves the pipe open, doesn't hold it up).
        let limit = std::time::Duration::from_secs(2);
        let script = format!("echo noise; echo {}", bin.display());
        assert_eq!(
            shell_answer("/bin/sh", &script, limit).as_deref(),
            Some(bin.as_path())
        );
        assert_eq!(shell_answer("/bin/sh", "echo /nonexistent/x", limit), None);
        let start = std::time::Instant::now();
        let short = std::time::Duration::from_millis(300);
        assert_eq!(shell_answer("/bin/sh", "sleep 5", short), None);
        assert_eq!(
            shell_answer("/bin/sh", "(sleep 5 &) ; echo /bin/sh", short),
            None
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
        // A remembered answer is dropped once the file is gone.
        let name = "rlord-test-stale-agent";
        ASKED.lock().unwrap().insert(
            name.to_string(),
            (std::time::Instant::now(), Some(bin.clone())),
        );
        assert_eq!(resolved(name).as_deref(), Some(bin.as_path()));
        std::fs::remove_file(&bin).unwrap();
        assert_eq!(resolved(name), None);
        assert!(
            !ASKED.lock().unwrap().contains_key(name),
            "forgotten: the next resolve asks the shell again"
        );
        // The switch that keeps scratch setups to what is on their PATH.
        assert!(login_shell_off(Some("1")) && login_shell_off(Some("yes")));
        assert!(
            !login_shell_off(None) && !login_shell_off(Some("")) && !login_shell_off(Some("0"))
        );
        // Names that aren't plain command names never reach a shell.
        assert_eq!(
            login_shell_path("codex; rm -rf x", std::time::Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn event_names_as_codex_keys_them() {
        assert_eq!(snake("SessionStart"), "session_start");
        assert_eq!(snake("UserPromptSubmit"), "user_prompt_submit");
        assert_eq!(snake("Stop"), "stop");
    }
}
