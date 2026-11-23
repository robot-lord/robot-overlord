//! Finding agent conversations rlord doesn't track yet, so existing work can
//! be imported. Reads only the agents' own files (no AI, and nothing specific
//! to whichever session manager started them): for Claude Code, the
//! transcripts under `~/.claude/projects/<cwd slug>/<uuid>.jsonl`; for
//! Codex, its rollouts (`~/.codex/sessions/YYYY/MM/DD/rollout-*-<id>.jsonl`)
//! and its thread index (`state_5.sqlite`), read as `agents::codex` reads
//! them for running sessions. OpenCode isn't importable.
//!
//! The formats are internal to the agents and change between versions, so
//! every field is optional and anything unreadable is skipped, never fatal.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::agents::transcript::{self, Format};
use crate::agents::{claude, codex};
use crate::git;

/// One conversation that could be imported.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Candidate {
    pub agent: String,
    /// The agent's own conversation id (Claude: the transcript's uuid).
    pub agent_session: String,
    pub transcript: String,
    /// Where the conversation runs; resuming must happen in this folder.
    pub cwd: String,
    pub dir_exists: bool,
    /// Main repo root, when `cwd` is (or was) inside a git repo.
    pub repo: Option<String>,
    /// The branch the conversation was last on.
    pub branch: Option<String>,
    /// `cwd` is a worktree of `repo`, not its main checkout.
    pub worktree: bool,
    /// `cwd` is gone, but `branch` still exists in `repo`, so the worktree
    /// can be recreated at `cwd` and the conversation resumed there.
    pub recreatable: bool,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub last_reply: Option<String>,
    pub pr_url: Option<String>,
    /// Where it was started from (Claude's `entrypoint`: cli, claude-desktop…).
    pub entrypoint: Option<String>,
    /// First message, ms epoch.
    pub started_at: Option<i64>,
    /// Latest real exchange (a user or assistant message), ms epoch. Not the
    /// file's mtime: Claude appends bookkeeping lines without any exchange,
    /// and copying transcripts between machines resets mtimes.
    pub last_active_at: i64,
    pub last_user_at: Option<i64>,
    pub last_agent_at: Option<i64>,
    /// A running process has this conversation id on its command line: it is
    /// open somewhere else right now (another session manager, a terminal).
    pub open_elsewhere: bool,
    /// (Codex) started by an app-server client (an editor, the Codex app,
    /// Codex's background server) and written in the last few minutes: it
    /// may still be held there.
    pub maybe_held_by_codex_server: bool,
    /// The transcript was written in the last few minutes: possibly open
    /// somewhere even if no command line names it.
    pub recently_written: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ScanOpts {
    /// Only conversations with an exchange at or after this (ms epoch).
    pub since_ms: i64,
    /// Max candidates, most recently active first. 0 = no limit.
    pub limit: usize,
    /// Only conversations whose id starts with one of these (full ids or
    /// prefixes; ignores `since_ms`).
    pub only: Vec<String>,
    /// rlord's own Codex sessions that haven't learned their thread id
    /// yet: (folder, created at ms). A thread started in that folder after
    /// that is maybe the session's own, so the scan doesn't list it.
    pub unbound_codex: Vec<(String, i64)>,
}

/// How recently a transcript must have been written to count as possibly open.
const RECENT_WRITE: Duration = Duration::from_secs(10 * 60);

/// Scan Claude Code's transcripts. `tracked` says whether rlord already has
/// a session for a conversation id; those are skipped. Git details are left
/// empty: call `add_git_info` on the candidates you keep.
pub fn scan_claude(opts: &ScanOpts, tracked: &dyn Fn(&str) -> bool) -> Vec<Candidate> {
    let projects = claude::config_dir().join("projects");
    let procs = process_commands();
    scan_claude_in(&projects, opts, tracked, &procs, &excluded_dirs())
}

/// The folders Claude Code conversations ran in, for the code-folder
/// suggestion: the newest `limit` project dirs (by mtime) under
/// `~/.claude/projects`, each read through the head and tail of at most a
/// few of its transcripts (no stat of every file).
pub fn claude_project_folders(limit: usize) -> Vec<String> {
    claude_project_folders_in(&claude::config_dir().join("projects"), limit)
}

/// How many of one project dir's transcripts `claude_project_folders`
/// reads at most looking for its folder.
const TRANSCRIPTS_PER_DIR: usize = 3;

/// `claude_project_folders` over an explicit projects dir (tests). The
/// slug is lossy, so a dir's folder is a `cwd` from its transcript whose
/// slug matches the dir's name; a dir with none is skipped.
pub fn claude_project_folders_in(projects: &Path, limit: usize) -> Vec<String> {
    let Ok(dirs) = std::fs::read_dir(projects) else {
        return Vec::new();
    };
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut dirs: Vec<(Option<SystemTime>, PathBuf)> = dirs
        .flatten()
        .filter(|d| d.file_type().is_ok_and(|t| t.is_dir()))
        .map(|d| (mtime(&d.path()), d.path()))
        .collect();
    dirs.sort_by_key(|a| std::cmp::Reverse(a.0));
    dirs.truncate(limit);
    let mut out = Vec::new();
    for (_, dir) in dirs {
        let slug = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        // Any of a folder's transcripts names it: the first few that
        // yield one, by name only (no stat of every file).
        let found = files
            .flatten()
            .map(|f| f.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .take(TRANSCRIPTS_PER_DIR)
            .find_map(|t| {
                let (head, tail) = transcript::read_head_tail(&t);
                head.iter()
                    .chain(tail.iter())
                    .filter_map(|v| v["cwd"].as_str())
                    .find(|c| claude::project_slug(c) == slug)
                    .map(str::to_string)
            });
        out.extend(found);
    }
    out
}

/// Whether any Claude Code conversation exists that `scan_claude` would
/// list with the same `since_ms` and `tracked`: for the first-run offer.
/// The same checks (`is_conversation_file`, mtime, `candidate`), but
/// bounded: the newest `MAX_DIRS` project dirs (rlord's own and temp ones
/// skipped by name before any read), at most `MAX_READS` transcripts read,
/// stopping at the first. If the budget runs out on rejects, it answers
/// true: in-window untracked transcripts exist, and the overlord's scan
/// gives the exact answer.
pub fn has_claude_conversations(since_ms: i64, tracked: &dyn Fn(&str) -> bool) -> bool {
    let projects = claude::config_dir().join("projects");
    has_claude_conversations_in(&projects, since_ms, tracked, &excluded_dirs())
}

const MAX_DIRS: usize = 500;
const MAX_READS: usize = 50;

/// `has_claude_conversations` over an explicit projects dir (tests).
pub fn has_claude_conversations_in(
    projects: &Path,
    since_ms: i64,
    tracked: &dyn Fn(&str) -> bool,
    excluded: &[PathBuf],
) -> bool {
    let Ok(dirs) = std::fs::read_dir(projects) else {
        return false;
    };
    // Skipped by name: the slug of a folder under an excluded one starts
    // with its slug (lossy, so `candidate` still checks the real cwd).
    let excluded_slugs: Vec<String> = excluded
        .iter()
        .map(|e| claude::project_slug(&e.to_string_lossy()))
        .collect();
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut dirs: Vec<(Option<SystemTime>, PathBuf, String)> = dirs
        .flatten()
        .filter(|d| d.file_type().is_ok_and(|t| t.is_dir()))
        .map(|d| (d.path(), d.file_name().to_string_lossy().into_owned()))
        .filter(|(_, slug)| {
            !slug.ends_with("-robot-overlord-overlord")
                && !excluded_slugs.iter().any(|e| slug.starts_with(e.as_str()))
        })
        .map(|(p, slug)| (mtime(&p), p, slug))
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
    let mut reads = 0;
    for (_, dir, slug) in dirs.into_iter().take(MAX_DIRS) {
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            let Some(id) = is_conversation_file(&path) else {
                continue;
            };
            if tracked(id) {
                continue;
            }
            let mtime = f.metadata().and_then(|m| m.modified()).ok().map(ms_epoch);
            if mtime.is_some_and(|m| m < since_ms) {
                continue;
            }
            // Budget spent on rejects: in-window untracked transcripts
            // exist, so offer; the overlord's scan has the exact answer.
            if reads >= MAX_READS {
                return true;
            }
            reads += 1;
            // (No process list: whether it's open elsewhere doesn't matter.)
            if candidate(&path, id, &slug, mtime, "", excluded)
                .is_ok_and(|c| c.last_active_at >= since_ms)
            {
                return true;
            }
        }
    }
    false
}

/// The conversation id when `path` is a Claude transcript: `<uuid>.jsonl`.
/// Conversations are named by uuid; anything else is not one.
fn is_conversation_file(path: &Path) -> Option<&str> {
    if path.extension().is_none_or(|e| e != "jsonl") {
        return None;
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
}

/// `scan_claude` over an explicit projects dir, process list and excluded
/// folders (tests).
pub fn scan_claude_in(
    projects: &Path,
    opts: &ScanOpts,
    tracked: &dyn Fn(&str) -> bool,
    procs: &str,
    excluded: &[PathBuf],
) -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(projects) else {
        return out;
    };
    for dir in dirs.flatten() {
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        let slug = dir.file_name().to_string_lossy().into_owned();
        for f in files.flatten() {
            let path = f.path();
            let Some(id) = is_conversation_file(&path) else {
                continue;
            };
            if tracked(id) {
                continue;
            }
            if !opts.only.is_empty() && !opts.only.iter().any(|o| id.starts_with(o.as_str())) {
                continue;
            }
            let mtime = f.metadata().and_then(|m| m.modified()).ok().map(ms_epoch);
            // Cheap pre-filter: a file untouched since the cutoff can't hold
            // a newer exchange.
            if opts.only.is_empty() && mtime.is_some_and(|m| m < opts.since_ms) {
                continue;
            }
            let Ok(c) = candidate(&path, id, &slug, mtime, procs, excluded) else {
                continue;
            };
            if opts.only.is_empty() && c.last_active_at < opts.since_ms {
                continue;
            }
            out.push(c);
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.last_active_at));
    if opts.limit > 0 {
        out.truncate(opts.limit);
    }
    out
}

fn candidate(
    path: &Path,
    id: &str,
    slug: &str,
    mtime: Option<i64>,
    procs: &str,
    excluded: &[PathBuf],
) -> Result<Candidate, String> {
    let (head, tail) = transcript::read_head_tail(path);
    let all = || head.iter().chain(tail.iter());

    // `claude --resume` finds a conversation by the folder it runs in, so the
    // cwd must be the one this transcript's directory is named after. Look
    // in the head and tail first; a long or worktree-hopping session may only
    // record it elsewhere, so then search the whole file.
    let cwd = match all()
        .filter_map(|v| v["cwd"].as_str())
        .find(|c| claude::project_slug(c) == slug)
    {
        Some(c) => c.to_string(),
        None => cwd_anywhere(path, slug).ok_or_else(|| {
            let seen: Vec<&str> = all().filter_map(|v| v["cwd"].as_str()).collect();
            format!(
                "no working directory in the transcript matches its folder {slug} \
                 (claude --resume couldn't find it); saw {:?}",
                seen.first()
            )
        })?,
    };
    // rlord's own conversations (the overlord, test sandboxes of it) and
    // anything in a temp dir are never the user's work.
    if cwd.ends_with("/robot-overlord/overlord") {
        return Err("rlord's own overlord conversation".into());
    }
    if let Some(d) = excluded.iter().find(|d| Path::new(&cwd).starts_with(d)) {
        return Err(format!(
            "runs in {} (rlord data or a temp dir)",
            d.display()
        ));
    }
    let entrypoint = all()
        .find_map(|v| v["entrypoint"].as_str())
        .map(str::to_string);
    // Headless runs (`claude -p`, the SDK) are tools, not conversations.
    if let Some(e) = entrypoint.as_deref().filter(|e| e.starts_with("sdk")) {
        return Err(format!(
            "a headless run (entrypoint {e}), not a conversation"
        ));
    }
    let mut ex = transcript::excerpts_from(Format::Claude, &head, &tail);
    // Something the user actually typed. The first prompt can sit past the
    // head window (big attachments) or follow a slash command, so the latest
    // one counts too; a session of only commands (/login, /exit) doesn't.
    let typed = |p: &Option<String>| p.as_deref().is_some_and(|t| !t.starts_with('/'));
    if !typed(&ex.first_prompt) && !typed(&ex.last_prompt) {
        return Err("nothing typed in it (an empty start, or only slash commands)".into());
    }
    if ex.first_prompt.is_none() {
        ex.first_prompt = ex.last_prompt.clone();
    }

    let ts = |v: &Value| v["timestamp"].as_str().and_then(transcript::parse_iso_ms);
    let exchange =
        |v: &&Value| (v["type"] == "user" || v["type"] == "assistant") && v["isMeta"] != true;
    let last_of = |kind: &str| {
        tail.iter()
            .rev()
            .filter(|v| v["type"] == kind && v["isMeta"] != true)
            .find_map(ts)
    };
    let last_active_at = tail
        .iter()
        .rev()
        .filter(exchange)
        .find_map(ts)
        .or(mtime)
        .ok_or("no timestamps in the transcript")?;
    let started_at = head.iter().find_map(ts);
    let branch = tail
        .iter()
        .rev()
        .chain(head.iter().rev())
        .filter_map(|v| v["gitBranch"].as_str())
        .find(|b| !b.is_empty() && *b != "HEAD")
        .map(str::to_string);
    let pr_url = tail
        .iter()
        .rev()
        .find_map(|v| v["prUrl"].as_str())
        .map(str::to_string);

    let dir_exists = Path::new(&cwd).is_dir();

    let recently_written =
        mtime.is_some_and(|m| ms_epoch(SystemTime::now()) - m < RECENT_WRITE.as_millis() as i64);
    Ok(Candidate {
        agent: "claude".into(),
        agent_session: id.to_string(),
        transcript: path.to_string_lossy().into_owned(),
        cwd,
        dir_exists,
        // Filled by `add_git_info`, only for the rows a caller keeps.
        repo: None,
        branch,
        worktree: false,
        recreatable: false,
        title: ex.title,
        first_prompt: ex.first_prompt,
        last_prompt: ex.last_prompt,
        last_reply: ex.last_reply,
        pr_url,
        entrypoint,
        started_at,
        last_active_at,
        last_user_at: last_of("user"),
        last_agent_at: last_of("assistant"),
        open_elsewhere: procs.lines().any(|l| l.contains(id)),
        maybe_held_by_codex_server: false,
        recently_written,
    })
}

/// The first `cwd` anywhere in the transcript whose slug is `slug` (only
/// when the head and tail windows had none). Bounded (the first
/// `CWD_SEARCH_MAX` bytes) and remembered per (file, size), so a transcript
/// without one isn't re-read on every scan.
fn cwd_anywhere(path: &Path, slug: &str) -> Option<String> {
    use std::io::{BufRead, Read};
    type Seen = std::collections::HashMap<(PathBuf, u64), Option<String>>;
    static SEEN: std::sync::LazyLock<std::sync::Mutex<Seen>> =
        std::sync::LazyLock::new(Default::default);
    let len = std::fs::metadata(path).ok()?.len();
    let key = (path.to_path_buf(), len);
    if let Some(hit) = SEEN.lock().ok()?.get(&key) {
        return hit.clone();
    }
    let f = std::fs::File::open(path).ok()?;
    let found = std::io::BufReader::new(f.take(CWD_SEARCH_MAX))
        .lines()
        .map_while(Result::ok)
        .filter(|l| l.contains("\"cwd\""))
        .find_map(|l| {
            serde_json::from_str::<Value>(&l)
                .ok()
                .and_then(|v| v["cwd"].as_str().map(str::to_string))
                .filter(|c| claude::project_slug(c) == slug)
        });
    if let Ok(mut seen) = SEEN.lock() {
        if seen.len() > 4096 {
            seen.clear();
        }
        seen.insert(key, found.clone());
    }
    found
}

const CWD_SEARCH_MAX: u64 = 64 * 1024 * 1024;

/// Why conversation `id` isn't importable (None: it is, or no transcript
/// by that id exists). For error messages when an import by id fails.
pub fn explain_claude(id: &str, tracked: &dyn Fn(&str) -> bool) -> Why {
    let projects = claude::config_dir().join("projects");
    // Every transcript whose id starts with `id` (a full id or a prefix).
    let mut hits = Vec::new();
    let Ok(dirs) = std::fs::read_dir(&projects) else {
        return Why::NotFound;
    };
    for dir in dirs.flatten() {
        for f in std::fs::read_dir(dir.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = f.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".jsonl").filter(|n| n.starts_with(id)) {
                hits.push((f.path(), stem.to_string(), dir.file_name()));
            }
        }
    }
    match hits.as_slice() {
        [] => Why::NotFound,
        [(_, full, _)] if tracked(full) => Why::Tracked,
        [(path, full, slug)] => {
            let slug = slug.to_string_lossy();
            match candidate(path, full, &slug, None, "", &excluded_dirs()) {
                Ok(_) => Why::Importable,
                Err(e) => Why::Rejected(e),
            }
        }
        _ => Why::Ambiguous(hits.len()),
    }
}

/// What one agent says about an id asked for by `explain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    NotFound,
    /// rlord tracks it already.
    Tracked,
    /// This many conversations start with the id.
    Ambiguous(usize),
    Rejected(String),
    Importable,
}

const TRACKED: &str = "rlord already tracks it (now or as an earlier conversation of a session)";

impl Why {
    /// Why it can't be imported (None: not found, or importable).
    pub fn reason(&self) -> Option<String> {
        match self {
            Why::NotFound | Why::Importable => None,
            Why::Tracked => Some(TRACKED.into()),
            Why::Ambiguous(_) => Some("several conversations match; use more of the id".into()),
            Why::Rejected(e) => Some(e.clone()),
        }
    }
}

// ---- Codex ----

/// Scan Codex's rollouts, as `scan_claude` scans Claude's transcripts.
pub fn scan_codex(opts: &ScanOpts, tracked: &dyn Fn(&str) -> bool) -> Vec<Candidate> {
    let procs = process_commands();
    scan_codex_in(&codex::home(), opts, tracked, &procs, &excluded_dirs())
}

/// Whether any Claude Code or Codex conversation exists that the scan
/// would list (the first-run offer). Bounded per agent.
pub fn has_conversations(since_ms: i64, tracked: &dyn Fn(&str) -> bool) -> bool {
    has_claude_conversations(since_ms, tracked)
        || has_codex_conversations_in(&codex::home(), since_ms, tracked, &excluded_dirs())
}

/// Whether any Codex conversation exists that `scan_codex` would list, over
/// an explicit Codex home: newest rollouts first, at most `MAX_READS` read
/// (the budget spent on rejects answers true, as Claude's does).
pub fn has_codex_conversations_in(
    home: &Path,
    since_ms: i64,
    tracked: &dyn Fn(&str) -> bool,
    excluded: &[PathBuf],
) -> bool {
    let index = codex_index(home);
    let mut files = codex::rollouts_in(&home.join("sessions"));
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut reads = 0;
    for (mtime, path) in files {
        let mtime = ms_epoch(mtime);
        if mtime < since_ms {
            // Newest first: the rest are older still.
            return false;
        }
        let Some(id) = codex_rollout_id(&path) else {
            continue;
        };
        // The index's own rejects cost no read.
        if tracked(id) || index.get(id).and_then(index_reject).is_some() {
            continue;
        }
        if reads >= MAX_READS {
            return true;
        }
        reads += 1;
        if codex_candidate(&path, id, Some(mtime), "", excluded, index.get(id))
            .is_ok_and(|c| c.last_active_at >= since_ms)
        {
            return true;
        }
    }
    false
}

/// The thread id when `path` is a Codex rollout: `rollout-<time>-<uuid>.jsonl`.
fn codex_rollout_id(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    uuid::Uuid::parse_str(id).is_ok().then_some(id)
}

/// What Codex's thread index says about a thread (columns an older index
/// lacks are None).
#[derive(Debug, Clone, Default)]
struct IndexRow {
    cwd: Option<String>,
    /// Where it was started from: `cli`, `vscode`, `exec`…
    source: Option<String>,
    /// `user`, or a sub-agent's.
    thread_source: Option<String>,
}

/// Every thread in Codex's index (one read-only query; empty without an
/// index or with a schema rlord doesn't know).
fn codex_index(home: &Path) -> std::collections::HashMap<String, IndexRow> {
    use rusqlite::OpenFlags;
    let mut out = std::collections::HashMap::new();
    let db = home.join("state_5.sqlite");
    if !db.exists() {
        return out;
    }
    let Ok(c) = rusqlite::Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return out;
    };
    // Codex may be writing: wait a little, never long.
    let _ = c.busy_timeout(Duration::from_millis(200));
    // An older index has no source columns: then only folders.
    let (mut q, full) = match c.prepare("SELECT id, cwd, source, thread_source FROM threads") {
        Ok(q) => (q, true),
        Err(_) => match c.prepare("SELECT id, cwd FROM threads") {
            Ok(q) => (q, false),
            Err(_) => return out,
        },
    };
    let rows = q.query_map([], |r| {
        let opt = |i| r.get::<_, Option<String>>(i);
        Ok((
            r.get::<_, String>(0)?,
            IndexRow {
                cwd: opt(1)?,
                source: if full { opt(2)? } else { None },
                thread_source: if full { opt(3)? } else { None },
            },
        ))
    });
    if let Ok(rows) = rows {
        out.extend(rows.flatten());
    }
    out
}

/// `scan_codex` over an explicit Codex home, process list and excluded
/// folders (tests).
pub fn scan_codex_in(
    home: &Path,
    opts: &ScanOpts,
    tracked: &dyn Fn(&str) -> bool,
    procs: &str,
    excluded: &[PathBuf],
) -> Vec<Candidate> {
    let index = codex_index(home);
    let mut out = Vec::new();
    for (mtime, path) in codex::rollouts_in(&home.join("sessions")) {
        let Some(id) = codex_rollout_id(&path) else {
            continue;
        };
        if tracked(id) {
            continue;
        }
        if !opts.only.is_empty() && !opts.only.iter().any(|o| id.starts_with(o.as_str())) {
            continue;
        }
        let mtime = ms_epoch(mtime);
        // Cheap pre-filter, as for Claude.
        if opts.only.is_empty() && mtime < opts.since_ms {
            continue;
        }
        if index.get(id).and_then(index_reject).is_some() {
            continue;
        }
        let Ok(c) = codex_candidate(&path, id, Some(mtime), procs, excluded, index.get(id)) else {
            continue;
        };
        if opts.only.is_empty() && c.last_active_at < opts.since_ms {
            continue;
        }
        if maybe_rlords_own(&c, &opts.unbound_codex) {
            continue;
        }
        out.push(c);
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.last_active_at));
    if opts.limit > 0 {
        out.truncate(opts.limit);
    }
    out
}

/// A field of the rollout's `session_meta` line, read from its first bytes
/// as `codex::rollout_cwd` reads `cwd` (the line can be megabytes of
/// instructions; its own fields come first). An object (`"source":
/// {"subagent": …}`) reads as its first key.
fn meta_field(meta: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":");
    let at = meta.find(&pat)? + pat.len();
    let v = serde_json::Deserializer::from_str(&meta[at..])
        .into_iter::<Value>()
        .next()?
        .ok()?;
    match v {
        Value::String(s) => Some(s),
        Value::Object(m) => m.keys().next().cloned(),
        _ => None,
    }
}

/// How much of a rollout's first line `codex_candidate` reads for its meta
/// when the line can't be parsed whole.
const META_BYTES: u64 = 16 * 1024;

/// A rollout's `session_meta` payload: parsed whole when possible, else
/// the first `META_BYTES` of its line, searched by key.
enum SessionMeta {
    Parsed(Value),
    Prefix(String),
}

impl SessionMeta {
    fn str(&self, key: &str) -> Option<String> {
        match self {
            SessionMeta::Parsed(p) => match &p[key] {
                Value::String(s) => Some(s.clone()),
                // An object source (`{"subagent": …}`): its first key.
                Value::Object(m) => m.keys().next().cloned(),
                _ => None,
            },
            SessionMeta::Prefix(t) => meta_field(t, key),
        }
    }

    fn branch(&self) -> Option<String> {
        match self {
            SessionMeta::Parsed(p) => p["git"]["branch"].as_str().map(str::to_string),
            SessionMeta::Prefix(t) => meta_field(&t[t.find("\"git\":")?..], "branch"),
        }
    }
}

/// The meta from the parsed head window; a first line too long for that
/// window is read whole (up to `codex::FIRST_LINE_MAX`, as
/// `rollout_first_line` does), and only past that by its prefix. So a big
/// `AGENTS.md` before `source` doesn't hide it.
fn session_meta(path: &Path, head: &[Value]) -> Result<SessionMeta, String> {
    use std::io::{BufRead, Read};
    if let Some(v) = head.first().filter(|v| v["type"] == "session_meta") {
        return Ok(SessionMeta::Parsed(v["payload"].clone()));
    }
    let f = std::fs::File::open(path).map_err(|e| format!("can't read the rollout: {e}"))?;
    let mut line = Vec::new();
    let _ = std::io::BufReader::new(f.take(codex::FIRST_LINE_MAX)).read_until(b'\n', &mut line);
    if let Ok(v) = serde_json::from_slice::<Value>(&line) {
        if v["type"] == "session_meta" {
            return Ok(SessionMeta::Parsed(v["payload"].clone()));
        }
    }
    line.truncate(META_BYTES as usize);
    Ok(SessionMeta::Prefix(
        String::from_utf8_lossy(&line).into_owned(),
    ))
}

/// Maybe the thread of one of rlord's own Codex sessions that hasn't bound
/// its id yet: same folder, started after that session was created.
fn maybe_rlords_own(c: &Candidate, unbound: &[(String, i64)]) -> bool {
    let started = c.started_at.unwrap_or(c.last_active_at);
    unbound
        .iter()
        .any(|(dir, created)| *dir == c.cwd && started >= *created)
}

fn is_headless_source(s: &str) -> bool {
    matches!(s, "exec" | "mcp")
}

/// Why the index alone rules a thread out (no rollout read needed).
fn index_reject(i: &IndexRow) -> Option<String> {
    if let Some(s) = i.source.as_deref().filter(|s| is_headless_source(s)) {
        return Some(format!("a headless run (source {s}), not a conversation"));
    }
    let sub = i.thread_source.as_deref().is_some_and(|t| t != "user")
        || i.source.as_deref().is_some_and(is_subagent_source);
    sub.then(|| "a sub-agent's thread, not the user's conversation".to_string())
}

/// A sub-agent's source: `subagent`, or (the index keeps an object source
/// as JSON text) an object with a `subagent` key.
fn is_subagent_source(s: &str) -> bool {
    s == "subagent" || serde_json::from_str::<Value>(s).is_ok_and(|v| v.get("subagent").is_some())
}

/// A Codex app-server (its background server, or one the Codex app runs)
/// is running: it may hold any thread an app-server client started.
fn codex_server_running(procs: &str) -> bool {
    procs
        .lines()
        .any(|l| l.contains("codex") && l.contains("app-server"))
}

fn codex_candidate(
    path: &Path,
    id: &str,
    mtime: Option<i64>,
    procs: &str,
    excluded: &[PathBuf],
    indexed: Option<&IndexRow>,
) -> Result<Candidate, String> {
    if let Some(why) = indexed.and_then(index_reject) {
        return Err(why);
    }
    let (head, tail) = transcript::read_head_tail(path);
    let meta = session_meta(path, &head)?;
    let originator = meta.str("originator");
    // Headless runs (`codex exec`, the SDK, an MCP server) and sub-agents'
    // threads aren't the user's conversations.
    // Both the index's and the rollout's own source count.
    let sources: Vec<String> = indexed
        .and_then(|i| i.source.clone())
        .into_iter()
        .chain(meta.str("source"))
        .collect();
    if let Some(s) = sources.iter().find(|s| is_headless_source(s)) {
        return Err(format!("a headless run (source {s}), not a conversation"));
    }
    let source = sources.first().cloned();
    if let Some(o) = originator.as_deref().filter(|o| o.contains("exec")) {
        return Err(format!(
            "a headless run (originator {o}), not a conversation"
        ));
    }
    let sub = indexed
        .and_then(|i| i.thread_source.as_deref())
        .is_some_and(|t| t != "user")
        || source.as_deref().is_some_and(is_subagent_source)
        || meta.str("source").is_some_and(|s| is_subagent_source(&s));
    if sub {
        return Err("a sub-agent's thread, not the user's conversation".into());
    }

    // Where it last ran: the latest turn's folder, else the index's, else
    // where it started.
    let cwd = tail
        .iter()
        .rev()
        .filter(|v| v["type"] == "turn_context")
        .find_map(|v| v["payload"]["cwd"].as_str().map(str::to_string))
        .or_else(|| indexed.and_then(|i| i.cwd.clone()))
        .or_else(|| meta.str("cwd"))
        .ok_or("no working directory in the rollout")?;
    if cwd.ends_with("/robot-overlord/overlord") {
        return Err("rlord's own overlord conversation".into());
    }
    if let Some(d) = excluded.iter().find(|d| Path::new(&cwd).starts_with(d)) {
        return Err(format!(
            "runs in {} (rlord data or a temp dir)",
            d.display()
        ));
    }
    let mut ex = transcript::excerpts_from(Format::Codex, &head, &tail);
    let typed = |p: &Option<String>| p.as_deref().is_some_and(|t| !t.starts_with('/'));
    if !typed(&ex.first_prompt) && !typed(&ex.last_prompt) {
        return Err("nothing typed in it (an empty start, or only slash commands)".into());
    }
    if ex.first_prompt.is_none() {
        ex.first_prompt = ex.last_prompt.clone();
    }
    let ts = |v: &Value| v["timestamp"].as_str().and_then(transcript::parse_iso_ms);
    let who = |v: &Value| Format::Codex.line(v).message().map(|(w, _)| w);
    let last_of = |kind: &str| {
        tail.iter()
            .rev()
            .filter(|v| who(v) == Some(kind))
            .find_map(ts)
    };
    let last_active_at = tail
        .iter()
        .rev()
        .filter(|v| matches!(who(v), Some("you" | "agent")))
        .find_map(ts)
        .or(mtime)
        .ok_or("no timestamps in the rollout")?;
    let started_at = head.iter().find_map(ts);
    // Codex records the branch only when the thread starts
    // (`session_meta.git`), in the folder it started in. A thread that moved
    // since: that branch says nothing about where it is now, so it isn't
    // offered for recreating a worktree. (A folder that exists gets its
    // current branch from git: `add_git_info`.)
    let moved = meta.str("cwd").is_some_and(|start| start != cwd);
    let branch = meta
        .branch()
        .filter(|b| !b.is_empty() && b != "HEAD" && !moved);
    let dir_exists = Path::new(&cwd).is_dir();
    let recently_written =
        mtime.is_some_and(|m| ms_epoch(SystemTime::now()) - m < RECENT_WRITE.as_millis() as i64);
    Ok(Candidate {
        agent: "codex".into(),
        agent_session: id.to_string(),
        transcript: path.to_string_lossy().into_owned(),
        cwd,
        dir_exists,
        repo: None,
        branch,
        worktree: false,
        recreatable: false,
        title: ex.title,
        first_prompt: ex.first_prompt,
        last_prompt: ex.last_prompt,
        last_reply: ex.last_reply,
        pr_url: None,
        entrypoint: originator,
        started_at,
        last_active_at,
        last_user_at: last_of("you"),
        last_agent_at: last_of("agent"),
        open_elsewhere: procs.lines().any(|l| l.contains(id)),
        // A thread an app-server client started (`source` vscode: an
        // editor, the Codex app, or Codex's background server) and written
        // lately may still be held there; resuming it in the pane
        // (`--no-daemon`) would run it twice.
        maybe_held_by_codex_server: sources.iter().any(|s| s == "vscode")
            && (recently_written || codex_server_running(procs)),
        recently_written,
    })
}

/// Why Codex thread `id` isn't importable (None: it is, or no rollout by
/// that id exists).
pub fn explain_codex(id: &str, tracked: &dyn Fn(&str) -> bool) -> Why {
    explain_codex_in(&codex::home(), id, tracked)
}

fn explain_codex_in(home: &Path, id: &str, tracked: &dyn Fn(&str) -> bool) -> Why {
    let hits: Vec<(PathBuf, String)> = codex::rollouts_in(&home.join("sessions"))
        .into_iter()
        .filter_map(|(_, p)| {
            let full = codex_rollout_id(&p)?.to_string();
            full.starts_with(id).then_some((p, full))
        })
        .collect();
    match hits.as_slice() {
        [] => Why::NotFound,
        [(_, full)] if tracked(full) => Why::Tracked,
        [(path, full)] => {
            let index = codex_index(home);
            match codex_candidate(path, full, None, "", &excluded_dirs(), index.get(full)) {
                Ok(_) => Why::Importable,
                Err(e) => Why::Rejected(e),
            }
        }
        _ => Why::Ambiguous(hits.len()),
    }
}

/// Every importable agent's conversations (Claude Code, Codex), most
/// recently active first, at most `opts.limit` overall.
pub fn scan_all(opts: &ScanOpts, tracked: &dyn Fn(&str) -> bool) -> Vec<Candidate> {
    let procs = process_commands();
    let ex = excluded_dirs();
    let projects = claude::config_dir().join("projects");
    let mut out = scan_claude_in(&projects, opts, tracked, &procs, &ex);
    out.extend(scan_codex_in(&codex::home(), opts, tracked, &procs, &ex));
    out.sort_by_key(|c| std::cmp::Reverse(c.last_active_at));
    if opts.limit > 0 {
        out.truncate(opts.limit);
    }
    out
}

/// Why conversation `id` (of any importable agent) isn't importable.
/// Starts with `TRACKED` when rlord tracks it (the CLI looks for that).
pub fn explain(id: &str, tracked: &dyn Fn(&str) -> bool) -> String {
    merge_why(id, explain_claude(id, tracked), explain_codex(id, tracked))
}

fn merge_why(id: &str, claude: Why, codex: Why) -> String {
    use Why::*;
    let count = |w: &Why| match w {
        NotFound => 0,
        Ambiguous(n) => *n,
        _ => 1,
    };
    match (claude, codex) {
        (NotFound, NotFound) => {
            format!("no Claude Code or Codex conversation's id starts with {id}")
        }
        (c, x) if count(&c) + count(&x) > 1 => format!(
            "{} conversations start with {id}; use more of the id",
            count(&c) + count(&x)
        ),
        (w, NotFound) | (NotFound, w) => w.reason().unwrap_or_else(|| "unknown".into()),
        // (Unreachable: one hit each counts two.)
        (c, _) => c.reason().unwrap_or_else(|| "unknown".into()),
    }
}

/// Fill in `repo`, `worktree` and `recreatable` (git subprocesses, so only
/// for the candidates a caller keeps, not every transcript scanned).
pub fn add_git_info(c: &mut Candidate) {
    if c.dir_exists {
        c.repo = git::main_repo_root(&c.cwd);
        // Codex records the branch only at the start: the checkout says
        // what it is now.
        if c.agent == "codex" {
            // (None: detached, so no branch.)
            c.branch = git::checked_out_branch(&c.cwd);
        }
        // A worktree has its own top level; a subfolder of the main checkout
        // shares the main one.
        let top = git::repo_root(&c.cwd)
            .and_then(|t| std::fs::canonicalize(t).ok())
            .map(|t| t.to_string_lossy().into_owned());
        c.worktree = matches!((&c.repo, &top), (Some(r), Some(t)) if r != t);
        c.recreatable = false;
    } else {
        // The folder is gone (a removed worktree?). If an ancestor is still
        // in a repo that has the branch, and no checkout has it (a subfolder
        // of the main checkout would: its branch is checked out there), the
        // worktree can come back where it was.
        c.repo = nearest_existing(Path::new(&c.cwd))
            .and_then(|p| git::main_repo_root(&p.to_string_lossy()));
        c.recreatable = match (&c.repo, &c.branch) {
            (Some(r), Some(b)) => git::branch_exists(r, b) && !git::branch_checked_out(r, b),
            _ => false,
        };
        c.worktree = c.recreatable;
    }
}

/// rlord's data dir (the overlord's folder) and the temp dirs.
pub fn excluded_dirs() -> Vec<PathBuf> {
    vec![
        crate::paths::data_dir(),
        std::env::temp_dir(),
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/private/var/folders"),
        PathBuf::from("/var/folders"),
    ]
}

fn nearest_existing(p: &Path) -> Option<PathBuf> {
    p.ancestors()
        .skip(1)
        .find(|a| a.is_dir() && *a != Path::new("/"))
        .map(Path::to_path_buf)
}

/// Every running process's command line, one per line ("" if `ps` fails),
/// leaving out rlord's own: `rlord import <id>` names the id it imports.
fn process_commands() -> String {
    let out = std::process::Command::new("ps")
        .args(["-axo", "command="])
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    out.lines()
        .filter(|l| {
            let argv0 = l.split_whitespace().next().unwrap_or("");
            Path::new(argv0).file_name().is_none_or(|n| n != "rlord")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

use crate::timefmt::ms_epoch;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID_A: &str = "11111111-1111-4111-8111-111111111111";
    const ID_B: &str = "22222222-2222-4222-8222-222222222222";

    fn write(projects: &Path, cwd: &str, id: &str, lines: &[Value]) -> PathBuf {
        let dir = projects.join(claude::project_slug(cwd));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{id}.jsonl"));
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(&p, text.join("\n") + "\n").unwrap();
        p
    }

    fn convo(cwd: &str, prompt: &str, last: &str) -> Vec<Value> {
        vec![
            json!({"type": "user", "cwd": cwd, "entrypoint": "cli", "gitBranch": "main",
                   "timestamp": "2026-11-01T10:00:00.000Z",
                   "message": {"role": "user", "content": prompt}}),
            json!({"type": "assistant", "cwd": cwd, "gitBranch": "feat/x",
                   "timestamp": last,
                   "message": {"content": [{"type": "text", "text": "done"}]}}),
            // Bookkeeping after the last exchange must not count as activity.
            json!({"type": "ai-title", "aiTitle": "Fix the login test",
                   "timestamp": "2026-11-09T00:00:00.000Z"}),
            json!({"type": "pr-link", "prUrl": "https://example.test/pr/7"}),
        ]
    }

    #[test]
    fn has_conversations_skips_excluded_and_non_ids() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        let excl = [PathBuf::from("/rlord/data")];
        let none = |_: &str| false;
        let has = |since, tracked: &dyn Fn(&str) -> bool| {
            has_claude_conversations_in(&projects, since, tracked, &excl)
        };
        assert!(!has(0, &none));
        let last = "2026-11-05T12:00:00.000Z";
        write(
            &projects,
            "/src/app",
            "notes",
            &convo("/src/app", "hi", last),
        );
        // Excluded by its cwd, as the scan does.
        let own = "/rlord/data/x";
        write(&projects, own, ID_A, &convo(own, "hi", last));
        assert!(!has(0, &none));
        write(&projects, "/src/app", ID_B, &convo("/src/app", "hi", last));
        assert!(has(0, &none));
        assert!(!has(0, &|id| id == ID_B));
        // Older than the window (by last exchange): not counted.
        let after = ms_epoch(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000));
        assert!(!has(after, &none));
    }

    #[test]
    fn claude_project_folders_come_from_matching_cwds() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        write(
            &projects,
            "/src/app",
            ID_A,
            &convo("/src/app", "hi", "2026-11-01T10:01:00.000Z"),
        );
        // A transcript whose cwd doesn't match its folder's slug: skipped.
        let dir_b = projects.join(claude::project_slug("/src/web"));
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(
            dir_b.join(format!("{ID_B}.jsonl")),
            json!({"type": "user", "cwd": "/elsewhere"}).to_string() + "\n",
        )
        .unwrap();
        assert_eq!(claude_project_folders_in(&projects, 300), ["/src/app"]);
        assert!(claude_project_folders_in(&dir.path().join("none"), 300).is_empty());
    }

    #[test]
    fn scans_by_last_exchange_and_skips_tracked_and_tooling() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let cwd = work.to_str().unwrap();

        write(
            &projects,
            cwd,
            ID_A,
            &convo(cwd, "fix the login test", "2026-11-05T12:00:00.000Z"),
        );
        write(
            &projects,
            cwd,
            ID_B,
            &convo(cwd, "older work", "2026-10-01T12:00:00.000Z"),
        );
        // Not conversations: a non-uuid file, a headless run, no typed prompt.
        write(
            &projects,
            cwd,
            "agent-abc",
            &convo(cwd, "x", "2026-11-05T12:00:00.000Z"),
        );
        let mut sdk = convo(cwd, "x", "2026-11-05T12:00:00.000Z");
        sdk[0]["entrypoint"] = json!("sdk-cli");
        write(&projects, cwd, "33333333-3333-4333-8333-333333333333", &sdk);
        let mut empty = convo(
            cwd,
            "<command-name>/clear</command-name>",
            "2026-11-05T12:00:00.000Z",
        );
        empty[0]["isMeta"] = json!(true);
        write(
            &projects,
            cwd,
            "44444444-4444-4444-8444-444444444444",
            &empty,
        );

        let since = transcript::parse_iso_ms("2026-11-01T00:00:00.000Z").unwrap();
        let opts = ScanOpts {
            since_ms: since,
            ..Default::default()
        };
        let procs = format!("claude --resume {ID_A}\n");
        let found = scan_claude_in(&projects, &opts, &|_| false, &procs, &[]);
        let ids: Vec<&str> = found.iter().map(|c| c.agent_session.as_str()).collect();
        // B's file is new (mtime now) but its last exchange is before `since`.
        assert_eq!(ids, [ID_A]);
        let c = &found[0];
        assert_eq!(c.cwd, cwd);
        assert_eq!(c.title.as_deref(), Some("Fix the login test"));
        assert_eq!(c.first_prompt.as_deref(), Some("fix the login test"));
        assert_eq!(c.branch.as_deref(), Some("feat/x"));
        assert_eq!(c.pr_url.as_deref(), Some("https://example.test/pr/7"));
        assert_eq!(
            c.last_active_at,
            transcript::parse_iso_ms("2026-11-05T12:00:00.000Z").unwrap()
        );
        assert!(c.open_elsewhere && c.dir_exists && c.repo.is_none());

        // Tracked conversations are skipped; `only` ignores the time window.
        assert!(scan_claude_in(&projects, &opts, &|id| id == ID_A, "", &[]).is_empty());
        // A prefix works too (`rlord import 22222222`).
        let only = ScanOpts {
            only: vec![ID_B[..8].into()],
            ..Default::default()
        };
        let found = scan_claude_in(&projects, &only, &|_| false, "", &[]);
        assert_eq!(found.len(), 1);
        assert!(!found[0].open_elsewhere);
    }

    #[test]
    fn cwd_must_match_the_transcript_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let cwd = tmp.path().join("work");
        let cwd = cwd.to_str().unwrap();
        // Filed under a folder whose slug matches no cwd in the file: it
        // couldn't be resumed from anywhere, so it isn't offered.
        let lines = convo("/somewhere/else", "hi", "2026-11-05T12:00:00.000Z");
        let p = write(&projects, cwd, ID_A, &lines);
        assert!(p.exists());
        let found = scan_claude_in(&projects, &ScanOpts::default(), &|_| false, "", &[]);
        assert!(found.is_empty());
    }

    #[test]
    fn subfolder_of_main_checkout_is_not_a_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(tmp.path()).unwrap().join("repo");
        let sub = repo.join("crates");
        std::fs::create_dir_all(&sub).unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(["-C", repo.to_str().unwrap()])
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ]);
        git(&["worktree", "add", "-q", "-b", "feat", ".worktrees/feat"]);
        let wt = repo.join(".worktrees/feat");

        let mut c = Candidate {
            agent: "claude".into(),
            agent_session: ID_A.into(),
            transcript: String::new(),
            cwd: sub.to_string_lossy().into_owned(),
            dir_exists: true,
            repo: None,
            branch: Some("main".into()),
            worktree: false,
            recreatable: false,
            title: None,
            first_prompt: None,
            last_prompt: None,
            last_reply: None,
            pr_url: None,
            entrypoint: None,
            started_at: None,
            last_active_at: 0,
            last_user_at: None,
            last_agent_at: None,
            open_elsewhere: false,
            maybe_held_by_codex_server: false,
            recently_written: false,
        };
        add_git_info(&mut c);
        assert_eq!(c.repo.as_deref(), repo.to_str());
        assert!(!c.worktree, "a subfolder of the main checkout");

        c.cwd = wt.to_string_lossy().into_owned();
        add_git_info(&mut c);
        assert!(c.worktree);

        // Gone: a deleted subfolder's branch (main) is checked out, so it
        // can't come back as a worktree; a removed worktree's branch can.
        c.cwd = sub.join("gone").to_string_lossy().into_owned();
        c.dir_exists = false;
        add_git_info(&mut c);
        assert!(!c.recreatable);
        git(&["worktree", "remove", ".worktrees/feat"]);
        c.cwd = wt.to_string_lossy().into_owned();
        c.branch = Some("feat".into());
        add_git_info(&mut c);
        assert!(c.recreatable && c.worktree);
    }

    #[test]
    fn odd_but_real_conversations_are_importable_and_rejections_say_why() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let cwd = work.to_str().unwrap();
        let ts = "2026-11-05T12:00:00.000Z";

        // Starts with a slash command and huge attachments: the typed prompt
        // is past the head window. Still a conversation.
        let big = "x".repeat(300 * 1024);
        let mut lines = vec![
            json!({"type": "user", "cwd": cwd, "timestamp": ts,
                   "message": {"content": "<command-name>/init</command-name>"}}),
            json!({"type": "attachment", "content": big}),
        ];
        lines.extend(convo(cwd, "now fix the build", ts));
        write(&projects, cwd, ID_A, &lines);
        let found = scan_claude_in(&projects, &ScanOpts::default(), &|_| false, "", &[]);
        assert_eq!(found.len(), 1, "{found:?}");

        // Filed under a folder no cwd in it matches: rejected, with a reason.
        let p = write(&projects, "/elsewhere", ID_B, &convo(cwd, "hi", ts));
        let slug = claude::project_slug("/elsewhere");
        let why = candidate(&p, ID_B, &slug, None, "", &[]).unwrap_err();
        assert!(why.contains("matches its folder"), "{why}");
    }

    // ---- Codex ----

    const CX_A: &str = "01a1ce53-c71d-7e13-b376-63f7271fa320";
    const CX_B: &str = "02b2ce53-c71d-7e13-b376-63f7271fa321";
    const CX_C: &str = "03c3ce53-c71d-7e13-b376-63f7271fa322";
    const CX_D: &str = "04d4ce53-c71d-7e13-b376-63f7271fa323";

    fn rollout(home: &Path, id: &str, meta: Value, lines: &[Value]) -> PathBuf {
        let day = home.join("sessions/2026/11/11");
        std::fs::create_dir_all(&day).unwrap();
        let p = day.join(format!("rollout-2026-11-11T21-55-59-{id}.jsonl"));
        let mut all = vec![json!({"timestamp": "2026-11-11T21:55:59.000Z",
                                  "type": "session_meta", "payload": meta})];
        all.extend_from_slice(lines);
        let text: Vec<String> = all.iter().map(Value::to_string).collect();
        std::fs::write(&p, text.join("\n") + "\n").unwrap();
        p
    }

    fn codex_turn(cwd: &str, prompt: &str, reply: &str) -> Vec<Value> {
        let item = |at: &str, ty: &str, payload: Value| json!({"timestamp": format!("2026-11-11T22:00:{at}.000Z"), "type": ty, "payload": payload});
        let msg = |role: &str, part: &str, text: &str| json!({"type": "message", "role": role, "content": [{"type": part, "text": text}]});
        vec![
            item("01", "turn_context", json!({"cwd": cwd, "model": "gpt-x"})),
            item("02", "response_item", msg("user", "input_text", prompt)),
            item(
                "05",
                "response_item",
                msg("assistant", "output_text", reply),
            ),
        ]
    }

    #[test]
    fn codex_conversations_are_found_with_their_folder_and_branch() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cwd = work.path().to_string_lossy().into_owned();
        // A conversation.
        rollout(
            home.path(),
            CX_A,
            json!({"id": CX_A, "cwd": cwd, "originator": "codex-tui",
                   "source": "cli", "git": {"branch": "fix-login"}}),
            &codex_turn(&cwd, "fix the flaky login test", "Fixed."),
        );
        // A headless run, a sub-agent's thread, nothing typed.
        rollout(
            home.path(),
            CX_B,
            json!({"id": CX_B, "cwd": cwd, "originator": "codex_exec", "source": "exec"}),
            &codex_turn(&cwd, "summarize", "ok"),
        );
        rollout(
            home.path(),
            CX_C,
            json!({"id": CX_C, "cwd": cwd, "source": {"subagent": {"other": "x"}}}),
            &codex_turn(&cwd, "look this up", "ok"),
        );
        rollout(
            home.path(),
            CX_D,
            json!({"id": CX_D, "cwd": "/src/app"}),
            &[],
        );
        let opts = ScanOpts::default();
        let found = scan_codex_in(home.path(), &opts, &|_| false, "", &[]);
        assert_eq!(found.len(), 1, "{found:?}");
        let c = &found[0];
        assert_eq!(
            (c.agent.as_str(), c.agent_session.as_str()),
            ("codex", CX_A)
        );
        assert_eq!(c.cwd, cwd);
        assert!(c.dir_exists);
        assert_eq!(c.branch.as_deref(), Some("fix-login"));
        assert_eq!(c.entrypoint.as_deref(), Some("codex-tui"));
        assert_eq!(c.first_prompt.as_deref(), Some("fix the flaky login test"));
        assert_eq!(c.last_reply.as_deref(), Some("Fixed."));
        assert_eq!(
            c.last_active_at,
            transcript::parse_iso_ms("2026-11-11T22:00:05.000Z").unwrap()
        );
        assert!(c.last_user_at < c.last_agent_at);

        // Tracked, out of the window, excluded folders, open elsewhere.
        assert!(scan_codex_in(home.path(), &opts, &|id| id == CX_A, "", &[]).is_empty());
        let late = ScanOpts {
            since_ms: c.last_active_at + 1,
            ..Default::default()
        };
        assert!(scan_codex_in(home.path(), &late, &|_| false, "", &[]).is_empty());
        let ex = [work.path().to_path_buf()];
        assert!(scan_codex_in(home.path(), &opts, &|_| false, "", &ex).is_empty());
        let procs = format!("codex resume {CX_A}");
        assert!(scan_codex_in(home.path(), &opts, &|_| false, &procs, &[])[0].open_elsewhere);

        // By id prefix, any age; the rejects say why.
        let only = ScanOpts {
            only: vec!["01a1".into()],
            since_ms: i64::MAX,
            ..Default::default()
        };
        assert_eq!(
            scan_codex_in(home.path(), &only, &|_| false, "", &[]).len(),
            1
        );
        let why = explain_codex_in(home.path(), CX_B, &|_| false)
            .reason()
            .unwrap();
        assert!(why.contains("headless"), "{why}");
        let why = explain_codex_in(home.path(), CX_C, &|_| false)
            .reason()
            .unwrap();
        assert!(why.contains("sub-agent"), "{why}");
        let why = explain_codex_in(home.path(), CX_D, &|_| false)
            .reason()
            .unwrap();
        assert!(why.contains("nothing typed"), "{why}");
        assert_eq!(
            explain_codex_in(home.path(), "ffff", &|_| false),
            Why::NotFound
        );

        // The first-run check sees the same.
        assert!(has_codex_conversations_in(home.path(), 0, &|_| false, &[]));
        assert!(!has_codex_conversations_in(
            home.path(),
            0,
            &|id| id == CX_A,
            &[]
        ));
    }

    #[test]
    fn codex_index_marks_sub_agents_and_headless_runs() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let cwd = work.path().to_string_lossy().into_owned();
        // The rollouts say nothing; the index does.
        for id in [CX_A, CX_B, CX_C, CX_D] {
            rollout(
                home.path(),
                id,
                json!({"id": id, "cwd": cwd}),
                &codex_turn(&cwd, "hello there", "hi"),
            );
        }
        let c = rusqlite::Connection::open(home.path().join("state_5.sqlite")).unwrap();
        c.execute_batch(&format!(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, \
               cwd TEXT NOT NULL, created_at_ms INTEGER, thread_source TEXT, \
               source TEXT NOT NULL DEFAULT 'cli');
             INSERT INTO threads VALUES ('{CX_A}', '', '{cwd}', 1, 'user', 'cli');
             INSERT INTO threads VALUES ('{CX_B}', '', '{cwd}', 1, 'subagent', 'cli');
             INSERT INTO threads VALUES ('{CX_C}', '', '{cwd}', 1, 'user', 'exec');
             INSERT INTO threads VALUES ('{CX_D}', '', '{cwd}', 1, 'user', 'vscode');"
        ))
        .unwrap();
        drop(c);
        let found = scan_codex_in(home.path(), &ScanOpts::default(), &|_| false, "", &[]);
        let ids: Vec<(&str, bool)> = found
            .iter()
            .map(|c| (c.agent_session.as_str(), c.maybe_held_by_codex_server))
            .collect();
        // A thread an app-server client started may be hosted there.
        assert_eq!(ids.len(), 2);
        assert!(
            ids.contains(&(CX_A, false)) && ids.contains(&(CX_D, true)),
            "{ids:?}"
        );
    }

    #[test]
    fn a_huge_session_meta_still_says_it_is_headless() {
        let home = tempfile::tempdir().unwrap();
        // Instructions far past the head window, before `source`.
        let big = "x".repeat(3 * 1024 * 1024);
        rollout(
            home.path(),
            CX_A,
            json!({"id": CX_A, "base_instructions": {"text": big},
                   "cwd": "/src/app", "source": "exec"}),
            &codex_turn("/src/app", "summarize", "ok"),
        );
        let why = explain_codex_in(home.path(), CX_A, &|_| false)
            .reason()
            .unwrap();
        assert!(why.contains("headless"), "{why}");
    }

    #[test]
    fn explanations_merge_by_kind() {
        use Why::*;
        let m = |c, x| merge_why("ab", c, x);
        assert!(m(NotFound, NotFound).starts_with("no Claude Code or Codex"));
        // The CLI's "already tracked" path keys on this prefix.
        assert!(m(NotFound, Tracked).starts_with("rlord already tracks"));
        assert!(m(Tracked, NotFound).starts_with("rlord already tracks"));
        assert_eq!(m(Rejected("x".into()), NotFound), "x");
        assert!(m(Tracked, Importable).starts_with("2 conversations"));
        assert!(m(Ambiguous(3), NotFound).starts_with("3 conversations"));
    }

    #[test]
    fn a_moved_codex_thread_keeps_no_start_branch() {
        let home = tempfile::tempdir().unwrap();
        rollout(
            home.path(),
            CX_A,
            json!({"id": CX_A, "cwd": "/gone/a", "git": {"branch": "feat"}}),
            &codex_turn("/gone/b", "hello there", "hi"),
        );
        let c = &scan_codex_in(home.path(), &ScanOpts::default(), &|_| false, "", &[])[0];
        assert_eq!(c.cwd, "/gone/b");
        assert_eq!(c.branch, None);
    }

    #[test]
    fn index_rejects_cost_no_read_and_app_threads_are_flagged_only_when_fresh() {
        let home = tempfile::tempdir().unwrap();
        // Unreadable rollouts the index already rules out: never read, so
        // the offer's budget isn't spent on them (the file isn't JSON).
        let day = home.path().join("sessions/2026/11/11");
        std::fs::create_dir_all(&day).unwrap();
        for id in [CX_B, CX_C] {
            std::fs::write(day.join(format!("rollout-x-{id}.jsonl")), "junk\n").unwrap();
        }
        rollout(
            home.path(),
            CX_A,
            json!({"id": CX_A, "cwd": "/src/app"}),
            &codex_turn("/src/app", "hello there", "hi"),
        );
        let c = rusqlite::Connection::open(home.path().join("state_5.sqlite")).unwrap();
        c.execute_batch(&format!(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT, thread_source TEXT, source TEXT);
             INSERT INTO threads VALUES ('{CX_A}', '/src/app', 'user', 'vscode');
             INSERT INTO threads VALUES ('{CX_B}', '/src/app', 'subagent', 'cli');
             INSERT INTO threads VALUES ('{CX_C}', '/src/app', 'user', 'exec');"
        ))
        .unwrap();
        drop(c);
        let found = scan_codex_in(home.path(), &ScanOpts::default(), &|_| false, "", &[]);
        assert_eq!(found.len(), 1);
        // Just written: may be held by the app.
        assert!(found[0].maybe_held_by_codex_server && !found[0].open_elsewhere);
        assert!(has_codex_conversations_in(home.path(), 0, &|_| false, &[]));
    }

    #[test]
    fn sub_agent_sources_and_running_servers() {
        assert!(is_subagent_source("subagent"));
        assert!(is_subagent_source(r#"{"subagent":{"other":"x"}}"#));
        assert!(!is_subagent_source("cli") && !is_subagent_source(r#"{"x":1}"#));
        let row = IndexRow {
            source: Some(r#"{"subagent":"review"}"#.into()),
            ..Default::default()
        };
        assert!(index_reject(&row).is_some_and(|w| w.contains("sub-agent")));
        assert!(codex_server_running("/opt/bin/codex app-server --listen x"));
        assert!(!codex_server_running("codex resume abc"));

        // An old app-server thread is flagged while a server runs.
        let home = tempfile::tempdir().unwrap();
        let p = rollout(
            home.path(),
            CX_A,
            json!({"id": CX_A, "cwd": "/src/app", "source": "vscode"}),
            &codex_turn("/src/app", "hello there", "hi"),
        );
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let scan = |procs: &str| {
            scan_codex_in(home.path(), &ScanOpts::default(), &|_| false, procs, &[])[0]
                .maybe_held_by_codex_server
        };
        assert!(!scan(""));
        assert!(scan("codex app-server"));
    }

    #[test]
    fn rlords_unbound_codex_sessions_and_meta_sources() {
        let home = tempfile::tempdir().unwrap();
        rollout(
            home.path(),
            CX_A,
            json!({"id": CX_A, "cwd": "/src/app"}),
            &codex_turn("/src/app", "hello there", "hi"),
        );
        let started = transcript::parse_iso_ms("2026-11-11T21:55:59.000Z").unwrap();
        let scan = |unbound: Vec<(String, i64)>| {
            let opts = ScanOpts {
                unbound_codex: unbound,
                ..Default::default()
            };
            scan_codex_in(home.path(), &opts, &|_| false, "", &[]).len()
        };
        assert_eq!(scan(vec![]), 1);
        // Created before the thread started, in its folder: maybe its own.
        assert_eq!(scan(vec![("/src/app".into(), started - 1)]), 0);
        // Created after, or elsewhere: not.
        assert_eq!(scan(vec![("/src/app".into(), started + 1)]), 1);
        assert_eq!(scan(vec![("/src/web".into(), 0)]), 1);

        // The rollout says exec though the index says cli: headless.
        rollout(
            home.path(),
            CX_B,
            json!({"id": CX_B, "cwd": "/src/app", "source": "exec"}),
            &codex_turn("/src/app", "hello there", "hi"),
        );
        let c = rusqlite::Connection::open(home.path().join("state_5.sqlite")).unwrap();
        c.execute_batch(&format!(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, cwd TEXT, thread_source TEXT, source TEXT);
             INSERT INTO threads VALUES ('{CX_B}', '/src/app', 'user', 'cli');"
        ))
        .unwrap();
        drop(c);
        let why = explain_codex_in(home.path(), CX_B, &|_| false)
            .reason()
            .unwrap();
        assert!(why.contains("headless"), "{why}");
    }

    #[test]
    fn codex_rollout_names_carry_the_id() {
        let p = |n: &str| PathBuf::from(format!("/s/{n}"));
        let a = p(&format!("rollout-2026-11-11T21-55-59-{CX_A}.jsonl"));
        assert_eq!(codex_rollout_id(&a), Some(CX_A));
        assert_eq!(codex_rollout_id(&p("rollout-x.jsonl")), None);
        assert_eq!(codex_rollout_id(&p(&format!("{CX_A}.jsonl"))), None);
    }
}
