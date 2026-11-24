//! Panes on a paired device: a session's tmux pane carried as a stream in
//! the device's live Noise session (`docs/working/e2e-protocol.md`,
//! "Panes"; the plan is "Slice 5 plan" in docs/working/web-ui-remote.md).
//!
//! The device opens one with the `pane.open` op (the daemon's allow-list
//! decides it and applies who controls the pane); the host attaches a view
//! client (`bridge.rs`) and runs a task for the stream. The client's
//! output goes to the device as pane frames, its input comes back the same
//! way; `resize`, `ack` and `close` are events on the control stream, and
//! the host says `closed` there when a pane ends.
//!
//! **Flow control.** The host sends on a stream only while what the
//! device hasn't acknowledged is under [`WINDOW`] bytes and [`FRAMES`]
//! frames (the relay closes a device 128 frames behind), and only while
//! the link's queue holds less than `outq::LOW_MARK` (frames are encrypted
//! when queued, so that mark is what keeps a busy pane from delaying an
//! answer), and no faster than the pane's own rate ([`RATE`], [`BURST`]).
//! The task asks for nothing: the actor hands it one permission at a time
//! (`Go`) to send one frame of at most so many bytes.
//!
//! **A view client is never left unread.** tmux stops discarding for a
//! client whose last redraw isn't written yet, so an unread client makes
//! the tmux server buffer a flood whole (gate 1a). The task keeps reading
//! while it may not send, into a hold of at most [`HOLD`] bytes that goes
//! first when it may send again; if the hold would overflow, the task drops
//! it, ends that view client, and when it may send again attaches a fresh
//! one at the same size behind `CAN ESC c` (abort any sequence, full
//! reset), so the device gets one whole redraw. Only bytes never sent are
//! dropped: the stream stays lossless.
//!
//! **Coalescing.** A read after [`COALESCE`] of quiet goes at once;
//! otherwise bytes gather until the frame is full or [`COALESCE`] has
//! passed, so a spinner is one small frame per tick.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc;

use rlord_e2e::stream::{self, CHUNK};

use super::session::{code_of, error_answer, Phase};
use super::{Actor, Input};
use crate::bridge::{size, view_name, Bridge};

/// Bytes a device may have unacknowledged on one stream.
pub const WINDOW: u64 = 64 * 1024;
/// Frames a device may have unacknowledged on one stream.
pub const FRAMES: u64 = 48;
/// Open panes per device.
pub const PER_DEVICE: usize = 2;
/// Remote panes in all (local pane sockets are counted apart).
pub const MAX_REMOTE: usize = 8;
/// A pane's own rate and burst, bytes.
pub const RATE: f64 = 64.0 * 1024.0;
pub const BURST: f64 = 256.0 * 1024.0;
/// How long a busy pane's bytes gather before they go.
pub const COALESCE: Duration = Duration::from_millis(15);
/// What a pane that may not send holds, at most, before it reattaches.
pub const HOLD: usize = 256 * 1024;
/// Input frames waiting for the PTY; one more closes the stream.
pub const INPUT_FRAMES: usize = 64;
/// What a fresh view client's first frame starts with after a hold
/// overflowed: CAN (abort any sequence), then RIS (full reset).
pub const RESET: &[u8] = b"\x18\x1bc";

/// The limits a device is told in its `hello`.
pub fn hello_limits() -> Value {
    json!({"max": PER_DEVICE, "window": WINDOW, "frames": FRAMES})
}

// ---- credit (the actor's side) ----

/// What one stream has sent and had acknowledged.
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub struct Credit {
    pub sent_bytes: u64,
    pub sent_frames: u64,
    pub acked_bytes: u64,
    pub acked_frames: u64,
}

impl Credit {
    /// How many bytes the next frame may carry, or None while the device
    /// is a window behind.
    pub fn grant(&self) -> Option<usize> {
        let bytes = self.sent_bytes - self.acked_bytes;
        let frames = self.sent_frames - self.acked_frames;
        if bytes >= WINDOW || frames >= FRAMES {
            return None;
        }
        Some(((WINDOW - bytes) as usize).min(CHUNK))
    }

    pub fn sent(&mut self, n: usize) {
        self.sent_bytes += n as u64;
        self.sent_frames += 1;
    }

    /// A cumulative acknowledgement. Past what was sent is an error (the
    /// stream is closed); an older one changes nothing.
    pub fn ack(&mut self, bytes: u64, frames: u64) -> Result<(), ()> {
        if bytes > self.sent_bytes || frames > self.sent_frames {
            return Err(());
        }
        self.acked_bytes = self.acked_bytes.max(bytes);
        self.acked_frames = self.acked_frames.max(frames);
        Ok(())
    }
}

// ---- gathering and rate (the task's side) ----

/// Bytes read and not yet sent, and when they may go.
#[derive(Default)]
pub struct Gather {
    pub hold: Vec<u8>,
    /// When the oldest unsent byte was read.
    since: Option<Instant>,
    last_read: Option<Instant>,
    /// The batch may go at once (it came after a quiet spell, or is what a
    /// full frame left behind).
    at_once: bool,
}

impl Gather {
    pub fn push(&mut self, now: Instant, bytes: &[u8]) {
        if self.hold.is_empty() {
            self.since = Some(now);
            self.at_once = self
                .last_read
                .is_none_or(|t| now.duration_since(t) >= COALESCE);
        }
        self.last_read = Some(now);
        self.hold.extend_from_slice(bytes);
    }

    /// Put `bytes` in front (the reset ahead of a fresh client).
    pub fn prefix(&mut self, now: Instant, bytes: &[u8]) {
        let mut v = bytes.to_vec();
        v.append(&mut self.hold);
        self.hold = v;
        self.since.get_or_insert(now);
    }

    /// Whether a frame of at most `n` bytes should go now.
    pub fn ready(&self, now: Instant, n: usize) -> bool {
        !self.hold.is_empty()
            && (self.at_once
                || self.hold.len() >= n
                || self
                    .since
                    .is_some_and(|t| now.duration_since(t) >= COALESCE))
    }

    /// When gathering ends by itself (nothing to wait for: None).
    pub fn deadline(&self) -> Option<Instant> {
        if self.hold.is_empty() || self.at_once {
            return None;
        }
        self.since.map(|t| t + COALESCE)
    }

    pub fn take(&mut self, now: Instant, n: usize) -> Vec<u8> {
        let n = n.min(self.hold.len());
        let out: Vec<u8> = self.hold.drain(..n).collect();
        if self.hold.is_empty() {
            self.since = None;
        } else {
            self.since = Some(now);
            self.at_once = true;
        }
        out
    }

    pub fn clear(&mut self) {
        self.hold.clear();
        self.since = None;
    }
}

/// A pane's own rate: a token bucket of [`RATE`] and [`BURST`], full at
/// first.
pub struct Rate {
    tokens: f64,
    at: Instant,
    rate: f64,
    burst: f64,
}

impl Rate {
    pub fn new(now: Instant) -> Rate {
        Rate::with(now, RATE, BURST)
    }

    pub fn with(now: Instant, rate: f64, burst: f64) -> Rate {
        Rate {
            tokens: burst,
            at: now,
            rate,
            burst,
        }
    }

    fn refill(&mut self, now: Instant) {
        self.tokens =
            (self.tokens + now.duration_since(self.at).as_secs_f64() * self.rate).min(self.burst);
        self.at = now;
    }

    /// How long until `n` bytes may go (zero: now).
    pub fn wait(&mut self, now: Instant, n: usize) -> Duration {
        self.refill(now);
        let need = (n as f64).min(self.burst);
        if self.tokens >= need {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((need - self.tokens) / self.rate)
        }
    }

    pub fn spend(&mut self, now: Instant, n: usize) {
        self.refill(now);
        self.tokens -= n as f64;
    }
}

// ---- the task ----

/// What the actor tells a pane's task.
#[derive(Debug)]
pub enum Cmd {
    /// Send one frame of at most this many bytes, when there is something.
    Go(usize),
    Resize(u16, u16),
}

/// What a pane's task tells the actor.
#[derive(Debug)]
pub enum Event {
    Data(Vec<u8>),
    /// The view client ended (or couldn't be attached again).
    Ended {
        reason: &'static str,
        message: String,
    },
}

/// Where a pane's task reports, and which pane it is.
pub struct Tag {
    pub link: u32,
    pub epoch: u64,
    pub stream: u8,
    pub gen: u64,
}

/// What a task needs to attach a fresh view client.
pub struct Target {
    pub tmux_session: String,
    pub read_only: bool,
}

fn report(tx: &mpsc::UnboundedSender<Input>, tag: &Tag, ev: Event) {
    let _ = tx.send(Input::Pane {
        link: tag.link,
        epoch: tag.epoch,
        stream: tag.stream,
        gen: tag.gen,
        ev,
    });
}

async fn end(b: Option<Bridge>) {
    if let Some(mut b) = b {
        let _ = tokio::task::spawn_blocking(move || b.end()).await;
    }
}

/// One stream's task: owns the view client; reads it always (see the
/// module's note), sends when the actor says `Go`, writes input in order.
/// Ends with the client (`Ended`), or quietly when the actor drops it.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    bridge: Bridge,
    target: Target,
    mut cols: u16,
    mut rows: u16,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    mut input: mpsc::Receiver<Vec<u8>>,
    tx: mpsc::UnboundedSender<Input>,
    tag: Tag,
) {
    let mut term = Some(bridge);
    let mut go: Option<usize> = None;
    let mut gather = Gather::default();
    let mut rate = Rate::new(Instant::now());
    let mut reattach = false;
    loop {
        let now = Instant::now();
        // A fresh client once it may send again.
        if reattach && go.is_some() && term.is_none() {
            let (t, view) = (target.tmux_session.clone(), view_name());
            let ro = target.read_only;
            let b = tokio::task::spawn_blocking(move || Bridge::attach(&t, &view, cols, rows, ro))
                .await;
            match b {
                Ok(Ok(b)) => {
                    term = Some(b);
                    reattach = false;
                    gather.clear();
                    gather.prefix(Instant::now(), RESET);
                }
                Ok(Err(e)) => {
                    return report(
                        &tx,
                        &tag,
                        Event::Ended {
                            reason: "ended",
                            message: format!("{e:#}"),
                        },
                    );
                }
                Err(e) => {
                    return report(
                        &tx,
                        &tag,
                        Event::Ended {
                            reason: "failed",
                            message: e.to_string(),
                        },
                    );
                }
            }
            continue;
        }
        // Send, if allowed and due.
        let mut wake: Option<Instant> = gather.deadline();
        if let Some(n) = go {
            if gather.ready(now, n) {
                let len = n.min(gather.hold.len());
                let wait = rate.wait(now, len);
                if wait.is_zero() {
                    let bytes = gather.take(now, n);
                    rate.spend(now, bytes.len());
                    go = None;
                    report(&tx, &tag, Event::Data(bytes));
                    continue;
                }
                wake = Some(now + wait);
            }
        }
        enum Got {
            Cmd(Option<Cmd>),
            Input(Option<Vec<u8>>),
            Read(std::io::Result<Vec<u8>>),
            Woke,
        }
        let got = {
            let read = async {
                match &term {
                    Some(b) => {
                        let mut buf = vec![0u8; CHUNK];
                        b.pty.read(&mut buf).await.map(|n| {
                            buf.truncate(n);
                            buf
                        })
                    }
                    None => std::future::pending().await,
                }
            };
            let sleep = async {
                match wake {
                    Some(t) => tokio::time::sleep_until(t.into()).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                c = cmds.recv() => Got::Cmd(c),
                i = input.recv() => Got::Input(i),
                r = read => Got::Read(r),
                _ = sleep => Got::Woke,
            }
        };
        let ended = Event::Ended {
            reason: "ended",
            message: String::new(),
        };
        match got {
            Got::Cmd(Some(Cmd::Go(n))) => go = Some(n.max(1)),
            Got::Cmd(Some(Cmd::Resize(c, r))) => {
                (cols, rows) = (c, r);
                if let Some(b) = &term {
                    let _ = b.pty.resize(c, r);
                }
            }
            // The actor dropped the pane.
            Got::Cmd(None) | Got::Input(None) => return end(term.take()).await,
            Got::Input(Some(bytes)) => {
                let failed = match &term {
                    Some(b) => b.pty.write_all(&bytes).await.is_err(),
                    None => false,
                };
                if failed {
                    end(term.take()).await;
                    return report(&tx, &tag, ended);
                }
            }
            Got::Read(Ok(bytes)) if !bytes.is_empty() => {
                gather.push(Instant::now(), &bytes);
                if gather.hold.len() > HOLD {
                    // Kept unread, tmux would buffer the flood whole:
                    // drop what wasn't sent and start a client afresh
                    // once the device can take a redraw.
                    gather.clear();
                    end(term.take()).await;
                    reattach = true;
                }
            }
            Got::Read(_) => {
                end(term.take()).await;
                return report(&tx, &tag, ended);
            }
            Got::Woke => {}
        }
    }
}

// ---- the actor's side ----

/// One open stream.
pub struct Pane {
    pub gen: u64,
    pub device: String,
    /// The session's id (the daemon's `target` from now on).
    pub session: String,
    pub cmds: mpsc::UnboundedSender<Cmd>,
    pub input: mpsc::Sender<Vec<u8>>,
    pub credit: Credit,
    /// A `Go` is with the task.
    pub go_out: bool,
    /// This device holds the pane (its keys reach it).
    pub control: bool,
    /// The device may type at all (not view-only).
    pub may_type: bool,
    pub cols: u16,
    pub rows: u16,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub typed: bool,
}

/// A `pane.open` on its way (counted against the limits).
pub struct Opening {
    pub device: String,
}

#[derive(Default)]
pub struct Panes {
    pub open: HashMap<(u32, u8), Pane>,
    pub opening: HashMap<(u32, u8), Opening>,
    /// The daemon's last `control` map (`web.subscribe`).
    pub control: Value,
    /// Each device's last `pane.take` target, until it is answered.
    pub taking: HashMap<String, String>,
    next_gen: u64,
}

impl Panes {
    fn count(&self, device: Option<&str>) -> usize {
        let of = |d: &str| device.is_none_or(|x| x == d);
        self.open.values().filter(|p| of(&p.device)).count()
            + self.opening.values().filter(|o| of(&o.device)).count()
    }

    fn streams_of(&self, link: u32) -> Vec<u8> {
        self.open
            .keys()
            .filter(|(l, _)| *l == link)
            .map(|(_, s)| *s)
            .collect()
    }
}

/// Whether `holder` (from the control map) is this device.
fn holds(control: &Value, session: &str, device: &str) -> Option<bool> {
    let entry = control.get(session)?;
    Some(entry["holder"].as_str() == Some(&format!("device:{device}")))
}

impl Actor {
    fn pane_error(&mut self, link: u32, id: &Value, code: &str, message: &str) {
        self.send_control(link, &error_answer(id, code, message));
    }

    /// `pane.open` from a live device (already through the rate limit).
    pub(crate) fn pane_open(&mut self, link: u32, req: &Value) {
        let id = req["id"].clone();
        let args = &req["args"];
        let Some(Phase::Live(live)) = self.links.get(&link).map(|l| &l.phase) else {
            return;
        };
        let device = live.device.clone();
        let epoch = self.links.get(&link).map(|l| l.epoch).unwrap_or(0);
        let stream = args["stream"]
            .as_u64()
            .filter(|s| u8::try_from(*s).is_ok_and(stream::is_pane))
            .map(|s| s as u8);
        let (Some(stream), Some((cols, rows)), Some(target)) =
            (stream, size(args), args["target"].as_str())
        else {
            return self.pane_error(
                link,
                &id,
                "bad_request",
                "pane.open needs a stream (1 to 250), a target and a size (2 to 1000)",
            );
        };
        if self.panes.open.contains_key(&(link, stream))
            || self.panes.opening.contains_key(&(link, stream))
        {
            return self.pane_error(link, &id, "bad_request", "that stream is open already");
        }
        if self.panes.count(Some(&device)) >= PER_DEVICE || self.panes.count(None) >= MAX_REMOTE {
            return self.pane_error(link, &id, "too_many", "too many panes are open");
        }
        let may_type = self.devices.by_id(&device).is_none_or(|d| d.input);
        self.panes.opening.insert(
            (link, stream),
            Opening {
                device: device.clone(),
            },
        );
        let (d, tx) = (self.d.clone(), self.tx_clone());
        let target = target.to_string();
        tokio::spawn(async move {
            let r = d
                .call(
                    "web.call",
                    json!({"op": "pane.open", "via": "remote", "device": device,
                        "args": {"stream": stream, "target": target, "cols": cols, "rows": rows}}),
                )
                .await;
            let result = match r {
                Err(e) => {
                    let code = match code_of(e.code) {
                        "failed" => "not_found",
                        c => c,
                    };
                    Err((code, e.message))
                }
                Ok(v) if v["running"] != true => Err(("not_running", "it isn't running".into())),
                Ok(v) => {
                    let t = v["tmux_session"].as_str().unwrap_or("").to_string();
                    let (c, r) = size(&v).unwrap_or((cols, rows));
                    let view = view_name();
                    let ro = !may_type;
                    let tt = t.clone();
                    match tokio::task::spawn_blocking(move || Bridge::attach(&tt, &view, c, r, ro))
                        .await
                    {
                        Ok(Ok(b)) => Ok((b, v, t, c, r)),
                        Ok(Err(e)) => Err(("not_running", format!("{e:#}"))),
                        Err(e) => Err(("failed", e.to_string())),
                    }
                }
            };
            let _ = tx.send(Input::PaneOpened {
                link,
                epoch,
                stream,
                id,
                device,
                may_type,
                result: Box::new(result),
            });
        });
    }

    /// A `pane.open` came back from the daemon (and the view client is
    /// attached, if it is running).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pane_opened(
        &mut self,
        link: u32,
        epoch: u64,
        stream: u8,
        id: Value,
        device: String,
        may_type: bool,
        result: OpenResult,
    ) {
        self.panes.opening.remove(&(link, stream));
        let live = self
            .links
            .get(&link)
            .is_some_and(|l| l.epoch == epoch && l.is_live());
        let (bridge, v, tmux_session, cols, rows) = match result {
            Err((code, message)) => {
                if live {
                    self.pane_error(link, &id, code, &message);
                }
                return;
            }
            Ok(x) => x,
        };
        let session = v["id"].as_str().unwrap_or("").to_string();
        if !live {
            // The link went meanwhile: no one to show it to.
            let mut b = bridge;
            tokio::task::spawn_blocking(move || b.end());
            self.pane_closed_daemon(&device, &session);
            return;
        }
        self.panes.next_gen += 1;
        let gen = self.panes.next_gen;
        let (ctx, crx) = mpsc::unbounded_channel();
        let (itx, irx) = mpsc::channel(INPUT_FRAMES);
        let control = holds(&self.panes.control, &session, &device).unwrap_or(v["control"] == true);
        let target = Target {
            tmux_session,
            read_only: !may_type,
        };
        tokio::spawn(run(
            bridge,
            target,
            cols,
            rows,
            crx,
            irx,
            self.tx_clone(),
            Tag {
                link,
                epoch,
                stream,
                gen,
            },
        ));
        self.panes.open.insert(
            (link, stream),
            Pane {
                gen,
                device: device.clone(),
                session: session.clone(),
                cmds: ctx,
                input: itx,
                credit: Credit::default(),
                go_out: false,
                control,
                may_type,
                cols,
                rows,
                bytes_in: 0,
                bytes_out: 0,
                typed: false,
            },
        );
        self.audit.log(
            &device,
            &format!(
                "pane open {session} {cols}x{rows} {}",
                if may_type { "rw" } else { "ro" }
            ),
        );
        let ok = json!({
            "stream": stream,
            "id": v["id"],
            "name": v["name"],
            "overlord": v["overlord"] == true,
            "input": may_type,
            "control": v["control"] == true,
            "cols": cols,
            "rows": rows,
            "window": v["window"],
        });
        self.send_control(link, &json!({"id": id, "ok": ok}));
        self.pane_pump(link, stream);
    }

    /// Hand the stream's task a `Go` if the credit and the link's queue
    /// allow one.
    pub(crate) fn pane_pump(&mut self, link: u32, stream: u8) {
        let Some(q) = self.queue() else {
            return;
        };
        let Some(p) = self.panes.open.get_mut(&(link, stream)) else {
            return;
        };
        if p.go_out {
            return;
        }
        let Some(n) = p.credit.grant() else {
            return;
        };
        // (Asks to be woken when the queue falls below the mark.)
        if !q.below_mark(link) {
            return;
        }
        if p.cmds.send(Cmd::Go(n)).is_ok() {
            p.go_out = true;
        }
    }

    /// The link's queue fell below the mark.
    pub(crate) fn pane_link_low(&mut self, link: u32) {
        for s in self.panes.streams_of(link) {
            self.pane_pump(link, s);
        }
    }

    /// Something from a pane's task.
    pub(crate) fn pane_event(&mut self, link: u32, epoch: u64, stream: u8, gen: u64, ev: Event) {
        let ok = self.links.get(&link).is_some_and(|l| l.epoch == epoch)
            && self
                .panes
                .open
                .get(&(link, stream))
                .is_some_and(|p| p.gen == gen);
        if !ok {
            return;
        }
        match ev {
            Event::Data(bytes) => {
                let plain = match stream::pane_frame(stream, &bytes) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                if let Some(p) = self.panes.open.get_mut(&(link, stream)) {
                    p.go_out = false;
                    p.credit.sent(bytes.len());
                    p.bytes_out += bytes.len() as u64;
                }
                if self.send_plain(link, &plain) {
                    self.pane_pump(link, stream);
                }
            }
            Event::Ended { reason, message } => {
                self.pane_end(link, stream, Some((reason, &message)));
            }
        }
    }

    /// A pane frame from the device: input for the PTY, if this device
    /// holds the pane and may type; else discarded (as is one for a stream
    /// that isn't open).
    pub(crate) fn pane_input(&mut self, link: u32, stream: u8, bytes: Vec<u8>) {
        let Some(p) = self.panes.open.get_mut(&(link, stream)) else {
            return;
        };
        if !p.control || !p.may_type || bytes.is_empty() {
            return;
        }
        let n = bytes.len() as u64;
        match p.input.try_send(bytes) {
            Ok(()) => {
                p.bytes_in += n;
                if !p.typed {
                    p.typed = true;
                    let (d, s) = (p.device.clone(), p.session.clone());
                    self.audit.log(&d, &format!("pane input {s}"));
                }
            }
            Err(_) => self.pane_end(
                link,
                stream,
                Some(("overflow", "input came faster than the pane took it")),
            ),
        }
    }

    /// `{"event":"pane",…}` from a live device.
    pub(crate) fn pane_device_event(&mut self, link: u32, ev: &Value) {
        let Some(stream) = ev["stream"]
            .as_u64()
            .and_then(|s| u8::try_from(s).ok())
            .filter(|s| stream::is_pane(*s))
        else {
            return;
        };
        if !self.panes.open.contains_key(&(link, stream)) {
            return;
        }
        match ev["type"].as_str() {
            Some("ack") => {
                let (b, f) = (
                    ev["bytes"].as_u64().unwrap_or(0),
                    ev["frames"].as_u64().unwrap_or(0),
                );
                let r = self
                    .panes
                    .open
                    .get_mut(&(link, stream))
                    .map(|p| p.credit.ack(b, f));
                match r {
                    Some(Ok(())) => self.pane_pump(link, stream),
                    _ => self.pane_end(
                        link,
                        stream,
                        Some(("failed", "acknowledged more than was sent")),
                    ),
                }
            }
            Some("resize") => {
                let Some((cols, rows)) = size(ev) else {
                    return;
                };
                let Some(p) = self.panes.open.get_mut(&(link, stream)) else {
                    return;
                };
                (p.cols, p.rows) = (cols, rows);
                let _ = p.cmds.send(Cmd::Resize(cols, rows));
                let (d, device, session) = (self.d.clone(), p.device.clone(), p.session.clone());
                // Pins the window again if this device holds the pane.
                tokio::spawn(async move {
                    let _ = d
                        .call(
                            "web.pane_size",
                            json!({"target": session, "surface": format!("device:{device}"),
                                "cols": cols, "rows": rows}),
                        )
                        .await;
                });
            }
            Some("close") => self.pane_end(link, stream, Some(("closed", ""))),
            _ => {}
        }
    }

    /// End a stream: its task stops (the view client goes), the device is
    /// told why (`say`: None when the link itself is going), the daemon
    /// that this surface stopped showing it.
    pub(crate) fn pane_end(&mut self, link: u32, stream: u8, say: Option<(&str, &str)>) {
        let Some(p) = self.panes.open.remove(&(link, stream)) else {
            return;
        };
        self.audit.log(
            &p.device,
            &format!(
                "pane close {} in={} out={}",
                p.session, p.bytes_in, p.bytes_out
            ),
        );
        // (Dropping the channels ends the task, and with it the client.)
        drop(p.cmds);
        drop(p.input);
        if let Some((reason, message)) = say {
            self.send_control(
                link,
                &json!({"event": "pane", "stream": stream, "type": "closed",
                    "reason": reason, "message": message}),
            );
        }
        let still = self
            .panes
            .open
            .values()
            .any(|o| o.device == p.device && o.session == p.session);
        if !still {
            self.pane_closed_daemon(&p.device, &p.session);
        }
    }

    fn pane_closed_daemon(&self, device: &str, session: &str) {
        if session.is_empty() {
            return;
        }
        let d = self.d.clone();
        let params = json!({"target": session, "surface": format!("device:{device}")});
        tokio::spawn(async move {
            let _ = d.call("web.pane_closed", params).await;
        });
    }

    /// Every pane of a link that is going (dropped, unlinked, the
    /// connection lost): their view clients end with it.
    pub(crate) fn pane_end_link(&mut self, link: u32) {
        self.panes.opening.retain(|(l, _), _| *l != link);
        for s in self.panes.streams_of(link) {
            self.pane_end(link, s, None);
        }
    }

    /// The daemon's control map changed: who may type where, and the size
    /// a viewer's client draws at.
    pub(crate) fn pane_control(&mut self, sessions: Value) {
        for p in self.panes.open.values_mut() {
            let Some(entry) = sessions.get(&p.session) else {
                p.control = false;
                continue;
            };
            p.control = entry["holder"].as_str() == Some(&format!("device:{}", p.device));
            if !p.control {
                if let Some((c, r)) = size(entry) {
                    if (c, r) != (p.cols, p.rows) {
                        (p.cols, p.rows) = (c, r);
                        let _ = p.cmds.send(Cmd::Resize(c, r));
                    }
                }
            }
        }
        self.panes.control = sessions;
    }

    /// The device's view-only switch changed: its open panes end (they
    /// were attached the other way), and it opens them again.
    pub(crate) fn pane_input_changed(&mut self, device: &str) {
        let streams: Vec<(u32, u8)> = self
            .panes
            .open
            .iter()
            .filter(|(_, p)| p.device == device)
            .map(|(k, _)| *k)
            .collect();
        for (l, s) in streams {
            self.pane_end(l, s, Some(("closed", "this device's typing was switched")));
        }
    }
}

/// What a `pane.open` attempt came to.
pub type OpenResult = Result<(Bridge, Value, String, u16, u16), (&'static str, String)>;

impl Actor {
    /// A device's `pane.take` is on its way to the daemon.
    pub(crate) fn pane_taking(&mut self, device: &str, target: &str) {
        self.panes
            .taking
            .insert(device.to_string(), target.to_string());
    }

    /// The daemon answered a device's `pane.take`: if it took the pane, its
    /// keys reach it at once (the control line that says so may come a
    /// moment after the answer, and a key typed between is not lost).
    pub(crate) fn pane_took(&mut self, device: &str, answer: &Value) {
        let Some(target) = self.panes.taking.remove(device) else {
            return;
        };
        if answer["ok"]["control"] != true {
            return;
        }
        for p in self.panes.open.values_mut() {
            if p.device == device && p.session == target {
                p.control = true;
            }
        }
    }
}

/// The `id` of `pane.take` and `op` names: `pane.take` goes the usual way.
pub fn audit_take(req: &Value) -> Option<String> {
    (req["op"] == "pane.take").then(|| {
        req["args"]["target"]
            .as_str()
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_stops_reading() {
        let mut c = Credit::default();
        assert_eq!(c.grant(), Some(CHUNK));
        // Four full frames: the window is spent.
        for _ in 0..4 {
            let n = c.grant().unwrap();
            c.sent(n);
        }
        assert_eq!(c.sent_bytes, 4 * CHUNK as u64);
        let left = c.grant().unwrap();
        assert_eq!(left as u64, WINDOW - 4 * CHUNK as u64);
        c.sent(left);
        assert_eq!(c.grant(), None);
        // The page parsed two frames' worth.
        c.ack(2 * CHUNK as u64, 2).unwrap();
        assert_eq!(c.grant(), Some(CHUNK));
        // Frames count too: 48 tiny frames stop it as well.
        let mut c = Credit::default();
        for _ in 0..FRAMES {
            c.sent(1);
        }
        assert_eq!(c.grant(), None);
        c.ack(FRAMES, 1).unwrap();
        assert!(c.grant().is_some());
    }

    #[test]
    fn ack_past_sent_closes_the_stream() {
        let mut c = Credit::default();
        c.sent(100);
        assert!(c.ack(101, 1).is_err());
        assert!(c.ack(100, 2).is_err());
        assert!(c.ack(100, 1).is_ok());
        // An older one changes nothing.
        assert!(c.ack(50, 1).is_ok());
        assert_eq!((c.acked_bytes, c.acked_frames), (100, 1));
    }

    #[test]
    fn a_quiet_pane_sends_at_once_and_a_busy_one_gathers() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut g = Gather::default();
        // A keystroke's echo after quiet: at once.
        g.push(ms(0), b"a");
        assert!(g.ready(ms(0), CHUNK));
        assert_eq!(g.take(ms(0), CHUNK), b"a");
        // Busy: reads 5 ms apart gather.
        g.push(ms(5), b"b");
        assert!(!g.ready(ms(5), CHUNK));
        assert_eq!(g.deadline(), Some(ms(20)));
        g.push(ms(10), b"c");
        assert!(!g.ready(ms(14), CHUNK));
        // 15 ms after the first of them, they go together.
        assert!(g.ready(ms(20), CHUNK));
        assert_eq!(g.take(ms(20), CHUNK), b"bc");
        // A full frame goes at once; what is left follows at once.
        g.push(ms(25), &[1; 10]);
        assert!(g.ready(ms(25), 4));
        assert_eq!(g.take(ms(25), 4).len(), 4);
        assert!(g.ready(ms(25), 4));
        // After 15 ms of quiet, at once again.
        let mut g = Gather::default();
        g.push(ms(0), b"x");
        g.take(ms(0), 9);
        g.push(ms(40), b"y");
        assert!(g.ready(ms(40), CHUNK));
        // The reset goes in front.
        g.prefix(ms(40), RESET);
        assert_eq!(g.take(ms(40), 99), b"\x18\x1bcy");
    }

    #[test]
    fn the_rate_bounds_a_flood() {
        let t0 = Instant::now();
        let mut r = Rate::new(t0);
        // The burst goes at once.
        let mut sent = 0;
        while r.wait(t0, CHUNK).is_zero() {
            r.spend(t0, CHUNK);
            sent += CHUNK;
        }
        assert!(sent as f64 >= BURST - CHUNK as f64 && sent as f64 <= BURST);
        // Then about RATE a second: ten seconds of a flood.
        let mut t = t0;
        let mut later = 0usize;
        let end = t0 + Duration::from_secs(10);
        while t < end {
            let w = r.wait(t, CHUNK);
            if w.is_zero() {
                r.spend(t, CHUNK);
                later += CHUNK;
            } else {
                t += w;
            }
        }
        let per_sec = later as f64 / 10.0;
        assert!((per_sec - RATE).abs() < RATE * 0.05, "{per_sec}");
    }

    #[tokio::test]
    async fn input_overflow_closes_the_stream_only() {
        // The input queue is bounded: one frame more than it holds fails,
        // which the actor answers by closing that stream (`overflow`),
        // not the link.
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(INPUT_FRAMES);
        for _ in 0..INPUT_FRAMES {
            tx.try_send(vec![1]).unwrap();
        }
        assert!(tx.try_send(vec![1]).is_err());
        rx.recv().await.unwrap();
        assert!(tx.try_send(vec![1]).is_ok());
    }

    #[test]
    fn holders_are_devices_by_id() {
        let m = json!({"s1": {"holder": "device:abc"}, "s2": {"holder": "terminal"}});
        assert_eq!(holds(&m, "s1", "abc"), Some(true));
        assert_eq!(holds(&m, "s1", "abd"), Some(false));
        assert_eq!(holds(&m, "s2", "abc"), Some(false));
        assert_eq!(holds(&m, "s3", "abc"), None);
        assert_eq!(
            hello_limits(),
            json!({"max": 2, "window": 65536, "frames": 48})
        );
    }
}
