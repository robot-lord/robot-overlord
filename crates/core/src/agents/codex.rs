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
use super::Effect;
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
    /// The prompt that started it ("" until Codex records one).
    pub first_user_message: String,
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
    let q = c.prepare(
        "SELECT id, rollout_path, created_at_ms, first_user_message FROM threads \
         WHERE cwd IN (?1, ?2) AND created_at_ms >= ?3 AND source = 'cli' \
           AND (thread_source IS NULL OR thread_source = 'user') \
         ORDER BY created_at_ms ASC LIMIT 8",
    );
    let Ok(mut q) = q else {
        return Vec::new(); // another Codex version's schema
    };
    let rows = q.query_map(rusqlite::params![cwd, real, since_ms], |r| {
        Ok(Thread {
            id: r.get(0)?,
            rollout_path: r.get(1)?,
            created_at_ms: r.get(2)?,
            first_user_message: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        })
    });
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// Whether thread `id` is one the user drives (not a sub-agent's), from
/// the index. None when the index doesn't know it (yet).
pub fn is_user_thread(home: &Path, id: &str) -> Option<bool> {
    open_state(home)?
        .query_row(
            "SELECT thread_source FROM threads WHERE id = ?1",
            [id],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .map(|source| source.is_none_or(|s| s == "user"))
}

/// The rollouts of conversations that ran in any of `folders`, from the
/// index (one query). None when there is no index to ask.
pub fn rollouts_in_folders(
    home: &Path,
    folders: &[String],
) -> Option<std::collections::HashSet<PathBuf>> {
    let c = open_state(home)?;
    let mut q = c
        .prepare("SELECT rollout_path FROM threads WHERE cwd = ?1")
        .ok()?;
    let mut out = std::collections::HashSet::new();
    for f in folders {
        let rows = q.query_map([f], |r| r.get::<_, String>(0)).ok()?;
        out.extend(rows.flatten().map(PathBuf::from));
    }
    Some(out)
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
            threads_started_in(dir.path(), "/src/app", 4000)[0].first_user_message,
            "fix it"
        );
        assert_eq!(is_user_thread(dir.path(), "mine"), Some(true));
        assert_eq!(is_user_thread(dir.path(), "sub"), Some(false));
        assert_eq!(is_user_thread(dir.path(), "unknown"), None);
        let in_web = rollouts_in_folders(dir.path(), &["/src/web".to_string()]).unwrap();
        assert!(in_web.contains(Path::new("/r/other.jsonl")) && in_web.len() == 1);
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
