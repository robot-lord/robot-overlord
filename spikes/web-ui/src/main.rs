//! Spike: serve the rlord overlord's tmux pane to a browser.
//!
//! A localhost HTTP server hands out one page (xterm.js) and one websocket.
//! Each websocket gets its own PTY running `tmux -L <socket> attach -t
//! =<overlord>`; bytes go both ways untouched (binary frames), resizes come
//! as text frames (`{"resize":[cols,rows]}`). The overlord is looked up from
//! the daemon (`overlord.get`) on every connect, so a reconnect follows a
//! restarted or switched overlord.
//!
//! Guardrails: binds 127.0.0.1 only; every path carries a random token;
//! the websocket needs a matching Origin and every request a matching Host
//! (DNS rebinding); refuses rlord's real tmux socket.
//!
//!   source DEMO/env.sh && cargo run -- [--port N] [--target SESSION]

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const PAGE: &str = include_str!("page.html");

struct App {
    token: String,
    port: u16,
    /// rlord's tmux socket name (`tmux -L`).
    socket: String,
    /// A fixed tmux session to serve instead of the overlord (testing).
    target: Option<String>,
    daemon_sock: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut port = 0u16;
    let mut target = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--port" => port = args.next().context("--port N")?.parse()?,
            "--target" => target = args.next(),
            _ => bail!("unknown argument {a}"),
        }
    }
    let socket = std::env::var("RLORD_TMUX_SOCKET").unwrap_or_default();
    if socket.is_empty() || socket == "rlord" {
        bail!("spike: scratch tmux servers only; set RLORD_TMUX_SOCKET (source the demo's env.sh)");
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR unset (source the demo's env.sh)")?;
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let port = listener.local_addr()?.port();
    let app = Arc::new(App {
        token: token(&runtime.join("robot-overlord/web-spike.token"))?,
        port,
        socket,
        target,
        daemon_sock: runtime.join("robot-overlord/daemon.sock"),
    });
    // Fail early (and show what it found) rather than on the first connect.
    match resolve(&app) {
        Ok(t) => eprintln!("serving tmux session {t:?} on socket {:?}", app.socket),
        Err(e) => eprintln!("warning: no overlord yet: {e:#}"),
    }
    println!("http://127.0.0.1:{port}/t/{}/", app.token);
    let router = Router::new()
        .route("/t/{token}/", get(page))
        .route("/t/{token}/ws", get(ws))
        .with_state(app);
    axum::serve(listener, router).await?;
    Ok(())
}

/// 128 random bits, hex. Kept in a 0600 file next to the daemon socket, so
/// an open page reconnects after this server restarts.
fn token(path: &std::path::Path) -> Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(t) = std::fs::read_to_string(path) {
        if t.trim().len() == 32 {
            return Ok(t.trim().to_string());
        }
    }
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?.write_all(t.as_bytes())?;
    Ok(t)
}

/// Constant-time token check, plus the Host header (a page on another
/// site that rebinds its name to 127.0.0.1 sends its own Host).
fn allowed(app: &App, token: &str, headers: &HeaderMap) -> bool {
    let a = app.token.as_bytes();
    let b = token.as_bytes();
    let same = a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0;
    let host = headers.get("host").and_then(|h| h.to_str().ok());
    same && host == Some(&format!("127.0.0.1:{}", app.port))
}

async fn page(State(app): State<Arc<App>>, Path(token): Path<String>, headers: HeaderMap) -> Response {
    if !allowed(&app, &token, &headers) {
        return StatusCode::NOT_FOUND.into_response();
    }
    (
        [
            ("content-security-policy", "default-src 'none'; script-src 'unsafe-inline' https://cdn.jsdelivr.net; style-src 'unsafe-inline' https://cdn.jsdelivr.net; connect-src 'self'; font-src https://cdn.jsdelivr.net"),
            ("referrer-policy", "no-referrer"),
            ("cache-control", "no-store"),
        ],
        Html(PAGE),
    )
        .into_response()
}

async fn ws(
    State(app): State<Arc<App>>,
    Path(token): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    up: WebSocketUpgrade,
) -> Response {
    let origin = headers.get("origin").and_then(|h| h.to_str().ok());
    if !allowed(&app, &token, &headers) || origin != Some(&format!("http://127.0.0.1:{}", app.port)) {
        eprintln!("refused websocket: origin {origin:?}");
        return StatusCode::FORBIDDEN.into_response();
    }
    let dim = |k: &str, d: u16| q.get(k).and_then(|v| v.parse().ok()).filter(|n| *n > 1 && *n < 1000).unwrap_or(d);
    let (cols, rows) = (dim("cols", 100), dim("rows", 30));
    up.on_upgrade(move |sock| async move {
        if let Err(e) = bridge(sock, app, cols, rows).await {
            eprintln!("bridge: {e:#}");
        }
    })
}

/// The tmux session to serve: `--target`, else the active overlord's, as
/// the daemon reports it. Checked to exist on rlord's server.
fn resolve(app: &App) -> Result<String> {
    let name = match &app.target {
        Some(t) => t.clone(),
        None => {
            let v = rpc(&app.daemon_sock, "overlord.get")?;
            let v = v.get("result").unwrap_or(&v);
            v.get("tmux_session")
                .and_then(|s| s.as_str())
                .with_context(|| format!("overlord.get has no tmux_session: {v}"))?
                .to_string()
        }
    };
    let ok = std::process::Command::new("tmux")
        .args(["-L", &app.socket, "has-session", "-t", &format!("={name}")])
        .status()?
        .success();
    if !ok {
        bail!("tmux session {name:?} is not running");
    }
    Ok(name)
}

/// One line-delimited JSON call to the daemon.
fn rpc(sock: &std::path::Path, method: &str) -> Result<serde_json::Value> {
    use std::io::BufRead;
    let mut s = std::os::unix::net::UnixStream::connect(sock)
        .with_context(|| format!("daemon socket {}", sock.display()))?;
    writeln!(s, "{}", serde_json::json!({"id": 1, "method": method, "params": {}}))?;
    let mut line = String::new();
    std::io::BufReader::new(s).read_line(&mut line)?;
    let v: serde_json::Value = serde_json::from_str(&line)?;
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        bail!("{method}: {e}");
    }
    Ok(v)
}

async fn bridge(ws: WebSocket, app: Arc<App>, cols: u16, rows: u16) -> Result<()> {
    let (mut tx, mut rx) = ws.split();
    let target = match resolve(&app) {
        Ok(t) => t,
        Err(e) => {
            // The page shows this and retries.
            let _ = tx.send(Message::Text(serde_json::json!({"error": format!("{e:#}")}).to_string().into())).await;
            return Ok(());
        }
    };
    let _ = tx.send(Message::Text(serde_json::json!({"target": target}).to_string().into())).await;
    let pty = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
    let mut cmd = CommandBuilder::new("tmux");
    cmd.args(["-L", &app.socket, "attach-session", "-t", &format!("={target}")]);
    cmd.env_remove("TMUX");
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let mut child = pty.slave.spawn_command(cmd)?;
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader()?;
    let mut writer = pty.master.take_writer()?;
    let (otx, mut orx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buf = [0u8; 16384];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if otx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut ended = "client closed";
    loop {
        tokio::select! {
            out = orx.recv() => match out {
                Some(b) => if tx.send(Message::Binary(b.into())).await.is_err() { break },
                None => { ended = "tmux client exited"; break }
            },
            inp = rx.next() => match inp {
                Some(Ok(Message::Binary(b))) => { writer.write_all(&b)?; }
                Some(Ok(Message::Text(t))) => {
                    let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
                    if let Some([c, r]) = v.get("resize").and_then(|r| r.as_array()).map(|a| [a.first(), a.get(1)]) {
                        let n = |x: Option<&serde_json::Value>| x.and_then(|x| x.as_u64()).filter(|n| *n > 1 && *n < 1000).map(|n| n as u16);
                        if let (Some(cols), Some(rows)) = (n(c), n(r)) {
                            pty.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }
    }
    eprintln!("session {target:?}: {ended}");
    let _ = child.kill();
    let _ = child.wait();
    let _ = tx.send(Message::Close(None)).await;
    Ok(())
}
