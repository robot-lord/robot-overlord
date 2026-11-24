//! The device engine (`rlord_e2e_client::Engine`, what the phone app
//! runs) with software keys and a plain vault, driven the way the page
//! drives it: JSON messages in, JSON messages out. The CLI and the engine
//! tests (`crates/web/tests/engine.rs`) use it, so they exercise the
//! phone's own code.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
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
        })
    }

    fn take(&mut self, o: Out) {
        match o {
            Out::Page(s) => {
                if let Ok(v) = serde_json::from_str(&s) {
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

    /// `background_check`, off the async threads.
    pub async fn background_check(&self, budget_ms: u64) -> BackgroundOutcome {
        let e = &self.engine;
        tokio::task::block_in_place(|| e.background_check(budget_ms))
    }
}
