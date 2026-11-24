//! A pane in the browser: one tmux client on a PTY per websocket, attached
//! to a throwaway view session grouped with the target
//! (docs/working/web-ui-remote.md, "Pane streaming"; the tmux side is
//! `rlord_core::tmux::view_command` and friends).
//!
//! The pane socket: after `{"auth"}`, the page sends
//! `{"type":"open","target","cols","rows","page"}` (`target` is `overlord`
//! or a session reference; `page` the id its control socket's hello gave
//! it). Then binary frames are bytes both ways (input in, output out), and
//! text frames are control: `{"type":"resize","cols","rows"}` and
//! `{"type":"close"}` in; `{"type":"opened","target","id","name",
//! "control","cols","rows","window"}` and `{"type":"closed","reason"}`
//! out. Opening never starts anything: a target that isn't running is
//! closed with reason `not_running`, and the page offers Resume.
//!
//! Control (docs/working/web-ui-remote.md, slice 5 decision 3): the
//! daemon says who holds each pane (`web.pane_open`, and its `control`
//! map after that). A page that holds it types into it and its resizes pin
//! the window; one that doesn't is a viewer: its input is dropped and its
//! client draws at the holder's size, which the page scales to fit.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::bridge::{size, view_name, Bridge};
use crate::http::{self, App, Slot};

/// At most this many pane sockets at once.
pub const MAX_PANES: usize = 32;
/// The largest input frame.
pub const MAX_PANE_FRAME: usize = 64 << 10;
/// How long the page has to say what to open, after its token.
const OPEN_WAIT: Duration = Duration::from_secs(5);

pub async fn upgrade(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    up: WebSocketUpgrade,
) -> Response {
    if !http::origin_ok(&app, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if app.panes.load(Ordering::SeqCst) >= MAX_PANES {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    up.max_message_size(MAX_PANE_FRAME)
        .max_frame_size(MAX_PANE_FRAME)
        .on_upgrade(move |ws| async move {
            let Some(_slot) = Slot::take(&app.panes, MAX_PANES) else {
                return;
            };
            pane(&app, ws).await;
        })
}

fn text(v: Value) -> Message {
    Message::Text(v.to_string().into())
}

/// Close the socket, saying why.
async fn closed(ws: &mut WebSocket, reason: &str, message: &str) {
    let _ = ws
        .send(text(
            json!({"type": "closed", "reason": reason, "message": message}),
        ))
        .await;
    let _ = ws.send(Message::Close(None)).await;
}

/// One pane socket, after the upgrade.
async fn pane(app: &Arc<App>, mut ws: WebSocket) {
    if !http::authenticate(app, &mut ws).await {
        return;
    }
    let open = match tokio::time::timeout(OPEN_WAIT, ws.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Value>(&t).unwrap_or_default(),
        _ => Value::Null,
    };
    let (Some(target), Some((cols, rows))) = (
        open["target"].as_str().filter(|_| open["type"] == "open"),
        size(&open),
    ) else {
        closed(
            &mut ws,
            "bad_request",
            "expected {\"type\":\"open\",\"target\",\"cols\",\"rows\"}",
        )
        .await;
        return;
    };
    // The page's id (from its control socket's hello): this surface's
    // name in the daemon's control map.
    // (A page that doesn't say is a surface of its own.)
    let surface = match open["page"].as_str().filter(|p| !p.is_empty()) {
        Some(p) => format!("page:{p}"),
        None => format!("page:{}", crate::bridge::view_name()),
    };
    let t = match app
        .daemon
        .call(
            "web.pane_open",
            json!({"target": target, "surface": surface, "cols": cols, "rows": rows}),
        )
        .await
    {
        Ok(t) => t,
        Err(e) => {
            closed(&mut ws, "not_found", &e.message).await;
            return;
        }
    };
    let session = t["tmux_session"].as_str().unwrap_or("").to_string();
    if t["running"] != true || session.is_empty() {
        closed(&mut ws, "not_running", "it isn't running").await;
        return;
    }
    let id = t["id"].as_str().unwrap_or("").to_string();
    // A viewer's client draws at the window's size.
    let (cols, rows) = size(&t).unwrap_or((cols, rows));
    let view = view_name();
    let bridge = {
        let (session, view) = (session.clone(), view.clone());
        tokio::task::spawn_blocking(move || Bridge::attach(&session, &view, cols, rows, false))
            .await
    };
    let mut bridge = match bridge {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            pane_closed(app, &id, &surface);
            closed(&mut ws, "not_running", &format!("{e:#}")).await;
            return;
        }
        Err(e) => {
            pane_closed(app, &id, &surface);
            closed(&mut ws, "failed", &e.to_string()).await;
            return;
        }
    };
    let opened = json!({"type": "opened", "target": target, "id": t["id"], "name": t["name"],
        "control": t["control"] == true, "cols": cols, "rows": rows, "window": t["window"]});
    let me = Me {
        session: id.clone(),
        surface: surface.clone(),
        control: t["control"] == true,
        size: (cols, rows),
    };
    let reason = if ws.send(text(opened)).await.is_err() {
        "gone"
    } else {
        pump(app, &mut ws, &bridge, me).await
    };
    if reason != "gone" {
        closed(&mut ws, reason, "").await;
    }
    pane_closed(app, &id, &surface);
    tokio::task::spawn_blocking(move || bridge.end()).await.ok();
}

/// This socket's pane and surface, and whether it holds the pane.
struct Me {
    session: String,
    surface: String,
    control: bool,
    size: (u16, u16),
}

impl Me {
    /// Follow the daemon's control map: whether our keys reach the pane;
    /// as a viewer, the size to draw at (the holder's).
    fn update(&mut self, map: &Value, pty: &crate::bridge::Pty) {
        let entry = &map[&self.session];
        self.control = entry["holder"].as_str() == Some(self.surface.as_str());
        if !self.control {
            if let Some(s) = size(entry).filter(|s| *s != self.size) {
                self.size = s;
                let _ = pty.resize(s.0, s.1);
            }
        }
    }
}

/// The daemon: this surface stopped showing the pane.
fn pane_closed(app: &Arc<App>, session: &str, surface: &str) {
    if session.is_empty() {
        return;
    }
    let d = app.daemon.clone();
    let params = json!({"target": session, "surface": surface});
    tokio::spawn(async move {
        let _ = d.call("web.pane_closed", params).await;
    });
}

/// Bytes both ways until one side ends; why it ended. Input reaches the
/// pane only while this page holds it (a viewer's is dropped).
async fn pump(app: &Arc<App>, ws: &mut WebSocket, b: &Bridge, mut me: Me) -> &'static str {
    let mut buf = vec![0u8; 16 << 10];
    let mut control = app.control.subscribe();
    me.update(&control.borrow_and_update(), &b.pty);
    loop {
        tokio::select! {
            n = b.pty.read(&mut buf) => match n {
                Ok(0) | Err(_) => return "ended",
                Ok(n) => {
                    if ws.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        return "gone";
                    }
                }
            },
            c = control.changed() => {
                if c.is_ok() {
                    me.update(&control.borrow_and_update(), &b.pty);
                }
            }
            m = ws.recv() => match m {
                Some(Ok(Message::Binary(bytes))) => {
                    if me.control && b.pty.write_all(&bytes).await.is_err() {
                        return "ended";
                    }
                }
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or_default();
                    match v["type"].as_str() {
                        Some("resize") => {
                            if let Some((cols, rows)) = size(&v) {
                                me.size = (cols, rows);
                                let _ = b.pty.resize(cols, rows);
                                // Pins the window again if this page holds it.
                                let d = app.daemon.clone();
                                let params = json!({"target": me.session, "surface": me.surface,
                                    "cols": cols, "rows": rows});
                                tokio::spawn(async move {
                                    let _ = d.call("web.pane_size", params).await;
                                });
                            }
                        }
                        Some("close") => return "closed",
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return "gone",
                Some(Ok(_)) => {}
            },
        }
    }
}
