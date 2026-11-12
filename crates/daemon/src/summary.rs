//! Archive summaries: what a session was for, what got done, what's left,
//! in a paragraph, written once when it's archived (or when asked).
//!
//! Plain code decides everything but the words. One worker, one summary at
//! a time, written by the session's own agent on a cheap model (`[archive]
//! summary_model` for Claude Code, `codex_summary_model` for Codex), with
//! nothing saved by the agent and nothing it could change. (OpenCode has
//! its own ways to the same two steps: see `opencode_summary`.)
//!
//! - **In full** when the conversation fits (`summary_max_tokens`): the
//!   agent reads a fork of it (Claude: `--resume … --fork-session`, no
//!   tools, no persistence; Codex: `codex exec fork … --ephemeral`,
//!   read-only sandbox) and is asked for the summary. The session itself is
//!   untouched, running or not, and nothing is typed into it.
//! - **From excerpts** otherwise (or if that fails): the first prompt and
//!   the conversation's last messages (`history::read`, bounded), sent as
//!   plain text.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use rlord_core::agents::opencode_service::Service;
use rlord_core::agents::transcript::{self, clip};
use rlord_core::agents::{claude, codex, opencode, Agent};
use rlord_core::types::{now_ms, Session};
use rlord_core::{history, paths};

use crate::Daemon;

const ASK: &str = "Summarize this coding session for an archive, in under 150 words of plain \
text (no headings, no lists unless they help). Cover: what it was for, what got done, what \
was left unfinished, and the key files, decisions or commands someone would need to pick it \
up again. Answer from the conversation only; don't use tools.";

/// Each run of the agent gets this long.
const TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Default)]
pub struct Summaries {
    queue: Mutex<Option<mpsc::Sender<(String, bool)>>>,
    pending: Mutex<HashSet<String>>,
}

impl Summaries {
    pub fn pending(&self) -> HashSet<String> {
        self.pending.lock().map(|p| p.clone()).unwrap_or_default()
    }
}

/// Where summary runs' output files go (`spawn_wait`, Codex's answer):
/// beside the excerpt runs' folder (`<data>/summaries`), never in it, so a
/// run never sees another's output.
fn output_dir() -> PathBuf {
    paths::data_dir().join("summary-output")
}

/// Start the worker. Output files an earlier daemon left (it ended
/// mid-run) are removed first: nothing runs yet.
pub fn start(d: &Arc<Daemon>) {
    rlord_core::process::sweep(&output_dir());
    // (Where they went before they had their own folder.)
    rlord_core::process::sweep(&paths::data_dir().join("summaries"));
    let (tx, rx) = mpsc::channel::<(String, bool)>();
    if let Ok(mut q) = d.summaries.queue.lock() {
        *q = Some(tx);
    }
    let d = Arc::clone(d);
    std::thread::spawn(move || {
        while let Ok((id, force)) = rx.recv() {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| make(&d, &id, force))) {
                Ok(Err(e)) => {
                    tracing::info!(session = %id, error = %format!("{e:#}"), "no summary")
                }
                Err(_) => tracing::warn!(session = %id, "summary panicked"),
                Ok(Ok(())) => {}
            }
            if let Ok(mut p) = d.summaries.pending.lock() {
                p.remove(&id);
            }
            d.changed();
        }
    });
}

/// Queue a summary (`force`: replace an existing one). Returns whether it
/// was queued (not already waiting).
pub fn enqueue(d: &Daemon, id: &str, force: bool) -> bool {
    let Ok(mut p) = d.summaries.pending.lock() else {
        return false;
    };
    if !p.insert(id.to_string()) {
        return false;
    }
    let sent = d
        .summaries
        .queue
        .lock()
        .ok()
        .and_then(|q| {
            q.as_ref()
                .map(|tx| tx.send((id.to_string(), force)).is_ok())
        })
        .unwrap_or(false);
    if !sent {
        p.remove(id);
    }
    sent
}

/// The transcript a session writes, if it's on disk.
pub fn transcript_of(d: &Daemon, s: &Session) -> Option<PathBuf> {
    transcripts_of(d, s).into_iter().next()
}

/// Every transcript a session has had on disk, current first: a `/clear`
/// starts a new one, and the work before it is in the older ones.
pub fn transcripts_of(d: &Daemon, s: &Session) -> Vec<PathBuf> {
    let (src, past) = {
        let db = d.db();
        (
            db.transcript_source(&s.id).ok().flatten(),
            db.past_agent_sessions(&s.id).unwrap_or_default(),
        )
    };
    let agent = Agent::parse(&s.agent);
    let path_of = |a: &str| agent.and_then(|ag| ag.transcript_path(&s.project_path, a));
    let mut out: Vec<PathBuf> = Vec::new();
    let current = src
        .map(PathBuf::from)
        .or_else(|| s.agent_session.as_deref().and_then(path_of));
    // Older ones are where the agent puts that conversation; Claude's may
    // also sit next to the current one (its project folder).
    let dir = current
        .as_ref()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .filter(|_| agent == Some(Agent::Claude));
    let older = past.iter().flat_map(|a| {
        let mut v: Vec<PathBuf> = path_of(a).into_iter().collect();
        if let Some(d) = &dir {
            v.push(d.join(format!("{a}.jsonl")));
        }
        v
    });
    for p in current.clone().into_iter().chain(older) {
        if transcript::exists(&p) && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Below this a transcript is just a cleared start: summarize the one
/// before it.
const TRIVIAL_TOKENS: u64 = 5_000;

fn make(d: &Daemon, id: &str, force: bool) -> Result<()> {
    let s = d.db().resolve_ref(id)?;
    // An existing summary stands unless there's been a message since it was
    // written (archived again after more work) or it's forced.
    let since = crate::triage::updated_at(&s);
    if !force && !s.summary.trim().is_empty() && s.summary_at.is_some_and(|t| t >= since) {
        return Ok(());
    }
    let agent = Agent::named(&s.agent)?;
    // The latest transcript with real work in it, and its conversation id.
    let all = transcripts_of(d, &s);
    let path = all
        .iter()
        .find(|p| context_tokens(p) >= TRIVIAL_TOKENS)
        .or(all.first())
        .cloned()
        .context("no transcript on disk")?;
    let conversation = conversation_of(agent, &path, &s);
    let cfg = d.config();
    let model = match agent {
        Agent::Claude => cfg.archive.summary_model.trim(),
        Agent::Codex => cfg.archive.codex_summary_model.trim(),
        Agent::OpenCode => cfg.archive.opencode_summary_model.trim(),
    };
    let mut ask = ASK.to_string();
    let known = [
        ("The user's one-line description", s.description.trim()),
        ("The user's notes", s.notes.trim()),
    ];
    for (what, v) in known.iter().filter(|(_, v)| !v.is_empty()) {
        ask.push_str(&format!("\n\n{what}: {}", clip(v, 1500)));
    }
    let fits = context_tokens(&path) <= cfg.archive.summary_max_tokens;
    if agent == Agent::OpenCode {
        let text = opencode_summary(&s, &path, conversation.as_deref(), &ask, fits, model)?;
        return save(d, &s, &text);
    }
    let full = match (&conversation, fits) {
        (Some(a), true) if Path::new(&s.project_path).is_dir() => {
            run(agent, model, Some((a, &ask)), &s.project_path, None)
                .map_err(|e| tracing::info!(session = %s.id, error = %format!("{e:#}"), "full summary failed; using excerpts"))
                .ok()
        }
        _ => None,
    };
    let text = match full {
        Some(t) => t,
        None => {
            let excerpt = excerpt(&path)?;
            let dir = paths::data_dir().join("summaries");
            std::fs::create_dir_all(&dir)?;
            let prompt = format!("{ask}\n\nThe conversation (excerpts):\n\n{excerpt}");
            run(agent, model, None, &dir.to_string_lossy(), Some(&prompt))?
        }
    };
    save(d, &s, &text)
}

fn save(d: &Daemon, s: &Session, text: &str) -> Result<()> {
    let text = text.trim();
    if text.is_empty() {
        bail!("empty summary");
    }
    d.db()
        .set_summary(&s.id, &keep_lines(text, 1500), Some(now_ms()))?;
    Ok(())
}

/// An OpenCode session's summary, by the first of these that works:
///
/// 1. **Its server, from the session's own context** (`POST
///    /api/session/{id}/generate`): the session's model answers and
///    nothing is added to the conversation (checked against OpenCode
///    2.2.7: no message, no status change; only its token totals grow).
///    When the conversation fits and the server is up.
/// 2. **A fork** (`opencode run -s <id> --fork --archived`, on the
///    session's own model), read from OpenCode's database and then
///    deleted (`opencode session delete`).
/// 3. **Excerpts**, as for the other agents, to a session of its own
///    (`opencode run -s <new id> --archived`), deleted afterwards.
fn opencode_summary(
    s: &Session,
    path: &Path,
    conversation: Option<&str>,
    ask: &str,
    fits: bool,
    model: &str,
) -> Result<String> {
    let note = |how: &str, e: anyhow::Error| tracing::info!(session = %s.id, error = %format!("{e:#}"), "{how} summary failed");
    if let Some(id) = conversation.filter(|_| fits) {
        let generated = Service::read(&opencode::service_file())
            .context("OpenCode's server isn't running")
            .and_then(|svc| svc.generate(id, ask, TIMEOUT));
        match generated {
            Ok(t) if !t.trim().is_empty() => return Ok(t),
            Ok(_) => note("generated", anyhow::anyhow!("empty answer")),
            Err(e) => note("generated", e),
        }
        if Path::new(&s.project_path).is_dir() {
            // (A fork keeps the session's own model: `model` is for the
            // excerpt run only.)
            match opencode_run("", Some(id), ask, &s.project_path) {
                Ok(t) => return Ok(t),
                Err(e) => note("forked", e),
            }
        }
    }
    let excerpt = excerpt(path)?;
    let dir = paths::data_dir().join("summaries");
    std::fs::create_dir_all(&dir)?;
    let prompt = format!("{ask}\n\nThe conversation (excerpts):\n\n{excerpt}");
    opencode_run(model, None, &prompt, &dir.to_string_lossy())
}

/// One `opencode run` in a throwaway session (a fork of `fork`, or a new
/// one), its answer read from OpenCode's database, the session deleted.
fn opencode_run(model: &str, fork: Option<&str>, ask: &str, cwd: &str) -> Result<String> {
    let db = opencode::db_path();
    let own = opencode::mint_session_id();
    // (A little before: the two clocks are the same machine's.)
    let since = now_ms() - 2_000;
    let argv = opencode::summary_argv(model, fork, &own, ask);
    let ran = spawn_wait("opencode", &argv, cwd, None);
    // The session it wrote in: the fork it made, or the one named.
    let wrote = match fork {
        Some(id) => opencode::fork_of(&db, id, since),
        // (Its own id: deleted unless surely not there.)
        None => Some(own).filter(|id| opencode::exists(&db, id) != Some(false)),
    };
    let answer = wrote
        .as_deref()
        .and_then(|id| opencode::latest_reply(&db, id));
    if let Some(id) = &wrote {
        match spawn_wait("opencode", &opencode::delete_argv(id), cwd, None) {
            Ok((st, _)) if st.success() => {}
            _ => tracing::warn!(opencode_session = %id, "a summary's session wasn't deleted"),
        }
    }
    let (status, _) = ran?;
    if !status.success() {
        bail!("opencode run failed ({status})");
    }
    answer.context("no answer from opencode")
}

/// At most `max` characters, line breaks kept.
fn keep_lines(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// The conversation a transcript holds, as its agent names it: Claude's
/// transcript is named after it; a Codex rollout's name ends in it.
fn conversation_of(agent: Agent, path: &Path, s: &Session) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    match agent {
        Agent::Claude => Some(stem),
        Agent::OpenCode => opencode::locate(path).map(|(_, id)| id.to_string()),
        Agent::Codex => s
            .agent_session
            .clone()
            .filter(|id| stem.ends_with(id.as_str()))
            // An older rollout of the session: the id is the name's last 36.
            .or_else(|| {
                let at = stem.len().checked_sub(36)?;
                stem.is_char_boundary(at).then(|| stem[at..].to_string())
            }),
    }
}

pub use transcript::context_tokens;

/// The start of the conversation and its last messages, as plain text,
/// about 14k characters at most.
fn excerpt(path: &Path) -> Result<String> {
    let first = transcript::transcript_excerpts(path).first_prompt;
    let last = history::read(path, None, 80, 0, 12_000)?;
    let mut out = String::new();
    if let Some(f) = first {
        out.push_str(&format!("[first prompt] {f}\n\n…\n\n"));
    }
    for m in last {
        out.push_str(&format!("[{}] {}\n\n", m.who, m.text));
    }
    if out.trim().is_empty() {
        bail!("nothing to summarize");
    }
    Ok(out)
}

/// One run of the agent with nothing saved and nothing it could change
/// (see `claude::summary_argv`, `codex::summary_argv`); its answer. `fork`:
/// the conversation to read in full and the question; else `stdin` is the
/// whole prompt.
fn run(
    agent: Agent,
    model: &str,
    fork: Option<(&str, &str)>,
    cwd: &str,
    stdin: Option<&str>,
) -> Result<String> {
    // Codex writes its answer to a file.
    let out_file = rlord_core::process::unique_file(&output_dir(), "answer");
    let argv = match agent {
        Agent::Claude => claude::summary_argv(model, fork),
        Agent::Codex => {
            if let Some(dir) = out_file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            codex::summary_argv(model, fork, &out_file)
        }
        // (Not through here: see `opencode_summary`.)
        Agent::OpenCode => bail!("OpenCode summaries don't run this way"),
    };
    let name = agent.name();
    let ran = spawn_wait(name, &argv, cwd, stdin);
    if ran.is_err() {
        let _ = std::fs::remove_file(&out_file);
    }
    let (status, out) = ran?;
    match agent {
        Agent::Claude => claude_answer(&out),
        Agent::Codex => {
            let answer = std::fs::read_to_string(&out_file);
            let _ = std::fs::remove_file(&out_file);
            if !status.success() {
                bail!("codex exec failed ({status})");
            }
            answer.context("no answer from codex")
        }
        Agent::OpenCode => unreachable!("refused above"),
    }
}

/// Run a command to its end (at most `TIMEOUT`): its exit status and what
/// it printed. Its output goes to a file, not a pipe: a command that
/// starts a background process (OpenCode's server) would hand it the
/// pipe, and reading to its end would then wait on that process.
fn spawn_wait(
    name: &str,
    argv: &[String],
    cwd: &str,
    stdin: Option<&str>,
) -> Result<(std::process::ExitStatus, String)> {
    spawn_wait_in(&output_dir(), name, argv, cwd, stdin)
}

/// `spawn_wait`, with the directory its output file goes in.
fn spawn_wait_in(
    dir: &Path,
    name: &str,
    argv: &[String],
    cwd: &str,
    stdin: Option<&str>,
) -> Result<(std::process::ExitStatus, String)> {
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        // Not one of rlord's sessions: its hooks must not report as one.
        .env_remove("RLORD_SESSION_ID")
        .env_remove("TMUX_PANE")
        .stderr(std::process::Stdio::null());
    rlord_core::process::run_to_file(cmd, dir, TIMEOUT, stdin)
        .with_context(|| format!("running {name}"))
}

/// The answer in `claude -p --output-format json`'s output.
fn claude_answer(out: &str) -> Result<String> {
    let v: Value = serde_json::from_str(out.trim()).context("reading claude's answer")?;
    if v["is_error"] == true {
        bail!(
            "claude: {}",
            v["result"]
                .as_str()
                .unwrap_or("error")
                .chars()
                .take(200)
                .collect::<String>()
        );
    }
    v["result"]
        .as_str()
        .map(str::to_string)
        .context("no result from claude")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(agent_session: Option<&str>) -> Session {
        serde_json::from_value(serde_json::json!({
            "id": "01s", "name": "s", "name_locked": false, "agent": "codex",
            "agent_session": agent_session, "agent_session_trust": "hook",
            "project_path": "/src/app", "tmux_session": null, "status": "stopped",
            "group_path": "", "pinned": false, "created_at": 1, "archived_at": null,
            "last_user_msg_at": null, "last_agent_msg_at": null,
            "last_state_change_at": null, "repo_path": null, "workspace_path": null,
            "branch": null, "model": null,
        }))
        .unwrap()
    }

    #[test]
    fn a_transcript_names_its_conversation() {
        let id = "01a1ce53-c71d-7e13-b376-63f7271fa320";
        let old = "01a1ce40-0000-7000-8000-000000000000";
        let rollout = |id: &str| {
            PathBuf::from(format!(
                "/h/.codex/sessions/2026/11/11/rollout-2026-11-11T21-55-59-{id}.jsonl"
            ))
        };
        let s = session(Some(id));
        assert_eq!(
            conversation_of(Agent::Codex, &rollout(id), &s).as_deref(),
            Some(id)
        );
        // An older rollout of the same session.
        assert_eq!(
            conversation_of(Agent::Codex, &rollout(old), &s).as_deref(),
            Some(old)
        );
        assert_eq!(
            conversation_of(Agent::Claude, Path::new("/p/abc.jsonl"), &s).as_deref(),
            Some("abc")
        );
        let oc = Path::new("/h/.local/share/opencode/opencode.db/ses_e3192cfeeffeUElN5gOL3f5nhg");
        assert_eq!(
            conversation_of(Agent::OpenCode, oc, &s).as_deref(),
            Some("ses_e3192cfeeffeUElN5gOL3f5nhg")
        );
    }

    #[test]
    fn a_command_that_leaves_something_running_doesnt_hold_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        // Prints, then leaves a child behind that keeps its output open
        // (as `opencode` does when it starts its server).
        let argv: Vec<String> = ["sh", "-c", "echo answer; (sleep 20 &) ; exit 0"]
            .map(String::from)
            .to_vec();
        let start = std::time::Instant::now();
        let (status, out) = spawn_wait_in(dir.path(), "sh", &argv, &cwd, None).unwrap();
        assert!(status.success());
        assert_eq!(out.trim(), "answer");
        assert!(start.elapsed() < Duration::from_secs(10));
        // What is sent on stdin arrives.
        let argv: Vec<String> = ["sh", "-c", "cat"].map(String::from).to_vec();
        let (_, out) = spawn_wait_in(dir.path(), "sh", &argv, &cwd, Some("the prompt")).unwrap();
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "output files removed"
        );
        assert_eq!(out, "the prompt");
    }

    #[test]
    fn claude_answers_are_read_from_its_json() {
        assert_eq!(
            claude_answer(r#"{"is_error": false, "result": "Did X."}"#).unwrap(),
            "Did X."
        );
        let e = claude_answer(r#"{"is_error": true, "result": "nope"}"#).unwrap_err();
        assert!(e.to_string().contains("nope"));
    }
}
