//! Claude Code adapter.
//!
//! Identity model (see docs/architecture.md): rlord MINTS the Claude
//! session UUID and launches with `--session-id <uuid>`, so the transcript
//! path is known from second one and `--resume <uuid>` is always attributable.
//! Ids scavenged from disk scans are never allowed to authorize a resume.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::types::Status;

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

/// Map a Claude hook event to (event kind for our log, optional new status).
pub fn map_hook_event(event: &str) -> (&'static str, Option<Status>) {
    match event {
        "SessionStart" => ("session_start", Some(Status::Idle)),
        "UserPromptSubmit" => ("prompt_submitted", Some(Status::Working)),
        "Stop" => ("agent_stopped", Some(Status::Idle)),
        "Notification" => ("notification", Some(Status::WaitingInput)),
        "SessionEnd" => ("session_end", Some(Status::Stopped)),
        "PreCompact" => ("pre_compact", None),
        "SubagentStart" => ("subagent_started", None),
        "SubagentStop" => ("subagent_stopped", None),
        _ => ("hook_other", None),
    }
}

const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "Notification",
    "SessionEnd",
    "PreCompact",
    "SubagentStart",
    "SubagentStop",
];

/// Merge our hook command into Claude's settings.json, preserving everything
/// already there (including other tools' hooks). Idempotent. Returns true if
/// the file was modified.
pub fn install_hooks(settings_path: &Path, hook_cmd: &str) -> Result<bool> {
    let mut root: Value = match std::fs::read_to_string(settings_path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).context("parsing existing settings.json")?
        }
        _ => json!({}),
    };
    if !root.is_object() {
        bail!("settings.json is not a JSON object; refusing to modify");
    }
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        bail!("settings.json 'hooks' is not an object; refusing to modify");
    }
    let mut changed = false;
    for event in HOOK_EVENTS {
        let arr = hooks
            .as_object_mut()
            .unwrap()
            .entry(*event)
            .or_insert_with(|| json!([]));
        if !arr.is_array() {
            bail!("settings.json hooks.{event} is not an array; refusing to modify");
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
            arr.as_array_mut()
                .unwrap()
                .push(json!({"hooks": [{"type": "command", "command": hook_cmd}]}));
            changed = true;
        }
    }
    if changed {
        if let Some(dir) = settings_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let backup = settings_path.with_extension("json.pre-rlord.bak");
        if settings_path.exists() && !backup.exists() {
            std::fs::copy(settings_path, &backup)?;
        }
        let tmp = settings_path.with_extension("json.rlord-tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&root)? + "\n")?;
        std::fs::rename(&tmp, settings_path)?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_observed_claude_encoding() {
        assert_eq!(
            project_slug("/home/alice/ko/myapp"),
            "-home-alice-ko-myapp"
        );
        assert_eq!(
            project_slug("/home/alice/ko/myapp/.worktrees/feature/x"),
            "-home-alice-ko-myapp--worktrees-feature-x"
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
    fn hook_install_is_idempotent_and_preserving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"other-tool hook-handler"}]}]}}"#,
        )
        .unwrap();
        assert!(install_hooks(&path, "/bin/rlord hook emit claude").unwrap());
        assert!(!install_hooks(&path, "/bin/rlord hook emit claude").unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["model"], "opus"); // untouched
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2); // other-tool entry preserved, ours appended
        assert!(v["hooks"]["UserPromptSubmit"].as_array().unwrap().len() == 1);
        assert!(path.with_extension("json.pre-rlord.bak").exists());
    }
}

// ---- sub-agents on disk ----

use crate::types::Subagent;

/// `<project dir>/<session uuid>/subagents/` for a session transcript path.
pub fn subagents_dir(transcript_path: &str) -> PathBuf {
    let p = Path::new(transcript_path);
    let stem = p.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
    p.with_file_name(stem).join("subagents")
}

/// Parse ISO-8601 `2026-10-27T17:03:44.498Z` to ms epoch (lenient).
pub(crate) fn parse_iso_ms(s: &str) -> Option<i64> {
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = rest.trim_end_matches('Z');
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let sec: f64 = t.next()?.parse().ok()?;
    // days from civil (Howard Hinnant)
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86_400_000 + h * 3_600_000 + mi * 60_000 + (sec * 1000.0) as i64)
}

/// What a session is about, read cheaply from its transcript (no AI): the
/// prompt that started it, the latest real prompt, the latest reply text,
/// and the title Claude gave the conversation. For the overlord's triage.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Excerpts {
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub last_reply: Option<String>,
    pub title: Option<String>,
}

/// Bytes read from each end of a transcript; they can be many MB.
const EXCERPT_WINDOW: u64 = 256 * 1024;

pub(crate) fn read_window(path: &Path, from_end: bool) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = if from_end {
        len.saturating_sub(EXCERPT_WINDOW)
    } else {
        0
    };
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(EXCERPT_WINDOW).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    // A tail window starts mid-line: drop the partial first line.
    Some(if start > 0 {
        text.split_once('\n')
            .map(|(_, r)| r.to_string())
            .unwrap_or_default()
    } else {
        text
    })
}

pub fn clip(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        s
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

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
    (!t.is_empty() && !t.starts_with('<') && !t.starts_with("Caveat:")).then(|| t.to_string())
}

fn reply_text(v: &Value) -> Option<String> {
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

pub fn transcript_excerpts(path: &Path) -> Excerpts {
    let (head, tail) = read_head_tail(path);
    excerpts_from(&head, &tail)
}

/// Parsed JSON lines from the start and the end of a transcript (each at
/// most `EXCERPT_WINDOW` bytes; a small file is read once and used for both).
pub fn read_head_tail(path: &Path) -> (Vec<Value>, Vec<Value>) {
    let parse = |s: String| -> Vec<Value> {
        s.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    };
    let head = read_window(path, false).map(parse).unwrap_or_default();
    let small = std::fs::metadata(path).is_ok_and(|m| m.len() <= EXCERPT_WINDOW);
    let tail = if small {
        head.clone()
    } else {
        read_window(path, true).map(parse).unwrap_or_default()
    };
    (head, tail)
}

pub(crate) fn excerpts_from(head: &[Value], tail: &[Value]) -> Excerpts {
    let mut e = Excerpts {
        first_prompt: head.iter().find_map(human_prompt).map(|t| clip(&t, 300)),
        ..Default::default()
    };
    for v in tail.iter().rev() {
        if e.last_prompt.is_none() {
            e.last_prompt = human_prompt(v).map(|t| clip(&t, 300));
        }
        if e.last_reply.is_none() {
            e.last_reply = reply_text(v).map(|t| clip(&t, 500));
        }
        // A title the user set wins over the one Claude generated.
        if e.title.is_none() && v["type"] == "custom-title" {
            e.title = v["customTitle"].as_str().map(str::to_string);
        }
        if e.last_prompt.is_some() && e.last_reply.is_some() && e.title.is_some() {
            break;
        }
    }
    if e.title.is_none() {
        e.title = tail
            .iter()
            .rev()
            .filter(|v| v["type"] == "ai-title")
            .find_map(|v| v["aiTitle"].as_str())
            .filter(|t| !t.trim().is_empty())
            .map(|t| clip(t, 120));
    }
    // Long tool-heavy turns push the last typed prompt out of the tail
    // window; Claude also records it in `last-prompt` entries.
    if e.last_prompt.is_none() {
        e.last_prompt = tail
            .iter()
            .rev()
            .filter(|v| v["type"] == "last-prompt")
            .find_map(|v| v["lastPrompt"].as_str())
            .filter(|t| !t.trim().is_empty())
            .map(|t| clip(t, 300));
    }
    // One prompt so far: don't repeat it. Claude's `last-prompt` record may
    // be a truncated copy ("…"), so a clipped one matches by prefix; an
    // unclipped one must match exactly (a short "continue" is a new prompt).
    if let (Some(l), Some(f)) = (&e.last_prompt, &e.first_prompt) {
        let same = match l.strip_suffix('…') {
            Some(clipped) => f.starts_with(clipped),
            None => l == f,
        };
        if same {
            e.last_prompt = None;
        }
    }
    e
}

/// Token totals and time span from a sub-agent transcript.
///
/// Incremental: a running agent's transcript is polled again and again while
/// it grows, so the totals so far and the offset read up to are kept per
/// file, and each call parses only the bytes appended since. A trailing line
/// without its newline counts once it parses as complete JSON. A file that
/// was replaced (new inode) or shrank is read again from the start.
pub fn transcript_stats(path: &Path) -> Option<TranscriptStats> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    let (len, ino) = (meta.len(), meta.ino());
    let modified = meta.modified().ok();
    // Copy the cursor out: the (possibly large) read happens unlocked, so one
    // slow file never holds up stats for another.
    let mut c = {
        let cursors = STATS_CURSORS.lock().unwrap_or_else(|e| e.into_inner());
        cursors.get(path).cloned().unwrap_or_default()
    };
    let before = (c.ino, c.offset);
    if c.ino != ino || len < c.offset {
        c = StatsCursor {
            ino,
            ..Default::default()
        };
    }
    if len > c.offset {
        let mut f = std::fs::File::open(path).ok()?;
        f.seek(SeekFrom::Start(c.offset)).ok()?;
        let mut buf = Vec::with_capacity((len - c.offset) as usize);
        f.take(len - c.offset).read_to_end(&mut buf).ok()?;
        // Complete lines, plus a trailing one if it is already whole JSON
        // (a final record may never get its newline).
        let mut end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |e| e + 1);
        let tail = &buf[end..];
        if !tail.is_empty() && serde_json::from_slice::<Value>(tail).is_ok() {
            end = buf.len();
        }
        for line in buf[..end].split(|&b| b == b'\n') {
            let Ok(v) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if let Some(ts) = v["timestamp"].as_str().and_then(parse_iso_ms) {
                c.first.get_or_insert(ts);
                c.last = Some(ts);
            }
            if let Some(u) = v["message"]["usage"].as_object() {
                c.input += u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
                c.output += u.get("output_tokens").and_then(Value::as_i64).unwrap_or(0);
            }
            // Done when the latest turn is a final reply (no tool call
            // pending); a tool result or new prompt reopens it.
            match v["type"].as_str() {
                // Any final stop reason (end_turn, max_tokens, …); not a
                // tool call, nor a partial message without one.
                Some("assistant") => {
                    c.ended = v["message"]["stop_reason"]
                        .as_str()
                        .is_some_and(|r| r != "tool_use")
                }
                Some("user") => c.ended = false,
                _ => {}
            }
        }
        c.offset += end as u64;
    }
    let mut stats = c.stats();
    stats.modified = modified;
    c.used = std::time::Instant::now();
    if (c.ino, c.offset) != before {
        c.dirty = true;
    }
    let mut cursors = STATS_CURSORS.lock().unwrap_or_else(|e| e.into_inner());
    cursors.insert(path.to_path_buf(), c);
    // Bounded: forget the least recently used files past the cap.
    if cursors.len() > STATS_CURSOR_CAP {
        let mut by_age: Vec<(std::time::Instant, PathBuf)> =
            cursors.iter().map(|(p, c)| (c.used, p.clone())).collect();
        by_age.sort();
        for (_, p) in by_age.into_iter().take(cursors.len() - STATS_CURSOR_CAP) {
            cursors.remove(&p);
        }
    }
    Some(stats)
}

/// What `transcript_stats` knows about a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TranscriptStats {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub first: Option<i64>,
    pub last: Option<i64>,
    /// The latest turn is a final reply: the agent finished.
    pub ended: bool,
    /// When the file was last written.
    pub modified: Option<std::time::SystemTime>,
}

/// How far `transcript_stats` has read a file, and the totals so far.
#[derive(Clone)]
struct StatsCursor {
    /// Moved since last saved (`take_dirty_cursors`).
    dirty: bool,
    ended: bool,
    ino: u64,
    offset: u64,
    input: i64,
    output: i64,
    first: Option<i64>,
    last: Option<i64>,
    used: std::time::Instant,
}

impl Default for StatsCursor {
    fn default() -> Self {
        StatsCursor {
            dirty: false,
            ended: false,
            ino: 0,
            offset: 0,
            input: 0,
            output: 0,
            first: None,
            last: None,
            used: std::time::Instant::now(),
        }
    }
}

impl StatsCursor {
    fn stats(&self) -> TranscriptStats {
        TranscriptStats {
            input_tokens: self.input,
            output_tokens: self.output,
            first: self.first,
            last: self.last,
            ended: self.ended,
            modified: None,
        }
    }
}

/// A transcript's read position and totals, as saved in the DB so a
/// restarted daemon reads forward instead of from the start.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CursorState {
    pub ino: u64,
    pub offset: u64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub first: Option<i64>,
    pub last: Option<i64>,
    pub ended: bool,
}

/// Bump when `transcript_stats` counts differently: saved positions from an
/// older version are dropped and their transcripts recounted.
pub const STATS_PARSE_VERSION: i64 = 1;

/// How many saved positions are loaded at startup (the most recent).
pub const STATS_CURSOR_SEED: usize = STATS_CURSOR_CAP;

/// Load saved positions (daemon start). Files already read this run keep
/// their in-memory position.
pub fn seed_cursors(saved: Vec<(PathBuf, CursorState)>) {
    let mut cursors = STATS_CURSORS.lock().unwrap_or_else(|e| e.into_inner());
    for (path, st) in saved {
        cursors.entry(path).or_insert_with(|| StatsCursor {
            ino: st.ino,
            offset: st.offset,
            input: st.input_tokens,
            output: st.output_tokens,
            first: st.first,
            last: st.last,
            ended: st.ended,
            ..Default::default()
        });
    }
}

/// Positions that moved since the last call, to save.
pub fn take_dirty_cursors() -> Vec<(PathBuf, CursorState)> {
    let mut cursors = STATS_CURSORS.lock().unwrap_or_else(|e| e.into_inner());
    cursors
        .iter_mut()
        .filter(|(_, c)| c.dirty)
        .map(|(p, c)| {
            c.dirty = false;
            (
                p.clone(),
                CursorState {
                    ino: c.ino,
                    offset: c.offset,
                    input_tokens: c.input,
                    output_tokens: c.output,
                    first: c.first,
                    last: c.last,
                    ended: c.ended,
                },
            )
        })
        .collect()
}

/// Forget every position: the next reads start from the top (`--rescan`).
pub fn clear_cursors() {
    STATS_CURSORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// A sub-agent whose transcript hasn't been written for this long has
/// stopped, whatever its meta file says.
const AGENT_QUIET: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Transcripts whose read position is remembered (most recently used).
const STATS_CURSOR_CAP: usize = 1000;

static STATS_CURSORS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, StatsCursor>>,
> = std::sync::LazyLock::new(Default::default);

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
        let stats = transcript_stats(&jsonl).unwrap_or_default();
        // Claude doesn't always mark an agent finished (older versions,
        // agents cut off when the session ended), so also: its last turn is
        // a final reply, or its transcript has gone quiet.
        let quiet = stats
            .modified
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d > AGENT_QUIET);
        let finished =
            meta["finished"].as_bool().unwrap_or(false) || stats.ended || quiet || !parent_running;
        out.push(Subagent {
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
        });
    }
    out
}

#[cfg(test)]
mod subagent_tests {
    use super::*;

    #[test]
    fn excerpts_skip_tool_results_and_wrappers() {
        let dir = std::env::temp_dir().join(format!("rlord-excerpt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let lines = [
            json!({"type":"user","message":{"content":"<command-name>/clear</command-name>"}}),
            json!({"type":"user","message":{"content":"fix the   flaky login test"}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"Looking."},{"type":"tool_use"}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}),
            json!({"type":"user","isCompactSummary":true,"message":{"content":"summary"}}),
            json!({"type":"user","message":{"content":[{"type":"text","text":"now add a retry"}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"Added retry."}]}}),
            json!({"type":"custom-title","customTitle":"login-flake"}),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&p, body).unwrap();
        let e = transcript_excerpts(&p);
        assert_eq!(e.first_prompt.as_deref(), Some("fix the flaky login test"));
        assert_eq!(e.last_prompt.as_deref(), Some("now add a retry"));
        assert_eq!(e.last_reply.as_deref(), Some("Added retry."));
        assert_eq!(e.title.as_deref(), Some("login-flake"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn iso_to_ms() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_iso_ms("2026-09-11T20:20:47.939Z"),
            Some(1_789_158_047_939)
        );
    }

    #[test]
    fn transcript_stats_reads_only_what_was_appended() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("agent-x.jsonl");
        let line = |ts: &str, i: i64| {
            format!(
                "{{\"timestamp\":\"2026-10-27T17:03:{ts}.000Z\",\"message\":{{\"usage\":{{\"input_tokens\":{i},\"output_tokens\":1}}}}}}\n"
            )
        };
        std::fs::write(&p, line("00", 10)).unwrap();
        assert_eq!(transcript_stats(&p).unwrap().input_tokens, 10);
        // Appended: a full line and half of the next.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        let next = line("05", 7);
        write!(f, "{}{}", line("02", 5), &next[..10]).unwrap();
        let st = transcript_stats(&p).unwrap();
        assert_eq!(
            (st.input_tokens, st.output_tokens),
            (15, 2),
            "the half line isn't counted yet"
        );
        assert_eq!(st.last.unwrap() - st.first.unwrap(), 2_000);
        write!(f, "{}", &next[10..]).unwrap();
        assert_eq!(transcript_stats(&p).unwrap().input_tokens, 22);
        // Rewritten shorter: read again from the start.
        std::fs::write(&p, line("00", 3)).unwrap();
        assert_eq!(transcript_stats(&p).unwrap().input_tokens, 3);
        // A final record without its newline still counts, once.
        let whole = line("09", 4);
        write!(
            std::fs::OpenOptions::new().append(true).open(&p).unwrap(),
            "{}",
            whole.trim_end()
        )
        .unwrap();
        assert_eq!(transcript_stats(&p).unwrap().input_tokens, 7);
        assert_eq!(transcript_stats(&p).unwrap().input_tokens, 7);
        assert!(!transcript_stats(&p).unwrap().ended);
        // A final reply ends it; a later tool result reopens it.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"stop_reason":"end_turn"}}}}"#
        )
        .unwrap();
        assert!(transcript_stats(&p).unwrap().ended);
        writeln!(f, r#"{{"type":"user","message":{{}}}}"#).unwrap();
        assert!(!transcript_stats(&p).unwrap().ended);
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"stop_reason":"tool_use"}}}}"#
        )
        .unwrap();
        assert!(!transcript_stats(&p).unwrap().ended);
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"stop_reason":"max_tokens"}}}}"#
        )
        .unwrap();
        assert!(transcript_stats(&p).unwrap().ended);
        // Replaced by a different, longer file: start over.
        let q = dir.path().join("other.jsonl");
        std::fs::write(
            &q,
            format!("{}{}{}", line("00", 1), line("01", 1), line("02", 1)),
        )
        .unwrap();
        std::fs::rename(&q, &p).unwrap();
        assert_eq!(transcript_stats(&p).unwrap().input_tokens, 3);
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
