//! The plugin end to end, without tmux: the real `rlord-web`, started by
//! an in-memory daemon's supervisor (as `rlord web` does), on a free port
//! of 127.0.0.1, its rlord folders in a temp dir. Checks the Host and
//! Origin checks, first-frame auth, the control socket's requests and
//! events, and that the plugin goes with the daemon.

use std::sync::Arc;
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

/// Tests take turns: the XDG folders and `RLORD_WEB_BIN` are the
/// process's.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Rig {
    daemon: Arc<Daemon>,
    port: u16,
    pid: u64,
    token: String,
    /// Dropping it stops the daemon (its runtime, and every connection).
    stop: Option<std::sync::mpsc::Sender<()>>,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn start() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        // Safety: under LOCK, before anything reads them.
        std::env::set_var("XDG_DATA_HOME", p.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", p.join("config"));
        std::env::set_var("XDG_RUNTIME_DIR", p.join("run"));
        std::env::set_var("RLORD_TMUX_SOCKET", "rlord-test-web-http");
        std::env::set_var("RLORD_NO_LOGIN_SHELL", "1");
        std::env::set_var("RLORD_WEB_BIN", env!("CARGO_BIN_EXE_rlord-web"));
        let cfg = paths::config_path();
        std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
        std::fs::write(&cfg, "[web]\nport = 0\n").unwrap();

        let db = Db::open_in_memory().unwrap();
        db.insert_session(&NewSession {
            id: "alpha",
            name: "alpha",
            agent: "claude",
            agent_session: Some("alpha"),
            project_path: "/tmp",
            tmux_session: None,
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
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
        let st = daemon.web().start(false).expect("the plugin starts");
        let port = st["port"].as_u64().unwrap() as u16;
        let pid = st["pid"].as_u64().unwrap();
        // (Hello comes before it subscribes and serves: wait for that.)
        let t = Instant::now();
        while !std::fs::read_to_string(plugin::log_path())
            .unwrap_or_default()
            .contains("listening on")
        {
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
            stop: Some(stop_tx),
            _dir: dir,
        }
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// A websocket to `path` with `origin` (None: no Origin header).
    async fn ws(
        &self,
        path: &str,
        origin: Option<&str>,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        u16,
    > {
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

    /// The control socket, authenticated; its hello.
    async fn control(
        &self,
    ) -> (
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Value,
    ) {
        let mut ws = self.ws("/ws", Some(&self.origin())).await.unwrap();
        ws.send(Message::text(json!({"auth": self.token}).to_string()))
            .await
            .unwrap();
        let hello = next_json(&mut ws).await;
        (ws, hello)
    }

    /// A plain HTTP GET with this Host: status and the whole response.
    async fn get(&self, path: &str, host: &str) -> (u16, String) {
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

/// The next text frame, as JSON (5 s at most).
async fn next_json<S>(ws: &mut S) -> Value
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
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
async fn close_code<S>(ws: &mut S) -> Option<u16>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
            Ok(Some(Ok(Message::Close(f)))) => return f.map(|f| u16::from(f.code)),
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn host_check_and_headers() {
    let _g = LOCK.lock().await;
    let r = tokio::task::spawn_blocking(Rig::start).await.unwrap();
    let host = format!("127.0.0.1:{}", r.port);
    let (status, text) = r.get("/", &host).await;
    assert_eq!(status, 200, "{text}");
    let lower = text.to_lowercase();
    assert!(lower.contains("content-type: text/html"), "{text}");
    for h in [
        "content-security-policy: default-src 'none'; script-src 'self';",
        "x-content-type-options: nosniff",
        "referrer-policy: no-referrer",
        "cross-origin-opener-policy: same-origin",
        "cross-origin-resource-policy: same-origin",
        "cache-control: no-store",
    ] {
        assert!(lower.contains(h), "missing {h}: {text}");
    }
    assert!(lower.contains(&format!("connect-src 'self' ws://127.0.0.1:{}", r.port)));
    assert!(!lower.contains("set-cookie"));
    // DNS rebinding: another name for this address is refused, headers
    // and all.
    for bad in [
        format!("localhost:{}", r.port),
        "evil.example".to_string(),
        format!("127.0.0.1:{}", r.port + 1),
    ] {
        let (status, text) = r.get("/", &bad).await;
        assert_eq!(status, 404, "{bad}: {text}");
        assert!(text.to_lowercase().contains("content-security-policy"));
    }
    assert_eq!(r.get("/no-such-file.js", &host).await.0, 404);
    drop(r);
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrades_need_the_exact_origin_and_the_token() {
    let _g = LOCK.lock().await;
    let r = tokio::task::spawn_blocking(Rig::start).await.unwrap();
    assert_eq!(r.ws("/ws", None).await.err(), Some(403));
    for bad in [
        "http://localhost:".to_string() + &r.port.to_string(),
        "http://evil.example".to_string(),
        "null".to_string(),
    ] {
        assert_eq!(r.ws("/ws", Some(&bad)).await.err(), Some(403), "{bad}");
    }
    // A wrong token, or something else first: closed with 4401.
    for first in [
        json!({"auth": "0".repeat(64)}).to_string(),
        json!({"auth": ""}).to_string(),
        json!({"id": 1, "op": "snapshot"}).to_string(),
        "hello".to_string(),
    ] {
        let mut ws = r.ws("/ws", Some(&r.origin())).await.unwrap();
        ws.send(Message::text(first.clone())).await.unwrap();
        assert_eq!(close_code(&mut ws).await, Some(4401), "{first}");
    }
    // The right one: hello.
    let (_ws, hello) = r.control().await;
    assert_eq!(hello["event"], "hello", "{hello}");
    assert_eq!(hello["version"], env!("CARGO_PKG_VERSION"));
    assert!(hello["build"]
        .as_str()
        .unwrap()
        .starts_with(env!("CARGO_PKG_VERSION")));
    assert_eq!(hello["rev"], r.daemon.rev().as_str());
    drop(r);
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_and_events_on_the_control_socket() {
    let _g = LOCK.lock().await;
    let r = tokio::task::spawn_blocking(Rig::start).await.unwrap();
    let (mut ws, _) = r.control().await;
    // A snapshot.
    ws.send(Message::text(
        json!({"id": 1, "op": "snapshot", "args": {"view": {}}}).to_string(),
    ))
    .await
    .unwrap();
    let a = next_json(&mut ws).await;
    assert_eq!(a["id"], 1, "{a}");
    assert_eq!(a["ok"]["rows"][0]["id"], "alpha", "{a}");
    let rev = a["ok"]["rev"].as_str().unwrap().to_string();
    // A change in the daemon: an event with the new rev.
    let sock = paths::socket_path();
    tokio::task::spawn_blocking(move || {
        rlord_daemon::client_call(
            &sock,
            "session.describe",
            json!({"ref": "alpha", "description": "from a test"}),
        )
        .unwrap()
    })
    .await
    .unwrap();
    let ev = next_json(&mut ws).await;
    assert_eq!(ev["event"], "changed", "{ev}");
    assert_ne!(ev["rev"], rev.as_str());
    assert_eq!(ev["rev"], r.daemon.rev().as_str());
    // Off the list; not JSON; no op.
    ws.send(Message::text(
        json!({"id": "x", "op": "admin.reset", "args": {}}).to_string(),
    ))
    .await
    .unwrap();
    let a = next_json(&mut ws).await;
    assert_eq!(a["id"], "x");
    assert_eq!(a["error"]["code"], "not_allowed", "{a}");
    ws.send(Message::text("{nope")).await.unwrap();
    assert_eq!(next_json(&mut ws).await["error"]["code"], "bad_request");
    ws.send(Message::text(json!({"id": 3}).to_string()))
        .await
        .unwrap();
    let a = next_json(&mut ws).await;
    assert_eq!(
        (a["id"].clone(), a["error"]["code"].clone()),
        (json!(3), json!("bad_request"))
    );
    // A method error comes back as one.
    ws.send(Message::text(
        json!({"id": 4, "op": "session.get", "args": {"ref": "nosuch"}}).to_string(),
    ))
    .await
    .unwrap();
    let a = next_json(&mut ws).await;
    assert_eq!(a["error"]["code"], "failed", "{a}");
    // A frame over the 1 MiB limit ends the socket, unanswered.
    let big = format!(
        "{{\"id\":5,\"op\":\"snapshot\",\"args\":{{\"x\":\"{}\"}}}}",
        "a".repeat(1 << 20)
    );
    let _ = ws.send(Message::text(big)).await;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => panic!("answered a frame over the limit: {t}"),
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => break,
            Ok(Some(Ok(_))) => continue,
            Err(_) => panic!("the socket stayed open"),
        }
    }
    drop(r);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_plugin_goes_with_the_daemon() {
    let _g = LOCK.lock().await;
    let mut r = tokio::task::spawn_blocking(Rig::start).await.unwrap();
    let alive = |pid: u64| unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
    assert!(alive(r.pid));
    drop(r.stop.take());
    let t = Instant::now();
    while alive(r.pid) {
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "rlord-web outlived its daemon"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let log = std::fs::read_to_string(plugin::log_path()).unwrap();
    assert!(log.contains("the daemon went away"), "{log}");
    drop(r);
}
