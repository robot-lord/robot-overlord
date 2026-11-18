//! Requests by mailbox: when a device finds its host offline (or has no
//! live session), it may queue `overlord.ask` or `overlord.new_session` at
//! the relay; the host answers the same way
//! (`docs/working/e2e-protocol.md`, "Requests by mailbox").
//!
//! Per device, in `remote/`: `mailbox-<id>.bin` (the exported mailbox
//! session) and `requests-<id>.json` (the last 64 request ids). Order per
//! request: decrypt; record its id and save the advanced state; then
//! acknowledge the relay and act; then encrypt the answer, save, and send
//! it. A crash before the save leaves the frame at the relay and the old
//! state on disk, so it is simply handled again; a crash after it can't
//! repeat the op (its id is recorded) and the device's resend is answered
//! `repeat`.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use anyhow::Result;
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

pub(crate) struct Mailboxes {
    dir: PathBuf,
    cache: HashMap<String, MailboxSession>,
    requests: HashMap<String, VecDeque<String>>,
}

impl Mailboxes {
    pub fn new(dir: PathBuf) -> Mailboxes {
        Mailboxes {
            dir,
            cache: HashMap::new(),
            requests: HashMap::new(),
        }
    }

    fn state_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("mailbox-{id}.bin"))
    }

    fn requests_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("requests-{id}.json"))
    }

    /// The device's mailbox, loaded on first use.
    pub fn get(&mut self, id: &str) -> Option<&mut MailboxSession> {
        if !self.cache.contains_key(id) {
            let b = std::fs::read(self.state_path(id)).ok()?;
            let m = MailboxSession::import(&b).ok()?;
            self.cache.insert(id.to_string(), m);
        }
        self.cache.get_mut(id)
    }

    /// A newly keyed mailbox replaces the device's old one.
    pub fn install(&mut self, id: &str, m: MailboxSession) -> Result<()> {
        self.cache.insert(id.to_string(), m);
        self.save(id)
    }

    /// Save the device's mailbox as it is in memory now.
    pub fn save(&mut self, id: &str) -> Result<()> {
        let Some(m) = self.cache.get(id) else {
            return Ok(());
        };
        let bytes = m.export().map_err(|e| anyhow::anyhow!("{e}"))?;
        private::dir(&self.dir)?;
        private::write(&self.state_path(id), &bytes)?;
        Ok(())
    }

    /// The mailbox failed (a gap, a replay, tampering): gone on this end;
    /// the device's next live session keys a new one.
    pub fn discard(&mut self, id: &str) {
        self.cache.remove(id);
        let _ = std::fs::remove_file(self.state_path(id));
    }

    /// The device was revoked.
    pub fn forget(&mut self, id: &str) {
        self.discard(id);
        self.requests.remove(id);
        let _ = std::fs::remove_file(self.requests_path(id));
    }

    fn ids(&mut self, id: &str) -> &mut VecDeque<String> {
        if !self.requests.contains_key(id) {
            let v: VecDeque<String> = std::fs::read(self.requests_path(id))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            self.requests.insert(id.to_string(), v);
        }
        self.requests.get_mut(id).expect("just inserted")
    }

    pub fn seen(&mut self, id: &str, rid: &str) -> bool {
        self.ids(id).iter().any(|r| r == rid)
    }

    pub fn record(&mut self, id: &str, rid: &str) -> Result<()> {
        let ids = self.ids(id);
        ids.push_back(rid.to_string());
        while ids.len() > REMEMBER {
            ids.pop_front();
        }
        let bytes = serde_json::to_vec(&*ids)?;
        private::dir(&self.dir)?;
        private::write(&self.requests_path(id), &bytes)?;
        Ok(())
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
                self.mailboxes.discard(&device);
                self.audit.log(&device, &format!("mailbox discarded ({e})"));
                return ack(self);
            }
        };
        let req = stream::single(&plain)
            .ok()
            .filter(|m| m.stream == CONTROL)
            .and_then(|m| serde_json::from_slice::<Value>(&m.body).ok());
        let Some(req) = req else {
            let _ = self.mailboxes.save(&device);
            ack(self);
            return self.mailbox_answer(
                &device,
                error_answer(&Value::Null, "bad_request", "not JSON"),
            );
        };
        let rid = req["id"].clone();
        let Some(rid_text) = request_id(&rid) else {
            let _ = self.mailboxes.save(&device);
            ack(self);
            return self.mailbox_answer(&device, error_answer(&rid, "bad_request", "no id"));
        };
        let op = req["op"].as_str().unwrap_or("").to_string();
        if self.mailboxes.seen(&device, &rid_text) {
            let _ = self.mailboxes.save(&device);
            ack(self);
            return self.mailbox_answer(
                &device,
                error_answer(&rid, "repeat", "this request was received already"),
            );
        }
        if !MAILBOX_OPS.contains(&op.as_str()) {
            let _ = self.mailboxes.save(&device);
            ack(self);
            return self.mailbox_answer(
                &device,
                error_answer(
                    &rid,
                    "not_allowed",
                    "only overlord.ask and overlord.new_session travel by mailbox",
                ),
            );
        }
        // Recorded and saved before acting: never done twice.
        if let Err(e) = self
            .mailboxes
            .record(&device, &rid_text)
            .and_then(|_| self.mailboxes.save(&device))
        {
            eprintln!("rlord-web: saving a mailbox: {e:#}");
            return;
        }
        ack(self);
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
    fn request_ids_persist_and_roll() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Mailboxes::new(tmp.path().to_path_buf());
        assert!(!m.seen("d", "r1"));
        m.record("d", "r1").unwrap();
        let mut m = Mailboxes::new(tmp.path().to_path_buf());
        assert!(m.seen("d", "r1"));
        for i in 0..REMEMBER {
            m.record("d", &format!("x{i}")).unwrap();
        }
        assert!(!m.seen("d", "r1"));
        m.forget("d");
        assert!(!tmp.path().join("requests-d.json").exists());
        assert_eq!(request_id(&json!(5)), Some("5".into()));
        assert_eq!(request_id(&json!("")), None);
        assert_eq!(request_id(&json!(null)), None);
    }
}
