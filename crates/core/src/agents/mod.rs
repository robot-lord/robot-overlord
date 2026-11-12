//! The coding agents rlord runs, behind one interface.
//!
//! `Agent` is what the rest of rlord asks: how to launch, resume and start
//! fresh, where the conversation id comes from, how hooks are installed and
//! what each hook event means, where transcripts live, and what the agent
//! can't do (Remote Control, sub-agents, an overlord seat). Each agent's
//! own facts live in its module (`claude`, `codex`); transcripts are read
//! through `transcript`, whichever agent wrote them.
//!
//! Adding an agent: a module with its argv and its own facts, a variant
//! here, and a `transcript::Format`.
//!
//! Two things are deliberately not part of this interface, because agents
//! differ in them:
//!
//! - **Where status comes from.** Command hooks (`map_hook_event`) are one
//!   source; the daemon's `discover` (the agent's own files) is another. A
//!   source turns what the agent says into a `Status` and writes it the
//!   same way; an agent with an event stream instead of hooks adds a
//!   source, not a hook mapping.
//! - **How a conversation is stored.** Consumers ask `transcript` for
//!   common shapes (excerpts, messages, stats, context size, model, turn
//!   state), never for lines. Claude and Codex both keep JSON lines, so
//!   both are read through `transcript::Line`; a store that isn't a JSONL
//!   file answers those same functions its own way.

pub mod claude;
pub mod codex;
pub mod install;
pub mod remote;
pub mod transcript;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::types::Status;
use transcript::Format;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

/// What a hook event does to a session's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Nothing,
    /// The session is now in this status.
    Set(Status),
    /// The agent moved on from being blocked on the user: `waiting_input`
    /// becomes `working`; any other status stays.
    Resume,
}

impl Effect {
    /// The status a session in `current` goes to, if it changes.
    pub fn apply(self, current: Status) -> Option<Status> {
        match self {
            Effect::Set(s) if s != current => Some(s),
            Effect::Resume if current == Status::WaitingInput => Some(Status::Working),
            _ => None,
        }
    }
}

/// A new conversation's command, and what is known about it before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub argv: Vec<String>,
    /// The conversation id, when rlord chose it (`Agent::mints_id`). None:
    /// the agent assigns one, bound later from its hooks or its own index.
    pub session_id: Option<String>,
    /// Where its transcript will be written, when that follows from the id.
    pub transcript: Option<PathBuf>,
}

impl Agent {
    pub const ALL: [Agent; 2] = [Agent::Claude, Agent::Codex];

    pub fn parse(name: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|a| a.name() == name.trim())
    }

    /// `parse`, or an error naming the agents there are.
    pub fn named(name: &str) -> Result<Agent> {
        Agent::parse(name).with_context(|| {
            let known: Vec<&str> = Agent::ALL.iter().map(|a| a.name()).collect();
            format!("agent '{name}' isn't supported ({})", known.join(", "))
        })
    }

    /// The name stored on sessions and used in config; also its command.
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    /// What to call it when talking to the user.
    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
        }
    }

    /// Its command can be found (PATH, or the user's login shell).
    pub fn installed(self) -> bool {
        install::resolve(self.name()).is_some()
    }

    pub fn format(self) -> Format {
        match self {
            Agent::Claude => Format::Claude,
            Agent::Codex => Format::Codex,
        }
    }

    // ---- identity ----

    /// rlord chooses the conversation id at launch (so the transcript path
    /// is known from the first second). Otherwise the agent assigns it and
    /// rlord learns it afterwards.
    pub fn mints_id(self) -> bool {
        matches!(self, Agent::Claude)
    }

    /// Whether a hook event may move a session to another conversation id.
    /// Claude reports the conversation it is writing on every event. Codex
    /// runs sub-agent threads in the same pane, so only a `SessionStart`
    /// (a new or resumed thread) does.
    pub fn adopts_new_id_on(self, event: &str) -> bool {
        match self {
            Agent::Claude => true,
            Agent::Codex => event == "SessionStart",
        }
    }

    // ---- commands ----

    /// A new conversation in `project`. `extra`: adapter flags that ride in
    /// the session's launch spec.
    pub fn launch(
        self,
        project: &str,
        prompt: Option<&str>,
        model: Option<&str>,
        extra: &[String],
    ) -> Launch {
        match self {
            Agent::Claude => {
                let id = claude::mint_session_id();
                Launch {
                    argv: claude::launch_argv(&id, prompt, model, extra),
                    transcript: Some(claude::transcript_path(project, &id)),
                    session_id: Some(id),
                }
            }
            Agent::Codex => Launch {
                argv: codex::launch_argv(prompt, model, extra),
                session_id: None,
                transcript: None,
            },
        }
    }

    /// Continue conversation `id`: the command, and whether it resumes
    /// (false: the conversation was never written, so it starts under the
    /// same id). A session without a bound id can't be resumed.
    pub fn resume(
        self,
        id: Option<&str>,
        trust: &str,
        project: &str,
        model: Option<&str>,
        extra: &[String],
    ) -> Result<(Vec<String>, bool)> {
        match self {
            Agent::Claude => {
                let id = id.context("session has no agent session id; cannot resume")?;
                let argv = claude::resume_argv(id, trust, project, model, extra)?;
                let resumed = argv.iter().any(|a| a == "--resume");
                Ok((argv, resumed))
            }
            Agent::Codex => {
                let Some(id) = id else {
                    bail!(
                        "rlord never learned this session's Codex conversation id, so it can't \
                         resume it (trust rlord's hooks in Codex with /hooks; see `rlord codex \
                         install-hooks`). Start it fresh, or resume it yourself with `codex resume`"
                    );
                };
                if trust == "scavenged" {
                    bail!("refusing to resume a scavenged session id (untrusted attribution)");
                }
                Ok((codex::resume_argv(id, model, extra), true))
            }
        }
    }

    // ---- hooks ----

    /// (event kind for rlord's log, what it does to the status).
    pub fn map_hook_event(self, event: &str, payload: &Value) -> (&'static str, Effect) {
        match self {
            Agent::Claude => claude::map_hook_event(event, payload),
            Agent::Codex => codex::map_hook_event(event),
        }
    }

    /// The file the agent reads its hooks from.
    pub fn hooks_file(self) -> PathBuf {
        match self {
            Agent::Claude => claude::config_dir().join("settings.json"),
            Agent::Codex => codex::home().join("hooks.json"),
        }
    }

    /// The hook command rlord installs: `<rlord> hook emit <agent>`.
    pub fn hook_command(self, rlord_bin: &str) -> String {
        format!("{rlord_bin} hook emit {}", self.name())
    }

    /// Merge rlord's hook command into the agent's hooks file (see
    /// `merge_hooks`). Returns whether the file changed.
    pub fn install_hooks(self, rlord_bin: &str) -> Result<bool> {
        self.install_hooks_in(&self.hooks_file(), rlord_bin)
    }

    pub fn install_hooks_in(self, file: &Path, rlord_bin: &str) -> Result<bool> {
        merge_hooks(file, &self.hook_command(rlord_bin), self.hook_events())
    }

    /// (event, matcher): every event rlord's hook is installed for.
    pub fn hook_events(self) -> &'static [(&'static str, Option<&'static str>)] {
        match self {
            Agent::Claude => claude::HOOK_EVENTS,
            Agent::Codex => codex::HOOK_EVENTS,
        }
    }

    /// Whether conversation `id` is one the user drives, not a sub-agent's
    /// own thread in the same pane. None when it can't be told (Claude's
    /// sub-agents report under the session's id; Codex's index may not
    /// know the thread yet).
    pub fn is_user_conversation(self, id: &str) -> Option<bool> {
        match self {
            Agent::Claude => None,
            Agent::Codex => codex::is_user_thread(&codex::home(), id),
        }
    }

    /// What the user must still do after the hooks are installed, if anything.
    pub fn hooks_next_step(self) -> Option<&'static str> {
        match self {
            Agent::Claude => None,
            Agent::Codex => Some(
                "Codex runs a hook only once you've trusted it. Trust rlord's once:\n\
                 \x20 1. start Codex (any folder): `codex`\n\
                 \x20 2. type `/hooks`\n\
                 \x20 3. review the `rlord hook emit codex` entries and trust them\n\
                 Until then rlord still runs Codex sessions: it finds each conversation in \
                 Codex's own index and reads working / your turn from its transcript (a \
                 little later, and without \"needs you\").",
            ),
        }
    }

    // ---- transcripts ----

    /// Where conversation `id` (started in `project`) is written, if it can
    /// be told. Claude's follows from the two; Codex's is looked up.
    pub fn transcript_path(self, project: &str, id: &str) -> Option<PathBuf> {
        match self {
            Agent::Claude => Some(claude::transcript_path(project, id)),
            Agent::Codex => codex::rollout_of(&codex::home(), id),
        }
    }

    /// The directory all of this agent's transcripts live under.
    pub fn transcripts_root(self) -> PathBuf {
        match self {
            Agent::Claude => claude::config_dir().join("projects"),
            Agent::Codex => codex::sessions_dir(),
        }
    }

    /// Every transcript of this agent on the machine, with when it was
    /// last written (for searching outside rlord's own sessions too).
    pub fn all_transcripts(self) -> Vec<(SystemTime, PathBuf)> {
        match self {
            Agent::Claude => {
                let mut v = Vec::new();
                for d in std::fs::read_dir(self.transcripts_root())
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                        let p = f.path();
                        if p.extension().is_some_and(|x| x == "jsonl") {
                            let t = f
                                .metadata()
                                .and_then(|m| m.modified())
                                .unwrap_or(SystemTime::UNIX_EPOCH);
                            v.push((t, p));
                        }
                    }
                }
                v
            }
            Agent::Codex => codex::rollouts_in(&self.transcripts_root()),
        }
    }

    /// The folder a transcript's conversation ran in, as well as it can be
    /// told (Claude: its project directory's name, the path with `/` as
    /// `-`; Codex: the folder itself).
    pub fn transcript_folder(self, transcript: &Path) -> String {
        match self {
            Agent::Claude => transcript
                .parent()
                .and_then(|d| d.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Agent::Codex => codex::rollout_cwd(transcript).unwrap_or_default(),
        }
    }

    // ---- what it can't do ----

    /// Remote Control (continuing a session from the agent's app).
    pub fn has_remote_control(self) -> bool {
        matches!(self, Agent::Claude)
    }

    /// rlord tracks its sub-agents.
    pub fn tracks_subagents(self) -> bool {
        matches!(self, Agent::Claude)
    }

    /// It can run the overlord's seats (rlord's MCP server, tool
    /// allow-list, effort). Why not, when it can't.
    pub fn overlord_seat(self) -> Result<()> {
        match self {
            Agent::Claude => Ok(()),
            Agent::Codex => bail!(
                "the overlord runs on Claude Code only for now (Codex seats aren't built yet); \
                 set `[overlord] agent = \"claude\"`"
            ),
        }
    }

    /// Can rlord import its conversations started outside rlord.
    pub fn importable(self) -> bool {
        matches!(self, Agent::Claude)
    }
}

/// The agent a transcript file belongs to (see `Format::of`).
pub fn agent_of(transcript: &Path) -> Agent {
    match Format::of(transcript) {
        Format::Claude => Agent::Claude,
        Format::Codex => Agent::Codex,
    }
}

/// Merge a hook command into an agent's hooks file for each of `events`,
/// preserving everything already there (including other tools' hooks).
/// Claude Code's `settings.json` and Codex's `hooks.json` share the shape:
/// `{"hooks": {"<Event>": [{"matcher"?, "hooks": [{"type": "command", "command": …}]}]}}`.
/// Idempotent; the file as it was is kept once as `<name>.pre-rlord.bak`.
/// Returns true if the file was modified.
pub fn merge_hooks(file: &Path, hook_cmd: &str, events: &[(&str, Option<&str>)]) -> Result<bool> {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut root: Value = match std::fs::read_to_string(file) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("parsing existing {name}"))?
        }
        _ => json!({}),
    };
    if !root.is_object() {
        bail!("{name} is not a JSON object; refusing to modify");
    }
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        bail!("{name} 'hooks' is not an object; refusing to modify");
    }
    let mut changed = false;
    for (event, matcher) in events {
        let arr = hooks
            .as_object_mut()
            .unwrap()
            .entry(*event)
            .or_insert_with(|| json!([]));
        if !arr.is_array() {
            bail!("{name} hooks.{event} is not an array; refusing to modify");
        }
        // Our entry is any command of the form `<path> hook emit ...` from a
        // current or former binary name. If it exists with a stale path
        // (moved/renamed binary), fix it in place; otherwise append.
        // ("overlord", the pre-rename binary, also contains "rlord".)
        let is_ours = |h: &Value| {
            h["command"]
                .as_str()
                .is_some_and(|c| c.contains("hook emit") && c.contains("rlord"))
        };
        let mut found = false;
        for entry in arr.as_array_mut().unwrap().iter_mut() {
            if let Some(hs) = entry["hooks"].as_array_mut() {
                for h in hs.iter_mut() {
                    if is_ours(h) {
                        found = true;
                        if h["command"].as_str() != Some(hook_cmd) {
                            h["command"] = json!(hook_cmd);
                            changed = true;
                        }
                    }
                }
            }
        }
        if !found {
            let mut group = json!({"hooks": [{"type": "command", "command": hook_cmd}]});
            // Only for what the matcher names (a tool, a notification type).
            if let Some(m) = matcher {
                group["matcher"] = json!(m);
            }
            arr.as_array_mut().unwrap().push(group);
            changed = true;
        }
    }
    if changed {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let backup = file.with_extension("json.pre-rlord.bak");
        if file.exists() && !backup.exists() {
            std::fs::copy(file, &backup)?;
        }
        let tmp = file.with_extension("json.rlord-tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&root)? + "\n")?;
        std::fs::rename(&tmp, file)?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_on_clears_only_needs_you() {
        use Status::*;
        assert_eq!(Effect::Resume.apply(WaitingInput), Some(Working));
        for s in [Working, Idle, Stopped, Starting] {
            assert_eq!(Effect::Resume.apply(s), None);
        }
        assert_eq!(Effect::Set(Idle).apply(Working), Some(Idle));
        assert_eq!(Effect::Set(Idle).apply(Idle), None);
        assert_eq!(Effect::Nothing.apply(WaitingInput), None);
    }

    #[test]
    fn names_round_trip() {
        for a in Agent::ALL {
            assert_eq!(Agent::parse(a.name()), Some(a));
        }
        assert_eq!(Agent::parse("gemini"), None);
        let e = Agent::named("gemini").unwrap_err().to_string();
        assert!(e.contains("claude, codex"), "{e}");
    }

    #[test]
    fn claude_mints_its_id_and_codex_is_bound_later() {
        let l = Agent::Claude.launch("/src/app", Some("hi"), Some("opus"), &[]);
        let id = l.session_id.clone().expect("minted");
        assert_eq!(
            l.argv,
            ["claude", "--session-id", &id, "--model", "opus", "hi"]
        );
        assert!(l.transcript.unwrap().ends_with(format!("{id}.jsonl")));
        let l = Agent::Codex.launch("/src/app", Some("hi"), None, &[]);
        assert_eq!(l.argv, ["codex", "--no-daemon", "--no-alt-screen", "hi"]);
        assert_eq!((l.session_id, l.transcript), (None, None));
        assert!(Agent::Claude.mints_id() && !Agent::Codex.mints_id());
    }

    #[test]
    fn resume_needs_a_bound_id() {
        let (argv, resumed) = Agent::Codex
            .resume(Some("t-1"), "hook", "/src/app", Some("gpt-6-luna"), &[])
            .unwrap();
        assert_eq!(
            argv,
            [
                "codex",
                "resume",
                "t-1",
                "--no-daemon",
                "--no-alt-screen",
                "--model",
                "gpt-6-luna"
            ]
        );
        assert!(resumed);
        let e = Agent::Codex
            .resume(None, "minted", "/src/app", None, &[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("can't") && e.contains("/hooks"), "{e}");
        assert!(Agent::Claude
            .resume(None, "minted", "/src/app", None, &[])
            .is_err());
        // No transcript for this id: Claude starts it under the same id.
        let (argv, resumed) = Agent::Claude
            .resume(Some("u-1"), "minted", "/nonexistent", None, &[])
            .unwrap();
        assert_eq!((argv[1].as_str(), resumed), ("--session-id", false));
    }

    #[test]
    fn only_a_session_start_moves_a_codex_session_to_another_thread() {
        assert!(Agent::Claude.adopts_new_id_on("Stop"));
        assert!(Agent::Codex.adopts_new_id_on("SessionStart"));
        assert!(!Agent::Codex.adopts_new_id_on("Stop"));
    }

    #[test]
    fn hook_install_is_idempotent_and_preserving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"other-tool hook-handler"}]}]}}"#,
        )
        .unwrap();
        assert!(Agent::Claude.install_hooks_in(&path, "/bin/rlord").unwrap());
        assert!(!Agent::Claude.install_hooks_in(&path, "/bin/rlord").unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["model"], "opus"); // untouched
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2); // the other tool's entry preserved, ours appended
        assert_eq!(
            stop[1]["hooks"][0]["command"],
            "/bin/rlord hook emit claude"
        );
        assert!(v["hooks"]["UserPromptSubmit"].as_array().unwrap().len() == 1);
        // Every tool finishing is heard; a tool starting only when it's the
        // question tool.
        assert!(v["hooks"]["PostToolUse"][0]["matcher"].is_null());
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "AskUserQuestion");
        assert!(path.with_extension("json.pre-rlord.bak").exists());
    }

    #[test]
    fn codex_hooks_go_in_hooks_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        // No file yet: created, nothing to back up.
        assert!(Agent::Codex.install_hooks_in(&path, "/bin/rlord").unwrap());
        assert!(!path.with_extension("json.pre-rlord.bak").exists());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut events: Vec<&String> = v["hooks"].as_object().unwrap().keys().collect();
        events.sort();
        assert_eq!(
            events,
            [
                "PermissionRequest",
                "PostToolUse",
                "SessionEnd",
                "SessionStart",
                "Stop",
                "UserPromptSubmit"
            ]
        );
        assert_eq!(
            v["hooks"]["SessionStart"][0]["hooks"][0],
            json!({"type": "command", "command": "/bin/rlord hook emit codex"})
        );
        // The binary moved: its entries follow, nothing is added twice.
        assert!(Agent::Codex.install_hooks_in(&path, "/opt/rlord").unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/opt/rlord hook emit codex"
        );
        assert!(path.with_extension("json.pre-rlord.bak").exists());
    }
}
