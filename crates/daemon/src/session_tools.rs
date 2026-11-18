//! A session's own look-ups: read-only tools its agent can call to pull
//! its background and what related sessions did, on demand, instead of the
//! overlord copying it into the prompt.
//!
//! - **Token.** Each session (never an overlord) gets a random token at its
//!   start, kept across restarts (kv `session_token:<id>` and the 0600 file
//!   `<data>/sessions/<id>/token`), removed when it is archived. The
//!   daemon maps a token to its session; `sctx.*` takes no session id from
//!   the caller.
//! - **Front ends.** `rlord mcp --session --token-file <p>` (an MCP server
//!   named `rlord_session`, wired into Claude and in-pane Codex at every
//!   start: `wiring`) and `rlord context <tool>` (any agent's shell).
//! - **Surface.** The tools in `tools()`, over the session's visible set
//!   (`visible`): itself, the sessions linked to it (background links, its
//!   fork parent and forks), sessions in the same repo, and sessions
//!   sharing a tag. Archived ones count; overlords never do. This is a
//!   separate dispatch from `mcp::call`: no overlord tool is reachable.
//! - **Bounds.** Plain code: no model, agent CLI or tmux. Per session 30
//!   calls a minute and 300 an hour; any answer is at most 16,000 chars.
//!
//! Not a boundary against a process running as the same user (it can read
//! any token file and call the socket): what it gives is a read-only
//! surface, bound to the worker, that can't be steered into other sessions.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::agents::transcript::{clip, transcript_excerpts};
use rlord_core::agents::{codex, Agent};
use rlord_core::db::{ArchivedMode, Db, SessionQuery};
use rlord_core::types::Session;

use crate::overlord::overlords_at;
use crate::Daemon;

/// The MCP server name the session's agent sees.
pub const SERVER: &str = "rlord_session";
/// The pane env var naming the token file (a path, never the token).
pub use rlord_core::agents::TOKEN_ENV;

/// Whether in-pane Codex sessions get the `rlord_session` MCP server
/// (`-c mcp_servers.rlord_session.*`). On since live check V5 passed
/// (Codex 0.169.0, `--no-daemon`: it called `session_brief` with no
/// approval prompt). Daemon-mode sessions still get the CLI line.
pub const CODEX_MCP: bool = true;

const ANSWER_MAX: usize = 16_000;
const PER_MINUTE: usize = 30;
const PER_HOUR: usize = 300;

fn token_key(id: &str) -> String {
    format!("session_token:{id}")
}

/// `<base>/sessions/<id>/token`.
pub fn token_path(base: &Path, id: &str) -> PathBuf {
    token_in(&crate::context::session_dir(base, id))
}

/// The token file in a session's own folder `dir`.
pub fn token_in(dir: &Path) -> PathBuf {
    dir.join("token")
}

/// Undo what `wiring` made for a session whose launch then failed: its
/// token (file and kv), its MCP config, its recorded look-ups.
pub fn forget(db: &Db, base: &Path, id: &str) {
    let _ = revoke(db, base, id);
    let _ = std::fs::remove_file(mcp_config_path(base, id));
    let _ = db.kv_delete(&kind_key(id));
}

/// `<base>/sessions/<id>/mcp.json` (Claude's `--mcp-config`).
pub fn mcp_config_path(base: &Path, id: &str) -> PathBuf {
    crate::context::session_dir(base, id).join("mcp.json")
}

/// 32 random bytes as hex (two v4 uuids' bytes: the OS's random source).
fn random_hex() -> String {
    [uuid::Uuid::new_v4(), uuid::Uuid::new_v4()]
        .iter()
        .flat_map(|u| *u.as_bytes())
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// The session's token file, made if it has none (or its file is gone):
/// kept across restarts. Returns its path.
pub fn mint(db: &Db, base: &Path, id: &str) -> Result<PathBuf> {
    let path = token_path(base, id);
    let token = match db.kv_get(&token_key(id))? {
        Some(t) if !t.is_empty() => t,
        _ => {
            let t = random_hex();
            db.kv_set(&token_key(id), &t)?;
            t
        }
    };
    let on_disk = std::fs::read_to_string(&path).ok();
    if on_disk.as_deref().map(str::trim) != Some(token.as_str()) {
        rlord_core::private::dir_under(base, &crate::context::session_dir(base, id))?;
        rlord_core::private::write(&path, token.as_bytes())?;
    }
    Ok(path)
}

/// Remove the session's token (archived): its tools stop answering. The
/// next start mints a new one.
pub fn revoke(db: &Db, base: &Path, id: &str) -> Result<()> {
    db.kv_delete(&token_key(id))?;
    match std::fs::remove_file(token_path(base, id)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Equal, in time that doesn't depend on where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The session a token belongs to. Refused: unknown, archived, overlord.
pub fn resolve(db: &Db, token: &str) -> Result<Session> {
    let token = token.trim();
    if token.len() < 32 {
        bail!("unknown session token");
    }
    let mut found = None;
    for (k, v) in db.kv_prefix("session_token:")? {
        if same(v.as_bytes(), token.as_bytes()) {
            found = Some(k["session_token:".len()..].to_string());
        }
    }
    let id = found.context("unknown session token")?;
    let s = db.resolve_ref(&id).context("unknown session token")?;
    if overlords_at(&s.role, &s.project_path) {
        bail!("not for the overlord");
    }
    if s.archived_at.is_some() {
        bail!("this session is archived");
    }
    Ok(s)
}

fn kind_key(id: &str) -> String {
    format!("session_lookup:{id}")
}

/// How session `s`'s agent can look things up: `mcp` (the
/// `rlord_session` tools: its last start was given them, kv
/// `session_lookup:<id>`), `cli` (`rlord context` only: Codex on its
/// background server, OpenCode, or a start whose wiring failed; not to be
/// relied on inside Codex's sandbox), `none` (an overlord, or no token
/// yet: its next start makes one).
pub fn lookup_kind(db: &Db, base: &Path, s: &Session) -> &'static str {
    if overlords_at(&s.role, &s.project_path) || !token_path(base, &s.id).is_file() {
        return "none";
    }
    match db.kv_get(&kind_key(&s.id)).ok().flatten().as_deref() {
        Some("mcp") => "mcp",
        _ => "cli",
    }
}

/// The flags that give session `id` its tools at a start, and make its
/// token (best effort: a failure is logged and the start has no tools).
/// Claude: `--mcp-config` and `--allowedTools` (in `=` form: both take
/// several values, and the first prompt may follow). Codex in its pane:
/// `-c mcp_servers.rlord_session.*`. Codex on its background server and
/// OpenCode: none (their background names `rlord context`). Overlords:
/// none, and no token.
pub fn wiring(
    db: &Db,
    base: &Path,
    id: &str,
    agent: Agent,
    role: &str,
    folder: &str,
    daemon: bool,
) -> Vec<String> {
    if overlords_at(role, folder) {
        return Vec::new();
    }
    let token = match mint(db, base, id) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(session = %id, error = %e, "making the session's token");
            let _ = db.kv_delete(&kind_key(id));
            return Vec::new();
        }
    };
    let bin = crate::methods::rlord_bin();
    let socket = rlord_core::paths::socket_path()
        .to_string_lossy()
        .into_owned();
    let args = match wiring_args(
        agent,
        daemon,
        &bin,
        &socket,
        &token,
        &mcp_config_path(base, id),
    ) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(session = %id, error = %e, "writing the session's MCP config");
            Vec::new()
        }
    };
    // What this start gives it (`lookup_kind`): tools only if it has flags.
    let kind = if args.is_empty() { "cli" } else { "mcp" };
    if let Err(e) = db.kv_set(&kind_key(id), kind) {
        tracing::warn!(session = %id, error = %e, "recording the session's look-ups");
    }
    args
}

/// `rlord mcp --session …` args.
pub fn server_args(socket: &str, token: &Path) -> Vec<String> {
    vec![
        "mcp".into(),
        "--session".into(),
        "--token-file".into(),
        token.to_string_lossy().into_owned(),
        "--socket".into(),
        socket.into(),
    ]
}

/// `wiring`'s flags (writing Claude's config file).
pub fn wiring_args(
    agent: Agent,
    daemon: bool,
    bin: &str,
    socket: &str,
    token: &Path,
    mcp_json: &Path,
) -> Result<Vec<String>> {
    let args = server_args(socket, token);
    Ok(match agent {
        Agent::Claude => {
            let cfg = json!({"mcpServers": {SERVER: {
                "type": "stdio", "command": bin, "args": args}}});
            rlord_core::private::write(mcp_json, serde_json::to_string_pretty(&cfg)?.as_bytes())?;
            vec![
                format!("--mcp-config={}", mcp_json.display()),
                format!("--allowedTools=mcp__{SERVER}"),
            ]
        }
        Agent::Codex if !daemon && CODEX_MCP => {
            let q: Vec<String> = args.iter().map(|a| codex::toml_str(a)).collect();
            [
                format!("mcp_servers.{SERVER}.command={}", codex::toml_str(bin)),
                format!("mcp_servers.{SERVER}.args=[{}]", q.join(",")),
                format!("mcp_servers.{SERVER}.default_tools_approval_mode=\"approve\""),
            ]
            .into_iter()
            .flat_map(|kv| ["-c".to_string(), kv])
            .collect()
        }
        Agent::Codex | Agent::OpenCode => Vec::new(),
    })
}

/// The "Looking up more" section of a session's background: how to call
/// its tools. `dir`: its own folder (`context::session_dir`). None when it
/// has no token yet.
pub fn lookup_section(dir: &Path) -> Option<String> {
    let token = token_in(dir);
    if !token.is_file() {
        return None;
    }
    let q = |p: &str| rlord_core::tmux::shell_quote(p);
    Some(format!(
        "\n## Looking up more\n\
         You can look up more yourself, read-only: your own brief again, \
         related sessions' descriptions, notes and summaries, and their \
         history. Use the `{SERVER}` tools if you have them; otherwise run \
         `{} context --token-file {} --socket {} help`.\n",
        q(&crate::methods::rlord_bin()),
        q(&token.to_string_lossy()),
        q(&rlord_core::paths::socket_path().to_string_lossy())
    ))
}

// ---- rate limit ----

/// Each session's recent look-up calls (one per `Daemon`).
#[derive(Default)]
pub struct Limiter(Mutex<HashMap<String, VecDeque<Instant>>>);

impl Limiter {
    /// Count a call for `id`; refused over the limits. Ids with no call in
    /// the last hour are dropped.
    pub fn admit(&self, id: &str, now: Instant) -> Result<()> {
        let hour = Duration::from_secs(3600);
        let mut g = self.0.lock().expect("rate mutex");
        for q in g.values_mut() {
            while q.front().is_some_and(|t| now.duration_since(*t) > hour) {
                q.pop_front();
            }
        }
        g.retain(|_, q| !q.is_empty());
        admit_into(g.entry(id.to_string()).or_default(), now)
    }

    #[cfg(test)]
    fn ids(&self) -> usize {
        self.0.lock().expect("rate mutex").len()
    }
}

fn admit_into(q: &mut VecDeque<Instant>, now: Instant) -> Result<()> {
    let last_minute = q
        .iter()
        .filter(|t| now.duration_since(**t) <= Duration::from_secs(60))
        .count();
    if last_minute >= PER_MINUTE || q.len() >= PER_HOUR {
        bail!("too many look-ups: at most {PER_MINUTE} a minute and {PER_HOUR} an hour");
    }
    q.push_back(now);
    Ok(())
}

// ---- tools ----

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"name": name, "description": description,
           "inputSchema": {"type": "object", "properties": properties, "required": required}})
}

/// The tools, for `sctx.tools` (every session gets the same list).
pub fn tools() -> Value {
    let s = |d: &str| json!({"type": "string", "description": d});
    let n = |d: &str| json!({"type": "integer", "description": d});
    let session = "One of your related sessions: its id, id prefix or name (see related_sessions)";
    json!([
        tool("session_brief",
             "Your own session as rlord knows it: name, description, notes, tags, folder, branch, the user's first prompt, the overlord's background notes, and your related sessions. Read it again after a compaction.",
             json!({}), &[]),
        tool("related_sessions",
             "Sessions related to yours (linked by the overlord, a fork, the same repo, or a shared tag), best first, each with what it is about and why it's related.",
             json!({"query": s("Words that must all appear in its name, description, notes, summary, tags or branch"),
                    "limit": n("How many (default 6, at most 10)")}), &[]),
        tool("session_summary",
             "One related session in more detail: description, notes, summary, the user's first and last prompts, and its last reply.",
             json!({"session": s(session)}), &["session"]),
        tool("search_history",
             "Search what your related sessions (and yours) said and did. Each hit has a line to pass to read_history.",
             json!({"query": s("Words to look for"),
                    "session": s(&format!("{session}; omit to search them all")),
                    "limit": n("How many sessions (default 5, at most 8)")}), &["query"]),
        tool("read_history",
             "Read a related session's conversation: its end, or around a line from search_history.",
             json!({"session": s(session),
                    "line": n("A hit's line; omit for the end of the conversation"),
                    "transcript": s("A hit's transcript, when search_history names one (an older conversation of that session)"),
                    "before": n("Messages before the line (default 6)"),
                    "after": n("Messages after it (default 14)")}), &["session"]),
    ])
}

/// Names of the tools.
pub fn names() -> Vec<String> {
    tools()
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str().map(str::to_string))
        .collect()
}

/// A visible session and why.
pub struct Seen {
    pub session: Session,
    pub why: &'static str,
    rank: u8,
}

/// `me`'s visible set, best first (linked, same repo, shared tag; then
/// most recently active), itself not included.
pub fn visible(db: &Db, me: &Session) -> Result<Vec<Seen>> {
    let mut linked: HashSet<String> = db.context_links(&me.id)?.into_iter().collect();
    linked.extend(db.context_linkers(&me.id)?);
    linked.extend(me.forked_from.clone());
    let all = db.sessions(&SessionQuery {
        archived: ArchivedMode::All,
        ..Default::default()
    })?;
    let mut out: Vec<Seen> = all
        .into_iter()
        .filter(|s| s.id != me.id)
        .filter(|s| !overlords_at(&s.role, &s.project_path))
        .filter_map(|s| {
            let (rank, why) = if linked.contains(&s.id) || s.forked_from.as_deref() == Some(&me.id)
            {
                (0, "linked")
            } else if me.repo_path.is_some() && s.repo_path == me.repo_path {
                (1, "same repo")
            } else if s.tags.iter().any(|t| me.tags.contains(t)) {
                (2, "same group")
            } else {
                return None;
            };
            Some(Seen {
                session: s,
                why,
                rank,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        a.rank
            .cmp(&b.rank)
            .then(crate::triage::updated_at(&b.session).cmp(&crate::triage::updated_at(&a.session)))
    });
    Ok(out)
}

/// A session `me` can see, by ref (itself included), resolved only among
/// them: its id, else its exact name, else a unique name prefix.
fn pick<'a>(me: &'a Session, seen: &'a [Seen], r: &str) -> Result<&'a Session> {
    let r = r.trim();
    let all: Vec<&Session> = std::iter::once(me)
        .chain(seen.iter().map(|v| &v.session))
        .collect();
    if let Some(s) = all.iter().find(|s| s.id == r) {
        return Ok(s);
    }
    if let Some(s) = all.iter().find(|s| s.name == r) {
        return Ok(s);
    }
    if !r.is_empty() {
        let ids: Vec<&&Session> = all.iter().filter(|s| s.id.starts_with(r)).collect();
        if let [s] = ids.as_slice() {
            return Ok(s);
        }
    }
    let pre: Vec<&&Session> = all
        .iter()
        .filter(|s| !r.is_empty() && s.name.starts_with(r))
        .collect();
    match pre.as_slice() {
        [s] => Ok(s),
        [] => bail!("no such session among those this one can see"),
        many => bail!(
            "'{r}' could be any of {}: give more of the name",
            many.iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn arg<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn status_of(s: &Session) -> String {
    if s.archived_at.is_some() {
        "archived".into()
    } else {
        s.status.label().to_string()
    }
}

/// The user's first prompt as rlord stored it: the launch spec's words,
/// else the launch's binding copy (for Codex only the line pointing at
/// its prompt file), else the transcript.
fn first_prompt(d: &Daemon, s: &Session) -> Option<String> {
    let stored = {
        let db = d.db();
        crate::first_prompt::launch_words(&db, &s.id).or_else(|| {
            db.kv_get(&crate::methods::bind_prompt_key(&s.id))
                .ok()
                .flatten()
                .filter(|p| !p.trim().is_empty())
        })
    };
    stored.or_else(|| {
        crate::summary::transcript_of(d, s).and_then(|p| {
            crate::first_prompt::task(&s.id, transcript_excerpts(&p).first_prompt, None)
        })
    })
}

/// Call tool `name` for the session holding `token`.
pub fn call(d: &Daemon, token: &str, name: &str, args: &Value) -> Result<Value> {
    let me = resolve(&d.db(), token)?;
    if !names().iter().any(|n| n == name) {
        bail!("{name}: not one of this session's tools");
    }
    d.session_calls.admit(&me.id, Instant::now())?;
    tracing::info!(session = %me.id, tool = name, "session tool call");
    // (Worked out once per call.)
    let seen = visible(&d.db(), &me)?;
    let v = run(d, &me, &seen, name, args)?;
    Ok(capped(v))
}

/// An answer at most `ANSWER_MAX` chars (as JSON); a longer one comes
/// back clipped as text.
fn capped(v: Value) -> Value {
    let size = |v: &Value| serde_json::to_string(v).map_or(0, |t| t.chars().count());
    if size(&v) <= ANSWER_MAX {
        return v;
    }
    // Long strings (notes, prompts, snippets) clipped shorter and shorter,
    // then long lists halved: always valid JSON, never cut text.
    let mut max = 4_000;
    let mut out = v;
    loop {
        clip_strings(&mut out, max);
        if size(&out) <= ANSWER_MAX - 20 {
            break;
        }
        if max > 50 {
            max /= 2;
        } else if !halve_lists(&mut out) {
            out = json!({});
            break;
        }
    }
    if let Value::Object(m) = &mut out {
        m.insert("clipped".into(), json!(true));
    }
    out
}

fn clip_strings(v: &mut Value, max: usize) {
    match v {
        Value::String(s) if s.chars().count() > max => *s = clip(s, max),
        Value::Array(a) => a.iter_mut().for_each(|x| clip_strings(x, max)),
        Value::Object(m) => m.values_mut().for_each(|x| clip_strings(x, max)),
        _ => {}
    }
}

/// Halve every list longer than one; false when there was none.
fn halve_lists(v: &mut Value) -> bool {
    match v {
        Value::Array(a) => {
            let mut any = a.len() > 1;
            a.truncate(a.len().div_ceil(2));
            for x in a.iter_mut() {
                any |= halve_lists(x);
            }
            any
        }
        Value::Object(m) => {
            let mut any = false;
            for x in m.values_mut() {
                any |= halve_lists(x);
            }
            any
        }
        _ => false,
    }
}

fn run(d: &Daemon, me: &Session, seen: &[Seen], name: &str, args: &Value) -> Result<Value> {
    match name {
        "session_brief" => {
            let rows = d.db().context_rows(&me.id)?;
            let related: Vec<Value> = seen
                .iter()
                .filter(|v| v.rank == 0)
                .map(|v| {
                    json!({"name": v.session.name, "id": v.session.id,
                           "status": status_of(&v.session)})
                })
                .collect();
            let notes: Vec<String> = rows
                .iter()
                .filter(|r| r.kind == "note")
                .map(|r| crate::context::clean(r.body.trim()))
                .collect();
            Ok(json!({
                "name": me.name, "id": me.id, "agent": me.agent,
                "description": me.description,
                "notes": clip(&me.notes, 8_000),
                "tags": me.tags,
                "folder": me.project_path, "repo": me.repo_path, "branch": me.branch,
                "forked_from": me.forked_from_name,
                "first_prompt": first_prompt(d, me).map(|p| clip(&p, 4_000)),
                "background_notes": clip(&notes.join("\n\n"), 8_000),
                "related": related,
            }))
        }
        "related_sessions" => {
            let limit = args["limit"].as_u64().unwrap_or(6).clamp(1, 10) as usize;
            let words: Vec<String> = arg(args, "query")
                .unwrap_or("")
                .split_whitespace()
                .map(str::to_lowercase)
                .collect();
            let rows: Vec<Value> = seen
                .iter()
                .filter(|v| {
                    let s = &v.session;
                    let hay = format!(
                        "{} {} {} {} {} {}",
                        s.name,
                        s.description,
                        s.notes,
                        s.summary,
                        s.tags.join(" "),
                        s.branch.as_deref().unwrap_or("")
                    )
                    .to_lowercase();
                    words.iter().all(|w| hay.contains(w.as_str()))
                })
                .take(limit)
                .map(|v| {
                    let s = &v.session;
                    json!({"name": s.name, "id": s.id, "agent": s.agent,
                           "status": status_of(s), "branch": s.branch, "folder": s.project_path,
                           "description": clip(&s.description, 200),
                           "notes": clip(&s.notes, 300),
                           "summary": clip(&s.summary, 500),
                           "why": v.why})
                })
                .collect();
            Ok(json!({"count": rows.len(), "sessions": rows}))
        }
        "session_summary" => {
            let r = arg(args, "session").context("session is required")?;
            let s = pick(me, seen, r)?;
            let ex = crate::summary::transcript_of(d, s)
                .map(|p| transcript_excerpts(&p))
                .unwrap_or_default();
            let c = |t: Option<String>| t.map(|t| clip(&t, 500));
            Ok(json!({
                "name": s.name, "id": s.id, "agent": s.agent, "status": status_of(s),
                "folder": s.project_path, "branch": s.branch,
                "description": s.description,
                "notes": clip(&s.notes, 2_000),
                "summary": clip(&s.summary, 3_000),
                "first_prompt": c(first_prompt(d, s)),
                "last_prompt": c(ex.last_prompt),
                "last_reply": c(ex.last_reply),
            }))
        }
        "search_history" => {
            let query = arg(args, "query").context("query is required")?;
            let limit = args["limit"].as_u64().unwrap_or(5).clamp(1, 8);
            let only: HashSet<String> = seen.iter().map(|v| v.session.id.clone()).collect();
            // (Not itself: it has its own conversation.)
            let mut p = json!({"query": query, "limit": limit});
            if let Some(r) = arg(args, "session") {
                p["session"] = json!(pick(me, seen, r)?.id);
            }
            let mut v = crate::search::search_in(d, &p, Some(&only))?;
            v["note"] = json!(
                "read_history with a session and a hit's line (and its transcript, when it names one) reads the conversation around it"
            );
            Ok(v)
        }
        "read_history" => {
            let r = arg(args, "session").context("session is required")?;
            let s = pick(me, seen, r)?;
            // A hit in an older transcript (before a `/clear`) names it:
            // `read_with` takes only one of that session's own.
            let mut p = json!({"session": s.id});
            if let Some(t) = arg(args, "transcript") {
                p["transcript"] = json!(t);
            }
            for k in ["line", "before", "after"] {
                if let Some(n) = args.get(k).and_then(Value::as_u64) {
                    p[k] = json!(n);
                }
            }
            crate::search::read_with(d, &p, 8_000)
        }
        _ => bail!("{name}: not one of this session's tools"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlord_core::config::Config;
    use rlord_core::db::NewSession;

    fn add(db: &Db, id: &str, repo: Option<&str>, role: &str) {
        db.insert_session(&NewSession {
            id,
            name: id,
            agent: "claude",
            agent_session: None,
            project_path: &format!("/tmp/rlord-sctx-test/{id}"),
            tmux_session: None,
            launch_spec: "{}",
            created_at: 1,
            repo_path: repo,
            workspace_path: None,
            branch: None,
            role,
        })
        .unwrap();
    }

    fn base(tag: &str) -> PathBuf {
        let b = std::env::temp_dir().join(format!("rlord-sctx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&b);
        b
    }

    fn daemon(tag: &str) -> (std::sync::Arc<Daemon>, PathBuf, String) {
        let db = Db::open_in_memory().unwrap();
        add(&db, "01ME", Some("/r/one"), "");
        add(&db, "01SAMEREPO", Some("/r/one"), "");
        add(&db, "01LINKED", Some("/r/two"), "");
        add(&db, "01TAGGED", Some("/r/three"), "");
        add(&db, "01STRANGER", Some("/r/four"), "");
        add(&db, "01OVER", Some("/r/one"), "overlord");
        db.context_add("01ME", 1, "link", "01LINKED", "overlord")
            .unwrap();
        db.add_tag("01ME", "web").unwrap();
        db.add_tag("01TAGGED", "web").unwrap();
        db.set_notes("01STRANGER", "secret plans").unwrap();
        let b = base(tag);
        let path = mint(&db, &b, "01ME").unwrap();
        let token = std::fs::read_to_string(path).unwrap();
        (Daemon::new(db, Config::default()), b, token)
    }

    #[test]
    fn the_token_is_private_kept_and_revoked() {
        let db = Db::open_in_memory().unwrap();
        add(&db, "01A", None, "");
        let b = base("tok");
        let p = mint(&db, &b, "01A").unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let t = std::fs::read_to_string(&p).unwrap();
        assert_eq!(t.len(), 64);
        // Kept across starts; its file rewritten when gone.
        std::fs::remove_file(&p).unwrap();
        mint(&db, &b, "01A").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), t);
        assert_eq!(resolve(&db, &t).unwrap().id, "01A");
        revoke(&db, &b, "01A").unwrap();
        assert!(!p.exists());
        assert!(resolve(&db, &t).is_err());
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn unknown_short_archived_and_overlord_tokens_are_refused() {
        let db = Db::open_in_memory().unwrap();
        add(&db, "01A", None, "");
        add(&db, "01O", None, "overlord");
        let b = base("ref");
        assert!(resolve(&db, "").is_err());
        assert!(resolve(&db, &"0".repeat(64)).is_err());
        let o = std::fs::read_to_string(mint(&db, &b, "01O").unwrap()).unwrap();
        assert!(resolve(&db, &o)
            .unwrap_err()
            .to_string()
            .contains("overlord"));
        let a = std::fs::read_to_string(mint(&db, &b, "01A").unwrap()).unwrap();
        db.set_archived("01A", Some(5)).unwrap();
        assert!(resolve(&db, &a).is_err());
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn the_visible_set_is_linked_same_repo_and_same_group_never_overlords() {
        let (d, b, _) = daemon("d1");
        let me = d.db().resolve_ref("01ME").unwrap();
        let seen: Vec<(String, &str)> = visible(&d.db(), &me)
            .unwrap()
            .into_iter()
            .map(|v| (v.session.id, v.why))
            .collect();
        assert_eq!(
            seen,
            [
                ("01LINKED".to_string(), "linked"),
                ("01SAMEREPO".to_string(), "same repo"),
                ("01TAGGED".to_string(), "same group"),
            ]
        );
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn a_session_cannot_read_an_unrelated_one() {
        let (d, b, token) = daemon("d2");
        for (tool, args) in [
            ("session_summary", json!({"session": "01STRANGER"})),
            ("read_history", json!({"session": "01STRANGER"})),
            (
                "search_history",
                json!({"query": "x", "session": "01STRANGER"}),
            ),
            ("session_summary", json!({"session": "01OVER"})),
        ] {
            let e = call(&d, &token, tool, &args).unwrap_err();
            assert!(e.to_string().contains("can see"), "{tool}: {e}");
        }
        let r = call(&d, &token, "related_sessions", &json!({})).unwrap();
        assert!(!r.to_string().contains("secret plans"), "{r}");
        assert!(!r.to_string().contains("01OVER"), "{r}");
        let ok = call(
            &d,
            &token,
            "session_summary",
            &json!({"session": "01LINKED"}),
        )
        .unwrap();
        assert_eq!(ok["id"], "01LINKED");
        let brief = call(&d, &token, "session_brief", &json!({})).unwrap();
        assert_eq!(brief["id"], "01ME");
        assert_eq!(brief["related"][0]["id"], "01LINKED");
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn overlord_tools_and_the_transcript_argument_are_not_reachable() {
        let (d, b, token) = daemon("d3");
        for t in [
            "send_to_session",
            "create_session",
            "update_session",
            "read_screen",
            "list_sessions",
        ] {
            assert!(call(&d, &token, t, &json!({})).is_err(), "{t}");
        }
        // No tool here is an overlord tool that changes anything: the
        // two shared names are on the read-only server too.
        let overlord: Vec<String> = crate::mcp::tools()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_string))
            .collect();
        for n in names() {
            assert!(
                !overlord.contains(&n) || crate::mcp::READ_ONLY_TOOLS.contains(&n.as_str()),
                "{n}"
            );
        }
        // A transcript path that isn't the session's own is refused.
        let e = call(
            &d,
            &token,
            "read_history",
            &json!({"session": "01LINKED", "transcript": "/etc/passwd"}),
        )
        .unwrap_err();
        assert!(e.to_string().contains("isn't this session's"), "{e}");
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn the_rate_limit_holds() {
        let l = Limiter::default();
        let t0 = Instant::now();
        for _ in 0..PER_MINUTE {
            l.admit("01RATE", t0).unwrap();
        }
        assert!(l.admit("01RATE", t0).is_err());
        // A minute on: room again, until the hour's cap.
        let t1 = t0 + Duration::from_secs(61);
        l.admit("01RATE", t1).unwrap();
        // An id idle for an hour is dropped.
        l.admit("01OTHER", t1 + Duration::from_secs(3_700)).unwrap();
        assert_eq!(l.ids(), 1);
    }

    #[test]
    fn answers_are_capped() {
        let v = capped(json!({"notes": "y".repeat(40_000), "name": "s"}));
        assert_eq!(v["clipped"], true);
        assert_eq!(v["name"], "s");
        assert!(v["notes"].as_str().unwrap().len() < 16_000);
        let hits: Vec<Value> = (0..2_000)
            .map(|i| json!({"line": i, "text": "z".repeat(60)}))
            .collect();
        let v = capped(json!({"sessions": [{"hits": hits}]}));
        assert!(serde_json::to_string(&v).unwrap().chars().count() <= ANSWER_MAX);
        assert!(v["sessions"][0]["hits"].as_array().unwrap().len() > 10);
        assert_eq!(capped(json!({"a": 1})), json!({"a": 1}));
    }

    #[test]
    fn the_background_ends_with_how_to_look_more_up_within_budget() {
        let b = base("look");
        let db = Db::open_in_memory().unwrap();
        add(&db, "01L", None, "");
        let dir = crate::context::session_dir(&b, "01L");
        assert!(lookup_section(&dir).is_none());
        mint(&db, &b, "01L").unwrap();
        let tail = lookup_section(&dir).unwrap();
        assert!(tail.contains("rlord_session") && tail.contains("context --token-file"));
        assert!(tail.contains(" --socket "));
        let rows: Vec<rlord_core::db::ContextRow> = (0..50)
            .map(|i| rlord_core::db::ContextRow {
                id: i + 1,
                at: 1,
                kind: "note".into(),
                body: "n".repeat(300),
                source: "overlord".into(),
                seen_at: None,
            })
            .collect();
        // The content first; the section if it still fits.
        let text = crate::context::render_as("s", true, &rows[..2], &[], None, 6_000, Some(&tail));
        assert!(text.ends_with(&tail));
        let text = crate::context::render_as("s", true, &rows, &[], None, 6_000, Some(&tail));
        assert!(!text.contains("Looking up more"));
        assert!(text.chars().count() <= 6_000, "{}", text.chars().count());
        // A budget too small for the section too: it is left out, never
        // over budget.
        let header = crate::context::render_as("s", true, &[], &[], None, 100_000, None)
            .chars()
            .count();
        let t = tail.chars().count();
        let few = &rows[..1];
        let one = crate::context::render_as("s", true, few, &[], None, 100_000, None)
            .chars()
            .count();
        for (budget, fits) in [(one + t - 1, false), (one + t, true)] {
            let text = crate::context::render_as("s", true, few, &[], None, budget, Some(&tail));
            assert_eq!(text.contains("Looking up more"), fits, "{budget}");
            assert!(text.chars().count() <= budget, "{budget}");
        }
        // Tight slack: every budget from the header alone up, never over.
        for budget in header..header + t + 300 {
            let text = crate::context::render_as("s", true, &rows, &[], None, budget, Some(&tail));
            assert!(text.chars().count() <= budget, "{budget}");
        }
        let _ = std::fs::remove_dir_all(&b);
    }

    /// `lookup` reaches the overlord through `mcp::call` without taking
    /// the DB lock twice (it isn't re-entrant: a second lock hangs).
    /// `create_session` and `fork_session` add it through the same
    /// `lookup_of` after their launch (which needs tmux and an agent, so
    /// isn't run here).
    #[test]
    fn get_session_shows_lookup_without_deadlock() {
        let (d, b, _) = daemon("d6");
        let (tx, rx) = std::sync::mpsc::channel();
        let d2 = d.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::mcp::call(
                &d2,
                "get_session",
                &json!({"session": "01ME"}),
            ));
        });
        let r = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("get_session hung (DB lock taken twice?)")
            .unwrap();
        // (Its token is in the test's own folder, not the data dir.)
        assert!(r["lookup"].is_string(), "{r}");
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn search_history_leaves_out_the_caller() {
        let (d, b, token) = daemon("d7");
        let e = call(
            &d,
            &token,
            "search_history",
            &json!({"query": "x", "session": "01ME"}),
        )
        .unwrap_err();
        assert!(e.to_string().contains("can see"), "{e}");
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn a_failed_launch_forgets_its_token() {
        let b = base("forget");
        let db = Db::open_in_memory().unwrap();
        let folder = "/tmp/rlord-sctx-test/01G";
        assert!(!wiring(&db, &b, "01G", Agent::Claude, "", folder, false).is_empty());
        forget(&db, &b, "01G");
        assert!(!token_path(&b, "01G").exists());
        assert!(!mcp_config_path(&b, "01G").exists());
        assert!(db.kv_get("session_token:01G").unwrap().is_none());
        assert!(db.kv_get("session_lookup:01G").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn the_cli_line_quotes_its_paths() {
        let b = base("quote it");
        let db = Db::open_in_memory().unwrap();
        mint(&db, &b, "01Q").unwrap();
        let tail = lookup_section(&crate::context::session_dir(&b, "01Q")).unwrap();
        let tok = token_path(&b, "01Q").to_string_lossy().into_owned();
        assert!(
            tail.contains(&rlord_core::tmux::shell_quote(&tok)),
            "{tail}"
        );
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn wiring_per_agent() {
        let b = base("wire");
        std::fs::create_dir_all(&b).unwrap();
        let tok = b.join("token");
        let cfg = b.join("mcp.json");
        let claude = wiring_args(Agent::Claude, false, "/bin/rlord", "/s", &tok, &cfg).unwrap();
        assert_eq!(claude[0], format!("--mcp-config={}", cfg.display()));
        assert_eq!(claude[1], "--allowedTools=mcp__rlord_session");
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            written["mcpServers"]["rlord_session"]["args"][1],
            "--session"
        );
        let codex = wiring_args(Agent::Codex, false, "/bin/rlord", "/s", &tok, &cfg).unwrap();
        if CODEX_MCP {
            assert_eq!(codex[0], "-c");
            assert!(codex[1].starts_with("mcp_servers.rlord_session.command="));
            assert!(codex
                .iter()
                .any(|a| a.contains("default_tools_approval_mode")));
        } else {
            assert!(codex.is_empty());
        }
        assert!(wiring_args(Agent::Codex, true, "/b", "/s", &tok, &cfg)
            .unwrap()
            .is_empty());
        assert!(wiring_args(Agent::OpenCode, false, "/b", "/s", &tok, &cfg)
            .unwrap()
            .is_empty());
        // Overlords: nothing, and no token.
        let db = Db::open_in_memory().unwrap();
        assert!(wiring(&db, &b, "01O", Agent::Claude, "overlord", "/x", false).is_empty());
        assert!(!token_path(&b, "01O").exists());
        // A session in an overlord's folder: the same.
        let folder = crate::overlord::dirs()[0].to_string_lossy().into_owned();
        assert!(wiring(&db, &b, "01F", Agent::Claude, "", &folder, false).is_empty());
        assert!(!token_path(&b, "01F").exists());
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn lookup_kind_says_mcp_cli_or_none() {
        let b = base("kind");
        let db = Db::open_in_memory().unwrap();
        add(&db, "01K", None, "");
        let s = db.resolve_ref("01K").unwrap();
        let folder = s.project_path.clone();
        assert_eq!(lookup_kind(&db, &b, &s), "none");
        // What the last start's wiring gave it.
        let start = |agent, daemon| wiring(&db, &b, "01K", agent, "", &folder, daemon);
        assert!(!start(Agent::Claude, false).is_empty());
        assert_eq!(lookup_kind(&db, &b, &s), "mcp");
        assert!(start(Agent::Codex, true).is_empty());
        assert_eq!(lookup_kind(&db, &b, &s), "cli");
        assert_eq!(!start(Agent::Codex, false).is_empty(), CODEX_MCP);
        let want = if CODEX_MCP { "mcp" } else { "cli" };
        assert_eq!(lookup_kind(&db, &b, &s), want);
        assert!(start(Agent::OpenCode, false).is_empty());
        assert_eq!(lookup_kind(&db, &b, &s), "cli");
        // A token but no recorded start (before 1.3.0's wiring): cli.
        db.kv_delete("session_lookup:01K").unwrap();
        assert_eq!(lookup_kind(&db, &b, &s), "cli");
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn refs_resolve_only_among_the_visible() {
        let (d, b, _) = daemon("d4");
        let me = d.db().resolve_ref("01ME").unwrap();
        let seen = visible(&d.db(), &me).unwrap();
        assert_eq!(pick(&me, &seen, "01LINKED").unwrap().id, "01LINKED");
        assert_eq!(pick(&me, &seen, "01ME").unwrap().id, "01ME");
        // A unique prefix of a visible name; one that is ambiguous, or
        // names only sessions outside the set, is refused.
        assert_eq!(pick(&me, &seen, "01LIN").unwrap().id, "01LINKED");
        // A unique id prefix that is no name's prefix.
        {
            let db = d.db();
            db.rename("01LINKED", "web-app").unwrap();
        }
        let seen = visible(&d.db(), &me).unwrap();
        assert_eq!(pick(&me, &seen, "01LINK").unwrap().id, "01LINKED");
        assert_eq!(pick(&me, &seen, "web").unwrap().id, "01LINKED");
        // (01STRANGER is not visible: no clash.)
        assert_eq!(pick(&me, &seen, "01S").unwrap().id, "01SAMEREPO");
        assert_eq!(pick(&me, &seen, "01SA").unwrap().id, "01SAMEREPO");
        assert!(pick(&me, &seen, "01ST").is_err());
        assert!(pick(&me, &seen, "01OV").is_err());
        let _ = std::fs::remove_dir_all(&b);
    }

    /// A hit in a session's older conversation (before a `/clear`) names
    /// its transcript: read_history reads it; a file that isn't one of
    /// that session's is refused.
    #[test]
    fn read_history_reads_a_hit_before_a_clear() {
        let (d, b, token) = daemon("d5");
        let dir = b.join("projects");
        std::fs::create_dir_all(&dir).unwrap();
        let line = |text: &str| {
            json!({"type": "user", "timestamp": "2026-01-01T00:00:00Z",
                   "message": {"role": "user", "content": text}})
            .to_string()
                + "\n"
        };
        let old = dir.join("conv-old.jsonl");
        let new = dir.join("conv-new.jsonl");
        std::fs::write(&old, line("the zebracorn plan")).unwrap();
        std::fs::write(&new, line("after the clear")).unwrap();
        {
            let db = d.db();
            db.upsert_transcript_source("01LINKED", &new.to_string_lossy())
                .unwrap();
            db.insert_event(
                "01LINKED",
                5,
                "fresh_conversation",
                &json!({"from": "conv-old", "to": "conv-new", "reason": "clear"}).to_string(),
            )
            .unwrap();
        }
        let hits = call(&d, &token, "search_history", &json!({"query": "zebracorn"})).unwrap();
        let hit = &hits["sessions"][0]["hits"][0];
        let t = hit["transcript"]
            .as_str()
            .expect("names the older transcript");
        let r = call(
            &d,
            &token,
            "read_history",
            &json!({"session": "01LINKED", "transcript": t, "line": hit["line"]}),
        )
        .unwrap();
        assert!(r.to_string().contains("zebracorn"), "{r}");
        let e = call(
            &d,
            &token,
            "read_history",
            &json!({"session": "01LINKED", "transcript": "/etc/passwd"}),
        )
        .unwrap_err();
        assert!(e.to_string().contains("isn't this session's"), "{e}");
        let _ = std::fs::remove_dir_all(&b);
    }
}
