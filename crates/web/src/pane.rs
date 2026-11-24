//! A pane in the browser: one tmux client on a PTY per websocket, attached
//! to a throwaway view session grouped with the target
//! (docs/working/web-ui-remote.md, "Pane streaming"; the tmux side is
//! `rlord_core::tmux::view_command` and friends).
//!
//! The pane socket: after `{"auth"}`, the page sends
//! `{"type":"open","target","cols","rows"}` (`target` is `overlord` or a
//! session reference). Then binary frames are bytes both ways (input in,
//! output out), and text frames are control: `{"type":"resize","cols",
//! "rows"}` and `{"type":"close"}` in; `{"type":"opened","target","id",
//! "name"}` and `{"type":"closed","reason"}` out. Opening never starts
//! anything: a target that isn't running is closed with reason
//! `not_running`, and the page offers Resume.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use tokio::io::unix::AsyncFd;

use rlord_core::tmux;

use crate::http::{self, App, Slot};

/// At most this many pane sockets at once.
pub const MAX_PANES: usize = 32;
/// The largest input frame.
pub const MAX_PANE_FRAME: usize = 64 << 10;
/// How long the page has to say what to open, after its token.
const OPEN_WAIT: Duration = Duration::from_secs(5);
/// How long the view session has to show the target once attached.
const VIEW_WAIT: Duration = Duration::from_secs(3);

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

/// A terminal size from the page, within reason.
fn size(v: &Value) -> Option<(u16, u16)> {
    let n = |k: &str| {
        v[k].as_u64()
            .filter(|n| (2..=1000).contains(n))
            .map(|n| n as u16)
    };
    Some((n("cols")?, n("rows")?))
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
    let t = match app
        .daemon
        .call("web.pane_target", json!({"target": target}))
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
    let view = view_name();
    let bridge = {
        let (session, view) = (session.clone(), view.clone());
        tokio::task::spawn_blocking(move || Bridge::attach(&session, &view, cols, rows)).await
    };
    let mut bridge = match bridge {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            closed(&mut ws, "not_running", &format!("{e:#}")).await;
            return;
        }
        Err(e) => {
            closed(&mut ws, "failed", &e.to_string()).await;
            return;
        }
    };
    let opened = json!({"type": "opened", "target": target, "id": t["id"], "name": t["name"]});
    let reason = if ws.send(text(opened)).await.is_err() {
        "gone"
    } else {
        pump(&mut ws, &bridge).await
    };
    if reason != "gone" {
        closed(&mut ws, reason, "").await;
    }
    tokio::task::spawn_blocking(move || bridge.end()).await.ok();
}

/// Bytes both ways until one side ends; why it ended.
async fn pump(ws: &mut WebSocket, b: &Bridge) -> &'static str {
    let mut buf = vec![0u8; 16 << 10];
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
            m = ws.recv() => match m {
                Some(Ok(Message::Binary(bytes))) => {
                    if b.pty.write_all(&bytes).await.is_err() {
                        return "ended";
                    }
                }
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or_default();
                    match v["type"].as_str() {
                        Some("resize") => {
                            if let Some((cols, rows)) = size(&v) {
                                let _ = b.pty.resize(cols, rows);
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

/// A fresh view session name.
fn view_name() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}{}-{}",
        tmux::VIEW_PREFIX,
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// A view's tmux client, running on a PTY.
struct Bridge {
    pty: Pty,
    child: std::process::Child,
    view: String,
}

impl Bridge {
    /// Make the web key table (root's mouse bindings now), start the
    /// view's tmux client on a PTY, and confirm the view shows `target`.
    /// Blocking (tmux).
    fn attach(target: &str, view: &str, cols: u16, rows: u16) -> Result<Bridge> {
        tmux::bind_web_table().context("making the web key table")?;
        let cmd = tmux::view_command(target, view, false)?;
        let (pty, child) = Pty::spawn(cmd, cols, rows)?;
        let mut b = Bridge {
            pty,
            child,
            view: view.to_string(),
        };
        let start = Instant::now();
        while !tmux::view_shows(view, target) {
            if start.elapsed() > VIEW_WAIT || b.child.try_wait().ok().flatten().is_some() {
                b.end();
                bail!("{target} went away");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(b)
    }

    /// Stop the client and make sure its view session is gone. Blocking.
    fn end(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            signal(self.child.id(), libc::SIGTERM);
            let start = Instant::now();
            while self.child.try_wait().ok().flatten().is_none() {
                if start.elapsed() > Duration::from_secs(1) {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        // (`destroy-unattached` should have done it.)
        if tmux::has_session(&self.view) {
            let _ = tmux::command()
                .args(["kill-session", "-t", &format!("={}", self.view)])
                .output();
        }
    }
}

fn signal(pid: u32, sig: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // Safety: kill(2) on our own unreaped child.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// The master side of a PTY, non-blocking, in tokio.
struct Pty {
    fd: AsyncFd<OwnedFd>,
}

impl Pty {
    /// Run `cmd` with a fresh PTY of `cols`×`rows` as its controlling
    /// terminal.
    fn spawn(
        mut cmd: std::process::Command,
        cols: u16,
        rows: u16,
    ) -> Result<(Pty, std::process::Child)> {
        use std::os::unix::process::CommandExt;
        let (mut m, mut s) = (0, 0);
        let mut ws = winsize(cols, rows);
        // Safety: plain libc calls on fds this function owns.
        unsafe {
            if libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                // A raw pointer: Linux's libc takes `*const winsize`,
                // macOS's `*mut` (a `&mut` trips clippy on Linux).
                &raw mut ws,
            ) != 0
            {
                bail!("openpty: {}", std::io::Error::last_os_error());
            }
            libc::fcntl(m, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(s, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        // Safety: freshly opened, owned here.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        cmd.stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave.try_clone()?);
        // Safety: only async-signal-safe calls between fork and exec. The
        // PTY becomes the child's controlling terminal (stdin is the slave).
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().context("starting the view's tmux client")?;
        drop(cmd);
        drop(slave);
        // Safety: fcntl on the fd owned above.
        unsafe {
            let fl = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        // Safety: an OwnedFd keeps its descriptor open, and the same one,
        // for as long as the AsyncFd owns it.
        let fd = unsafe { AsyncFd::register(master) }.map_err(|e| e.into_parts().1)?;
        Ok((Pty { fd }, child))
    }

    async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut guard = self.fd.readable().await?;
            let r = guard.try_io(|fd| {
                // Safety: read(2) into a buffer we own.
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match r {
                Ok(r) => return r,
                Err(_would_block) => continue,
            }
        }
    }

    async fn write_all(&self, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            let mut guard = self.fd.writable().await?;
            let r = guard.try_io(|fd| {
                // Safety: write(2) from a buffer we own.
                let n = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match r {
                Ok(Ok(n)) => data = &data[n..],
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// The terminal's new size; its client gets SIGWINCH.
    fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        let ws = winsize(cols, rows);
        // Safety: TIOCSWINSZ on our own PTY master.
        if unsafe { libc::ioctl(self.fd.get_ref().as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_bounded() {
        assert_eq!(size(&json!({"cols": 80, "rows": 24})), Some((80, 24)));
        assert_eq!(size(&json!({"cols": 1000, "rows": 2})), Some((1000, 2)));
        for bad in [
            json!({"cols": 0, "rows": 24}),
            json!({"cols": 80, "rows": 1}),
            json!({"cols": 1001, "rows": 24}),
            json!({"cols": "80", "rows": 24}),
            json!({"cols": 80}),
            json!({"cols": -1, "rows": 24}),
        ] {
            assert_eq!(size(&bad), None, "{bad}");
        }
    }

    #[test]
    fn view_names_are_views_and_unique() {
        let (a, b) = (view_name(), view_name());
        assert!(tmux::is_view(&a) && tmux::is_view(&b));
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn a_pty_carries_bytes_and_its_size() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "stty size; read line; echo got:$line"]);
        let (pty, mut child) = Pty::spawn(cmd, 91, 27).unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        while !String::from_utf8_lossy(&out).contains("27 91") {
            let n = pty.read(&mut buf).await.unwrap();
            out.extend_from_slice(&buf[..n]);
        }
        pty.write_all(b"hi\n").await.unwrap();
        while !String::from_utf8_lossy(&out).contains("got:hi") {
            match pty.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
        assert!(String::from_utf8_lossy(&out).contains("got:hi"), "{out:?}");
        pty.resize(100, 30).unwrap();
        let _ = child.wait();
    }
}
