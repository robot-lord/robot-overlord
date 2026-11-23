//! The machine's IO: tokio, the websocket (`link`), timers, storage and
//! the platform's callbacks. One task runs the [`Machine`]; each link is a
//! task of its own that reports what it reads and sends what it is told,
//! in order. Actions are carried out in the order the machine queued them,
//! so a `Save` is on disk before the frame queued after it is handed to
//! the link (and if the save fails, nothing queued after it is sent).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use crate::link::{self, KeySigner};
use crate::machine::{Action, DialError, Input, Machine, Now};
use crate::store::{self, Record};
use crate::{BackgroundOutcome, Listener, Signers, Vault};

/// What the engine's facade sends the driver.
pub enum Cmd {
    Input(Input),
    /// Start a background check; its outcome goes to `reply`.
    Background {
        budget_ms: u64,
        reply: std::sync::mpsc::SyncSender<BackgroundOutcome>,
    },
    Shutdown,
}

enum LinkCmd {
    Text(String),
    Binary(Vec<u8>),
    Ping,
    Close(u16),
}

/// The clock the machine reads through the driver.
#[derive(Clone, Copy)]
pub struct Clock {
    start: Instant,
}

impl Clock {
    pub fn new() -> Clock {
        Clock {
            start: Instant::now(),
        }
    }

    pub fn now(&self) -> Now {
        Now {
            mono: self.start.elapsed().as_millis() as u64,
            wall: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }
}

impl Default for Clock {
    fn default() -> Self {
        Clock::new()
    }
}

/// Reads the sealed host records in `dir` (ones that don't open are left
/// alone).
pub fn load(dir: &std::path::Path, vault: &dyn Vault) -> Vec<Record> {
    store::host_files(dir)
        .into_iter()
        .filter_map(|(_, p)| std::fs::read(p).ok())
        .filter_map(|b| vault.open(b).ok())
        .filter_map(|b| Record::decode(&b))
        .collect()
}

pub struct Driver {
    machine: Machine,
    dir: PathBuf,
    tls: Arc<rustls::ClientConfig>,
    vault: Arc<dyn Vault>,
    signers: Arc<dyn Signers>,
    listener: Arc<dyn Listener>,
    clock: Clock,
    links: HashMap<u64, (mpsc::UnboundedSender<LinkCmd>, tokio::task::JoinHandle<()>)>,
    ev_tx: mpsc::UnboundedSender<Input>,
    ev_rx: mpsc::UnboundedReceiver<Input>,
    bg_reply: Option<std::sync::mpsc::SyncSender<BackgroundOutcome>>,
}

impl Driver {
    pub fn new(
        machine: Machine,
        dir: PathBuf,
        tls: Arc<rustls::ClientConfig>,
        vault: Arc<dyn Vault>,
        signers: Arc<dyn Signers>,
        listener: Arc<dyn Listener>,
    ) -> Driver {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        Driver {
            machine,
            dir,
            tls,
            vault,
            signers,
            listener,
            clock: Clock::new(),
            links: HashMap::new(),
            ev_tx,
            ev_rx,
            bg_reply: None,
        }
    }

    pub async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<Cmd>) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let wake = self.machine.deadline().map(|d| {
                let now = self.clock.now().mono;
                tokio::time::Instant::now() + Duration::from_millis(d.saturating_sub(now))
            });
            let sleep = async {
                match wake {
                    Some(w) => tokio::time::sleep_until(w).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                c = cmds.recv() => match c {
                    None | Some(Cmd::Shutdown) => break,
                    Some(Cmd::Input(i)) => self.handle(i),
                    Some(Cmd::Background { budget_ms, reply }) => {
                        if let Some(old) = self.bg_reply.replace(reply) {
                            let _ = old.try_send(BackgroundOutcome::Failed {
                                reason: "another check started".into(),
                            });
                        }
                        self.handle(Input::Background { budget_ms });
                    }
                },
                Some(ev) = self.ev_rx.recv() => self.handle(ev),
                _ = tick.tick() => self.handle(Input::Tick),
                _ = sleep => self.handle(Input::Tick),
            }
        }
        for (_, (tx, task)) in self.links.drain() {
            let _ = tx.send(LinkCmd::Close(1000));
            drop(tx);
            let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
        }
    }

    fn handle(&mut self, input: Input) {
        self.machine.handle(self.clock.now(), input);
        self.execute();
    }

    fn execute(&mut self) {
        let mut failed: Option<String> = None;
        while let Some(a) = self.machine.poll() {
            match a {
                Action::Save { file, plain } => {
                    if let Err(e) = self.save(&file, plain) {
                        failed.get_or_insert(e);
                    }
                }
                Action::Text { link, json } => {
                    if failed.is_none() {
                        self.link_cmd(link, LinkCmd::Text(json));
                    }
                }
                Action::Binary { link, bytes } => {
                    if failed.is_none() {
                        self.link_cmd(link, LinkCmd::Binary(bytes));
                    }
                }
                Action::Ping { link } => self.link_cmd(link, LinkCmd::Ping),
                Action::Close { link, code } => {
                    if let Some((tx, _task)) = self.links.remove(&link) {
                        // The task sends what was queued, then closes.
                        let _ = tx.send(LinkCmd::Close(code));
                    }
                }
                Action::Dial {
                    link,
                    pin,
                    role,
                    target,
                    label,
                } => self.dial(link, pin, role, target, label),
                Action::Delete { file } => {
                    let _ = store::remove(&self.dir, &file);
                }
                Action::ToPage(json) => self.listener.to_page(json),
                Action::Notify { kind, count } => self.listener.notify(kind, count),
                Action::BackgroundDone(o) => {
                    if let Some(r) = self.bg_reply.take() {
                        let _ = r.try_send(o);
                    }
                }
            }
        }
        if let Some(reason) = failed {
            self.machine
                .handle(self.clock.now(), Input::SaveFailed { reason });
            self.execute();
        }
    }

    fn save(&self, file: &str, plain: Vec<u8>) -> Result<(), String> {
        let sealed = self.vault.seal(plain).map_err(|e| e.message)?;
        store::write_atomic(&self.dir, file, &sealed).map_err(|e| e.to_string())
    }

    fn link_cmd(&self, link: u64, c: LinkCmd) {
        if let Some((tx, _)) = self.links.get(&link) {
            let _ = tx.send(c);
        }
    }

    fn dial(
        &mut self,
        link: u64,
        pin: rlord_relay_proto::RelayAddress,
        role: rlord_relay_proto::Role,
        target: Vec<u8>,
        label: String,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let ev = self.ev_tx.clone();
        let tls = self.tls.clone();
        let signers = self.signers.clone();
        let task = tokio::spawn(async move {
            let signer = match KeySigner::new(signers, &label) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ev.send(Input::Dialed {
                        link,
                        result: Err(DialError::Other(format!("no relay key: {e}"))),
                    });
                    return;
                }
            };
            let ev2 = ev.clone();
            let r = link::connect(&pin, &signer, role, &target, tls, move |url, code| {
                let _ = ev2.send(Input::Enroll { link, url, code });
            })
            .await;
            match r {
                Err(e) => {
                    let _ = ev.send(Input::Dialed {
                        link,
                        result: Err(e),
                    });
                }
                Ok((ws, linked)) => {
                    let _ = ev.send(Input::Dialed {
                        link,
                        result: Ok(linked),
                    });
                    pump(link, ws, rx, ev).await;
                }
            }
        });
        if let Some((_, old)) = self.links.insert(link, (tx, task)) {
            old.abort();
        }
    }
}

/// One link after `ready`: what the relay sends goes to the machine;
/// what the machine sends goes out, in order.
async fn pump(
    link: u64,
    ws: link::Ws,
    mut rx: mpsc::UnboundedReceiver<LinkCmd>,
    ev: mpsc::UnboundedSender<Input>,
) {
    let (mut sink, mut stream) = ws.split();
    loop {
        tokio::select! {
            c = rx.recv() => {
                let m = match c {
                    Some(LinkCmd::Text(t)) => Message::Text(t.into()),
                    Some(LinkCmd::Binary(b)) => Message::Binary(b.into()),
                    Some(LinkCmd::Ping) => Message::Ping(Vec::new().into()),
                    Some(LinkCmd::Close(code)) => {
                        let _ = sink
                            .send(Message::Close(Some(CloseFrame {
                                code: CloseCode::from(code),
                                reason: "".into(),
                            })))
                            .await;
                        let _ = tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
                        return;
                    }
                    None => {
                        let _ = tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
                        return;
                    }
                };
                if sink.send(m).await.is_err() {
                    let _ = ev.send(Input::Closed { link, code: 1006 });
                    return;
                }
            }
            m = stream.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    let _ = ev.send(Input::Text { link, text: t.to_string() });
                }
                Some(Ok(Message::Binary(b))) => {
                    let _ = ev.send(Input::Binary { link, bytes: b.to_vec() });
                }
                Some(Ok(Message::Close(c))) => {
                    let code = c.map(|c| u16::from(c.code)).unwrap_or(1005);
                    let _ = ev.send(Input::Closed { link, code });
                    return;
                }
                Some(Ok(_)) => {
                    let _ = ev.send(Input::Heard { link });
                }
                Some(Err(_)) | None => {
                    let _ = ev.send(Input::Closed { link, code: 1006 });
                    return;
                }
            }
        }
    }
}
