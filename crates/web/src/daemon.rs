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

/// How long a call may take (`overlord.ensure` may start an agent).
const CALL_LIMIT: Duration = Duration::from_secs(60);

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

/// The daemon's push stream.
pub struct Subscription {
    lines: Lines<BufReader<OwnedReadHalf>>,
    /// Held: dropping it would end the subscription.
    _w: OwnedWriteHalf,
}

impl Subscription {
    /// The next change's rev; None once the daemon has gone.
    pub async fn next(&mut self) -> Option<String> {
        loop {
            let line = self.lines.next_line().await.ok()??;
            let v: Value = serde_json::from_str(&line).unwrap_or_default();
            if v["notify"] == "changed" {
                return Some(v["rev"].as_str().unwrap_or("").to_string());
            }
        }
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
