//! Requests by mailbox: when a device finds its host offline (or has no
//! live session), it may queue `overlord.ask` or `overlord.new_session` at
//! the relay; the host answers the same way
//! (`docs/working/e2e-protocol.md`, "Requests by mailbox").
//!
//! Per device, in `remote/mailbox-<id>.json`, in one atomic write: the
//! exported mailbox session, the last 64 request ids, and the relay boot
//! and queue number of the last frame handled. Order per request: decrypt;
//! record its id and the frame's number and save with the advanced state;
//! then acknowledge the relay and act; then encrypt the answer, save, and
//! send it. A crash before the save leaves the frame at the relay and the
//! old state on disk, so it is handled again as if new; a crash after it
//! is recognised when the relay delivers the frame again (by its number),
//! never repeats the op (its id is recorded), and the device's resend is
//! answered `repeat`.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use rlord_core::private;
use rlord_e2e::stream::{self, CONTROL};
use rlord_e2e::MailboxSession;
use rlord_relay_proto::frame::FromHost;
use rlord_relay_proto::{ToRelay, B64};

use super::session::error_answer;
use super::{Actor, Input};

/// Ops a device may send by mailbox.
pub const MAILBOX_OPS: &[&str] = &["overlord.ask", "overlord.new_session"];
/// Request ids remembered per device.
const REMEMBER: usize = 64;

/// A device's mailbox and what has been handled through it, saved
/// together in one file so a crash never leaves them out of step.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    /// `MailboxSession::export`, base64url; None after a failure.
    #[serde(default)]
    state: Option<String>,
    /// The last request ids acted on (or refused).
    #[serde(default)]
    ids: VecDeque<String>,
    /// The relay boot and queue number of the last frame handled: a frame
    /// the relay delivers again (we crashed before acknowledging it) is
    /// recognised and not decrypted twice.
    #[serde(default)]
    boot: String,
    #[serde(default)]
    seq: u64,
}

struct Entry {
    session: Option<MailboxSession>,
    saved: Saved,
}

pub(crate) struct Mailboxes {
    dir: PathBuf,
    cache: HashMap<String, Entry>,
}

impl Mailboxes {
    pub fn new(dir: PathBuf) -> Mailboxes {
        Mailboxes {
            dir,
            cache: HashMap::new(),
        }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("mailbox-{id}.json"))
    }

    fn entry(&mut self, id: &str) -> &mut Entry {
        if !self.cache.contains_key(id) {
            let saved: Saved = std::fs::read(self.path(id))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            let session = saved
                .state
                .as_deref()
                .and_then(super::session::unb64)
                .and_then(|b| MailboxSession::import(&b).ok());
            self.cache.insert(id.to_string(), Entry { session, saved });
        }
        self.cache.get_mut(id).expect("just inserted")
    }

    /// The device's mailbox, loaded on first use.
    pub fn get(&mut self, id: &str) -> Option<&mut MailboxSession> {
        self.entry(id).session.as_mut()
    }

    /// A newly keyed mailbox replaces the device's old one.
    pub fn install(&mut self, id: &str, m: MailboxSession) -> Result<()> {
        self.entry(id).session = Some(m);
        self.save(id)
    }

    /// Save the device's mailbox and what was handled, as they are in
    /// memory now (one atomic write).
    pub fn save(&mut self, id: &str) -> Result<()> {
        let dir = self.dir.clone();
        let path = self.path(id);
        let e = self.entry(id);
        e.saved.state = match &e.session {
            Some(m) => Some(super::session::b64(
                &m.export().map_err(|e| anyhow::anyhow!("{e}"))?,
            )),
            None => None,
        };
        let bytes = serde_json::to_vec(&e.saved)?;
        private::dir(&dir)?;
        private::write(&path, &bytes)?;
        Ok(())
    }

    /// The mailbox failed (a gap, a replay, tampering): gone on this end;
    /// the device's next live session keys a new one. The request ids stay.
    pub fn discard(&mut self, id: &str) -> Result<()> {
        self.entry(id).session = None;
        self.save(id)
    }

    /// The device was revoked.
    pub fn forget(&mut self, id: &str) {
        self.cache.remove(id);
        let _ = std::fs::remove_file(self.path(id));
    }

    /// Was the frame `(boot, seq)` handled already?
    pub fn handled(&mut self, id: &str, boot: &str, seq: u64) -> bool {
        let s = &self.entry(id).saved;
        !boot.is_empty() && s.boot == boot && seq <= s.seq
    }

    /// Note the frame `(boot, seq)` as handled (saved with the next save).
    pub fn mark(&mut self, id: &str, boot: &str, seq: u64) {
        let s = &mut self.entry(id).saved;
        s.boot = boot.to_string();
        s.seq = seq;
    }

    pub fn seen(&mut self, id: &str, rid: &str) -> bool {
        self.entry(id).saved.ids.iter().any(|r| r == rid)
    }

    /// Note a request id (saved with the next save).
    pub fn record(&mut self, id: &str, rid: &str) {
        let ids = &mut self.entry(id).saved.ids;
        ids.push_back(rid.to_string());
        while ids.len() > REMEMBER {
            ids.pop_front();
        }
    }
}

/// For the crash tests only (debug builds): exit at the named point when
/// `RLORD_WEB_TEST_CRASH` names it.
fn crash_point(name: &str) {
    if cfg!(debug_assertions) && std::env::var("RLORD_WEB_TEST_CRASH").is_ok_and(|v| v == name) {
        eprintln!("rlord-web: crashing at {name} (RLORD_WEB_TEST_CRASH)");
        std::process::exit(9);
    }
}

/// A request's id as text (a device may send a number or a string).
fn request_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() && s.len() <= 128 => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl Actor {
    /// A lane-2 frame from a device, queued by the relay.
    pub(crate) fn mailbox_frame(&mut self, fp: [u8; 32], seq: u64, payload: &[u8]) {
        let ack = |a: &mut Actor| {
            a.send_relay(|n| ToRelay::Ack {
                n,
                seq,
                device: Some(B64(fp.to_vec())),
            })
        };
        let Some(device) = self.devices.by_fp(&fp).map(|d| d.id.clone()) else {
            return ack(self);
        };
        let boot = self.boot.clone();
        // Delivered again after a crash before our acknowledgement: handled.
        if self.mailboxes.handled(&device, &boot, seq) {
            return ack(self);
        }
        let Some(m) = self.mailboxes.get(&device) else {
            return ack(self);
        };
        // A frame for a mailbox since replaced: dropped, not an error.
        if payload.len() < 8 || payload[..8] != m.id() {
            return ack(self);
        }
        let plain = match m.decrypt(&payload[8..]) {
            Ok(p) => p,
            Err(e) => {
                if let Err(e) = self.mailboxes.discard(&device) {
                    eprintln!("rlord-web: saving a mailbox: {e:#}");
                }
                self.audit.log(&device, &format!("mailbox discarded ({e})"));
                return ack(self);
            }
        };
        crash_point("mailbox_decrypted");
        self.mailboxes.mark(&device, &boot, seq);
        let req = stream::single(&plain)
            .ok()
            .filter(|m| m.stream == CONTROL)
            .and_then(|m| serde_json::from_slice::<Value>(&m.body).ok());
        let rid = req.as_ref().map(|r| r["id"].clone()).unwrap_or(Value::Null);
        let refusal = match &req {
            None => Some(error_answer(&Value::Null, "bad_request", "not JSON")),
            Some(r) => match request_id(&rid) {
                None => Some(error_answer(&rid, "bad_request", "no id")),
                Some(t) if self.mailboxes.seen(&device, &t) => Some(error_answer(
                    &rid,
                    "repeat",
                    "this request was received already",
                )),
                Some(t) => {
                    self.mailboxes.record(&device, &t);
                    let op = r["op"].as_str().unwrap_or("");
                    (!MAILBOX_OPS.contains(&op)).then(|| {
                        error_answer(
                            &rid,
                            "not_allowed",
                            "only overlord.ask and overlord.new_session travel by mailbox",
                        )
                    })
                }
            },
        };
        // The advanced state, the request id and this frame's number, in
        // one write, before acting: never done twice.
        if let Err(e) = self.mailboxes.save(&device) {
            eprintln!("rlord-web: saving a mailbox: {e:#}");
            return;
        }
        crash_point("mailbox_saved");
        ack(self);
        if let Some(answer) = refusal {
            return self.mailbox_answer(&device, answer);
        }
        let req = req.unwrap_or_default();
        let op = req["op"].as_str().unwrap_or("").to_string();
        self.audit.log(&device, &format!("mailbox op {op}"));
        let args = req.get("args").cloned().unwrap_or(json!({}));
        let (d, tx) = (self.d.clone(), self.tx_clone());
        tokio::spawn(async move {
            let r = d
                .call(
                    "web.call",
                    json!({"op": op, "args": args, "via": "remote", "device": device}),
                )
                .await;
            let answer = match r {
                Ok(v) => json!({"id": rid, "ok": v}),
                Err(e) => error_answer(&rid, "failed", &e.message),
            };
            let _ = tx.send(Input::MailboxDone { device, answer });
        });
    }

    pub(crate) fn mailbox_done(&mut self, device: &str, answer: Value) {
        self.mailbox_answer(device, answer);
    }

    /// Encrypt an answer into the device's mailbox, save, send.
    fn mailbox_answer(&mut self, device: &str, answer: Value) {
        let Some(fp) = self.devices.by_id(device).and_then(|d| d.relay_fp()) else {
            return;
        };
        let mut body = answer.to_string().into_bytes();
        if body.len() > stream::CHUNK {
            body = error_answer(
                &answer["id"],
                "too_large",
                "the answer is too large for the mailbox",
            )
            .to_string()
            .into_bytes();
        }
        let Some(m) = self.mailboxes.get(device) else {
            return;
        };
        let id = m.id();
        let frame = match stream::fragment(CONTROL, &body).map(|f| m.encrypt(&f[0])) {
            Ok(Ok(f)) => f,
            _ => return,
        };
        // Saved before it leaves.
        if let Err(e) = self.mailboxes.save(device) {
            eprintln!("rlord-web: saving a mailbox: {e:#}");
            return;
        }
        let mut payload = id.to_vec();
        payload.extend_from_slice(&frame);
        self.send_frame(FromHost::Mailbox {
            device: fp,
            payload,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_and_frames_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Mailboxes::new(tmp.path().to_path_buf());
        assert!(!m.seen("d", "r1"));
        m.record("d", "r1");
        m.mark("d", "b1", 7);
        m.save("d").unwrap();
        let mut m = Mailboxes::new(tmp.path().to_path_buf());
        assert!(m.seen("d", "r1"));
        assert!(m.handled("d", "b1", 7) && m.handled("d", "b1", 3));
        assert!(!m.handled("d", "b1", 8) && !m.handled("d", "b2", 1));
        for i in 0..REMEMBER {
            m.record("d", &format!("x{i}"));
        }
        assert!(!m.seen("d", "r1"));
        m.forget("d");
        assert!(!tmp.path().join("mailbox-d.json").exists());
        assert_eq!(request_id(&json!(5)), Some("5".into()));
        assert_eq!(request_id(&json!("")), None);
        assert_eq!(request_id(&json!(null)), None);
    }
}
