//! Finding agent conversations rlord doesn't track yet, so existing work can
//! be imported. Reads only the agents' own files (no AI, and nothing specific
//! to whichever session manager started them): for Claude Code, the
//! transcripts under `~/.claude/projects/<cwd slug>/<uuid>.jsonl`.
//!
//! The format is internal to Claude Code and changes between versions, so
//! every field is optional and anything unreadable is skipped, never fatal.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::agents::claude;
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
    /// Only these conversation ids (ignores `since_ms`).
    pub only: Vec<String>,
}

/// How recently a transcript must have been written to count as possibly open.
const RECENT_WRITE: Duration = Duration::from_secs(10 * 60);

/// Scan Claude Code's transcripts. `tracked` says whether rlord already has
/// a session for a conversation id; those are skipped.
pub fn scan_claude(opts: &ScanOpts, tracked: &dyn Fn(&str) -> bool) -> Vec<Candidate> {
    let projects = claude::config_dir().join("projects");
    let procs = process_commands();
    scan_claude_in(&projects, opts, tracked, &procs, &excluded_dirs())
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
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Conversations are named by uuid; anything else is not one.
            if uuid::Uuid::parse_str(id).is_err() || tracked(id) {
                continue;
            }
            if !opts.only.is_empty() && !opts.only.iter().any(|o| o == id) {
                continue;
            }
            let mtime = f.metadata().and_then(|m| m.modified()).ok().map(ms_epoch);
            // Cheap pre-filter: a file untouched since the cutoff can't hold
            // a newer exchange.
            if opts.only.is_empty() && mtime.is_some_and(|m| m < opts.since_ms) {
                continue;
            }
            let Some(c) = candidate(&path, id, &slug, mtime, procs, excluded) else {
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
) -> Option<Candidate> {
    let (head, tail) = claude::read_head_tail(path);
    let all = || head.iter().chain(tail.iter());

    // `claude --resume` finds a conversation by the folder it runs in, so the
    // cwd must be the one this transcript's directory is named after.
    let cwd = all()
        .filter_map(|v| v["cwd"].as_str())
        .find(|c| claude::project_slug(c) == slug)?
        .to_string();
    // rlord's own conversations (the overlord, test sandboxes of it) and
    // anything in a temp dir are never the user's work.
    if cwd.ends_with("/robot-overlord/overlord")
        || excluded.iter().any(|d| Path::new(&cwd).starts_with(d))
    {
        return None;
    }
    let entrypoint = all()
        .find_map(|v| v["entrypoint"].as_str())
        .map(str::to_string);
    // Headless runs (`claude -p`, the SDK) are tools, not conversations.
    if entrypoint.as_deref().is_some_and(|e| e.starts_with("sdk")) {
        return None;
    }
    let ex = claude::excerpts_from(&head, &tail);
    // Nothing the user ever typed: an empty or aborted start.
    ex.first_prompt.as_ref()?;

    let ts = |v: &Value| v["timestamp"].as_str().and_then(claude::parse_iso_ms);
    let exchange =
        |v: &&Value| (v["type"] == "user" || v["type"] == "assistant") && v["isMeta"] != true;
    let last_of = |kind: &str| {
        tail.iter()
            .rev()
            .filter(|v| v["type"] == kind && v["isMeta"] != true)
            .find_map(ts)
    };
    let last_active_at = tail.iter().rev().filter(exchange).find_map(ts).or(mtime)?;
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
    let (repo, worktree, recreatable) = if dir_exists {
        let repo = git::main_repo_root(&cwd);
        let canon = std::fs::canonicalize(&cwd)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| cwd.clone());
        let worktree = repo.as_deref().is_some_and(|r| r != canon);
        (repo, worktree, false)
    } else {
        // The folder is gone (a removed worktree). If an ancestor is still a
        // repo with the branch, the worktree can come back where it was.
        let repo = nearest_existing(Path::new(&cwd))
            .and_then(|p| git::main_repo_root(&p.to_string_lossy()));
        let recreatable = match (&repo, &branch) {
            (Some(r), Some(b)) => git::branch_exists(r, b),
            _ => false,
        };
        let worktree = repo.is_some();
        (repo, worktree, recreatable)
    };

    let recently_written =
        mtime.is_some_and(|m| ms_epoch(SystemTime::now()) - m < RECENT_WRITE.as_millis() as i64);
    Some(Candidate {
        agent: "claude".into(),
        agent_session: id.to_string(),
        transcript: path.to_string_lossy().into_owned(),
        cwd,
        dir_exists,
        repo,
        branch,
        worktree,
        recreatable,
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
        open_elsewhere: procs.contains(id),
        recently_written,
    })
}

/// rlord's data dir (the overlord's folder) and the temp dirs.
fn excluded_dirs() -> Vec<PathBuf> {
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

/// Every running process's command line, one per line ("" if `ps` fails).
fn process_commands() -> String {
    std::process::Command::new("ps")
        .args(["-axo", "command="])
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn ms_epoch(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

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

        let since = claude::parse_iso_ms("2026-11-01T00:00:00.000Z").unwrap();
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
            claude::parse_iso_ms("2026-11-05T12:00:00.000Z").unwrap()
        );
        assert!(c.open_elsewhere && c.dir_exists && c.repo.is_none());

        // Tracked conversations are skipped; `only` ignores the time window.
        assert!(scan_claude_in(&projects, &opts, &|id| id == ID_A, "", &[]).is_empty());
        let only = ScanOpts {
            only: vec![ID_B.into()],
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
}
