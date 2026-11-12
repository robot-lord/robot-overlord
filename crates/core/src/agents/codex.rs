//! Codex CLI adapter.
//!
//! Identity model: Codex assigns the conversation (thread) id itself, so
//! rlord can't choose one at launch as it does for Claude. The id is bound
//! afterwards: by the `SessionStart` hook (which carries `session_id` and
//! `transcript_path`), or, while rlord's hooks aren't trusted in Codex yet,
//! from Codex's own thread index (`state_5.sqlite`, table `threads`): the
//! thread started in the session's folder after it was launched.
//!
//! Everything under `$CODEX_HOME` is read, never written, except
//! `hooks.json` by `rlord codex install-hooks`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use super::transcript::{clip, Title, Usage};
use super::{Effect, Indexed};
use crate::types::Status;

/// `~/.codex` or `$CODEX_HOME`.
pub fn home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".codex")
        })
}

/// Where Codex keeps its transcripts ("rollouts"):
/// `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`.
pub fn sessions_dir() -> PathBuf {
    home().join("sessions")
}

fn with_model(mut argv: Vec<String>, model: Option<&str>, extra: &[String]) -> Vec<String> {
    // In the pane itself, not Codex's shared background app-server: there
    // the turn (and its hooks) would run outside the pane, without the
    // environment that tells rlord which session a hook is from.
    argv.push("--no-daemon".to_string());
    // Inline, not the alternate screen: tmux keeps the scrollback, so
    // copy-mode scrolling and search work (as Claude's `claude_tui =
    // "default"` does).
    argv.push("--no-alt-screen".to_string());
    if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
        argv.push("--model".to_string());
        argv.push(m.to_string());
    }
    argv.extend(extra.iter().cloned());
    argv
}

/// A new conversation. (Codex has no flag for choosing its id.)
pub fn launch_argv(
    first_prompt: Option<&str>,
    model: Option<&str>,
    extra: &[String],
) -> Vec<String> {
    let mut argv = with_model(vec!["codex".to_string()], model, extra);
    argv.extend(first_prompt.map(str::to_string));
    argv
}

/// `codex resume <id>`: the conversation continues in the same thread.
pub fn resume_argv(session_id: &str, model: Option<&str>, extra: &[String]) -> Vec<String> {
    with_model(
        vec![
            "codex".to_string(),
            "resume".to_string(),
            session_id.to_string(),
        ],
        model,
        extra,
    )
}

/// A TOML basic string (for a `-c key=value` override's value).
pub fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The flags a Codex overlord always runs with (in `dir`, its own folder):
/// rlord's MCP server, its tools called without a prompt, and nothing else
/// loosened. All as `-c` overrides: nothing is written to `~/.codex`.
///
/// - `mcp_servers.rlord.*`: the server (`rlord mcp --socket <socket>`: the
///   daemon that launched it, whatever the environment says), with
///   `default_tools_approval_mode = "approve"` (Codex's per-server
///   counterpart of an allow-list) and `env_vars` passing on what it reads
///   (`MCP_ENV`; Codex starts a server with a short environment).
/// - `sandbox_mode = "read-only"`, `approval_policy = "on-request"`:
///   commands run in the read-only sandbox without a prompt, and anything
///   that needs more (a write, the network, leaving the sandbox) asks the
///   user. ("untrusted", which would ask before every command not known to
///   be read-only, is refused by Codex 0.167: "no longer supported".) The
///   sandbox stops a shell command reaching rlord's tmux socket (on macOS
///   it refuses the connection, checked live; on Linux that is
///   unverified). It does not stop the overlord typing into other
///   sessions: rlord's own `send_to_session` does that, and like rlord's
///   other tools it runs without a prompt, by design. rlord expects to be
///   run by its user on a machine they trust, or over a remote connection
///   they trust; little untrusted text reaches the overlord (it works from
///   rlord's own data and the user's requests), so it can type into
///   sessions without asking, which is what lets it orchestrate.
/// - `confirm_sends` (`[overlord] confirm_sends`, off by default):
///   `mcp_servers.rlord.tools.send_to_session.approval_mode = "prompt"`,
///   so that one tool asks each time (for anyone who wants that).
/// - `check_for_update_on_startup = false`: no update prompt in front of
///   the composer (rlord types into it once it is up).
/// - `projects = {"<dir>" = {trust_level = "trusted"}}`: its folder is
///   rlord's own, so Codex doesn't stop to ask about it, and reads the
///   sub-agents rlord writes to its `.codex/agents`.
pub fn overlord_args(rlord_bin: &str, socket: &str, dir: &str, confirm_sends: bool) -> Vec<String> {
    let args: Vec<String> = super::mcp_args(socket)
        .iter()
        .map(|a| toml_str(a))
        .collect();
    let env: Vec<String> = super::MCP_ENV.iter().map(|v| toml_str(v)).collect();
    [
        format!("mcp_servers.rlord.command={}", toml_str(rlord_bin)),
        format!("mcp_servers.rlord.args=[{}]", args.join(",")),
        format!("mcp_servers.rlord.env_vars=[{}]", env.join(",")),
        "mcp_servers.rlord.default_tools_approval_mode=\"approve\"".to_string(),
        // This replaces the user's whole `projects` table, for this run
        // only: their other folders' trust is unknown to this process (it
        // is still in ~/.codex/config.toml, untouched). The overlord works
        // in its own folder, so nothing it does needs theirs. (A whole
        // table because a dotted override path can't hold a quoted key: it
        // is accepted and ignored, and Codex asks about the folder at every
        // start.)
        format!("projects={{{}={{trust_level=\"trusted\"}}}}", toml_str(dir)),
        "sandbox_mode=\"read-only\"".to_string(),
        "approval_policy=\"on-request\"".to_string(),
        "check_for_update_on_startup=false".to_string(),
    ]
    .into_iter()
    .chain(
        confirm_sends.then(|| {
            "mcp_servers.rlord.tools.send_to_session.approval_mode=\"prompt\"".to_string()
        }),
    )
    .flat_map(|kv| ["-c".to_string(), kv])
    .collect()
}

/// Codex's screen shows its composer, ready for a prompt. Only the bottom
/// input area counts (its last few lines: the composer and the footer
/// under it): the lowest line there that starts with `›` (its prompt mark)
/// must not be a numbered choice. The same mark points at the picked
/// option in a dialog (trust this folder, update now, approve a command),
/// which Codex draws in place of the composer; a `›` higher up is a prompt
/// in the history, not the input. Startup screens before it (an update
/// prompt, a login, a trust question) and an approval don't count.
pub fn composer_shown(screen: &str) -> bool {
    // A dialog's footer sits under its choices: only lines below the
    // lowest `›` count (the same words in the transcript above don't).
    let mut footer = false;
    for l in super::bottom_lines(screen, 6) {
        if let Some(rest) = l.strip_prefix('›') {
            return !footer && !super::numbered_choice(rest.trim_start());
        }
        footer |= dialog_footer(l);
    }
    false
}

/// A dialog's own footer ("Press enter to confirm or esc to cancel", an
/// MCP approval's "enter to submit | esc to cancel"). The composer's says
/// "esc to interrupt" while a turn runs.
fn dialog_footer(line: &str) -> bool {
    ["Press enter to confirm", "enter to submit", "esc to cancel"]
        .iter()
        .any(|m| line.contains(m))
}

/// What is typed in the composer (`screen` captured with attributes,
/// `capture-pane -e`), if anything: the text after the lowest `›` in the
/// input area. Its placeholder ("Ask Codex to do anything") is drawn dim
/// and isn't a draft.
pub fn draft(screen: &str) -> Option<String> {
    let line = screen
        .lines()
        .rev()
        .filter(|l| !super::strip_ansi(l).trim().is_empty())
        .take(6)
        .find(|l| super::strip_ansi(l).starts_with('›'))?;
    let rest = line.split_once('›')?.1;
    let text = super::undimmed(rest);
    let text = text.trim_matches([' ', '\u{a0}']);
    (!text.is_empty()).then(|| text.to_string())
}

/// A numbered-choice dialog is at the bottom of Codex's screen: an
/// approval, a trust or update question, the login choice (marked `›` or
/// `>` at the picked option), or a dialog's footer (`dialog_footer`).
pub fn dialog_shown(screen: &str) -> bool {
    super::bottom_lines(screen, 12).any(|l| {
        dialog_footer(l)
            || l.trim_start()
                .strip_prefix(['›', '>'])
                .is_some_and(|rest| super::numbered_choice(rest.trim_start()))
    })
}

/// A sub-agent file for Codex (`.codex/agents/<name>.toml`): its name, when
/// to use it, and its instructions. It inherits the overlord's MCP servers
/// and approvals (a file can't narrow them), and runs at low effort: the
/// passes it is for are routine.
pub fn subagent_toml(name: &str, description: &str, instructions: &str) -> String {
    format!(
        "name = {}\ndescription = {}\nmodel_reasoning_effort = \"low\"\ndeveloper_instructions = {}\n",
        toml_str(name),
        toml_str(description),
        toml_str(instructions.trim()),
    )
}

/// Map a Codex hook event to (event kind for our log, what it does to the
/// status). A permission prompt is its "needs you"; it ends when the tool
/// it was for has run (PostToolUse).
pub fn map_hook_event(event: &str) -> (&'static str, Effect) {
    use Effect::{Nothing, Resume, Set};
    match event {
        "SessionStart" => ("session_start", Set(Status::Idle)),
        "UserPromptSubmit" => ("prompt_submitted", Set(Status::Working)),
        "Stop" => ("agent_stopped", Set(Status::Idle)),
        "PermissionRequest" => ("permission_request", Set(Status::WaitingInput)),
        "PostToolUse" => ("tool", Resume),
        "SessionEnd" => ("session_end", Set(Status::Stopped)),
        _ => ("hook_other", Nothing),
    }
}

/// (event, matcher). PostToolUse fires on every tool call; the handler
/// does nothing unless the session was waiting on the user.
pub(crate) const HOOK_EVENTS: &[(&str, Option<&str>)] = &[
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    ("Stop", None),
    ("PermissionRequest", None),
    ("PostToolUse", None),
    ("SessionEnd", None),
];

// ---- Codex's thread index ----

/// A row of Codex's `threads` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub id: String,
    pub rollout_path: String,
    pub created_at_ms: i64,
    /// The prompt that started it ("" until Codex records one). None: this
    /// Codex's index doesn't keep it.
    pub first_user_message: Option<String>,
}

fn open_state(home: &Path) -> Option<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    let db = home.join("state_5.sqlite");
    if !db.exists() {
        return None;
    }
    let c = rusqlite::Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    // Codex may be writing: wait a little, never long.
    c.busy_timeout(std::time::Duration::from_millis(200)).ok()?;
    Some(c)
}

/// Threads the user started from the CLI in `cwd` at or after `since_ms`,
/// oldest first (at most 8): candidates for the conversation a session
/// just launched there. Not sub-agent threads, nor ones started from an
/// editor or the desktop app in the same folder. `cwd` is matched as given
/// and resolved (symlinks).
pub fn threads_started_in(home: &Path, cwd: &str, since_ms: i64) -> Vec<Thread> {
    let Some(c) = open_state(home) else {
        return Vec::new();
    };
    let real = std::fs::canonicalize(cwd)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| cwd.to_string());
    // An older Codex's index has neither where a thread was started from
    // nor its first message: then every thread in the folder is a
    // candidate, with no message to tell them apart (None).
    let (mut q, full) = match c.prepare(
        "SELECT id, rollout_path, created_at_ms, first_user_message FROM threads \
         WHERE cwd IN (?1, ?2) AND created_at_ms >= ?3 AND source = 'cli' \
           AND (thread_source IS NULL OR thread_source = 'user') \
         ORDER BY created_at_ms ASC LIMIT 8",
    ) {
        Ok(q) => (q, true),
        Err(_) => match c.prepare(
            "SELECT id, rollout_path, created_at_ms FROM threads \
             WHERE cwd IN (?1, ?2) AND created_at_ms >= ?3 \
             ORDER BY created_at_ms ASC LIMIT 8",
        ) {
            Ok(q) => (q, false),
            Err(_) => return Vec::new(), // not a schema rlord knows
        },
    };
    let rows = q.query_map(rusqlite::params![cwd, real, since_ms], |r| {
        Ok(Thread {
            id: r.get(0)?,
            rollout_path: r.get(1)?,
            created_at_ms: r.get(2)?,
            first_user_message: if full {
                Some(r.get::<_, Option<String>>(3)?.unwrap_or_default())
            } else {
                None
            },
        })
    });
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// What the index says about thread `id`: the user's, or a sub-agent's.
/// An older Codex's index doesn't say whose a thread is: there a listed
/// thread counts as the user's.
pub fn thread_kind(home: &Path, id: &str) -> Indexed {
    use rusqlite::OptionalExtension;
    let Some(c) = open_state(home) else {
        return Indexed::NoIndex;
    };
    let source = c
        .query_row(
            "SELECT thread_source FROM threads WHERE id = ?1",
            [id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional();
    match source {
        Ok(Some(source)) if source.as_deref().is_none_or(|s| s == "user") => Indexed::User,
        Ok(Some(_)) => Indexed::SubAgent,
        Ok(None) => Indexed::NotListed,
        // No such column: is the thread there at all?
        Err(_) => {
            let listed = c
                .query_row("SELECT 1 FROM threads WHERE id = ?1", [id], |_| Ok(()))
                .optional();
            match listed {
                Ok(Some(())) => Indexed::User,
                Ok(None) => Indexed::NotListed,
                Err(_) => Indexed::NoIndex, // not an index rlord can read
            }
        }
    }
}

/// The folder each indexed rollout's conversation ran in (one query).
/// None when there is no index to ask; a rollout it doesn't list (older
/// than the index) says its own folder (`rollout_cwd`).
pub fn rollout_folders(home: &Path) -> Option<std::collections::HashMap<PathBuf, String>> {
    let c = open_state(home)?;
    let mut q = c.prepare("SELECT rollout_path, cwd FROM threads").ok()?;
    let rows = q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .ok()?;
    Some(
        rows.flatten()
            .map(|(p, cwd)| (PathBuf::from(p), cwd))
            .collect(),
    )
}

/// The rollout file of a conversation: from the thread index, else by its
/// name under `sessions/` (the file name ends in the id).
pub fn rollout_of(home: &Path, id: &str) -> Option<PathBuf> {
    // A rollout never moves: remember where each was found.
    static FOUND: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<(PathBuf, String), PathBuf>>,
    > = std::sync::LazyLock::new(Default::default);
    let key = (home.to_path_buf(), id.to_string());
    let known = FOUND.lock().ok().and_then(|f| f.get(&key).cloned());
    if let Some(p) = known.filter(|p| p.exists()) {
        return Some(p);
    }
    let found = find_rollout(home, id)?;
    if let Ok(mut f) = FOUND.lock() {
        f.insert(key, found.clone());
    }
    Some(found)
}

fn find_rollout(home: &Path, id: &str) -> Option<PathBuf> {
    let indexed = open_state(home).and_then(|c| {
        c.query_row(
            "SELECT rollout_path FROM threads WHERE id = ?1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .ok()
    });
    if let Some(p) = indexed.map(PathBuf::from).filter(|p| p.exists()) {
        return Some(p);
    }
    let suffix = format!("-{id}.jsonl");
    rollouts_in(&home.join("sessions"))
        .into_iter()
        .map(|(_, p)| p)
        .find(|p| p.to_string_lossy().ends_with(&suffix))
}

/// Every rollout under a `sessions` directory, with when it was written.
pub fn rollouts_in(sessions: &Path) -> Vec<(SystemTime, PathBuf)> {
    fn walk(dir: &Path, depth: u8, out: &mut Vec<(SystemTime, PathBuf)>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if depth > 0 {
                if p.is_dir() {
                    walk(&p, depth - 1, out);
                }
            } else if p.extension().is_some_and(|x| x == "jsonl")
                && e.file_name().to_string_lossy().starts_with("rollout-")
            {
                let t = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                out.push((t, p));
            }
        }
    }
    let mut out = Vec::new();
    // sessions/YYYY/MM/DD/rollout-*.jsonl
    walk(sessions, 3, &mut out);
    out
}

/// The folder a rollout's conversation ran in, from its first bytes (the
/// `session_meta` line starts with it; that line can be very long).
pub fn rollout_cwd(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(16 * 1024)
        .read_to_end(&mut buf)
        .ok()?;
    let text = String::from_utf8_lossy(&buf);
    let at = text.find("\"cwd\":")? + "\"cwd\":".len();
    serde_json::Deserializer::from_str(&text[at..])
        .into_iter::<String>()
        .next()?
        .ok()
}

// ---- reading rollout lines (see `transcript::Line`) ----

/// The payload of a `response_item` line.
fn item(v: &Value) -> Option<&Value> {
    (v["type"] == "response_item").then(|| &v["payload"])
}

/// The payload of an `event_msg` line of the given kind.
fn event<'a>(v: &'a Value, kind: &str) -> Option<&'a Value> {
    (v["type"] == "event_msg" && v["payload"]["type"] == kind).then(|| &v["payload"])
}

/// The text parts of a message item (`input_text` / `output_text`).
fn content_text(p: &Value, part: &str) -> Option<String> {
    let t = p["content"]
        .as_array()?
        .iter()
        .filter(|x| x["type"] == part)
        .filter_map(|x| x["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!t.trim().is_empty()).then_some(t)
}

/// What Codex injects as user messages: each starts with one of these
/// (the tagged ones also close their tag). Named one by one, so a real
/// prompt that happens to start with `<` is kept.
const INJECTED_TAGS: &[&str] = &[
    "environment_context",
    "user_instructions",
    "INSTRUCTIONS",
    "turn_aborted",
    "user_shell_command",
    "subagent_notification",
    "skill",
    "permissions instructions",
];

fn injected(text: &str) -> bool {
    let t = text.trim_start();
    if t.starts_with("# AGENTS.md instructions for ") {
        return true;
    }
    INJECTED_TAGS.iter().any(|tag| {
        t.strip_prefix('<')
            .and_then(|r| r.strip_prefix(tag))
            .is_some_and(|r| r.starts_with('>'))
            && t.contains(&format!("</{tag}>"))
    })
}

/// A prompt the human typed. Codex also writes context it injects as user
/// messages (`<environment_context>`, `<user_instructions>`, the AGENTS.md
/// instructions, …): not those.
pub(crate) fn human_prompt(v: &Value) -> Option<String> {
    let p = item(v)?;
    if p["type"] != "message" || p["role"] != "user" {
        return None;
    }
    let t = content_text(p, "input_text")?;
    let t = t.trim();
    (!injected(t)).then(|| t.to_string())
}

pub(crate) fn reply_text(v: &Value) -> Option<String> {
    let p = item(v)?;
    (p["type"] == "message" && p["role"] == "assistant")
        .then(|| content_text(p, "output_text"))
        .flatten()
}

/// A tool call's main argument, from its JSON arguments.
fn main_arg(arguments: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(arguments).ok();
    let named = parsed.as_ref().and_then(|a| {
        [
            "cmd",
            "command",
            "file_path",
            "path",
            "pattern",
            "query",
            "url",
            "description",
        ]
        .iter()
        .find_map(|k| match &a[*k] {
            Value::String(s) => Some(s.clone()),
            // An argv: `["bash", "-lc", "cargo test"]`.
            Value::Array(parts) => Some(
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        })
    });
    named.unwrap_or_else(|| clip(arguments, 200))
}

/// What a tool returned: a string, or `{content}` / a list of text parts.
fn output_text(out: &Value) -> Option<String> {
    let t = match out {
        Value::String(s) => s.clone(),
        Value::Object(_) => match &out["content"] {
            Value::String(s) => s.clone(),
            other => return output_text(other),
        },
        Value::Array(parts) => parts
            .iter()
            .filter_map(|x| x["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!t.trim().is_empty()).then_some(t)
}

/// A message's text and who said it. Developer messages and injected
/// context aren't conversation; reasoning isn't shown.
pub(crate) fn message(v: &Value) -> Option<(&'static str, String)> {
    let p = item(v)?;
    let name = || p["name"].as_str().unwrap_or("tool");
    match p["type"].as_str()? {
        "message" => match p["role"].as_str()? {
            "user" => human_prompt(v).map(|t| ("you", t)),
            "assistant" => reply_text(v).map(|t| ("agent", t)),
            _ => None,
        },
        // What it ran: the tool and its main argument.
        "function_call" => Some((
            "agent",
            format!(
                "[{}] {}",
                name(),
                main_arg(p["arguments"].as_str().unwrap_or(""))
            ),
        )),
        "custom_tool_call" => Some((
            "agent",
            format!(
                "[{}] {}",
                name(),
                clip(p["input"].as_str().unwrap_or(""), 200)
            ),
        )),
        "local_shell_call" => Some((
            "agent",
            format!("[shell] {}", main_arg(&p["action"].to_string())),
        )),
        "function_call_output" | "custom_tool_call_output" => {
            output_text(&p["output"]).map(|t| ("tool", t))
        }
        _ => None,
    }
}

/// A name the user gave the thread.
pub(crate) fn title(v: &Value) -> Option<Title> {
    event(v, "thread_name_updated")?["thread_name"]
        .as_str()
        .filter(|t| !t.trim().is_empty())
        .map(|t| Title::User(t.to_string()))
}

/// `token_count` events carry the conversation's running totals and the
/// last request's usage (its input is the whole context sent).
pub(crate) fn usage(v: &Value) -> Option<Usage> {
    let info = &event(v, "token_count")?["info"];
    let total = info["total_token_usage"].as_object()?;
    let n = |k: &str| total.get(k).and_then(Value::as_i64).unwrap_or(0);
    Some(Usage {
        input: n("input_tokens"),
        output: n("output_tokens"),
        context: info["last_token_usage"]["input_tokens"]
            .as_u64()
            .unwrap_or(0),
        // (Its input counts cached tokens too.)
        fresh: (n("input_tokens") - n("cached_input_tokens")).max(0) + n("output_tokens"),
        window: info["model_context_window"].as_u64().filter(|w| *w > 0),
        cumulative: true,
    })
}

/// The model a turn ran on (`turn_context`, or the thread's settings).
pub(crate) fn model(v: &Value) -> Option<&str> {
    let m = if v["type"] == "turn_context" {
        v["payload"]["model"].as_str()
    } else {
        event(v, "thread_settings_applied")?["thread_settings"]["model"].as_str()
    };
    m.filter(|m| !m.is_empty())
}

pub(crate) fn turn_ended(v: &Value) -> Option<bool> {
    if v["type"] != "event_msg" {
        return None;
    }
    match v["payload"]["type"].as_str()? {
        "task_started" => Some(false),
        "task_complete" | "turn_aborted" => Some(true),
        _ => None,
    }
}

// ---- models ----

/// The models Codex offers in its own picker, best first, from the catalog
/// it caches (`models_cache.json`). Empty when there is none (not logged
/// in, never run): the model is then typed in.
pub fn models(home: &Path) -> Vec<String> {
    let Some(v) = std::fs::read_to_string(home.join("models_cache.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    else {
        return Vec::new();
    };
    let mut listed: Vec<(i64, String)> = v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"].as_str().is_none_or(|vis| vis == "list"))
        .filter_map(|m| {
            Some((
                m["priority"].as_i64().unwrap_or(i64::MAX),
                m["slug"].as_str()?.to_string(),
            ))
        })
        .collect();
    listed.sort();
    listed.into_iter().map(|(_, slug)| slug).collect()
}

// ---- archive summaries ----

/// `codex exec` flags for a one-shot answer that saves nothing
/// (`--ephemeral`) and can't change anything (read-only sandbox, no
/// approvals to wait on). `fork`: the conversation to read in full (a fork
/// of it; the session's own thread is untouched) and the question; without
/// one the prompt arrives on stdin. The answer is written to `out`.
pub fn summary_argv(model: &str, fork: Option<(&str, &str)>, out: &Path) -> Vec<String> {
    let mut a: Vec<String> = vec!["codex".into(), "exec".into()];
    if let Some((id, _)) = fork {
        a.extend(["fork".to_string(), id.to_string()]);
    }
    a.extend(
        [
            "--ephemeral",
            "--skip-git-repo-check",
            "-c",
            "sandbox_mode=\"read-only\"",
            "-c",
            "approval_policy=\"never\"",
        ]
        .map(String::from),
    );
    if !model.is_empty() {
        a.extend(["--model".to_string(), model.to_string()]);
    }
    a.extend(["-o".to_string(), out.to_string_lossy().into_owned()]);
    a.push(match fork {
        Some((_, ask)) => ask.to_string(),
        None => "-".to_string(),
    });
    a
}

#[cfg(test)]
mod tests {
    use super::super::transcript::{self, Format};
    use super::*;

    #[test]
    fn footer_words_in_the_transcript_dont_hide_the_composer() {
        let idle = "• Press esc to cancel, or enter to submit.\n\n› \n\n  ? for shortcuts    100% context left";
        assert!(composer_shown(idle));
        let dialog = "Allow it?\n› 1. Yes\n  2. No\n  Press enter to confirm or esc to cancel";
        assert!(!composer_shown(dialog));
    }
    use serde_json::json;

    /// A rollout as Codex 0.167 writes one: two turns, a tool call, the
    /// context Codex injects, token counts.
    pub(crate) fn rollout_lines() -> Vec<Value> {
        let ts = |s: &str| format!("2026-11-12T05:56:{s}Z");
        let msg = |role: &str, part: &str, text: &str| {
            json!({"type": "message", "role": role,
                   "content": [{"type": part, "text": text}]})
        };
        let tokens = |input: i64, output: i64, last: i64| {
            json!({"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": input, "cached_input_tokens": 100,
                                      "output_tokens": output, "total_tokens": input + output},
                "last_token_usage": {"input_tokens": last, "output_tokens": 5},
                "model_context_window": 258400}})
        };
        let line = |at: &str, ty: &str, payload: Value| json!({"timestamp": ts(at), "type": ty, "payload": payload});
        vec![
            line(
                "02.272",
                "session_meta",
                json!({"id": "01a1ce53-c71d-7e13-b376-63f7271fa320",
                "cwd": "/src/app", "originator": "codex-tui", "cli_version": "0.167.2",
                "base_instructions": {"text": "You are Codex"}}),
            ),
            line(
                "02.273",
                "event_msg",
                json!({"type": "task_started", "turn_id": "t1"}),
            ),
            line(
                "02.385",
                "response_item",
                msg("developer", "input_text", "You are Codex, an agent"),
            ),
            line(
                "02.386",
                "response_item",
                msg(
                    "user",
                    "input_text",
                    "<environment_context>\n  <cwd>/src/app</cwd>\n</environment_context>",
                ),
            ),
            line(
                "02.387",
                "turn_context",
                json!({"turn_id": "t1", "cwd": "/src/app", "model": "gpt-6-luna"}),
            ),
            line(
                "02.388",
                "event_msg",
                json!({"type": "thread_settings_applied",
                "thread_settings": {"model": "gpt-6-luna"}}),
            ),
            line(
                "02.390",
                "response_item",
                msg("user", "input_text", "fix the flaky login test"),
            ),
            line(
                "02.391",
                "event_msg",
                json!({"type": "item_completed",
                "item": {"type": "UserMessage", "content": [{"type": "text", "text": "fix the flaky login test"}]}}),
            ),
            line(
                "03.000",
                "response_item",
                json!({"type": "reasoning", "summary": [], "encrypted_content": "x"}),
            ),
            line(
                "03.100",
                "response_item",
                json!({"type": "function_call", "name": "exec_command",
                "arguments": "{\"cmd\":\"cargo test login\",\"workdir\":\"/src/app\"}", "call_id": "c1"}),
            ),
            line(
                "04.000",
                "response_item",
                json!({"type": "function_call_output", "call_id": "c1",
                "output": "test login::retries ... FAILED"}),
            ),
            line(
                "05.000",
                "response_item",
                msg("assistant", "output_text", "The retry never fired; fixed."),
            ),
            line(
                "05.001",
                "token_usage_record",
                json!({"usage": {"input_tokens": 900, "output_tokens": 40}}),
            ),
            line("05.002", "event_msg", tokens(900, 40, 900)),
            line(
                "05.003",
                "event_msg",
                json!({"type": "task_complete", "turn_id": "t1",
                "last_agent_message": "The retry never fired; fixed."}),
            ),
            line(
                "09.000",
                "event_msg",
                json!({"type": "task_started", "turn_id": "t2"}),
            ),
            line(
                "09.001",
                "response_item",
                msg("user", "input_text", "now add a retry budget"),
            ),
            line(
                "10.000",
                "response_item",
                json!({"type": "custom_tool_call", "name": "apply_patch",
                "input": "*** Begin Patch\n*** Update File: src/login.rs", "call_id": "c2"}),
            ),
            line(
                "10.500",
                "response_item",
                json!({"type": "custom_tool_call_output", "call_id": "c2",
                "output": {"content": "Success. Updated src/login.rs"}}),
            ),
            line("11.000", "event_msg", tokens(2100, 95, 1200)),
        ]
    }

    fn write_rollout(dir: &Path, lines: &[Value]) -> PathBuf {
        let day = dir.join("sessions/2026/11/11");
        std::fs::create_dir_all(&day).unwrap();
        let p = day.join("rollout-2026-11-11T21-55-59-01a1ce53-c71d-7e13-b376-63f7271fa320.jsonl");
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn argv() {
        assert_eq!(
            launch_argv(Some("hello"), Some("gpt-6-luna"), &[]),
            [
                "codex",
                "--no-daemon",
                "--no-alt-screen",
                "--model",
                "gpt-6-luna",
                "hello"
            ]
        );
        assert_eq!(
            launch_argv(None, Some(" "), &[]),
            ["codex", "--no-daemon", "--no-alt-screen"]
        );
        assert_eq!(
            resume_argv("t-1", None, &["--search".to_string()]),
            [
                "codex",
                "resume",
                "t-1",
                "--no-daemon",
                "--no-alt-screen",
                "--search"
            ]
        );
    }

    /// The overlord's overrides are each valid TOML as Codex parses a
    /// `-c key=value` (the value alone), whatever the paths hold.
    #[test]
    fn overlord_overrides_are_toml() {
        let args = overlord_args(
            "/opt/my \"apps\"/rlord",
            "/run/r\"lord/daemon.sock",
            "/data/o'lord\\x",
            true,
        );
        assert_eq!(args.len() % 2, 0);
        let mut seen = std::collections::HashMap::new();
        for pair in args.chunks(2) {
            assert_eq!(pair[0], "-c");
            let (k, v) = pair[1].split_once('=').unwrap();
            let doc: toml::Table = format!("v = {v}").parse().unwrap_or_else(|e| {
                panic!("{k}: {v}: {e}");
            });
            seen.insert(k.to_string(), doc["v"].clone());
        }
        assert_eq!(
            seen["mcp_servers.rlord.command"].as_str(),
            Some("/opt/my \"apps\"/rlord")
        );
        let args: Vec<&str> = seen["mcp_servers.rlord.args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(args, ["mcp", "--socket", "/run/r\"lord/daemon.sock"]);
        let env = seen["mcp_servers.rlord.env_vars"].as_array().unwrap();
        assert_eq!(env.len(), crate::agents::MCP_ENV.len());
        assert!(env.iter().any(|v| v.as_str() == Some("RLORD_SESSION_ID")));
        assert_eq!(
            seen["mcp_servers.rlord.default_tools_approval_mode"].as_str(),
            Some("approve")
        );
        assert_eq!(seen["sandbox_mode"].as_str(), Some("read-only"));
        // ("untrusted" is refused by Codex 0.167.)
        assert_eq!(seen["approval_policy"].as_str(), Some("on-request"));
        assert_eq!(seen["check_for_update_on_startup"].as_bool(), Some(false));
        // Sends confirmed: that one tool asks.
        assert_eq!(
            seen["mcp_servers.rlord.tools.send_to_session.approval_mode"].as_str(),
            Some("prompt")
        );
        let off = overlord_args("/r", "/s", "/d", false);
        assert!(!off.iter().any(|a| a.contains("approval_mode=\"prompt\"")));
        // The folder's trust: one table keyed by the path itself.
        assert_eq!(
            seen["projects"]["/data/o'lord\\x"]["trust_level"].as_str(),
            Some("trusted")
        );
    }

    #[test]
    fn the_composer_is_told_from_a_dialog() {
        // As captured from codex-cli 0.167.2 (folder trusted, no prompt).
        let ready = "\n  >_ OpenAI Codex (v0.167.2)\n     /tmp/x\n\n\n› Ask Codex to do anything\n\n  GPT-6.1-Sol medium · /tmp/x  ⚠ 1 warning · f2 to view\n\n";
        assert!(composer_shown(ready));
        let typed = "› what is on my list\n\n  ? for shortcuts";
        assert!(composer_shown(typed));
        let dialog =
            "  Update available!\n\n› 1. Update now\n  2. Skip\n  3. Skip until next version\n";
        assert!(!composer_shown(dialog));
        let trust =
            "  Do you trust the contents of this directory?\n\n› 1. Yes, continue\n  2. No, quit\n";
        assert!(!composer_shown(trust));
        assert!(!composer_shown(""));
        for d in [dialog, trust] {
            assert!(dialog_shown(d));
        }
        assert!(!dialog_shown(ready) && !dialog_shown(typed));
    }

    /// An approval during a turn, as captured from codex-cli 0.167.2: the
    /// user's prompt (`›`) is in the history above it.
    #[test]
    fn an_approval_is_not_the_composer() {
        let approval = "› Run the shell command: touch probe.txt\n\n\n\
            • I'll run touch probe.txt, requesting approval.\n\n\
            • Running touch probe.txt\n\n\n\
              Would you like to run the following command?\n\n\
              Reason: Allow creating probe.txt in this read-only workspace?\n\n\
              $ touch probe.txt\n\n\n\
            › 1. Yes, proceed (y)\n\
              2. Yes, and don't ask again for commands that start with `touch probe.txt` (p)\n\
              3. No, and tell Codex what to do differently (esc)\n\n\
              Press enter to confirm or esc to cancel\n";
        assert!(!composer_shown(approval));
        assert!(dialog_shown(approval));
        // The user moved to another option: still a dialog.
        let moved = approval
            .replace("› 1. Yes", "  1. Yes")
            .replace("  2. Yes, and", "› 2. Yes, and");
        assert!(!composer_shown(&moved) && dialog_shown(&moved));
        // A short dialog under a prompt in the history: the history's `›`
        // doesn't count.
        let short = "› fix the tests\n\n› 1. Update now\n  2. Skip\n";
        assert!(!composer_shown(short) && dialog_shown(short));
        // Working, as captured: the composer is up under the status line.
        let working = "› Count slowly from 1 to 5.\n\n\n\
            • Working (1s • esc to interrupt)\n\n\n\
            › Ask Codex to do anything\n\n\
              GPT-6.1-Sol medium · /tmp/x\n\
              ← for agents · ? for shortcuts      ⚠ 1 warning · f2 to view\n";
        assert!(composer_shown(working) && !dialog_shown(working));
        // An MCP tool's approval (`approval_mode = "prompt"`), as captured:
        // its choices are indented.
        let mcp = "• Calling\n  └ rlord.send_to_session({\"session\":\"cxt2\"})\n\n\
              Field 1/1\n\n  Allow the rlord MCP server to run tool \"send_to_session\"?\n\n\
              session: cxt2\n  text: Reply with the word PONG.\n\n\
              › 1. Allow   Run the tool and continue\n\
                2. Cancel  Cancel this tool call\n\n\
              enter to submit | esc to cancel\n";
        assert!(!composer_shown(mcp) && dialog_shown(mcp));
        // The login screen marks its choice with `>`.
        let login = "  Choose how you want to use Codex.\n\n> 1. Sign in with ChatGPT\n  2. Sign in with Device Code\n\n  Press enter to continue\n";
        assert!(!composer_shown(login) && dialog_shown(login));
        // A dialog's footer alone, with a `›` choice that isn't numbered:
        // not the composer.
        for footer in ["enter to submit | esc to cancel", "  esc to cancel"] {
            let s = format!("  Allow it?\n\n› Allow\n  Cancel\n\n{footer}\n");
            assert!(!composer_shown(&s) && dialog_shown(&s), "{footer}");
        }
    }

    /// The composer as captured with attributes from codex-cli 0.167.2:
    /// the placeholder is dim, typed text isn't.
    #[test]
    fn a_draft_in_the_composer() {
        let footer = "\n  \x1b[38;2;246;226;183mGPT-6.1-Sol medium\x1b[39m · /tmp/x\n\
            \x1b[39m  \x1b[1m←\x1b[0m for agents · \x1b[1m?\x1b[0m for shortcuts\n";
        let empty = format!("\x1b[1m›\x1b[0m \x1b[2mAsk Codex to do anything\x1b[0m\n{footer}");
        assert_eq!(draft(&empty), None);
        let typed = format!("› old prompt\n\n• Done.\n\n\x1b[1m›\x1b[0m half a draft\n{footer}");
        assert_eq!(draft(&typed).as_deref(), Some("half a draft"));
        // A prompt in the history above an empty composer isn't a draft.
        let history = format!("› old prompt\n\n• Done.\n\n{empty}");
        assert_eq!(draft(&history), None);
        assert_eq!(draft(""), None);
    }

    #[test]
    fn a_subagent_file_is_toml() {
        let text = subagent_toml(
            "rlord-tidy",
            "Tidy \"the\" list",
            "Line one.\n\nUse `triage`.\n",
        );
        let doc: toml::Table = text.parse().unwrap();
        assert_eq!(doc["name"].as_str(), Some("rlord-tidy"));
        assert_eq!(doc["description"].as_str(), Some("Tidy \"the\" list"));
        assert_eq!(
            doc["developer_instructions"].as_str(),
            Some("Line one.\n\nUse `triage`.")
        );
        assert_eq!(doc["model_reasoning_effort"].as_str(), Some("low"));
    }

    #[test]
    fn hooks_map_to_status() {
        assert_eq!(
            map_hook_event("PermissionRequest"),
            ("permission_request", Effect::Set(Status::WaitingInput))
        );
        assert_eq!(map_hook_event("Stop").1, Effect::Set(Status::Idle));
        assert_eq!(map_hook_event("SessionEnd").1, Effect::Set(Status::Stopped));
        // The approved tool ran: the agent moved on.
        assert_eq!(map_hook_event("PostToolUse"), ("tool", Effect::Resume));
        // Not installed, and never a status: sub-agents aren't tracked yet.
        assert_eq!(
            map_hook_event("SubagentStart"),
            ("hook_other", Effect::Nothing)
        );
    }

    #[test]
    fn lines_read_as_conversation() {
        let lines = rollout_lines();
        let said: Vec<(&str, String)> = lines
            .iter()
            .filter_map(|v| Format::Codex.line(v).message())
            .collect();
        let who: Vec<&str> = said.iter().map(|m| m.0).collect();
        // Not the developer message, the injected context, the reasoning,
        // or the event copies of each message.
        assert_eq!(
            who,
            ["you", "agent", "tool", "agent", "you", "agent", "tool"]
        );
        assert_eq!(said[0].1, "fix the flaky login test");
        assert_eq!(said[1].1, "[exec_command] cargo test login");
        assert_eq!(said[2].1, "test login::retries ... FAILED");
        assert!(said[5].1.starts_with("[apply_patch] *** Begin Patch"));
        assert_eq!(said[6].1, "Success. Updated src/login.rs");
        assert_eq!(
            Format::Codex.line(&lines[0]).at(),
            Some("2026-11-12T05:56:02.272Z")
        );
    }

    #[test]
    fn only_what_codex_injects_is_not_a_prompt() {
        let user = |text: &str| {
            json!({"type": "response_item", "payload": {"type": "message", "role": "user",
                   "content": [{"type": "input_text", "text": text}]}})
        };
        let prompt = |text: &str| human_prompt(&user(text));
        for injected in [
            "<environment_context>\n  <cwd>/src/app</cwd>\n</environment_context>",
            "<user_instructions>\nbe brief\n</user_instructions>",
            "# AGENTS.md instructions for /src/app\n\n<INSTRUCTIONS>\nuse tabs\n</INSTRUCTIONS>",
            "<turn_aborted>\ninterrupted\n</turn_aborted>",
        ] {
            assert_eq!(prompt(injected), None, "{injected}");
        }
        // A real prompt may start with `<`, or mention a tag.
        for real in [
            "<div> isn't closing in the header, fix it",
            "<Button> should be disabled while loading",
            "<environment_context> is noisy in the logs, can we trim it?",
            "fix the flaky login test",
        ] {
            assert_eq!(prompt(real).as_deref(), Some(real));
        }
    }

    #[test]
    fn excerpts_stats_model_and_turn_state() {
        let dir = tempfile::tempdir().unwrap();
        let lines = rollout_lines();
        let p = write_rollout(dir.path(), &lines);
        let e = transcript::transcript_excerpts(&p);
        assert_eq!(e.first_prompt.as_deref(), Some("fix the flaky login test"));
        assert_eq!(e.last_prompt.as_deref(), Some("now add a retry budget"));
        assert_eq!(
            e.last_reply.as_deref(),
            Some("The retry never fired; fixed.")
        );
        assert_eq!(e.title, None);
        // Running totals, not a sum of them; the context is the last request's.
        let st = transcript::transcript_stats(&p).unwrap();
        assert_eq!((st.input_tokens, st.output_tokens), (2100, 95));
        assert!(!st.ended, "the second turn is still running");
        assert_eq!(st.last.unwrap() - st.first.unwrap(), 8_728);
        assert_eq!(transcript::context_tokens(&p), 1200);
        // The Usage column: prompts typed, tokens net of cache reads, and
        // the window Codex reports.
        let u = crate::usage::of(&p, None).unwrap();
        assert_eq!((u.turns, u.tokens, u.context), (2, 2000 + 95, 1200));
        assert_eq!(u.window, Some(258400));
        assert_eq!(
            transcript::observed_model(&p).as_deref(),
            Some("gpt-6-luna")
        );
        assert_eq!(transcript::working(&p), Some(true));
        // The turn ends: idle again.
        let mut done = lines.clone();
        done.push(json!({"type": "event_msg", "payload": {"type": "task_complete"}}));
        let p = write_rollout(dir.path(), &done);
        assert_eq!(transcript::working(&p), Some(false));
        assert!(transcript::transcript_stats(&p).unwrap().ended);
        // A name the user gave the thread is its title.
        done.push(json!({"type": "event_msg",
            "payload": {"type": "thread_name_updated", "thread_name": "login flake"}}));
        let p = write_rollout(dir.path(), &done);
        assert_eq!(
            transcript::transcript_excerpts(&p).title.as_deref(),
            Some("login flake")
        );
    }

    #[test]
    fn rollouts_are_found_by_id_and_say_their_folder() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_rollout(dir.path(), &rollout_lines());
        std::fs::write(dir.path().join("sessions/2026/11/11/notes.txt"), "x").unwrap();
        // (No thread index in this home: found by name.)
        assert_eq!(
            rollout_of(dir.path(), "01a1ce53-c71d-7e13-b376-63f7271fa320").as_deref(),
            Some(p.as_path())
        );
        assert_eq!(rollout_of(dir.path(), "nope"), None);
        assert_eq!(rollouts_in(&dir.path().join("sessions")).len(), 1);
        assert_eq!(rollout_cwd(&p).as_deref(), Some("/src/app"));
    }

    #[test]
    fn threads_started_in_a_folder_come_from_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let c = rusqlite::Connection::open(dir.path().join("state_5.sqlite")).unwrap();
        c.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, \
               cwd TEXT NOT NULL, created_at_ms INTEGER, thread_source TEXT, \
               source TEXT NOT NULL DEFAULT 'cli', first_user_message TEXT NOT NULL DEFAULT '');
             INSERT INTO threads VALUES ('old', '/r/old.jsonl', '/src/app', 1000, 'user', 'cli', '');
             INSERT INTO threads VALUES ('mine', '/r/mine.jsonl', '/src/app', 5000, 'user', 'cli', 'fix it');
             INSERT INTO threads VALUES ('sub', '/r/sub.jsonl', '/src/app', 5500, 'subagent', 'cli', '');
             INSERT INTO threads VALUES ('editor', '/r/ed.jsonl', '/src/app', 5600, 'user', 'vscode', '');
             INSERT INTO threads VALUES ('later', '/r/later.jsonl', '/src/app', 9000, 'user', 'cli', '');
             INSERT INTO threads VALUES ('other', '/r/other.jsonl', '/src/web', 6000, 'user', 'cli', '');",
        )
        .unwrap();
        drop(c);
        let ids: Vec<String> = threads_started_in(dir.path(), "/src/app", 4000)
            .into_iter()
            .map(|t| t.id)
            .collect();
        // Not the sub-agent's thread, nor the one started from an editor.
        assert_eq!(ids, ["mine", "later"]);
        assert_eq!(
            threads_started_in(dir.path(), "/src/app", 4000)[0]
                .first_user_message
                .as_deref(),
            Some("fix it")
        );
        assert_eq!(thread_kind(dir.path(), "mine"), Indexed::User);
        assert_eq!(thread_kind(dir.path(), "sub"), Indexed::SubAgent);
        assert_eq!(thread_kind(dir.path(), "unknown"), Indexed::NotListed);
        assert_eq!(
            thread_kind(&dir.path().join("absent"), "mine"),
            Indexed::NoIndex
        );
        let folders = rollout_folders(dir.path()).unwrap();
        assert_eq!(folders[Path::new("/r/other.jsonl")], "/src/web");
        assert_eq!(folders.len(), 6);
        // An older Codex's index (no `source`, no first message): the
        // threads in the folder, with nothing to tell them apart.
        let old = tempfile::tempdir().unwrap();
        let c = rusqlite::Connection::open(old.path().join("state_5.sqlite")).unwrap();
        c.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, \
               cwd TEXT NOT NULL, created_at_ms INTEGER);
             INSERT INTO threads VALUES ('a', '/r/a.jsonl', '/src/app', 5000);",
        )
        .unwrap();
        drop(c);
        let found = threads_started_in(old.path(), "/src/app", 4000);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].first_user_message, None);
        // It doesn't say whose a thread is: one it lists is the user's.
        assert_eq!(thread_kind(old.path(), "a"), Indexed::User);
        assert_eq!(thread_kind(old.path(), "b"), Indexed::NotListed);
        assert!(threads_started_in(dir.path(), "/src/none", 0).is_empty());
        // No index at all (Codex never ran): nothing, quietly.
        assert!(threads_started_in(&dir.path().join("absent"), "/src/app", 0).is_empty());
    }

    #[test]
    fn models_come_from_codex_cache() {
        let dir = tempfile::tempdir().unwrap();
        assert!(models(dir.path()).is_empty());
        std::fs::write(
            dir.path().join("models_cache.json"),
            json!({"models": [
                {"slug": "gpt-5.6-terra", "visibility": "list", "priority": 3},
                {"slug": "codex-auto-review", "visibility": "hide", "priority": 5},
                {"slug": "gpt-6-luna", "visibility": "list", "priority": 2},
            ]})
            .to_string(),
        )
        .unwrap();
        assert_eq!(models(dir.path()), ["gpt-6-luna", "gpt-5.6-terra"]);
    }

    #[test]
    fn summary_runs_save_nothing_and_change_nothing() {
        let out = Path::new("/tmp/o.txt");
        let a = summary_argv("gpt-6-luna", Some(("t-1", "Summarize")), out);
        assert_eq!(a[..4], ["codex", "exec", "fork", "t-1"]);
        assert_eq!(a.last().unwrap(), "Summarize");
        let b = summary_argv("", None, out);
        assert_eq!(b.last().unwrap(), "-", "the prompt comes on stdin");
        for argv in [&a, &b] {
            assert!(argv.contains(&"--ephemeral".to_string()));
            assert!(argv.contains(&"sandbox_mode=\"read-only\"".to_string()));
            assert!(!argv.iter().any(|x| x.contains("dangerously")));
        }
        assert!(!b.contains(&"--model".to_string()));
    }
}
