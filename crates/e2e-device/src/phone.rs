//! The device engine (`rlord_e2e_client::Engine`, what the phone app
//! runs) with software keys and a plain vault, driven the way the page
//! drives it: JSON messages in, JSON messages out. The CLI and the engine
//! tests (`crates/web/tests/engine.rs`) use it, so they exercise the
//! phone's own code.
//!
//! Panes go through it as the page's terminal drives them (`pane_open`,
//! `pane_input`, `pane_ack`, `pane_resize`, `pane_close`); what a pane
//! drew since its last open is kept per stream ([`PaneLog`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use serde_json::{json, Value};

use rlord_e2e_client::soft::{Collect, Out, PlainVault, SoftSigners};
use rlord_e2e_client::{BackgroundOutcome, Config, Engine, Tuning};

pub struct Phone {
    pub engine: Engine,
    pub dir: PathBuf,
    pub signers: Arc<SoftSigners>,
    rx: tokio::sync::mpsc::UnboundedReceiver<Out>,
    /// Every message the engine sent the page, in order.
    pub log: Vec<Value>,
    /// Notifications asked for (kind, count).
    pub notified: Vec<(String, u32)>,
    /// `wait` looks at messages from here on.
    mark: usize,
    /// Messages a `wait` returned already.
    taken: std::collections::HashSet<usize>,
    next: u64,
    /// Each pane stream's output since its last `pane_open`.
    panes: HashMap<u8, PaneLog>,
}

/// What one pane stream sent the page since it was last opened.
#[derive(Clone, Debug, Default)]
pub struct PaneLog {
    /// Every `data` message's bytes, in order.
    pub bytes: Vec<u8>,
    /// `data` messages (one per host frame).
    pub frames: u64,
    /// The `closed` event, once it came.
    pub closed: Option<Value>,
}

fn std_b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

impl Phone {
    /// An engine over `dir` (its records; keys in `dir/keys`).
    pub fn open(dir: &Path) -> Result<Phone> {
        Phone::open_tuned(dir, Tuning::default())
    }

    pub fn open_tuned(dir: &Path, tuning: Tuning) -> Result<Phone> {
        let signers = Arc::new(SoftSigners::in_dir(dir.join("keys")));
        let (listener, rx) = Collect::new();
        let engine = Engine::with_tuning(
            Config {
                dir: dir.to_path_buf(),
                ca_pem: None,
            },
            Arc::new(PlainVault),
            signers.clone(),
            Arc::new(listener),
            tuning,
        )
        .map_err(|e| anyhow!("{e}"))?;
        Ok(Phone {
            engine,
            dir: dir.to_path_buf(),
            signers,
            rx,
            log: Vec::new(),
            notified: Vec::new(),
            mark: 0,
            taken: Default::default(),
            next: 0,
            panes: HashMap::new(),
        })
    }

    fn take(&mut self, o: Out) {
        match o {
            Out::Page(s) => {
                if let Ok(v) = serde_json::from_str::<Value>(&s) {
                    self.pane_message(&v);
                    self.log.push(v);
                }
            }
            Out::Notify(k, n) => self.notified.push((k, n)),
        }
    }

    /// Takes in what the engine has said so far.
    pub fn drain(&mut self) {
        while let Ok(o) = self.rx.try_recv() {
            self.take(o);
        }
    }

    /// Later `wait`s only see what comes after now.
    pub fn mark(&mut self) {
        self.drain();
        self.mark = self.log.len();
    }

    /// The first message since the mark that `pick` accepts (it is not
    /// returned again).
    pub async fn wait(
        &mut self,
        what: &str,
        limit: Duration,
        pick: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        let until = tokio::time::Instant::now() + limit;
        loop {
            self.drain();
            let found = (self.mark..self.log.len())
                .find(|i| !self.taken.contains(i) && pick(&self.log[*i]));
            if let Some(i) = found {
                self.taken.insert(i);
                return Ok(self.log[i].clone());
            }
            match tokio::time::timeout_at(until, self.rx.recv()).await {
                Ok(Some(o)) => self.take(o),
                Ok(None) => bail!("the engine stopped while waiting for {what}"),
                Err(_) => bail!(
                    "timed out waiting for {what}; the engine said: {}",
                    self.tail()
                ),
            }
        }
    }

    fn tail(&self) -> String {
        let from = self.log.len().saturating_sub(8);
        self.log[from..]
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n  ")
    }

    /// The latest `conn` state.
    pub fn conn_state(&mut self) -> String {
        self.drain();
        self.log
            .iter()
            .rev()
            .find(|v| v["event"] == "conn")
            .and_then(|v| v["state"].as_str())
            .unwrap_or("")
            .to_string()
    }

    /// Waits until the connection state is `state`.
    pub async fn conn(&mut self, state: &str, limit: Duration) -> Result<()> {
        if self.conn_state() == state {
            return Ok(());
        }
        self.mark();
        self.wait(&format!("conn {state}"), limit, |v| {
            v["event"] == "conn" && v["state"] == state
        })
        .await?;
        Ok(())
    }

    /// A page message (an `app` message or a request) as is.
    pub fn say(&self, v: Value) {
        self.engine.page_message(v.to_string());
    }

    /// Sends a request as the page does; its page id.
    pub fn send(&mut self, op: &str, args: Value) -> u64 {
        self.next += 1;
        self.say(json!({"id": self.next, "op": op, "args": args}));
        self.next
    }

    /// The reply to page request `id`.
    pub async fn reply(&mut self, id: u64, limit: Duration) -> Result<Value> {
        self.drain();
        if let Some(v) = self.log.iter().find(|v| v["id"] == id) {
            return Ok(v.clone());
        }
        let mark = self.mark;
        self.mark = 0;
        let r = self
            .wait(&format!("the reply to {id}"), limit, |v| v["id"] == id)
            .await;
        self.mark = mark;
        r
    }

    /// Sends a request and waits for its reply.
    pub async fn request(&mut self, op: &str, args: Value, limit: Duration) -> Result<Value> {
        let id = self.send(op, args);
        self.reply(id, limit).await
    }

    /// The latest `outbox` event's items.
    pub fn outbox(&mut self) -> Vec<Value> {
        self.drain();
        self.log
            .iter()
            .rev()
            .find(|v| v["event"] == "outbox")
            .and_then(|v| v["items"].as_array().cloned())
            .unwrap_or_default()
    }

    /// Scans `qr` and continues as `name`: the code the phone shows, once
    /// the host waits for its user.
    pub async fn start_pairing(&mut self, qr: &str, name: &str) -> Result<String> {
        self.mark();
        self.engine.scanned(qr.to_string(), "Test Phone".into());
        let ev = self
            .wait("pairing scanned", Duration::from_secs(5), |v| {
                v["event"] == "pairing"
            })
            .await?;
        if ev["state"] != "scanned" {
            bail!("pairing: {ev}");
        }
        self.say(json!({"app": "pair_continue", "name": name}));
        let ev = self
            .wait("pairing waiting", Duration::from_secs(15), |v| {
                v["event"] == "pairing" && v["state"] != "connecting"
            })
            .await?;
        if ev["state"] != "waiting" {
            bail!("pairing: {ev}");
        }
        Ok(ev["code"].as_str().unwrap_or("").to_string())
    }

    /// The pairing's end: Ok once done.
    pub async fn pairing_outcome(&mut self, limit: Duration) -> Result<()> {
        let ev = self
            .wait("the pairing's outcome", limit, |v| {
                v["event"] == "pairing"
                    && matches!(v["state"].as_str(), Some("done" | "rejected" | "failed"))
            })
            .await?;
        if ev["state"] != "done" {
            bail!("pairing {}: {}", ev["state"], ev["reason"]);
        }
        Ok(())
    }

    /// The engine's `status` event.
    pub async fn status(&mut self) -> Result<Value> {
        self.mark();
        self.say(json!({"app": "status"}));
        self.wait("status", Duration::from_secs(5), |v| v["event"] == "status")
            .await
    }

    // ---- panes ----

    fn pane_message(&mut self, v: &Value) {
        if v["event"] != "pane" {
            return;
        }
        let Some(stream) = v["stream"].as_u64().and_then(|s| u8::try_from(s).ok()) else {
            return;
        };
        let log = self.panes.entry(stream).or_default();
        match v["type"].as_str() {
            Some("data") => {
                let b = v["b"].as_str().unwrap_or("");
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b) {
                    log.bytes.extend_from_slice(&bytes);
                    log.frames += 1;
                }
            }
            Some("closed") => log.closed = Some(v.clone()),
            _ => {}
        }
    }

    /// Opens a pane on `stream` (1 to 250; reuse one only after its
    /// `closed`) for `target` (`overlord` or a session reference) at
    /// `cols` x `rows`, as the page does; the host's answer (`ok` with
    /// `stream`, `id`, `control`, `cols`, `rows`, … or `error`). The
    /// stream's [`PaneLog`] starts empty.
    pub async fn pane_open(
        &mut self,
        stream: u8,
        target: &str,
        cols: u16,
        rows: u16,
        limit: Duration,
    ) -> Result<Value> {
        self.drain();
        self.panes.insert(stream, PaneLog::default());
        self.request(
            "pane.open",
            json!({"stream": stream, "target": target, "cols": cols, "rows": rows}),
            limit,
        )
        .await
    }

    /// `pane.take` for `target` at `cols` x `rows`; the host's answer.
    pub async fn pane_take(
        &mut self,
        target: &str,
        cols: u16,
        rows: u16,
        limit: Duration,
    ) -> Result<Value> {
        self.request(
            "pane.take",
            json!({"target": target, "cols": cols, "rows": rows}),
            limit,
        )
        .await
    }

    /// Types `bytes` into the pane (the engine splits them into frames).
    pub fn pane_input(&self, stream: u8, bytes: &[u8]) {
        self.say(json!({"pane": "input", "stream": stream, "b": std_b64(bytes)}));
    }

    /// The pane's view changed size.
    pub fn pane_resize(&self, stream: u8, cols: u16, rows: u16) {
        self.say(json!({"pane": "resize", "stream": stream, "cols": cols, "rows": rows}));
    }

    /// Acknowledges `bytes` and `frames` consumed (cumulative totals).
    pub fn pane_ack(&self, stream: u8, bytes: u64, frames: u64) {
        self.say(json!({"pane": "ack", "stream": stream, "bytes": bytes, "frames": frames}));
    }

    /// Acknowledges everything received on the stream so far (what the
    /// page does once its terminal has parsed it).
    pub fn pane_ack_all(&mut self, stream: u8) {
        let log = self.pane(stream);
        self.pane_ack(stream, log.bytes.len() as u64, log.frames);
    }

    /// Asks the host to close the pane (its `closed` follows).
    pub fn pane_close(&self, stream: u8) {
        self.say(json!({"pane": "close", "stream": stream}));
    }

    /// What the stream sent since its last `pane_open`, so far.
    pub fn pane(&mut self, stream: u8) -> PaneLog {
        self.drain();
        self.panes.get(&stream).cloned().unwrap_or_default()
    }

    /// Waits until the stream's output since its last open satisfies
    /// `done` (`what` names it in errors); that output. Fails if the
    /// stream is closed first.
    pub async fn pane_until(
        &mut self,
        stream: u8,
        what: &str,
        limit: Duration,
        done: impl Fn(&[u8]) -> bool,
    ) -> Result<Vec<u8>> {
        let until = tokio::time::Instant::now() + limit;
        loop {
            self.drain();
            if let Some(log) = self.panes.get(&stream) {
                if done(&log.bytes) {
                    return Ok(log.bytes.clone());
                }
                if let Some(c) = &log.closed {
                    bail!("pane {stream} closed while waiting for {what}: {c}");
                }
            }
            match tokio::time::timeout_at(until, self.rx.recv()).await {
                Ok(Some(o)) => self.take(o),
                Ok(None) => bail!("the engine stopped while waiting for {what}"),
                Err(_) => {
                    let got = self.panes.get(&stream).map(|l| l.bytes.clone());
                    bail!(
                        "timed out waiting for {what} on pane {stream}; it sent {:?}",
                        String::from_utf8_lossy(&got.unwrap_or_default())
                    )
                }
            }
        }
    }

    /// Waits until the pane's output contains `text`.
    pub async fn pane_text(&mut self, stream: u8, text: &str, limit: Duration) -> Result<Vec<u8>> {
        let needle = text.as_bytes().to_vec();
        self.pane_until(stream, text, limit, move |b| {
            needle.is_empty() || b.windows(needle.len()).any(|w| w == needle.as_slice())
        })
        .await
    }

    /// Waits for the stream's `closed` event (since its last open).
    pub async fn pane_closed(&mut self, stream: u8, limit: Duration) -> Result<Value> {
        let until = tokio::time::Instant::now() + limit;
        loop {
            self.drain();
            if let Some(c) = self.panes.get(&stream).and_then(|l| l.closed.clone()) {
                return Ok(c);
            }
            match tokio::time::timeout_at(until, self.rx.recv()).await {
                Ok(Some(o)) => self.take(o),
                Ok(None) => bail!("the engine stopped while waiting for pane {stream}'s closed"),
                Err(_) => bail!(
                    "timed out waiting for pane {stream}'s closed; the engine said: {}",
                    self.tail()
                ),
            }
        }
    }

    /// `background_check`, off the async threads.
    pub async fn background_check(&self, budget_ms: u64) -> BackgroundOutcome {
        let e = &self.engine;
        tokio::task::block_in_place(|| e.background_check(budget_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rlord-phone-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn page(p: &mut Phone, v: Value) {
        p.take(Out::Page(v.to_string()));
    }

    /// A pane's output is kept per stream, from its last open; `closed`
    /// is noted. (The engine's side of panes is `machine`'s tests; the
    /// host's, `crates/web/tests/remote_pane.rs`.)
    #[tokio::test(flavor = "multi_thread")]
    async fn pane_logs_follow_the_engines_messages() {
        let dir = scratch("panes");
        let mut p = Phone::open(&dir).unwrap();
        let data = |s: u8, b: &[u8]| json!({"event": "pane", "stream": s, "type": "data", "b": std_b64(b)});
        page(&mut p, data(1, b"hel"));
        page(&mut p, data(1, b"lo"));
        page(&mut p, data(2, b"other"));
        let log = p.pane(1);
        assert_eq!(log.bytes, b"hello");
        assert_eq!(log.frames, 2);
        assert!(log.closed.is_none());
        let out = p.pane_text(1, "llo", Duration::from_secs(1)).await.unwrap();
        assert_eq!(out, b"hello");
        page(
            &mut p,
            json!({"event": "pane", "stream": 1, "type": "closed", "reason": "link"}),
        );
        assert_eq!(
            p.pane_closed(1, Duration::from_secs(1)).await.unwrap()["reason"],
            "link"
        );
        assert!(p
            .pane_text(1, "never", Duration::from_secs(1))
            .await
            .is_err());
        // Opening again starts the log over; unpaired, the engine refuses.
        let r = p
            .pane_open(1, "overlord", 54, 20, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(r["error"]["code"], "disconnected", "{r}");
        assert_eq!(p.pane(1).frames, 0);
        assert_eq!(p.pane(2).bytes, b"other");
        p.engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
