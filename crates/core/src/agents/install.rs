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

use super::{codex, Agent};

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
    // https://opencode.ai/docs/
    Tool {
        name: "opencode",
        label: "OpenCode",
        marker: &[("▣", Some((20, 184, 166)))],
        agent: None,
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

/// Where a command is, as a session's pane would find it: on this
/// process's PATH, else where the user's login shell resolves it (`$SHELL
/// -lc 'command -v <bin>'`: the daemon may run with a shorter PATH than
/// the user's shell has). The shell's answer is remembered for a minute
/// (it can be slow to start), and it gets three seconds.
pub fn resolve(bin: &str) -> Option<PathBuf> {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};
    use std::time::{Duration, Instant};
    if let Some(p) = on_path(bin) {
        return Some(p);
    }
    type Asked = HashMap<String, (Instant, Option<PathBuf>)>;
    static ASKED: LazyLock<Mutex<Asked>> = LazyLock::new(Default::default);
    if let Some((at, found)) = ASKED.lock().ok()?.get(bin) {
        if at.elapsed() < Duration::from_secs(60) {
            return found.clone();
        }
    }
    let found = login_shell_path(bin, Duration::from_secs(3));
    if let Ok(mut asked) = ASKED.lock() {
        asked.insert(bin.to_string(), (Instant::now(), found.clone()));
    }
    found
}

/// `$SHELL -lc 'command -v <bin>'`: an absolute path, or nothing.
fn login_shell_path(bin: &str, limit: std::time::Duration) -> Option<PathBuf> {
    use std::io::Read;
    // (Only ever rlord's own agent names: never text from outside.)
    if !bin.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    let mut child = std::process::Command::new(shell)
        .args(["-lc", &format!("command -v {bin}")])
        .env_remove("RLORD_SESSION_ID")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => break,
            Ok(None) if start.elapsed() < limit => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    // (A shell rc may print before it: the answer is the last line.)
    let p = PathBuf::from(out.lines().next_back()?.trim());
    (p.is_absolute() && p.is_file()).then_some(p)
}

/// What `<bin> --version` prints (its first line), remembered until the
/// binary changes. Bounded: a command that doesn't answer in two seconds
/// has no version.
pub fn version(bin: &Path) -> Option<String> {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};
    use std::time::{Duration, Instant, SystemTime};
    // Per binary: when it was last modified, and what it answered then.
    type Seen = HashMap<PathBuf, (SystemTime, Option<String>)>;
    static SEEN: LazyLock<Mutex<Seen>> = LazyLock::new(Default::default);
    // (Through symlinks: an upgrade replaces what the link points at.)
    let modified = std::fs::metadata(bin).and_then(|m| m.modified()).ok()?;
    if let Some((at, v)) = SEEN.lock().ok()?.get(bin) {
        if *at == modified {
            return v.clone();
        }
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
    /// run it).
    pub hooks_installed: Option<bool>,
    /// Some are, some aren't: installed by an older rlord.
    pub hooks_outdated: bool,
    /// Codex only: the user trusted rlord's hooks (`/hooks`).
    pub hooks_trusted: Option<bool>,
    /// How to install it, when it isn't.
    pub install: Option<Suggestion>,
    /// What's left to do, in a line (None: ready).
    pub next: Option<String>,
}

/// Every known agent's state. `versions`: also run each one's `--version`
/// (cached; skip it where only "is it there" matters).
pub fn status(versions: bool) -> Vec<Status> {
    let machine = Machine::detect();
    TOOLS
        .iter()
        .map(|t| {
            let path = resolve(t.name);
            let installed = path.is_some();
            let missing = t.agent.map(|a| hooks_missing(a, &a.hooks_file()));
            let hooks_installed = missing.as_ref().map(Vec::is_empty);
            let hooks_outdated = t
                .agent
                .zip(missing.as_ref())
                .is_some_and(|(a, m)| !m.is_empty() && m.len() < a.hook_events().len());
            let hooks_trusted = match t.agent {
                Some(Agent::Codex) => codex_hooks_trusted(&codex::home()),
                _ => None,
            };
            let next = match (installed, t.agent, hooks_installed, hooks_trusted) {
                (false, _, _, _) => Some(format!(
                    "not installed: run the install command yourself{}",
                    if t.agent.is_some() {
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
                version: path.as_deref().filter(|_| versions).and_then(version),
                path: path.map(|p| p.to_string_lossy().into_owned()),
                hooks_installed,
                hooks_outdated,
                hooks_trusted,
                install: (!installed).then(|| suggest(t, machine)),
                next,
            }
        })
        .collect()
}

/// Why sessions on `agent` can't launch here, with what to do about it;
/// None when its command can be found (on PATH, or by the user's login
/// shell).
pub fn missing(agent: Agent) -> Option<String> {
    if resolve(agent.name()).is_some() {
        return None;
    }
    let t = tool(agent.name())?;
    let s = suggest(t, Machine::detect());
    Some(format!(
        "{} isn't installed (`{}` isn't on PATH). rlord doesn't install it for you; run this \
         yourself: {}  (other ways: {}). Then `rlord install-hooks`.",
        t.label, t.name, s.command, s.docs
    ))
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
        // Known, hinted, not runnable yet.
        assert_eq!(tool("opencode").unwrap().agent, None);
        // Nothing here installs with elevated rights or runs unprompted.
        for t in &TOOLS {
            for c in [t.script, t.brew, t.npm] {
                assert!(!c.contains("sudo"), "{c}");
            }
        }
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
