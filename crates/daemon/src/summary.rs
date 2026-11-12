//! Archive summaries: what a session was for, what got done, what's left,
//! in a paragraph, written once when it's archived (or when asked).
//!
//! Plain code decides everything but the words. One worker, one summary at
//! a time, written by the session's own agent on a cheap model (`[archive]
//! summary_model` for Claude Code, `codex_summary_model` for Codex), with
//! nothing saved by the agent and nothing it could change:
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
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use rlord_core::agents::transcript::{self, clip};
use rlord_core::agents::{claude, codex, Agent};
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

/// Start the worker.
pub fn start(d: &Arc<Daemon>) {
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
        if p.exists() && !out.contains(&p) {
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
    let text = text.trim();
    if text.is_empty() {
        bail!("empty summary");
    }
    d.db()
        .set_summary(&s.id, &keep_lines(text, 1500), Some(now_ms()))?;
    Ok(())
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
    use std::io::{Read, Write};
    // Codex writes its answer to a file.
    let out_file = paths::data_dir().join("summaries").join(format!(
        "answer-{}-{}.txt",
        std::process::id(),
        now_ms()
    ));
    let argv = match agent {
        Agent::Claude => claude::summary_argv(model, fork),
        Agent::Codex => {
            if let Some(dir) = out_file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            codex::summary_argv(model, fork, &out_file)
        }
    };
    let name = agent.name();
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        // Not one of rlord's sessions: its hooks must not report as one.
        .env_remove("RLORD_SESSION_ID")
        .env_remove("TMUX_PANE")
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().with_context(|| format!("running {name}"))?;
    // Written from its own thread: a stalled agent must not block the
    // worker past the timeout.
    if let (Some(text), Some(mut w)) = (stdin, child.stdin.take()) {
        let text = text.to_string();
        std::thread::spawn(move || {
            let _ = w.write_all(text.as_bytes());
        });
    }
    // Read while waiting: a full pipe would stall it.
    let mut so = child.stdout.take().context("the agent's output")?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = so.read_to_string(&mut out);
        out
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(200)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&out_file);
                bail!("{name} took too long");
            }
        }
    };
    let out = reader.join().unwrap_or_default();
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
    }
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
