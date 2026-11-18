//! The plugin's client for the daemon's Unix socket (newline-delimited
//! JSON, `crates/daemon/src/rpc.rs`). Its own small client: the plugin
//! doesn't depend on `rlord-daemon`. One connection per call; the
//! subscription keeps one open.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

/// How long a call may take: `overlord.ensure` and `session.resume` may
/// start an agent, and `overlord.ask` waits up to 90 s for a just-started
/// overlord to take input (`crates/daemon/src/ask.rs`).
const CALL_LIMIT: Duration = Duration::from_secs(120);

/// A failed call: the daemon's error (its RPC code), or no daemon (code 0).
#[derive(Debug, Clone)]
pub struct CallError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CallError {}

impl CallError {
    fn unreachable(e: impl std::fmt::Display) -> CallError {
        CallError {
            code: 0,
            message: format!("the rlord daemon isn't answering: {e}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Daemon {
    socket: PathBuf,
}

impl Daemon {
    pub fn new(socket: PathBuf) -> Daemon {
        Daemon { socket }
    }

    async fn connect(&self) -> Result<UnixStream, CallError> {
        UnixStream::connect(&self.socket)
            .await
            .map_err(|e| CallError::unreachable(format!("{}: {e}", self.socket.display())))
    }

    /// One request, one answer.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        let s = self.connect().await?;
        let (r, mut w) = s.into_split();
        let mut lines = BufReader::new(r).lines();
        let go = async {
            send(&mut w, method, params).await?;
            answer(&mut lines).await
        };
        tokio::time::timeout(CALL_LIMIT, go)
            .await
            .map_err(|_| CallError::unreachable(format!("{method} took too long")))?
    }

    /// `web.hello`: this plugin, its protocol, and the port it listens on.
    pub async fn hello(&self, port: u16) -> Result<Value, CallError> {
        self.call(
            "web.hello",
            json!({
                "protocol": rlord_core::plugin::WEB_PROTOCOL,
                "version": env!("CARGO_PKG_VERSION"),
                "pid": std::process::id(),
                "port": port,
            }),
        )
        .await
    }

    /// `web.pages`: the pages open now, the whole list.
    pub async fn pages(&self, ids: Vec<String>) -> Result<Value, CallError> {
        self.call("web.pages", json!({"pages": ids})).await
    }

    /// `web.remote_reply`: the answer to the user's relay command `id`
    /// (an `Event::Remote`), `ok` or an error message.
    pub async fn remote_reply(
        &self,
        id: &str,
        answer: Result<Value, String>,
    ) -> Result<Value, CallError> {
        self.call("web.remote_reply", remote_reply_params(id, answer))
            .await
    }

    /// `web.remote_state`: this plugin's relay state, the whole of it, on
    /// every change (`{relay, connected, error, devices, pairing,
    /// pending}`; crates/daemon/src/relay.rs `normalize`).
    pub async fn remote_state(&self, state: Value) -> Result<Value, CallError> {
        self.call("web.remote_state", state).await
    }

    /// `web.subscribe`: the current rev, and the stream of later ones.
    pub async fn subscribe(&self) -> Result<(String, Subscription), CallError> {
        let s = self.connect().await?;
        let (r, mut w) = s.into_split();
        let mut lines = BufReader::new(r).lines();
        send(&mut w, "web.subscribe", json!({})).await?;
        let ack = answer(&mut lines).await?;
        let rev = ack["rev"].as_str().unwrap_or("").to_string();
        Ok((rev, Subscription { lines, _w: w }))
    }
}

/// `web.remote_reply`'s parameters.
fn remote_reply_params(id: &str, answer: Result<Value, String>) -> Value {
    match answer {
        Ok(v) => json!({"id": id, "ok": v}),
        Err(e) => json!({"id": id, "error": e}),
    }
}

/// What the daemon's push stream says.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Something changed; the state's new rev.
    Changed(String),
    /// A UI command for one page (`crates/daemon/src/ui.rs`).
    Ui { page: String, cmd: Value },
    /// The user's relay command (`crates/daemon/src/relay.rs`): answer it
    /// with `Daemon::remote_reply(id, …)` within 10 s. `req` is
    /// `{"cmd": "setup" | "pair" | "pair_cancel" | "confirm" | "reject" |
    /// "revoke" | "disable", …}`.
    Remote { id: String, req: Value },
}

/// The daemon's push stream.
pub struct Subscription {
    lines: Lines<BufReader<OwnedReadHalf>>,
    /// Held: dropping it would end the subscription.
    _w: OwnedWriteHalf,
}

impl Subscription {
    /// The next change or UI command; None once the daemon has gone.
    pub async fn next(&mut self) -> Option<Event> {
        loop {
            let line = self.lines.next_line().await.ok()??;
            if let Some(e) = event(&line) {
                return Some(e);
            }
        }
    }
}

/// A line of the push stream as an `Event` (None: not one this plugin
/// knows).
fn event(line: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(line).ok()?;
    match v["notify"].as_str()? {
        "changed" => Some(Event::Changed(v["rev"].as_str().unwrap_or("").to_string())),
        "ui" => Some(Event::Ui {
            page: v["page"].as_str()?.to_string(),
            cmd: v.get("cmd")?.clone(),
        }),
        "remote" => Some(Event::Remote {
            id: v["id"].as_str()?.to_string(),
            req: v.get("req").filter(|r| r.is_object())?.clone(),
        }),
        _ => None,
    }
}

async fn send(w: &mut OwnedWriteHalf, method: &str, params: Value) -> Result<(), CallError> {
    let mut line = json!({"id": 1, "method": method, "params": params}).to_string();
    line.push('\n');
    w.write_all(line.as_bytes())
        .await
        .map_err(CallError::unreachable)
}

async fn answer(lines: &mut Lines<BufReader<OwnedReadHalf>>) -> Result<Value, CallError> {
    let line = lines
        .next_line()
        .await
        .map_err(CallError::unreachable)?
        .ok_or_else(|| CallError::unreachable("it hung up"))?;
    let v: Value = serde_json::from_str(&line).map_err(CallError::unreachable)?;
    if let Some(e) = v.get("error") {
        return Err(CallError {
            code: e["code"].as_i64().unwrap_or(-1),
            message: e["message"].as_str().unwrap_or("?").to_string(),
        });
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_lines_are_events() {
        assert_eq!(
            event(r#"{"notify":"changed","rev":"b-3"}"#),
            Some(Event::Changed("b-3".into()))
        );
        assert_eq!(
            event(r#"{"notify":"ui","page":"p1-ab","cmd":{"cmd":"show_list"}}"#),
            Some(Event::Ui {
                page: "p1-ab".into(),
                cmd: json!({"cmd": "show_list"})
            })
        );
        assert_eq!(event(r#"{"notify":"ui","cmd":{"cmd":"show_list"}}"#), None);
        assert_eq!(
            event(r#"{"id":"r1","notify":"remote","req":{"cmd":"pair"}}"#),
            Some(Event::Remote {
                id: "r1".into(),
                req: json!({"cmd": "pair"})
            })
        );
        assert_eq!(event(r#"{"notify":"remote","req":{"cmd":"pair"}}"#), None);
        assert_eq!(event(r#"{"notify":"remote","id":"r1","req":"pair"}"#), None);
        assert_eq!(
            remote_reply_params("r1", Ok(json!({"qr": "q"}))),
            json!({"id": "r1", "ok": {"qr": "q"}})
        );
        assert_eq!(
            remote_reply_params("r2", Err("no".into())),
            json!({"id": "r2", "error": "no"})
        );
        assert_eq!(event(r#"{"notify":"other"}"#), None);
        assert_eq!(event("not json"), None);
    }
}
