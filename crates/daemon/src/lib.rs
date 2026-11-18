pub mod ask;
pub mod autostart;
pub mod background;
pub mod codex_daemon;
pub mod context;
pub mod discover;
pub mod fork;
pub mod import;
pub mod limits;
pub mod locks;
pub mod mcp;
pub mod methods;
pub mod opencode;
pub mod overlord;
pub mod own;
pub mod profile;
pub mod recall;
pub mod relay;
pub mod remote;
pub mod repo_latest;
mod repo_scan;
pub mod rpc;
pub mod search;
pub mod send;
pub mod setup;
pub mod summary;
pub mod triage;
pub mod ui;
pub mod usage;
pub mod warm;
pub mod web;

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
    /// The repo finder's background disk scan (`repo_scan.rs`).
    repo_scan: repo_scan::Scanner,
    /// This daemon, for work that outlives a call (a late fetch result).
    pub(crate) me: std::sync::Weak<Daemon>,
    /// `repo_latest`: fetches, per-repo locks (`repo_latest.rs`).
    pub(crate) latest: repo_latest::Latest,
    /// Push stream to subscribers, one JSON line per message:
    /// `{"notify":"changed"}` — coarse tick, refetch what you display
    /// (`{"notify":"changed","relay":true}` when only the plugin's relay
    /// status moved);
    /// `{"notify":"ui","to":"terminal","cmd":{...}}` — a UI command for
    /// the TUI (set_view, new_session) issued by the overlord;
    /// `{"notify":"ui","to":"web","page":"<id>","cmd":{...}}` — one for a
    /// web page (`web.subscribe` passes these on; the TUI ignores them);
    /// `{"notify":"remote","id","req"}` — a user's relay command for the
    /// plugin (`relay.rs`; only `web.subscribe` gets these).
    notify: tokio::sync::broadcast::Sender<String>,
    /// The TUI's current list view, as it last reported it.
    pub(crate) ui_view: Mutex<Value>,
    /// Last UI command for the terminal sent while no TUI was subscribed,
    /// replayed to the next TUI that starts (if fresh; `ui.pending`).
    pending_ui: Mutex<Option<(std::time::Instant, Value)>>,
    /// Last UI command for a web page, and that page's id: handed to the
    /// next page that asks (`page.pending`) when it never reached its own
    /// (that page closed, or the plugin wasn't connected).
    pub(crate) pending_web: Mutex<Option<(std::time::Instant, String, Value)>>,
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
    /// Each Codex thread's fork flag, read once (`discover`).
    pub(crate) fork_flags: Mutex<discover::ForkFlags>,
    /// Background loading of sessions' sub-agent data (see warm.rs).
    pub(crate) warm: warm::Warm,
    /// Remote Control: availability and each session's link (remote.rs).
    pub(crate) remote: remote::Remote,
    /// Remote access through a relay: the plugin's state, and the user's
    /// commands waiting for its answer (relay.rs).
    pub(crate) relay: relay::Relay,
    /// Archive summaries being written (summary.rs).
    pub(crate) summaries: summary::Summaries,
    /// Sessions stopped on a usage limit (limits.rs).
    pub(crate) limits: limits::Limits,
    /// Background work still running at a turn's end (background.rs).
    pub(crate) background: background::Background,
    /// The last `overlord::roster`, keyed by the rows it was built from
    /// (it runs tmux and the install checks; the TUI asks on every
    /// refresh).
    roster: Mutex<Option<(std::time::Instant, String, Value)>>,
    /// Random per daemon start: the first half of `rev`, so a revision
    /// never repeats across a restart.
    boot_id: String,
    /// Bumped by every `changed()`: the second half of `rev`.
    changes: std::sync::atomic::AtomicU64,
    /// The web plugin, when it runs (web.rs).
    web: Arc<web::Web>,
    /// `propose_session`'s proposals: id and the overlord's background
    /// (ui.rs `keep_proposal`).
    pub(crate) proposals: Mutex<std::collections::VecDeque<(String, Value)>>,
    /// The open web pages and what each shows (web.rs `WebUi`).
    web_ui: Mutex<web::WebUi>,
    /// Open `web.subscribe` streams (the plugin's; a restarted plugin's
    /// may overlap the old one's briefly). The pages go with the last.
    web_subscribers: AtomicUsize,
}

impl Daemon {
    pub fn new(db: Db, config: Config) -> Arc<Self> {
        let (notify, _) = tokio::sync::broadcast::channel(64);
        Arc::new_cyclic(|me| Daemon {
            me: me.clone(),
            db: Mutex::new(db),
            config: std::sync::RwLock::new(Arc::new(config)),
            repo_scan: repo_scan::Scanner::default(),
            latest: Default::default(),
            notify,
            ui_view: Mutex::new(Value::Null),
            pending_ui: Mutex::new(None),
            pending_web: Mutex::new(None),
            tui_subscribers: AtomicUsize::new(0),
            overlay: Mutex::new(None),
            overlay_seq: std::sync::atomic::AtomicU64::new(1),
            overlord_send: Mutex::new(()),
            sessions: Default::default(),
            warm: Default::default(),
            remote: Default::default(),
            relay: Default::default(),
            summaries: Default::default(),
            limits: Default::default(),
            background: Default::default(),
            roster: Mutex::new(None),
            fork_flags: Default::default(),
            boot_id: rlord_core::plugin::random_hex(6)
                .unwrap_or_else(|_| format!("{:x}", std::process::id())),
            changes: std::sync::atomic::AtomicU64::new(0),
            web: Default::default(),
            web_ui: Default::default(),
            proposals: Default::default(),
            web_subscribers: AtomicUsize::new(0),
        })
    }

    pub(crate) fn db(&self) -> MutexGuard<'_, Db> {
        self.db.lock().expect("db mutex poisoned")
    }

    /// `overlord::roster`, reused while its rows are unchanged and it is
    /// fresher than `ROSTER_TTL` (a tmux session dying, an agent being
    /// installed, shows up within that). Not under the DB guard.
    pub(crate) fn roster(&self, rows: (Vec<rlord_core::types::Session>, Option<String>)) -> Value {
        const ROSTER_TTL: std::time::Duration = std::time::Duration::from_secs(5);
        let key = serde_json::to_string(&json!({
            "active": rows.1,
            "rows": rows.0.iter().map(|s| json!([s.id, s.agent, s.status, s.tmux_session]))
                .collect::<Vec<_>>(),
        }))
        .unwrap_or_default();
        if let Some((at, k, v)) = self.roster.lock().expect("roster mutex").as_ref() {
            if *k == key && at.elapsed() < ROSTER_TTL {
                return v.clone();
            }
        }
        let v = overlord::roster(rows);
        *self.roster.lock().expect("roster mutex") =
            Some((std::time::Instant::now(), key, v.clone()));
        v
    }

    pub(crate) fn changed(&self) {
        self.changes.fetch_add(1, Ordering::SeqCst);
        let _ = self.notify.send(CHANGED.to_string());
    }

    /// Only the plugin's relay status moved (`relay.rs`): the local page
    /// and the TUI show it, a device doesn't, so `web.subscribe` marks it
    /// and the plugin wakes no device for it.
    pub(crate) fn changed_relay(&self) {
        self.changes.fetch_add(1, Ordering::SeqCst);
        let _ = self.notify.send(CHANGED_RELAY.to_string());
    }

    /// The web plugin's supervisor.
    pub fn web(&self) -> &Arc<web::Web> {
        &self.web
    }

    /// A line for the subscribers' streams, as is.
    pub(crate) fn notify_line(&self, line: String) {
        let _ = self.notify.send(line);
    }

    /// How many `web.subscribe` streams are open (the plugin's).
    pub(crate) fn web_subscribers(&self) -> usize {
        self.web_subscribers.load(Ordering::SeqCst)
    }

    /// The open web pages.
    pub(crate) fn web_ui(&self) -> MutexGuard<'_, web::WebUi> {
        self.web_ui.lock().expect("web ui mutex")
    }

    /// The state's revision, `"<boot id>-<n>"`: moves on every change and
    /// never repeats across a restart (the web UI's "anything new?").
    pub fn rev(&self) -> String {
        format!("{}-{}", self.boot_id, self.changes.load(Ordering::SeqCst))
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

    /// Swap the config for one changed by `f` (a copy: readers holding the
    /// old one keep it).
    pub(crate) fn update_config<T>(&self, f: impl FnOnce(&mut Config) -> T) -> T {
        let mut w = self.config.write().expect("config lock");
        let mut cfg = (**w).clone();
        let r = f(&mut cfg);
        *w = Arc::new(cfg);
        r
    }

    /// The config as last loaded.
    pub fn config(&self) -> Arc<Config> {
        self.config.read().expect("config lock").clone()
    }

    /// Re-read config.toml (the key screen saved it) and re-apply what
    /// depends on it: the global keys and each session's key hints.
    pub(crate) fn reload_config(&self) -> Result<Value> {
        let mut cfg = Config::load()?;
        // (Not in the file: the `rlord` that opened the list set it. Kept
        // under the same lock as the swap, so a concurrent set isn't lost.)
        self.update_config(|c| {
            cfg.tmux.profile_env = c.tmux.profile_env.take();
            *c = cfg;
        });
        self.bind_nav_keys();
        profile::sync_all(self, false);
        overlord::style(&self.db(), &self.config());
        Ok(json!({"reloaded": true}))
    }

    /// Push a UI command to the terminal's TUIs; park it for the next TUI
    /// if none is up.
    pub(crate) fn ui_cmd(&self, cmd: Value) {
        if self.tui_count() == 0 {
            *self.pending_ui.lock().expect("ui mutex") =
                Some((std::time::Instant::now(), cmd.clone()));
        }
        let _ = self
            .notify
            .send(json!({"notify": "ui", "to": "terminal", "cmd": cmd}).to_string());
    }

    /// Push a UI command to `to` (`ui::target`): the terminal's TUIs, or
    /// one web page. A web command is also parked, for the next page that
    /// asks, in case it never reaches its page (`page.pending`). Nowhere:
    /// nothing is sent.
    pub(crate) fn ui_cmd_to(&self, to: &ui::Target, cmd: Value) {
        match to {
            ui::Target::Terminal(_) => self.ui_cmd(cmd),
            ui::Target::Web(page) => {
                *self.pending_web.lock().expect("ui mutex") =
                    Some((std::time::Instant::now(), page.clone(), cmd.clone()));
                let _ = self.notify.send(
                    json!({"notify": "ui", "to": "web", "page": page, "cmd": cmd}).to_string(),
                );
            }
            ui::Target::None => {}
        }
    }

    /// A one-line notice (never parked) on the surfaces other than `to`
    /// that are there now: the terminal's TUI, or the most recently used
    /// web page. So a command landing on the screen the user isn't
    /// looking at doesn't go unseen.
    pub(crate) fn notice_others(&self, to: &ui::Target, text: &str) {
        let cmd = json!({"cmd": "notice", "text": text});
        match to {
            ui::Target::Web(_) => {
                if self.tui_count() > 0 {
                    let _ = self
                        .notify
                        .send(json!({"notify": "ui", "to": "terminal", "cmd": cmd}).to_string());
                }
            }
            ui::Target::Terminal(_) => {
                if let Some(page) = ui::latest_page(self) {
                    let _ = self.notify.send(
                        json!({"notify": "ui", "to": "web", "page": page, "cmd": cmd}).to_string(),
                    );
                }
            }
            ui::Target::None => {}
        }
    }

    /// A one-line notice (never parked) on every surface there now: the
    /// terminal's TUIs and every open web page, each in its own words.
    pub(crate) fn notice_all(&self, terminal: &str, web: &str) {
        if self.tui_count() > 0 {
            let cmd = json!({"cmd": "notice", "text": terminal});
            let _ = self
                .notify
                .send(json!({"notify": "ui", "to": "terminal", "cmd": cmd}).to_string());
        }
        let cmd = json!({"cmd": "notice", "text": web});
        let pages: Vec<String> = self.web_ui().pages().iter().map(|p| p.id.clone()).collect();
        for page in pages {
            let _ = self
                .notify
                .send(json!({"notify": "ui", "to": "web", "page": page, "cmd": cmd}).to_string());
        }
    }

    /// Internal call that maps method errors to anyhow (used by the MCP layer).
    pub(crate) fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        match self.call(method, params) {
            Ok(v) => Ok(v),
            Err(MethodError::NotFound) => anyhow::bail!("no such method: {method}"),
            Err(MethodError::Internal(e)) => Err(e),
        }
    }

    /// The folders the repo finder scans (as written, `~` unexpanded): the
    /// configured roots, or `~` while setup is pending (`finder_roots`).
    pub(crate) fn finder_roots(&self, cfg: &Config) -> Vec<String> {
        let pending = setup::needed(&self.db(), cfg);
        cfg.session.finder_roots(pending)
    }

    /// Git repos under `roots` (the finder's, `finder_roots`: every folder
    /// the user named), the folders the scan stayed out of, and whether the
    /// first scan of them is still running (`repo_scan.rs`: never blocks
    /// for long). No roots: none. Nothing here touches the disk.
    fn scanned_repos(&self, cfg: &Config, roots: &[String]) -> repo_scan::Found {
        if roots.is_empty() {
            return repo_scan::Found::default();
        }
        let key = (roots.to_vec(), cfg.session.repo_scan_depth);
        self.repo_scan
            .get(&key, |k| rlord_core::git::scan_repos(&k.0, k.1))
    }

    /// Install the navigation bindings on the tmux server. Best-effort: no
    /// server yet (nothing running) is normal at daemon start; we retry on
    /// every session launch.
    pub fn bind_nav_keys(&self) {
        let cfg = self.config();
        let bindings = cfg.tmux.bindings();
        if let Some(verb) = cfg.tmux.switch_overlap() {
            tracing::warn!(key = %cfg.tmux.overlord_switch, bound_to = verb,
                "[tmux] overlord_switch is also another rlord key, so the overlay's switch key is off: change one in config.toml");
        }
        match tmux::bind_nav_keys(&methods::rlord_bin(), &bindings, cfg.tmux.mouse) {
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
        // (Said once, here.)
        if let Some(why) = self.config().session.base_problem() {
            tracing::warn!("{why}: change it in config.toml");
        }
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
            if let Err(e) = methods::hook_replay(&db, &params) {
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

    /// For a restart of session `r` (every path that restarts one calls it
    /// before taking the DB lock and passes the answer as the restart's
    /// `hooks_work`): whether rlord's hooks still work for its agent, when
    /// its background goes by hook; None otherwise (nothing to check). It
    /// reads the agent's settings files, so never under the lock.
    /// (Overlords, restarted by `overlord.rs` and `recall.rs`, have no
    /// background: nothing to ask.)
    pub fn restart_hooks(&self, r: &str) -> Option<bool> {
        let agent = {
            let db = self.db();
            let s = db.resolve_ref(r).ok()?;
            if context::mode(&db, &s.id).ok().flatten() != Some(context::Delivery::Hook) {
                return None;
            }
            rlord_core::agents::Agent::parse(&s.agent)?
        };
        Some(context::hooks_work(agent))
    }

    /// For background added to session `r` after launch: whether rlord's
    /// hooks work for its agent, when it has no delivery mode yet (its
    /// first background: `methods::context_store` records one); None
    /// otherwise. Never under the DB lock (it reads settings files).
    pub fn first_background_hooks(&self, r: &str) -> Option<bool> {
        let agent = {
            let db = self.db();
            let s = db.resolve_ref(r).ok()?;
            if context::mode(&db, &s.id).ok()?.is_some() {
                return None;
            }
            rlord_core::agents::Agent::parse(&s.agent)?
        };
        Some(context::hooks_work(agent))
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
                let kind = match req.method.as_str() {
                    // TUIs identify themselves so the daemon knows whether a
                    // UI command has anyone to land on.
                    "events.subscribe" => Some(Push::Events {
                        tui: req.params.get("client").and_then(Value::as_str) == Some("tui"),
                    }),
                    "web.subscribe" => Some(Push::Web),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let ack = match kind {
                        Push::Web => json!({"subscribed": true, "rev": self.rev()}),
                        Push::Events { .. } => json!({"subscribed": true}),
                    };
                    let mut out = serde_json::to_vec(&Response::ok(req.id, ack))?;
                    out.push(b'\n');
                    write_half.write_all(&out).await?;
                    return self.push_loop(write_half, kind).await;
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

    /// A subscription's push stream, until the client hangs up or the
    /// daemon goes. `events.subscribe` gets every notify line as sent;
    /// `web.subscribe` gets `{"notify":"changed","rev"}`, at most one per
    /// 100 ms (the changes in between folded into it; `"relay": true` when
    /// all of them were `changed_relay`), and the UI commands
    /// addressed to web pages (`{"notify":"ui","page","cmd"}`), never
    /// folded or dropped by the folding. A lagged receiver counts as a
    /// change (and may lose a command: `page.pending` has the last one).
    async fn push_loop(&self, mut w: tokio::net::unix::OwnedWriteHalf, kind: Push) -> Result<()> {
        use tokio::sync::broadcast::error::{RecvError, TryRecvError};
        const WEB_EVERY: std::time::Duration = std::time::Duration::from_millis(100);
        let mut rx = self.notify.subscribe();
        let tui = matches!(kind, Push::Events { tui: true });
        if tui {
            self.tui_subscribers.fetch_add(1, Ordering::SeqCst);
        }
        if let Push::Web = kind {
            self.web_subscribers.fetch_add(1, Ordering::SeqCst);
        }
        let mut last_web: Option<std::time::Instant> = None;
        let r = loop {
            let line = match rx.recv().await {
                Ok(line) => line,
                Err(RecvError::Lagged(_)) => CHANGED.to_string(),
                Err(RecvError::Closed) => break Ok(()),
            };
            // (A relay command is for the plugin alone.)
            if matches!(kind, Push::Events { .. }) && is_remote_line(&line) {
                continue;
            }
            let mut out = vec![line];
            if let Push::Web = kind {
                if out[0] != CHANGED && out[0] != CHANGED_RELAY {
                    // A UI command (through at once if it is the web's) or
                    // a relay command.
                    match web_line(&out[0]) {
                        Some(l) => out[0] = l,
                        None => continue,
                    }
                } else {
                    if let Some(at) = last_web {
                        let wait = WEB_EVERY.saturating_sub(at.elapsed());
                        if !wait.is_zero() {
                            tokio::time::sleep(wait).await;
                        }
                    }
                    // Whatever came meanwhile: changes are folded into this
                    // one, the web's UI commands kept (in order, after it).
                    let mut relay_only = out[0] == CHANGED_RELAY;
                    out = vec![String::new()];
                    loop {
                        match rx.try_recv() {
                            Ok(l) if l == CHANGED_RELAY => {}
                            Ok(l) if l == CHANGED => relay_only = false,
                            Ok(l) => {
                                if let Some(l) = web_line(&l) {
                                    out.push(l);
                                }
                            }
                            Err(TryRecvError::Lagged(_)) => relay_only = false,
                            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
                        }
                    }
                    let mut changed = json!({"notify": "changed", "rev": self.rev()});
                    if relay_only {
                        changed["relay"] = json!(true);
                    }
                    out[0] = changed.to_string();
                    last_web = Some(std::time::Instant::now());
                }
            }
            let mut buf = String::new();
            for l in out {
                buf.push_str(&l);
                buf.push('\n');
            }
            if w.write_all(buf.as_bytes()).await.is_err() {
                break Ok(()); // client gone
            }
        };
        if tui {
            self.tui_subscribers.fetch_sub(1, Ordering::SeqCst);
        }
        // The plugin is gone, and its pages and relay state with it (unless
        // a restarted plugin's stream is already up: it reports its own).
        if let Push::Web = kind {
            if self.web_subscribers.fetch_sub(1, Ordering::SeqCst) == 1 {
                self.web_ui().clear();
                if self.relay.plugin_gone() {
                    self.changed();
                }
            }
        }
        r
    }

    fn dispatch(&self, req: Request) -> Response {
        let id = req.id;
        // The user's relay commands: from the socket only, never through
        // `rpc` (so no MCP tool or web op reaches them).
        let r = match relay::user_call(self, &req.method, &req.params) {
            Some(r) => r.map_err(MethodError::Internal),
            None => self.call(&req.method, req.params),
        };
        match r {
            Ok(result) => Response::ok(id, result),
            Err(MethodError::NotFound) => Response::err(
                id,
                rpc::ERR_METHOD_NOT_FOUND,
                format!("no such method: {}", req.method),
            ),
            Err(MethodError::Internal(e)) => {
                tracing::warn!(method = %req.method, error = %e, "method failed");
                let code = if e.downcast_ref::<web::Incompatible>().is_some() {
                    rlord_core::plugin::RPC_ERR_INCOMPATIBLE
                } else if e.downcast_ref::<web::NotAllowed>().is_some() {
                    rlord_core::plugin::RPC_ERR_NOT_ALLOWED
                } else {
                    rpc::ERR_INTERNAL
                };
                Response::err(id, code, e.to_string())
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
                | "session.fork"
                | "session.restart"
                // (Enter on a stopped session resumes it.)
                | "ui.goto"
                | "session.defaults"
                | "overlord.ensure"
                | "overlord.switch"
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
        // A Codex session about to start on Codex's shared background
        // server: the server runs first, started by rlord (codex_daemon.rs).
        if self.starts_on_codex_daemon(method, &params) {
            codex_daemon::ensure_running();
        }
        if let Some(r) = self.call_ui(method, &params) {
            if r.is_ok() {
                codex_daemon::sync_gate(&self.db());
            }
            return r.map_err(MethodError::Internal);
        }
        let mutating = !matches!(
            method,
            "session.list"
                | "session.get"
                | "session.log"
                | "session.context"
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
        // Stopping a session on Codex's daemon: its running turn is
        // interrupted first, with no DB lock held (it can take seconds).
        let mut params = params;
        if matches!(method, "session.stop" | "session.archive") {
            let s = session_id
                .as_deref()
                .and_then(|id| self.db().resolve_ref(id).ok())
                .filter(|s| s.codex_daemon && s.status != rlord_core::types::Status::Stopped);
            if let Some(s) = s {
                params["interrupt"] = codex_daemon::interrupt(&s).to_json();
            }
        }
        // Where a new session's branch starts (it may fetch: seconds):
        // worked out here, never under the DB lock, and never taken from
        // the caller.
        // (A launch that will be refused anyway waits on no fetch; its
        // checks run once, here, and `session_new` takes their result.)
        let mut prechecked = None;
        if method == "session.new" {
            prechecked = Some(
                methods::precheck_new(&self.db(), &self.config(), &params)
                    .map_err(MethodError::Internal)?,
            );
            repo_latest::set_launch_base(self, &mut params).map_err(MethodError::Internal)?;
        }
        // Whether rlord's hooks work for the agent (it reads the agent's
        // settings files): asked here, never under the DB lock.
        match method {
            // (Only a launch with background asks.)
            "session.new" if context::needs_background(&params) => {
                let works =
                    methods::agent_param(&self.config(), &params).is_ok_and(context::hooks_work);
                params["hooks_work"] = json!(works);
            }
            "session.restart" => {
                if let Some(works) = self.restart_hooks(params["ref"].as_str().unwrap_or("")) {
                    params["hooks_work"] = json!(works);
                }
            }
            "session.context_add" => {
                let r = params["ref"].as_str().unwrap_or("").to_string();
                if let Some(works) = self.first_background_hooks(&r) {
                    params["hooks_work"] = json!(works);
                }
            }
            _ => {}
        }
        // A status filter is matched after annotating
        // (`shown_status_matches`: the overlays and your turn by the status
        // shown, the others by the stored one); `limited` and `background`
        // aren't stored, so with one of them the DB query drops the filter.
        let status_filter = (method == "session.list")
            .then(|| take_status_filter(&mut params))
            .flatten();
        // The repo scan walks the disk: never under the DB lock, or one slow
        // folder stalls every other method.
        // (One config read for both, so the roots reported are the ones
        // scanned.)
        let scanned = (method == "repo.find").then(|| {
            let cfg = self.config();
            let roots = self.finder_roots(&cfg);
            let found = self.scanned_repos(&cfg, &roots);
            (found, roots, cfg)
        });
        let db = self.db.lock().expect("db mutex poisoned");
        let r = match method {
            "session.list" => methods::session_list(&db, &params),
            "session.new" => {
                methods::session_new_with(&db, &self.config(), &params, prechecked.take())
            }
            "repo.list" => methods::repo_list(&db, &self.config()),
            "repo.find" => {
                let (found, roots, cfg) = scanned.expect("scanned for repo.find");
                methods::repo_find(&db, &cfg, &roots, found)
            }
            "session.defaults" => {
                let cfg = self.config();
                let needed = setup::needed(&db, &cfg);
                methods::session_defaults(&cfg, needed)
            }
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
            "session.context" => methods::session_context(&db, &params),
            "session.context_add" => methods::session_context_add(&db, &params),
            // The hook client printed an answer: what it carried is handed over.
            "hook.ack" => methods::hook_ack(&db, &params),
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
                annotate_status(self, &mut rows, true);
                if let Some((taken, others)) = &status_filter {
                    rows.retain(|r| shown_status_matches(r, taken, others));
                }
                usage::list(self, &params, &mut rows);
                Ok(Value::Array(rows))
            }
            ("session.get", Ok(row)) => {
                let mut rows = [row];
                annotate_status(self, &mut rows, true);
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
            codex_daemon::sync_gate(&self.db());
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
    /// Whether `method` starts (or restarts) a Codex session that runs on
    /// Codex's shared background server. Asked before any lock is taken.
    fn starts_on_codex_daemon(&self, method: &str, params: &Value) -> bool {
        match method {
            "session.new" => methods::new_params_on_codex_daemon(&self.config(), params),
            "session.restart" | "ui.goto" => params["ref"]
                .as_str()
                .and_then(|r| self.db().resolve_ref(r).ok())
                // (`ui.goto` restarts one that is stopped or whose tmux
                // session is gone, as `ui::goto` does.)
                .is_some_and(|s| {
                    s.codex_daemon
                        && (method != "ui.goto"
                            || s.status == rlord_core::types::Status::Stopped
                            || !s
                                .tmux_session
                                .as_deref()
                                .is_some_and(rlord_core::tmux::has_session))
                }),
            _ => false,
        }
    }

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
                let r = {
                    // One guard for both reads (std's Mutex isn't reentrant).
                    let db = self.db();
                    overlord::session(&db).and_then(|o| {
                        let Some(o) = o else {
                            return Ok((Value::Null, Default::default()));
                        };
                        let mut v = serde_json::to_value(&o)?;
                        v["conversations"] = recall::summary(&db, &o);
                        // What it runs on now (config may since have changed).
                        v["effort"] = json!(overlord::launched_effort(&db, &o));
                        Ok((v, overlord::roster_rows(&db)?))
                    })
                };
                // (The DB guard is gone: `roster` runs tmux and the
                // install checks, `annotate` takes the guard.)
                r.map(|(mut v, rows)| {
                    if v.is_null() {
                        return v;
                    }
                    // The other agents' overlords, for the switchers.
                    v["overlords"] = self.roster(rows);
                    // Stopped on a usage limit: the row and the overlay say
                    // so (the user may switch to another agent's).
                    let mut rows = [v];
                    annotate_status(self, &mut rows, false);
                    let [v] = rows;
                    v
                })
            }
            // Make another agent's overlord the active one (starting or
            // creating it). Not while something is being pasted into the
            // active one: the paste goes to whichever is active under
            // this lock.
            "overlord.switch" => {
                let _send = self.overlord_send.lock().expect("overlord send lock");
                let target = params["agent"]
                    .as_str()
                    .and_then(rlord_core::agents::Agent::parse)
                    .and_then(|a| overlord::for_agent(&self.db(), a).ok().flatten());
                let _session = target.as_ref().map(|t| self.sessions.lock(&t.id));
                let r = overlord::switch(
                    &self.db(),
                    &self.config(),
                    params["agent"].as_str().unwrap_or(""),
                );
                if r.is_ok() {
                    self.changed();
                    self.bind_nav_keys();
                }
                r
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
                let (st, rows) = {
                    let db = self.db();
                    (
                        overlord::status(&db, &self.config()),
                        overlord::roster_rows(&db),
                    )
                };
                // (Without the guard: `roster` runs tmux and the install
                // checks.)
                st.and_then(|mut v| {
                    v["overlords"] = self.roster(rows?);
                    v["remote"] = remote::offer(self);
                    Ok(v)
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
                overlord::style(&self.db(), &self.config());
                Ok(json!({"styled": true}))
            }
            // (Fetches and moves a tree: per-repo lock, no DB lock held;
            // locks.rs.)
            "repo.latest" => repo_latest::latest_rpc(self, params.clone()),
            "repo.latest_list" => repo_latest::list_rpc(self),
            "repo.latest_prune" => repo_latest::prune_rpc(self, params.clone()),
            "session.import_scan" => import::scan(self, params),
            // (Its own locking: OpenCode's fork call, the hooks check and
            // `git worktree add` run without the DB lock.)
            "session.fork" => {
                let r = fork::fork(self, params);
                if let Ok(v) = &r {
                    self.changed();
                    self.bind_nav_keys();
                    if let Some(id) = v["id"].as_str() {
                        // A new tmux session starts on rlord's defaults.
                        profile::sync_ref(self, id);
                        // Its copied history: loaded in the background, as
                        // for an import.
                        warm::enqueue_ref(self, id);
                    }
                }
                r
            }
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
            "limits.resume" => limits::resume(self, params),
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
            "tmux.profile_mode" => profile::profile_mode(self, params),
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
            "setup.status" => setup::status(self),
            "setup.suggest" => setup::suggest(self, params),
            "setup.apply" => setup::apply(self, params),
            "ui.confirm_setup" => setup::confirm(self, params),
            "overlord.setup_request" => ask::setup_request(self, params),
            "keys.list" => ui::keys_list(self),
            "config.reload" => self.reload_config(),
            "ui.view.set" => ui::set_view(self, params),
            "ui.view.default" => ui::default_view(self),
            "ui.usage" => ui::usage(self, params),
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
            // Without `overlord: true` (`rlord mcp --overlord`) the server
            // is read-only: only `mcp::READ_ONLY_TOOLS`, listed and callable.
            // The web plugin (web.rs). Never reachable from MCP or from a
            // browser's `web.call`.
            "web.hello" => web::hello(self, params),
            "web.call" => web::call(self, params),
            "web.pane_target" => web::pane_target(self, params),
            "web.pages" => web::pages(self, params),
            "web.status" => web::status(self),
            "web.start" => web::start(self),
            "web.stop" => web::stop(self),
            "web.rotate" => web::rotate(self),
            // The plugin's relay side (relay.rs).
            "web.remote_reply" => relay::remote_reply(self, params),
            "web.remote_state" => relay::remote_state(self, params),
            "mcp.tools" => Ok(mcp::tools_for(params["overlord"] == true)),
            "mcp.call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let overlord = params["overlord"] == true;
                tracing::info!(tool = name, overlord, caller = ?params.get("caller"), "mcp tool call");
                if overlord || mcp::READ_ONLY_TOOLS.contains(&name) {
                    mcp::call(self, name, &args)
                } else {
                    Err(anyhow::anyhow!(
                        "{name}: not available on the read-only rlord MCP server \
                         (only the overlord's `rlord mcp --overlord` serves it)"
                    ))
                }
            }
            _ => return None,
        };
        Some(r)
    }
}

/// A change (`Daemon::changed`), and one only of the relay status
/// (`Daemon::changed_relay`), on the push stream.
const CHANGED: &str = r#"{"notify":"changed"}"#;
const CHANGED_RELAY: &str = r#"{"notify":"changed","relay":true}"#;

/// A broadcast line as `web.subscribe` passes it on: a UI command for a
/// web page (`{"notify":"ui","page","cmd"}`), or a relay command for the
/// plugin (`{"notify":"remote","id","req"}`, as is).
fn web_line(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v["notify"] == "remote" {
        return Some(line.to_string());
    }
    (v["notify"] == "ui" && v["to"] == "web")
        .then(|| json!({"notify": "ui", "page": v["page"], "cmd": v["cmd"]}).to_string())
}

/// A relay command (`relay.rs`), which only the plugin's stream carries.
fn is_remote_line(line: &str) -> bool {
    line.contains(r#""notify":"remote""#)
        && serde_json::from_str::<Value>(line).is_ok_and(|v| v["notify"] == "remote")
}

/// What a subscription's push stream carries (`Daemon::push_loop`).
#[derive(Clone, Copy)]
enum Push {
    /// `events.subscribe`: every notify line; a TUI is counted.
    Events { tui: bool },
    /// `web.subscribe`: coalesced `changed` with the `rev`, and the UI
    /// commands addressed to web pages.
    Web,
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
    client_call_within(socket_path, method, params, None)
}

/// `client_call`, giving up on a send or an answer after `timeout` (None:
/// waits as long as it takes).
pub fn client_call_within(
    socket_path: &Path,
    method: &str,
    params: Value,
    timeout: Option<std::time::Duration>,
) -> Result<Value> {
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket_path).with_context(|| {
        format!(
            "connecting to daemon at {} (is `rlord daemon` running?)",
            socket_path.display()
        )
    })?;
    stream.set_write_timeout(timeout)?;
    stream.set_read_timeout(timeout)?;
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
/// Of a prompt, only what identifies its text goes (`prompt_hash`,
/// `prompt_pasted`: how the daemon knows a prompt rlord typed itself,
/// `own.rs`).
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
            if let Some(prompt) = v.get("prompt").and_then(Value::as_str) {
                let mark = rlord_core::types::PromptMark::of(prompt);
                out.insert("prompt_hash".to_string(), json!(mark.hash));
                if mark.pasted {
                    out.insert("prompt_pasted".to_string(), json!(true));
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

/// What the daemon answered a hook with, for the agent: the hook's output
/// (e.g. the session's background at SessionStart), and the id to
/// acknowledge it with once printed (`send_hook_ack`).
#[derive(Debug, Clone, PartialEq)]
pub struct HookReply {
    pub output: Value,
    pub ack: Option<String>,
}

/// Send one hook event, never waiting long: a hook runs inside the agent's
/// tool call, and a stalled daemon must not hold that up. A second to
/// connect, a second to send, two for the answer.
///
/// Only a timeout while waiting for the answer is `Slow`. Anything else
/// short of an answer (no socket, refused, not connected in time, a send
/// that didn't complete, the connection closed or reset) is `Unreachable`:
/// the daemon did not take a whole event and answer, so it is kept.
///
/// Also what the daemon answered the hook with, for the agent, if
/// anything. Only a `Delivered` event has one.
pub fn send_hook_event_for_reply(
    socket_path: &Path,
    params: &Value,
) -> (HookSent, Option<HookReply>) {
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
    let (tx, rx) = mpsc::channel::<(HookSent, Option<HookReply>)>();
    let path = socket_path.to_path_buf();
    let req = json!({"id": 1, "method": "hook.emit", "params": params});
    let sender_state = Arc::clone(&state);
    // On its own thread: connecting to a socket nobody accepts on can block.
    std::thread::spawn(move || {
        let state = sender_state;
        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&path) else {
            let _ = tx.send((HookSent::Unreachable, None));
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
            let _ = tx.send((HookSent::Unreachable, None));
            return;
        }
        let mut line = String::new();
        let _ = tx.send(match StdBufReader::new(&mut stream).read_line(&mut line) {
            Ok(n) if n > 0 => {
                let result = serde_json::from_str::<Value>(&line)
                    .ok()
                    .map(|v| v["result"].clone())
                    .unwrap_or(Value::Null);
                let reply = result
                    .get("reply")
                    .filter(|r| !r.is_null())
                    .map(|r| HookReply {
                        output: r.clone(),
                        ack: result["reply_ack"].as_str().map(str::to_string),
                    });
                (HookSent::Delivered, reply)
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                (HookSent::Slow, None)
            }
            // Closed or reset before an answer: the daemon went away.
            _ => (HookSent::Unreachable, None),
        });
    });
    // Connected within a second, or the event is kept (and the sender,
    // should it connect later, drops it).
    let connect = Duration::from_secs(1);
    let start = std::time::Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(sent) => return sent,
            Err(mpsc::RecvTimeoutError::Disconnected) => return (HookSent::Unreachable, None),
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
            return (HookSent::Unreachable, None);
        }
    }
    // Sending: the sender's own timeouts (1s + 2s) end it; a little more
    // than that is waited, so its verdict is the one acted on.
    rx.recv_timeout(Duration::from_millis(3500))
        .unwrap_or((HookSent::Slow, None))
}

/// Tell the daemon a hook's answer was printed for the agent (`ack` from
/// its `HookReply`), so what it carried counts as handed over. Best
/// effort, a second at most each way: if it never arrives, the next
/// prompt gets the same again.
pub fn send_hook_ack(socket_path: &Path, ack: &str) -> bool {
    client_call_within(
        socket_path,
        "hook.ack",
        json!({"ack": ack}),
        Some(std::time::Duration::from_secs(1)),
    )
    .is_ok()
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
        rlord_core::private::dir_under(&paths::data_dir(), dir)?;
    }
    use std::io::Write;
    let mut f = rlord_core::private::append(&path)?;
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

/// A status that isn't stored but laid over rows after the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    Limited,
    Background,
}

impl Overlay {
    fn of(s: &str) -> Option<Overlay> {
        match s {
            "limited" => Some(Overlay::Limited),
            "background" => Some(Overlay::Background),
            _ => None,
        }
    }
}

/// What every place that shows sessions lays over their rows: Remote
/// Control (`remote`, where it applies), background work, usage limits,
/// then `shown_status` from those. Not under the DB lock (the annotations
/// take it).
pub(crate) fn annotate_status(d: &Daemon, rows: &mut [Value], remote: bool) {
    if remote {
        remote::annotate(d, rows);
    }
    background::annotate(d, rows);
    limits::annotate(d, rows);
    set_shown_status(rows);
}

/// Set each row's `shown_status` once, after the annotations: what the
/// list, `rlord ls`, the overlord and the filters all go by.
fn set_shown_status(rows: &mut [Value]) {
    for r in rows {
        r["shown_status"] = json!(shown_status(r));
    }
}

/// The status a row shows: a usage limit over anything, then background
/// work over your turn, else the stored status. The one place this
/// precedence is decided.
fn shown_status(r: &Value) -> &str {
    if !r["limit"].is_null() {
        "limited"
    } else if !r["async_tasks"].is_null() {
        "background"
    } else {
        r["status"].as_str().unwrap_or("")
    }
}

/// Whether a row matches a status filter. The overlays and the status they
/// are drawn over (`idle`: your turn, `background`, `limited`) match the
/// status the row shows, so `status:turn` leaves out a row shown as
/// background work or limited. The other stored statuses (`stopped`,
/// `working`, `error`, …) match the stored status: a limit drawn over a
/// stopped session doesn't change that it is stopped.
fn shown_status_matches(r: &Value, taken: &[Overlay], others: &[String]) -> bool {
    let shown = r["shown_status"]
        .as_str()
        .unwrap_or_else(|| shown_status(r));
    let stored = r["status"].as_str().unwrap_or("");
    if Overlay::of(shown).is_some_and(|o| taken.contains(&o)) {
        return true;
    }
    others.iter().any(|o| match o.as_str() {
        "idle" => shown == "idle",
        o => stored == o,
    })
}

/// A `session.list` query's `status` filter, split into the overlay
/// statuses it names (`limited`, `background`) and the stored ones, to be
/// matched after annotating (`shown_status_matches`: overlays and your
/// turn by the shown status, the other stored statuses by the stored one).
/// With an overlay named, the filter is taken out of the DB query (the DB
/// doesn't store them); with stored ones only, it stays there to narrow
/// the rows first. None: no status filter.
fn take_status_filter(params: &mut Value) -> Option<(Vec<Overlay>, Vec<String>)> {
    let st = params.get("status")?.as_array()?;
    let taken: Vec<Overlay> = st.iter().filter_map(|v| Overlay::of(v.as_str()?)).collect();
    // (Any status filter is matched again after annotating: `status:turn`
    // must leave out the rows shown as background or limited.)
    if st.is_empty() {
        return None;
    }
    let others = st
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| Overlay::of(s).is_none())
        .map(str::to_string)
        .collect();
    // Only stored statuses asked for: the DB narrows them first.
    if !taken.is_empty() {
        params["status"] = json!([]);
    }
    Some((taken, others))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `web.subscribe` stream over a socket pair: the daemon's end runs
    /// `push_loop`, the lines come out of the other.
    fn web_stream(
        d: &Arc<Daemon>,
        rt: &tokio::runtime::Runtime,
    ) -> tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>> {
        rt.block_on(async {
            let (a, b) = UnixStream::pair().unwrap();
            let (_, w) = a.into_split();
            let d = Arc::clone(d);
            tokio::spawn(async move {
                let _ = d.push_loop(w, Push::Web).await;
            });
            let (r, _) = b.into_split();
            // (Subscribed once the loop runs.)
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            BufReader::new(r).lines()
        })
    }

    /// The web's UI commands go through the folding of changes: a burst of
    /// changes with a command in it delivers the command once, after a
    /// `changed`, and a command for the terminal isn't passed on.
    #[test]
    fn web_commands_survive_the_folding() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        d.web_ui().set_pages(&["p1-aa".to_string()]);
        let mut lines = web_stream(&d, &rt);
        // (Under the channel's 64, so the stream doesn't lag.)
        for round in 0..3 {
            for _ in 0..8 {
                d.changed();
            }
            d.ui_cmd(json!({"cmd": "set_view"}));
            d.ui_cmd_to(
                &ui::Target::Web("p1-aa".into()),
                json!({"cmd": "goto", "n": round}),
            );
            for _ in 0..8 {
                d.changed();
            }
        }
        let last = d.rev();
        let mut got = vec![];
        let mut seen_last = false;
        rt.block_on(async {
            loop {
                let l = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
                    .await
                    .unwrap_or_else(|_| panic!("in time: {got:?}"))
                    .unwrap()
                    .expect("open");
                let v: Value = serde_json::from_str(&l).unwrap();
                seen_last |= v["rev"] == last.as_str();
                got.push(v);
                // (The commands sent last come after the last `changed`.)
                if seen_last && got.iter().filter(|v| v["notify"] == "ui").count() == 3 {
                    break;
                }
            }
        });
        let cmds: Vec<&Value> = got.iter().filter(|v| v["notify"] == "ui").collect();
        assert_eq!(cmds.len(), 3, "{got:?}");
        for (i, c) in cmds.iter().enumerate() {
            assert_eq!(c["cmd"], json!({"cmd": "goto", "n": i}));
            assert_eq!(c["page"], "p1-aa");
            assert!(c.get("to").is_none(), "{c}");
        }
        assert_eq!(got[0]["notify"], "changed");
    }

    /// The user's relay commands are reached only from the socket: never
    /// through `rpc`, which the MCP tools and the web ops call methods by.
    #[test]
    fn relay_commands_are_not_reachable_through_rpc() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        for m in [
            "relay.status",
            "relay.setup",
            "relay.pair",
            "relay.pair_cancel",
            "relay.confirm",
            "relay.reject",
            "relay.devices",
            "relay.revoke",
            "relay.disable",
        ] {
            let e = d.rpc(m, json!({})).unwrap_err().to_string();
            assert!(e.contains("no such method"), "{m}: {e}");
        }
        assert!(relay::user_call(&d, "relay.other", &json!({})).is_none());
        // The dispatcher answers them.
        let r = d.dispatch(Request {
            id: 1,
            method: "relay.status".into(),
            params: json!({}),
        });
        assert_eq!(
            serde_json::to_value(&r).unwrap()["result"]["enabled"],
            false
        );
    }

    /// A relay command reaches the plugin's stream only.
    #[test]
    fn remote_lines_are_the_plugins() {
        let line = json!({"notify": "remote", "id": "r1", "req": {"cmd": "pair"}}).to_string();
        assert!(is_remote_line(&line));
        assert_eq!(web_line(&line), Some(line.clone()));
        assert!(!is_remote_line(r#"{"notify":"changed"}"#));
        assert!(!is_remote_line(
            r#"{"notify":"ui","to":"web","page":"p","cmd":{"cmd":"notice","text":"\"notify\":\"remote\""}}"#
        ));
    }

    /// A command for a page that closed before it arrived is handed to the
    /// next page that asks, once; the terminal's slot is separate.
    #[test]
    fn a_lost_web_command_is_parked() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let pending = |d: &Daemon| {
            d.rpc("web.call", json!({"op": "page.pending", "via": "local"}))
                .unwrap()
        };
        d.web_ui().set_pages(&["p1-aa".to_string()]);
        d.ui_cmd_to(
            &ui::Target::Web("p1-aa".into()),
            json!({"cmd": "show_list"}),
        );
        // Its page is still open (it got it): nothing to hand on.
        assert_eq!(pending(&d), Value::Null);
        // The page reloads: the new one gets it, once.
        d.web_ui().set_pages(&["p2-bb".to_string()]);
        assert_eq!(d.rpc("ui.pending", json!({})).unwrap(), Value::Null);
        assert_eq!(pending(&d), json!({"cmd": "show_list"}));
        assert_eq!(pending(&d), Value::Null);
        // A page that said something after it had it: not handed on when
        // it closes.
        d.ui_cmd_to(&ui::Target::Web("p2-bb".into()), json!({"cmd": "goto"}));
        d.rpc(
            "web.call",
            json!({"op": "page.report", "via": "local", "page": "p2-bb", "args": {}}),
        )
        .unwrap();
        d.web_ui().set_pages(&["p3-cc".to_string()]);
        assert_eq!(pending(&d), Value::Null);
        // The terminal's parked command stays the terminal's.
        d.ui_cmd(json!({"cmd": "keys"}));
        assert_eq!(pending(&d), Value::Null);
        assert_eq!(
            d.rpc("ui.pending", json!({})).unwrap(),
            json!({"cmd": "keys"})
        );
    }

    #[test]
    fn filters_match_the_status_shown() {
        let idle = json!({"status": "idle"});
        let bg = json!({"status": "idle", "async_tasks": 2});
        let limited_bg = json!({"status": "idle", "async_tasks": 2, "limit": {"until": "3pm"}});
        let turn = (vec![], vec!["idle".to_string()]);
        let bgf = (vec![Overlay::Background], vec![]);
        let lim = (vec![Overlay::Limited], vec![]);
        let m = |r: &Value, f: &(Vec<Overlay>, Vec<String>)| shown_status_matches(r, &f.0, &f.1);
        assert!(m(&idle, &turn));
        // status:turn leaves out the rows shown as background or limited.
        assert!(!m(&bg, &turn));
        assert!(!m(&limited_bg, &turn));
        // status:bg: not when a limit is drawn over it.
        assert!(m(&bg, &bgf));
        assert!(!m(&limited_bg, &bgf));
        assert!(m(&limited_bg, &lim));
        // status:stopped still finds a stopped session a limit is drawn over.
        let limited_stopped = json!({"status": "stopped", "limit": {"until": "3pm"}});
        let stopped = (vec![], vec!["stopped".to_string()]);
        assert!(m(&limited_stopped, &stopped));
        assert!(m(&limited_stopped, &lim));
        assert!(!m(&idle, &stopped));
    }

    #[test]
    fn status_limited_is_taken_out_of_the_db_query() {
        let mut p = json!({"status": ["limited", "waiting_input"]});
        assert_eq!(
            take_status_filter(&mut p),
            Some((vec![Overlay::Limited], vec!["waiting_input".to_string()]))
        );
        assert_eq!(p["status"], json!([]));
        let mut p = json!({"status": ["background", "limited"]});
        assert_eq!(
            take_status_filter(&mut p),
            Some((vec![Overlay::Background, Overlay::Limited], vec![]))
        );
        let mut p = json!({"status": ["idle"]});
        assert_eq!(
            take_status_filter(&mut p),
            Some((vec![], vec!["idle".to_string()]))
        );
        assert_eq!(p["status"], json!(["idle"]));
        let mut p = json!({});
        assert_eq!(take_status_filter(&mut p), None);
        // Nothing limited: the list is empty, not every session.
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        d.db()
            .insert_session(&rlord_core::db::NewSession {
                id: "s1",
                name: "a",
                agent: "claude",
                agent_session: None,
                project_path: "/tmp/p",
                tmux_session: None,
                launch_spec: "{}",
                created_at: 1000,
                repo_path: None,
                workspace_path: None,
                branch: None,
                role: "",
            })
            .unwrap();
        assert_eq!(
            d.rpc("session.list", json!({}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let rows = d
            .rpc("session.list", json!({"status": ["limited"]}))
            .unwrap();
        assert_eq!(rows, json!([]));
    }

    /// `update_session` checks the background it adds before writing
    /// anything: a refused one leaves the rest of the session as it was.
    #[test]
    fn a_refused_background_changes_nothing_else() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        d.db()
            .insert_session(&rlord_core::db::NewSession {
                id: "s1",
                name: "a",
                agent: "claude",
                agent_session: None,
                project_path: "/tmp/p",
                tmux_session: None,
                launch_spec: "{}",
                created_at: 1000,
                repo_path: None,
                workspace_path: None,
                branch: None,
                role: "",
            })
            .unwrap();
        let long = "x".repeat(context::NOTE_MAX + 1);
        for bad in [
            json!({"session": "a", "name": "renamed", "description": "d", "add_context": long}),
            json!({"session": "a", "name": "renamed", "add_related": ["nope"]}),
        ] {
            assert!(mcp::call(&d, "update_session", &bad).is_err());
            let s = d.db().resolve_ref("s1").unwrap();
            assert_eq!((s.name.as_str(), s.description.as_str()), ("a", ""));
            assert!(d.db().context_rows("s1").unwrap().is_empty());
        }
        // The overlord's own row gets no background.
        d.db()
            .insert_session(&rlord_core::db::NewSession {
                id: "o1",
                name: "overlord",
                agent: "claude",
                agent_session: None,
                project_path: "/tmp/o",
                tmux_session: None,
                launch_spec: "{}",
                created_at: 1000,
                repo_path: None,
                workspace_path: None,
                branch: None,
                role: "overlord",
            })
            .unwrap();
        let e = mcp::call(
            &d,
            "update_session",
            &json!({"session": "o1", "add_context": "x"}),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("the overlord can't be given background"), "{e}");
        assert!(d.db().context_rows("o1").unwrap().is_empty());
    }

    #[test]
    fn the_read_only_mcp_server_refuses_mutating_tools() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        let names = |overlord: bool| -> Vec<String> {
            d.rpc("mcp.tools", json!({"overlord": overlord}))
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect()
        };
        let mut ro = names(false);
        ro.sort();
        let mut want: Vec<String> = mcp::READ_ONLY_TOOLS.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(ro, want);
        assert!(names(true).len() > ro.len());
        // No flag at all means read-only too.
        assert_eq!(
            d.rpc("mcp.tools", json!({}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            ro.len()
        );
        let call = |name: &str, overlord: Value| {
            d.rpc(
                "mcp.call",
                json!({"name": name, "arguments": {"session": "nope", "name": "x"}, "overlord": overlord}),
            )
        };
        for tool in [
            "update_session",
            "send_to_session",
            "session_action",
            "set_list_view",
        ] {
            let e = format!("{:#}", call(tool, Value::Null).unwrap_err());
            assert!(e.contains("read-only"), "{tool}: {e}");
        }
        // The overlord's call gets past the gate (and fails on the ref).
        let e = format!("{:#}", call("update_session", json!(true)).unwrap_err());
        assert!(!e.contains("read-only"), "{e}");
        // A read tool works read-only.
        assert!(call("list_groups", Value::Null).is_ok());
    }

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
        // Each row says which measure the list used: asked for, else the
        // list's saved pick (as `rlord ls` without --usage).
        assert_eq!(v[0]["usage_metric"], "tokens");
        assert_eq!(
            list(json!({"sort": "usage"})).await.unwrap()[0]["usage_metric"],
            "turns"
        );
        daemon.db().kv_set("ui_usage", "tokens").unwrap();
        let v = list(json!({"sort": "usage"})).await.unwrap();
        assert_eq!(v[0]["usage_metric"], "tokens");
        assert_eq!(
            ids(&v),
            ["01b", "01a", "01c"],
            "sorted by the saved pick: {v}"
        );
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

        // A prompt rlord typed itself (marked right before its paste) is
        // logged as rlord's and is not the user's; the user's next one,
        // moments later with other text, is theirs.
        let user_at = s["last_user_msg_at"].as_i64().unwrap();
        own::mark(
            &daemon.db(),
            "01test",
            "continue",
            own::Source::Resume,
            rlord_core::types::now_ms(),
        )
        .unwrap();
        let prompt = |text: &str| {
            let raw = json!({"hook_event_name": "UserPromptSubmit", "session_id": "uuid-1",
                             "prompt": text})
            .to_string();
            let payload = slim_hook_payload(&raw);
            assert!(payload.get("prompt").is_none(), "only its hash is sent");
            call(
                "hook.emit",
                json!({"instance": "01test", "agent": "claude", "event": "UserPromptSubmit",
                       "payload": payload}),
            )
        };
        prompt("continue\n").await.unwrap();
        let s = call("session.get", json!({"ref": "01test"})).await.unwrap();
        assert_eq!(s["status"], "working");
        assert_eq!(s["last_user_msg_at"], user_at);
        assert!(s["last_rlord_msg_at"].as_i64().unwrap() >= user_at);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        prompt("no, stop").await.unwrap();
        let s = call("session.get", json!({"ref": "01test"})).await.unwrap();
        assert!(s["last_user_msg_at"].as_i64().unwrap() > user_at);
        let log = call("session.log", json!({"ref": "01test"})).await.unwrap();
        let last: Vec<(&str, &str)> = log
            .as_array()
            .unwrap()
            .iter()
            .skip(2)
            .map(|e| {
                (
                    e["kind"].as_str().unwrap(),
                    e["payload"]["source"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(last, [("rlord_prompt", "resume"), ("prompt_submitted", "")]);

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
            send_hook_event_for_reply(&dir.path().join("absent.sock"), &json!({})),
            (HookSent::Unreachable, None)
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
            send_hook_event_for_reply(&sock, &json!({"event": "Stop"})),
            (HookSent::Slow, None)
        );
        assert!(start.elapsed() < std::time::Duration::from_millis(3400));
        hold.join().unwrap();
        // A daemon that goes away before answering (a restart): the event
        // is kept, not lost.
        let sock = dir.path().join("restarting.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let close = std::thread::spawn(move || drop(listener.accept()));
        assert_eq!(
            send_hook_event_for_reply(&sock, &json!({"event": "Stop"})),
            (HookSent::Unreachable, None)
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
            send_hook_event_for_reply(&sock, &json!({"event": "Stop"})),
            (HookSent::Delivered, None)
        );
        answer.join().unwrap();
        // One that answers with something for the agent: handed back.
        let sock = dir.path().join("reply.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let answer = std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let (mut c, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(&c).read_line(&mut line).unwrap();
            c.write_all(
                b"{\"id\":1,\"result\":{\"ok\":true,\"reply\":{\"x\":1},\"reply_ack\":\"a1\"}}\n",
            )
            .unwrap();
        });
        assert_eq!(
            send_hook_event_for_reply(&sock, &json!({"event": "SessionStart"})),
            (
                HookSent::Delivered,
                Some(HookReply {
                    output: json!({"x": 1}),
                    ack: Some("a1".into())
                })
            )
        );
        answer.join().unwrap();
        // The acknowledgement names the answer.
        let sock = dir.path().join("ack.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let answer = std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            let (mut c, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(&c).read_line(&mut line).unwrap();
            c.write_all(b"{\"id\":1,\"result\":{\"acked\":true}}\n")
                .unwrap();
            serde_json::from_str::<Value>(&line).unwrap()
        });
        assert!(send_hook_ack(&sock, "a1"));
        let req = answer.join().unwrap();
        assert_eq!(req["method"], "hook.ack");
        assert_eq!(req["params"]["ack"], "a1");
        assert!(!send_hook_ack(&dir.path().join("absent.sock"), "a1"));
        // Nobody there: no reply (the hook prints nothing).
        assert_eq!(
            send_hook_event_for_reply(&dir.path().join("absent.sock"), &json!({})),
            (HookSent::Unreachable, None)
        );
    }

    /// A session's own hook gets its background back; a hook from outside
    /// its pane, or a session without background, gets nothing.
    #[test]
    fn a_sessions_own_hook_gets_its_background() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&rlord_core::db::NewSession {
            id: "01bg",
            name: "web-footer",
            agent: "claude",
            agent_session: Some("u-1"),
            project_path: "/tmp",
            tmux_session: Some("rlord_01bg_absent"),
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        let emit = |event: &str, extra: Value| {
            let mut params = json!({"instance": "01bg", "agent": "claude", "event": event,
                                    "payload": {"session_id": "u-1", "hook_event_name": event}});
            for (k, v) in extra.as_object().unwrap() {
                params[k] = v.clone();
            }
            methods::hook_emit(&db, &params).unwrap()
        };
        assert!(
            emit("SessionStart", json!({}))["reply"].is_null(),
            "no background"
        );
        context::store(&db, "01bg", &["use the staging DB".into()], &[], 5).unwrap();
        // From outside rlord's tmux (a forked copy): nothing.
        let r = emit("SessionStart", json!({"pane": null}));
        assert!(r["ignored"].is_string() && r["reply"].is_null());
        // Replayed from the spool (its client is gone): no answer, nothing
        // marked.
        let replayed = methods::hook_replay(
            &db,
            &json!({"instance": "01bg", "agent": "claude", "event": "SessionStart",
                    "payload": {"session_id": "u-1", "hook_event_name": "SessionStart"}}),
        )
        .unwrap();
        assert!(replayed["reply"].is_null());
        assert_eq!(db.context_unseen("01bg").unwrap().len(), 1);
        let r = emit("SessionStart", json!({}));
        let out = &r["reply"]["hookSpecificOutput"];
        assert_eq!(out["hookEventName"], "SessionStart");
        assert!(out["additionalContext"]
            .as_str()
            .unwrap()
            .contains("use the staging DB"));
        // The client acknowledges it once printed (`hook.ack`): logged
        // then, once.
        let sent = || {
            db.list_events("01bg", 100)
                .unwrap()
                .iter()
                .filter(|e| e.kind == "context_sent")
                .count()
        };
        assert_eq!(sent(), 0);
        let ack = r["reply_ack"].as_str().unwrap();
        assert_eq!(
            methods::hook_ack(&db, &json!({"ack": ack})).unwrap(),
            json!({"acked": true})
        );
        assert_eq!(sent(), 1);
        // Already given: the next prompt gets nothing more, and the status
        // still follows the hook.
        let r = emit("UserPromptSubmit", json!({}));
        assert!(r["reply"].is_null());
        assert_eq!(
            db.resolve_ref("01bg").unwrap().status,
            rlord_core::types::Status::Working
        );
        context::store(&db, "01bg", &["PR is #12".into()], &[], 6).unwrap();
        let r = emit("UserPromptSubmit", json!({}));
        assert!(r["reply"]["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("PR is #12"));
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
        assert!(overlord::fresh_at(&db, &o.id).unwrap() > 0);
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

    /// A session on Codex's daemon: its hooks arrive with no session named
    /// (`by: "thread"`), bind by folder, then go by thread.
    #[test]
    fn codex_daemon_hooks_go_by_thread() {
        use rlord_core::types::Status;
        let db = Db::open_in_memory().unwrap();
        for (id, folder) in [("01dmn", "/src/app"), ("01dmn2", "/src/web")] {
            db.insert_session(&rlord_core::db::NewSession {
                id,
                name: id,
                agent: "codex",
                agent_session: None,
                project_path: folder,
                tmux_session: Some("rlord_01dmn_absent"),
                launch_spec: r#"{"agent":"codex","codex_host":"daemon"}"#,
                created_at: 1,
                repo_path: None,
                workspace_path: None,
                branch: None,
                role: "",
            })
            .unwrap();
            db.set_status(id, Status::Starting, 1).unwrap();
        }
        let index = |_: rlord_core::agents::Agent, _: &str| rlord_core::agents::Indexed::User;
        let emit = |event: &str, sid: &str, cwd: &str| {
            methods::hook_emit_with(
                &db,
                &json!({"instance": null, "by": "thread", "agent": "codex", "event": event,
                        "payload": {"session_id": sid, "hook_event_name": event, "cwd": cwd,
                                    "transcript_path": format!("/c/rollout-x-{sid}.jsonl")}}),
                &index,
            )
            .unwrap()
        };
        let get = |id: &str| db.resolve_ref(id).unwrap();
        // Not a first SessionStart: nothing to bind it to.
        assert!(emit("UserPromptSubmit", "t-1", "/src/app")["ignored"].is_string());
        // The first SessionStart in its folder binds it, confirmed by hook.
        emit("SessionStart", "t-1", "/src/app");
        let s = get("01dmn");
        assert!(s.codex_daemon);
        assert_eq!(s.agent_session.as_deref(), Some("t-1"));
        assert_eq!(s.agent_session_trust, "hook");
        assert_eq!(get("01dmn2").agent_session, None);
        // Then by thread, from wherever it was sent.
        emit("UserPromptSubmit", "t-1", "/elsewhere");
        assert_eq!(get("01dmn").status, Status::Working);
        emit("Stop", "t-1", "/src/app");
        assert_eq!(get("01dmn").status, Status::Idle);
        // Someone else's thread in the same folder: not this session's.
        assert!(emit("UserPromptSubmit", "t-9", "/src/app")["ignored"].is_string());
        assert_eq!(get("01dmn").status, Status::Idle);
        // A stopped one's thread never brings it back.
        db.set_status("01dmn", Status::Stopped, 2).unwrap();
        assert!(emit("UserPromptSubmit", "t-1", "/src/app")["ignored"].is_string());
        assert_eq!(get("01dmn").status, Status::Stopped);
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
