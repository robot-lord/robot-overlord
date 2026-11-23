//! A device's requests, live or by mailbox, carried to the daemon as
//! `web.call {op, args, via: "remote", device}`, at most once each
//! (`docs/working/e2e-protocol.md`, "Delivery and recovery").
//!
//! An op the daemon marks idempotent (`web::OPS`, told to the plugin in
//! `web.hello`'s answer) just runs, as often as it is asked. Any other op
//! needs a request id, and the device's request store (`mailbox.rs`)
//! remembers it: a repeat of a finished request gets its stored answer, a
//! repeat of a running one `in_progress` and then the answer, by whichever
//! way it came (a live link, the mailbox, or both). That is what makes a
//! device's resend by id safe after a lost link, a relay restart or a
//! host restart.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::mailbox::{request_id, Lookup};
use super::session::{code_of, error_answer};
use super::{Actor, Input};

/// Where an answer goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waiter {
    /// The live link it came on, if it is still the same link.
    Live { link: u32, epoch: u64 },
    /// The device's mailbox.
    Mailbox,
}

/// What to do with an admitted request.
pub(crate) enum Next {
    /// Answer now (a refusal, a stored answer, `in_progress`).
    Answer(Waiter, Value),
    /// Call the daemon. `key`: the request id when the store records it.
    Run {
        op: String,
        args: Value,
        rid: Value,
        key: Option<String>,
        from: Waiter,
    },
}

/// Running requests' waiters, per (device, request id).
pub(crate) type Waiting = HashMap<(String, String), Vec<Waiter>>;

/// How long a device that asked to leave keeps its link for the answer
/// to reach it (it closes the link itself once it has it).
const LEAVE_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

impl Actor {
    /// Look a request up in the device's store and say what to do. Records
    /// a new non-idempotent request as running; the caller saves the store
    /// before acting (`proceed`).
    pub(crate) fn admit(&mut self, device: &str, req: &Value, from: Waiter) -> Next {
        let rid = req["id"].clone();
        let Some(op) = req["op"].as_str().map(str::to_string) else {
            return Next::Answer(from, error_answer(&rid, "bad_request", "no op"));
        };
        let args = req.get("args").cloned().unwrap_or(json!({}));
        if self.idempotent.contains(&op) {
            return Next::Run {
                op,
                args,
                rid,
                key: None,
                from,
            };
        }
        let Some(key) = request_id(&rid) else {
            return Next::Answer(
                from,
                error_answer(
                    &rid,
                    "bad_request",
                    "this op needs a request id (a resend uses the same one)",
                ),
            );
        };
        match self.mailboxes.lookup(device, &key) {
            Lookup::Done(answer) => {
                self.audit.log(device, &format!("repeat {op}"));
                Next::Answer(from, answer)
            }
            Lookup::Running => {
                let w = self.waiting.entry((device.to_string(), key)).or_default();
                if !w.contains(&from) {
                    w.push(from);
                }
                Next::Answer(
                    from,
                    error_answer(
                        &rid,
                        "in_progress",
                        "this request is still running; its answer follows",
                    ),
                )
            }
            Lookup::New => {
                self.mailboxes.begin(device, &key);
                self.waiting
                    .insert((device.to_string(), key.clone()), vec![from]);
                Next::Run {
                    op,
                    args,
                    rid,
                    key: Some(key),
                    from,
                }
            }
        }
    }

    /// Act on `admit`'s decision (the store saved).
    pub(crate) fn proceed(&mut self, device: &str, next: Next, what: &str) {
        match next {
            Next::Answer(to, answer) => self.deliver(device, to, answer),
            Next::Run {
                op,
                args,
                rid,
                key,
                from,
            } => {
                self.audit.log(device, &format!("{what} {op}"));
                let (d, tx) = (self.d.clone(), self.tx_clone());
                let device = device.to_string();
                tokio::spawn(async move {
                    let r = d
                        .call(
                            "web.call",
                            json!({"op": op, "args": args, "via": "remote", "device": device}),
                        )
                        .await;
                    let answer = match r {
                        Ok(v) => json!({"id": rid, "ok": v}),
                        Err(e) => error_answer(&rid, code_of(e.code), &e.message),
                    };
                    let _ = tx.send(Input::Done {
                        device,
                        op,
                        key,
                        from,
                        answer,
                    });
                });
            }
        }
    }

    /// A daemon call finished: store the answer, and send it to everyone
    /// waiting for it. A `device.leave` the daemon allowed then has the
    /// device removed as `revoke` removes one, once its answer has had
    /// time to reach it (`leave_due`): closing its link at once could lose
    /// the answer at the relay.
    pub(crate) fn done(
        &mut self,
        device: &str,
        op: &str,
        key: Option<String>,
        from: Waiter,
        answer: Value,
    ) {
        self.answer_waiters(device, key, from, &answer);
        if op == "device.leave" && answer.get("ok").is_some() {
            self.leaving
                .entry(device.to_string())
                .or_insert_with(std::time::Instant::now);
        }
    }

    /// Remove the devices that asked to leave: `gone` (its link just
    /// closed: it had the answer, or never will), and any whose answer has
    /// had `LEAVE_GRACE`.
    pub(crate) fn leave_due(&mut self, gone: Option<&str>) {
        let now = std::time::Instant::now();
        let due: Vec<String> = self
            .leaving
            .iter()
            .filter(|(id, at)| Some(id.as_str()) == gone || now.duration_since(**at) >= LEAVE_GRACE)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            self.leaving.remove(&id);
            if let Err(e) = self.revoke(&id, "left: unpaired on the device") {
                eprintln!("rlord-web: device.leave: {e}");
            }
        }
    }

    /// Store a finished request's answer and send it to its waiters.
    fn answer_waiters(&mut self, device: &str, key: Option<String>, from: Waiter, answer: &Value) {
        let waiters = match key {
            Some(k) => {
                self.mailboxes.finish(device, &k, answer);
                if let Err(e) = self.mailboxes.save(device) {
                    eprintln!("rlord-web: saving a request store: {e:#}");
                }
                self.waiting
                    .remove(&(device.to_string(), k))
                    .unwrap_or_else(|| vec![from])
            }
            None => vec![from],
        };
        for w in waiters {
            self.deliver(device, w, answer.clone());
        }
    }

    /// Send an answer its way: on the live link it came on, if that link is
    /// still there, or by mailbox.
    pub(crate) fn deliver(&mut self, device: &str, to: Waiter, answer: Value) {
        match to {
            Waiter::Live { link, epoch } => {
                if self
                    .links
                    .get(&link)
                    .is_some_and(|l| l.epoch == epoch && l.is_live())
                {
                    self.send_control(link, &answer);
                }
            }
            Waiter::Mailbox => self.mailbox_answer(device, answer),
        }
    }
}
