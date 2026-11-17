//! The rig the plugin's integration tests share: an in-memory daemon on
//! a socket of its own, whose supervisor starts the real `rlord-web` (as
//! `rlord web` does) on a free port of 127.0.0.1, and websocket and HTTP
//! helpers. rlord's data and config folders are one scratch folder per
//! test binary (tmux's generated config is written once per process).

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use rlord_core::config::Config;
use rlord_core::db::{Db, NewSession};
use rlord_core::{paths, plugin};
use rlord_daemon::Daemon;

pub type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Tests take turns: the environment is the process's.
pub static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Once per test binary: rlord's folders under `name` in the target's tmp
/// folder, `[web] port = 0`, tmux on scratch socket `tmux_socket`, the
/// real plugin as `RLORD_WEB_BIN`. Returns the folder.
pub fn env_once(name: &str, tmux_socket: &str) -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Safety: set once, under LOCK, before anything reads them.
        std::env::set_var("XDG_DATA_HOME", dir.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", dir.join("config"));
        std::env::set_var("RLORD_TMUX_SOCKET", tmux_socket);
        std::env::set_var("RLORD_NO_LOGIN_SHELL", "1");
        std::env::set_var("RLORD_WEB_BIN", env!("CARGO_BIN_EXE_rlord-web"));
        let cfg = paths::config_path();
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(&cfg, "[web]\nport = 0\n").unwrap();
        dir
    })
    .clone()
}

/// A session row for a test.
pub fn add(db: &Db, id: &str, tmux: Option<&str>) {
    db.insert_session(&NewSession {
        id,
        name: id,
        agent: "claude",
        agent_session: Some(id),
        project_path: "/tmp",
        tmux_session: tmux,
        launch_spec: "{}",
        created_at: 1,
        repo_path: None,
        workspace_path: None,
        branch: None,
        role: "",
    })
    .unwrap();
}

pub struct Rig {
    pub daemon: Arc<Daemon>,
    pub port: u16,
    pub pid: u64,
    pub token: String,
    /// Where `web.log` stood before this plugin started.
    pub log_from: usize,
    /// Dropping it stops the daemon (its runtime, and every connection).
    pub stop: Option<std::sync::mpsc::Sender<()>>,
    _run: tempfile::TempDir,
}

impl Rig {
    /// A daemon over the rows `setup` adds, and its plugin, serving.
    /// Blocking: call through `spawn_blocking`.
    pub fn start(setup: impl FnOnce(&Db)) -> Rig {
        // The daemon's socket: a folder of its own each time.
        let run = tempfile::tempdir().unwrap();
        // Safety: under LOCK.
        std::env::set_var("XDG_RUNTIME_DIR", run.path());
        let db = Db::open_in_memory().unwrap();
        setup(&db);
        let daemon = Daemon::new(db, Config::load().unwrap());
        daemon.web().set_expected_hash(None);
        // (A plugin that exits in a test isn't restarted under it.)
        daemon.web().set_backoff_base(Duration::from_secs(30));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let d2 = daemon.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let listener = Daemon::bind(&paths::socket_path()).unwrap();
                ready_tx.send(()).unwrap();
                let stop = tokio::task::spawn_blocking(move || {
                    let _ = stop_rx.recv();
                });
                tokio::select! {
                    _ = d2.serve(listener) => {}
                    _ = stop => {}
                }
            });
            // Dropping the runtime ends every connection.
            rt.shutdown_background();
        });
        ready_rx.recv().unwrap();
        let log_from = log().len();
        let st = daemon.web().start(false).expect("the plugin starts");
        let port = st["port"].as_u64().unwrap() as u16;
        let pid = st["pid"].as_u64().unwrap();
        // (Hello comes before it subscribes and serves: wait for that.)
        let t = Instant::now();
        while !log()[log_from..].contains("listening on") {
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "rlord-web never served"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let token = std::fs::read_to_string(plugin::token_path()).unwrap();
        Rig {
            daemon,
            port,
            pid,
            token: token.trim().to_string(),
            log_from,
            stop: Some(stop_tx),
            _run: run,
        }
    }

    /// What this plugin wrote to `web.log`.
    pub fn log(&self) -> String {
        log().get(self.log_from..).unwrap_or("").to_string()
    }

    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// A websocket to `path` with `origin` (None: no Origin header); the
    /// HTTP status when the upgrade is refused.
    pub async fn ws(&self, path: &str, origin: Option<&str>) -> Result<Ws, u16> {
        let mut req = format!("ws://127.0.0.1:{}{path}", self.port)
            .into_client_request()
            .unwrap();
        if let Some(o) = origin {
            req.headers_mut().insert("origin", o.parse().unwrap());
        }
        match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => Ok(ws),
            Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
            Err(e) => panic!("{e}"),
        }
    }

    /// A websocket to `path`, authenticated.
    pub async fn authed(&self, path: &str) -> Ws {
        let mut ws = self.ws(path, Some(&self.origin())).await.unwrap();
        ws.send(Message::text(json!({"auth": self.token}).to_string()))
            .await
            .unwrap();
        ws
    }

    /// The control socket, authenticated; its hello.
    pub async fn control(&self) -> (Ws, Value) {
        let mut ws = self.authed("/ws").await;
        let hello = next_json(&mut ws).await;
        (ws, hello)
    }

    /// A plain HTTP GET with this Host: status and the whole response.
    pub async fn get(&self, path: &str, host: &str) -> (u16, String) {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap();
        (status, text)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.daemon.web().stop();
        drop(self.stop.take());
    }
}

fn log() -> String {
    std::fs::read_to_string(plugin::log_path()).unwrap_or_default()
}

/// The next text frame, as JSON (5 s at most).
pub async fn next_json(ws: &mut Ws) -> Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("a frame in time")
            .expect("open")
            .expect("no error");
        if let Message::Text(t) = m {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

/// The close code the server ends with.
pub async fn close_code(ws: &mut Ws) -> Option<u16> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            Ok(Some(Ok(Message::Close(f)))) => return f.map(|f| u16::from(f.code)),
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

pub fn alive(pid: u64) -> bool {
    // Safety: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}
