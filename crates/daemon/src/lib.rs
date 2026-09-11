pub mod rpc;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use overlord_core::db::Db;
use rpc::{Request, Response};

pub struct Daemon {
    db: Mutex<Db>,
}

impl Daemon {
    pub fn new(db: Db) -> Arc<Self> {
        Arc::new(Daemon { db: Mutex::new(db) })
    }

    /// Bind the socket (0700 dir, 0600 socket — the socket IS the auth boundary).
    pub fn bind(socket_path: &Path) -> Result<UnixListener> {
        let dir = socket_path.parent().context("socket path has no parent")?;
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        // A leftover socket from a dead daemon must not block startup; a live
        // daemon is detected by trying to connect first.
        if socket_path.exists() {
            if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
                anyhow::bail!(
                    "another daemon is already listening on {}",
                    socket_path.display()
                );
            }
            std::fs::remove_file(socket_path)?;
        }
        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("binding {}", socket_path.display()))?;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    pub async fn serve(self: Arc<Self>, listener: UnixListener) -> Result<()> {
        tracing::info!("daemon listening");
        loop {
            let (stream, _addr) = listener.accept().await?;
            let daemon = self.clone();
            tokio::spawn(async move {
                if let Err(e) = daemon.handle_conn(stream).await {
                    tracing::debug!(error = %e, "connection ended with error");
                }
            });
        }
    }

    async fn handle_conn(self: Arc<Self>, stream: UnixStream) -> Result<()> {
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let resp = match serde_json::from_str::<Request>(&line) {
                Ok(req) => self.dispatch(req),
                Err(e) => Response::err(0, rpc::ERR_BAD_REQUEST, format!("bad request: {e}")),
            };
            let mut out = serde_json::to_vec(&resp)?;
            out.push(b'\n');
            write_half.write_all(&out).await?;
        }
        Ok(())
    }

    fn dispatch(&self, req: Request) -> Response {
        let id = req.id;
        match self.call(&req.method, req.params) {
            Ok(result) => Response::ok(id, result),
            Err(MethodError::NotFound) => Response::err(
                id,
                rpc::ERR_METHOD_NOT_FOUND,
                format!("no such method: {}", req.method),
            ),
            Err(MethodError::Internal(e)) => {
                tracing::error!(method = %req.method, error = %e, "method failed");
                Response::err(id, rpc::ERR_INTERNAL, e.to_string())
            }
        }
    }

    fn call(&self, method: &str, params: Value) -> std::result::Result<Value, MethodError> {
        match method {
            "ping" => Ok(json!({"pong": true, "version": env!("CARGO_PKG_VERSION")})),
            "session.list" => {
                let include_archived = params
                    .get("archived")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let sessions = self
                    .db
                    .lock()
                    .expect("db mutex poisoned")
                    .list_sessions(include_archived)
                    .map_err(MethodError::Internal)?;
                Ok(serde_json::to_value(sessions).map_err(|e| MethodError::Internal(e.into()))?)
            }
            _ => Err(MethodError::NotFound),
        }
    }
}

enum MethodError {
    NotFound,
    Internal(anyhow::Error),
}

/// Blocking one-shot client used by the CLI: connect, send one request, read one response.
pub fn client_call(socket_path: &PathBuf, method: &str, params: Value) -> Result<Value> {
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket_path).with_context(|| {
        format!(
            "connecting to daemon at {} (is `overlord daemon` running?)",
            socket_path.display()
        )
    })?;
    let req = json!({"id": 1, "method": method, "params": params});
    let mut buf = serde_json::to_vec(&req)?;
    buf.push(b'\n');
    stream.write_all(&buf)?;
    let mut line = String::new();
    StdBufReader::new(&mut stream).read_line(&mut line)?;
    let resp: Value = serde_json::from_str(&line).context("parsing daemon response")?;
    if let Some(err) = resp.get("error") {
        anyhow::bail!(
            "daemon error: {}",
            err.get("message").and_then(Value::as_str).unwrap_or("?")
        );
    }
    Ok(resp.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ping_and_list_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let daemon = Daemon::new(Db::open_in_memory().unwrap());
        let listener = Daemon::bind(&sock).unwrap();
        let server = tokio::spawn(daemon.serve(listener));

        let sock2 = sock.clone();
        let pong =
            tokio::task::spawn_blocking(move || client_call(&sock2, "ping", Value::Null).unwrap())
                .await
                .unwrap();
        assert_eq!(pong["pong"], json!(true));

        let list = tokio::task::spawn_blocking(move || {
            client_call(&sock, "session.list", json!({})).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(list, json!([]));

        server.abort();
    }
}
