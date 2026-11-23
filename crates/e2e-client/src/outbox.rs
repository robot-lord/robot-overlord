//! The outbox: requests that change something, kept (and saved, sealed)
//! until the host answers them, so a lost link, a relay restart or an app
//! restart neither loses nor repeats one. Pure data and rules; the machine
//! decides when to send.
//!
//! - Every request that changes something gets an entry with a
//!   device-unique id (16 random bytes, base64url), saved before it is
//!   sent and resent by that id after any reconnect.
//! - Only [`may_wait`] ops are accepted without a live session; they
//!   travel by the mailbox or wait for the host, for up to
//!   [`EXPIRY_MS`] (then `expired`).
//! - Any other entry is resent for [`RESEND_WINDOW_MS`] after it first
//!   left, then ends `interrupted` ("check, then ask anew").
//! - At most [`MAX_ENTRIES`]; the oldest finished-with-an-error entry
//!   makes room, else a new one is refused (`outbox_full`).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// At most this many entries.
pub const MAX_ENTRIES: usize = 16;
/// A sent request that changes something (other than an ask) is resent by
/// id for this long, then ends `interrupted`.
pub const RESEND_WINDOW_MS: u64 = 2 * 60 * 1000;
/// An ask that never got an answer ends `expired` after this long.
pub const EXPIRY_MS: u64 = 24 * 60 * 60 * 1000;

/// The ops that may wait for the host (by mailbox, or until it is back).
pub fn may_wait(op: &str) -> bool {
    matches!(op, "overlord.ask" | "overlord.new_session")
}

/// Look-ups: they run again harmlessly, so they get no entry; one in
/// flight when the link dies fails with `disconnected`. An op this list
/// doesn't know is treated as one that changes something.
pub fn is_lookup(op: &str) -> bool {
    matches!(
        op,
        "snapshot"
            | "session.get"
            | "session.messages"
            | "overlord.ensure"
            | "overlord.messages"
            | "view.default"
            | "form.defaults"
            | "form.repos"
    )
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Not sent yet (no live session; waiting for one or the mailbox).
    Waiting,
    /// Sent (live, or queued at the relay); no answer yet.
    Sent,
    /// The host said `in_progress`: its answer follows.
    Running,
    /// Ended with an error nobody was there to see (shown until
    /// cancelled).
    Failed,
    /// The host can't say whether it happened (`interrupted`), or it
    /// outlived its resend window.
    Interrupted,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    /// The request id (device-unique).
    pub id: String,
    pub op: String,
    pub args: Value,
    /// Made, Unix ms.
    pub at: u64,
    pub state: State,
    /// First sent, Unix ms.
    #[serde(default)]
    pub first_sent: Option<u64>,
    /// The relay `boot` under which the mailbox frame carrying it was
    /// queued: a restart of the relay (another boot) lost it.
    #[serde(default)]
    pub queued_boot: Option<String>,
    #[serde(default)]
    pub error: Option<ErrorInfo>,
}

impl Entry {
    pub fn new(id: String, op: &str, args: Value, at: u64) -> Entry {
        Entry {
            id,
            op: op.to_string(),
            args,
            at,
            state: State::Waiting,
            first_sent: None,
            queued_boot: None,
            error: None,
        }
    }

    /// Still owed an answer.
    pub fn open(&self) -> bool {
        matches!(self.state, State::Waiting | State::Sent | State::Running)
    }

    /// The request as the host reads it.
    pub fn request(&self) -> Value {
        json!({"id": self.id, "op": self.op, "args": self.args})
    }

    fn item(&self) -> Value {
        let mut v = json!({
            "id": self.id,
            "op": self.op,
            "at": self.at,
            "state": self.state,
        });
        if let Some(t) = self.args.get("text").and_then(Value::as_str) {
            v["text"] = json!(t);
        }
        if let Some(e) = &self.error {
            v["error"] = json!({"code": e.code, "message": e.message});
        }
        v
    }
}

/// The outbox is full.
#[derive(Debug, PartialEq, Eq)]
pub struct Full;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Outbox {
    pub entries: Vec<Entry>,
}

impl Outbox {
    /// Adds `e` (oldest first); a full outbox gives up its oldest finished
    /// entry, or refuses.
    pub fn add(&mut self, e: Entry) -> Result<(), Full> {
        if self.entries.len() >= MAX_ENTRIES {
            match self.entries.iter().position(|e| !e.open()) {
                Some(i) => {
                    self.entries.remove(i);
                }
                None => return Err(Full),
            }
        }
        self.entries.push(e);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    pub fn remove(&mut self, id: &str) -> Option<Entry> {
        let i = self.entries.iter().position(|e| e.id == id)?;
        Some(self.entries.remove(i))
    }

    /// The ids still owed an answer, oldest first.
    pub fn open_ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.open())
            .map(|e| e.id.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Ends what has waited too long: an ask after [`EXPIRY_MS`]
    /// (`expired`), anything else [`RESEND_WINDOW_MS`] after it first
    /// left (`interrupted`) unless the host said it is running it on a
    /// session that is still live (`running_live`). The ids ended.
    pub fn expire(&mut self, now: u64, running_live: bool) -> Vec<String> {
        let mut ended = Vec::new();
        for e in self.entries.iter_mut().filter(|e| e.open()) {
            if may_wait(&e.op) {
                if now.saturating_sub(e.at) >= EXPIRY_MS {
                    e.state = State::Failed;
                    e.error = Some(ErrorInfo {
                        code: "expired".into(),
                        message: "no answer from the computer within 24 hours; not sent again"
                            .into(),
                    });
                    ended.push(e.id.clone());
                }
            } else if let Some(t) = e.first_sent {
                let running = running_live && e.state == State::Running;
                if !running && now.saturating_sub(t) >= RESEND_WINDOW_MS {
                    e.state = State::Interrupted;
                    e.error = Some(ErrorInfo {
                        code: "interrupted".into(),
                        message: "no answer from the computer; check whether it happened, \
                                  then send it again"
                            .into(),
                    });
                    ended.push(e.id.clone());
                }
            }
        }
        ended
    }

    /// The page's `outbox` event.
    pub fn event(&self) -> Value {
        json!({
            "event": "outbox",
            "items": self.entries.iter().map(Entry::item).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(id: &str, at: u64) -> Entry {
        Entry::new(id.into(), "overlord.ask", json!({"text": id}), at)
    }

    #[test]
    fn outbox_bounds() {
        let mut o = Outbox::default();
        for i in 0..MAX_ENTRIES {
            o.add(ask(&format!("a{i}"), 0)).unwrap();
        }
        assert_eq!(o.add(ask("x", 0)), Err(Full));
        // A finished one makes room (the oldest such).
        o.get_mut("a3").unwrap().state = State::Failed;
        o.get_mut("a5").unwrap().state = State::Interrupted;
        o.add(ask("y", 0)).unwrap();
        assert!(o.get("a3").is_none() && o.get("a5").is_some());
        assert_eq!(o.len(), MAX_ENTRIES);
        assert_eq!(o.open_ids().len(), MAX_ENTRIES - 1);
    }

    #[test]
    fn asks_expire_after_a_day_others_after_their_window() {
        let mut o = Outbox::default();
        o.add(ask("a", 0)).unwrap();
        let mut r = Entry::new("r".into(), "session.rename", json!({}), 0);
        r.state = State::Sent;
        r.first_sent = Some(1000);
        o.add(r).unwrap();
        let mut run = Entry::new("q".into(), "session.restart", json!({}), 0);
        run.state = State::Running;
        run.first_sent = Some(1000);
        o.add(run).unwrap();
        assert!(o.expire(1000 + RESEND_WINDOW_MS - 1, true).is_empty());
        // Running on a live session: the answer follows; not ended.
        assert_eq!(o.expire(1000 + RESEND_WINDOW_MS, true), ["r"]);
        assert_eq!(o.get("r").unwrap().state, State::Interrupted);
        assert_eq!(o.expire(1000 + RESEND_WINDOW_MS, false), ["q"]);
        assert!(o.expire(EXPIRY_MS - 1, false).is_empty());
        assert_eq!(o.expire(EXPIRY_MS, false), ["a"]);
        let a = o.get("a").unwrap();
        assert_eq!(a.state, State::Failed);
        assert_eq!(a.error.as_ref().unwrap().code, "expired");
        let ev = o.event();
        assert_eq!(ev["items"][0]["text"], "a");
        assert_eq!(ev["items"][0]["state"], "failed");
        assert_eq!(ev["items"][1]["error"]["code"], "interrupted");
    }

    #[test]
    fn only_asks_and_new_sessions_wait() {
        assert!(may_wait("overlord.ask") && may_wait("overlord.new_session"));
        assert!(!may_wait("session.rename") && !may_wait("snapshot"));
        assert!(is_lookup("snapshot") && !is_lookup("session.rename"));
        assert!(!is_lookup("some.new_op"));
    }
}
