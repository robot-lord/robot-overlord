//! The overlord's own past. It restarts on a fresh conversation after
//! sitting idle (`overlord::start_fresh_keeper`), and the user may `/clear`
//! it, so what it did before isn't in its context. Nothing is preloaded
//! into a fresh conversation: it looks things up here when it needs them.
//!
//! rlord already has the whole history: every switch to another
//! conversation is an event on the overlord's session (`fresh_conversation`
//! with a reason, or `agent_session_changed`), and each conversation's
//! transcript is still on disk. This module reads those, for two readers
//! that must agree on what "earlier conversations" are:
//!
//! - the overlord's `recall` tool: search its earlier conversations, or a
//!   short digest of the previous one;
//! - the list: "fresh 2h ago · 14 earlier" on the overlord's row, its past
//!   conversations in the details pane, one of them in the preview.
//!
//! With an overlord on more than one agent, the list shows the active
//! one's own conversations; `recall` reads every overlord's (`pasts`).
//!
//! Plain code, no model call; every answer is bounded.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::agents::transcript::{self, clip, Format};
use rlord_core::agents::Agent;
use rlord_core::db::Db;
use rlord_core::history;
use rlord_core::timefmt::{fmt_ts, rel_age};
use rlord_core::types::{now_ms, Session, Status};

use crate::{overlord, Daemon};

/// One conversation the overlord has had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conv {
    /// The agent's conversation id.
    pub id: String,
    /// When the overlord moved to it (None: its first, started at setup).
    pub started_at: Option<i64>,
    /// When it moved on, and why: `idle` (the idle restart), `clear` (the
    /// user's `/clear`), `manual` (a fresh restart asked for), `continued`
    /// (the user went back to an earlier one), `switched` (anything else,
    /// e.g. a resume). None for the current one.
    pub ended_at: Option<i64>,
    pub ended: Option<String>,
    /// Minutes of idleness that ended it (idle restarts).
    pub idle_mins: Option<u64>,
}

/// Earlier conversations kept in view (the newest).
const MAX_EARLIER: usize = 200;

/// The overlord's conversations from its session's events: the current one
/// (None until the agent has one), and the earlier ones, newest first.
pub fn list(db: &Db, o: &Session) -> Result<(Option<Conv>, Vec<Conv>)> {
    Ok(from_changes(
        &db.conversation_changes(&o.id)?,
        o.agent_session.as_deref(),
    ))
}

fn from_changes(
    changes: &[(i64, String, Value)],
    current: Option<&str>,
) -> (Option<Conv>, Vec<Conv>) {
    let mut by_id: HashMap<String, Conv> = HashMap::new();
    let new = |id: &str| Conv {
        id: id.to_string(),
        started_at: None,
        ended_at: None,
        ended: None,
        idle_mins: None,
    };
    for (ts, kind, p) in changes {
        let reason = p["reason"]
            .as_str()
            .unwrap_or(if kind == "fresh_conversation" {
                // (Before reasons were recorded, only the idle restart made one.)
                "idle"
            } else {
                "switched"
            });
        if let Some(from) = p["from"].as_str().filter(|f| !f.is_empty()) {
            let c = by_id.entry(from.to_string()).or_insert_with(|| new(from));
            c.ended_at = Some(*ts);
            c.ended = Some(reason.to_string());
            c.idle_mins = p["idle_mins"].as_u64();
        }
        if let Some(to) = p["to"].as_str().filter(|t| !t.is_empty()) {
            let c = by_id.entry(to.to_string()).or_insert_with(|| new(to));
            c.started_at.get_or_insert(*ts);
            // Back on a conversation it had left: it hasn't ended.
            c.ended_at = None;
            c.ended = None;
            c.idle_mins = None;
        }
    }
    let cur = current.map(|id| {
        let mut c = by_id.remove(id).unwrap_or_else(|| new(id));
        // (An agent that assigns its own id: the event had no `to`.)
        c.ended_at = None;
        c.ended = None;
        c
    });
    let mut earlier: Vec<Conv> = by_id
        .into_values()
        .filter(|c| c.ended_at.is_some())
        .collect();
    earlier.sort_by(|a, b| b.ended_at.cmp(&a.ended_at).then(b.id.cmp(&a.id)));
    earlier.truncate(MAX_EARLIER);
    (cur, earlier)
}

/// For the overlord's row and its overlay: when its conversation started
/// (None: it has never restarted), how many came before, and why the last
/// one ended. Events only: no file is read.
pub fn summary(db: &Db, o: &Session) -> Value {
    let Ok(changes) = db.conversation_changes(&o.id) else {
        return Value::Null;
    };
    let (_, earlier) = from_changes(&changes, o.agent_session.as_deref());
    let last = earlier.first();
    // A fresh start: a restart on a new conversation or a `/clear`, not a
    // switch to another one (going back to an earlier one isn't "fresh").
    let fresh_at = changes
        .iter()
        .rev()
        .find(|(_, kind, p)| kind == "fresh_conversation" && p["reason"] != "continued")
        .map(|(ts, _, _)| *ts);
    json!({
        "fresh_at": fresh_at,
        "earlier": earlier.len(),
        "reason": last.and_then(|c| c.ended.clone()),
        "why": last.map(why),
    })
}

/// "fresh 2h ago · 14 earlier", from `summary` (None: never restarted).
pub fn fresh_label(summary: &Value) -> Option<String> {
    let n = summary["earlier"].as_u64().filter(|n| *n > 0)?;
    Some(format!(
        "fresh {} ago · {n} earlier",
        rel_age(summary["fresh_at"].as_i64())
    ))
}

/// Why a conversation ended, in words.
pub fn why(c: &Conv) -> String {
    match c.ended.as_deref() {
        Some("idle") => match c.idle_mins {
            Some(m) => format!("idle {m}m, restarted on a fresh conversation"),
            None => "idle, restarted on a fresh conversation".into(),
        },
        Some("clear") => "you cleared it with /clear".into(),
        Some("manual") => "restarted on a fresh conversation".into(),
        Some("continued") => "you went back to an earlier conversation".into(),
        Some(_) => "the overlord moved to another conversation".into(),
        None => "current".into(),
    }
}

/// Where conversation `id` of the overlord is on disk, if it still is.
fn transcript_of(db: &Db, o: &Session, id: &str) -> Option<PathBuf> {
    let agent = Agent::parse(&o.agent)?;
    let mut candidates: Vec<PathBuf> = agent
        .transcript_path(&o.project_path, id)
        .into_iter()
        .collect();
    // Claude keeps a folder's conversations together: next to the current.
    if agent == Agent::Claude {
        if let Some(dir) = db
            .transcript_source(&o.id)
            .ok()
            .flatten()
            .and_then(|p| Path::new(&p).parent().map(Path::to_path_buf))
        {
            candidates.push(dir.join(format!("{id}.jsonl")));
        }
    }
    candidates.into_iter().find(|p| transcript::exists(p))
}

/// What a conversation's transcript says about it, read once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Digest {
    first_at: Option<i64>,
    last_at: Option<i64>,
    /// Prompts the user typed.
    turns: usize,
    first_prompt: Option<String>,
    /// The last exchanges: (when, the user's ask, the overlord's reply),
    /// each clipped.
    exchanges: Vec<(Option<String>, String, Option<String>)>,
}

/// Exchanges kept from the end of a conversation.
const KEEP_EXCHANGES: usize = 12;
/// A transcript is read up to this many bytes (the overlord's are small:
/// it starts fresh after every idle spell).
const MAX_READ: u64 = 16 * 1024 * 1024;

fn digest(path: &Path) -> Digest {
    use std::io::{BufRead, Read};
    // By size: a finished conversation's file never changes again.
    static CACHE: Mutex<Option<HashMap<PathBuf, (u64, Digest)>>> = Mutex::new(None);
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if let Some((l, d)) = CACHE
        .lock()
        .expect("digest cache")
        .as_ref()
        .and_then(|c| c.get(path))
    {
        if *l == len {
            return d.clone();
        }
    }
    let format = Format::of(path);
    let mut d = Digest::default();
    let mut last: VecDeque<(Option<String>, String, Option<String>)> = VecDeque::new();
    if let Ok(f) = std::fs::File::open(path) {
        for l in std::io::BufReader::new(f.take(MAX_READ)).lines() {
            let Ok(l) = l else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&l) else {
                continue;
            };
            let line = format.line(&v);
            if let Some(at) = line.at().and_then(transcript::parse_iso_ms) {
                d.first_at.get_or_insert(at);
                d.last_at = Some(at);
            }
            if let Some(p) = line.prompt() {
                d.turns += 1;
                if d.first_prompt.is_none() {
                    d.first_prompt = Some(clip(&p, 160));
                }
                if last.len() == KEEP_EXCHANGES {
                    last.pop_front();
                }
                last.push_back((line.at().map(str::to_string), clip(&p, 300), None));
            } else if let Some(r) = line.reply() {
                // The turn's last reply is the answer.
                if let Some(e) = last.back_mut() {
                    e.2 = Some(clip(&r, 400));
                }
            }
        }
    }
    d.exchanges = last.into();
    let mut cache = CACHE.lock().expect("digest cache");
    let c = cache.get_or_insert_with(HashMap::new);
    if c.len() > 512 {
        c.clear();
    }
    c.insert(path.to_path_buf(), (len, d.clone()));
    d
}

/// The overlord's session and its conversations (current, earlier).
fn load(d: &Daemon) -> Result<(Session, Option<Conv>, Vec<Conv>)> {
    let db = d.db();
    let o = overlord::session(&db)?.context("no overlord yet")?;
    let (cur, earlier) = list(&db, &o)?;
    Ok((o, cur, earlier))
}

/// Rows the details pane lists at most.
const LIST_ROWS: usize = 30;

/// `overlord.conversations`: the current conversation and the earlier ones
/// (newest first, the first `LIST_ROWS`), each with when it started and
/// ended, why, its turns and its first prompt. The current one's transcript
/// is still growing, so it isn't read (no turns or first prompt).
pub fn conversations_rpc(d: &Daemon) -> Result<Value> {
    let (o, cur, earlier) = load(d)?;
    let row = |c: &Conv, current: bool| {
        let path = transcript_of(&d.db(), &o, &c.id);
        let dg = match &path {
            Some(p) if !current => digest(p),
            _ => Digest::default(),
        };
        json!({
            "id": c.id, "current": current,
            "started_at": c.started_at.or(dg.first_at).or(current.then_some(o.created_at)),
            "ended_at": c.ended_at,
            "ended": c.ended, "why": why(c),
            "turns": dg.turns, "first_prompt": dg.first_prompt,
            "on_disk": path.is_some(),
        })
    };
    let rows: Vec<Value> = earlier
        .iter()
        .take(LIST_ROWS)
        .map(|c| row(c, false))
        .collect();
    Ok(json!({
        "current": cur.as_ref().map(|c| row(c, true)),
        "earlier": rows,
        "total_earlier": earlier.len(),
    }))
}

/// An earlier conversation by id or id prefix.
fn find<'a>(earlier: &'a [Conv], id: &str) -> Result<&'a Conv> {
    let id = id.trim();
    let mut m = earlier.iter().filter(|c| c.id.starts_with(id));
    match (m.next(), m.next()) {
        (Some(c), None) if !id.is_empty() => Ok(c),
        (Some(_), Some(_)) => bail!("more than one earlier conversation starts with {id:?}"),
        _ => bail!("no earlier conversation {id:?}"),
    }
}

/// `overlord.conversation {id}`: the last messages of one earlier
/// conversation (text only, clipped: the reader search uses), for the
/// list's preview.
pub fn conversation(d: &Daemon, params: &Value) -> Result<Value> {
    let (o, _, earlier) = load(d)?;
    let c = find(&earlier, params["id"].as_str().unwrap_or(""))?;
    let path = transcript_of(&d.db(), &o, &c.id)
        .context("that conversation's transcript is no longer on disk")?;
    let msgs = history::read(&path, None, 80, 0, 16_000)?;
    Ok(json!({"id": c.id, "transcript": path, "messages": msgs}))
}

/// Conversations a search answers with.
const RECALL_LIMIT: usize = 6;
/// Characters a digest's exchanges may take in all.
const DIGEST_BUDGET: usize = 6_000;

/// A conversation `recall` can reach, and whose it was.
struct Past {
    o: Session,
    c: Conv,
    /// When it ended; for another overlord's current one, when it was
    /// last spoken in.
    when: Option<i64>,
}

/// What `recall` reads: every overlord's earlier conversations, and the
/// *other* overlords' current ones, newest first. Across all of them on
/// purpose: to the user there is one overlord desk, and they switch agents
/// by hand (often because one hit its usage limit), so "as I said
/// earlier" may well have been said to the other one. Only the active
/// overlord's own current conversation is left out: it is in its context.
fn pasts(d: &Daemon) -> Result<Vec<Past>> {
    let db = d.db();
    let active = overlord::session(&db)?.context("no overlord yet")?;
    let mut out = Vec::new();
    for o in overlord::all(&db)? {
        let (cur, earlier) = list(&db, &o)?;
        if o.id != active.id {
            if let Some(c) = cur {
                let when = o.last_user_msg_at.max(o.last_agent_msg_at);
                out.push(Past {
                    o: o.clone(),
                    c,
                    when,
                });
            }
        }
        out.extend(earlier.into_iter().map(|c| Past {
            o: o.clone(),
            when: c.ended_at,
            c,
        }));
    }
    out.sort_by(|a, b| b.when.cmp(&a.when).then(b.c.id.cmp(&a.c.id)));
    Ok(out)
}

fn find_past<'a>(pasts: &'a [Past], id: &str) -> Result<&'a Past> {
    let id = id.trim();
    let mut m = pasts.iter().filter(|p| p.c.id.starts_with(id));
    match (m.next(), m.next()) {
        (Some(p), None) if !id.is_empty() => Ok(p),
        (Some(_), Some(_)) => bail!("more than one earlier conversation starts with {id:?}"),
        _ => bail!("no earlier conversation {id:?}"),
    }
}

/// `overlord.recall {query?, conversation?, limit?}`: with a query, search
/// the overlords' earlier conversations (`pasts`); without, a digest of
/// the previous one (or of `conversation`).
pub fn recall(d: &Daemon, params: &Value) -> Result<Value> {
    let pasts = pasts(d)?;
    let ago = |ts: Option<i64>| ts.map(|t| format!("{} ago", rel_age(Some(t))));
    // Whose it was, said only when there is more than one overlord.
    let several = overlord::all(&d.db())?.len() > 1;
    let whose = |p: &Past| several.then(|| p.o.agent.clone());
    let query = params["query"].as_str().unwrap_or("").trim();
    if !query.is_empty() {
        let limit = params["limit"]
            .as_u64()
            .map_or(RECALL_LIMIT, |n| n.clamp(1, 15) as usize);
        let mut by_path: HashMap<PathBuf, &Past> = HashMap::new();
        let mut paths = Vec::new();
        for p in &pasts {
            if let Some(path) = transcript_of(&d.db(), &p.o, &p.c.id) {
                by_path.insert(path.clone(), p);
                paths.push(path);
            }
        }
        let mut found: Vec<(&Past, history::FileHits)> = history::search(&paths, query, 3)
            .into_iter()
            .filter_map(|fh| Some((*by_path.get(&fh.path)?, fh)))
            .collect();
        found.sort_by_key(|(p, _)| std::cmp::Reverse(p.when));
        let total = found.len();
        let rows: Vec<Value> = found
            .into_iter()
            .take(limit)
            .map(|(p, fh)| {
                json!({
                    "conversation": p.c.id,
                    "overlord": whose(p),
                    "ended": ago(p.c.ended_at),
                    "ended_at": p.c.ended_at.map(fmt_ts),
                    "transcript": fh.path,
                    "mentions": fh.total,
                    "hits": fh.hits,
                })
            })
            .collect();
        return Ok(json!({
            "query": query,
            "conversations": rows,
            "more": total.saturating_sub(limit),
            "searched": paths.len(),
            "note": "the overlord's own earlier conversations only, on every agent it runs on \
                     (search_history covers the sessions). read_history with a `transcript` \
                     and a hit's `line` reads around it",
        }));
    }
    let pick = match params["conversation"].as_str().map(str::trim) {
        Some(id) if !id.is_empty() => {
            let p = find_past(&pasts, id)?;
            let path = transcript_of(&d.db(), &p.o, &p.c.id)
                .context("that conversation's transcript is no longer on disk")?;
            Some((p, digest(&path)))
        }
        _ => pasts.iter().take(20).find_map(|p| {
            let path = transcript_of(&d.db(), &p.o, &p.c.id)?;
            let dg = digest(&path);
            (dg.turns > 0).then_some((p, dg))
        }),
    };
    let Some((p, dg)) = pick else {
        return Ok(json!({
            "earlier": pasts.len(),
            "note": "no earlier conversation with anything in it",
        }));
    };
    let c = &p.c;
    let mut used = 0;
    let mut kept: Vec<Value> = Vec::new();
    for (at, you, reply) in dg.exchanges.iter().rev() {
        used += you.len() + reply.as_ref().map_or(0, String::len);
        if used > DIGEST_BUDGET && !kept.is_empty() {
            break;
        }
        kept.push(json!({"at": at, "you": you, "overlord": reply}));
    }
    kept.reverse();
    Ok(json!({
        "conversation": c.id,
        "overlord": whose(p),
        "started": ago(c.started_at.or(dg.first_at)),
        "ended": ago(c.ended_at),
        "why_it_ended": if c.ended_at.is_some() {
            why(c)
        } else {
            format!("still open on the {} overlord (the user switched to you)", p.o.agent)
        },
        "turns": dg.turns,
        "exchanges_shown": kept.len(),
        "exchanges": kept,
        "earlier": pasts.len(),
        "note": "the last exchanges of the previous conversation, clipped. `query` searches all \
                 the earlier conversations; `conversation` (an id from a search) digests another",
    }))
}

/// `overlord.continue {id}` for a conversation another agent's overlord
/// had: switch to that overlord, then continue it there (if it isn't
/// that overlord's current one already). None: it is the active one's.
fn continue_elsewhere(d: &Daemon, id: &str) -> Result<Option<Value>> {
    let (active, _, earlier) = load(d)?;
    if find(&earlier, id).is_ok() {
        return Ok(None);
    }
    let pasts = pasts(d)?;
    let Ok(p) = find_past(&pasts, id) else {
        return Ok(None);
    };
    if p.o.id == active.id {
        return Ok(None);
    }
    let (conv, agent) = (p.c.id.clone(), p.o.agent.clone());
    let current = p.o.agent_session.as_deref() == Some(conv.as_str());
    if current {
        // Its current conversation: switching (which starts it) is all.
        let switched = d.rpc("overlord.switch", json!({"agent": agent}))?;
        return Ok(Some(
            json!({"continued": conv, "switched_to": agent, "session": switched}),
        ));
    }
    // `continue_conversation`'s checks first, so a switch that can't
    // carry on is never made.
    let before = d.db().resolve_ref(&p.o.id)?;
    can_continue(&d.db(), &before, &conv)?;
    let was_running = overlord::running(&before);
    // Made active without starting it: `continue_conversation` then starts
    // (or restarts) it once, onto that conversation.
    let activate = |id: &str, from: String| -> Result<()> {
        let _send = d.overlord_send.lock().expect("overlord send lock");
        let _session = d.sessions.lock(id);
        overlord::activate(&d.db(), id, Some(from), false)
    };
    activate(&p.o.id, active.agent.clone())?;
    d.bind_nav_keys();
    d.changed();
    let mut v = match continue_conversation(d, &json!({"id": conv})) {
        Ok(v) => v,
        Err(e) => {
            // Back as it was: the user's overlord active again, and this
            // one stopped if it was (`continue_conversation` already put
            // it back on its own conversation).
            let undo = || -> Result<()> {
                {
                    let _session = d.sessions.lock(&before.id);
                    let db = d.db();
                    let now = db.resolve_ref(&before.id)?;
                    if !was_running && overlord::running(&now) {
                        crate::methods::session_stop(&db, &json!({"ref": before.id}))?;
                    }
                }
                activate(&active.id, p.o.agent.clone())
            };
            if let Err(u) = undo() {
                tracing::warn!(error = %u, "undoing a failed switch to continue a conversation");
            }
            d.bind_nav_keys();
            d.changed();
            return Err(e);
        }
    };
    v["switched_to"] = json!(agent);
    Ok(Some(v))
}

/// Whether overlord `o` can go back to conversation `conv` now: its
/// agent installed, between turns, the transcript still on disk (where).
fn can_continue(db: &Db, o: &Session, conv: &str) -> Result<PathBuf> {
    if let Some(why) = rlord_core::agents::install::missing(Agent::named(&o.agent)?) {
        bail!("{why}");
    }
    if !matches!(
        o.status,
        Status::Idle | Status::WaitingInput | Status::Stopped
    ) {
        bail!(
            "the {} overlord is busy: continue it once its turn is over",
            o.agent
        );
    }
    transcript_of(db, o, conv).context("that conversation's transcript is no longer on disk")
}

/// `overlord.continue {id}`: go back to an earlier conversation. The
/// overlord is restarted resuming it, and the current one ends (it stays in
/// the list), as at an idle restart. Only between turns. One another
/// agent's overlord had (from `recall`): that overlord is switched to
/// first (`continue_elsewhere`).
pub fn continue_conversation(d: &Daemon, params: &Value) -> Result<Value> {
    // (Where the agent's command is: asked before any lock is taken.)
    rlord_core::agents::install::warm();
    // Another agent's overlord's: switch to it first (before any lock:
    // the switch takes its own).
    if let Some(v) = continue_elsewhere(d, params["id"].as_str().unwrap_or(""))? {
        return Ok(v);
    }
    // Nothing may be pasted into the pane while it's being replaced.
    let _send = d.overlord_send.lock().expect("overlord send lock");
    let _session = d.lock_overlord();
    let (o, _, earlier) = load(d)?;
    let c = find(&earlier, params["id"].as_str().unwrap_or(""))?;
    let db = d.db();
    let path = can_continue(&db, &o, &c.id)?;
    // Onto the conversation, then exactly one restart: `ensure` restarts
    // it when it isn't running (stopped, or its pane is gone) and leaves a
    // running one alone, which is restarted here.
    let source = db.transcript_source(&o.id)?;
    // (The agent wrote it: it is there to resume.)
    db.set_agent_session(&o.id, &c.id, "hook")?;
    db.upsert_transcript_source(&o.id, &path.to_string_lossy())?;
    let running = overlord::running(&db.resolve_ref(&o.id)?);
    let started = overlord::ensure(&db, &d.config(), &json!({})).and_then(|v| {
        if running {
            crate::methods::session_restart(&db, &json!({"ref": o.id}))
        } else {
            Ok(v)
        }
    });
    let v = match started {
        Ok(v) => v,
        Err(e) => {
            // Back on the conversation it had, and up again if it was.
            let undo = || -> Result<()> {
                if let Some(prev) = o.agent_session.as_deref() {
                    db.set_agent_session(&o.id, prev, &o.agent_session_trust)?;
                }
                if let Some(src) = source.as_deref() {
                    db.upsert_transcript_source(&o.id, src)?;
                }
                if running && !overlord::running(&db.resolve_ref(&o.id)?) {
                    overlord::ensure_session(&db, &d.config(), &o.id)?;
                }
                Ok(())
            };
            if let Err(u) = undo() {
                tracing::warn!(error = %u, "undoing a failed continue");
            }
            return Err(e);
        }
    };
    // Only now that it is on it: the conversation changed.
    let now = now_ms();
    db.insert_event(
        &o.id,
        now,
        "fresh_conversation",
        &json!({"from": o.agent_session, "to": c.id, "reason": "continued"}).to_string(),
    )?;
    overlord::set_fresh_at(&db, &o.id, now)?;
    Ok(json!({"continued": c.id, "session": v}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: i64, kind: &str, p: Value) -> (i64, String, Value) {
        (ts, kind.to_string(), p)
    }

    #[test]
    fn conversations_from_events() {
        let changes = vec![
            // Before reasons were recorded: an idle restart.
            ev(10, "fresh_conversation", json!({"from": "a", "to": "b"})),
            ev(
                20,
                "fresh_conversation",
                json!({"from": "b", "to": "c", "reason": "clear"}),
            ),
            ev(30, "agent_session_changed", json!({"from": "c", "to": "d"})),
            ev(
                40,
                "fresh_conversation",
                json!({"from": "d", "to": "e", "reason": "idle", "idle_mins": 15}),
            ),
        ];
        let (cur, earlier) = from_changes(&changes, Some("e"));
        let cur = cur.unwrap();
        assert_eq!(
            (cur.id.as_str(), cur.started_at, cur.ended_at),
            ("e", Some(40), None)
        );
        let ids: Vec<&str> = earlier.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["d", "c", "b", "a"], "newest first");
        assert_eq!(
            why(&earlier[0]),
            "idle 15m, restarted on a fresh conversation"
        );
        assert_eq!(earlier[1].ended.as_deref(), Some("switched"));
        assert_eq!(why(&earlier[2]), "you cleared it with /clear");
        assert_eq!(earlier[3].ended.as_deref(), Some("idle"));
        assert_eq!(
            (earlier[3].started_at, earlier[3].ended_at),
            (None, Some(10))
        );
        assert_eq!(earlier[2].started_at, Some(10));

        // Going back to an earlier one: it is current again, not earlier.
        let mut back = changes.clone();
        back.push(ev(
            50,
            "fresh_conversation",
            json!({"from": "e", "to": "b", "reason": "continued"}),
        ));
        let (cur, earlier) = from_changes(&back, Some("b"));
        assert_eq!(cur.unwrap().id, "b");
        let ids: Vec<&str> = earlier.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["e", "d", "c", "a"]);

        // No events: one conversation, nothing earlier.
        let (cur, earlier) = from_changes(&[], Some("x"));
        assert_eq!(cur.unwrap().id, "x");
        assert!(earlier.is_empty());
        // An agent that assigns its own id (`to` unknown at the restart).
        let (cur, earlier) = from_changes(
            &[ev(
                5,
                "fresh_conversation",
                json!({"from": "a", "to": null, "reason": "idle"}),
            )],
            None,
        );
        assert!(cur.is_none());
        assert_eq!(earlier.len(), 1);
    }

    #[test]
    fn the_bar_says_when_and_how_many() {
        assert_eq!(fresh_label(&json!({"fresh_at": null, "earlier": 0})), None);
        let l = fresh_label(&json!({"fresh_at": now_ms() - 2 * 3_600_000, "earlier": 14})).unwrap();
        assert_eq!(l, "fresh 2h ago · 14 earlier");
    }

    /// "fresh N ago" counts fresh starts only: going back to an earlier
    /// conversation (or a resume) isn't one.
    #[test]
    fn fresh_at_is_the_last_fresh_start() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&rlord_core::db::NewSession {
            id: "01O",
            name: "overlord",
            agent: "claude",
            agent_session: Some("c"),
            project_path: "/d/o",
            tmux_session: Some("overlord-rlord"),
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "overlord",
        })
        .unwrap();
        let add = |ts, kind, p: Value| db.insert_event("01O", ts, kind, &p.to_string()).unwrap();
        add(
            10,
            "fresh_conversation",
            json!({"from": "a", "to": "b", "reason": "idle"}),
        );
        add(
            20,
            "fresh_conversation",
            json!({"from": "b", "to": "x", "reason": "clear"}),
        );
        add(
            30,
            "fresh_conversation",
            json!({"from": "x", "to": "a", "reason": "continued"}),
        );
        add(40, "agent_session_changed", json!({"from": "a", "to": "c"}));
        let o = db.resolve_ref("01O").unwrap();
        assert_eq!(summary(&db, &o)["fresh_at"], 20);
    }

    fn line(ts: &str, role: &str, text: &str) -> String {
        let content = if role == "user" {
            json!(text)
        } else {
            json!([{"type": "text", "text": text}])
        };
        json!({"type": role, "timestamp": ts, "sessionId": "s",
               "message": {"role": role, "content": content, "stop_reason": "end_turn"}})
        .to_string()
    }

    #[test]
    fn a_digest_is_asks_and_last_replies() {
        let dir =
            std::env::temp_dir().join(format!("rlord-recall-{}", rlord_core::types::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("conv.jsonl");
        let mut text = String::new();
        text += &line(
            "2026-11-12T10:00:00.000Z",
            "user",
            "where is the auth session?",
        );
        text.push('\n');
        text += &line("2026-11-12T10:00:02.000Z", "assistant", "looking");
        text.push('\n');
        text += &line(
            "2026-11-12T10:00:05.000Z",
            "assistant",
            "It's auth-fix, in #work.",
        );
        text.push('\n');
        for i in 0..15 {
            text += &line("2026-11-12T11:00:00.000Z", "user", &format!("ask {i}"));
            text.push('\n');
            text += &line(
                "2026-11-12T11:00:01.000Z",
                "assistant",
                &format!("done {i}"),
            );
            text.push('\n');
        }
        std::fs::write(&p, &text).unwrap();
        let d = digest(&p);
        assert_eq!(d.turns, 16);
        assert_eq!(
            d.first_prompt.as_deref(),
            Some("where is the auth session?")
        );
        assert_eq!(d.exchanges.len(), KEEP_EXCHANGES);
        assert_eq!(d.exchanges.last().unwrap().1, "ask 14");
        assert_eq!(d.exchanges.last().unwrap().2.as_deref(), Some("done 14"));
        assert!(d.first_at.unwrap() < d.last_at.unwrap());
        // The turn's last reply, not its first.
        std::fs::write(&p, text.lines().take(3).collect::<Vec<_>>().join("\n")).unwrap();
        let d = digest(&p);
        assert_eq!(d.exchanges.len(), 1);
        assert_eq!(
            d.exchanges[0].2.as_deref(),
            Some("It's auth-fix, in #work.")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
