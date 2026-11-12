//! OpenCode's background server: where it is, a small client for it, and
//! what its events mean.
//!
//! OpenCode v2 runs one server per user (`opencode serve --service`,
//! started by the first `opencode` that needs it). `service.json` in its
//! state directory names the URL and the password (HTTP Basic, user
//! `opencode`); both change whenever the server restarts, so the file is
//! read again on every connection.
//!
//! The client is deliberately small: plain HTTP/1.1 to a local address
//! over a `TcpStream`, one request per connection, every wait bounded. It
//! speaks only to the address `service.json` names.
//!
//! `GET /api/event` is a server-sent event stream. It has no replay: what
//! happened while disconnected is gone, so a reader resyncs after every
//! connect. Each event is one `data:` line of JSON (`{type, data}`); the
//! server sends a comment line as a heartbeat every few seconds.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::opencode::is_session_id;

/// How to reach the server.
#[derive(Clone, PartialEq, Eq)]
pub struct Service {
    host: String,
    port: u16,
    password: String,
}

// (Never the password.)
impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Service(http://{}:{})", self.host, self.port)
    }
}

impl Service {
    /// From `service.json`'s text: `{"url": "http://127.0.0.1:49374",
    /// "password": "…", …}`. Only plain HTTP to this machine.
    pub fn parse(text: &str) -> Result<Service> {
        let v: Value = serde_json::from_str(text).context("reading service.json")?;
        let url = v["url"].as_str().context("service.json has no url")?;
        let rest = url
            .strip_prefix("http://")
            .with_context(|| format!("unexpected server URL {url}"))?;
        let authority = rest.split('/').next().unwrap_or("");
        let (host, port) = authority
            .rsplit_once(':')
            .with_context(|| format!("no port in server URL {url}"))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if !matches!(host, "127.0.0.1" | "localhost" | "::1") {
            bail!("OpenCode's server isn't on this machine ({host}): not followed");
        }
        Ok(Service {
            host: host.to_string(),
            port: port.parse().context("server port")?,
            password: v["password"].as_str().unwrap_or("").to_string(),
        })
    }

    /// From the file (None: no server has run, or the file can't be read).
    pub fn read(file: &Path) -> Option<Service> {
        Service::parse(&std::fs::read_to_string(file).ok()?).ok()
    }

    fn connect(&self, timeout: Duration) -> Result<TcpStream> {
        let addrs = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .context("resolving the server address")?;
        let mut last = None;
        for a in addrs {
            match TcpStream::connect_timeout(&a, Duration::from_secs(1)) {
                Ok(s) => {
                    s.set_read_timeout(Some(timeout))?;
                    s.set_write_timeout(Some(Duration::from_secs(5)))?;
                    return Ok(s);
                }
                Err(e) => last = Some(e),
            }
        }
        match last {
            Some(e) => Err(e).context("OpenCode's server isn't answering"),
            None => bail!("no address for OpenCode's server"),
        }
    }

    /// Send one request; the response's status and its body reader.
    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        accept: &str,
        read_timeout: Duration,
    ) -> Result<(u16, Body<BufReader<TcpStream>>)> {
        let mut stream = self.connect(read_timeout)?;
        let auth = base64(format!("opencode:{}", self.password).as_bytes());
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: Basic {auth}\r\n\
             Accept: {accept}\r\nConnection: close\r\n",
            self.host, self.port
        );
        let payload = body.map(Value::to_string);
        if let Some(p) = &payload {
            req.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                p.len()
            ));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes())?;
        if let Some(p) = &payload {
            stream.write_all(p.as_bytes())?;
        }
        let mut r = BufReader::new(stream);
        let (status, mode) = read_head(&mut r)?;
        Ok((status, Body::new(r, mode)))
    }

    /// One request and its whole answer: (status, body). `limit`: how long
    /// the server gets.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        limit: Duration,
    ) -> Result<(u16, String)> {
        let start = Instant::now();
        let (status, mut b) = self.send(method, path, body, "application/json", limit)?;
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            if start.elapsed() > limit {
                bail!("OpenCode's server took too long");
            }
            match b.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if timed_out(&e) => bail!("OpenCode's server took too long"),
                Err(e) => return Err(e.into()),
            }
            // (Nothing rlord asks for is large.)
            if out.len() > 4 * 1024 * 1024 {
                bail!("answer too large");
            }
        }
        Ok((status, String::from_utf8_lossy(&out).into_owned()))
    }

    /// `request` that must succeed (2xx): its JSON (null for an empty body).
    pub fn json(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        limit: Duration,
    ) -> Result<Value> {
        let (status, text) = self.request(method, path, body, limit)?;
        if !(200..300).contains(&status) {
            bail!("{method} {path}: {status} {}", clip_error(&text));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("{method} {path}: not JSON"))
    }

    /// Subscribe to events of `types`. A read on the stream waits at most
    /// `tick` (see `Events::poll`).
    pub fn events(&self, types: &[&str], tick: Duration) -> Result<Events> {
        let query: Vec<String> = types.iter().map(|t| format!("types={t}")).collect();
        let path = format!("/api/event?{}", query.join("&"));
        let (status, body) = self.send("GET", &path, None, "text/event-stream", tick)?;
        if status != 200 {
            bail!("subscribing to OpenCode's events: {status}");
        }
        Ok(Events {
            body,
            sse: Sse::default(),
            heard: Instant::now(),
        })
    }

    // ---- the calls rlord makes ----

    /// Whether the server has session `id` (None: it couldn't be asked).
    pub fn has_session(&self, id: &str) -> Option<bool> {
        if !is_session_id(id) {
            return Some(false);
        }
        let (status, _) = self
            .request(
                "GET",
                &format!("/api/session/{id}"),
                None,
                Duration::from_secs(3),
            )
            .ok()?;
        match status {
            200 => Some(true),
            404 => Some(false),
            _ => None,
        }
    }

    /// What session `id` is waiting on the user for, as `asked` names them
    /// (`perm:<id>`, `form:<id>`): its pending permission requests and
    /// forms. None: it couldn't be asked.
    pub fn pending(&self, id: &str) -> Option<Vec<String>> {
        if !is_session_id(id) {
            return Some(Vec::new());
        }
        let mut out = Vec::new();
        for (what, prefix) in [("permission", "perm"), ("form", "form")] {
            let v = self
                .json(
                    "GET",
                    &format!("/api/session/{id}/{what}"),
                    None,
                    Duration::from_secs(3),
                )
                .ok()?;
            out.extend(
                v["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x["id"].as_str())
                    .map(|x| format!("{prefix}:{x}")),
            );
        }
        Some(out)
    }

    /// The models the server offers, as `provider/model` (None: it
    /// couldn't be asked).
    pub fn models(&self) -> Option<Vec<String>> {
        let v = self
            .json("GET", "/api/model", None, Duration::from_secs(3))
            .ok()?;
        Some(model_names(&v))
    }

    /// Switch the model session `id` uses from its next turn on.
    pub fn set_model(&self, id: &str, model: &Value) -> Result<()> {
        if !is_session_id(id) {
            bail!("not an OpenCode session id");
        }
        self.json(
            "POST",
            &format!("/api/session/{id}/model"),
            Some(&serde_json::json!({ "model": model })),
            Duration::from_secs(5),
        )
        .map(|_| ())
    }

    /// Send session `id` a prompt, as if typed.
    pub fn prompt(&self, id: &str, text: &str) -> Result<()> {
        if !is_session_id(id) {
            bail!("not an OpenCode session id");
        }
        self.json(
            "POST",
            &format!("/api/session/{id}/prompt"),
            Some(&serde_json::json!({ "text": text })),
            Duration::from_secs(10),
        )
        .map(|_| ())
    }

    /// Text generated from session `id`'s context without adding to its
    /// history (`POST /api/session/{id}/generate`): the session's own
    /// model answers, and only its token totals change.
    pub fn generate(&self, id: &str, prompt: &str, limit: Duration) -> Result<String> {
        if !is_session_id(id) {
            bail!("not an OpenCode session id");
        }
        let v = self.json(
            "POST",
            &format!("/api/session/{id}/generate"),
            Some(&serde_json::json!({ "prompt": prompt })),
            limit,
        )?;
        v["data"]["text"]
            .as_str()
            .map(str::to_string)
            .context("no text in OpenCode's answer")
    }
}

/// `GET /api/model`'s answer as `provider/model` names, in its order.
fn model_names(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in v["data"].as_array().into_iter().flatten() {
        let Some(provider) = m["providerID"].as_str().filter(|p| !p.is_empty()) else {
            continue;
        };
        // (The model's own id; `id` may already carry the provider.)
        let Some(id) = m["modelID"]
            .as_str()
            .or(m["id"].as_str())
            .filter(|i| !i.is_empty())
        else {
            continue;
        };
        let name = match id.strip_prefix(provider).and_then(|r| r.strip_prefix('/')) {
            Some(_) => id.to_string(),
            None => format!("{provider}/{id}"),
        };
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

fn clip_error(text: &str) -> String {
    let message = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| text.to_string());
    message.chars().take(200).collect()
}

fn timed_out(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// ---- HTTP response reading ----

/// How a response's body is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    Chunked,
    Length(usize),
    /// Until the server closes the connection.
    ToEnd,
}

/// The status line and headers: (status, how the body is framed).
fn read_head(r: &mut impl BufRead) -> Result<(u16, Framing)> {
    let mut line = String::new();
    r.read_line(&mut line).context("reading the response")?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .with_context(|| format!("not an HTTP response: {}", line.trim()))?;
    let mut framing = Framing::ToEnd;
    for _ in 0..200 {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim();
        if l.is_empty() {
            return Ok((status, framing));
        }
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                framing = Framing::Chunked;
            } else if k == "content-length" && framing != Framing::Chunked {
                framing = Framing::Length(v.parse().unwrap_or(0));
            }
        }
    }
    bail!("response headers never ended")
}

/// Where a chunked body's reader is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunk {
    /// Before a chunk's size line.
    Size,
    /// Inside a chunk, this many bytes left.
    Data(usize),
    /// After a chunk's data, before its closing line break.
    End,
    Done,
}

/// A response body, unframed. A read that times out leaves it where it
/// was: the next read goes on (an event stream is read with short waits).
struct Body<R> {
    r: R,
    framing: Framing,
    chunk: Chunk,
    /// A size line read so far (a wait may end mid-line).
    line: Vec<u8>,
}

impl<R: BufRead> Body<R> {
    fn new(r: R, framing: Framing) -> Self {
        Body {
            r,
            framing,
            chunk: Chunk::Size,
            line: Vec::new(),
        }
    }

    /// A whole line into `self.line` (kept across timeouts). False: the
    /// connection ended first.
    fn line(&mut self) -> std::io::Result<bool> {
        self.r.read_until(b'\n', &mut self.line)?;
        Ok(self.line.last() == Some(&b'\n'))
    }
}

impl<R: BufRead> Read for Body<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        match self.framing {
            Framing::ToEnd => self.r.read(out),
            Framing::Length(left) => {
                if left == 0 {
                    return Ok(0);
                }
                let want = left.min(out.len());
                let n = self.r.read(&mut out[..want])?;
                self.framing = Framing::Length(left - n);
                Ok(n)
            }
            Framing::Chunked => loop {
                match self.chunk {
                    Chunk::Done => return Ok(0),
                    Chunk::Size => {
                        if !self.line()? {
                            self.chunk = Chunk::Done;
                            return Ok(0);
                        }
                        let text = String::from_utf8_lossy(&self.line).into_owned();
                        self.line.clear();
                        let hex = text.trim().split(';').next().unwrap_or("").trim();
                        let size = usize::from_str_radix(hex, 16).map_err(|_| {
                            std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size")
                        })?;
                        // (The last chunk: trailers, if any, are not read.)
                        self.chunk = if size == 0 {
                            Chunk::Done
                        } else {
                            Chunk::Data(size)
                        };
                    }
                    Chunk::Data(left) => {
                        let want = left.min(out.len());
                        let n = self.r.read(&mut out[..want])?;
                        if n == 0 {
                            self.chunk = Chunk::Done;
                            return Ok(0);
                        }
                        self.chunk = if left == n {
                            Chunk::End
                        } else {
                            Chunk::Data(left - n)
                        };
                        return Ok(n);
                    }
                    Chunk::End => {
                        if !self.line()? {
                            self.chunk = Chunk::Done;
                            return Ok(0);
                        }
                        self.line.clear();
                        self.chunk = Chunk::Size;
                    }
                }
            },
        }
    }
}

/// Server-sent events, from bytes as they arrive: each complete event's
/// `data`, parsed. Comment lines (heartbeats) and other fields are skipped.
#[derive(Default)]
struct Sse {
    pending: Vec<u8>,
    data: String,
}

impl Sse {
    fn feed(&mut self, bytes: &[u8]) -> Vec<Value> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(at) = self.pending.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.pending.drain(..=at).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                // The end of an event.
                if !self.data.is_empty() {
                    if let Ok(v) = serde_json::from_str::<Value>(&self.data) {
                        out.push(v);
                    }
                    self.data.clear();
                }
            } else if let Some(d) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(d.strip_prefix(' ').unwrap_or(d));
            }
        }
        // (An event never comes near this: a stream gone wrong.)
        if self.pending.len() > 8 * 1024 * 1024 || self.data.len() > 8 * 1024 * 1024 {
            self.pending.clear();
            self.data.clear();
        }
        out
    }
}

/// How long the stream may be silent (the server heartbeats every few
/// seconds) before it is taken as dead.
const SILENCE: Duration = Duration::from_secs(45);

/// An open event stream.
pub struct Events {
    body: Body<BufReader<TcpStream>>,
    sse: Sse,
    heard: Instant,
}

impl Events {
    /// Wait for events, at most the stream's tick: those that arrived
    /// (none when the wait ran out). An error means the stream is over
    /// (closed, broken, or silent too long): reconnect and resync.
    pub fn poll(&mut self) -> Result<Vec<Value>> {
        let mut buf = [0u8; 16 * 1024];
        match self.body.read(&mut buf) {
            Ok(0) => bail!("OpenCode's event stream ended"),
            Ok(n) => {
                self.heard = Instant::now();
                Ok(self.sse.feed(&buf[..n]))
            }
            Err(e) if timed_out(&e) => {
                if self.heard.elapsed() > SILENCE {
                    bail!("OpenCode's event stream went silent");
                }
                Ok(Vec::new())
            }
            Err(e) => Err(e).context("reading OpenCode's event stream"),
        }
    }
}

// ---- what events mean ----

/// The event types rlord subscribes to (everything else, such as text
/// deltas, is never sent).
pub const EVENT_TYPES: &[&str] = &[
    "session.created",
    "session.execution.started",
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.interrupted",
    "session.status",
    "session.idle",
    "session.inbox.enqueued",
    "permission.asked",
    "permission.replied",
    "form.created",
    "form.replied",
    "form.cancelled",
];

/// What an event says about a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// The session exists now (its TUI is up).
    Created,
    /// The user sent a prompt.
    Prompt,
    /// A turn is running.
    Busy,
    /// The turn is over (finished, failed or interrupted).
    Ended,
    /// It waits on the user: a permission request (`perm:<id>`) or a
    /// question form (`form:<id>`).
    Asked(String),
    /// That one was answered or withdrawn.
    Answered(String),
}

/// (session id, what the event says about it); None for events that say
/// nothing rlord follows.
pub fn map_event(v: &Value) -> Option<(String, Signal)> {
    let data = &v["data"];
    let session = |d: &Value| d["sessionID"].as_str().map(str::to_string);
    let id = |prefix: &str, d: &Value| d.as_str().map(|x| format!("{prefix}:{x}"));
    let signal = match v["type"].as_str()? {
        "session.created" => Signal::Created,
        "session.execution.started" => Signal::Busy,
        "session.execution.succeeded"
        | "session.execution.failed"
        | "session.execution.interrupted"
        | "session.idle" => Signal::Ended,
        "session.status" => match data["status"]["type"].as_str()? {
            "busy" | "retry" => Signal::Busy,
            "idle" => Signal::Ended,
            _ => return None,
        },
        "session.inbox.enqueued" if data["item"]["type"] == "user" => Signal::Prompt,
        "permission.asked" => Signal::Asked(id("perm", &data["id"])?),
        "permission.replied" => Signal::Answered(id("perm", &data["requestID"])?),
        "form.created" => {
            let form = &data["form"];
            return Some((session(form)?, Signal::Asked(id("form", &form["id"])?)));
        }
        "form.replied" | "form.cancelled" => Signal::Answered(id("form", &data["id"])?),
        _ => return None,
    };
    Some((session(data)?, signal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn service_json_names_a_local_server() {
        let s = Service::parse(
            r#"{"id":"svc_1","version":"2.2.7","url":"http://127.0.0.1:49374","pid":44730,"password":"s3cret"}"#,
        )
        .unwrap();
        assert_eq!((s.host.as_str(), s.port), ("127.0.0.1", 49374));
        assert_eq!(s.password, "s3cret");
        assert!(!format!("{s:?}").contains("s3cret"));
        let s = Service::parse(r#"{"url":"http://[::1]:8080/","password":""}"#).unwrap();
        assert_eq!((s.host.as_str(), s.port), ("::1", 8080));
        // Not local, not plain HTTP, or not there: nothing to connect to.
        for bad in [
            r#"{"url":"http://example.com:80","password":"x"}"#,
            r#"{"url":"https://127.0.0.1:443","password":"x"}"#,
            r#"{"url":"http://127.0.0.1","password":"x"}"#,
            r#"{"password":"x"}"#,
            "not json",
        ] {
            assert!(Service::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Service::read(Path::new("/nonexistent/service.json")), None);
    }

    #[test]
    fn the_servers_models_are_named_provider_slash_model() {
        let v = json!({"location": {}, "data": [
            {"id": "glm-5.3-flash-free", "providerID": "opencode", "name": "GLM"},
            {"id": "opencode/beam-free", "modelID": "beam-free", "providerID": "opencode"},
            {"id": "opencode/iceman-free", "providerID": "opencode"},
            {"id": "anthropic/claude", "providerID": "openrouter"},
            {"id": "glm-5.3-flash-free", "providerID": "opencode"},
            {"id": "x"},
        ]});
        assert_eq!(
            model_names(&v),
            [
                "opencode/glm-5.3-flash-free",
                "opencode/beam-free",
                "opencode/iceman-free",
                "openrouter/anthropic/claude"
            ]
        );
        assert!(model_names(&json!({})).is_empty());
    }

    #[test]
    fn basic_auth_encoding() {
        assert_eq!(base64(b"opencode:pw"), "b3BlbmNvZGU6cHc=");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b""), "");
    }

    #[test]
    fn responses_are_unframed() {
        let read = |raw: &str| {
            let mut r = Cursor::new(raw.as_bytes().to_vec());
            let (status, framing) = read_head(&mut r).unwrap();
            let mut out = String::new();
            Body::new(r, framing).read_to_string(&mut out).unwrap();
            (status, out)
        };
        assert_eq!(
            read("HTTP/1.1 200 OK\r\ncontent-length: 11\r\n\r\n{\"data\":{}}trailing"),
            (200, r#"{"data":{}}"#.to_string())
        );
        assert_eq!(
            read("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n7;x=1\r\n, world\r\n0\r\n\r\n"),
            (200, "hello, world".to_string())
        );
        assert_eq!(
            read("HTTP/1.1 404 Not Found\r\n\r\ngone"),
            (404, "gone".to_string())
        );
        assert_eq!(
            read("HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n").1,
            ""
        );
        assert!(read_head(&mut Cursor::new(b"garbage\r\n\r\n".to_vec())).is_err());
    }

    /// A reader that gives its bytes a few at a time, "timing out" between.
    struct Dripping {
        bytes: Vec<u8>,
        at: usize,
        step: usize,
        wait: bool,
    }

    impl Read for Dripping {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            self.wait = !self.wait;
            if self.wait {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            let n = self.step.min(out.len()).min(self.bytes.len() - self.at);
            out[..n].copy_from_slice(&self.bytes[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    #[test]
    fn a_stream_survives_waits_that_end_mid_chunk() {
        let event = |n: u8| format!("data: {{\"type\":\"session.idle\",\"n\":{n}}}\n\n");
        let mut raw = String::new();
        for part in [
            event(1),
            ": heartbeat\n\n".to_string(),
            format!("{}{}", event(2), event(3)),
        ] {
            raw.push_str(&format!("{:x}\r\n{part}\r\n", part.len()));
        }
        for step in [1, 2, 3, 7, 1000] {
            let drip = Dripping {
                bytes: raw.as_bytes().to_vec(),
                at: 0,
                step,
                wait: false,
            };
            // (One byte of buffer: every line is split across reads.)
            let mut body = Body::new(BufReader::with_capacity(step, drip), Framing::Chunked);
            let mut sse = Sse::default();
            let mut got = Vec::new();
            let mut buf = [0u8; 64];
            loop {
                match body.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => got.extend(sse.feed(&buf[..n])),
                    Err(e) if timed_out(&e) => continue,
                    Err(e) => panic!("{e}"),
                }
            }
            let ns: Vec<i64> = got.iter().filter_map(|v| v["n"].as_i64()).collect();
            assert_eq!(ns, [1, 2, 3], "step {step}");
        }
    }

    #[test]
    fn events_are_one_data_line_each() {
        let mut sse = Sse::default();
        assert!(sse.feed(b"data: {\"type\":\"server.conn").is_empty());
        let got =
            sse.feed(b"ected\",\"data\":{}}\r\n\r\n: heartbeat\n\nevent: x\ndata: not json\n\n");
        assert_eq!(got, [json!({"type": "server.connected", "data": {}})]);
    }

    #[test]
    fn events_map_to_what_a_session_is_doing() {
        let ev =
            |ty: &str, data: Value| json!({"id": "evt_1", "created": 1, "type": ty, "data": data});
        let s = "ses_1a";
        let on = |ty: &str| map_event(&ev(ty, json!({"sessionID": s})));
        assert_eq!(on("session.created"), Some((s.into(), Signal::Created)));
        assert_eq!(
            on("session.execution.started"),
            Some((s.into(), Signal::Busy))
        );
        for ended in [
            "session.execution.succeeded",
            "session.execution.failed",
            "session.execution.interrupted",
            "session.idle",
        ] {
            assert_eq!(on(ended), Some((s.into(), Signal::Ended)), "{ended}");
        }
        let status = |t: &str| {
            map_event(&ev(
                "session.status",
                json!({"sessionID": s, "status": {"type": t}}),
            ))
            .map(|x| x.1)
        };
        assert_eq!(status("busy"), Some(Signal::Busy));
        assert_eq!(status("retry"), Some(Signal::Busy));
        assert_eq!(status("idle"), Some(Signal::Ended));
        // A permission request and a question form block; their answers
        // (or a form's withdrawal) unblock, each by its own id.
        assert_eq!(
            map_event(&ev(
                "permission.asked",
                json!({"id": "per_1", "sessionID": s, "action": "shell", "resources": ["rm"]})
            )),
            Some((s.into(), Signal::Asked("perm:per_1".into())))
        );
        assert_eq!(
            map_event(&ev(
                "permission.replied",
                json!({"sessionID": s, "requestID": "per_1", "reply": "once"})
            )),
            Some((s.into(), Signal::Answered("perm:per_1".into())))
        );
        assert_eq!(
            map_event(&ev(
                "form.created",
                json!({"form": {"id": "frm_1", "sessionID": s, "title": "Which?", "fields": []}})
            )),
            Some((s.into(), Signal::Asked("form:frm_1".into())))
        );
        for done in ["form.replied", "form.cancelled"] {
            assert_eq!(
                map_event(&ev(done, json!({"id": "frm_1", "sessionID": s}))),
                Some((s.into(), Signal::Answered("form:frm_1".into())))
            );
        }
        // A prompt the user sent; not other things queued for the session.
        let inbox = |ty: &str| {
            map_event(&ev(
                "session.inbox.enqueued",
                json!({"sessionID": s, "inboxID": "msg_1", "item": {"type": ty}}),
            ))
        };
        assert_eq!(inbox("user"), Some((s.into(), Signal::Prompt)));
        assert_eq!(inbox("synthetic"), None);
        // Nothing rlord follows, or nothing it can place.
        assert_eq!(on("session.usage.updated"), None);
        assert_eq!(on("server.connected"), None);
        assert_eq!(map_event(&ev("session.created", json!({}))), None);
        // Every type mapped is subscribed to.
        for ty in [
            "session.created",
            "session.execution.started",
            "session.idle",
            "permission.asked",
            "form.cancelled",
            "session.inbox.enqueued",
        ] {
            assert!(EVENT_TYPES.contains(&ty), "{ty}");
        }
    }

    /// A stand-in server on a local port: answers each connection with
    /// the next canned response, and records what was asked.
    fn serve(responses: Vec<String>) -> (Service, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((mut s, _)) = listener.accept() else {
                    return;
                };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut head = String::new();
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0u8; len];
                let _ = r.read_exact(&mut body);
                let _ = tx.send(format!("{head}\n{}", String::from_utf8_lossy(&body)));
                let _ = s.write_all(response.as_bytes());
            }
        });
        let service = Service {
            host: "127.0.0.1".into(),
            port,
            password: "pw".into(),
        };
        (service, rx)
    }

    fn ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn requests_carry_the_password_and_read_the_answer() {
        let (svc, asked) = serve(vec![
            ok(r#"{"data":{"text":"Fixed the login test.","tokens":{},"cost":0}}"#),
            "HTTP/1.1 404 Not Found\r\ncontent-length: 43\r\n\r\n{\"_tag\":\"SessionNotFoundError\",\"message\":\"x\"}".into(),
            ok(r#"{"data":[{"id":"per_1","sessionID":"ses_1a"}]}"#),
            ok(r#"{"data":[{"id":"frm_9","sessionID":"ses_1a"}]}"#),
            "HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n".into(),
        ]);
        let limit = Duration::from_secs(5);
        assert_eq!(
            svc.generate("ses_1a", "Summarize", limit).unwrap(),
            "Fixed the login test."
        );
        let req = asked.recv().unwrap();
        assert!(
            req.starts_with("POST /api/session/ses_1a/generate HTTP/1.1\r\n"),
            "{req}"
        );
        assert!(
            req.contains("Authorization: Basic b3BlbmNvZGU6cHc=\r\n"),
            "{req}"
        );
        assert!(req.ends_with(r#"{"prompt":"Summarize"}"#), "{req}");
        assert_eq!(svc.has_session("ses_1a"), Some(false));
        assert_eq!(svc.pending("ses_1a").unwrap(), ["perm:per_1", "form:frm_9"]);
        svc.set_model("ses_1a", &json!({"providerID": "opencode", "id": "x-free"}))
            .unwrap();
        let req = asked.iter().nth(3).unwrap();
        assert!(req.starts_with("POST /api/session/ses_1a/model "), "{req}");
        assert!(
            req.ends_with(r#"{"model":{"id":"x-free","providerID":"opencode"}}"#),
            "{req}"
        );
        // An id that isn't one never reaches a URL.
        assert!(svc.generate("ses_1a/../x", "hi", limit).is_err());
        assert_eq!(svc.has_session("../x"), Some(false));
    }

    #[test]
    fn an_event_stream_is_read_in_ticks() {
        let part = "data: {\"type\":\"session.execution.started\",\"data\":{\"sessionID\":\"ses_1a\"}}\n\n";
        let (svc, asked) = serve(vec![format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{part}\r\n",
            part.len()
        )]);
        let mut events = svc
            .events(
                &["session.created", "session.idle"],
                Duration::from_millis(200),
            )
            .unwrap();
        let req = asked.recv().unwrap();
        assert!(
            req.starts_with("GET /api/event?types=session.created&types=session.idle HTTP/1.1"),
            "{req}"
        );
        let mut got = Vec::new();
        let mut over = false;
        for _ in 0..20 {
            match events.poll() {
                Ok(evs) => got.extend(evs),
                // The stand-in hangs up after its one response.
                Err(_) => {
                    over = true;
                    break;
                }
            }
        }
        assert!(over, "a closed stream is an error: reconnect");
        assert_eq!(
            got.iter().filter_map(map_event).collect::<Vec<_>>(),
            [("ses_1a".to_string(), Signal::Busy)]
        );
        // Nothing listening: an error, quickly.
        let gone = Service {
            host: "127.0.0.1".into(),
            port: 1,
            password: String::new(),
        };
        assert!(gone
            .events(EVENT_TYPES, Duration::from_millis(100))
            .is_err());
    }
}
