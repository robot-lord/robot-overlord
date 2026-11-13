//! rlord's own prompts: telling what rlord typed into a session from what
//! the user typed.
//!
//! rlord types into agent panes (`send::type_into`: `send_to_session`, a
//! held `when_idle` message, a resume after a usage limit, `overlord
//! ask`). The agent then reports a prompt like any other: its
//! UserPromptSubmit hook, OpenCode's events, a turn `discover` finds in a
//! transcript. Those must not count as the user's: `last_user_msg_at` is
//! "when the user last typed" for the list's sort, triage and resume jobs.
//!
//! So right before each paste, `type_into` leaves a **marker** for the
//! session (`mark`): a hash of the text, the time and who sent it, in kv
//! `own_prompts` (a daemon restart keeps it). Every prompt a session
//! reports goes through `prompt`, which looks for its marker:
//!
//! - **by text**, when the report carries it. Claude's and Codex's hooks
//!   give the prompt; `rlord hook emit` sends only its hash
//!   (`slim_hook_payload`: prompts stay out of the daemon). A marker with
//!   the same hash, at most `TEXT_TTL_MS` old, is that prompt. A prompt
//!   with another hash is the user's, however close to a paste.
//! - **by time**, when it doesn't (OpenCode, a turn read from a
//!   transcript, a hook payload that was cut off): a marker from
//!   `SLACK_MS` after the prompt to `TIME_WINDOW_MS` before it.
//!
//! A matched prompt is logged as `rlord_prompt` (with `source`) and moves
//! `last_rlord_msg_at`; any other is `prompt_submitted` and moves
//! `last_user_msg_at`. A marker is used once per way of reporting
//! (`Via`): the same prompt told a second way soon after (a turn read
//! from the transcript, then its hook) is still rlord's, while the next
//! prompt told the same way is the user's. One whose paste failed is
//! taken back (`unmark`); one never matched is dropped after
//! `TEXT_TTL_MS`.

use anyhow::Result;
use serde_json::{json, Map, Value};

use rlord_core::db::Db;

/// kv: pastes whose prompt hasn't been reported yet,
/// `{session id: [{at, source, hash, via?, claimed_at?}]}`. `via`,
/// `claimed_at`: how and when its prompt was first reported (kept
/// `TIME_WINDOW_MS` more, for the same prompt told another way).
pub const KEY: &str = "own_prompts";

/// A marker matches a prompt with the same text this long. (A message
/// typed into a working overlord is queued by its agent, and reported
/// when the turn in front of it ends.)
const TEXT_TTL_MS: i64 = 10 * 60_000;
/// Without the text: a prompt reported this long after a paste is that
/// paste's.
const TIME_WINDOW_MS: i64 = 30_000;
/// Clock slack: a prompt stamped this much before the marker (written
/// right before the paste) is still that paste's.
const SLACK_MS: i64 = 1_000;
/// Markers kept per session (the oldest go first).
const MAX_PER_SESSION: usize = 8;

/// Who typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `send_to_session`, directly or a held `when_idle` message.
    Send,
    /// A resume after a usage limit.
    Resume,
    /// `overlord ask`.
    Ask,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Send => "send",
            Source::Resume => "resume",
            Source::Ask => "ask",
        }
    }
}

/// A hash of a prompt's text, the same for what rlord pasted and what the
/// agent reports of it: whitespace runs count as one space (a paste may
/// change line ends), the ends are trimmed. FNV-1a, written out: the hook
/// client and the daemon must agree across versions.
pub fn text_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: u8| {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for (i, word) in text.split_whitespace().enumerate() {
        if i > 0 {
            eat(b' ');
        }
        word.bytes().for_each(&mut eat);
    }
    format!("{h:016x}")
}

/// How a prompt was reported. One prompt can be told more than one way
/// (a turn `discover` reads from the transcript, then the hook for it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The agent's prompt hook.
    Hook,
    /// OpenCode's event stream.
    Events,
    /// OpenCode's session row, read after events were missed.
    Session,
    /// A turn found open in a transcript (`discover`).
    Transcript,
}

impl Via {
    fn name(self) -> &'static str {
        match self {
            Via::Hook => "hook",
            Via::Events => "events",
            Via::Session => "session",
            Via::Transcript => "transcript",
        }
    }
}

fn load(db: &Db) -> Result<Map<String, Value>> {
    db.kv_map(KEY)
}

fn save(db: &Db, m: &Map<String, Value>) -> Result<()> {
    db.kv_set_map(KEY, m)
}

/// Drop markers too old to match anything (every session's: the map
/// stays small whatever happens to a session).
fn prune(m: &mut Map<String, Value>, now: i64) {
    m.retain(|_, list| {
        let Some(list) = list.as_array_mut() else {
            return false;
        };
        list.retain(|x| {
            let fresh = x["at"].as_i64().is_some_and(|at| now - at <= TEXT_TTL_MS);
            let told = x["claimed_at"]
                .as_i64()
                .is_some_and(|at| now - at > TIME_WINDOW_MS);
            fresh && !told
        });
        !list.is_empty()
    });
}

/// rlord is about to paste `text` into session `id` (`now`: the time).
/// Returns the marker's time, for `unmark`.
pub fn mark(db: &Db, id: &str, text: &str, source: Source, now: i64) -> Result<i64> {
    let mut m = load(db)?;
    prune(&mut m, now);
    let list = m.entry(id.to_string()).or_insert_with(|| json!([]));
    if let Some(list) = list.as_array_mut() {
        // (Never two with one time: `unmark` takes back exactly its own.)
        let at = list
            .iter()
            .filter_map(|x| x["at"].as_i64())
            .max()
            .map_or(now, |last| now.max(last + 1));
        list.push(json!({"at": at, "source": source.name(), "hash": text_hash(text)}));
        if list.len() > MAX_PER_SESSION {
            let extra = list.len() - MAX_PER_SESSION;
            list.drain(..extra);
        }
        save(db, &m)?;
        return Ok(at);
    }
    Ok(now)
}

/// The paste marked at `at` failed: nothing was sent, so no prompt will
/// come of it.
pub fn unmark(db: &Db, id: &str, at: i64) -> Result<()> {
    let mut m = load(db)?;
    let Some(list) = m.get_mut(id).and_then(Value::as_array_mut) else {
        return Ok(());
    };
    list.retain(|x| x["at"].as_i64() != Some(at));
    if list.is_empty() {
        m.remove(id);
    }
    save(db, &m)
}

/// Whether marker `x` is the paste behind a prompt reported at `at`
/// (`hash`: of its text, when the report carried it; `via`: how it was
/// reported).
fn matches(x: &Value, hash: Option<&str>, at: i64, via: Via) -> bool {
    let Some(marked) = x["at"].as_i64() else {
        return false;
    };
    let same_text = hash.is_none_or(|h| x["hash"].as_str() == Some(h));
    // Its prompt was reported already: only the same one told another
    // way, about then, is still it.
    if let Some(told) = x["claimed_at"].as_i64() {
        return same_text
            && x["via"].as_str() != Some(via.name())
            && (at - told).abs() <= TIME_WINDOW_MS;
    }
    let since = at - marked;
    match hash {
        Some(_) => same_text && (-SLACK_MS..=TEXT_TTL_MS).contains(&since),
        None => (-SLACK_MS..=TIME_WINDOW_MS).contains(&since),
    }
}

/// Session `id` reported a prompt at `at`. If rlord typed it, its marker
/// is used up (for this way of reporting) and who sent it is returned.
fn claim(db: &Db, id: &str, hash: Option<&str>, at: i64, via: Via) -> Result<Option<String>> {
    let before = load(db)?;
    let mut m = before.clone();
    let found = m
        .get_mut(id)
        .and_then(Value::as_array_mut)
        .and_then(|list| {
            // (The oldest that fits: pastes are reported in order.)
            let i = list.iter().position(|x| matches(x, hash, at, via))?;
            let source = list[i]["source"].as_str().unwrap_or("send").to_string();
            if list[i]["claimed_at"].is_null() {
                list[i]["via"] = json!(via.name());
                list[i]["claimed_at"] = json!(at);
            } else {
                list.remove(i);
            }
            Some(source)
        });
    // (After the match: what is left and can't match any more goes.)
    prune(&mut m, at);
    if m != before {
        save(db, &m)?;
    }
    Ok(found)
}

/// Record a prompt session `id` reported at `at` (`hash`: of its text,
/// when known; `via`: how it was reported): rlord's own (`last_rlord_msg_at`) or the user's
/// (`last_user_msg_at`). Returns the event to log it as, and its source
/// when it was rlord's.
pub fn prompt(
    db: &Db,
    id: &str,
    hash: Option<&str>,
    at: i64,
    via: Via,
) -> Result<(&'static str, Option<String>)> {
    match claim(db, id, hash, at, via)? {
        Some(source) => {
            db.touch_last_rlord_msg(id, at)?;
            Ok(("rlord_prompt", Some(source)))
        }
        None => {
            db.touch_last_user_msg(id, at)?;
            Ok(("prompt_submitted", None))
        }
    }
}

/// `detail` (an event's JSON), with who sent the prompt when it was
/// rlord's.
pub fn with_source(mut detail: Value, source: Option<String>) -> Value {
    if let Some(source) = source {
        detail["source"] = json!(source);
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&rlord_core::db::NewSession {
            id: "s1",
            name: "web",
            agent: "claude",
            agent_session: None,
            project_path: "/tmp",
            tmux_session: None,
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        db
    }

    fn times(db: &Db) -> (Option<i64>, Option<i64>) {
        let s = db.resolve_ref("s1").unwrap();
        (s.last_user_msg_at, s.last_rlord_msg_at)
    }

    #[test]
    fn the_hash_ignores_how_whitespace_was_pasted() {
        assert_eq!(
            text_hash("fix the\nbuild"),
            text_hash("  fix the\r\n build \n")
        );
        assert_ne!(text_hash("fix the build"), text_hash("fix the builds"));
        assert_ne!(text_hash("ab c"), text_hash("a bc"));
    }

    /// A prompt with the pasted text is rlord's, once; any other text is
    /// the user's, however soon after the paste.
    #[test]
    fn a_prompt_is_matched_by_its_text() {
        let db = db();
        mark(&db, "s1", "continue", Source::Resume, 10_000).unwrap();
        // The user got in first with something else.
        let user = text_hash("wait, stop");
        let (kind, source) = prompt(&db, "s1", Some(&user), 10_200, Via::Hook).unwrap();
        assert_eq!((kind, source), ("prompt_submitted", None));
        assert_eq!(times(&db), (Some(10_200), None));
        // rlord's own, minutes later (it sat in the agent's queue).
        let own = text_hash("continue\n");
        let (kind, source) = prompt(&db, "s1", Some(&own), 200_000, Via::Hook).unwrap();
        assert_eq!((kind, source.as_deref()), ("rlord_prompt", Some("resume")));
        assert_eq!(times(&db), (Some(10_200), Some(200_000)));
        // The marker is used up: the same words again are the user's.
        let (kind, _) = prompt(&db, "s1", Some(&own), 201_000, Via::Hook).unwrap();
        assert_eq!(kind, "prompt_submitted");
    }

    /// One prompt told two ways (a turn read from the transcript, then
    /// its hook) is rlord's both times; the user's next one is theirs.
    #[test]
    fn a_prompt_reported_twice_is_still_rlords() {
        let db = db();
        mark(&db, "s1", "continue", Source::Resume, 10_000).unwrap();
        let own = text_hash("continue");
        let (kind, _) = prompt(&db, "s1", None, 12_000, Via::Transcript).unwrap();
        assert_eq!(kind, "rlord_prompt");
        // The same way again is another prompt: the user's.
        let (kind, _) = prompt(&db, "s1", None, 13_000, Via::Transcript).unwrap();
        assert_eq!(kind, "prompt_submitted");
        // Other text by hook: the user's too.
        let other = text_hash("stop");
        let (kind, _) = prompt(&db, "s1", Some(&other), 13_500, Via::Hook).unwrap();
        assert_eq!(kind, "prompt_submitted");
        // The paste's own hook, late: rlord's, and the marker is gone.
        let (kind, source) = prompt(&db, "s1", Some(&own), 14_000, Via::Hook).unwrap();
        assert_eq!((kind, source.as_deref()), ("rlord_prompt", Some("resume")));
        assert_eq!(db.kv_get(KEY).unwrap(), None);
        assert_eq!(times(&db), (Some(13_500), Some(14_000)));
        // A reported marker doesn't linger: the next report clears it.
        mark(&db, "s1", "continue", Source::Resume, 20_000).unwrap();
        prompt(&db, "s1", Some(&own), 20_100, Via::Hook).unwrap();
        let late = 20_100 + TIME_WINDOW_MS + 1;
        let (kind, _) = prompt(&db, "s1", None, late, Via::Transcript).unwrap();
        assert_eq!(kind, "prompt_submitted");
        assert_eq!(db.kv_get(KEY).unwrap(), None);
    }

    /// Without the text, the first prompt soon after a paste is rlord's.
    #[test]
    fn a_prompt_without_text_is_matched_by_time() {
        let db = db();
        mark(&db, "s1", "hi", Source::Send, 10_000).unwrap();
        // Stamped just before the marker: clock slack.
        let (kind, source) = prompt(&db, "s1", None, 9_500, Via::Hook).unwrap();
        assert_eq!((kind, source.as_deref()), ("rlord_prompt", Some("send")));
        // The next one is the user's, even inside the window.
        assert_eq!(
            prompt(&db, "s1", None, 12_000, Via::Hook).unwrap().0,
            "prompt_submitted"
        );
        // Too long after the paste: not its prompt.
        mark(&db, "s1", "hi", Source::Send, 20_000).unwrap();
        let late = 20_000 + TIME_WINDOW_MS + 1;
        assert_eq!(
            prompt(&db, "s1", None, late, Via::Hook).unwrap().0,
            "prompt_submitted"
        );
        // A prompt from before the paste isn't it either.
        assert_eq!(
            prompt(&db, "s1", None, 20_000 - SLACK_MS - 1, Via::Hook)
                .unwrap()
                .0,
            "prompt_submitted"
        );
        // Another session's marker is not this one's.
        assert_eq!(
            prompt(&db, "s2", None, 20_100, Via::Hook).unwrap().0,
            "prompt_submitted"
        );
    }

    /// A failed paste takes its marker back; one never matched expires.
    #[test]
    fn markers_go_away() {
        let db = db();
        let first = mark(&db, "s1", "one", Source::Ask, 10_000).unwrap();
        let second = mark(&db, "s1", "two", Source::Ask, 10_000).unwrap();
        assert_ne!(first, second);
        unmark(&db, "s1", second).unwrap();
        let two = text_hash("two");
        assert_eq!(
            prompt(&db, "s1", Some(&two), 10_500, Via::Hook).unwrap().0,
            "prompt_submitted"
        );
        let one = text_hash("one");
        let late = 10_000 + TEXT_TTL_MS + 1;
        assert_eq!(
            prompt(&db, "s1", Some(&one), late, Via::Hook).unwrap().0,
            "prompt_submitted"
        );
        // The next paste (anywhere) clears what expired.
        mark(&db, "s2", "x", Source::Send, late).unwrap();
        let m = load(&db).unwrap();
        assert!(!m.contains_key("s1") && m.contains_key("s2"));
        // Never more than a few per session.
        for i in 0..20 {
            mark(&db, "s1", "x", Source::Send, late + i).unwrap();
        }
        assert_eq!(
            load(&db).unwrap()["s1"].as_array().unwrap().len(),
            MAX_PER_SESSION
        );
    }
}
