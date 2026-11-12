pub mod ask;
pub mod autostart;
pub mod discover;
pub mod import;
pub mod mcp;
pub mod methods;
pub mod overlord;
pub mod profile;
pub mod remote;
pub mod rpc;
pub mod search;
pub mod summary;
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
    /// The user's config; `config.reload` swaps it after the key screen
    /// saves (read it with `config()`).
    config: std::sync::RwLock<Arc<Config>>,
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
    /// Held while text is pasted into the mastermind, and while an idle one
    /// is restarted fresh: the two must not overlap.
    pub(crate) mastermind_send: Mutex<()>,
    /// Background loading of sessions' sub-agent data (see warm.rs).
    pub(crate) warm: warm::Warm,
    /// Remote Control: availability and each session's link (remote.rs).
    pub(crate) remote: remote::Remote,
    /// Archive summaries being written (summary.rs).
    pub(crate) summaries: summary::Summaries,
}

impl Daemon {
    pub fn new(db: Db, config: Config) -> Arc<Self> {
        let (notify, _) = tokio::sync::broadcast::channel(64);
        Arc::new(Daemon {
            db: Mutex::new(db),
            config: std::sync::RwLock::new(Arc::new(config)),
            repo_scan: Mutex::new(None),
            notify,
            ui_view: Mutex::new(Value::Null),
            pending_ui: Mutex::new(None),
            tui_subscribers: AtomicUsize::new(0),
            overlay: Mutex::new(None),
            overlay_seq: std::sync::atomic::AtomicU64::new(1),
            overlay_seat: Mutex::new(overlord::FRONT.key),
            mastermind_send: Mutex::new(()),
            warm: Default::default(),
            remote: Default::default(),
            summaries: Default::default(),
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

    /// The config as last loaded.
    pub fn config(&self) -> Arc<Config> {
        self.config.read().expect("config lock").clone()
    }

    /// Re-read config.toml (the key screen saved it) and re-apply what
    /// depends on it: the global keys and each session's key hints.
    pub(crate) fn reload_config(&self) -> Result<Value> {
        let cfg = Config::load()?;
        *self.config.write().expect("config lock") = Arc::new(cfg);
        self.bind_nav_keys();
        profile::sync_all(self, false);
        overlord::style(&self.config());
        Ok(json!({"reloaded": true}))
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
            &self.config().session.repo_roots,
            self.config().session.repo_scan_depth,
        );
        *cache = Some((std::time::Instant::now(), v.clone()));
        v
    }

    /// Install the navigation bindings on the tmux server. Best-effort: no
    /// server yet (nothing running) is normal at daemon start; we retry on
    /// every session launch.
    pub fn bind_nav_keys(&self) {
        let cfg = self.config();
        let bindings = cfg.tmux.bindings();
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
        methods::set_claude_tui(&self.config());
        let db = self.db.lock().expect("db mutex poisoned");
        // Read positions saved by the last run: transcripts are read forward
        // from there, not from the start.
        {
            use rlord_core::agents::transcript::{seed_cursors, STATS_CURSOR_SEED};
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
        if let Err(e) = methods::sync_tmux_names(&db) {
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
            "session.new" => methods::session_new(&db, &self.config(), &params),
            "repo.list" => methods::repo_list(&db, &self.config()),
            "repo.find" => {
                let scanned = self.scanned_repos();
                methods::repo_find(&db, &self.config(), scanned)
            }
            "session.defaults" => methods::session_defaults(&self.config()),
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
                let summarizing = self.summaries.pending();
                for row in &mut rows {
                    let id = row["id"].as_str().unwrap_or("").to_string();
                    row["loading"] = json!(loading.contains(&id));
                    row["summarizing"] = json!(summarizing.contains(&id));
                }
                remote::annotate(self, &mut rows);
                Ok(Value::Array(rows))
            }
            ("session.get", Ok(row)) => {
                let mut rows = [row];
                remote::annotate(self, &mut rows);
                let [row] = rows;
                Ok(row)
            }
            (_, r) => r,
        };
        // Archived: write down what it did (once, in the background).
        if method == "session.archive" && r.is_ok() && self.config().archive.summary {
            if let Ok(s) = self.db().resolve_ref(params["ref"].as_str().unwrap_or("")) {
                // Once; again only after more work (summary.rs checks).
                summary::enqueue(self, &s.id, false);
            }
        }
        if method == "session.unarchive" && r.is_ok() {
            if let Some(id) = params["ref"].as_str() {
                warm::enqueue_ref(self, id);
            }
        }
        // (A hook that changed nothing, e.g. one per tool call, isn't a change:
        // no list refetch.)
        let unchanged = method == "hook.emit" && r.as_ref().is_ok_and(|v| v["unchanged"] == true);
        if mutating && r.is_ok() && !unchanged {
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
                let r = overlord::ensure(&self.db(), &self.config(), params);
                // (ensure syncs the seat's tmux keys itself.)
                if r.is_ok() {
                    self.changed();
                    self.bind_nav_keys();
                }
                r
            }
            // `seat: "all"`: both seats' sessions (those that exist), in order.
            "overlord.get" if params["seat"] == "all" => {
                let db = self.db();
                overlord::SEATS
                    .iter()
                    .map(|seat| seat.session(&db))
                    .collect::<Result<Vec<_>>>()
                    .and_then(|v| {
                        Ok(serde_json::to_value(
                            v.into_iter().flatten().collect::<Vec<_>>(),
                        )?)
                    })
            }
            "overlord.get" => overlord::seat(params["seat"].as_str().unwrap_or("overlord"))
                .session(&self.db())
                .and_then(|o| Ok(serde_json::to_value(o)?)),
            "overlord.status" => {
                // The db guard must drop before offer() locks it again.
                let st = overlord::status(&self.db(), &self.config());
                st.map(|mut v| {
                    v["remote"] = remote::offer(self);
                    v
                })
            }
            // Don't offer Remote Control for the overlord again.
            "remote.decline_offer" => self
                .db()
                .kv_set("remote_offer_declined", "1")
                .map(|_| json!({"declined": true})),
            // Don't offer this seat's default model again (the user said no).
            "overlord.decline_default" => {
                let seat = overlord::seat(params["seat"].as_str().unwrap_or("overlord"));
                let m = params["model"].as_str().unwrap_or("");
                self.db()
                    .kv_set(&format!("default_declined:{}", seat.key), m)
                    .map(|_| json!({"declined": m}))
            }
            "agents.list" => methods::agents_list(),
            "session.subagents" => warm::session_subagents(self, params),
            "history.search" => search::search(self, params),
            "history.read" => search::read(self, params),
            "session.summarize" => search::summarize(self, params),
            "remote.availability" => remote::availability(self, params),
            "remote.status" => remote::status(self, params),
            "remote.set" => remote::set(self, params),
            "overlord.style" => {
                overlord::style(&self.config());
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
                let r = overlord::set_model(&self.db(), &self.config(), params);
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
            "ui.keys" => ui::keys_open(self, params),
            "keys.list" => ui::keys_list(self),
            "config.reload" => self.reload_config(),
            "ui.view.set" => ui::set_view(self, params),
            "ui.view.default" => ui::default_view(self),
            // The list's selected session: set with `id`, read without.
            "ui.selected" => match params.get("id").and_then(Value::as_str) {
                Some(id) => self
                    .db()
                    .kv_set("ui_selected", id)
                    .map(|_| json!({"id": id})),
                None => self.db().kv_get("ui_selected").map(|v| json!({"id": v})),
            },
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
                // The mastermind does the work handed to it; it can't hand
                // off to itself.
                if name == "hand_off" {
                    let caller = params["caller"].as_str().unwrap_or("");
                    let is_mastermind = self
                        .db()
                        .resolve_ref(caller)
                        .is_ok_and(|s| s.role == overlord::MASTERMIND.key);
                    if is_mastermind {
                        return Some(Err(anyhow::anyhow!(
                            "you are the mastermind: do it yourself (hand_off is the overlord's)"
                        )));
                    }
                }
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

// ---- hooks: the client side (`rlord hook emit`) ----

/// The fields of a hook payload rlord uses. Everything else (a tool's
/// whole input and output, prompts) stays out of the daemon and the spool.
const HOOK_FIELDS: &[&str] = &[
    "hook_event_name",
    "session_id",
    "transcript_path",
    "cwd",
    "source",
    "reason",
    "notification_type",
    "message",
    "tool_name",
    "tool_use_id",
    "agent_id",
    "agent_type",
    "agent_transcript_path",
    "last_assistant_message",
];

/// What rlord needs of a hook's stdin: the fields above, long ones cut.
/// Stdin that isn't whole JSON (cut off at the read limit by a huge tool
/// input) still yields the fields found in it, so the event is known.
pub fn slim_hook_payload(raw: &str) -> Value {
    let mut out = serde_json::Map::new();
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => {
            for k in HOOK_FIELDS {
                if let Some(x) = v.get(*k).filter(|x| !x.is_null()) {
                    out.insert((*k).to_string(), x.clone());
                }
            }
        }
        Err(_) => {
            // Each field's first `"name":` followed by a JSON string. (Inside
            // a string value the quotes are escaped, so this is a real key.)
            for k in HOOK_FIELDS {
                let key = format!("\"{k}\":");
                let Some(at) = raw.find(&key) else { continue };
                let rest = raw[at + key.len()..].trim_start();
                let value = serde_json::Deserializer::from_str(rest)
                    .into_iter::<String>()
                    .next()
                    .and_then(|r| r.ok());
                if let Some(text) = value {
                    out.insert((*k).to_string(), json!(text));
                }
            }
        }
    }
    for (k, max) in [("message", 300), ("last_assistant_message", 4000)] {
        if let Some(Value::String(text)) = out.get_mut(k) {
            if text.chars().count() > max {
                *text = text.chars().take(max).collect();
            }
        }
    }
    Value::Object(out)
}

/// What became of a hook event sent to the daemon.
#[derive(Debug, PartialEq, Eq)]
pub enum HookSent {
    Delivered,
    /// The daemon isn't there: keep the event for when it is.
    Unreachable,
    /// It took the event but didn't answer in time: it has it (or will);
    /// spooling too would deliver it twice.
    Slow,
}

/// Send one hook event, never waiting long: a hook runs inside the agent's
/// tool call, and a stalled daemon must not hold that up. A second to
/// connect and send, two for the answer.
pub fn send_hook_event(socket_path: &Path, params: &Value) -> HookSent {
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    use std::sync::mpsc;
    use std::time::Duration;
    let (tx, rx) = mpsc::channel::<HookSent>();
    let path = socket_path.to_path_buf();
    let req = json!({"id": 1, "method": "hook.emit", "params": params});
    // On its own thread: a connect to a socket nobody accepts on can block.
    std::thread::spawn(move || {
        let sent = (|| {
            let mut stream = std::os::unix::net::UnixStream::connect(&path).ok()?;
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .ok()?;
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
            let mut buf = serde_json::to_vec(&req).ok()?;
            buf.push(b'\n');
            stream.write_all(&buf).ok()?;
            Some(stream)
        })();
        let Some(mut stream) = sent else {
            let _ = tx.send(HookSent::Unreachable);
            return;
        };
        let mut line = String::new();
        let answered = StdBufReader::new(&mut stream).read_line(&mut line);
        let _ = tx.send(match answered {
            Ok(n) if n > 0 => HookSent::Delivered,
            _ => HookSent::Slow,
        });
    });
    // (Not connected within the first second: unreachable.)
    match rx.recv_timeout(Duration::from_millis(3500)) {
        Ok(sent) => sent,
        Err(_) => HookSent::Unreachable,
    }
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

    #[test]
    fn hook_payloads_are_cut_to_what_rlord_uses() {
        let big = "x".repeat(50_000);
        let raw = json!({"hook_event_name": "PostToolUse", "session_id": "s-1",
            "transcript_path": "/t.jsonl", "tool_name": "Bash", "agent_id": "a-1",
            "tool_input": {"command": big}, "tool_response": {"stdout": big},
            "message": "m".repeat(1000)})
        .to_string();
        let v = slim_hook_payload(&raw);
        assert_eq!(v["hook_event_name"], "PostToolUse");
        assert_eq!(v["tool_name"], "Bash");
        assert_eq!(v["agent_id"], "a-1");
        assert!(v.get("tool_input").is_none() && v.get("tool_response").is_none());
        assert_eq!(v["message"].as_str().unwrap().len(), 300);
        assert!(v.to_string().len() < 1000);
        // Cut off mid-way (a tool input past the read limit): the event and
        // the conversation are still known.
        let cut = &raw[..raw.len() / 2];
        assert!(serde_json::from_str::<Value>(cut).is_err());
        let v = slim_hook_payload(cut);
        assert_eq!(v["hook_event_name"], "PostToolUse");
        assert_eq!(v["session_id"], "s-1");
        // (What came after the cut is lost: here the tool's name.)
        assert!(v.get("tool_name").is_none());
        // A key named inside a string value isn't the field.
        let tricky = r#"{"tool_input": {"command": "echo \"hook_event_name\": \"Stop\""}, "hook_event_name": "PreToolUse", "#;
        assert_eq!(slim_hook_payload(tricky)["hook_event_name"], "PreToolUse");
        assert_eq!(slim_hook_payload("not json"), json!({}));
    }

    #[test]
    fn a_hook_never_waits_long_for_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        // No daemon: unreachable (so the event is spooled).
        assert_eq!(
            send_hook_event(&dir.path().join("absent.sock"), &json!({})),
            HookSent::Unreachable
        );
        // A daemon that takes the event and never answers: the hook gives
        // up after its read timeout, and doesn't spool a second copy.
        let sock = dir.path().join("stalled.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let hold = std::thread::spawn(move || {
            let conn = listener.accept().map(|(c, _)| c);
            std::thread::sleep(std::time::Duration::from_secs(3));
            drop(conn);
        });
        let start = std::time::Instant::now();
        assert_eq!(
            send_hook_event(&sock, &json!({"event": "Stop"})),
            HookSent::Slow
        );
        assert!(start.elapsed() < std::time::Duration::from_millis(3400));
        hold.join().unwrap();
    }

    /// "Needs you" is a block mid-turn, cleared when the agent moves on;
    /// Claude's idle reminder leaves a finished session "your turn".
    #[test]
    fn claude_needs_you_and_your_turn_mean_one_thing_each() {
        use rlord_core::types::Status;
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&rlord_core::db::NewSession {
            id: "01cl",
            name: "c",
            agent: "claude",
            agent_session: Some("u-1"),
            project_path: "/tmp",
            tmux_session: Some("rlord_01cl_absent"),
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        db.set_status("01cl", Status::Starting, 1).unwrap();
        let emit = |event: &str, extra: Value| {
            let mut payload = json!({"session_id": "u-1", "hook_event_name": event});
            for (k, v) in extra.as_object().unwrap() {
                payload[k] = v.clone();
            }
            methods::hook_emit(
                &db,
                &json!({"instance": "01cl", "agent": "claude", "event": event, "payload": payload}),
            )
            .unwrap()
        };
        let status = || db.resolve_ref("01cl").unwrap().status;
        emit("UserPromptSubmit", json!({}));
        emit("PreToolUse", json!({"tool_name": "AskUserQuestion"}));
        assert_eq!(status(), Status::WaitingInput, "a question mid-turn");
        emit("PostToolUse", json!({"tool_name": "AskUserQuestion"}));
        assert_eq!(status(), Status::Working, "answered");
        emit("PermissionRequest", json!({"tool_name": "Bash"}));
        assert_eq!(status(), Status::WaitingInput);
        emit(
            "Notification",
            json!({"notification_type": "permission_prompt"}),
        );
        assert_eq!(status(), Status::WaitingInput);
        emit("PostToolUseFailure", json!({"tool_name": "Bash"}));
        assert_eq!(status(), Status::Working, "denied: the agent carries on");
        emit("Stop", json!({}));
        assert_eq!(status(), Status::Idle);
        // A sub-agent's tool finishing doesn't answer the main agent's
        // prompt; a prompt for a sub-agent's tool is cleared by that tool.
        emit("UserPromptSubmit", json!({}));
        emit("PermissionRequest", json!({"tool_name": "Bash"}));
        let r = emit(
            "PostToolUse",
            json!({"tool_name": "Read", "agent_id": "agent-7"}),
        );
        assert_eq!(r["unchanged"], true);
        assert_eq!(status(), Status::WaitingInput);
        emit("PostToolUse", json!({"tool_name": "Bash"}));
        assert_eq!(status(), Status::Working);
        emit(
            "PermissionRequest",
            json!({"tool_name": "Bash", "agent_id": "agent-7"}),
        );
        assert_eq!(
            emit("PostToolUse", json!({"tool_name": "Edit"}))["unchanged"],
            true
        );
        emit(
            "PostToolUse",
            json!({"tool_name": "Bash", "agent_id": "agent-7"}),
        );
        assert_eq!(status(), Status::Working);
        emit("Stop", json!({}));
        let s = db.resolve_ref("01cl").unwrap();
        // A minute later Claude says it's idle: still "your turn", nothing logged.
        let events = db.list_events("01cl", 100).unwrap().len();
        let r = emit("Notification", json!({"notification_type": "idle_prompt"}));
        assert_eq!(r["unchanged"], true);
        let after = db.resolve_ref("01cl").unwrap();
        assert_eq!(after.status, Status::Idle);
        assert_eq!(after.last_state_change_at, s.last_state_change_at);
        assert_eq!(db.list_events("01cl", 100).unwrap().len(), events);
        // A tool finishing while it's your turn changes nothing either.
        assert_eq!(emit("PostToolUse", json!({}))["unchanged"], true);
        assert_eq!(status(), Status::Idle);
    }

    /// Codex assigns its own conversation id: the first hook binds it.
    #[test]
    fn codex_hooks_bind_the_conversation_and_set_status() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&rlord_core::db::NewSession {
            id: "01codex",
            name: "c",
            agent: "codex",
            agent_session: None,
            project_path: "/tmp",
            tmux_session: Some("rlord_01codex_absent"),
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        db.set_status("01codex", rlord_core::types::Status::Starting, 1)
            .unwrap();
        let emit = |event: &str, sid: &str| {
            methods::hook_emit(
                &db,
                &json!({"instance": "01codex", "agent": "codex", "event": event,
                        "payload": {"session_id": sid, "hook_event_name": event,
                                    "transcript_path": format!("/c/rollout-x-{sid}.jsonl")}}),
            )
            .unwrap()
        };
        let get = || db.resolve_ref("01codex").unwrap();
        emit("SessionStart", "thread-1");
        let s = get();
        assert_eq!(s.agent_session.as_deref(), Some("thread-1"));
        assert_eq!(s.agent_session_trust, "hook");
        assert_eq!(s.status, rlord_core::types::Status::Idle);
        assert_eq!(
            db.transcript_source("01codex").unwrap().as_deref(),
            Some("/c/rollout-x-thread-1.jsonl")
        );
        emit("UserPromptSubmit", "thread-1");
        assert_eq!(get().status, rlord_core::types::Status::Working);
        emit("PermissionRequest", "thread-1");
        assert_eq!(get().status, rlord_core::types::Status::WaitingInput);
        // The user approved and the tool ran: working again, and neither
        // "updated" time moved.
        let before = get();
        let r = emit("PostToolUse", "thread-1");
        assert!(r["unchanged"].is_null());
        let s = get();
        assert_eq!(s.status, rlord_core::types::Status::Working);
        assert_eq!(s.last_user_msg_at, before.last_user_msg_at);
        assert_eq!(s.last_agent_msg_at, before.last_agent_msg_at);
        // Every other tool call changes nothing and logs nothing.
        let events = db.list_events("01codex", 100).unwrap().len();
        assert_eq!(emit("PostToolUse", "thread-1")["unchanged"], true);
        assert_eq!(db.list_events("01codex", 100).unwrap().len(), events);
        emit("PermissionRequest", "thread-1");
        // A sub-agent's thread in the same pane: not this session's events.
        let r = emit("Stop", "thread-sub");
        assert!(r["ignored"].is_string());
        let s = get();
        assert_eq!(s.agent_session.as_deref(), Some("thread-1"));
        assert_eq!(s.status, rlord_core::types::Status::WaitingInput);
        emit("Stop", "thread-1");
        assert_eq!(get().status, rlord_core::types::Status::Idle);
        // A new thread in the pane (/new): the session follows it.
        emit("SessionStart", "thread-2");
        assert_eq!(get().agent_session.as_deref(), Some("thread-2"));
        assert_eq!(db.past_agent_sessions("01codex").unwrap(), ["thread-1"]);
        // A sub-agent's thread is blocked on a prompt: the session needs
        // you, and only that thread moving on clears it.
        emit("UserPromptSubmit", "thread-2");
        emit("PermissionRequest", "thread-sub");
        let s = get();
        assert_eq!(s.status, rlord_core::types::Status::WaitingInput);
        assert_eq!(s.agent_session.as_deref(), Some("thread-2"), "not rebound");
        assert_eq!(emit("PostToolUse", "thread-2")["unchanged"], true);
        assert_eq!(get().status, rlord_core::types::Status::WaitingInput);
        emit("PostToolUse", "thread-sub");
        assert_eq!(get().status, rlord_core::types::Status::Working);
        emit("Stop", "thread-2");
        // A fresh conversation: until its SessionStart, late events from
        // the process just replaced neither bind its thread nor stop it.
        db.clear_agent_session("01codex").unwrap();
        db.set_status("01codex", rlord_core::types::Status::Starting, 2)
            .unwrap();
        assert!(emit("SessionEnd", "thread-2")["ignored"].is_string());
        let s = get();
        assert_eq!(s.agent_session, None);
        assert_eq!(s.status, rlord_core::types::Status::Starting);
        // Bound from Codex's index (rlord's guess), and the hooks then name
        // another thread: theirs is the one.
        db.set_agent_session("01codex", "thread-guess", "minted")
            .unwrap();
        emit("UserPromptSubmit", "thread-3");
        let s = get();
        assert_eq!(s.agent_session.as_deref(), Some("thread-3"));
        assert_eq!(s.agent_session_trust, "hook");
        assert_eq!(s.status, rlord_core::types::Status::Working);
        emit("Stop", "thread-3");
        // Claude's hooks from a Codex session's pane are only logged.
        methods::hook_emit(
            &db,
            &json!({"instance": "01codex", "agent": "claude", "event": "SessionEnd",
                    "payload": {"session_id": "other"}}),
        )
        .unwrap();
        let s = get();
        assert_eq!(s.agent_session.as_deref(), Some("thread-3"));
        assert_eq!(s.status, rlord_core::types::Status::Idle);
    }
}
