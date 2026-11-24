//! `rlord-device bridge`: the device engine run natively, with the phone
//! shell's message bridge offered as a loopback websocket. A debug-only
//! stand-in for the app's shell, so the phone page runs in a desktop
//! browser (`cargo xtask app dev`) or in Node (`cargo xtask app test-e2e`)
//! against the code the phone runs. Never part of the app.
//!
//! It plays the shell (`apps/ios/rlord/Shell.swift`):
//!
//! - every websocket text message from the page goes to
//!   [`Engine::page_message`], except the two the shell serves itself:
//!   `{"app":"settings",…}` (answered with a `settings` event, kept in
//!   memory) and `{"app":"scan"}` (there is no camera: it says
//!   `{"event":"scan"}` on stdout, and whoever runs the bridge answers
//!   with a `qr` line on stdin);
//! - every [`Listener::to_page`](rlord_e2e_client::Listener) message goes
//!   back as one text message;
//! - the engine is in the foreground while a page is connected, in the
//!   background otherwise. One page at a time: a new one replaces the
//!   last (as a reload does).
//!
//! The listener binds 127.0.0.1 only. A connection that names an
//! `Origin` is refused unless that origin was given (`--origin`): a page
//! from anywhere else open in the same browser can't reach the engine.
//! Node's WebSocket sends no `Origin`.
//!
//! Stdout: one JSON line per event (`ready` with the URL, `page`, `scan`,
//! `notify`). Stdin: `qr <RLORD1:…>` (as if scanned), `network up|down`,
//! `quit`; the end of stdin quits too.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;

use rlord_e2e_client::soft::{Collect, Out, PlainVault, SoftSigners};
use rlord_e2e_client::{Config, Engine};

/// How the bridge is set up.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The engine's folder (its records; software keys in `dir/keys`).
    pub dir: PathBuf,
    /// The loopback port (0: any free one).
    pub port: u16,
    /// An extra root CA (PEM text) for the relay's certificate.
    pub ca_pem: Option<String>,
    /// A pairing QR to scan at start.
    pub qr: Option<String>,
    /// The name offered for this device when a QR is scanned.
    pub name: String,
    /// Page origins allowed to connect (a connection without `Origin` is
    /// always allowed).
    pub origins: Vec<String>,
}

pub const USAGE: &str = "rlord-device bridge --state DIR [--port N] [--ca FILE] [--qr RLORD1:…] \
[--name NAME] [--origin ORIGIN]…";

impl Options {
    /// From `rlord-device bridge`'s arguments (after `bridge`).
    pub fn parse(args: &[String]) -> Result<Options> {
        let mut o = Options {
            name: "rlord-device bridge".into(),
            ..Options::default()
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let mut value = || it.next().cloned().with_context(|| format!("{a}: {USAGE}"));
            match a.as_str() {
                "--state" => o.dir = value()?.into(),
                "--port" => o.port = value()?.parse().context("--port")?,
                "--ca" => {
                    let f = value()?;
                    o.ca_pem =
                        Some(std::fs::read_to_string(&f).with_context(|| format!("reading {f}"))?);
                }
                "--qr" => o.qr = Some(value()?),
                "--name" => o.name = value()?,
                "--origin" => o.origins.push(value()?),
                other => bail!("unknown argument {other}\nusage: {USAGE}"),
            }
        }
        if o.dir.as_os_str().is_empty() {
            bail!("usage: {USAGE}");
        }
        Ok(o)
    }
}

/// What the shell answers `settings` with (kept in memory).
#[derive(Clone, Copy, Debug, Default)]
struct Settings {
    app_lock: bool,
    notifications: bool,
}

impl Settings {
    fn event(&self) -> String {
        json!({
            "event": "settings",
            "app_lock": self.app_lock,
            "notifications": self.notifications,
            "platform": "bridge",
        })
        .to_string()
    }
}

/// A page message the shell serves itself (`scan`, `settings`); None for
/// the engine's.
#[derive(Debug, PartialEq, Eq)]
enum ShellMessage {
    Scan,
    Settings {
        app_lock: Option<bool>,
        notifications: Option<bool>,
    },
}

fn shell_message(text: &str) -> Option<ShellMessage> {
    let v: Value = serde_json::from_str(text).ok()?;
    match v.get("app")?.as_str()? {
        "scan" => Some(ShellMessage::Scan),
        "settings" => Some(ShellMessage::Settings {
            app_lock: v["app_lock"].as_bool(),
            notifications: v["notifications"].as_bool(),
        }),
        _ => None,
    }
}

/// Whether a connection with this `Origin` header (None: absent) may
/// connect.
fn origin_allowed(origin: Option<&str>, allowed: &[String]) -> bool {
    match origin {
        None => true,
        Some(o) => allowed.iter().any(|a| a.trim_end_matches('/') == o),
    }
}

/// A control line from stdin.
#[derive(Debug, PartialEq, Eq)]
pub enum Control {
    Qr(String),
    Network(bool),
    Quit,
}

pub fn control(line: &str) -> Result<Option<Control>> {
    let line = line.trim();
    let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
    Ok(Some(match (word, rest.trim()) {
        ("", _) => return Ok(None),
        ("qr", qr) if !qr.is_empty() => Control::Qr(qr.to_string()),
        ("network", "up") => Control::Network(true),
        ("network", "down") => Control::Network(false),
        ("quit", _) => Control::Quit,
        _ => bail!("unknown command {line:?} (qr RLORD1:…, network up|down, quit)"),
    }))
}

/// The page connected now: its outgoing queue and its number.
struct Page {
    tx: mpsc::UnboundedSender<String>,
    n: u64,
}

/// A running bridge.
pub struct Bridge {
    pub addr: SocketAddr,
    pub engine: Arc<Engine>,
    page: Arc<Mutex<Option<Page>>>,
    name: String,
    /// Events for whoever runs it (stdout in the CLI).
    events: mpsc::UnboundedReceiver<Value>,
}

impl Bridge {
    /// Starts the engine and the listener (in the current tokio runtime).
    pub async fn start(o: Options) -> Result<Bridge> {
        let signers = Arc::new(SoftSigners::in_dir(o.dir.join("keys")));
        let (listener, mut from_engine) = Collect::new();
        let engine = Arc::new(
            Engine::new(
                Config {
                    dir: o.dir.clone(),
                    ca_pem: o.ca_pem.clone(),
                },
                Arc::new(PlainVault),
                signers,
                Arc::new(listener),
            )
            .map_err(|e| anyhow!("starting the engine: {e}"))?,
        );
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, o.port))
            .await
            .with_context(|| format!("listening on 127.0.0.1:{}", o.port))?;
        let addr = tcp.local_addr()?;
        let page: Arc<Mutex<Option<Page>>> = Arc::default();
        let (ev_tx, events) = mpsc::unbounded_channel();

        // The engine's messages to the page that is connected now (none
        // connected: dropped; the next page's `ready` replays its state).
        let to_page = page.clone();
        let ev = ev_tx.clone();
        tokio::spawn(async move {
            while let Some(out) = from_engine.recv().await {
                match out {
                    Out::Page(s) => {
                        if let Some(p) = to_page.lock().unwrap().as_ref() {
                            let _ = p.tx.send(s);
                        }
                    }
                    Out::Notify(kind, count) => {
                        let _ = ev.send(json!({"event": "notify", "kind": kind, "count": count}));
                    }
                }
            }
        });

        let shared = Shared {
            engine: engine.clone(),
            page: page.clone(),
            settings: Arc::default(),
            origins: Arc::new(o.origins.clone()),
            events: ev_tx,
            next: Arc::default(),
        };
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = tcp.accept().await else {
                    continue;
                };
                let s = shared.clone();
                tokio::spawn(async move { s.serve(stream).await });
            }
        });
        if let Some(qr) = o.qr {
            engine.scanned(qr, o.name.clone());
        }
        Ok(Bridge {
            addr,
            engine,
            page,
            name: o.name,
            events,
        })
    }

    /// `ws://127.0.0.1:<port>/`.
    pub fn url(&self) -> String {
        format!("ws://{}/", self.addr)
    }

    /// The next event for whoever runs the bridge.
    pub async fn event(&mut self) -> Option<Value> {
        self.events.recv().await
    }

    /// Acts on a control line's command; false to quit.
    pub fn control(&self, c: Control) -> bool {
        match c {
            Control::Qr(qr) => self.engine.scanned(qr, self.name.clone()),
            Control::Network(up) => self.engine.network(up),
            Control::Quit => return false,
        }
        true
    }

    /// Whether a page is connected.
    pub fn connected(&self) -> bool {
        self.page.lock().unwrap().is_some()
    }

    pub fn shutdown(&self) {
        self.page.lock().unwrap().take();
        self.engine.shutdown();
    }
}

#[derive(Clone)]
struct Shared {
    engine: Arc<Engine>,
    page: Arc<Mutex<Option<Page>>>,
    settings: Arc<Mutex<Settings>>,
    origins: Arc<Vec<String>>,
    events: mpsc::UnboundedSender<Value>,
    next: Arc<Mutex<u64>>,
}

impl Shared {
    async fn serve(self, stream: tokio::net::TcpStream) {
        let origins = self.origins.clone();
        // (The callback's shape is tungstenite's.)
        #[allow(clippy::result_large_err)]
        let check = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
            let origin = req
                .headers()
                .get("origin")
                .map(|v| v.to_str().unwrap_or("?"));
            if origin_allowed(origin, &origins) {
                Ok(resp)
            } else {
                let mut r = ErrorResponse::new(Some("origin not allowed".into()));
                *r.status_mut() = StatusCode::FORBIDDEN;
                Err(r)
            }
        };
        let ws = match tokio_tungstenite::accept_hdr_async(stream, check).await {
            Ok(ws) => ws,
            Err(e) => {
                eprintln!("rlord-device bridge: refused a connection: {e}");
                return;
            }
        };
        let (mut sink, mut source) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let n = {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            *next
        };
        // A new page replaces the last: dropping its queue closes it.
        self.page.lock().unwrap().replace(Page { tx, n });
        self.engine.lifecycle(true);
        let _ = self
            .events
            .send(json!({"event": "page", "connected": true, "n": n}));
        let writer = tokio::spawn(async move {
            while let Some(s) = rx.recv().await {
                if sink.send(Message::text(s)).await.is_err() {
                    return;
                }
            }
            let _ = sink.send(Message::Close(None)).await;
        });
        while let Some(Ok(m)) = source.next().await {
            let text = match m {
                Message::Text(t) => t.to_string(),
                Message::Close(_) => break,
                _ => continue,
            };
            // Replaced by a newer page: its messages no longer count.
            if !self.current(n) {
                break;
            }
            self.page_said(&text);
        }
        let mut page = self.page.lock().unwrap();
        if page.as_ref().is_some_and(|p| p.n == n) {
            page.take();
            drop(page);
            self.engine.lifecycle(false);
            let _ = self
                .events
                .send(json!({"event": "page", "connected": false, "n": n}));
        }
        writer.abort();
    }

    fn current(&self, n: u64) -> bool {
        self.page.lock().unwrap().as_ref().is_some_and(|p| p.n == n)
    }

    fn reply(&self, text: String) {
        if let Some(p) = self.page.lock().unwrap().as_ref() {
            let _ = p.tx.send(text);
        }
    }

    fn page_said(&self, text: &str) {
        match shell_message(text) {
            Some(ShellMessage::Scan) => {
                eprintln!(
                    "rlord-device bridge: the page asked to scan; give a pairing code with \
                     `qr RLORD1:…` on stdin"
                );
                let _ = self.events.send(json!({"event": "scan"}));
            }
            Some(ShellMessage::Settings {
                app_lock,
                notifications,
            }) => {
                let s = {
                    let mut s = self.settings.lock().unwrap();
                    if let Some(v) = app_lock {
                        s.app_lock = v;
                    }
                    if let Some(v) = notifications {
                        s.notifications = v;
                    }
                    *s
                };
                self.reply(s.event());
            }
            None => self.engine.page_message(text.to_string()),
        }
    }
}

/// `rlord-device bridge …`: runs until `quit` or the end of stdin.
pub async fn run(args: &[String]) -> Result<()> {
    let o = Options::parse(args)?;
    let mut bridge = Bridge::start(o).await?;
    say(&json!({"event": "ready", "url": bridge.url(), "port": bridge.addr.port()}));
    // Stdin on a thread of its own (its end is a `quit`).
    let (tx, mut lines) = mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut events = std::mem::replace(&mut bridge.events, mpsc::unbounded_channel().1);
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break };
                match control(&line) {
                    Ok(Some(c)) => {
                        if !bridge.control(c) {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => say(&json!({"event": "error", "message": e.to_string()})),
                }
            }
            Some(ev) = events.recv() => say(&ev),
        }
    }
    bridge.shutdown();
    Ok(())
}

fn say(v: &Value) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_messages() {
        assert_eq!(shell_message(r#"{"app":"scan"}"#), Some(ShellMessage::Scan));
        assert_eq!(
            shell_message(r#"{"app":"settings","app_lock":true}"#),
            Some(ShellMessage::Settings {
                app_lock: Some(true),
                notifications: None
            })
        );
        for engine in [
            r#"{"app":"ready"}"#,
            r#"{"app":"status"}"#,
            r#"{"id":1,"op":"snapshot","args":{}}"#,
            "not json",
        ] {
            assert_eq!(shell_message(engine), None, "{engine}");
        }
    }

    #[test]
    fn origins() {
        let allowed = vec!["http://127.0.0.1:5173/".to_string()];
        assert!(origin_allowed(None, &allowed));
        assert!(origin_allowed(Some("http://127.0.0.1:5173"), &allowed));
        assert!(!origin_allowed(Some("http://127.0.0.1:5174"), &allowed));
        assert!(!origin_allowed(Some("https://example.com"), &allowed));
        assert!(!origin_allowed(Some("null"), &[]));
    }

    #[test]
    fn controls() {
        assert_eq!(
            control("qr RLORD1:abc").unwrap(),
            Some(Control::Qr("RLORD1:abc".into()))
        );
        assert_eq!(
            control("network down").unwrap(),
            Some(Control::Network(false))
        );
        assert_eq!(control(" quit ").unwrap(), Some(Control::Quit));
        assert_eq!(control("").unwrap(), None);
        assert!(control("qr").is_err());
        assert!(control("network sideways").is_err());
    }

    #[test]
    fn options() {
        let a = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        let o = Options::parse(&a(
            "--state /tmp/x --port 7 --qr RLORD1:q --origin http://a",
        ))
        .unwrap();
        assert_eq!(o.dir, PathBuf::from("/tmp/x"));
        assert_eq!(o.port, 7);
        assert_eq!(o.qr.as_deref(), Some("RLORD1:q"));
        assert_eq!(o.origins, ["http://a"]);
        assert!(Options::parse(&a("--port 7")).is_err());
        assert!(Options::parse(&a("--state x --bogus")).is_err());
    }
}
