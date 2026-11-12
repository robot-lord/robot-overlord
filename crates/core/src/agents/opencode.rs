//! OpenCode adapter (OpenCode v2).
//!
//! OpenCode differs from Claude Code and Codex in three ways that shape
//! everything here:
//!
//! - **One shared background server per user** runs the sessions; the TUI
//!   in a pane is a client of it. There are no command hooks: status comes
//!   from the server's event stream (`opencode_service`, read by the
//!   daemon's `opencode` task).
//! - **rlord chooses the conversation id**: `opencode -s <ses_…>` continues
//!   that session, or creates it. Resume is the same command.
//! - **Conversations are rows in OpenCode's SQLite database**, not files. A
//!   session's "transcript path" in rlord is `<opencode.db>/<session id>`:
//!   not a file, but it names the store and the conversation, so the rest
//!   of rlord keeps passing paths (`locate` takes one apart). The functions
//!   under "reading conversations" answer `transcript`'s and `history`'s
//!   questions from the database.
//!
//! The database also holds OpenCode's credentials. rlord opens it
//! read-only and queries only `session_v2` and `session_message`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use super::transcript::{clip, Excerpts, TranscriptStats};

/// The oldest OpenCode rlord runs (v1 has no shared server, another
/// storage and other flags).
pub const MIN_MAJOR: u32 = 2;

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(fallback)
        })
}

/// OpenCode's database: `$XDG_DATA_HOME/opencode/opencode.db`
/// (`~/.local/share/…`, on macOS too).
pub fn db_path() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("opencode/opencode.db")
}

/// Where the background server says how to reach it:
/// `$XDG_STATE_HOME/opencode/service.json`.
pub fn service_file() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("opencode/service.json")
}

// ---- identity ----

/// A new session id in OpenCode's own shape: `ses_`, twelve hex digits of
/// time (descending, so newer sessions sort first, as OpenCode's do), and
/// fourteen random base-62 characters.
pub fn mint_session_id() -> String {
    let ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    mint_at(ms, uuid::Uuid::new_v4().as_bytes())
}

fn mint_at(ms: u64, random: &[u8; 16]) -> String {
    const MASK: u64 = (1 << 48) - 1;
    const B62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let time = !((ms << 12) | 1) & MASK;
    let tail: String = random[..14]
        .iter()
        .map(|b| B62[(*b as usize) % 62] as char)
        .collect();
    format!("ses_{time:012x}{tail}")
}

/// An id OpenCode would accept, and that is safe in a URL path and a file
/// name: `ses_` and letters and digits.
pub fn is_session_id(id: &str) -> bool {
    id.strip_prefix("ses_")
        .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric()))
}

// ---- commands ----

/// `opencode -s <id> [--prompt <text>]`: the full TUI, on the shared
/// background server (never `--standalone`: its events are what rlord
/// reads status from). The TUI takes no model flag: a model is set through
/// the server once the session exists (`Deferred`).
pub fn launch_argv(id: &str, prompt: Option<&str>, extra: &[String]) -> Vec<String> {
    let mut argv = vec!["opencode".to_string(), "-s".to_string(), id.to_string()];
    argv.extend(extra.iter().cloned());
    if let Some(p) = prompt.map(str::trim).filter(|p| !p.is_empty()) {
        argv.push("--prompt".to_string());
        argv.push(p.to_string());
    }
    argv
}

/// `provider/model[#variant]` as the server names a model:
/// `{providerID, id[, variant]}`. (A model id may itself contain `/`.)
pub fn model_ref(model: &str) -> Option<Value> {
    let (provider, rest) = model.trim().split_once('/')?;
    let (id, variant) = match rest.split_once('#') {
        Some((id, v)) => (id, Some(v)),
        None => (rest, None),
    };
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    let mut v = serde_json::json!({"providerID": provider, "id": id});
    if let Some(variant) = variant.filter(|v| !v.is_empty()) {
        v["variant"] = Value::String(variant.to_string());
    }
    Some(v)
}

/// A model as rlord shows it: `provider/model` (from `{providerID, id}`).
fn model_name(m: &Value) -> Option<String> {
    Some(format!(
        "{}/{}",
        m["providerID"].as_str().filter(|p| !p.is_empty())?,
        m["id"].as_str().filter(|p| !p.is_empty())?
    ))
}

// ---- version ----

/// The major version in what `opencode --version` prints (`opencode
/// v2.2.7`, or a bare `1.4.3`).
pub fn major(version: &str) -> Option<u32> {
    let at = version.find(|c: char| c.is_ascii_digit())?;
    version[at..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Why this OpenCode can't be run by rlord, from its `--version` line
/// (None: it can, or the version can't be told).
pub fn too_old(version: &str) -> Option<String> {
    let m = major(version)?;
    (m < MIN_MAJOR).then(|| {
        format!(
            "rlord needs OpenCode v{MIN_MAJOR} or newer (this is {})",
            version.trim()
        )
    })
}

// ---- models ----

/// What `opencode models` prints, one `provider/model` a line.
pub fn parse_models(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| {
            l.contains('/')
                && !l.contains(char::is_whitespace)
                && l.chars().all(|c| c.is_ascii_graphic())
        })
        .map(str::to_string)
        .collect()
}

type Listed = Option<(Instant, Vec<String>)>;
static MODELS: std::sync::Mutex<Listed> = std::sync::Mutex::new(None);

/// How long a model list is taken as current.
const MODELS_FOR: Duration = Duration::from_secs(600);

/// The models OpenCode offers, as last listed (`list_models`). Starts
/// nothing: safe under a lock. Empty until listed: the model is typed in.
pub fn models() -> Vec<String> {
    MODELS
        .lock()
        .ok()
        .and_then(|m| m.as_ref().map(|(_, v)| v.clone()))
        .unwrap_or_default()
}

fn models_fresh() -> bool {
    MODELS.lock().ok().is_some_and(|m| {
        m.as_ref()
            .is_some_and(|(at, v)| !v.is_empty() && at.elapsed() < MODELS_FOR)
    })
}

fn keep_models(listed: Vec<String>) -> bool {
    // (A failed or empty listing keeps the last one.)
    if listed.is_empty() {
        return false;
    }
    if let Ok(mut m) = MODELS.lock() {
        *m = Some((Instant::now(), listed));
    }
    true
}

/// List OpenCode's models when the last list is older than ten minutes:
/// from its background server when one is running (`GET /api/model`), and
/// otherwise, only when `bin` is given, by running `<bin> models` (five
/// seconds at most).
///
/// `bin` is for when the user asked to see them (the new-session form,
/// `rlord agents`): the command may start OpenCode's server, which rlord
/// must not do on its own (at daemon start, before a launch). Makes a
/// request or runs a command: never call it holding a lock others wait on.
pub fn list_models(bin: Option<&Path>) {
    if models_fresh() {
        return;
    }
    let served = super::opencode_service::Service::read(&service_file())
        .and_then(|svc| svc.models())
        .is_some_and(keep_models);
    if served {
        return;
    }
    if let Some(bin) = bin {
        let listed = run_bounded(bin, &["models"], Duration::from_secs(5))
            .map(|out| parse_models(&out))
            .unwrap_or_default();
        keep_models(listed);
    }
}

/// Run a command and take what it prints, giving up at `limit`.
fn run_bounded(bin: &Path, args: &[&str], limit: Duration) -> Option<String> {
    use std::io::Read;
    let mut child = std::process::Command::new(bin)
        .args(args)
        .env_remove("RLORD_SESSION_ID")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        let _ = tx.send(out);
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => break,
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    rx.recv_timeout(Duration::from_millis(500)).ok()
}

// ---- archive summaries (the fallback; see the daemon's `summary`) ----

/// `opencode run` for a one-shot answer in a session of its own, created
/// archived so it stays out of OpenCode's session lists; the caller reads
/// the answer from the database and deletes that session.
/// `fork`: the conversation to read in full (a fork of it; the session
/// itself is untouched). Otherwise `own` is the id the new session gets.
pub fn summary_argv(model: &str, fork: Option<&str>, own: &str, ask: &str) -> Vec<String> {
    let mut a: Vec<String> = vec!["opencode".into(), "run".into(), "-s".into()];
    match fork {
        Some(id) => a.extend([id.to_string(), "--fork".to_string()]),
        None => a.push(own.to_string()),
    }
    a.push("--archived".into());
    if !model.trim().is_empty() {
        a.extend(["-m".to_string(), model.trim().to_string()]);
    }
    a.push(ask.to_string());
    a
}

/// `opencode session delete <id>`.
pub fn delete_argv(id: &str) -> Vec<String> {
    ["opencode", "session", "delete", id]
        .map(String::from)
        .to_vec()
}

// ---- reading conversations ----

/// The path rlord keeps for conversation `id`: `<opencode.db>/<id>`.
pub fn transcript_path(id: &str) -> PathBuf {
    db_path().join(id)
}

/// Whether a path names an OpenCode conversation, by its shape alone.
pub fn is_transcript(path: &Path) -> bool {
    locate(path).is_some()
}

/// Take a conversation's path apart: (the database, the session id).
pub fn locate(path: &Path) -> Option<(&Path, &str)> {
    let id = path.file_name()?.to_str()?;
    let db = path.parent()?;
    (is_session_id(id) && db.file_name().is_some_and(|n| n == "opencode.db")).then_some((db, id))
}

fn open(db: &Path) -> Option<rusqlite::Connection> {
    use rusqlite::OpenFlags;
    if !db.is_file() {
        return None;
    }
    let c = rusqlite::Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    // OpenCode's server may be writing: wait a little, never long.
    c.busy_timeout(Duration::from_millis(200)).ok()?;
    Some(c)
}

/// One message of a conversation, in rlord's common shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    /// The row's sequence number in its session: what `history` calls the
    /// line. A row with several parts (text, tool calls) shares it.
    pub seq: u64,
    pub at_ms: i64,
    /// `you`, `agent` or `tool`.
    pub who: &'static str,
    pub text: String,
}

/// A tool call's main argument.
fn main_arg(input: &Value) -> String {
    [
        "command",
        "cmd",
        "path",
        "file_path",
        "filePath",
        "pattern",
        "query",
        "url",
        "description",
        "prompt",
    ]
    .iter()
    .find_map(|k| input[*k].as_str().map(str::to_string))
    .unwrap_or_else(|| match input {
        Value::Null => String::new(),
        other => clip(&other.to_string(), 200),
    })
}

fn parts_text(parts: &Value) -> Option<String> {
    let t = parts
        .as_array()?
        .iter()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!t.trim().is_empty()).then_some(t)
}

/// A `session_message` row as conversation messages: the user's prompt; an
/// assistant row's text, each tool call (`[tool] main argument`) and what
/// it returned. Reasoning and bookkeeping rows (`idle`, …) aren't
/// conversation.
pub fn said(kind: &str, data: &Value) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    match kind {
        "user" => {
            if let Some(t) = data["text"].as_str().filter(|t| !t.trim().is_empty()) {
                out.push(("you", t.to_string()));
            }
        }
        "assistant" => {
            for part in data["content"].as_array().into_iter().flatten() {
                match part["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = part["text"].as_str().filter(|t| !t.trim().is_empty()) {
                            out.push(("agent", t.to_string()));
                        }
                    }
                    Some("tool") => {
                        let name = part["name"].as_str().unwrap_or("tool");
                        let state = &part["state"];
                        out.push((
                            "agent",
                            format!("[{name}] {}", main_arg(&state["input"]))
                                .trim_end()
                                .to_string(),
                        ));
                        let result = parts_text(&state["content"]).or_else(|| {
                            state["error"]
                                .as_str()
                                .filter(|e| !e.trim().is_empty())
                                .map(str::to_string)
                        });
                        if let Some(t) = result {
                            out.push(("tool", t));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// The text an assistant row answered with (its text parts).
fn reply_text(data: &Value) -> Option<String> {
    parts_text(&data["content"])
}

/// Rows of one session, oldest first, through `each` (false stops).
/// `like`: only rows whose JSON contains it (ASCII case-insensitive).
fn each_row(
    c: &rusqlite::Connection,
    id: &str,
    like: Option<&str>,
    mut each: impl FnMut(u64, &str, i64, &Value) -> bool,
) {
    let sql = format!(
        "SELECT seq, type, time_created, data FROM session_message \
         WHERE session_id = ?1 AND type IN ('user', 'assistant') {} ORDER BY seq",
        if like.is_some() {
            "AND data LIKE ?2 ESCAPE '\\'"
        } else {
            "AND ?2 IS NULL"
        }
    );
    let Ok(mut q) = c.prepare(&sql) else {
        return; // not a schema rlord knows
    };
    let pattern = like.map(|k| {
        let escaped = k
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        format!("%{escaped}%")
    });
    let Ok(mut rows) = q.query(rusqlite::params![id, pattern]) else {
        return;
    };
    while let Ok(Some(r)) = rows.next() {
        let (Ok(seq), Ok(kind), Ok(at), Ok(data)) = (
            r.get::<_, i64>(0),
            r.get::<_, String>(1),
            r.get::<_, i64>(2),
            r.get::<_, String>(3),
        ) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if !each(seq.max(0) as u64, &kind, at, &v) {
            break;
        }
    }
}

/// The whole conversation, oldest first.
pub fn conversation(db: &Path, id: &str) -> Vec<Said> {
    let Some(c) = open(db) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    each_row(&c, id, None, |seq, kind, at_ms, data| {
        out.extend(said(kind, data).into_iter().map(|(who, text)| Said {
            seq,
            at_ms,
            who,
            text,
        }));
        true
    });
    out
}

/// Messages of `id` containing every word of `words` (lowercased), oldest
/// first: all of them counted, the first `keep` returned. `key`: a word
/// that must appear in a row's raw JSON (narrows what is parsed).
pub fn search(
    db: &Path,
    id: &str,
    words: &[String],
    key: Option<&str>,
    keep: usize,
) -> (Vec<Said>, usize) {
    let Some(c) = open(db) else {
        return (Vec::new(), 0);
    };
    let (mut hits, mut total) = (Vec::new(), 0);
    each_row(&c, id, key, |seq, kind, at_ms, data| {
        for (who, text) in said(kind, data) {
            let lower = text.to_lowercase();
            if words.iter().all(|w| lower.contains(w.as_str())) {
                total += 1;
                if hits.len() < keep {
                    hits.push(Said {
                        seq,
                        at_ms,
                        who,
                        text,
                    });
                }
            }
        }
        true
    });
    (hits, total)
}

/// One row's `data` by a query that selects it.
fn one_data(c: &rusqlite::Connection, sql: &str, id: &str) -> Option<Value> {
    let data: String = c.query_row(sql, [id], |r| r.get(0)).ok()?;
    serde_json::from_str(&data).ok()
}

/// The prompt that started it, the latest prompt, the latest reply and the
/// title, by a few indexed look-ups (never the whole conversation).
pub fn excerpts(db: &Path, id: &str) -> Excerpts {
    let Some(c) = open(db) else {
        return Excerpts::default();
    };
    let prompt = |order: &str| {
        one_data(
            &c,
            &format!(
                "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'user' \
                 ORDER BY seq {order} LIMIT 1"
            ),
            id,
        )
        .and_then(|v| v["text"].as_str().map(|t| clip(t, 300)))
        .filter(|t| !t.is_empty())
    };
    let first_prompt = prompt("ASC");
    let last_prompt = prompt("DESC").filter(|l| Some(l) != first_prompt.as_ref());
    Excerpts {
        first_prompt,
        last_prompt,
        last_reply: last_reply(&c, id).map(|t| clip(&t, 500)),
        title: c
            .query_row("SELECT title FROM session_v2 WHERE id = ?1", [id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .ok()
            .flatten()
            .filter(|t| !t.trim().is_empty())
            .map(|t| clip(&t, 120)),
    }
}

/// The latest text the agent answered with (among its last rows: a turn
/// ends in tool-free text, but the very last row may be a tool call).
fn last_reply(c: &rusqlite::Connection, id: &str) -> Option<String> {
    let mut q = c
        .prepare(
            "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'assistant' \
             ORDER BY seq DESC LIMIT 20",
        )
        .ok()?;
    let rows = q.query_map([id], |r| r.get::<_, String>(0)).ok()?;
    for data in rows.flatten() {
        let reply = serde_json::from_str::<Value>(&data)
            .ok()
            .and_then(|v| reply_text(&v));
        if reply.is_some() {
            return reply;
        }
    }
    None
}

/// The latest reply of conversation `id` (for a summary run's answer).
pub fn latest_reply(db: &Path, id: &str) -> Option<String> {
    last_reply(&open(db)?, id)
}

/// When a session's latest turn began and ended, as its row records them
/// (`time_busy`, `time_idle`; ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Turn {
    pub busy: Option<i64>,
    pub idle: Option<i64>,
}

impl Turn {
    /// A turn is running: it began after the last one ended.
    pub fn working(&self) -> bool {
        match (self.busy, self.idle) {
            (Some(b), Some(i)) => b > i,
            (Some(_), None) => true,
            _ => false,
        }
    }
}

/// The latest turn of session `id`. None: OpenCode has no such session
/// (yet), or its database can't be read.
pub fn turn(db: &Path, id: &str) -> Option<Turn> {
    open(db)?
        .query_row(
            "SELECT time_busy, time_idle FROM session_v2 WHERE id = ?1",
            [id],
            |r| {
                Ok(Turn {
                    busy: r.get(0)?,
                    idle: r.get(1)?,
                })
            },
        )
        .ok()
}

/// What OpenCode's session row says: token totals, when it started and
/// was last written, and whether a turn is running (`time_busy` after
/// `time_idle`). None: no such session (yet).
pub fn stats(db: &Path, id: &str) -> Option<TranscriptStats> {
    let c = open(db)?;
    c.query_row(
        "SELECT tokens_input, tokens_output, time_created, time_updated, time_busy, time_idle \
         FROM session_v2 WHERE id = ?1",
        [id],
        |r| {
            let (busy, idle): (Option<i64>, Option<i64>) = (r.get(4)?, r.get(5)?);
            let updated: i64 = r.get(3)?;
            Ok(TranscriptStats {
                input_tokens: r.get(0)?,
                output_tokens: r.get(1)?,
                first: Some(r.get(2)?),
                // (A row's own times; `time_updated` stops at the last
                // message, `time_idle` is when the turn ended.)
                last: Some(updated.max(idle.unwrap_or(0))),
                ended: busy.is_some() && !Turn { busy, idle }.working(),
                turn_seen: busy.is_some(),
                modified: Some(
                    SystemTime::UNIX_EPOCH
                        + Duration::from_millis(updated.max(idle.unwrap_or(0)).max(0) as u64),
                ),
            })
        },
    )
    .ok()
}

/// The size of the context the latest request sent (its input and cached
/// tokens), 0 when there has been none.
pub fn context_tokens(db: &Path, id: &str) -> u64 {
    let Some(c) = open(db) else {
        return 0;
    };
    one_data(
        &c,
        "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'assistant' \
         ORDER BY seq DESC LIMIT 1",
        id,
    )
    .map(|v| {
        let t = &v["tokens"];
        [&t["input"], &t["cache"]["read"], &t["cache"]["write"]]
            .iter()
            .filter_map(|n| n.as_u64())
            .sum()
    })
    .unwrap_or(0)
}

/// The model of the latest turn (`provider/model`); before any, the one
/// the session is set to.
pub fn observed_model(db: &Path, id: &str) -> Option<String> {
    let c = open(db)?;
    one_data(
        &c,
        "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'assistant' \
         ORDER BY seq DESC LIMIT 1",
        id,
    )
    .and_then(|v| model_name(&v["model"]))
    .or_else(|| {
        one_data(&c, "SELECT model FROM session_v2 WHERE id = ?1", id).and_then(|m| model_name(&m))
    })
}

/// OpenCode has a session `id`.
pub fn exists(db: &Path, id: &str) -> bool {
    open(db).is_some_and(|c| {
        c.query_row("SELECT 1 FROM session_v2 WHERE id = ?1", [id], |_| Ok(()))
            .is_ok()
    })
}

/// The folder a session runs in.
pub fn directory(db: &Path, id: &str) -> Option<String> {
    open(db)?
        .query_row(
            "SELECT directory FROM session_v2 WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .ok()
}

/// The session `id` is a child of (a sub-agent's session names its parent).
pub fn parent(db: &Path, id: &str) -> Option<String> {
    open(db)?
        .query_row(
            "SELECT parent_id FROM session_v2 WHERE id = ?1",
            [id],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
}

/// The newest archived fork of `id` created at or after `since_ms`: the
/// one a summary run made (`summary_argv`). Archived only, so a fork the
/// user made themselves at that moment is never taken for it.
pub fn fork_of(db: &Path, id: &str, since_ms: i64) -> Option<String> {
    open(db)?
        .query_row(
            "SELECT id FROM session_v2 WHERE fork_session_id = ?1 AND time_created >= ?2 \
             AND time_archived IS NOT NULL ORDER BY time_created DESC LIMIT 1",
            rusqlite::params![id, since_ms],
            |r| r.get(0),
        )
        .ok()
}

/// Every top-level session (not sub-agents' child sessions), with when it
/// was last written.
pub fn sessions(db: &Path) -> Vec<(SystemTime, String)> {
    let Some(c) = open(db) else {
        return Vec::new();
    };
    let Ok(mut q) = c.prepare("SELECT id, time_updated FROM session_v2 WHERE parent_id IS NULL")
    else {
        return Vec::new();
    };
    let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)));
    match rows {
        Ok(rows) => rows
            .flatten()
            .filter(|(id, _)| is_session_id(id))
            .map(|(id, ms)| {
                (
                    SystemTime::UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64),
                    id,
                )
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Milliseconds since the epoch as ISO-8601 (`2026-11-12T16:55:21.075Z`),
/// the form the other agents' transcripts give message times in.
pub fn iso_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    // civil from days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) const SES: &str = "ses_e3192cfeeffeUElN5gOL3f5nhg";

    /// A database as OpenCode 2.2.7 keeps one (the columns rlord reads),
    /// with a session shaped like a real export: a prompt, tool calls with
    /// reasoning, the answer, the `idle` row that ends the turn.
    pub(crate) fn store(dir: &Path) -> PathBuf {
        let db = dir.join("opencode.db");
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE session_v2 (id text PRIMARY KEY, project_id text NOT NULL, \
               parent_id text, fork_session_id text, directory text NOT NULL, title text, \
               tokens_input integer DEFAULT 0 NOT NULL, tokens_output integer DEFAULT 0 NOT NULL, \
               model text, time_created integer NOT NULL, time_updated integer NOT NULL, \
               time_busy integer, time_idle integer, time_archived integer);
             CREATE TABLE session_message (id text PRIMARY KEY, session_id text NOT NULL, \
               type text NOT NULL, seq integer NOT NULL, time_created integer NOT NULL, \
               time_updated integer NOT NULL, data text NOT NULL);
             CREATE TABLE credential (id text PRIMARY KEY, secret text);",
        )
        .unwrap();
        c.execute(
            "INSERT INTO session_v2 VALUES (?1, 'p', NULL, NULL, '/src/app', 'Repo purpose walkthrough', \
             18736, 1545, ?2, 1794464624674, 1794464641936, 1794464624706, 1794464641938, NULL)",
            rusqlite::params![SES, json!({"id": "deepseek-v4.1-flash-free", "providerID": "opencode"}).to_string()],
        )
        .unwrap();
        let model = json!({"id": "deepseek-v4.1-flash-free", "providerID": "opencode", "variant": "default"});
        let tool = |name: &str, input: Value, output: &str| {
            json!({"type": "tool", "id": "call_1", "name": name, "executed": false,
                   "state": {"status": "completed", "input": input,
                             "content": [{"type": "text", "text": output}],
                             "metadata": {"exit": 0}}})
        };
        let rows = [
            (
                4,
                "user",
                1794464624711_i64,
                json!({"time": {"created": 1794464624711_i64},
                "text": "take a look at this repo and describe what it does", "files": [], "agents": []}),
            ),
            (
                7,
                "assistant",
                1794464624720,
                json!({"agent": "build", "model": model,
                "content": [{"type": "text", "text": "I'll explore the repo to understand what it does."},
                            tool("shell", json!({"command": "ls -la"}), "total 616\nCLAUDE.md\nMakefile")],
                "finish": "tool-calls",
                "tokens": {"input": 900, "output": 40, "reasoning": 0, "cache": {"read": 0, "write": 0}}}),
            ),
            (
                31,
                "assistant",
                1794464627768,
                json!({"agent": "build", "model": model,
                "content": [{"type": "reasoning", "text": "Let me look at the structure a bit more."},
                            tool("read", json!({"path": "README.md"}), "# wait2watch"),
                            tool("glob", json!({"pattern": "apps/*"}), "apps/web\napps/worker")],
                "finish": "tool-calls",
                "tokens": {"input": 1200, "output": 60, "reasoning": 12, "cache": {"read": 9000, "write": 0}}}),
            ),
            (
                81,
                "assistant",
                1794464634808,
                json!({"agent": "build", "model": model,
                "content": [{"type": "reasoning", "text": "I have enough to describe the repo well."},
                            {"type": "text", "text": "The repo tracks where films are streaming and notifies you."}],
                "finish": "stop",
                "tokens": {"input": 1517, "output": 880, "reasoning": 18, "cache": {"read": 20736, "write": 0}}}),
            ),
            (
                88,
                "idle",
                1794464641938,
                json!({"time": {"created": 1794464641938_i64}, "outcome": "succeeded"}),
            ),
        ];
        for (seq, kind, at, data) in rows {
            c.execute(
                "INSERT INTO session_message VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
                rusqlite::params![format!("msg_{seq}"), SES, kind, seq, at, data.to_string()],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn minted_ids_have_opencodes_shape() {
        // The time part of a real id: its session was created at …624674.
        let id = mint_at(1794464624657, &[7; 16]);
        assert!(id.starts_with("ses_e3192cfeeffe"), "{id}");
        assert_eq!(id.len(), "ses_".len() + 12 + 14);
        assert!(is_session_id(&id));
        let (a, b) = (mint_session_id(), mint_session_id());
        assert!(is_session_id(&a) && a.len() == 30);
        assert_ne!(a, b);
        // Newer ids sort first.
        assert!(mint_at(2_000, &[0; 16]) < mint_at(1_000, &[0; 16]));
        for bad in ["", "ses_", "e3192cfeeffe", "ses_../x", "ses_a b", "msg_abc"] {
            assert!(!is_session_id(bad), "{bad}");
        }
    }

    #[test]
    fn argv() {
        assert_eq!(
            launch_argv("ses_1a", Some("hello"), &[]),
            ["opencode", "-s", "ses_1a", "--prompt", "hello"]
        );
        assert_eq!(
            launch_argv("ses_1a", Some("  "), &["--auto".to_string()]),
            ["opencode", "-s", "ses_1a", "--auto"]
        );
        let a = summary_argv("opencode/x-free", Some("ses_1a"), "ses_2b", "Summarize");
        assert_eq!(
            a,
            [
                "opencode",
                "run",
                "-s",
                "ses_1a",
                "--fork",
                "--archived",
                "-m",
                "opencode/x-free",
                "Summarize"
            ]
        );
        let b = summary_argv("", None, "ses_2b", "Summarize");
        assert_eq!(
            b,
            ["opencode", "run", "-s", "ses_2b", "--archived", "Summarize"]
        );
        // Never auto-approving, never a private server.
        for argv in [&a, &b] {
            assert!(!argv.iter().any(|x| x == "--auto" || x == "--standalone"));
        }
    }

    #[test]
    fn models_are_named_provider_slash_model() {
        assert_eq!(
            model_ref("opencode/glm-5.3-flash-free"),
            Some(json!({"providerID": "opencode", "id": "glm-5.3-flash-free"}))
        );
        assert_eq!(
            model_ref(" openrouter/anthropic/claude#high "),
            Some(json!({"providerID": "openrouter", "id": "anthropic/claude", "variant": "high"}))
        );
        assert_eq!(model_ref("opus"), None);
        assert_eq!(model_ref("/x"), None);
        let out = "opencode/beam-free\nopencode/deepseek-v4.1-flash-free\n\nsome notice here\n";
        assert_eq!(
            parse_models(out),
            ["opencode/beam-free", "opencode/deepseek-v4.1-flash-free"]
        );
    }

    #[test]
    fn version_two_is_required() {
        assert_eq!(major("opencode v2.2.7"), Some(2));
        assert_eq!(major("1.4.3"), Some(1));
        assert_eq!(major("opencode"), None);
        assert_eq!(too_old("opencode v2.2.7"), None);
        assert_eq!(too_old("v3.0.0"), None);
        assert!(too_old("1.4.3").unwrap().contains("v2"));
        // Unknown: not refused.
        assert_eq!(too_old("dev build"), None);
    }

    #[test]
    fn a_path_names_the_store_and_the_conversation() {
        let p = PathBuf::from("/h/.local/share/opencode/opencode.db").join(SES);
        assert_eq!(
            locate(&p),
            Some((Path::new("/h/.local/share/opencode/opencode.db"), SES))
        );
        for other in [
            "/h/.claude/projects/-x/abc.jsonl",
            "/h/.codex/sessions/2026/11/11/rollout-x.jsonl",
            "/h/opencode.db/notes.txt",
            "/h/other.db/ses_abc",
        ] {
            assert_eq!(locate(Path::new(other)), None, "{other}");
        }
    }

    #[test]
    fn rows_read_as_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        let all = conversation(&db, SES);
        let who: Vec<&str> = all.iter().map(|m| m.who).collect();
        // Not the reasoning, nor the `idle` row.
        assert_eq!(
            who,
            ["you", "agent", "agent", "tool", "agent", "tool", "agent", "tool", "agent"]
        );
        assert_eq!(all[2].text, "[shell] ls -la");
        assert_eq!(all[3].text, "total 616\nCLAUDE.md\nMakefile");
        assert_eq!(all[4].text, "[read] README.md");
        assert_eq!(all[6].text, "[glob] apps/*");
        assert_eq!((all[0].seq, all[1].seq, all[8].seq), (4, 7, 81));
        assert_eq!(
            super::super::transcript::parse_iso_ms(&iso_ms(all[0].at_ms)),
            Some(all[0].at_ms)
        );
        assert_eq!(iso_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_ms(1_789_158_047_939), "2026-09-11T20:20:47.939Z");
        assert!(conversation(&db, "ses_none").is_empty());
        assert!(conversation(&dir.path().join("absent.db"), SES).is_empty());
    }

    #[test]
    fn excerpts_stats_model_and_turn_state() {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        let e = excerpts(&db, SES);
        assert_eq!(
            e.first_prompt.as_deref(),
            Some("take a look at this repo and describe what it does")
        );
        assert_eq!(e.last_prompt, None, "one prompt so far: not repeated");
        assert_eq!(
            e.last_reply.as_deref(),
            Some("The repo tracks where films are streaming and notifies you.")
        );
        assert_eq!(e.title.as_deref(), Some("Repo purpose walkthrough"));
        let st = stats(&db, SES).unwrap();
        assert_eq!((st.input_tokens, st.output_tokens), (18736, 1545));
        assert!(st.ended && st.turn_seen);
        assert_eq!(st.last.unwrap() - st.first.unwrap(), 17_264);
        assert_eq!(context_tokens(&db, SES), 1517 + 20736);
        assert_eq!(
            observed_model(&db, SES).as_deref(),
            Some("opencode/deepseek-v4.1-flash-free")
        );
        assert!(exists(&db, SES) && !exists(&db, "ses_none"));
        assert_eq!(directory(&db, SES).as_deref(), Some("/src/app"));
        assert_eq!(stats(&db, "ses_none"), None);

        // A second prompt: a turn is running again.
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute(
            "INSERT INTO session_message VALUES ('msg_95', ?1, 'user', 95, 1794464700000, \
             1794464700000, ?2)",
            rusqlite::params![SES, json!({"text": "now add a retry budget"}).to_string()],
        )
        .unwrap();
        c.execute(
            "UPDATE session_v2 SET time_busy = 1794464700001, time_updated = 1794464700000",
            [],
        )
        .unwrap();
        let st = stats(&db, SES).unwrap();
        assert!(!st.ended && st.turn_seen);
        assert!(turn(&db, SES).unwrap().working());
        assert_eq!(turn(&db, "ses_none"), None);
        // Created, nothing asked yet: no turn.
        let fresh = Turn {
            busy: None,
            idle: None,
        };
        assert!(!fresh.working());
        assert_eq!(
            excerpts(&db, SES).last_prompt.as_deref(),
            Some("now add a retry budget")
        );
        // A child session and a fork.
        c.execute_batch(
            "INSERT INTO session_v2 (id, project_id, parent_id, directory, time_created, time_updated) \
               VALUES ('ses_child', 'p', 'ses_e3192cfeeffeUElN5gOL3f5nhg', '/src/app', 5, 5);
             INSERT INTO session_v2 (id, project_id, fork_session_id, directory, time_created, time_updated, time_archived) \
               VALUES ('ses_fork', 'p', 'ses_e3192cfeeffeUElN5gOL3f5nhg', '/src/app', 9000, 9000, 9000);",
        )
        .unwrap();
        assert_eq!(parent(&db, "ses_child").as_deref(), Some(SES));
        assert_eq!(parent(&db, SES), None);
        assert_eq!(fork_of(&db, SES, 8000).as_deref(), Some("ses_fork"));
        assert_eq!(fork_of(&db, SES, 9001), None);
        // A fork the user made (not archived) isn't the summary's.
        c.execute(
            "UPDATE session_v2 SET time_archived = NULL WHERE id = 'ses_fork'",
            [],
        )
        .unwrap();
        assert_eq!(fork_of(&db, SES, 8000), None);
        let mut ids: Vec<String> = sessions(&db).into_iter().map(|(_, id)| id).collect();
        ids.sort();
        assert_eq!(ids, [SES, "ses_fork"], "not the child session");
    }

    #[test]
    fn search_needs_every_word_in_one_message() {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        let ws = |q: &str| -> Vec<String> { q.split_whitespace().map(str::to_string).collect() };
        let (hits, total) = search(&db, SES, &ws("repo describe"), Some("describe"), 5);
        assert_eq!((hits.len(), total), (1, 1));
        assert_eq!((hits[0].who, hits[0].seq), ("you", 4));
        let (hits, total) = search(&db, SES, &ws("repo"), Some("repo"), 2);
        assert_eq!((hits.len(), total), (2, 3));
        assert_eq!((hits[1].who, hits[1].seq), ("agent", 7));
        // A tool's label and its output are searched; the count goes on
        // past what is kept.
        assert_eq!(search(&db, SES, &ws("[shell]"), None, 5).1, 1);
        assert_eq!(search(&db, SES, &ws("apps"), Some("apps"), 1).1, 2);
        assert_eq!(search(&db, SES, &ws("apps"), Some("apps"), 1).0.len(), 1);
        assert_eq!(
            search(&db, SES, &ws("repo nothing"), Some("nothing"), 5).1,
            0
        );
        // `%` and `_` in a word are themselves, not wildcards.
        assert_eq!(search(&db, SES, &ws("100%"), Some("100%"), 5).1, 0);
    }
}
