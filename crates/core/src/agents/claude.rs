//! Claude Code adapter.
//!
//! Identity model (see docs/architecture.md): rlord MINTS the Claude
//! session UUID and launches with `--session-id <uuid>`, so the transcript
//! path is known from second one and `--resume <uuid>` is always attributable.
//! Ids scavenged from disk scans are never allowed to authorize a resume.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde_json::Value;

use super::transcript::{transcript_stats, Title, Usage};
use super::Effect;
use crate::types::{Status, Subagent};

pub fn mint_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `~/.claude` or `$CLAUDE_CONFIG_DIR`.
pub fn config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".claude")
        })
}

/// Claude Code's project-directory slug: every non-alphanumeric char of the
/// absolute project path becomes `-` (verified against real `~/.claude/projects`
/// entries; format is internal to Claude Code and pinned by tests).
pub fn project_slug(project_path: &str) -> String {
    project_path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn transcript_path(project_path: &str, session_uuid: &str) -> PathBuf {
    config_dir()
        .join("projects")
        .join(project_slug(project_path))
        .join(format!("{session_uuid}.jsonl"))
}

/// `extra` are adapter flags the session was created with (e.g. the
/// overlord's `--mcp-config`); they ride in launch_spec so resume reuses them.
pub fn launch_argv(
    session_uuid: &str,
    first_prompt: Option<&str>,
    model: Option<&str>,
    extra: &[String],
) -> Vec<String> {
    let mut argv = vec![
        "claude".to_string(),
        "--session-id".to_string(),
        session_uuid.to_string(),
    ];
    if let Some(m) = model.filter(|m| !m.trim().is_empty()) {
        argv.push("--model".to_string());
        argv.push(m.trim().to_string());
    }
    argv.extend(extra.iter().cloned());
    if let Some(p) = first_prompt {
        argv.push(p.to_string());
    }
    argv
}

/// Resume argv. `--resume` only when a transcript actually exists (a resume of
/// a conversation Claude never wrote errors out); otherwise re-declare the id
/// so the *next* restart can resume. Scavenged ids never resume.
pub fn resume_argv(
    session_uuid: &str,
    trust: &str,
    project_path: &str,
    model: Option<&str>,
    extra: &[String],
) -> Result<Vec<String>> {
    if trust == "scavenged" {
        bail!("refusing to resume a scavenged session id (untrusted attribution)");
    }
    let flag = if transcript_path(project_path, session_uuid).exists() {
        "--resume"
    } else {
        "--session-id"
    };
    let mut argv = vec![
        "claude".to_string(),
        flag.to_string(),
        session_uuid.to_string(),
    ];
    if let Some(m) = model.filter(|m| !m.trim().is_empty()) {
        argv.push("--model".to_string());
        argv.push(m.trim().to_string());
    }
    argv.extend(extra.iter().cloned());
    Ok(argv)
}

/// Map a Claude hook event to (event kind for our log, what it does to the
/// status).
///
/// "Needs you" (blocked mid-turn on the user) comes from a permission
/// request, a question (`AskUserQuestion`, seen as its PreToolUse), or a
/// notification that says so; it ends when the agent moves on (a tool
/// finished or failed, an elicitation answered). The idle notification
/// Claude sends a minute after a turn ends is not that: the session is
/// already "your turn".
pub fn map_hook_event(event: &str, payload: &Value) -> (&'static str, Effect) {
    use Effect::{Nothing, Resume, Set};
    match event {
        "SessionStart" => ("session_start", Set(Status::Idle)),
        "UserPromptSubmit" => ("prompt_submitted", Set(Status::Working)),
        "Stop" => ("agent_stopped", Set(Status::Idle)),
        "PermissionRequest" => ("permission_request", Set(Status::WaitingInput)),
        "PreToolUse" if payload["tool_name"] == "AskUserQuestion" => {
            ("question_asked", Set(Status::WaitingInput))
        }
        "PreToolUse" => ("tool", Nothing),
        "PostToolUse" | "PostToolUseFailure" => ("tool", Resume),
        "Notification" => ("notification", notification_effect(payload)),
        "SessionEnd" => ("session_end", Set(Status::Stopped)),
        "PreCompact" => ("pre_compact", Nothing),
        "SubagentStart" => ("subagent_started", Nothing),
        "SubagentStop" => ("subagent_stopped", Nothing),
        _ => ("hook_other", Nothing),
    }
}

/// What a notification means, from its `notification_type` (older Claude
/// Code sends none: then from its message).
fn notification_effect(payload: &Value) -> Effect {
    match payload["notification_type"].as_str() {
        // (agent_needs_input: a background session of this one waits on
        // the user.)
        Some(
            "permission_prompt"
            | "elicitation_dialog"
            | "elicitation_url_dialog"
            | "agent_needs_input",
        ) => Effect::Set(Status::WaitingInput),
        Some("elicitation_complete" | "elicitation_response") => Effect::Resume,
        // idle_prompt, auth, quota, agent view, plugins: not this session
        // blocked on the user.
        Some(_) => Effect::Nothing,
        None => {
            let m = payload["message"].as_str().unwrap_or("").to_lowercase();
            if m.contains("permission") || m.contains("approval") {
                Effect::Set(Status::WaitingInput)
            } else {
                Effect::Nothing
            }
        }
    }
}

/// (event, matcher): the events rlord's hook is installed for. The tool
/// events fire on every tool call (the handler does nothing unless the
/// session's status changes); PreToolUse only for the question tool.
pub(crate) const HOOK_EVENTS: &[(&str, Option<&str>)] = &[
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    ("Stop", None),
    ("Notification", None),
    ("PermissionRequest", None),
    ("PreToolUse", Some("AskUserQuestion")),
    ("PostToolUse", None),
    ("PostToolUseFailure", None),
    ("SessionEnd", None),
    ("PreCompact", None),
    ("SubagentStart", None),
    ("SubagentStop", None),
];

// ---- reading transcript lines (see `transcript::Line`) ----

/// A prompt the human typed (not tool results, meta, summaries, or command
/// wrappers like `<command-name>`).
pub(crate) fn human_prompt(v: &Value) -> Option<String> {
    if v["type"] != "user"
        || v["isMeta"] == true
        || v["isCompactSummary"] == true
        || v["isVisibleInTranscriptOnly"] == true
    {
        return None;
    }
    let text = match &v["message"]["content"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter(|x| x["type"] == "text")
            .filter_map(|x| x["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let t = text.trim();
    // (`[Request interrupted by user]`, `… for tool use]`: Claude writes
    // these when the user presses Esc; they're not prompts.)
    (!t.is_empty()
        && !t.starts_with('<')
        && !t.starts_with("Caveat:")
        && !t.starts_with("[Request interrupted by user"))
    .then(|| t.to_string())
}

pub(crate) fn reply_text(v: &Value) -> Option<String> {
    if v["type"] != "assistant" {
        return None;
    }
    let t = v["message"]["content"]
        .as_array()?
        .iter()
        .filter(|x| x["type"] == "text")
        .filter_map(|x| x["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    (!t.trim().is_empty()).then_some(t)
}

/// A message's text and who said it, from one transcript line. None for
/// lines that aren't conversation (titles, snapshots, meta).
pub(crate) fn message(v: &Value) -> Option<(&'static str, String)> {
    if v["isMeta"] == true || v["isVisibleInTranscriptOnly"] == true {
        return None;
    }
    let content = &v["message"]["content"];
    match v["type"].as_str()? {
        "user" => match content {
            Value::String(s) => Some(("you", s.clone())),
            Value::Array(a) => {
                // Tool results come back as user lines.
                let mut text = Vec::new();
                let mut tool = false;
                for x in a {
                    match x["type"].as_str() {
                        Some("text") => text.extend(x["text"].as_str().map(str::to_string)),
                        Some("tool_result") => {
                            tool = true;
                            match &x["content"] {
                                Value::String(s) => text.push(s.clone()),
                                Value::Array(c) => text.extend(
                                    c.iter()
                                        .filter_map(|y| y["text"].as_str())
                                        .map(str::to_string),
                                ),
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                let t = text.join("\n");
                (!t.trim().is_empty()).then_some((if tool { "tool" } else { "you" }, t))
            }
            _ => None,
        },
        "assistant" => {
            let a = content.as_array()?;
            let mut text = Vec::new();
            for x in a {
                match x["type"].as_str() {
                    Some("text") => text.extend(x["text"].as_str().map(str::to_string)),
                    // What it ran: the tool and its main argument.
                    Some("tool_use") => {
                        let input = &x["input"];
                        let arg = ["command", "file_path", "pattern", "url", "description"]
                            .iter()
                            .find_map(|k| input[*k].as_str())
                            .unwrap_or("");
                        text.push(format!("[{}] {arg}", x["name"].as_str().unwrap_or("tool")));
                    }
                    _ => {}
                }
            }
            let t = text.join("\n");
            (!t.trim().is_empty()).then_some(("agent", t))
        }
        _ => None,
    }
}

pub(crate) fn title(v: &Value) -> Option<Title> {
    match v["type"].as_str()? {
        "custom-title" => v["customTitle"]
            .as_str()
            .map(|t| Title::User(t.to_string())),
        "ai-title" => v["aiTitle"]
            .as_str()
            .map(|t| Title::Generated(t.to_string())),
        _ => None,
    }
}

pub(crate) fn last_prompt(v: &Value) -> Option<&str> {
    (v["type"] == "last-prompt")
        .then(|| v["lastPrompt"].as_str())
        .flatten()
}

/// One request's usage; its context is everything it sent, cached or not.
/// None for messages Claude Code made up itself (`"model":"<synthetic>"`,
/// e.g. an API error shown as a reply) and any that sent no context: their
/// zeros aren't a request, and would drop context % to 0.
pub(crate) fn usage(v: &Value) -> Option<Usage> {
    if v["message"]["model"] == "<synthetic>" {
        return None;
    }
    let u = v["message"]["usage"].as_object()?;
    let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    let context: u64 = [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ]
    .iter()
    .filter_map(|k| u.get(*k).and_then(Value::as_u64))
    .sum();
    if context == 0 {
        return None;
    }
    Some(Usage {
        input: n("input_tokens"),
        output: n("output_tokens"),
        context,
        fresh: n("input_tokens") + n("cache_creation_input_tokens") + n("output_tokens"),
        window: None,
        cumulative: false,
    })
}

pub(crate) fn model(v: &Value) -> Option<&str> {
    v["message"]["model"]
        .as_str()
        .filter(|m| m.starts_with("claude"))
}

/// Any final stop reason (end_turn, max_tokens, …) ends the turn; not a
/// tool call, nor a partial message without one. A user line (a prompt or
/// a tool result) reopens it.
pub(crate) fn turn_ended(v: &Value) -> Option<bool> {
    match v["type"].as_str()? {
        "assistant" => Some(
            v["message"]["stop_reason"]
                .as_str()
                .is_some_and(|r| r != "tool_use"),
        ),
        "user" => Some(false),
        _ => None,
    }
}

// ---- archive summaries ----

/// `claude -p` flags for a one-shot answer with no tools and nothing saved.
/// `fork`: the conversation to read in full (a fork of it; the session is
/// untouched) with the question as the prompt; without one the prompt
/// arrives on stdin. The answer is JSON on stdout (`result`).
pub fn summary_argv(model: &str, fork: Option<(&str, &str)>) -> Vec<String> {
    let mut a: Vec<String> = ["claude", "-p", "--output-format", "json"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    a.extend(["--no-session-persistence", "--tools", ""].map(String::from));
    if !model.is_empty() {
        a.extend(["--model".to_string(), model.to_string()]);
    }
    if let Some((id, ask)) = fork {
        a.extend(["--resume", id, "--fork-session", ask].map(String::from));
    }
    a
}

// ---- sub-agents on disk ----

/// `<project dir>/<session uuid>/subagents/` for a session transcript path.
pub fn subagents_dir(transcript_path: &str) -> PathBuf {
    let p = Path::new(transcript_path);
    let stem = p.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
    p.with_file_name(stem).join("subagents")
}

/// A sub-agent whose transcript hasn't been written for this long has
/// stopped, whatever its meta file says.
const AGENT_QUIET: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Every sub-agent described by a `*.meta.json` in the session's subagents
/// dir (the Agent tool writes one per real sub-agent; Claude's internal
/// helper agents have none and are deliberately excluded).
pub fn scan_subagents(session_id: &str, transcript_path: &str) -> Vec<Subagent> {
    scan_subagents_where(session_id, transcript_path, true, &|_| true)
}

/// `scan_subagents` for only the agent ids `want` accepts. Reading a
/// sub-agent means reading its whole transcript (token totals), so callers
/// skip the ones they already know are finished: on a long-lived session
/// that is dozens of multi-MB files, and this runs as the user navigates.
/// `parent_running`: false when the session has stopped; then none of its
/// sub-agents can still be running.
pub fn scan_subagents_where(
    session_id: &str,
    transcript_path: &str,
    parent_running: bool,
    want: &dyn Fn(&str) -> bool,
) -> Vec<Subagent> {
    scan_subagents_read(session_id, transcript_path, parent_running, want)
        .into_iter()
        .map(|(a, _)| a)
        .collect()
}

/// `scan_subagents_where`, saying for each agent whether its transcript
/// was read (its token totals are a count from it; false: they're zeros
/// because it couldn't be, and mustn't replace anything).
pub fn scan_subagents_read(
    session_id: &str,
    transcript_path: &str,
    parent_running: bool,
    want: &dyn Fn(&str) -> bool,
) -> Vec<(Subagent, bool)> {
    let dir = subagents_dir(transcript_path);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(id) = name
            .strip_prefix("agent-")
            .and_then(|n| n.strip_suffix(".meta.json"))
        else {
            continue;
        };
        if !want(id) {
            continue;
        }
        let Ok(meta) = std::fs::read_to_string(e.path())
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .ok_or(())
        else {
            continue;
        };
        let jsonl = dir.join(format!("agent-{id}.jsonl"));
        let read = transcript_stats(&jsonl);
        let counted = read.is_some();
        let stats = read.unwrap_or_default();
        // Claude doesn't always mark an agent finished (older versions,
        // agents cut off when the session ended), so also: its last turn is
        // a final reply, or its transcript has gone quiet.
        let quiet = stats
            .modified
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d > AGENT_QUIET);
        let finished =
            meta["finished"].as_bool().unwrap_or(false) || stats.ended || quiet || !parent_running;
        out.push((
            Subagent {
                agent_id: id.to_string(),
                session_id: session_id.to_string(),
                agent_type: meta["agentType"].as_str().unwrap_or("").to_string(),
                description: meta["description"].as_str().unwrap_or("").to_string(),
                tool_use_id: meta["toolUseId"].as_str().map(str::to_string),
                spawn_depth: meta["spawnDepth"].as_i64().unwrap_or(1),
                background: meta["requestShape"].as_str() == Some("background"),
                started_at: stats.first,
                finished_at: if finished {
                    stats.last.or(stats.first)
                } else {
                    None
                },
                transcript_path: jsonl.exists().then(|| jsonl.to_string_lossy().into_owned()),
                result: String::new(),
                input_tokens: stats.input_tokens,
                output_tokens: stats.output_tokens,
            },
            counted,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interruptions_are_not_prompts() {
        let user = |c: Value| json!({"type": "user", "message": {"role": "user", "content": c}});
        assert_eq!(
            human_prompt(&user(json!("fix it"))).as_deref(),
            Some("fix it")
        );
        assert_eq!(
            human_prompt(&user(json!("[Request interrupted by user]"))),
            None
        );
        assert_eq!(
            human_prompt(&user(json!([
                {"type": "text", "text": "[Request interrupted by user for tool use]"}
            ]))),
            None
        );
    }

    #[test]
    fn synthetic_and_empty_messages_carry_no_usage() {
        let msg = |model: &str, input: u64, cached: u64| {
            json!({"type": "assistant", "message": {"id": "m", "model": model,
                "usage": {"input_tokens": input, "cache_read_input_tokens": cached,
                          "output_tokens": 0}}})
        };
        assert_eq!(usage(&msg("claude-opus-5-5", 1, 500)).unwrap().context, 501);
        // An API error Claude Code shows as a reply: zeros, not a request.
        assert!(usage(&msg("<synthetic>", 0, 0)).is_none());
        assert!(usage(&msg("claude-opus-5-5", 0, 0)).is_none());
    }

    #[test]
    fn slug_matches_observed_claude_encoding() {
        assert_eq!(
            project_slug("/home/alice/src/myapp"),
            "-home-alice-src-myapp"
        );
        assert_eq!(
            project_slug("/home/alice/src/myapp/.worktrees/feature/x"),
            "-home-alice-src-myapp--worktrees-feature-x"
        );
    }

    #[test]
    fn launch_and_resume_argv() {
        let argv = launch_argv("u-1", Some("hello"), None, &[]);
        assert_eq!(argv, ["claude", "--session-id", "u-1", "hello"]);
        let argv = launch_argv("u-1", None, Some("opus"), &[]);
        assert_eq!(argv, ["claude", "--session-id", "u-1", "--model", "opus"]);
        // No transcript on disk for this fake id → re-declare, don't resume.
        let argv = resume_argv(
            "00000000-0000-0000-0000-000000000000",
            "minted",
            "/nonexistent",
            None,
            &["--strict-mcp-config".to_string()],
        )
        .unwrap();
        assert_eq!(argv[1], "--session-id");
        assert_eq!(argv.last().unwrap(), "--strict-mcp-config");
        assert!(resume_argv("u-1", "scavenged", "/tmp", None, &[]).is_err());
    }

    #[test]
    fn needs_you_is_a_block_mid_turn_not_an_idle_reminder() {
        let note = |ty: &str| map_hook_event("Notification", &json!({"notification_type": ty})).1;
        assert_eq!(note("permission_prompt"), Effect::Set(Status::WaitingInput));
        assert_eq!(
            note("elicitation_dialog"),
            Effect::Set(Status::WaitingInput)
        );
        assert_eq!(note("agent_needs_input"), Effect::Set(Status::WaitingInput));
        // A minute after the turn ended: still "your turn".
        assert_eq!(note("idle_prompt"), Effect::Nothing);
        assert_eq!(note("auth_success"), Effect::Nothing);
        assert_eq!(note("elicitation_response"), Effect::Resume);
        // Older Claude Code: no type, only the text.
        let old = |m: &str| map_hook_event("Notification", &json!({"message": m})).1;
        assert_eq!(
            old("Claude needs your permission to use Bash"),
            Effect::Set(Status::WaitingInput)
        );
        assert_eq!(old("Claude is waiting for your input"), Effect::Nothing);
        // A question mid-turn blocks; any other tool starting doesn't.
        let pre = |tool: &str| map_hook_event("PreToolUse", &json!({"tool_name": tool}));
        assert_eq!(
            pre("AskUserQuestion"),
            ("question_asked", Effect::Set(Status::WaitingInput))
        );
        assert_eq!(pre("Bash").1, Effect::Nothing);
        assert_eq!(
            map_hook_event("PermissionRequest", &json!({})).1,
            Effect::Set(Status::WaitingInput)
        );
        // The agent moved on.
        for e in ["PostToolUse", "PostToolUseFailure"] {
            assert_eq!(map_hook_event(e, &json!({})), ("tool", Effect::Resume));
        }
    }

    #[test]
    fn dir_layout_and_scan() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("abc.jsonl");
        let sub = subagents_dir(transcript.to_str().unwrap());
        assert!(sub.ends_with("abc/subagents"));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("agent-a1.meta.json"),
            r#"{"agentType":"Explore","description":"Find X","toolUseId":"toolu_1","spawnDepth":1,"requestShape":"background","finished":true}"#,
        )
        .unwrap();
        std::fs::write(
            sub.join("agent-a1.jsonl"),
            "{\"type\":\"user\",\"timestamp\":\"2026-10-27T17:03:44.000Z\"}\n\
             {\"type\":\"assistant\",\"timestamp\":\"2026-10-27T17:03:48.000Z\",\"message\":{\"usage\":{\"input_tokens\":6,\"output_tokens\":260}}}\n",
        )
        .unwrap();
        std::fs::write(sub.join("junk.txt"), "x").unwrap();
        // Callers skip agents they already know (finished ones).
        let t = transcript.to_str().unwrap();
        assert!(scan_subagents_where("s1", t, true, &|id| id != "a1").is_empty());
        let v = scan_subagents("s1", transcript.to_str().unwrap());
        assert_eq!(v.len(), 1);
        let a = &v[0];
        assert_eq!(a.agent_id, "a1");
        assert_eq!(a.description, "Find X");
        assert!(a.background);
        assert_eq!(a.output_tokens, 260);
        assert_eq!(a.finished_at.unwrap() - a.started_at.unwrap(), 4000);
    }
}
