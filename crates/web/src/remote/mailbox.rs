//! Requests by mailbox, and each device's request store
//! (`docs/working/e2e-protocol.md`, "Requests by mailbox" and "Delivery and
//! recovery").
//!
//! When a device finds its host offline (or has no live session), it may
//! queue `overlord.ask` or `overlord.new_session` at the relay; the host
//! answers the same way.
//!
//! Per device, in `remote/mailbox-<id>.json`, in one atomic write: the
//! exported mailbox session, the request store (the last 64 request ids
//! that change something, each with its answer once it has one), and the
//! relay boot and queue number of the last frame handled. The store serves
//! both paths, live and mailbox: a request whose id is in it is never acted
//! on again; the device gets the stored answer, or `in_progress` (and the
//! answer when it comes).
//!
//! Order per mailbox request: decrypt; record its id (running) and the
//! frame's number and save with the advanced state; then acknowledge the
//! relay and act; then store the answer, encrypt it, save, and send it. A
//! crash before the save leaves the frame at the relay and the old state on
//! disk, so it is handled again as if new; a crash after it is recognised
//! when the relay delivers the frame again (by its number), never repeats
//! the op (its id is recorded), and the device's resend is answered
//! `interrupted` (a request still running when the host stopped).
//!
//! Answers leave one at a time per device: the next is encrypted and sent
//! only once the relay said `queued` for the one before, so they reach the
//! device in counter order. One the relay refuses `full` is sent again,
//! the same bytes, when the device next links or a while later; one whose
//! fate a lost connection hides is encrypted afresh (the device skips the
//! gap, or drops the duplicate by request id).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use rlord_core::private;
use rlord_e2e::stream::{self, CONTROL};
use rlord_e2e::MailboxSession;
use rlord_relay_proto::frame::FromHost;
use rlord_relay_proto::{ToRelay, B64};

use super::outq::Full;
use super::requests::{Next, Waiter};
use super::session::error_answer;
use super::Actor;

/// Ops a device may send by mailbox.
pub const MAILBOX_OPS: &[&str] = &["overlord.ask", "overlord.new_session"];
/// Requests remembered per device.
const REMEMBER: usize = 64;
/// A stored answer larger than this is kept as a short note instead.
const ANSWER_MAX: usize = 8 * 1024;
/// Answers waiting to leave by mailbox, per device.
const OUTBOX_MAX: usize = 16;
/// Times an answer is offered to a full queue before it is given up (the
/// device's resend by id still gets it from the store).
const TRIES: u32 = 20;
/// Refused answers are offered again this often (and when the device
/// links).
pub const RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// One request in the store.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
struct Req {
    id: String,
    /// None while it runs.
    #[serde(default)]
    answer: Option<Value>,
}

/// A device's mailbox and what has been handled through it, saved
/// together in one file so a crash never leaves them out of step.
#[derive(Default, Clone, Serialize, Deserialize)]
struct Saved {
    /// `MailboxSession::export`, base64url; None after a failure.
    #[serde(default)]
    state: Option<String>,
    /// The request store, oldest first.
    #[serde(default)]
    requests: VecDeque<Req>,
    /// The relay boot and queue number of the last frame handled: a frame
    /// the relay delivers again (we crashed before acknowledging it) is
    /// recognised and not decrypted twice.
    #[serde(default)]
    boot: String,
    #[serde(default)]
    seq: u64,
}

/// An answer waiting to leave by mailbox.
struct Pending {
    answer: Value,
    /// The lane-2 payload once encrypted (mailbox id, then the frame).
    payload: Option<Vec<u8>>,
    tries: u32,
}

#[derive(Default)]
struct Outbox {
    queue: VecDeque<Pending>,
    /// The front was handed to the relay; its `queued` or `refused` is due.
    in_flight: bool,
    /// The front was refused; offer it again at this time (or on a link).
    retry_at: Option<Instant>,
}

struct Entry {
    session: Option<MailboxSession>,
    saved: Saved,
    outbox: Outbox,
}

/// A device's mailbox and saved state as they were before a frame was
/// handled, put back if saving the result fails ([`Mailboxes::restore`]).
pub(crate) struct Snapshot {
    saved: Saved,
    /// `MailboxSession::export` (None: no mailbox, or one that can't be
    /// exported, which is put back as none).
    session: Option<Vec<u8>>,
}

/// What the store says about a request id.
#[derive(Debug, PartialEq)]
pub enum Lookup {
    New,
    Running,
    Done(Value),
}

pub(crate) struct Mailboxes {
    dir: PathBuf,
    cache: HashMap<String, Entry>,
}

/// A request that was running when the host stopped: its answer is lost,
/// and whether it happened is unknown.
fn interrupted(id: &str) -> Value {
    error_answer(
        &json!(id),
        "interrupted",
        "the host stopped while acting on this request: it may or may not have happened; \
         check, then ask again with a new id",
    )
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
            let mut saved: Saved = std::fs::read(self.path(id))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            // Running when the last plugin stopped: never finishing now.
            for r in saved.requests.iter_mut().filter(|r| r.answer.is_none()) {
                r.answer = Some(interrupted(&r.id));
            }
            let session = saved
                .state
                .as_deref()
                .and_then(super::session::unb64)
                .and_then(|b| MailboxSession::import(&b).ok());
            self.cache.insert(
                id.to_string(),
                Entry {
                    session,
                    saved,
                    outbox: Outbox::default(),
                },
            );
        }
        self.cache.get_mut(id).expect("just inserted")
    }

    /// The device's mailbox, loaded on first use.
    pub fn get(&mut self, id: &str) -> Option<&mut MailboxSession> {
        self.entry(id).session.as_mut()
    }

    /// A newly keyed mailbox replaces the device's old one. Answers not yet
    /// encrypted go under the new one; one encrypted under the old is
    /// encrypted again.
    pub fn install(&mut self, id: &str, m: MailboxSession) -> Result<()> {
        let e = self.entry(id);
        e.session = Some(m);
        if !e.outbox.in_flight {
            if let Some(p) = e.outbox.queue.front_mut() {
                p.payload = None;
            }
        }
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

    /// The device's mailbox and saved state now, for [`Mailboxes::restore`].
    pub fn snapshot(&mut self, id: &str) -> Snapshot {
        let e = self.entry(id);
        Snapshot {
            saved: e.saved.clone(),
            session: e.session.as_ref().and_then(|m| m.export().ok()),
        }
    }

    /// Put back what [`Mailboxes::snapshot`] took: memory again matches
    /// what is on disk (a save failed), so a frame left unacknowledged at
    /// the relay is handled afresh when it comes again.
    pub fn restore(&mut self, id: &str, s: Snapshot) {
        let e = self.entry(id);
        e.saved = s.saved;
        e.session = s.session.and_then(|b| MailboxSession::import(&b).ok());
    }

    /// The mailbox failed (a gap past what the cipher skips, a replay,
    /// tampering): gone on this end; the device's next live session keys a
    /// new one. The request store stays.
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

    /// What the store knows of request `rid`.
    pub fn lookup(&mut self, id: &str, rid: &str) -> Lookup {
        match self.entry(id).saved.requests.iter().find(|r| r.id == rid) {
            None => Lookup::New,
            Some(Req { answer: None, .. }) => Lookup::Running,
            Some(Req {
                answer: Some(a), ..
            }) => Lookup::Done(a.clone()),
        }
    }

    /// Note request `rid` as running (saved with the next save). The
    /// oldest answered request makes room.
    pub fn begin(&mut self, id: &str, rid: &str) {
        let reqs = &mut self.entry(id).saved.requests;
        reqs.push_back(Req {
            id: rid.to_string(),
            answer: None,
        });
        while reqs.len() > REMEMBER {
            match reqs.iter().position(|r| r.answer.is_some()) {
                Some(i) => reqs.remove(i),
                None => reqs.pop_front(),
            };
        }
    }

    /// Store request `rid`'s answer (a large one as a short note).
    pub fn finish(&mut self, id: &str, rid: &str, answer: &Value) {
        let kept = if answer.to_string().len() <= ANSWER_MAX {
            answer.clone()
        } else {
            error_answer(
                &answer["id"],
                "too_large",
                "done, but the answer was too large to keep: look at the state instead",
            )
        };
        let reqs = &mut self.entry(id).saved.requests;
        match reqs.iter_mut().find(|r| r.id == rid) {
            Some(r) => r.answer = Some(kept),
            None => reqs.push_back(Req {
                id: rid.to_string(),
                answer: Some(kept),
            }),
        }
    }

    /// Queue an answer to leave by mailbox; the oldest waiting one makes
    /// room (the device's resend by id still gets it from the store).
    fn push_answer(&mut self, id: &str, answer: Value) -> bool {
        let o = &mut self.entry(id).outbox;
        let mut dropped = false;
        while o.queue.len() >= OUTBOX_MAX {
            let at = usize::from(o.in_flight);
            if at >= o.queue.len() {
                break;
            }
            o.queue.remove(at);
            dropped = true;
        }
        o.queue.push_back(Pending {
            answer,
            payload: None,
            tries: 0,
        });
        dropped
    }

    /// Devices with answers waiting.
    fn waiting(&self) -> Vec<String> {
        self.cache
            .iter()
            .filter(|(_, e)| !e.outbox.queue.is_empty())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// The relay connection went: an answer in flight may or may not have
    /// been queued, so it is encrypted afresh next time.
    pub fn connection_lost(&mut self) {
        for e in self.cache.values_mut() {
            if e.outbox.in_flight {
                e.outbox.in_flight = false;
                if let Some(p) = e.outbox.queue.front_mut() {
                    p.payload = None;
                }
            }
            e.outbox.retry_at = None;
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

/// For the tests only (debug builds): true once, the first time the named
/// point is reached, when `RLORD_WEB_TEST_FAIL` names it.
fn fail_point(name: &str) -> bool {
    static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    cfg!(debug_assertions)
        && std::env::var("RLORD_WEB_TEST_FAIL").is_ok_and(|v| v == name)
        && !FAILED.swap(true, std::sync::atomic::Ordering::Relaxed)
}

/// A request's id as text (a device may send a number or a string).
pub(crate) fn request_id(v: &Value) -> Option<String> {
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
        // Until the save below, everything here changes memory only (the
        // mailbox's counter, the frame's number, the request as running);
        // a failed save puts it back, so the frame, left at the relay, is
        // handled afresh when it comes again rather than taken for done.
        let before = self.mailboxes.snapshot(&device);
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
        // The request's waiters before `admit`, to put back with the rest.
        let waiting_key = req
            .as_ref()
            .and_then(|r| request_id(&r["id"]))
            .map(|k| (device.clone(), k));
        let waiting_before = waiting_key
            .as_ref()
            .and_then(|k| self.waiting.get(k).cloned());
        let next = match &req {
            None => Next::Answer(
                Waiter::Mailbox,
                error_answer(&Value::Null, "bad_request", "not JSON"),
            ),
            Some(r) if !MAILBOX_OPS.contains(&r["op"].as_str().unwrap_or("")) => Next::Answer(
                Waiter::Mailbox,
                error_answer(
                    &r["id"],
                    "not_allowed",
                    "only overlord.ask and overlord.new_session travel by mailbox",
                ),
            ),
            Some(r) => self.admit(&device, r, Waiter::Mailbox),
        };
        // The advanced state, the request as running and this frame's
        // number, in one write, before acting: never done twice.
        let saved = if fail_point("mailbox_frame_save") {
            Err(anyhow::anyhow!(
                "failing at mailbox_frame_save (RLORD_WEB_TEST_FAIL)"
            ))
        } else {
            self.mailboxes.save(&device)
        };
        if let Err(e) = saved {
            eprintln!("rlord-web: saving a mailbox: {e:#}");
            self.mailboxes.restore(&device, before);
            if let Some(k) = waiting_key {
                match waiting_before {
                    Some(w) => self.waiting.insert(k, w),
                    None => self.waiting.remove(&k),
                };
            }
            self.audit.log(
                &device,
                &format!("mailbox frame not saved; left at the relay ({e})"),
            );
            return;
        }
        crash_point("mailbox_saved");
        ack(self);
        self.proceed(&device, next, "mailbox op");
    }

    /// Queue an answer to leave by the device's mailbox.
    pub(crate) fn mailbox_answer(&mut self, device: &str, answer: Value) {
        if self.mailboxes.push_answer(device, answer) {
            eprintln!(
                "rlord-web: too many mailbox answers waiting for {device}; dropped the oldest"
            );
        }
        self.mailbox_pump(device);
    }

    /// Send the device's next waiting answer, if none is in flight.
    pub(crate) fn mailbox_pump(&mut self, device: &str) {
        let Some(q) = self.queue() else {
            return;
        };
        let Some(fp) = self.devices.by_id(device).and_then(|d| d.relay_fp()) else {
            return;
        };
        let e = self.mailboxes.entry(device);
        if e.outbox.in_flight || e.outbox.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let Some(p) = e.outbox.queue.front() else {
            return;
        };
        let payload = match &p.payload {
            Some(b) => b.clone(),
            None => {
                let Some(m) = e.session.as_mut() else {
                    // No mailbox (discarded): nothing can be sent until the
                    // next live link keys one; the store has the answers.
                    e.outbox.queue.clear();
                    return;
                };
                let mut body = p.answer.to_string().into_bytes();
                if body.len() > stream::CHUNK {
                    body = error_answer(
                        &p.answer["id"],
                        "too_large",
                        "the answer is too large for the mailbox",
                    )
                    .to_string()
                    .into_bytes();
                }
                let frame = match stream::fragment(CONTROL, &body).map(|f| m.encrypt(&f[0])) {
                    Ok(Ok(f)) => f,
                    _ => {
                        e.outbox.queue.pop_front();
                        return;
                    }
                };
                let mut payload = m.id().to_vec();
                payload.extend_from_slice(&frame);
                // Saved before it leaves.
                if let Err(err) = self.mailboxes.save(device) {
                    eprintln!("rlord-web: saving a mailbox: {err:#}");
                    return;
                }
                let e = self.mailboxes.entry(device);
                if let Some(p) = e.outbox.queue.front_mut() {
                    p.payload = Some(payload.clone());
                }
                payload
            }
        };
        let frame = FromHost::Mailbox {
            device: fp,
            payload,
        }
        .encode();
        match q.mailbox(frame) {
            Ok(()) => {
                let o = &mut self.mailboxes.entry(device).outbox;
                o.in_flight = true;
                o.retry_at = None;
            }
            // Our own queue is full: try again on the next round.
            Err(Full::Mailbox) => {
                self.mailboxes.entry(device).outbox.retry_at = Some(Instant::now() + RETRY_EVERY)
            }
            Err(_) => {}
        }
    }

    /// The relay's answer to our last lane-2 frame for `fp`: `queued`
    /// (`refused` is None) or `refused` with its code.
    pub(crate) fn mailbox_reply(&mut self, fp: &[u8], refused: Option<&str>) {
        let Some(device) = self
            .devices
            .list
            .iter()
            .find(|d| d.relay_fp().is_some_and(|f| f[..] == *fp))
            .map(|d| d.id.clone())
        else {
            return;
        };
        let o = &mut self.mailboxes.entry(&device).outbox;
        if !o.in_flight {
            return;
        }
        o.in_flight = false;
        match refused {
            None => {
                o.queue.pop_front();
            }
            Some(code) => {
                let full = code == rlord_relay_proto::msg::code::FULL;
                let tries = o.queue.front_mut().map(|p| {
                    p.tries += 1;
                    p.tries
                });
                if full && tries.is_some_and(|t| t < TRIES) {
                    // The same bytes again later: the device's queue at the
                    // relay drains when it connects, or as messages expire.
                    o.retry_at = Some(Instant::now() + RETRY_EVERY);
                    return;
                }
                o.queue.pop_front();
                self.audit
                    .log(&device, &format!("mailbox answer not delivered ({code})"));
            }
        }
        self.mailbox_pump(&device);
    }

    /// Offer waiting answers again: every device's when `device` is None
    /// (a new connection, the retry tick), else that device's now (it just
    /// linked, so its queue at the relay is draining).
    pub(crate) fn mailbox_retry(&mut self, device: Option<&str>) {
        let devices = match device {
            Some(d) => {
                self.mailboxes.entry(d).outbox.retry_at = None;
                vec![d.to_string()]
            }
            None => self.mailboxes.waiting(),
        };
        for d in devices {
            self.mailbox_pump(&d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_frames_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Mailboxes::new(tmp.path().to_path_buf());
        assert_eq!(m.lookup("d", "r1"), Lookup::New);
        m.begin("d", "r1");
        assert_eq!(m.lookup("d", "r1"), Lookup::Running);
        m.finish("d", "r1", &json!({"id": "r1", "ok": 1}));
        assert_eq!(
            m.lookup("d", "r1"),
            Lookup::Done(json!({"id": "r1", "ok": 1}))
        );
        m.begin("d", "r2");
        m.mark("d", "b1", 7);
        m.save("d").unwrap();
        let mut m = Mailboxes::new(tmp.path().to_path_buf());
        assert_eq!(
            m.lookup("d", "r1"),
            Lookup::Done(json!({"id": "r1", "ok": 1}))
        );
        // Running when the plugin stopped: interrupted, never run again.
        let Lookup::Done(a) = m.lookup("d", "r2") else {
            panic!()
        };
        assert_eq!(a["error"]["code"], "interrupted");
        assert!(m.handled("d", "b1", 7) && m.handled("d", "b1", 3));
        assert!(!m.handled("d", "b1", 8) && !m.handled("d", "b2", 1));
        m.forget("d");
        assert!(!tmp.path().join("mailbox-d.json").exists());
        assert_eq!(request_id(&json!(5)), Some("5".into()));
        assert_eq!(request_id(&json!("")), None);
        assert_eq!(request_id(&json!(null)), None);
    }

    #[test]
    fn a_failed_save_is_undone_by_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Mailboxes::new(tmp.path().to_path_buf());
        m.begin("d", "r0");
        m.mark("d", "b1", 3);
        m.save("d").unwrap();
        // Handling a frame: its number and request, then the save fails
        // (the folder is now a file).
        let before = m.snapshot("d");
        m.mark("d", "b1", 4);
        m.begin("d", "r1");
        std::fs::remove_dir_all(tmp.path()).unwrap();
        std::fs::write(tmp.path(), b"").unwrap();
        assert!(m.save("d").is_err());
        m.restore("d", before);
        // As before the frame: it is new again when redelivered, and so is
        // its request.
        assert!(!m.handled("d", "b1", 4));
        assert!(m.handled("d", "b1", 3));
        assert_eq!(m.lookup("d", "r1"), Lookup::New);
        assert_eq!(m.lookup("d", "r0"), Lookup::Running);
        std::fs::remove_file(tmp.path()).unwrap();
    }

    #[test]
    fn the_store_is_bounded_and_keeps_running_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Mailboxes::new(tmp.path().to_path_buf());
        m.begin("d", "running");
        for i in 0..REMEMBER {
            m.begin("d", &format!("x{i}"));
            m.finish("d", &format!("x{i}"), &json!({"ok": i}));
        }
        // The oldest answered one went, not the one still running.
        assert_eq!(m.lookup("d", "running"), Lookup::Running);
        assert_eq!(m.lookup("d", "x0"), Lookup::New);
        assert!(matches!(m.lookup("d", "x1"), Lookup::Done(_)));
        // A large answer is kept as a note.
        m.finish(
            "d",
            "x5",
            &json!({"id": "x5", "ok": "y".repeat(ANSWER_MAX)}),
        );
        let Lookup::Done(a) = m.lookup("d", "x5") else {
            panic!()
        };
        assert_eq!(a["error"]["code"], "too_large");
        assert_eq!(a["id"], "x5");
    }

    #[test]
    fn the_outbox_is_bounded_and_spares_the_one_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let mut m = Mailboxes::new(tmp.path().to_path_buf());
        m.push_answer("d", json!(0));
        m.entry("d").outbox.in_flight = true;
        for i in 1..=OUTBOX_MAX {
            m.push_answer("d", json!(i));
        }
        let q: Vec<Value> = m
            .entry("d")
            .outbox
            .queue
            .iter()
            .map(|p| p.answer.clone())
            .collect();
        assert_eq!(q.len(), OUTBOX_MAX);
        assert_eq!(q[0], json!(0));
        assert_eq!(q[1], json!(2));
        m.entry("d").outbox.queue[0].payload = Some(vec![1]);
        m.connection_lost();
        let e = m.entry("d");
        assert!(!e.outbox.in_flight && e.outbox.queue[0].payload.is_none());
    }
}
