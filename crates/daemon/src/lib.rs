pub mod ask;
pub mod autostart;
pub mod import;
pub mod mcp;
pub mod methods;
pub mod overlord;
pub mod profile;
pub mod rpc;
pub mod triage;
pub mod ui;
pub mod warm;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

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
    /// Push stream to subscribers, one JSON line per message:
    /// `{"notify":"changed"}` — coarse tick, refetch what you display;
    /// `{"notify":"ui","cmd":{...}}` — a UI command for the TUI (set_view,
    /// new_session) issued by the overlord.
    notify: tokio::sync::broadcast::Sender<String>,
    /// The TUI's current list view, as it last reported it.
    pub(crate) ui_view: Mutex<Value>,
    /// Last UI command sent while no TUI was subscribed, replayed to the
    /// next TUI that starts (if fresh).
    pending_ui: Mutex<Option<(std::time::Instant, Value)>>,
    tui_subscribers: AtomicUsize,
    /// The terminal (tmux client) the overlord overlay is open on, and that
    /// overlay's token (so a replaced overlay's late "closed" report can't
    /// clear its successor).
    pub(crate) overlay: Mutex<Option<(String, u64)>>,
    pub(crate) overlay_seq: std::sync::atomic::AtomicU64,
    /// Which overlord seat the open overlay shows (`overlord`/`mastermind`).
    pub(crate) overlay_seat: Mutex<&'static str>,
    /// Background loading of sessions' sub-agent data (see warm.rs).
    pub(crate) warm: warm::Warm,
}

impl Daemon {
    pub fn new(db: Db, config: Config) -> Arc<Self> {
        let (notify, _) = tokio::sync::broadcast::channel(64);
        Arc::new(Daemon {
            db: Mutex::new(db),
            config,
            repo_scan: Mutex::new(None),
            notify,
            ui_view: Mutex::new(Value::Null),
            pending_ui: Mutex::new(None),
            tui_subscribers: AtomicUsize::new(0),
            overlay: Mutex::new(None),
            overlay_seq: std::sync::atomic::AtomicU64::new(1),
            overlay_seat: Mutex::new(overlord::FRONT.key),
            warm: Default::default(),
        })
    }

    pub(crate) fn db(&self) -> MutexGuard<'_, Db> {
        self.db.lock().expect("db mutex poisoned")
    }

    pub(crate) fn changed(&self) {
        let _ = self.notify.send(r#"{"notify":"changed"}"#.to_string());
    }

    pub(crate) fn tui_count(&self) -> usize {
        self.tui_subscribers.load(Ordering::SeqCst)
    }

    /// Push a UI command to TUIs; park it for the next TUI if none is up.
    pub(crate) fn ui_cmd(&self, cmd: Value) {
        if self.tui_count() == 0 {
            *self.pending_ui.lock().expect("ui mutex") =
                Some((std::time::Instant::now(), cmd.clone()));
        }
        let _ = self
            .notify
            .send(json!({"notify": "ui", "cmd": cmd}).to_string());
    }

    /// Internal call that maps method errors to anyhow (used by the MCP layer).
    pub(crate) fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        match self.call(method, params) {
            Ok(v) => Ok(v),
            Err(MethodError::NotFound) => anyhow::bail!("no such method: {method}"),
            Err(MethodError::Internal(e)) => Err(e),
        }
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
        // Read positions saved by the last run: transcripts are read forward
        // from there, not from the start.
        {
            use rlord_core::agents::claude::{seed_cursors, STATS_CURSOR_SEED};
            if let Err(e) = db.prune_transcript_cursors(5 * STATS_CURSOR_SEED) {
                tracing::warn!(error = %e, "pruning transcript read positions");
            }
            match db.transcript_cursors(STATS_CURSOR_SEED) {
                Ok(saved) => seed_cursors(saved),
                Err(e) => tracing::warn!(error = %e, "loading transcript read positions"),
            }
        }
        for params in drain_spool()? {
            if let Err(e) = methods::hook_emit(&db, &params) {
                tracing::warn!(error = %e, "spooled hook event failed");
            }
        }
        methods::save_cursors(&db);
        if let Err(e) = methods::rename_legacy_tmux(&db) {
            tracing::warn!(error = %e, "renaming legacy tmux sessions");
        }
        methods::reconcile(&db)?;
        drop(db);
        self.bind_nav_keys();
        profile::sync_all(self, false);
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
                    let mut rx = self.notify.subscribe();
                    // TUIs identify themselves so the daemon knows whether a
                    // UI command has anyone to land on.
                    let is_tui = req.params.get("client").and_then(Value::as_str) == Some("tui");
                    if is_tui {
                        self.tui_subscribers.fetch_add(1, Ordering::SeqCst);
                    }
                    let r = loop {
                        let line = match rx.recv().await {
                            Ok(line) => line,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                r#"{"notify":"changed"}"#.to_string()
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break Ok(()),
                        };
                        if write_half
                            .write_all(format!("{line}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            break Ok(()); // client gone
                        }
                    };
                    if is_tui {
                        self.tui_subscribers.fetch_sub(1, Ordering::SeqCst);
                    }
                    return r;
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
            return Ok(json!({"pong": true, "version": env!("CARGO_PKG_VERSION"),
                             "build": build_id(), "exe": exe_id()}));
        }
        if let Some(r) = self.call_ui(method, &params) {
            return r.map_err(MethodError::Internal);
        }
        let mutating = !matches!(
            method,
            "session.list"
                | "session.get"
                | "session.log"
                | "repo.list"
                | "repo.find"
                | "session.defaults"
                | "tag.list"
        ) && !(method == "tag.order" && params.get("order").is_none());
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
            "tag.list" => methods::tag_list(&db),
            "overlord.mark_reviewed" => triage::mark_reviewed(&db, &params),
            "tag.rename" => methods::tag_rename(&db, &params),
            "tag.order" => methods::tag_order(&db, &params),
            "session.pin" => methods::session_pin(&db, &params),
            "session.describe" => methods::session_describe(&db, &params),
            "session.notes" => methods::session_notes(&db, &params),
            "session.group" => methods::session_group(&db, &params),
            "session.log" => methods::session_log(&db, &params),
            "hook.emit" => {
                let r = methods::hook_emit(&db, &params);
                methods::save_cursors(&db);
                r
            }
            _ => return Err(MethodError::NotFound),
        };
        drop(db);
        // Rows the warm-up hasn't loaded yet say so (the list shows it).
        let r = match (method, r) {
            ("session.list", Ok(Value::Array(mut rows))) => {
                let loading = self.warm.loading();
                for row in &mut rows {
                    let id = row["id"].as_str().unwrap_or("");
                    row["loading"] = json!(loading.contains(id));
                }
                Ok(Value::Array(rows))
            }
            (_, r) => r,
        };
        if method == "session.unarchive" && r.is_ok() {
            if let Some(id) = params["ref"].as_str() {
                warm::enqueue_ref(self, id);
            }
        }
        if mutating && r.is_ok() {
            self.changed();
            if method == "session.new" {
                self.bind_nav_keys();
            }
            // A new or respawned tmux session starts on rlord's defaults.
            let created = match (method, &r) {
                ("session.new", Ok(v)) => v["id"].as_str().map(str::to_string),
                ("session.restart", _) => params["ref"].as_str().map(str::to_string),
                _ => None,
            };
            if let Some(id) = created {
                profile::sync_ref(self, &id);
            }
        }
        r.map_err(MethodError::Internal)
    }
}

impl Daemon {
    /// Methods that manage their own locking: the overlord, UI control, and
    /// the MCP bridge (which re-enters `call`). None = not one of these.
    fn call_ui(&self, method: &str, params: &Value) -> Option<Result<Value>> {
        let r = match method {
            "overlord.ensure" => {
                let r = overlord::ensure(&self.db(), &self.config, params);
                if let Ok(v) = &r {
                    self.changed();
                    self.bind_nav_keys();
                    // A (re)started overlord is a fresh tmux session: give it
                    // the user's keys again.
                    if let Some(id) = v["id"].as_str() {
                        profile::sync_ref(self, id);
                    }
                }
                r
            }
            "overlord.get" => overlord::seat(params["seat"].as_str().unwrap_or("overlord"))
                .session(&self.db())
                .and_then(|o| Ok(serde_json::to_value(o)?)),
            "overlord.status" => overlord::status(&self.db(), &self.config),
            "session.subagents" => warm::session_subagents(self, params),
            "overlord.style" => {
                overlord::style(&self.config);
                Ok(json!({"styled": true}))
            }
            "session.import_scan" => import::scan(self, params),
            "session.import" => {
                let r = import::import(self, params);
                if let Ok(v) = &r {
                    self.changed();
                    // Imported with history: load it in the background.
                    if let Some(id) = v["id"].as_str() {
                        warm::enqueue_ref(self, id);
                    }
                }
                r
            }
            "overlord.ask" => ask::ask(self, params),
            "overlord.hand_off" => ask::hand_off(self, params),
            "overlord.triage" => triage::triage(self, params),
            "overlord.new_session" => ask::new_session(self, params),
            "overlord.set_model" => {
                let r = overlord::set_model(&self.db(), &self.config, params);
                if r.is_ok() {
                    self.changed();
                }
                r
            }
            "ui.state" => ui::state(self),
            "ui.goto" => ui::goto(self, params),
            "ui.confirm_new_session" => ui::confirm_new_session(self, params),
            "ui.overlord" => ui::overlord(self, params),
            "admin.reset" => ui::reset(self, params),
            "tmux.profile" => profile::profile(self, params),
            "tmux.reload" => profile::reload(self),
            "ui.overlay.close_on" => ui::close_overlay_on(self, params),
            "ui.overlay.report" => {
                // The overlay closed itself (Option-O inside it, or exit).
                if params.get("open").and_then(Value::as_bool) == Some(false) {
                    let token = params.get("token").and_then(Value::as_u64);
                    let mut o = self.overlay.lock().expect("ui mutex");
                    if o.as_ref().map(|(_, t)| Some(*t)) == Some(token) {
                        *o = None;
                    }
                }
                Ok(json!({"ok": true}))
            }
            "ui.show_list" => ui::show_list(self),
            "ui.view.set" => ui::set_view(self, params),
            "ui.view.default" => ui::default_view(self),
            "ui.view.report" => {
                *self.ui_view.lock().expect("ui mutex") =
                    params.get("view").cloned().unwrap_or(Value::Null);
                Ok(json!({"ok": true}))
            }
            "ui.pending" => {
                let p = self.pending_ui.lock().expect("ui mutex").take();
                Ok(
                    p.filter(|(at, _)| at.elapsed() < std::time::Duration::from_secs(30))
                        .map(|(_, cmd)| cmd)
                        .unwrap_or(Value::Null),
                )
            }
            "ui.propose_session" => ui::propose_session(self, params),
            "mcp.tools" => Ok(mcp::tools()),
            "mcp.call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                tracing::info!(tool = name, caller = ?params.get("caller"), "mcp tool call");
                mcp::call(self, name, &args)
            }
            _ => return None,
        };
        Some(r)
    }
}

enum MethodError {
    NotFound,
    Internal(anyhow::Error),
}

/// Canonical path of the binary this process was started from (pinned when
/// first asked). Clients only restart a daemon running their own binary, so
/// two installs (say a worktree's debug build and ~/.local/bin/rlord) don't
/// take turns restarting the shared daemon.
pub fn exe_id() -> String {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    })
    .clone()
}

/// Identity of the binary this process was started from (its mtime when
/// first asked): a daemon whose build differs from the CLI's is stale.
pub fn build_id() -> u64 {
    static ID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *ID.get_or_init(|| {
        std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .and_then(std::fs::metadata)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    })
}

/// Blocking one-shot client used by the CLI: connect, send one request, read one response.
pub fn client_call(socket_path: &Path, method: &str, params: Value) -> Result<Value> {
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
                tmux_session: Some("rlord_01test"),
                launch_spec: "{}",
                created_at: 1,
                repo_path: None,
                workspace_path: None,
                branch: None,
                role: "",
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
