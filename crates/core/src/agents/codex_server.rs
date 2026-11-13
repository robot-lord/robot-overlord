//! Codex's shared background server (its "daemon"): whether it runs,
//! starting it, and a small client for its control socket.
//!
//! Codex runs one app-server daemon per user. A Codex TUI started without
//! `--no-daemon` is a client of it, and the daemon runs the turn (and its
//! hooks). rlord's daemon-mode sessions (`codex::on_daemon`) run this way.
//!
//! - `codex app-server daemon version` prints JSON: `status`, `socketPath`,
//!   `cliVersion`, `appServerVersion`. The daemon updates itself, so it can
//!   be newer than the CLI. rlord runs `daemon start` when none runs, and
//!   never `update`, `restart` or `stop`.
//! - The control socket speaks JSON-RPC over WebSocket over a Unix socket
//!   (checked against a 0.169.0 daemon): an HTTP upgrade, then one JSON
//!   message per text frame, `initialize` / `initialized` first. Responses
//!   carry no `"jsonrpc"` field.
//!
//! The client is deliberately small and only reads, except
//! `turn/interrupt`: one connection per call, every wait bounded, nothing
//! subscribed (no `thread/resume`), so no notification or approval request
//! for a thread is ever addressed to it.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// What `codex app-server daemon version` says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DaemonInfo {
    /// `running`, or what else the daemon is (`stopped`, …).
    pub status: String,
    pub socket: Option<PathBuf>,
    pub cli_version: Option<String>,
    pub app_server_version: Option<String>,
}

impl DaemonInfo {
    /// From the command's JSON output.
    pub fn parse(text: &str) -> Option<DaemonInfo> {
        // (Whatever the CLI printed before it, the JSON is the last line.)
        let line = text
            .lines()
            .rev()
            .find(|l| l.trim_start().starts_with('{'))?;
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        let s = |k: &str| v[k].as_str().map(str::to_string).filter(|s| !s.is_empty());
        Some(DaemonInfo {
            status: s("status").unwrap_or_default(),
            socket: s("socketPath").map(PathBuf::from),
            cli_version: s("cliVersion"),
            app_server_version: s("appServerVersion"),
        })
    }

    pub fn running(&self) -> bool {
        self.status == "running"
    }

    /// The daemon's version, when it differs from the CLI's.
    pub fn differs(&self) -> Option<&str> {
        let d = self.app_server_version.as_deref()?;
        (self.cli_version.as_deref() != Some(d)).then_some(d)
    }
}

/// The control socket's usual place (what `daemon version` names).
pub fn default_socket() -> PathBuf {
    super::codex::home().join("app-server-control/app-server-control.sock")
}

/// What a command rlord runs to manage the daemon must not carry: which
/// rlord session or pane it was started from. A daemon started with them
/// would hand them to every hook it runs, for every thread.
const SCRUBBED_ENV: &[&str] = &["RLORD_SESSION_ID", "TMUX", "TMUX_PANE"];

fn daemon_cmd(program: &Path, verb: &str) -> Command {
    let mut c = Command::new(program);
    c.args(["app-server", "daemon", verb]);
    for v in SCRUBBED_ENV {
        c.env_remove(v);
    }
    // Not in any session's folder (a worktree can be removed under it).
    if let Some(home) = std::env::var_os("HOME") {
        c.current_dir(home);
    }
    c.stderr(std::process::Stdio::null());
    c
}

/// Where the output of a daemon command goes for a moment.
fn scratch_dir() -> PathBuf {
    crate::paths::data_dir().join("tmp")
}

/// `codex app-server daemon version`, at most a few seconds. None: Codex
/// couldn't say (too old to have a daemon, or it didn't answer).
pub fn version(program: &Path) -> Option<DaemonInfo> {
    let (st, out) = crate::process::run_to_file(
        daemon_cmd(program, "version"),
        &scratch_dir(),
        Duration::from_secs(5),
        None,
    )
    .ok()?;
    st.success().then(|| DaemonInfo::parse(&out)).flatten()
}

/// The daemon runs: asked, and started when it doesn't (`daemon start`,
/// with rlord's session variables left out of its environment). Bounded;
/// an error says why it isn't running.
pub fn ensure(program: &Path) -> Result<DaemonInfo> {
    if let Some(d) = version(program).filter(DaemonInfo::running) {
        return Ok(d);
    }
    let (st, out) = crate::process::run_to_file(
        daemon_cmd(program, "start"),
        &scratch_dir(),
        Duration::from_secs(20),
        None,
    )
    .context("starting Codex's background server")?;
    if !st.success() {
        bail!(
            "`codex app-server daemon start` failed: {}",
            out.lines().last().unwrap_or("").trim()
        );
    }
    version(program)
        .filter(DaemonInfo::running)
        .context("Codex's background server didn't come up")
}

/// What a thread is doing, as the daemon says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    /// Not loaded in the daemon (no turn can be running there).
    NotLoaded,
    Idle,
    /// A turn runs; `waiting`: it waits on an approval or the user's input.
    Active {
        waiting: bool,
    },
    SystemError,
}

impl ThreadState {
    /// From a `thread/read` result's (or a notification's) `status`.
    pub fn parse(status: &Value) -> Option<ThreadState> {
        Some(match status["type"].as_str()? {
            "notLoaded" => ThreadState::NotLoaded,
            "idle" => ThreadState::Idle,
            "systemError" => ThreadState::SystemError,
            "active" => ThreadState::Active {
                waiting: status["activeFlags"]
                    .as_array()
                    .is_some_and(|f| !f.is_empty()),
            },
            _ => return None,
        })
    }

    pub fn running(self) -> bool {
        matches!(self, ThreadState::Active { .. })
    }
}

/// Whether a thread's turn is running on the daemon. None: the daemon
/// couldn't be asked.
pub fn thread_state(socket: &Path, thread: &str) -> Option<ThreadState> {
    let mut c = Client::connect(socket, LIMIT).ok()?;
    let r = c
        .request("thread/read", json!({"threadId": thread}), LIMIT)
        .ok()?;
    ThreadState::parse(&r["thread"]["status"])
}

/// What `interrupt` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupted {
    /// Asked the running turn to stop.
    Asked,
    /// No turn was running.
    NotRunning,
}

/// Interrupt the thread's running turn, if one runs (`thread/turns/list`
/// for the latest turn, then `turn/interrupt`). It returns once the daemon
/// has the request; the caller waits for the turn to end.
pub fn interrupt(socket: &Path, thread: &str) -> Result<Interrupted> {
    let mut c = Client::connect(socket, LIMIT)?;
    let turns = c.request(
        "thread/turns/list",
        json!({"threadId": thread, "limit": 1, "sortDirection": "desc",
               "itemsView": "notLoaded"}),
        LIMIT,
    )?;
    let Some(turn) = turns["data"]
        .as_array()
        .and_then(|t| t.first())
        .filter(|t| t["status"] == "inProgress")
        .and_then(|t| t["id"].as_str())
    else {
        return Ok(Interrupted::NotRunning);
    };
    c.request(
        "turn/interrupt",
        json!({"threadId": thread, "turnId": turn}),
        LIMIT,
    )?;
    Ok(Interrupted::Asked)
}

/// Each step of a call (connect, one request) waits at most this.
const LIMIT: Duration = Duration::from_secs(3);

/// One connection to the daemon's control socket, initialized.
pub struct Client {
    stream: UnixStream,
    buf: Vec<u8>,
    next_id: u64,
}

impl Client {
    /// Connect, upgrade to WebSocket, and `initialize`.
    ///
    /// The name it gives (`clientInfo.name`) is the TUI's own: the daemon
    /// was seen (0.169.0) to keep the first name it is given after it
    /// starts as the originator of every thread it then creates, whoever
    /// creates them. Naming the TUI leaves that as it would be anyway.
    pub fn connect(socket: &Path, limit: Duration) -> Result<Client> {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("connecting to {}", socket.display()))?;
        stream.set_read_timeout(Some(limit))?;
        stream.set_write_timeout(Some(limit))?;
        let mut c = Client {
            stream,
            buf: Vec::new(),
            next_id: 1,
        };
        c.handshake(limit)?;
        c.request(
            "initialize",
            json!({"clientInfo": {"name": "codex-tui", "title": "rlord",
                                  "version": env!("CARGO_PKG_VERSION")}}),
            limit,
        )?;
        c.send(&json!({"method": "initialized"}))?;
        Ok(c)
    }

    fn handshake(&mut self, limit: Duration) -> Result<()> {
        let key = base64(uuid::Uuid::new_v4().as_bytes());
        let req = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        self.stream.write_all(req.as_bytes())?;
        let start = Instant::now();
        let end = loop {
            if let Some(i) = find(&self.buf, b"\r\n\r\n") {
                break i + 4;
            }
            if start.elapsed() > limit || self.buf.len() > 16 * 1024 {
                bail!("no WebSocket upgrade from Codex's server");
            }
            self.fill()?;
        };
        let head = String::from_utf8_lossy(&self.buf[..end]).into_owned();
        self.buf.drain(..end);
        if !head.starts_with("HTTP/1.1 101") {
            bail!(
                "Codex's server refused the upgrade: {}",
                head.lines().next().unwrap_or("")
            );
        }
        Ok(())
    }

    fn fill(&mut self) -> Result<()> {
        let mut chunk = [0u8; 16 * 1024];
        let n = self.stream.read(&mut chunk)?;
        if n == 0 {
            bail!("Codex's server closed the connection");
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    fn send(&mut self, msg: &Value) -> Result<()> {
        let frame = frame(0x1, msg.to_string().as_bytes(), mask());
        self.stream.write_all(&frame)?;
        Ok(())
    }

    /// Send a request and wait for its answer (notifications and requests
    /// from the server in between are passed over).
    pub fn request(&mut self, method: &str, params: Value, limit: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"id": id, "method": method, "params": params}))?;
        let start = Instant::now();
        loop {
            if start.elapsed() > limit {
                bail!("Codex's server didn't answer {method} in time");
            }
            let msg = self.message()?;
            let Ok(v) = serde_json::from_slice::<Value>(&msg) else {
                continue;
            };
            if v["id"] != json!(id) || v.get("method").is_some() {
                continue;
            }
            if let Some(e) = v.get("error") {
                bail!(
                    "{method}: {}",
                    e["message"].as_str().unwrap_or("error from Codex's server")
                );
            }
            return Ok(v["result"].clone());
        }
    }

    /// The next whole text message (pings answered, fragments joined).
    fn message(&mut self) -> Result<Vec<u8>> {
        let mut text = Vec::new();
        loop {
            let (fin, op, payload) = loop {
                if let Some((f, used)) = parse_frame(&self.buf)? {
                    self.buf.drain(..used);
                    break f;
                }
                self.fill()?;
            };
            match op {
                0x1 | 0x0 => {
                    text.extend_from_slice(&payload);
                    if text.len() > MAX_MESSAGE {
                        bail!("a message from Codex's server was too large");
                    }
                    if fin {
                        return Ok(text);
                    }
                }
                0x8 => bail!("Codex's server closed the connection"),
                0x9 => {
                    let pong = frame(0xA, &payload, mask());
                    self.stream.write_all(&pong)?;
                }
                _ => {}
            }
        }
    }
}

/// What rlord reads of one message at most (a `thread/read` answer is a
/// few kB; the server's own limit is 16 MB).
const MAX_MESSAGE: usize = 4 << 20;

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn mask() -> [u8; 4] {
    let b = *uuid::Uuid::new_v4().as_bytes();
    [b[0], b[1], b[2], b[3]]
}

/// A client frame (always masked, unfragmented).
fn frame(op: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut f = vec![0x80 | op];
    let n = payload.len();
    if n < 126 {
        f.push(0x80 | n as u8);
    } else if n < 65536 {
        f.push(0x80 | 126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    f.extend_from_slice(&mask);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    f
}

/// A frame read: whether it is a message's last, its opcode, its payload.
type Frame = (bool, u8, Vec<u8>);

/// One frame from the front of `buf`: (the frame, bytes used), or None
/// while it isn't all there.
fn parse_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let fin = buf[0] & 0x80 != 0;
    let op = buf[0] & 0x0F;
    let masked = buf[1] & 0x80 != 0;
    let (len, mut at) = match buf[1] & 0x7F {
        126 if buf.len() >= 4 => (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4),
        127 if buf.len() >= 10 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[2..10]);
            (
                usize::try_from(u64::from_be_bytes(b)).unwrap_or(usize::MAX),
                10,
            )
        }
        126 | 127 => return Ok(None),
        n => (n as usize, 2),
    };
    if len > MAX_MESSAGE {
        bail!("a frame from Codex's server was too large");
    }
    let key = if masked {
        if buf.len() < at + 4 {
            return Ok(None);
        }
        let k = [buf[at], buf[at + 1], buf[at + 2], buf[at + 3]];
        at += 4;
        Some(k)
    } else {
        None
    };
    if buf.len() < at + len {
        return Ok(None);
    }
    let mut payload = buf[at..at + len].to_vec();
    if let Some(k) = key {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= k[i % 4];
        }
    }
    Ok(Some(((fin, op, payload), at + len)))
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_version_json() {
        // codex-cli 0.167.2 with a newer, self-updated daemon.
        let out = r#"{"status":"running","backend":"pid","managedCodexPath":"/h/.codex/packages/app-server-daemon/current/bin/codex","managedCodexVersion":"0.169.0","socketPath":"/h/.codex/app-server-control/app-server-control.sock","cliVersion":"0.167.2","appServerVersion":"0.169.0"}"#;
        let d = DaemonInfo::parse(out).unwrap();
        assert!(d.running());
        assert_eq!(d.differs(), Some("0.169.0"));
        assert_eq!(
            d.socket.as_deref(),
            Some(Path::new(
                "/h/.codex/app-server-control/app-server-control.sock"
            ))
        );
        let same = DaemonInfo::parse(
            "warning: something\n{\"status\":\"stopped\",\"cliVersion\":\"1\",\"appServerVersion\":\"1\"}",
        )
        .unwrap();
        assert!(!same.running());
        assert_eq!(same.differs(), None);
        assert_eq!(DaemonInfo::parse("not json"), None);
    }

    #[test]
    fn thread_states() {
        let p = |v: Value| ThreadState::parse(&v);
        assert_eq!(p(json!({"type": "idle"})), Some(ThreadState::Idle));
        assert_eq!(
            p(json!({"type": "notLoaded"})),
            Some(ThreadState::NotLoaded)
        );
        assert_eq!(
            p(json!({"type": "active", "activeFlags": []})),
            Some(ThreadState::Active { waiting: false })
        );
        assert_eq!(
            p(json!({"type": "active", "activeFlags": ["waitingOnApproval"]})),
            Some(ThreadState::Active { waiting: true })
        );
        assert!(p(json!({"type": "active", "activeFlags": []}))
            .unwrap()
            .running());
        assert_eq!(p(json!({"type": "later"})), None);
    }

    #[test]
    fn frames_round_trip() {
        let f = frame(0x1, b"hello", [1, 2, 3, 4]);
        let ((fin, op, payload), used) = parse_frame(&f).unwrap().unwrap();
        assert!(fin);
        assert_eq!(
            (op, payload.as_slice(), used),
            (0x1, &b"hello"[..], f.len())
        );
        // Not all there yet.
        assert!(parse_frame(&f[..f.len() - 1]).unwrap().is_none());
        // A server frame (unmasked), 16-bit length.
        let body = vec![b'x'; 300];
        let mut s = vec![0x81, 126];
        s.extend_from_slice(&300u16.to_be_bytes());
        s.extend_from_slice(&body);
        let ((_, _, payload), used) = parse_frame(&s).unwrap().unwrap();
        assert_eq!((payload.len(), used), (300, s.len()));
        let big = frame(0x1, &vec![0u8; 70_000], [0; 4]);
        assert_eq!(parse_frame(&big).unwrap().unwrap().0 .2.len(), 70_000);
    }

    #[test]
    fn websocket_key_encoding() {
        assert_eq!(base64(b"the sample nonce"), "dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(base64(b"ab"), "YWI=");
    }

    /// A stand-in control socket: upgrade, then answer each request by
    /// method, with a notification first (as the daemon sends).
    #[test]
    fn a_client_reads_and_interrupts() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("c.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for conn in l.incoming().take(2) {
                let mut s = conn.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while find(&buf, b"\r\n\r\n").is_none() {
                    let n = s.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let end = find(&buf, b"\r\n\r\n").unwrap() + 4;
                buf.drain(..end);
                s.write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
                    .unwrap();
                let server = |v: Value| {
                    let t = v.to_string();
                    let mut f = vec![0x81];
                    if t.len() < 126 {
                        f.push(t.len() as u8);
                    } else {
                        f.push(126);
                        f.extend_from_slice(&(t.len() as u16).to_be_bytes());
                    }
                    f.extend_from_slice(t.as_bytes());
                    f
                };
                loop {
                    let got = loop {
                        if let Some((f, used)) = parse_frame(&buf).unwrap() {
                            buf.drain(..used);
                            break Some(f);
                        }
                        match s.read(&mut chunk) {
                            Ok(0) | Err(_) => break None,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let Some((_, _, payload)) = got else { break };
                    let v: Value = serde_json::from_slice(&payload).unwrap();
                    let method = v["method"].as_str().unwrap_or("").to_string();
                    tx.send(method.clone()).unwrap();
                    if v.get("id").is_none() {
                        continue;
                    }
                    let result = match method.as_str() {
                        "initialize" => json!({"userAgent": "x"}),
                        "thread/read" => {
                            json!({"thread": {"status": {"type": "active", "activeFlags": []}}})
                        }
                        "thread/turns/list" => {
                            json!({"data": [{"id": "turn-9", "status": "inProgress"}]})
                        }
                        _ => json!({}),
                    };
                    s.write_all(&server(json!({"method": "account/updated", "params": {}})))
                        .unwrap();
                    s.write_all(&server(json!({"id": v["id"], "result": result})))
                        .unwrap();
                }
            }
        });
        assert_eq!(
            thread_state(&sock, "t-1"),
            Some(ThreadState::Active { waiting: false })
        );
        assert_eq!(interrupt(&sock, "t-1").unwrap(), Interrupted::Asked);
        let seen: Vec<String> = rx.try_iter().collect();
        assert_eq!(
            seen,
            [
                "initialize",
                "initialized",
                "thread/read",
                "initialize",
                "initialized",
                "thread/turns/list",
                "turn/interrupt"
            ]
        );
        // No daemon there: nothing to say.
        assert_eq!(thread_state(&dir.path().join("none.sock"), "t-1"), None);
    }
}
