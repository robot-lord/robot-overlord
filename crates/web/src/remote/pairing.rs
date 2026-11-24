//! Pairing a device, with the host's confirmation
//! (`docs/working/e2e-protocol.md`, "Pairing").
//!
//! User-only: started, confirmed and rejected by `relay.*` commands from
//! the daemon (the CLI, the TUI or the local page), never by a device or
//! the overlord. One pairing at a time, valid for 300 s and one device; a
//! waiting device is answered within 120 s or rejected. Until the user
//! confirms, the device is not stored, not on the relay's allow-list and
//! can call nothing.

use serde_json::{json, Value};

use rlord_e2e::stream::{self, CONTROL};
use rlord_e2e::{device_id, Kind, QrPayload, Responder};
use rlord_relay_proto::{auth, ToRelay, B64};

use super::devices::{clean_name, Device};
use super::session::{next_epoch, unb64, Link, Phase};
use super::{now, Actor};

/// How long a QR is valid.
const PAIRING_TTL: u64 = 300;
/// How long a device waits for the user's answer.
const CONFIRM_TTL: u64 = 120;
/// Failed attempts that close a rendezvous.
const MAX_ATTEMPTS: u32 = 3;

pub(crate) struct Pairing {
    pub qr: QrPayload,
    /// Unix seconds.
    pub expires: u64,
    pub attempts: u32,
    /// The pairing link now (one at a time).
    pub link: Option<u32>,
    pub pending: Option<Pending>,
}

/// A device waiting for the user.
pub(crate) struct Pending {
    pub link: u32,
    pub name: String,
    pub code: u32,
    pub static_key: [u8; 32],
    pub relay_fp: [u8; 32],
    /// Unix seconds.
    pub deadline: u64,
}

impl Actor {
    /// `relay.pair`: a new pairing (replacing any open one).
    pub(crate) fn pair(&mut self) -> Result<Value, String> {
        if !self.cfg.enabled {
            return Err("remote access is off: run `rlord relay setup <relay address>`".into());
        }
        let keys = self.pin_ok()?;
        let qr = QrPayload::new(keys.relay_id(), *keys.host.public()).map_err(|e| e.to_string())?;
        self.cancel_pairing("replaced by a new pairing");
        let expires = now() + PAIRING_TTL;
        let encoded = qr.encode();
        self.pairing = Some(Pairing {
            qr,
            expires,
            attempts: 0,
            link: None,
            pending: None,
        });
        self.audit.log("", "pairing opened");
        // Connected already: open the rendezvous now; else the connection
        // this pairing wants opens it (`pairing_connected`).
        if self.connected_now() {
            self.open_rendezvous();
        }
        Ok(json!({"qr": encoded, "expires": expires}))
    }

    fn open_rendezvous(&mut self) {
        let Some(p) = &self.pairing else {
            return;
        };
        let id = B64(p.qr.rendezvous_id.to_vec());
        let ttl = p.expires.saturating_sub(now()).max(1);
        self.send_relay(|n| ToRelay::Rendezvous { n, id, ttl });
    }

    /// A relay connection came up.
    pub(crate) fn pairing_connected(&mut self) {
        if let Some(p) = &mut self.pairing {
            p.link = None;
        }
        self.open_rendezvous();
    }

    /// The relay connection went: a pairing link with it.
    pub(crate) fn pairing_link_gone(&mut self) {
        let failed = self.pairing.as_ref().and_then(|p| p.link).is_some();
        if let Some(p) = &mut self.pairing {
            p.link = None;
            if failed {
                p.pending = None;
            }
        }
    }

    /// Close the pairing (and its rendezvous and link).
    pub(crate) fn cancel_pairing(&mut self, why: &str) {
        let Some(p) = self.pairing.take() else {
            return;
        };
        if let Some(l) = p.link {
            if p.pending.is_some() {
                self.send_control(l, &json!({"event": "pair", "state": "rejected"}));
            }
            self.links.remove(&l);
            self.send_relay(|n| ToRelay::Drop { n, link: l });
        }
        let id = B64(p.qr.rendezvous_id.to_vec());
        self.send_relay(|n| ToRelay::RendezvousClose { n, id });
        self.audit.log("", &format!("pairing closed ({why})"));
    }

    /// Expiries: the QR after 300 s, a waiting device after 120 s.
    pub(crate) fn pairing_tick(&mut self) {
        let t = now();
        let Some(p) = &self.pairing else {
            return;
        };
        if p.pending.as_ref().is_some_and(|w| w.deadline <= t) {
            let _ = self.reject_pending("no answer in time");
        } else if p.pending.is_none() && p.expires <= t {
            self.cancel_pairing("expired");
        }
    }

    /// The relay says a pairing device connected to our rendezvous.
    pub(crate) fn pair_link(&mut self, id: u32, fp: [u8; 32], rendezvous: Option<B64>) {
        let ok = self.pairing.as_ref().is_some_and(|p| {
            p.link.is_none()
                && p.pending.is_none()
                && rendezvous.as_ref().map(|r| r.0.as_slice()) == Some(&p.qr.rendezvous_id[..])
        });
        if !ok {
            return self.drop_link(id, "no such pairing");
        }
        if let Some(p) = &mut self.pairing {
            p.link = Some(id);
        }
        self.links.insert(id, Link::new(fp, true, next_epoch()));
    }

    /// A frame on the pairing link.
    pub(crate) fn pair_frame(&mut self, id: u32, payload: &[u8]) {
        let Some(l) = self.links.get_mut(&id) else {
            return;
        };
        let fp = l.fp();
        match std::mem::replace(&mut l.phase, Phase::AwaitMsg1) {
            Phase::AwaitMsg1 => {
                let (Some(keys), Some(p)) = (self.keys.as_ref(), self.pairing.as_ref()) else {
                    return self.drop_link(id, "no pairing");
                };
                if Kind::of_message(payload).ok() != Some(Kind::Pair) {
                    return self.drop_link(id, "expected a pairing");
                }
                match Responder::pair(&keys.host, &p.qr, &keys.relay_id(), payload, None).and_then(
                    |(est, msg2)| {
                        let code = est.short_code();
                        let device_static = est.remote_static;
                        est.into_session().map(|s| (s, msg2, code, device_static))
                    },
                ) {
                    Ok((session, msg2, code, device_static)) => {
                        if let Some(l) = self.links.get_mut(&id) {
                            l.phase = Phase::PairFirst {
                                session,
                                code,
                                device_static,
                            };
                        }
                        self.send_live(id, msg2);
                    }
                    Err(e) => self.drop_link(id, &format!("pairing handshake: {e}")),
                }
            }
            Phase::PairFirst {
                mut session,
                code,
                device_static,
            } => {
                // The device's first frame: it holds the pairing secret.
                let first = session
                    .decrypt(payload)
                    .ok()
                    .and_then(|p| stream::single(&p).ok())
                    .filter(|m| m.stream == CONTROL)
                    .and_then(|m| serde_json::from_slice::<Value>(&m.body).ok());
                let Some(first) = first else {
                    return self.drop_link(id, "pairing: the first frame failed");
                };
                if let Some(l) = self.links.get_mut(&id) {
                    l.proved = true;
                }
                let relay_key = first["key"].as_str().and_then(unb64).unwrap_or_default();
                // The key the device says it has is the one the relay
                // linked: else something in between swapped them.
                if first["event"].as_str() != Some("pair")
                    || auth::check_public(&relay_key).is_err()
                    || auth::fingerprint(&relay_key) != fp
                {
                    return self.drop_link(id, "pairing: the device's relay key doesn't match");
                }
                let name = clean_name(first["name"].as_str().unwrap_or(""));
                let deadline = now() + CONFIRM_TTL;
                if let Some(l) = self.links.get_mut(&id) {
                    l.phase = Phase::PairWaiting(session);
                }
                if let Some(p) = &mut self.pairing {
                    p.pending = Some(Pending {
                        link: id,
                        name: name.clone(),
                        code,
                        static_key: device_static,
                        relay_fp: fp,
                        deadline,
                    });
                }
                self.audit.log(
                    &device_id(&device_static),
                    &format!("pairing: waiting for confirmation ({name})"),
                );
                self.send_control(
                    id,
                    &json!({"event": "pair", "state": "waiting", "code": format!("{code:06}")}),
                );
            }
            Phase::PairWaiting(mut session) => {
                // Nothing is due while it waits; a frame that fails ends it.
                if session.decrypt(payload).is_err() {
                    return self.drop_link(id, "pairing: a bad frame");
                }
                if let Some(l) = self.links.get_mut(&id) {
                    l.phase = Phase::PairWaiting(session);
                }
            }
            other => {
                if let Some(l) = self.links.get_mut(&id) {
                    l.phase = other;
                }
            }
        }
    }

    /// The pairing link went (unlinked or dropped).
    pub(crate) fn pair_unlinked(&mut self, id: u32, l: Link) {
        let Some(p) = &mut self.pairing else {
            return;
        };
        if p.link != Some(id) {
            return;
        }
        p.link = None;
        if p.pending.as_ref().is_some_and(|w| w.link == id) {
            // A waiting device left: nothing stored, start again.
            p.pending = None;
            self.audit
                .log("", "pairing: the device left before an answer");
            self.cancel_pairing("the device left");
            return;
        }
        if !l.proved {
            p.attempts += 1;
            if p.attempts >= MAX_ATTEMPTS {
                self.cancel_pairing("three failed attempts");
            }
        }
    }

    /// `relay.confirm`: store the waiting device and tell it.
    pub(crate) fn confirm(&mut self) -> Result<Value, String> {
        let Some(w) = self.pairing.as_mut().and_then(|p| p.pending.take()) else {
            return Err("no device is waiting for confirmation".into());
        };
        let id = device_id(&w.static_key);
        let device = Device {
            id: id.clone(),
            name: w.name.clone(),
            static_key: B64(w.static_key.to_vec()),
            relay_fp: B64(w.relay_fp.to_vec()),
            paired_at: now(),
            last_seen: None,
            // New devices may type (slice 5 answer 2).
            input: true,
        };
        if let Err(e) = self.devices.add(device) {
            // Not stored: the device still waits, so confirming can be
            // tried again (or the pairing rejected).
            if let Some(p) = self.pairing.as_mut() {
                p.pending = Some(w);
            }
            return Err(format!("{e:#}"));
        }
        self.audit.log(&id, &format!("paired ({})", w.name));
        self.send_allow();
        let host_key = self
            .keys
            .as_ref()
            .map(|k| super::session::b64(&k.relay.fingerprint()))
            .unwrap_or_default();
        self.send_control(
            w.link,
            &json!({"event": "pair", "state": "done", "host": host_name(), "host_key": host_key}),
        );
        // Done with this QR: the rendezvous and the link close.
        if let Some(p) = self.pairing.take() {
            let rid = B64(p.qr.rendezvous_id.to_vec());
            self.send_relay(|n| ToRelay::RendezvousClose { n, id: rid });
        }
        self.links.remove(&w.link);
        let link = w.link;
        self.send_relay(|n| ToRelay::Drop { n, link });
        Ok(json!({"device": {"id": id, "name": w.name}}))
    }

    /// `relay.reject` (or the timeout): nothing stored; start again.
    pub(crate) fn reject_pending(&mut self, why: &str) -> Result<(), String> {
        let Some(w) = self.pairing.as_mut().and_then(|p| p.pending.take()) else {
            return Err("no device is waiting for confirmation".into());
        };
        self.send_control(w.link, &json!({"event": "pair", "state": "rejected"}));
        self.audit.log(
            &device_id(&w.static_key),
            &format!("pairing rejected ({why})"),
        );
        self.cancel_pairing(why);
        Ok(())
    }
}

/// This machine's name, for the device's list.
fn host_name() -> String {
    let mut buf = [0u8; 256];
    // Safety: gethostname writes at most `len` bytes into `buf`.
    let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if r != 0 {
        return "rlord".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let s = String::from_utf8_lossy(&buf[..end]).into_owned();
    if s.is_empty() {
        "rlord".into()
    } else {
        s
    }
}
