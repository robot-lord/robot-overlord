//! The HTTP side: static files, the control websocket (the pane socket is
//! `pane.rs`), and the checks and
//! headers every request goes through (docs/working/web-ui-remote.md,
//! "Auth, headers, assets").
//!
//! - Bound to 127.0.0.1 only (`main.rs`).
//! - Every request: Host must be exactly `127.0.0.1:<port>`, else 404
//!   (DNS rebinding). Every response carries the security headers.
//! - Every websocket upgrade: Origin must be exactly
//!   `http://127.0.0.1:<port>`, else 403. Its first frame must be
//!   `{"auth":"<token>"}` within 5 s, else it closes with 4401.
//! - No cookies, no REST: everything that reads or changes state goes
//!   over a websocket.
//!
//! The control socket: requests `{"id","op","args"}` are answered with
//! `{"id","ok"}` or `{"id","error":{"code","message"}}`; events are
//! `{"event":"hello",…,"page"}` (first), `{"event":"changed","rev"}` and
//! `{"event":"ui","cmd"}` (a UI command from the overlord, for this page
//! only).
//!
//! Each control socket is a page with an id this plugin gives it
//! (`p<n>-<8 hex>`): told to the page in `hello`, stamped on every
//! `web.call` it makes (the browser can't choose it), and reported to the
//! daemon with `web.pages` whenever a page opens or closes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

use crate::daemon::Daemon;
use crate::{assets, token};

/// At most this many control sockets at once.
pub const MAX_CONTROL: usize = 16;
/// The largest control frame.
pub const MAX_CONTROL_FRAME: usize = 1 << 20;
/// How long a websocket has to send its token.
pub const AUTH_WAIT: Duration = Duration::from_secs(5);
/// The close code for a missing or wrong token.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;

/// What every handler shares.
pub struct App {
    pub token: String,
    pub port: u16,
    pub daemon: Daemon,
    /// The daemon's latest rev; `changed` carries it.
    pub rev: Mutex<String>,
    /// A tick per daemon change (the rev is in `rev`).
    pub changes: broadcast::Sender<()>,
    pub controls: AtomicUsize,
    pub panes: AtomicUsize,
    /// The open pages: id, and where its socket's messages go.
    pages: Mutex<BTreeMap<String, mpsc::Sender<Message>>>,
    page_seq: AtomicU64,
    /// Held around each `web.pages`, so the daemon gets the lists in order.
    pages_sent: tokio::sync::Mutex<()>,
}

impl App {
    pub fn new(token: String, port: u16, daemon: Daemon, rev: String) -> Arc<App> {
        Arc::new(App {
            token,
            port,
            daemon,
            rev: Mutex::new(rev),
            changes: broadcast::channel(16).0,
            controls: AtomicUsize::new(0),
            panes: AtomicUsize::new(0),
            pages: Mutex::new(BTreeMap::new()),
            page_seq: AtomicU64::new(0),
            pages_sent: tokio::sync::Mutex::new(()),
        })
    }

    /// A new page's id: `p<n>-<8 hex>`.
    fn page_id(&self) -> String {
        let n = self.page_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let r = rlord_core::plugin::random_hex(4).unwrap_or_else(|_| format!("{:08x}", n));
        format!("p{n}-{r}")
    }

    fn page_ids(&self) -> Vec<String> {
        self.pages
            .lock()
            .expect("pages mutex")
            .keys()
            .cloned()
            .collect()
    }

    /// Tell the daemon which pages are open (the whole list).
    pub async fn report_pages(&self) {
        let _order = self.pages_sent.lock().await;
        if let Err(e) = self.daemon.pages(self.page_ids()).await {
            eprintln!("rlord-web: web.pages: {e}");
        }
    }

    /// A UI command for page `page`: to its socket only (dropped when that
    /// page has gone; the daemon keeps it for the next page to ask).
    pub async fn ui(&self, page: &str, cmd: Value) {
        let to = self.pages.lock().expect("pages mutex").get(page).cloned();
        if let Some(to) = to {
            let ev = json!({"event": "ui", "cmd": cmd});
            let _ = to.send(Message::Text(ev.to_string().into())).await;
        }
    }

    pub fn rev(&self) -> String {
        self.rev.lock().expect("rev mutex").clone()
    }

    /// The daemon said something changed.
    pub fn changed(&self, rev: String) {
        *self.rev.lock().expect("rev mutex") = rev;
        let _ = self.changes.send(());
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The Content-Security-Policy. `style-src 'unsafe-inline'` only
    /// because xterm.js makes `<style>` elements at run time (6.0 still
    /// does, with either renderer, and takes no nonce); scripts stay
    /// strict.
    pub fn csp(&self) -> String {
        format!(
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; font-src 'self'; connect-src 'self' ws://127.0.0.1:{}; \
             base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            self.port
        )
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/ws", get(control_upgrade))
        .route("/pane", get(crate::pane::upgrade))
        .fallback(static_file)
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// Every request: the Host check first, the headers on whatever answers.
async fn guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let host_ok = req
        .headers()
        .get(header::HOST)
        .is_some_and(|h| h.as_bytes() == app.host().as_bytes());
    let mut resp = if host_ok {
        next.run(req).await
    } else {
        StatusCode::NOT_FOUND.into_response()
    };
    let h = resp.headers_mut();
    if let Ok(csp) = HeaderValue::from_str(&app.csp()) {
        h.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    for (k, v) in [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::CACHE_CONTROL, "no-store"),
    ] {
        h.insert(k, HeaderValue::from_static(v));
    }
    for (k, v) in [
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
    ] {
        h.insert(k, HeaderValue::from_static(v));
    }
    resp
}

async fn static_file(method: Method, req: Request) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    match assets::find(req.uri().path()) {
        Some((ct, bytes)) => {
            ([(header::CONTENT_TYPE, ct)], Body::from(bytes.into_owned())).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Whether an upgrade's Origin is exactly this server's.
pub fn origin_ok(app: &App, headers: &HeaderMap) -> bool {
    headers
        .get(header::ORIGIN)
        .is_some_and(|o| o.as_bytes() == app.origin().as_bytes())
}

/// One place in a bounded set of sockets, given back when dropped.
pub struct Slot<'a>(&'a AtomicUsize);

impl<'a> Slot<'a> {
    pub fn take(count: &'a AtomicUsize, max: usize) -> Option<Slot<'a>> {
        if count.fetch_add(1, Ordering::SeqCst) >= max {
            count.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Slot(count))
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Wait for the first frame and check its token. False: the socket was
/// closed with 4401.
pub async fn authenticate(app: &App, ws: &mut WebSocket) -> bool {
    let first = tokio::time::timeout(AUTH_WAIT, ws.recv()).await;
    let ok = match first {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Value>(&t)
            .ok()
            .and_then(|v| v["auth"].as_str().map(|g| token::same(&app.token, g)))
            .unwrap_or(false),
        _ => false,
    };
    if !ok {
        let _ = ws
            .send(Message::Close(Some(CloseFrame {
                code: CLOSE_UNAUTHORIZED,
                reason: "unauthorized".into(),
            })))
            .await;
    }
    ok
}

async fn control_upgrade(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    up: WebSocketUpgrade,
) -> Response {
    if !origin_ok(&app, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if app.controls.load(Ordering::SeqCst) >= MAX_CONTROL {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    up.max_message_size(MAX_CONTROL_FRAME)
        .max_frame_size(MAX_CONTROL_FRAME)
        .on_upgrade(move |ws| async move {
            let Some(_slot) = Slot::take(&app.controls, MAX_CONTROL) else {
                return;
            };
            control(&app, ws).await;
        })
}

/// A browser error for the control socket.
fn error(id: &Value, code: &str, message: &str) -> Message {
    Message::Text(
        json!({"id": id, "error": {"code": code, "message": message}})
            .to_string()
            .into(),
    )
}

/// One control socket, after the upgrade.
async fn control(app: &Arc<App>, mut ws: WebSocket) {
    if !authenticate(app, &mut ws).await {
        return;
    }
    let page = app.page_id();
    let (out_tx, out_rx) = mpsc::channel::<Message>(64);
    app.pages
        .lock()
        .expect("pages mutex")
        .insert(page.clone(), out_tx.clone());
    // (Before hello: the page's first requests name a page the daemon
    // knows.)
    app.report_pages().await;
    serve_page(app, ws, &page, out_tx, out_rx).await;
    app.pages.lock().expect("pages mutex").remove(&page);
    app.report_pages().await;
}

/// A control socket's life as page `page`, from its hello on.
async fn serve_page(
    app: &Arc<App>,
    mut ws: WebSocket,
    page: &str,
    out_tx: mpsc::Sender<Message>,
    mut out_rx: mpsc::Receiver<Message>,
) {
    let hello = json!({
        "event": "hello",
        "version": env!("CARGO_PKG_VERSION"),
        "build": assets::build_id(),
        "rev": app.rev(),
        "page": page,
    });
    if ws
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    let mut changes = app.changes.subscribe();
    let (mut tx, mut rx) = ws.split();
    loop {
        tokio::select! {
            m = rx.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    let req: Value = match serde_json::from_str(&t) {
                        Ok(v) => v,
                        Err(_) => {
                            let _ = out_tx.send(error(&Value::Null, "bad_request", "not JSON")).await;
                            continue;
                        }
                    };
                    let id = req["id"].clone();
                    let Some(op) = req["op"].as_str().map(str::to_string) else {
                        let _ = out_tx.send(error(&id, "bad_request", "no op")).await;
                        continue;
                    };
                    let args = req.get("args").cloned().unwrap_or(json!({}));
                    // Answered in any order: each request is its own task.
                    let (app, out, page) = (Arc::clone(app), out_tx.clone(), page.to_string());
                    tokio::spawn(async move {
                        // (`page` is this socket's, never the browser's.)
                        let r = app
                            .daemon
                            .call("web.call", json!({"op": op, "args": args, "page": page}))
                            .await;
                        let msg = match r {
                            Ok(v) => Message::Text(json!({"id": id, "ok": v}).to_string().into()),
                            Err(e) => error(&id, browser_code(e.code), &e.message),
                        };
                        let _ = out.send(msg).await;
                    });
                }
                Some(Ok(Message::Binary(_))) => {
                    let _ = out_tx.send(error(&Value::Null, "bad_request", "text frames only")).await;
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            c = changes.recv() => match c {
                Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    let ev = json!({"event": "changed", "rev": app.rev()});
                    if tx.send(Message::Text(ev.to_string().into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            Some(m) = out_rx.recv() => {
                if tx.send(m).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// The browser's error code for a daemon RPC code.
fn browser_code(code: i64) -> &'static str {
    match code {
        0 => "daemon_unavailable",
        c if c == i64::from(rlord_core::plugin::RPC_ERR_NOT_ALLOWED) => "not_allowed",
        _ => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> Arc<App> {
        App::new(
            "t".repeat(64),
            47811,
            Daemon::new("/nonexistent".into()),
            "r-0".into(),
        )
    }

    #[test]
    fn origin_must_be_exact() {
        let a = app();
        let with = |o: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::ORIGIN, HeaderValue::from_str(o).unwrap());
            origin_ok(&a, &h)
        };
        assert!(with("http://127.0.0.1:47811"));
        for bad in [
            "http://localhost:47811",
            "http://127.0.0.1:47812",
            "https://127.0.0.1:47811",
            "http://127.0.0.1:47811/",
            "http://evil.example",
            "null",
        ] {
            assert!(!with(bad), "{bad}");
        }
        assert!(!origin_ok(&a, &HeaderMap::new()));
    }

    #[test]
    fn csp_names_no_outside_host() {
        let csp = app().csp();
        assert!(csp.contains("script-src 'self';"));
        assert!(!csp.contains("unsafe-eval"));
        assert!(!csp.contains("script-src 'self' 'unsafe-inline'"));
        for part in csp.split(';') {
            for src in part.split_whitespace().skip(1) {
                assert!(
                    src.starts_with('\'') || src == "data:" || src == "ws://127.0.0.1:47811",
                    "outside source {src}"
                );
            }
        }
    }

    #[test]
    fn slots_are_bounded_and_given_back() {
        let n = AtomicUsize::new(0);
        let a = Slot::take(&n, 2).unwrap();
        let _b = Slot::take(&n, 2).unwrap();
        assert!(Slot::take(&n, 2).is_none());
        drop(a);
        assert!(Slot::take(&n, 2).is_some());
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn daemon_codes_map_to_browser_codes() {
        assert_eq!(browser_code(0), "daemon_unavailable");
        assert_eq!(
            browser_code(rlord_core::plugin::RPC_ERR_NOT_ALLOWED.into()),
            "not_allowed"
        );
        assert_eq!(browser_code(-32603), "failed");
    }
}
