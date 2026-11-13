//! rlord's own prompts: telling what rlord typed into a session from what
//! the user typed.
//!
//! rlord types its own messages into agent panes (`send::type_into`:
//! `send_to_session`, a held `when_idle` message, a resume after a usage
//! limit). The agent then reports a prompt like any other: its
//! UserPromptSubmit hook, OpenCode's events, a turn `discover` finds in a
//! transcript. Those must not count as the user's: `last_user_msg_at` is
//! "when the user last typed" for the list's "last prompt" sort and for
//! resume jobs. (`overlord ask` goes through `type_into` too, but carries
//! what the user typed: it is not marked, and is the user's prompt.)
//!
//! So right before each paste, `type_into` leaves a **marker** for the
//! session (`mark`): a hash of the text, the time and who sent it, in kv
//! `own_prompts` (a daemon restart keeps it). Every prompt a session
//! reports goes through `prompt`, which looks for its marker:
//!
//! - **by text**, when the report carries it: Claude's and Codex's hooks
//!   (`rlord hook emit` sends only what identifies the text,
//!   `PromptMark`: prompts stay out of the daemon) and OpenCode's
//!   `session.inbox.enqueued` event. A marker with the same hash, at most
//!   `TEXT_TTL_MS` old, is that prompt. A prompt with another hash is the
//!   user's, however close to a paste, with one exception: where the
//!   agent may have changed the text on the way (`PromptMark::pasted`, or
//!   a long paste of rlord's), a prompt within `PASTE_WINDOW_MS` of the
//!   paste is taken as it.
//! - **by time**, when it doesn't (OpenCode's session row, a turn read
//!   from a transcript, a hook payload that was cut off): a marker from
//!   `SLACK_MS` after the prompt to `TIME_WINDOW_MS` before it.
//!
//! A matched prompt is logged as `rlord_prompt` (with `source`) and moves
//! `last_rlord_msg_at`; any other is `prompt_submitted` and moves
//! `last_user_msg_at`. A marker is used once per way of reporting
//! (`Via`): the same prompt told a second way (a turn read from the
//! transcript, then its hook) within `SAME_TURN_MS` of the first is
//! still rlord's; any later prompt, or one told the same way, is the
//! user's. One whose paste failed is taken back (`unmark`); one never
//! matched is dropped after `TEXT_TTL_MS`.

use anyhow::Result;
use serde_json::{json, Map, Value};

use rlord_core::db::Db;
use rlord_core::types::PromptMark;

/// kv: pastes whose prompt hasn't been reported yet,
/// `{session id: [{at, source, hash, chars, via?, claimed_at?}]}`. `via`,
/// `claimed_at`: how and when its prompt was first reported (kept
/// `SAME_TURN_MS` more, for the same prompt told another way).
pub const KEY: &str = "own_prompts";

/// A marker matches a prompt with the same text this long. (A message an
/// agent queues behind a running turn is reported when that turn ends.)
const TEXT_TTL_MS: i64 = 10 * 60_000;
/// Without the text: a prompt reported this long after a paste is that
/// paste's.
const TIME_WINDOW_MS: i64 = 30_000;
/// With a text that may have been changed on the way (see the module
/// doc): a prompt this soon after the paste is that paste's.
const PASTE_WINDOW_MS: i64 = 5_000;
/// Two reports of one prompt (its hook, and the turn read from its
/// transcript) are at most this far apart.
const SAME_TURN_MS: i64 = 5_000;
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
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Send => "send",
            Source::Resume => "resume",
        }
    }
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

/// What a hook payload says of its prompt's text (`slim_hook_payload`'s
/// `prompt_hash` and `prompt_pasted`).
pub fn mark_of_payload(payload: &Value) -> Option<PromptMark> {
    Some(PromptMark {
        hash: payload["prompt_hash"].as_str()?.to_string(),
        pasted: payload["prompt_pasted"] == true,
    })
}

/// Drop markers that can't match anything any more (every session's: the
/// map stays small whatever happens to a session). Returns whether any
/// went.
fn prune(m: &mut Map<String, Value>, now: i64) -> bool {
    let mut dropped = false;
    m.retain(|_, list| {
        let Some(list) = list.as_array_mut() else {
            dropped = true;
            return false;
        };
        let before = list.len();
        list.retain(|x| {
            let fresh = x["at"].as_i64().is_some_and(|at| now - at <= TEXT_TTL_MS);
            let told = x["claimed_at"]
                .as_i64()
                .is_some_and(|at| now - at > SAME_TURN_MS);
            fresh && !told
        });
        dropped |= list.len() != before;
        !list.is_empty()
    });
    dropped
}

/// rlord is about to paste `text` into session `id` (`now`: the time).
/// Returns the marker's time, for `unmark`.
pub fn mark(db: &Db, id: &str, text: &str, source: Source, now: i64) -> Result<i64> {
    let mut m = db.kv_map(KEY)?;
    prune(&mut m, now);
    let list = m.entry(id.to_string()).or_insert_with(|| json!([]));
    if let Some(list) = list.as_array_mut() {
        // (Never two with one time: `unmark` takes back exactly its own.)
        let at = list
            .iter()
            .filter_map(|x| x["at"].as_i64())
            .max()
            .map_or(now, |last| now.max(last + 1));
        list.push(json!({
            "at": at, "source": source.name(), "hash": PromptMark::hash_of(text),
            "chars": text.chars().count(),
        }));
        if list.len() > MAX_PER_SESSION {
            let extra = list.len() - MAX_PER_SESSION;
            list.drain(..extra);
        }
        db.kv_set_map(KEY, &m)?;
        return Ok(at);
    }
    Ok(now)
}

/// The paste marked at `at` failed: nothing was sent, so no prompt will
/// come of it.
pub fn unmark(db: &Db, id: &str, at: i64) -> Result<()> {
    let mut m = db.kv_map(KEY)?;
    let Some(list) = m.get_mut(id).and_then(Value::as_array_mut) else {
        return Ok(());
    };
    list.retain(|x| x["at"].as_i64() != Some(at));
    if list.is_empty() {
        m.remove(id);
    }
    db.kv_set_map(KEY, &m)
}

/// Whether marker `x` is the paste behind a prompt reported at `at`
/// (`text`: what identifies its text, when the report carried it; `via`:
/// how it was reported).
fn matches(x: &Value, text: Option<&PromptMark>, at: i64, via: Via) -> bool {
    let Some(marked) = x["at"].as_i64() else {
        return false;
    };
    let same_text = text.is_some_and(|t| x["hash"].as_str() == Some(t.hash.as_str()));
    // Its prompt was reported already: only the same one told another
    // way, at about that time, is still it.
    if let Some(told) = x["claimed_at"].as_i64() {
        return (text.is_none() || same_text)
            && x["via"].as_str() != Some(via.name())
            && (at - told).abs() <= SAME_TURN_MS;
    }
    let since = at - marked;
    match text {
        Some(_) if same_text => (-SLACK_MS..=TEXT_TTL_MS).contains(&since),
        // Another text. Only where the agent may have changed rlord's on
        // the way, and only right after the paste.
        Some(t) => {
            let long = x["chars"]
                .as_u64()
                .is_some_and(|n| n >= PromptMark::LONG_CHARS as u64);
            (t.pasted || long) && (-SLACK_MS..=PASTE_WINDOW_MS).contains(&since)
        }
        None => (-SLACK_MS..=TIME_WINDOW_MS).contains(&since),
    }
}

/// Session `id` reported a prompt at `at`. If rlord typed it, its marker
/// is used up (for this way of reporting) and who sent it is returned.
fn claim(
    db: &Db,
    id: &str,
    text: Option<&PromptMark>,
    at: i64,
    via: Via,
) -> Result<Option<String>> {
    let mut m = db.kv_map(KEY)?;
    let found = m
        .get_mut(id)
        .and_then(Value::as_array_mut)
        .and_then(|list| {
            // (The oldest that fits: pastes are reported in order.)
            let i = list.iter().position(|x| matches(x, text, at, via))?;
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
    let dirty = prune(&mut m, at) | found.is_some();
    if dirty {
        db.kv_set_map(KEY, &m)?;
    }
    Ok(found)
}

/// Record a prompt session `id` reported at `at` (`text`: what identifies
/// its text, when known; `via`: how it was reported): rlord's own
/// (`last_rlord_msg_at`) or the user's (`last_user_msg_at`). Returns the
/// event to log it as, and its source when it was rlord's.
pub fn prompt(
    db: &Db,
    id: &str,
    text: Option<&PromptMark>,
    at: i64,
    via: Via,
) -> Result<(&'static str, Option<String>)> {
    match claim(db, id, text, at, via)? {
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
            PromptMark::of("fix the\nbuild"),
            PromptMark::of("  fix the\r\n build \n")
        );
        assert_ne!(
            PromptMark::of("fix the build"),
            PromptMark::of("fix the builds")
        );
        assert_ne!(PromptMark::of("ab c"), PromptMark::of("a bc"));
    }

    /// A prompt with the pasted text is rlord's, once; any other text is
    /// the user's, however soon after the paste.
    #[test]
    fn a_prompt_is_matched_by_its_text() {
        let db = db();
        mark(&db, "s1", "continue", Source::Resume, 10_000).unwrap();
        // The user got in first with something else.
        let user = PromptMark::of("wait, stop");
        let (kind, source) = prompt(&db, "s1", Some(&user), 10_200, Via::Hook).unwrap();
        assert_eq!((kind, source), ("prompt_submitted", None));
        assert_eq!(times(&db), (Some(10_200), None));
        // rlord's own, minutes later (it sat in the agent's queue).
        let own = PromptMark::of("continue\n");
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
        let own = PromptMark::of("continue");
        let (kind, _) = prompt(&db, "s1", None, 12_000, Via::Transcript).unwrap();
        assert_eq!(kind, "rlord_prompt");
        // The same way again is another prompt: the user's.
        let (kind, _) = prompt(&db, "s1", None, 13_000, Via::Transcript).unwrap();
        assert_eq!(kind, "prompt_submitted");
        // Other text by hook: the user's too.
        let other = PromptMark::of("stop");
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

    /// The user's quick follow-up after rlord's prompt was reported is
    /// theirs, whichever way it is told.
    #[test]
    fn a_quick_follow_up_is_the_users() {
        let db = db();
        mark(&db, "s1", "continue", Source::Resume, 10_000).unwrap();
        let (kind, _) = prompt(&db, "s1", None, 12_000, Via::Transcript).unwrap();
        assert_eq!(kind, "rlord_prompt");
        // Well inside 30s of the paste, but not the same turn's report.
        let next = 12_000 + SAME_TURN_MS + 1;
        let (kind, _) = prompt(&db, "s1", None, next, Via::Hook).unwrap();
        assert_eq!(kind, "prompt_submitted");
        // Even with the very same words.
        mark(&db, "s1", "continue", Source::Resume, 50_000).unwrap();
        let own = PromptMark::of("continue");
        prompt(&db, "s1", Some(&own), 50_500, Via::Events).unwrap();
        let (kind, _) = prompt(&db, "s1", Some(&own), 58_000, Via::Hook).unwrap();
        assert_eq!(kind, "prompt_submitted");
        assert_eq!(times(&db), (Some(58_000), Some(50_500)));
    }

    /// A text the agent may have changed on the way (a paste placeholder,
    /// a long paste) still matches, but only right after the paste.
    #[test]
    fn a_changed_paste_is_matched_right_after_it() {
        let db = db();
        let long = "word ".repeat(400);
        let other = |text: &str| PromptMark::of(text);
        // A long paste of rlord's, reported with another text.
        mark(&db, "s1", &long, Source::Send, 10_000).unwrap();
        let cut = other("word word");
        let (kind, _) = prompt(&db, "s1", Some(&cut), 12_000, Via::Hook).unwrap();
        assert_eq!(kind, "rlord_prompt");
        // Not later than a few seconds.
        mark(&db, "s1", &long, Source::Send, 100_000).unwrap();
        let late = 100_000 + PASTE_WINDOW_MS + 1;
        let (kind, _) = prompt(&db, "s1", Some(&cut), late, Via::Hook).unwrap();
        assert_eq!(kind, "prompt_submitted");
        // A short paste: another short text right after is the user's...
        mark(&db, "s1", "continue", Source::Resume, 200_000).unwrap();
        let (kind, _) = prompt(&db, "s1", Some(&other("stop")), 200_500, Via::Hook).unwrap();
        assert_eq!(kind, "prompt_submitted");
        // ...a placeholder in it is the paste.
        let placeholder = other("[Pasted text #1 +3 lines]");
        assert!(placeholder.pasted);
        let (kind, _) = prompt(&db, "s1", Some(&placeholder), 201_000, Via::Hook).unwrap();
        assert_eq!(kind, "rlord_prompt");
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
        let first = mark(&db, "s1", "one", Source::Send, 10_000).unwrap();
        let second = mark(&db, "s1", "two", Source::Send, 10_000).unwrap();
        assert_ne!(first, second);
        unmark(&db, "s1", second).unwrap();
        let two = PromptMark::of("two");
        assert_eq!(
            prompt(&db, "s1", Some(&two), 10_500, Via::Hook).unwrap().0,
            "prompt_submitted"
        );
        let one = PromptMark::of("one");
        let late = 10_000 + TEXT_TTL_MS + 1;
        assert_eq!(
            prompt(&db, "s1", Some(&one), late, Via::Hook).unwrap().0,
            "prompt_submitted"
        );
        // The next paste (anywhere) clears what expired.
        mark(&db, "s2", "x", Source::Send, late).unwrap();
        let m = db.kv_map(KEY).unwrap();
        assert!(!m.contains_key("s1") && m.contains_key("s2"));
        // Never more than a few per session.
        for i in 0..20 {
            mark(&db, "s1", "x", Source::Send, late + i).unwrap();
        }
        assert_eq!(
            db.kv_map(KEY).unwrap()["s1"].as_array().unwrap().len(),
            MAX_PER_SESSION
        );
    }
}
