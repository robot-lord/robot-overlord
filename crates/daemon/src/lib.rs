pub mod methods;
pub mod rpc;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use rlord_core::config::Config;
use rlord_core::db::Db;
use rlord_core::{paths, tmux};
use rpc::{Request, Response};

pub struct Daemon {
    db: Mutex<Db>,
    config: Config,
    /// Repo-finder scan cache (the scan walks the home dir; a minute is plenty).
    repo_scan: Mutex<Option<(std::time::Instant, Vec<String>)>>,
    /// Coarse change signal: any successful mutation broadcasts one tick;
    /// subscribers refetch what they display. Fine-grained events can come
    /// later without changing the wire shape.
    changes: tokio::sync::broadcast::Sender<()>,
}

impl Daemon {
    pub fn new(db: Db, config: Config) -> Arc<Self> {
        let (changes, _) = tokio::sync::broadcast::channel(64);
        Arc::new(Daemon {
            db: Mutex::new(db),
            config,
            repo_scan: Mutex::new(None),
            changes,
        })
    }

    /// Git repos under the configured roots, cached for 60s.
    fn scanned_repos(&self) -> Vec<String> {
        let mut cache = self.repo_scan.lock().expect("scan mutex poisoned");
        if let Some((at, v)) = &*cache {
            if at.elapsed() < std::time::Duration::from_secs(60) {
                return v.clone();
            }
        }
        let v = rlord_core::git::find_repos(
            &self.config.session.repo_roots,
            self.config.session.repo_scan_depth,
        );
        *cache = Some((std::time::Instant::now(), v.clone()));
        v
    }

    /// Install the navigation bindings on the tmux server. Best-effort: no
    /// server yet (nothing running) is normal at daemon start; we retry on
    /// every session launch.
    pub fn bind_nav_keys(&self) {
        let bindings = self.config.tmux.bindings();
        match tmux::bind_nav_keys(&methods::rlord_bin(), &bindings) {
            Ok(conflicts) => {
                for c in conflicts {
                    tracing::warn!(key = %c.key, bound_to = %c.bound_to,
                        "nav key skipped: already taken — change [tmux] in config.toml");
                }
            }
            Err(e) => tracing::debug!(error = %e, "nav keys not bound (no tmux server yet?)"),
        }
    }

    /// Startup housekeeping: drain spooled hook events, then reconcile
    /// DB state against the live tmux server.
    pub fn startup(&self) -> Result<()> {
        let db = self.db.lock().expect("db mutex poisoned");
        for params in drain_spool()? {
            if let Err(e) = methods::hook_emit(&db, &params) {
                tracing::warn!(error = %e, "spooled hook event failed");
            }
        }
        methods::reconcile(&db)?;
        drop(db);
        self.bind_nav_keys();
        Ok(())
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
            // A subscription takes over the connection: ack, then push a
            // notify line on every change until the client hangs up.
            if let Ok(req) = serde_json::from_str::<Request>(&line) {
                if req.method == "events.subscribe" {
                    let ack = Response::ok(req.id, json!({"subscribed": true}));
                    let mut out = serde_json::to_vec(&ack)?;
                    out.push(b'\n');
                    write_half.write_all(&out).await?;
                    let mut rx = self.changes.subscribe();
                    loop {
                        match rx.recv().await {
                            Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                if write_half
                                    .write_all(b"{\"notify\":\"changed\"}\n")
                                    .await
                                    .is_err()
                                {
                                    return Ok(()); // client gone
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                        }
                    }
                }
            }
            let resp = match serde_json::from_str::<Request>(&line) {
                Ok(req) => {
                    let daemon = self.clone();
                    tokio::task::spawn_blocking(move || daemon.dispatch(req))
                        .await
                        .context("dispatch task panicked")?
                }
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
                tracing::warn!(method = %req.method, error = %e, "method failed");
                Response::err(id, rpc::ERR_INTERNAL, e.to_string())
            }
        }
    }

    fn call(&self, method: &str, params: Value) -> std::result::Result<Value, MethodError> {
        if method == "ping" {
            return Ok(json!({"pong": true, "version": env!("CARGO_PKG_VERSION")}));
        }
        let mutating = !matches!(
            method,
            "session.list"
                | "session.get"
                | "session.log"
                | "repo.list"
                | "repo.find"
                | "session.defaults"
        );
        let db = self.db.lock().expect("db mutex poisoned");
        let r = match method {
            "session.list" => methods::session_list(&db, &params),
            "session.new" => methods::session_new(&db, &self.config, &params),
            "repo.list" => methods::repo_list(&db, &self.config),
            "repo.find" => {
                let scanned = self.scanned_repos();
                methods::repo_find(&db, &self.config, scanned)
            }
            "session.defaults" => methods::session_defaults(&self.config),
            "session.get" => methods::session_get(&db, &params),
            "session.restart" => methods::session_restart(&db, &params),
            "session.stop" => methods::session_stop(&db, &params),
            "session.archive" => methods::session_archive(&db, &params),
            "session.unarchive" => methods::session_unarchive(&db, &params),
            "session.rename" => methods::session_rename(&db, &params),
            "session.tag" => methods::session_tag(&db, &params),
            "session.pin" => methods::session_pin(&db, &params),
            "session.group" => methods::session_group(&db, &params),
            "session.log" => methods::session_log(&db, &params),
            "hook.emit" => methods::hook_emit(&db, &params),
            _ => return Err(MethodError::NotFound),
        };
        if mutating && r.is_ok() {
            let _ = self.changes.send(());
            if method == "session.new" {
                self.bind_nav_keys();
            }
        }
        r.map_err(MethodError::Internal)
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
            "connecting to daemon at {} (is `rlord daemon` running?)",
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

// ---- hook spool: hook evidence must never be lost to a down daemon ----

fn spool_path() -> PathBuf {
    paths::data_dir().join("spool/hooks.jsonl")
}

/// Append a hook.emit params object to the spool (used by `rlord hook emit`
/// when the daemon is unreachable).
pub fn spool_hook_event(params: &Value) -> Result<()> {
    let path = spool_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{params}")?;
    Ok(())
}

fn drain_spool() -> Result<Vec<Value>> {
    let path = spool_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Ok(vec![]);
    };
    let events: Vec<Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if !events.is_empty() {
        tracing::info!(count = events.len(), "draining spooled hook events");
    }
    std::fs::remove_file(&path).ok();
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn ping_and_list_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let daemon = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
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

    #[tokio::test(flavor = "multi_thread")]
    async fn hook_emit_updates_status_and_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let daemon = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        {
            let db = daemon.db.lock().unwrap();
            db.insert_session(&rlord_core::db::NewSession {
                id: "01test",
                name: "t",
                agent: "claude",
                agent_session: Some("uuid-1"),
                project_path: "/tmp",
                tmux_session: "rlord_01test",
                launch_spec: "{}",
                created_at: 1,
                repo_path: None,
                workspace_path: None,
                branch: None,
            })
            .unwrap();
        }
        let listener = Daemon::bind(&sock).unwrap();
        let server = tokio::spawn(daemon.clone().serve(listener));

        let call = |method: &'static str, params: Value| {
            let sock = sock.clone();
            tokio::task::spawn_blocking(move || client_call(&sock, method, params).unwrap())
        };
        call(
            "hook.emit",
            json!({"instance": "01test", "agent": "claude", "event": "UserPromptSubmit",
                   "payload": {"session_id": "uuid-1", "transcript_path": "/tmp/t.jsonl"}}),
        )
        .await
        .unwrap();
        let s = call("session.get", json!({"ref": "01test"})).await.unwrap();
        assert_eq!(s["status"], "working");
        assert!(s["last_user_msg_at"].is_i64());
        assert_eq!(s["agent_session_trust"], "hook"); // minted id confirmed

        call(
            "hook.emit",
            json!({"instance": "01test", "agent": "claude", "event": "Stop",
                   "payload": {"session_id": "uuid-1"}}),
        )
        .await
        .unwrap();
        let s = call("session.get", json!({"ref": "01test"})).await.unwrap();
        assert_eq!(s["status"], "idle");
        assert!(s["last_agent_msg_at"].is_i64());

        let log = call("session.log", json!({"ref": "01test"})).await.unwrap();
        let kinds: Vec<&str> = log
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["prompt_submitted", "agent_stopped"]);

        server.abort();
    }
}
