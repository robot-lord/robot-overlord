pub mod ask;
pub mod autostart;
pub mod discover;
pub mod import;
pub mod locks;
pub mod mcp;
pub mod methods;
pub mod opencode;
pub mod overlord;
pub mod profile;
pub mod recall;
pub mod remote;
pub mod rpc;
pub mod search;
pub mod send;
pub mod summary;
pub mod triage;
pub mod ui;
pub mod usage;
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
    /// Held while text is pasted into the overlord, and while an idle one
    /// is restarted fresh: the two must not overlap. (Taken before
    /// `sessions` and the DB.)
    pub(crate) overlord_send: Mutex<()>,
    /// One session at a time: typing into it vs restarting, stopping or
    /// archiving it (locks.rs, which has the lock order).
    pub(crate) sessions: locks::SessionLocks,
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
            overlord_send: Mutex::new(()),
            sessions: Default::default(),
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

    /// Hold the overlord's session (`sessions`), if there is one: around
    /// a start or restart of it. (Before the DB: see locks.rs.)
    pub(crate) fn lock_overlord(&self) -> Option<locks::SessionGuard<'_>> {
        let id = overlord::session(&self.db()).ok().flatten()?.id;
        Some(self.sessions.lock(&id))
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
        // An overlord from when it was two sessions: now one.
        if let Err(e) = overlord::migrate(&db) {
            tracing::warn!(error = %e, "making the overlord one session");
        }
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
        // Where each agent's command is may mean asking the user's login
        // shell (seconds): done here, before any lock is taken, so the
        // methods below only read the answer.
        if matches!(
            method,
            "session.new"
                | "session.restart"
                // (Enter on a stopped session resumes it.)
                | "ui.goto"
                | "session.defaults"
                | "overlord.ensure"
                | "overlord.set_model"
                | "overlord.new_session"
        ) {
            rlord_core::agents::install::warm();
        }
        // The user is about to choose a model, or is looking at their
        // agents: lists that must be asked for (OpenCode's) are, here
        // only. In the background (it can take seconds): this answer has
        // the list as last listed, the next one the fresh list.
        if matches!(method, "session.defaults" | "agents.list") && params["no_listing"] != true {
            refresh_models();
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
        // Restarting, stopping or archiving a session: not while text is
        // being typed into it (locks.rs: this lock before the DB's).
        let session_id = matches!(
            method,
            "session.restart" | "session.stop" | "session.archive"
        )
        .then(|| {
            let r = params["ref"].as_str().unwrap_or("");
            self.db().resolve_ref(r).ok().map(|s| s.id)
        })
        .flatten();
        let _session = session_id.as_deref().map(|id| self.sessions.lock(id));
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
                usage::list(self, &params, &mut rows);
                Ok(Value::Array(rows))
            }
            ("session.get", Ok(row)) => {
                let mut rows = [row];
                remote::annotate(self, &mut rows);
                usage::annotate(self, &mut rows, true);
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
                let _session = self.lock_overlord();
                let r = overlord::ensure(&self.db(), &self.config(), params);
                // (ensure syncs its tmux keys itself.)
                if r.is_ok() {
                    self.changed();
                    self.bind_nav_keys();
                }
                r
            }
            // The overlord's session (null: not set up), with when its
            // conversation started and how many came before.
            "overlord.get" => {
                // One guard for both reads (std's Mutex isn't reentrant).
                let db = self.db();
                overlord::session(&db).and_then(|o| {
                    let Some(o) = o else {
                        return Ok(Value::Null);
                    };
                    let mut v = serde_json::to_value(&o)?;
                    v["conversations"] = recall::summary(&db, &o);
                    // What it runs on now (config may since have changed).
                    v["effort"] = json!(overlord::launched_effort(&db, &o));
                    Ok(v)
                })
            }
            "overlord.conversations" => recall::conversations_rpc(self),
            "overlord.conversation" => recall::conversation(self, params),
            "overlord.recall" => recall::recall(self, params),
            "overlord.continue" => {
                let r = recall::continue_conversation(self, params);
                if r.is_ok() {
                    self.changed();
                }
                r
            }
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
            // Don't offer the overlord's default model again (the user said no).
            "overlord.decline_default" => {
                let m = params["model"].as_str().unwrap_or("");
                self.db()
                    .kv_set(overlord::DEFAULT_DECLINED, m)
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
            "session.send" => send::send(self, params),
            "overlord.triage" => triage::triage(self, params),
            "overlord.new_session" => ask::new_session(self, params),
            "overlord.set_model" => {
                let _session = self.lock_overlord();
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

/// List OpenCode's models (`install::list_models`) on a thread of its
/// own, one at a time: a request never waits on it.
fn refresh_models() {
    static LISTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if LISTING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let _ = std::panic::catch_unwind(rlord_core::agents::install::list_models);
        LISTING.store(false, Ordering::SeqCst);
    });
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
/// input) still yields the event's name, marked `truncated`.
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
            // Cut off: only the event's name is taken (its first
            // `"hook_event_name":` followed by a JSON string; inside a
            // string value the quotes are escaped, so this is the key).
            // Nothing else: a later key of the same name may be nested in
            // a tool's input, and who the event is about is then unknown
            // (`truncated`), which the daemon treats as such.
            let key = "\"hook_event_name\":";
            if let Some(at) = raw.find(key) {
                let rest = raw[at + key.len()..].trim_start();
                let name = serde_json::Deserializer::from_str(rest)
                    .into_iter::<String>()
                    .next()
                    .and_then(|r| r.ok());
                if let Some(name) = name {
                    out.insert("hook_event_name".to_string(), json!(name));
                    out.insert("truncated".to_string(), json!(true));
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
    /// The daemon isn't there, or went away before answering (a restart):
    /// keep the event for when it is back.
    Unreachable,
    /// It has the whole event and hasn't answered in time: it will handle
    /// it; keeping a copy too would deliver it twice.
    Slow,
}

/// Send one hook event, never waiting long: a hook runs inside the agent's
/// tool call, and a stalled daemon must not hold that up. A second to
/// connect, a second to send, two for the answer.
///
/// Only a timeout while waiting for the answer is `Slow`. Anything else
/// short of an answer (no socket, refused, not connected in time, a send
/// that didn't complete, the connection closed or reset) is `Unreachable`:
/// the daemon did not take a whole event and answer, so it is kept.
pub fn send_hook_event(socket_path: &Path, params: &Value) -> HookSent {
    use std::io::{BufRead, BufReader as StdBufReader, ErrorKind, Write};
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;
    // Who decided what happens to the event: the sender (connected in
    // time, it goes ahead) or the caller (gave up on connecting: the sender
    // must not deliver it later, the caller keeps it). Exactly one wins.
    const CONNECTING: u8 = 0;
    const SENDING: u8 = 1;
    const GAVE_UP: u8 = 2;
    let state = Arc::new(AtomicU8::new(CONNECTING));
    let (tx, rx) = mpsc::channel::<HookSent>();
    let path = socket_path.to_path_buf();
    let req = json!({"id": 1, "method": "hook.emit", "params": params});
    let sender_state = Arc::clone(&state);
    // On its own thread: connecting to a socket nobody accepts on can block.
    std::thread::spawn(move || {
        let state = sender_state;
        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&path) else {
            let _ = tx.send(HookSent::Unreachable);
            return;
        };
        if state
            .compare_exchange(CONNECTING, SENDING, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return; // the caller gave up and kept the event
        }
        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buf = serde_json::to_vec(&req).unwrap_or_default();
        buf.push(b'\n');
        // (A send that didn't complete left the daemon half a line: it
        // can't act on that.)
        if stream.write_all(&buf).is_err() {
            let _ = tx.send(HookSent::Unreachable);
            return;
        }
        let mut line = String::new();
        let _ = tx.send(match StdBufReader::new(&mut stream).read_line(&mut line) {
            Ok(n) if n > 0 => HookSent::Delivered,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                HookSent::Slow
            }
            // Closed or reset before an answer: the daemon went away.
            _ => HookSent::Unreachable,
        });
    });
    // Connected within a second, or the event is kept (and the sender,
    // should it connect later, drops it).
    let connect = Duration::from_secs(1);
    let start = std::time::Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(sent) => return sent,
            Err(mpsc::RecvTimeoutError::Disconnected) => return HookSent::Unreachable,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if state.load(Ordering::SeqCst) == SENDING {
            break;
        }
        if start.elapsed() >= connect
            && state
                .compare_exchange(CONNECTING, GAVE_UP, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return HookSent::Unreachable;
        }
    }
    // Sending: the sender's own timeouts (1s + 2s) end it; a little more
    // than that is waited, so its verdict is the one acted on.
    rx.recv_timeout(Duration::from_millis(3500))
        .unwrap_or(HookSent::Slow)
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
    async fn the_list_carries_usage_and_sorts_by_it() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let daemon = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        // a: 3 prompts, little output; b: 1 prompt, a lot; c: no transcript.
        let transcript = |name: &str, prompts: usize, output: u64| {
            let p = dir.path().join(format!("{name}.jsonl"));
            let mut text = String::new();
            for i in 0..prompts {
                text += &format!(
                    "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"p{i}\"}}}}\n\
                     {{\"type\":\"assistant\",\"message\":{{\"id\":\"{name}{i}\",\
                     \"model\":\"claude-opus-5-5\",\"content\":[],\"usage\":{{\"input_tokens\":1,\
                     \"cache_read_input_tokens\":1000,\"output_tokens\":{output}}}}}}}\n"
                );
            }
            std::fs::write(&p, text).unwrap();
            p
        };
        {
            let db = daemon.db.lock().unwrap();
            for (id, t) in [
                ("01a", Some(transcript("a", 3, 10))),
                ("01b", Some(transcript("b", 1, 50_000))),
                ("01c", None),
            ] {
                db.insert_session(&rlord_core::db::NewSession {
                    id,
                    name: id,
                    agent: "claude",
                    agent_session: Some(id),
                    project_path: "/tmp",
                    tmux_session: None,
                    launch_spec: "{}",
                    created_at: 1,
                    repo_path: None,
                    workspace_path: None,
                    branch: None,
                    role: "",
                })
                .unwrap();
                if let Some(t) = t {
                    db.upsert_transcript_source(id, &t.to_string_lossy())
                        .unwrap();
                }
            }
        }
        let listener = Daemon::bind(&sock).unwrap();
        let server = tokio::spawn(daemon.clone().serve(listener));
        let list = |params: Value| {
            let sock = sock.clone();
            tokio::task::spawn_blocking(move || client_call(&sock, "session.list", params).unwrap())
        };
        let ids = |v: &Value| -> Vec<String> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect()
        };
        // Unread transcripts: the warm-up reads them, not the list (here
        // there's no worker yet: nothing is read at all).
        let v = list(json!({"sort": "usage"})).await.unwrap();
        assert!(
            v.as_array().unwrap().iter().all(|r| r["usage"].is_null()),
            "{v}"
        );
        crate::warm::start(&daemon);
        let mut v = Value::Null;
        for _ in 0..200 {
            v = list(json!({"sort": "usage"})).await.unwrap();
            if v[0]["usage"].is_object() && v[1]["usage"].is_object() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(ids(&v), ["01a", "01b", "01c"], "most turns first: {v}");
        assert_eq!(v[0]["usage"]["turns"], 3);
        assert_eq!(v[0]["usage"]["context_pct"], 1);
        assert!(v[2]["usage"].is_null(), "no transcript: unknown");
        let v = list(json!({"sort": "usage", "usage": "tokens"}))
            .await
            .unwrap();
        assert_eq!(ids(&v), ["01b", "01a", "01c"], "most tokens first: {v}");
        assert_eq!(v[0]["usage"]["tokens"], 50_001);
        let v = list(json!({"sort": "usage", "usage": "tokens", "reverse": true}))
            .await
            .unwrap();
        assert_eq!(ids(&v), ["01a", "01b", "01c"], "unknown stays last: {v}");
        server.abort();
    }

    /// Tests that plant made-up saved read positions (to tell a forward
    /// read from a full re-read) and tests that fill the shared in-memory
    /// positions past their cap must not run at once.
    static CURSOR_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn add_session(db: &Db, id: &str, transcript: &std::path::Path) {
        db.insert_session(&rlord_core::db::NewSession {
            id,
            name: id,
            agent: "claude",
            agent_session: Some(id),
            project_path: "/tmp",
            tmux_session: None,
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        db.upsert_transcript_source(id, &transcript.to_string_lossy())
            .unwrap();
    }

    /// A transcript of `prompts` prompts, and a saved read position at its
    /// end that claims `turns` (so a forward read gives `turns`, a full
    /// re-read `prompts`).
    fn planted(
        dir: &std::path::Path,
        name: &str,
        prompts: usize,
        turns: u64,
    ) -> (
        std::path::PathBuf,
        rlord_core::agents::transcript::CursorState,
    ) {
        use std::os::unix::fs::MetadataExt;
        let p = dir.join(format!("{name}.jsonl"));
        let text: String = (0..prompts)
            .map(|i| {
                format!(
                    "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"p{i}\"}}}}\n"
                )
            })
            .collect();
        std::fs::write(&p, text).unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        let c = rlord_core::agents::transcript::CursorState {
            ino: meta.ino(),
            offset: meta.len(),
            turns,
            ..Default::default()
        };
        (p, c)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_subagents_never_reads_a_whole_main_transcript() {
        use rlord_core::agents::transcript;
        let _lock = CURSOR_TESTS.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let daemon = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        // a: never read; b: read before, its position saved.
        let (a, _) = planted(dir.path(), "a", 2, 0);
        let (b, saved) = planted(dir.path(), "b", 3, 7);
        {
            let db = daemon.db.lock().unwrap();
            add_session(&db, "01a", &a);
            add_session(&db, "01b", &b);
            db.save_transcript_cursors(&[(b.clone(), saved)]).unwrap();
        }
        let listener = Daemon::bind(&sock).unwrap();
        let server = tokio::spawn(daemon.clone().serve(listener));
        for id in ["01a", "01b"] {
            let sock = sock.clone();
            tokio::task::spawn_blocking(move || {
                client_call(&sock, "session.subagents", json!({"ref": id})).unwrap()
            })
            .await
            .unwrap();
        }
        assert_eq!(transcript::unread_bytes(&a), None, "read on the request");
        // b went forward from its saved position, not from the start.
        assert_eq!(transcript::transcript_stats(&b).unwrap().turns, 7);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_list_past_the_position_cap_reads_nothing_again() {
        use rlord_core::agents::transcript::STATS_CURSOR_SEED;
        let _lock = CURSOR_TESTS.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let daemon = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let n = STATS_CURSOR_SEED + 50;
        {
            let db = daemon.db.lock().unwrap();
            let mut saved = Vec::new();
            for i in 0..n {
                let (p, c) = planted(dir.path(), &format!("t{i}"), 1, 7);
                add_session(&db, &format!("01s{i:05}"), &p);
                saved.push((p, c));
            }
            db.save_transcript_cursors(&saved).unwrap();
        }
        let listener = Daemon::bind(&sock).unwrap();
        let server = tokio::spawn(daemon.clone().serve(listener));
        let sock2 = sock.clone();
        let v = tokio::task::spawn_blocking(move || {
            client_call(&sock2, "session.list", json!({})).unwrap()
        })
        .await
        .unwrap();
        let rows = v.as_array().unwrap();
        assert_eq!(rows.len(), n);
        // Every row from its saved position (7), none re-read (1) or left
        // unknown (evicted before its turn).
        let off: Vec<&Value> = rows.iter().filter(|r| r["usage"]["turns"] != 7).collect();
        assert!(off.is_empty(), "{} rows off, e.g. {}", off.len(), off[0]);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requests_never_read_a_whole_unread_transcript() {
        use rlord_core::agents::transcript::{self, CursorState};
        let _lock = CURSOR_TESTS.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let daemon = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let line = |i: usize| {
            format!(
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"p{i}\"}}}}\n"
            )
        };
        let write = |name: &str, prompts: usize| {
            let p = dir.path().join(format!("{name}.jsonl"));
            std::fs::write(&p, (0..prompts).map(line).collect::<String>()).unwrap();
            p
        };
        // a: never read. b: read before (a saved position, no longer in
        // memory) up to its end; its saved totals say 7 turns.
        let (a, b) = (write("a", 2), write("b", 3));
        {
            use std::os::unix::fs::MetadataExt;
            let db = daemon.db.lock().unwrap();
            for (id, t) in [("01a", &a), ("01b", &b)] {
                db.insert_session(&rlord_core::db::NewSession {
                    id,
                    name: id,
                    agent: "claude",
                    agent_session: Some(id),
                    project_path: "/tmp",
                    tmux_session: None,
                    launch_spec: "{}",
                    created_at: 1,
                    repo_path: None,
                    workspace_path: None,
                    branch: None,
                    role: "",
                })
                .unwrap();
                db.upsert_transcript_source(id, &t.to_string_lossy())
                    .unwrap();
            }
            let meta = std::fs::metadata(&b).unwrap();
            let saved = CursorState {
                ino: meta.ino(),
                offset: meta.len(),
                turns: 7,
                ..Default::default()
            };
            db.save_transcript_cursors(&[(b.clone(), saved)]).unwrap();
        }
        let listener = Daemon::bind(&sock).unwrap();
        let server = tokio::spawn(daemon.clone().serve(listener));
        let call = |method: &'static str, params: Value| {
            let sock = sock.clone();
            tokio::task::spawn_blocking(move || client_call(&sock, method, params).unwrap())
        };
        let v = call("session.list", json!({})).await.unwrap();
        let row = |id: &str| {
            v.as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == id)
                .unwrap()
                .clone()
        };
        // Never read: not read by the list (no worker here to queue it on).
        assert!(row("01a")["usage"].is_null(), "{v}");
        assert_eq!(transcript::unread_bytes(&a), None, "the list read it");
        // Read before: its saved totals, without reading it again.
        assert_eq!(row("01b")["usage"]["turns"], 7, "{v}");
        // One session's details may read a small unread transcript.
        let one = call("session.get", json!({"ref": "01a"})).await.unwrap();
        assert_eq!(one["usage"]["turns"], 2, "{one}");
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
        // ...only the event is known: who it is about is not taken from
        // text that may be part of a tool's input.
        let v = slim_hook_payload(cut);
        assert_eq!(
            v,
            json!({"hook_event_name": "PostToolUse", "truncated": true})
        );
        let nested = r#"{"hook_event_name": "PostToolUse", "tool_input": {"agent_id": "evil", "session_id": "other", "x": ""#;
        assert_eq!(
            slim_hook_payload(nested),
            json!({"hook_event_name": "PostToolUse", "truncated": true})
        );
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
        // A daemon that goes away before answering (a restart): the event
        // is kept, not lost.
        let sock = dir.path().join("restarting.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let close = std::thread::spawn(move || drop(listener.accept()));
        assert_eq!(
            send_hook_event(&sock, &json!({"event": "Stop"})),
            HookSent::Unreachable
        );
        close.join().unwrap();
        // And one that answers.
        let sock = dir.path().join("up.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let answer = std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let (mut c, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(&c).read_line(&mut line).unwrap();
            c.write_all(b"{\"id\":1,\"result\":{}}\n").unwrap();
        });
        assert_eq!(
            send_hook_event(&sock, &json!({"event": "Stop"})),
            HookSent::Delivered
        );
        answer.join().unwrap();
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
        // The sub-agent is blocked, then stops without running the tool
        // (denied, or cut off): nobody is blocked any more.
        emit(
            "PermissionRequest",
            json!({"tool_name": "Bash", "agent_id": "agent-7"}),
        );
        emit(
            "SubagentStop",
            json!({"agent_id": "agent-7", "agent_type": "Explore"}),
        );
        assert_eq!(status(), Status::Working);
        // Both blocked: needs you until the main prompt is answered too.
        emit("PermissionRequest", json!({"tool_name": "Bash"}));
        emit(
            "PermissionRequest",
            json!({"tool_name": "Edit", "agent_id": "agent-8"}),
        );
        emit(
            "PostToolUse",
            json!({"tool_name": "Edit", "agent_id": "agent-8"}),
        );
        assert_eq!(status(), Status::WaitingInput);
        // A permission notification names nobody (it has no agent id, and
        // comes before or after the request): it doesn't claim the wait
        // for the main agent, so the sub-agent's tool still clears it.
        emit("PostToolUse", json!({"tool_name": "Bash"}));
        emit(
            "PermissionRequest",
            json!({"tool_name": "Edit", "agent_id": "agent-9"}),
        );
        emit(
            "Notification",
            json!({"notification_type": "permission_prompt"}),
        );
        emit(
            "PostToolUse",
            json!({"tool_name": "Edit", "agent_id": "agent-9"}),
        );
        assert_eq!(status(), Status::Working);
        emit("PermissionRequest", json!({"tool_name": "Bash"}));
        // A cut-off payload names nobody: it can't clear the wait.
        emit("PostToolUse", json!({"truncated": true}));
        assert_eq!(status(), Status::WaitingInput);
        emit("PostToolUse", json!({"tool_name": "Bash"}));
        assert_eq!(status(), Status::Working);
        emit("Stop", json!({}));
        assert_eq!(db.kv_get("needs_you_by:01cl").unwrap(), None);
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

    /// A user's `/clear` in the overlord: recorded as a fresh conversation
    /// with its reason, like an idle restart, so the list of its earlier
    /// conversations is complete. A resume is an ordinary switch.
    #[test]
    fn a_clear_is_a_fresh_conversation() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&rlord_core::db::NewSession {
            id: "01over",
            name: "overlord",
            agent: "claude",
            agent_session: Some("conv-1"),
            project_path: "/tmp/o",
            tmux_session: Some("rlord_01over_absent"),
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "overlord",
        })
        .unwrap();
        db.set_status("01over", rlord_core::types::Status::Idle, 1)
            .unwrap();
        let emit = |sid: &str, source: &str| {
            methods::hook_emit_with(
                &db,
                &json!({"instance": "01over", "agent": "claude", "event": "SessionStart",
                        "payload": {"session_id": sid, "hook_event_name": "SessionStart",
                                    "source": source}}),
                &|_, _| rlord_core::agents::Indexed::NoIndex,
            )
            .unwrap()
        };
        emit("conv-2", "clear");
        emit("conv-3", "resume");
        let o = db.overlord().unwrap().unwrap();
        assert_eq!(o.agent_session.as_deref(), Some("conv-3"));
        let changes = db.conversation_changes("01over").unwrap();
        let got: Vec<(&str, &str, Option<&str>)> = changes
            .iter()
            .map(|(_, k, p)| (k.as_str(), p["to"].as_str().unwrap(), p["reason"].as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("fresh_conversation", "conv-2", Some("clear")),
                ("agent_session_changed", "conv-3", None),
            ]
        );
        // The overlord's "fresh" clock restarts with a clear.
        assert!(db.kv_get(overlord::FRESH_AT).unwrap().is_some());
        let (cur, earlier) = recall::list(&db, &o).unwrap();
        assert_eq!(cur.unwrap().id, "conv-3");
        assert_eq!(recall::why(&earlier[1]), "you cleared it with /clear");
        assert_eq!(recall::summary(&db, &o)["earlier"], 2);
        assert_eq!(db.past_agent_sessions("01over").unwrap().len(), 2);
        // The list's row for it: the session, with that summary (one db
        // guard for both reads: a second lock would never return).
        let d = Daemon::new(db, Config::default());
        let o = d.rpc("overlord.get", json!({})).unwrap();
        assert_eq!(o["role"], "overlord");
        assert_eq!(o["conversations"]["earlier"], 2);
        assert_eq!(o["conversations"]["reason"], "switched");
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
        // Codex's index, as the test has it: `thread-sub…` are sub-agents'
        // threads, `thread-new…` aren't indexed yet, the rest are the user's.
        use rlord_core::agents::Indexed;
        let index = |_: rlord_core::agents::Agent, id: &str| {
            if id.starts_with("thread-sub") {
                Indexed::SubAgent
            } else if id.starts_with("thread-new") {
                Indexed::NotListed
            } else {
                Indexed::User
            }
        };
        let emit = |event: &str, sid: &str| {
            methods::hook_emit_with(
                &db,
                &json!({"instance": "01codex", "agent": "codex", "event": event,
                        "payload": {"session_id": sid, "hook_event_name": event,
                                    "transcript_path": format!("/c/rollout-x-{sid}.jsonl")}}),
                &index,
            )
            .unwrap()
        };
        let get = || db.resolve_ref("01codex").unwrap();
        db.kv_set("bind_since:01codex", "1").unwrap();
        emit("SessionStart", "thread-1");
        assert_eq!(db.kv_get("bind_since:01codex").unwrap(), None);
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
        assert_eq!(r["unchanged"], true);
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
        assert_eq!(
            db.kv_get("needs_you_by:01codex").unwrap(),
            None,
            "kept only while waiting"
        );
        // Two are blocked: it needs you until both have moved on. A
        // sub-agent that ends without running its tool counts as moved on.
        emit("PermissionRequest", "thread-2");
        emit("PermissionRequest", "thread-sub");
        emit("PostToolUse", "thread-2");
        assert_eq!(get().status, rlord_core::types::Status::WaitingInput);
        assert!(emit("Stop", "thread-sub")["ignored"].is_null());
        let s = get();
        assert_eq!(s.status, rlord_core::types::Status::Working);
        assert_eq!(s.agent_session.as_deref(), Some("thread-2"));
        // A thread the index doesn't know yet: the session waits for one
        // it does, rather than moving to what may be a sub-agent's.
        assert_eq!(emit("SessionStart", "thread-new")["unchanged"], true);
        assert_eq!(get().agent_session.as_deref(), Some("thread-2"));
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
        // A thread that asked for permission before the index listed it
        // (so as "another conversation"), then became the session's: its
        // own tool running clears the wait.
        emit("PermissionRequest", "thread-new-4");
        assert_eq!(get().status, rlord_core::types::Status::WaitingInput);
        db.set_agent_session("01codex", "thread-new-4", "hook")
            .unwrap();
        assert_eq!(emit("PostToolUse", "thread-new-4")["unchanged"], true);
        db.set_agent_session("01codex", "thread-3", "hook").unwrap();
        db.kv_set("needs_you_by:01codex", r#"["thread-5"]"#)
            .unwrap();
        emit("PostToolUse", "thread-5"); // listed now: adopted, and it moved on
        let s = get();
        assert_eq!(s.agent_session.as_deref(), Some("thread-5"));
        assert_eq!(s.status, rlord_core::types::Status::Working);
        emit("Stop", "thread-5");
        db.set_agent_session("01codex", "thread-3", "hook").unwrap();
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
