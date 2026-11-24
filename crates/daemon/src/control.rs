//! Control of a pane: which screen a running session's pane is laid out
//! for, and the only one whose keys reach it. Plain code, no agent decides
//! anything (docs/working/web-ui-remote.md, slice 5, decision 3).
//!
//! Each running session (the overlord's included) has at most one
//! **holder**: the terminal (rlord's TUI and the user's tmux client, the
//! overlord overlay), a local page (`page:<id>`) or a paired device
//! (`device:<id>`). Every other screen showing it is a **viewer**, told
//! who holds it and offered Take over.
//!
//! - A free pane goes to the screen that opens it; a pane another present
//!   screen holds is never taken without a take (`pane.take`, the
//!   terminal's take key: `control.take`).
//! - A holder that leaves (its stream or socket closes, the terminal moves
//!   to another session or closes the overlay) is marked away and keeps
//!   control for `GRACE`. Then control passes to the terminal if it shows
//!   the pane, else to the screen used last (`ui::target`'s order), else
//!   the pane is free and keeps its size.
//! - A view-only device (`input: false`) is only ever a viewer.
//! - In memory only: a daemon restart forgets it, and lets go of what the
//!   last daemon held in tmux (`sweep`).
//!
//! The tmux side: a page or device holding the pane pins its window to its
//! size (`tmux::resize_window`) and holds the session's keys (the
//! `rlord-held` key table, `tmux::hold_keys`); the terminal holding it
//! unpins it (`tmux::auto_size_window`), so it follows the terminal's own
//! client; free, its size is kept. Every view client attaches with
//! `ignore-size`, so nothing else sizes a window.
//!
//! Who learns it: the snapshot's rows and overlord carry `control`
//! (`entry`), a change moves `rev`, and the plugin gets the whole map of
//! held panes on its `web.subscribe` stream (`{"notify":"control",
//! "sessions"}`), since it enforces typing and sizes the viewers' clients.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::tmux;
use rlord_core::types::Status;

use crate::{methods, overlord, Daemon};

/// How long a holder that left keeps control (ms).
pub const GRACE_MS: i64 = 30_000;

/// How often control is looked at while any pane is held or viewed: the
/// grace running out, the terminal coming or going.
const TICK: Duration = Duration::from_secs(1);

/// The smallest and largest size a screen may give (the local pane
/// socket's bounds).
const MIN_SIZE: u16 = 2;
const MAX_SIZE: u16 = 1000;

/// A screen that shows panes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Surface {
    /// The computer's terminal: rlord's TUI and the user's tmux client,
    /// the overlord overlay.
    Terminal,
    /// A local web page, by the plugin's id for it.
    Page(String),
    /// A paired device, by its id.
    Device(String),
}

impl Surface {
    /// `terminal`, `page:<id>` or `device:<id>`.
    pub fn parse(s: &str) -> Option<Surface> {
        if s == "terminal" {
            return Some(Surface::Terminal);
        }
        let (kind, id) = s.split_once(':')?;
        if id.is_empty() || id.len() > 80 {
            return None;
        }
        match kind {
            "page" => Some(Surface::Page(id.to_string())),
            "device" => Some(Surface::Device(id.to_string())),
            _ => None,
        }
    }

    /// Its wire form (`parse`'s input).
    pub fn key(&self) -> String {
        match self {
            Surface::Terminal => "terminal".into(),
            Surface::Page(id) => format!("page:{id}"),
            Surface::Device(id) => format!("device:{id}"),
        }
    }

    /// `terminal`, `web` or `phone`.
    pub fn by(&self) -> &'static str {
        match self {
            Surface::Terminal => "terminal",
            Surface::Page(_) => "web",
            Surface::Device(_) => "phone",
        }
    }
}

/// A screen asking: which, what it is called, and whether it may type (a
/// view-only device may not).
#[derive(Clone, Debug)]
pub struct Who {
    pub surface: Surface,
    pub name: String,
    pub input: bool,
}

/// A pane's holder.
#[derive(Clone, Debug, PartialEq)]
pub struct Holder {
    pub surface: Surface,
    pub name: String,
    /// Its terminal's size (the terminal's: its window's, as last seen).
    pub cols: u16,
    pub rows: u16,
    pub since: i64,
    /// Since when it is gone (None: it shows the pane).
    pub away: Option<i64>,
}

/// A page or device showing a pane (the holder too, while it shows it).
#[derive(Clone, Debug, PartialEq)]
struct Viewer {
    who: Surface,
    name: String,
    input: bool,
    cols: u16,
    rows: u16,
    opened: i64,
}

/// One session's pane.
#[derive(Clone, Debug, PartialEq)]
pub struct Pane {
    pub tmux: String,
    pub overlord: bool,
    pub holder: Option<Holder>,
    viewers: Vec<Viewer>,
}

/// What tmux should do with a window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Size {
    /// Pinned to a page's or device's size.
    Pin(u16, u16),
    /// Following the terminal's client.
    Unpin,
    /// Left as it is (a free pane keeps its size).
    Keep,
}

/// What tmux should show for one session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Want {
    pub tmux: String,
    pub size: Size,
    /// Held for another screen: the line a key press and the hint bar say.
    pub held: Option<String>,
}

/// The state: every pane that a page or device shows or holds (and a
/// terminal-held one while a page or device views it). A pane nobody but
/// the terminal looks at has no entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Control {
    panes: BTreeMap<String, Pane>,
    /// The take key as it reads (`⌥t`), for the held line.
    pub key: String,
}

impl Control {
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    pub fn pane(&self, id: &str) -> Option<&Pane> {
        self.panes.get(id)
    }

    fn pane_mut(&mut self, id: &str, tmux: &str, overlord: bool) -> &mut Pane {
        let p = self.panes.entry(id.to_string()).or_insert_with(|| Pane {
            tmux: tmux.to_string(),
            overlord,
            holder: None,
            viewers: vec![],
        });
        p.tmux = tmux.to_string();
        p.overlord = overlord;
        p
    }

    /// `who` opens session `id`'s pane at its size; `terminal_here`: the
    /// terminal shows it now. Whether `who` holds it: a free pane goes to
    /// its opener (the terminal, if it is there, being the opener before
    /// any page or device), one held by a present screen stays held, and
    /// a holder opening again (within its grace) is back.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        &mut self,
        id: &str,
        tmux: &str,
        overlord: bool,
        who: &Who,
        cols: u16,
        rows: u16,
        terminal_here: bool,
        now: i64,
    ) -> bool {
        let host_name = terminal_name();
        let p = self.pane_mut(id, tmux, overlord);
        if who.surface != Surface::Terminal {
            p.viewers.retain(|v| v.who != who.surface);
            p.viewers.push(Viewer {
                who: who.surface.clone(),
                name: who.name.clone(),
                input: who.input,
                cols,
                rows,
                opened: now,
            });
        }
        match &mut p.holder {
            Some(h) if h.surface == who.surface => {
                h.away = None;
                if who.surface != Surface::Terminal {
                    (h.cols, h.rows) = (cols, rows);
                }
                true
            }
            Some(_) => false,
            None if terminal_here && who.surface != Surface::Terminal => {
                p.holder = Some(Holder {
                    surface: Surface::Terminal,
                    name: host_name,
                    cols: 0,
                    rows: 0,
                    since: now,
                    away: None,
                });
                false
            }
            None if who.input => {
                p.holder = Some(Holder {
                    surface: who.surface.clone(),
                    name: who.name.clone(),
                    cols,
                    rows,
                    since: now,
                    away: None,
                });
                true
            }
            None => false,
        }
    }

    /// `who` takes session `id`'s pane (at its size, or the size it
    /// opened it at when not given). Running it twice is the same as once.
    /// `showing`: `who` shows the pane now (a page or device: it opened
    /// it; the terminal: `terminal_here`); a holder that doesn't is away
    /// from the start. Refused for a screen that may not type.
    #[allow(clippy::too_many_arguments)]
    pub fn take(
        &mut self,
        id: &str,
        tmux: &str,
        overlord: bool,
        who: &Who,
        size: Option<(u16, u16)>,
        terminal_here: bool,
        now: i64,
    ) -> Result<(), &'static str> {
        if !who.input {
            return Err("this device is view-only: it can't take a pane");
        }
        let p = self.pane_mut(id, tmux, overlord);
        let shown = p.viewers.iter().find(|v| v.who == who.surface);
        let showing = match who.surface {
            Surface::Terminal => terminal_here,
            _ => shown.is_some(),
        };
        let (cols, rows) = size.or(shown.map(|v| (v.cols, v.rows))).unwrap_or((0, 0));
        let since = match &p.holder {
            Some(h) if h.surface == who.surface => h.since,
            _ => now,
        };
        p.holder = Some(Holder {
            surface: who.surface.clone(),
            name: who.name.clone(),
            cols,
            rows,
            since,
            away: (!showing).then_some(now),
        });
        self.prune(id);
        Ok(())
    }

    /// `who`'s terminal for session `id`'s pane is now `cols`×`rows`.
    /// Whether it holds the pane (then the window follows).
    pub fn resize(&mut self, id: &str, who: &Surface, cols: u16, rows: u16) -> bool {
        let Some(p) = self.panes.get_mut(id) else {
            return false;
        };
        if let Some(v) = p.viewers.iter_mut().find(|v| &v.who == who) {
            (v.cols, v.rows) = (cols, rows);
        }
        match &mut p.holder {
            Some(h) if &h.surface == who => {
                (h.cols, h.rows) = (cols, rows);
                true
            }
            _ => false,
        }
    }

    /// `who` stopped showing session `id`'s pane: a holder is away from
    /// now (`GRACE_MS` later control moves on, `tick`).
    pub fn closed(&mut self, id: &str, who: &Surface, now: i64) {
        let Some(p) = self.panes.get_mut(id) else {
            return;
        };
        p.viewers.retain(|v| &v.who != who);
        if let Some(h) = p.holder.as_mut().filter(|h| &h.surface == who) {
            h.away.get_or_insert(now);
        }
        self.prune(id);
    }

    /// `who` is gone everywhere (the plugin went, a device was unpaired).
    pub fn gone(&mut self, who: &Surface, now: i64) {
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        for id in ids {
            self.closed(&id, who, now);
        }
    }

    /// Session `id` stopped running: nothing to hold.
    pub fn ended(&mut self, id: &str) {
        self.panes.remove(id);
    }

    /// The terminal holding session `id` sees its window at this size.
    pub fn observed(&mut self, id: &str, cols: u16, rows: u16) {
        if let Some(h) = self
            .panes
            .get_mut(id)
            .and_then(|p| p.holder.as_mut())
            .filter(|h| h.surface == Surface::Terminal)
        {
            (h.cols, h.rows) = (cols, rows);
        }
    }

    /// Time passes: the terminal coming to or leaving each pane
    /// (`terminal_here`, by session id), holders whose grace ran out
    /// replaced (`used`: each screen's last use, ms, for "the one used
    /// last"), entries nobody needs dropped.
    pub fn tick(
        &mut self,
        now: i64,
        terminal_here: &dyn Fn(&str) -> bool,
        used: &dyn Fn(&Surface) -> Option<i64>,
    ) {
        let host = terminal_name();
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        for id in ids {
            let here = terminal_here(&id);
            let p = self.panes.get_mut(&id).expect("listed");
            match p.holder.as_mut() {
                Some(h) if h.surface == Surface::Terminal => {
                    if here {
                        h.away = None;
                    } else {
                        h.away.get_or_insert(now);
                    }
                }
                // A free pane the terminal came to: its opener's.
                None if here => {
                    p.holder = Some(Holder {
                        surface: Surface::Terminal,
                        name: host.clone(),
                        cols: 0,
                        rows: 0,
                        since: now,
                        away: None,
                    });
                }
                _ => {}
            }
            let expired = p
                .holder
                .as_ref()
                .and_then(|h| h.away)
                .is_some_and(|at| now - at >= GRACE_MS);
            if expired {
                let old = p.holder.take().map(|h| h.surface);
                p.holder = if here && old != Some(Surface::Terminal) {
                    Some(Holder {
                        surface: Surface::Terminal,
                        name: host.clone(),
                        cols: 0,
                        rows: 0,
                        since: now,
                        away: None,
                    })
                } else {
                    p.viewers
                        .iter()
                        .filter(|v| v.input && Some(&v.who) != old.as_ref())
                        .max_by_key(|v| (used(&v.who), v.opened))
                        .map(|v| Holder {
                            surface: v.who.clone(),
                            name: v.name.clone(),
                            cols: v.cols,
                            rows: v.rows,
                            since: now,
                            away: None,
                        })
                };
            }
            self.prune(&id);
        }
    }

    /// Drop session `id`'s entry when nobody but (at most) the terminal is
    /// in it: free with no viewer, or the terminal's with no viewer.
    fn prune(&mut self, id: &str) {
        let drop = self.panes.get(id).is_some_and(|p| {
            p.viewers.is_empty()
                && p.holder
                    .as_ref()
                    .is_none_or(|h| h.surface == Surface::Terminal && h.away.is_none())
        });
        if drop {
            self.panes.remove(id);
        }
    }

    /// What tmux should show, per session.
    pub fn wants(&self) -> BTreeMap<String, Want> {
        self.panes
            .iter()
            .map(|(id, p)| {
                let (size, held) = match &p.holder {
                    Some(h) if h.surface == Surface::Terminal => (Size::Unpin, None),
                    Some(h) => (
                        if h.cols >= MIN_SIZE && h.rows >= MIN_SIZE {
                            Size::Pin(h.cols, h.rows)
                        } else {
                            Size::Keep
                        },
                        Some(held_line(h, &self.key)),
                    ),
                    None => (Size::Keep, None),
                };
                (
                    id.clone(),
                    Want {
                        tmux: p.tmux.clone(),
                        size,
                        held,
                    },
                )
            })
            .collect()
    }

    /// Session `id`'s `control`, as the snapshot carries it: null, or
    /// `{holder, by, name, away, cols, rows}` (`cols`, `rows` null when
    /// not known).
    pub fn entry(&self, id: &str) -> Value {
        match self.panes.get(id).and_then(|p| p.holder.as_ref()) {
            None => Value::Null,
            Some(h) => {
                let n = |v: u16| if v == 0 { Value::Null } else { json!(v) };
                json!({
                    "holder": h.surface.key(),
                    "by": h.surface.by(),
                    "name": h.name,
                    "away": h.away.is_some(),
                    "cols": n(h.cols),
                    "rows": n(h.rows),
                })
            }
        }
    }

    /// The plugin's line: every held pane, with its tmux session.
    pub fn notify_line(&self) -> String {
        let sessions: serde_json::Map<String, Value> = self
            .panes
            .iter()
            .filter(|(_, p)| p.holder.is_some())
            .map(|(id, p)| {
                let mut e = self.entry(id);
                e["tmux_session"] = json!(p.tmux);
                (id.clone(), e)
            })
            .collect();
        json!({"notify": "control", "sessions": sessions}).to_string()
    }
}

/// "Controlled by <name> · <key> to take over" (", away" when the holder
/// left).
fn held_line(h: &Holder, key: &str) -> String {
    let away = if h.away.is_some() { " (away)" } else { "" };
    let take = if key.is_empty() {
        String::new()
    } else {
        format!(" · {key} to take over")
    };
    // (A name is shown in tmux's status line: no `#`, which starts a
    // format there.)
    let name: String = h.name.chars().filter(|c| *c != '#').collect();
    format!("Controlled by {name}{away}{take}")
}

/// What the terminal is called on other screens.
fn terminal_name() -> String {
    format!("{}'s terminal", host_name())
}

/// This computer's name, short (no `.local`).
pub fn host_name() -> String {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        let mut buf = [0u8; 256];
        // Safety: gethostname writes at most `len` bytes into `buf`.
        let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let s = if r == 0 {
            String::from_utf8_lossy(&buf[..end]).into_owned()
        } else {
            String::new()
        };
        let s = s.split('.').next().unwrap_or("").trim().to_string();
        if s.is_empty() {
            "this computer".into()
        } else {
            s
        }
    })
    .clone()
}

// ---- the daemon's side ----

/// Control as the daemon keeps it: the state, what was last applied to
/// tmux, and whether the ticker runs.
#[derive(Default)]
pub struct Shared {
    state: Mutex<Inner>,
    ticking: AtomicBool,
}

#[derive(Default)]
struct Inner {
    control: Control,
    applied: BTreeMap<String, Want>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Session `id`'s `control` (null: free, or nobody else looks).
    pub fn entry(&self, id: &str) -> Value {
        self.lock().control.entry(id)
    }

    /// The plugin's control line.
    pub(crate) fn notify_line(&self) -> String {
        self.lock().control.notify_line()
    }
}

/// A pane's target, resolved: `overlord` (the active one) or a session
/// reference.
struct Target {
    id: String,
    name: String,
    overlord: bool,
    tmux: Option<String>,
    running: bool,
}

fn resolve(d: &Daemon, target: &str) -> Result<Target> {
    let s = {
        let db = d.db();
        if target == "overlord" {
            overlord::session(&db)?.context("no overlord is set up yet")?
        } else {
            db.resolve_ref(target)?
        }
    };
    // (tmux, without the DB guard.)
    let running =
        s.status != Status::Stopped && s.tmux_session.as_deref().is_some_and(tmux::has_session);
    Ok(Target {
        overlord: overlord::is_overlord_role(&s.role),
        id: s.id,
        name: s.name,
        tmux: s.tmux_session,
        running,
    })
}

/// Whether the terminal shows tmux session `tmux` now: a person's client
/// on it, or (the overlord) the overlay open.
fn terminal_shows(d: &Daemon, clients: &[tmux::Client], tmux: &str, overlord: bool) -> bool {
    clients.iter().any(|c| c.session == tmux)
        || (overlord && d.overlay.lock().expect("ui mutex").is_some())
}

/// The take key as it reads (`⌥t`), from config (empty: none).
fn take_label(d: &Daemon) -> String {
    let k = d.config().tmux.take.trim().to_string();
    if k.is_empty() {
        k
    } else {
        tmux::key_label(&k)
    }
}

/// A screen's name: the terminal's and a page's from this computer's name,
/// a device's from its pairing (as the plugin reports it).
pub(crate) fn who(d: &Daemon, surface: Surface) -> Who {
    match &surface {
        Surface::Terminal => Who {
            name: terminal_name(),
            surface,
            input: true,
        },
        Surface::Page(_) => Who {
            name: format!("a browser on {}", host_name()),
            surface,
            input: true,
        },
        Surface::Device(id) => {
            let me = crate::relay::paired(d).into_iter().find(|p| &p.id == id);
            Who {
                name: me
                    .as_ref()
                    .map(|p| p.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "a phone".into()),
                // Only the user makes a device view-only, and the plugin
                // reports it: one it doesn't list (no report yet) may type,
                // as new devices may.
                input: me.is_none_or(|p| p.input),
                surface,
            }
        }
    }
}

/// Run `f` on the state, apply what it changed to tmux, and tell everyone
/// when the public part moved (a change moves `rev`; the plugin gets the
/// map). Returns `f`'s answer.
fn with<T>(d: &Daemon, f: impl FnOnce(&mut Control) -> T) -> T {
    let mut inner = d.control.lock();
    let before = inner.control.notify_line();
    let entries_before = public(&inner.control);
    inner.control.key = take_label(d);
    let r = f(&mut inner.control);
    apply(&mut inner);
    let after = inner.control.notify_line();
    let moved = entries_before != public(&inner.control);
    let busy = !inner.control.is_empty();
    // (Under the lock: two changes reach the plugin in their order.)
    if moved || before != after {
        d.changed();
        d.notify_line(after);
    }
    drop(inner);
    if busy {
        ensure_ticker(d);
    }
    r
}

/// The part of the state others see (each held pane's entry).
fn public(c: &Control) -> Vec<(String, Value)> {
    c.panes.keys().map(|id| (id.clone(), c.entry(id))).collect()
}

/// Bring tmux in line with the state (only what changed since last time).
/// Under the state's lock, so two changes apply in order. Failures are
/// logged: a session that just went has nothing to apply.
fn apply(inner: &mut Inner) {
    let now = inner.control.wants();
    let mut ids: Vec<&String> = now.keys().chain(inner.applied.keys()).collect();
    ids.sort();
    ids.dedup();
    let mut keys_bound = false;
    for id in ids {
        let (was, will) = (inner.applied.get(id), now.get(id));
        let tmux_name = will.or(was).map(|w| w.tmux.clone()).unwrap_or_default();
        let held_was = was.and_then(|w| w.held.clone());
        let held_now = will.and_then(|w| w.held.clone());
        if held_was != held_now {
            let r = match &held_now {
                Some(line) => {
                    if !keys_bound {
                        keys_bound = true;
                        if let Err(e) = bind_held() {
                            tracing::debug!(error = %e, "binding the held key table");
                        }
                    }
                    tmux::hold_keys(&tmux_name, line)
                }
                None => tmux::release_keys(&tmux_name),
            };
            if let Err(e) = r {
                tracing::debug!(session = %tmux_name, error = %e, "holding or releasing keys");
            }
        }
        let size_was = was.map(|w| &w.size);
        let size_now = will.map(|w| &w.size);
        let r = match (size_was, size_now) {
            (a, Some(Size::Pin(c, r))) if a != Some(&Size::Pin(*c, *r)) => {
                tmux::resize_window(&tmux_name, *c, *r)
            }
            (Some(Size::Pin(..) | Size::Keep), Some(Size::Unpin) | None) => {
                tmux::auto_size_window(&tmux_name)
            }
            _ => Ok(()),
        };
        if let Err(e) = r {
            tracing::debug!(session = %tmux_name, error = %e, "sizing a held window");
        }
    }
    inner.applied = now;
}

/// The held key table, from config: the navigation keys and the take key.
fn bind_held() -> Result<()> {
    let cfg = rlord_core::config::Config::load().unwrap_or_default();
    // (A debug build's tests point the keys at a stand-in: the test
    // binary is no `rlord`.)
    let bin = std::env::var("RLORD_TEST_HELD_BIN")
        .ok()
        .filter(|_| cfg!(debug_assertions))
        .unwrap_or_else(methods::rlord_bin);
    let mut keys: Vec<(String, String)> = cfg
        .tmux
        .bindings()
        .into_iter()
        .map(|(k, verb)| {
            (
                k.to_string(),
                format!("{} nav {verb}", tmux::shell_quote(&bin)),
            )
        })
        .collect();
    let take = cfg.tmux.take.trim();
    if !take.is_empty() {
        keys.push((
            take.to_string(),
            format!(
                "{} control take '#{{session_name}}'",
                tmux::shell_quote(&bin)
            ),
        ));
    }
    tmux::bind_held_table(&keys)
}

/// Start the ticker if it isn't running: once a second while any pane is
/// held or viewed, then it stops.
fn ensure_ticker(d: &Daemon) {
    if d.control.ticking.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(d) = d.me.upgrade() else {
        d.control.ticking.store(false, Ordering::SeqCst);
        return;
    };
    std::thread::spawn(move || loop {
        std::thread::sleep(TICK);
        tick(&d);
        if d.control.lock().control.is_empty() {
            d.control.ticking.store(false, Ordering::SeqCst);
            // (A pane opened just now: keep going.)
            if d.control.lock().control.is_empty() || d.control.ticking.swap(true, Ordering::SeqCst)
            {
                return;
            }
        }
    });
}

/// Look at the terminal and the clock now (the ticker, an overlay opening
/// or closing, a terminal switching session).
pub fn tick(d: &Daemon) {
    if d.control.lock().control.is_empty() {
        return;
    }
    let clients = tmux::list_clients();
    let overlay = d.overlay.lock().expect("ui mutex").is_some();
    let panes: Vec<(String, String, bool)> = {
        let c = &d.control.lock().control;
        c.panes
            .iter()
            .map(|(id, p)| (id.clone(), p.tmux.clone(), p.overlord))
            .collect()
    };
    let mut here = std::collections::HashSet::new();
    let mut gone = vec![];
    let mut sizes = vec![];
    for (id, t, o) in &panes {
        if !tmux::has_session(t) {
            gone.push(id.clone());
            continue;
        }
        if clients.iter().any(|c| &c.session == t) || (*o && overlay) {
            here.insert(id.clone());
        }
        sizes.push((id.clone(), tmux::window_size(t)));
    }
    let used = crate::ui::surface_uses(d);
    let now = rlord_core::types::now_ms();
    with(d, |c| {
        for id in &gone {
            c.ended(id);
        }
        c.tick(now, &|id| here.contains(id), &|s| {
            used.get(s).copied().flatten()
        });
        for (id, size) in sizes {
            if let Some((w, h)) = size {
                c.observed(&id, w, h);
            }
        }
    });
}

/// A size from a call's parameters, checked.
fn size_of(p: &Value) -> Result<Option<(u16, u16)>> {
    let (c, r) = (p["cols"].as_u64(), p["rows"].as_u64());
    match (c, r) {
        (None, None) => Ok(None),
        (Some(c), Some(r)) => {
            let ok = |v: u64| (u64::from(MIN_SIZE)..=u64::from(MAX_SIZE)).contains(&v);
            if !ok(c) || !ok(r) {
                bail!(crate::web::BadRequest(format!(
                    "a pane's size is {MIN_SIZE} to {MAX_SIZE} columns and rows"
                )));
            }
            Ok(Some((c as u16, r as u16)))
        }
        _ => bail!(crate::web::BadRequest(
            "a size needs both cols and rows".into()
        )),
    }
}

/// `pane.open` from a device, and `web.pane_open {target, surface, cols,
/// rows}` from a local page: the pane's session, whether `who` holds it
/// (a free one is its opener's, unless `who` may not type), and the size
/// to attach at (its own when it holds the pane, else the window's).
/// Starts nothing: a session that isn't running says so and changes
/// nothing.
pub(crate) fn open(d: &Daemon, who: Who, params: &Value) -> Result<Value> {
    let target = methods::p_str(params, "target")?;
    let size = size_of(params)?
        .ok_or_else(|| crate::web::BadRequest("pane.open needs cols and rows".into()))?;
    let t = resolve(d, target)?;
    let tmux_name = t.tmux.clone().filter(|_| t.running);
    let mut out = json!({
        "id": t.id,
        "name": t.name,
        "overlord": t.overlord,
        "tmux_session": t.tmux,
        "running": t.running,
        "control": false,
        "cols": size.0,
        "rows": size.1,
        "window": null,
    });
    let Some(tmux_name) = tmux_name else {
        return Ok(out);
    };
    let here = terminal_shows(d, &tmux::list_clients(), &tmux_name, t.overlord);
    let now = rlord_core::types::now_ms();
    let control = with(d, |c| {
        c.open(
            &t.id, &tmux_name, t.overlord, &who, size.0, size.1, here, now,
        )
    });
    let window = tmux::window_size(&tmux_name);
    out["control"] = json!(control);
    if let Some((w, h)) = window {
        out["window"] = json!({"cols": w, "rows": h});
        if !control {
            (out["cols"], out["rows"]) = (json!(w), json!(h));
        }
    }
    Ok(out)
}

/// `web.pane_size {target, surface, cols, rows}`: the screen's terminal
/// changed size; if it holds the pane, the window follows.
pub(crate) fn size(d: &Daemon, params: &Value) -> Result<Value> {
    let surface = surface_of(params)?;
    let id = resolve(d, methods::p_str(params, "target")?)?.id;
    let (cols, rows) = size_of(params)?
        .ok_or_else(|| crate::web::BadRequest("pane_size needs cols and rows".into()))?;
    let control = with(d, |c| c.resize(&id, &surface, cols, rows));
    Ok(json!({"control": control}))
}

/// `web.pane_closed {target, surface}`: the screen stopped showing the
/// pane (a holder is away for `GRACE_MS`).
pub(crate) fn closed(d: &Daemon, params: &Value) -> Result<Value> {
    let surface = surface_of(params)?;
    let id = resolve(d, methods::p_str(params, "target")?)?.id;
    let now = rlord_core::types::now_ms();
    with(d, |c| c.closed(&id, &surface, now));
    Ok(json!({"ok": true}))
}

/// `pane.take {target, cols, rows}` from a page or device, and
/// `control.take {target}` from the terminal: `who` holds the pane now.
/// Running it twice is the same as once.
pub(crate) fn take(d: &Daemon, who: Who, params: &Value) -> Result<Value> {
    let target = methods::p_str(params, "target")?;
    let size = size_of(params)?;
    if !who.input {
        bail!(crate::web::NotAllowed(
            "this device is view-only: it can't take a pane".into()
        ));
    }
    let t = resolve(d, target)?;
    let Some(tmux_name) = t.tmux.clone().filter(|_| t.running) else {
        bail!("{} isn't running", t.name);
    };
    let here = terminal_shows(d, &tmux::list_clients(), &tmux_name, t.overlord);
    let now = rlord_core::types::now_ms();
    let r = with(d, |c| {
        let r = c.take(&t.id, &tmux_name, t.overlord, &who, size, here, now);
        let h = c.pane(&t.id).and_then(|p| p.holder.clone());
        r.map(|_| h)
    });
    let h = match r {
        Ok(h) => h,
        Err(e) => bail!(crate::web::NotAllowed(e.into())),
    };
    let (mut cols, mut rows) = h.map(|h| (h.cols, h.rows)).unwrap_or((0, 0));
    if cols == 0 {
        // The terminal's: its window's size.
        (cols, rows) = tmux::window_size(&tmux_name).unwrap_or((0, 0));
    }
    Ok(json!({"control": true, "cols": cols, "rows": rows}))
}

/// `control.get {target}`: the pane's `control` (null: free, or nobody
/// else looks), for the overlay.
pub(crate) fn get(d: &Daemon, params: &Value) -> Result<Value> {
    let t = resolve(d, methods::p_str(params, "target")?)?;
    Ok(json!({"id": t.id, "control": d.control.entry(&t.id)}))
}

/// `surface` from a plugin call.
fn surface_of(params: &Value) -> Result<Surface> {
    let s = methods::p_str(params, "surface")?;
    Surface::parse(s).ok_or_else(|| crate::web::BadRequest(format!("unknown surface {s:?}")).into())
}

/// The plugin's stream ended: its pages and devices show nothing now
/// (holders are away; a restarted plugin's screens open again).
pub(crate) fn plugin_gone(d: &Daemon) {
    let screens: Vec<Surface> = {
        let c = &d.control.lock().control;
        let mut v: Vec<Surface> = c
            .panes
            .values()
            .flat_map(|p| {
                p.viewers
                    .iter()
                    .map(|v| v.who.clone())
                    .chain(p.holder.iter().map(|h| h.surface.clone()))
            })
            .filter(|s| *s != Surface::Terminal)
            .collect();
        v.sort();
        v.dedup();
        v
    };
    if screens.is_empty() {
        return;
    }
    let now = rlord_core::types::now_ms();
    with(d, |c| {
        for s in &screens {
            c.gone(s, now);
        }
    });
}

/// Only these devices are still paired: any other's panes are let go of at
/// once (an unpaired device is no screen any more).
pub(crate) fn keep_devices(d: &Daemon, paired: &[String]) {
    let unpaired: Vec<Surface> = {
        let c = &d.control.lock().control;
        let mut v: Vec<Surface> = c
            .panes
            .values()
            .flat_map(|p| {
                p.viewers
                    .iter()
                    .map(|v| v.who.clone())
                    .chain(p.holder.iter().map(|h| h.surface.clone()))
            })
            .filter(|s| matches!(s, Surface::Device(id) if !paired.contains(id)))
            .collect();
        v.sort();
        v.dedup();
        v
    };
    if unpaired.is_empty() {
        return;
    }
    // Away long enough already: the next tick passes control on.
    let then = rlord_core::types::now_ms() - GRACE_MS;
    with(d, |c| {
        for s in &unpaired {
            c.gone(s, then);
        }
    });
}

/// At daemon start: what a previous daemon held in tmux is let go of (a
/// restart forgets who held what: every pane is free again).
pub fn sweep() {
    for s in tmux::held_sessions() {
        if let Err(e) = tmux::release_keys(&s) {
            tracing::debug!(session = %s, error = %e, "letting go of a held session");
        }
        let _ = tmux::auto_size_window(&s);
    }
}

/// After the profile or the keys were re-applied (they reset a session's
/// key table): hold again what is held, and remake the held table.
pub(crate) fn reassert(d: &Daemon) {
    let mut inner = d.control.lock();
    let held: Vec<(String, String)> = inner
        .applied
        .values()
        .filter_map(|w| w.held.clone().map(|l| (w.tmux.clone(), l)))
        .collect();
    if held.is_empty() {
        return;
    }
    if let Err(e) = bind_held() {
        tracing::debug!(error = %e, "binding the held key table");
    }
    for (t, line) in held {
        let _ = tmux::hold_keys(&t, &line);
    }
    // (Nothing else moved.)
    inner.applied = inner.control.wants();
}

/// Tests: `surface` opens session `id`'s pane (tmux session `tmux`) as
/// `pane.open` would, without asking tmux whether it runs.
#[doc(hidden)]
pub fn test_open(d: &Daemon, id: &str, tmux: &str, surface: &str, cols: u16, rows: u16) -> bool {
    let who = who(d, Surface::parse(surface).expect("a surface"));
    let now = rlord_core::types::now_ms();
    with(d, |c| c.open(id, tmux, false, &who, cols, rows, false, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(id: &str) -> Who {
        Who {
            surface: Surface::Page(id.into()),
            name: "a browser on mac".into(),
            input: true,
        }
    }

    fn phone(id: &str, input: bool) -> Who {
        Who {
            surface: Surface::Device(id.into()),
            name: format!("{id}'s phone"),
            input,
        }
    }

    fn term() -> Who {
        Who {
            surface: Surface::Terminal,
            name: "mac's terminal".into(),
            input: true,
        }
    }

    fn holder(c: &Control, id: &str) -> Option<String> {
        c.pane(id)
            .and_then(|p| p.holder.as_ref())
            .map(|h| h.surface.key())
    }

    fn never(_: &str) -> bool {
        false
    }

    fn no_use(_: &Surface) -> Option<i64> {
        None
    }

    #[test]
    fn a_free_pane_goes_to_its_opener() {
        let mut c = Control::default();
        assert!(c.open("s1", "t1", false, &page("p1"), 80, 24, false, 0));
        assert_eq!(holder(&c, "s1").as_deref(), Some("page:p1"));
        assert_eq!(
            c.wants()["s1"],
            Want {
                tmux: "t1".into(),
                size: Size::Pin(80, 24),
                held: Some("Controlled by a browser on mac".into()),
            }
        );
        let e = c.entry("s1");
        assert_eq!(e["holder"], "page:p1");
        assert_eq!(e["by"], "web");
        assert_eq!(
            (e["cols"].clone(), e["rows"].clone()),
            (json!(80), json!(24))
        );
        // The terminal showing it when a phone opens it: the terminal's,
        // and the window follows it.
        assert!(!c.open("s2", "t2", false, &phone("a", true), 50, 30, true, 0));
        assert_eq!(holder(&c, "s2").as_deref(), Some("terminal"));
        assert_eq!(c.wants()["s2"].size, Size::Unpin);
        assert_eq!(c.wants()["s2"].held, None);
        // A view-only device never holds one, even free: it stays free and
        // keeps its size.
        assert!(!c.open("s3", "t3", false, &phone("b", false), 50, 30, false, 0));
        assert_eq!(holder(&c, "s3"), None);
        assert_eq!(c.wants()["s3"].size, Size::Keep);
        assert!(c.entry("s3").is_null());
    }

    #[test]
    fn an_opener_never_takes() {
        let mut c = Control::default();
        c.open("s1", "t1", false, &page("p1"), 80, 24, false, 0);
        assert!(!c.open("s1", "t1", false, &phone("a", true), 50, 30, false, 1));
        assert!(!c.open("s1", "t1", false, &page("p2"), 100, 40, false, 2));
        assert_eq!(holder(&c, "s1").as_deref(), Some("page:p1"));
        // The window stays the holder's.
        assert_eq!(c.wants()["s1"].size, Size::Pin(80, 24));
        // A viewer's resize changes nothing; the holder's pins again.
        assert!(!c.resize("s1", &Surface::Page("p2".into()), 90, 40));
        assert!(c.resize("s1", &Surface::Page("p1".into()), 81, 25));
        assert_eq!(c.wants()["s1"].size, Size::Pin(81, 25));
    }

    #[test]
    fn take_swaps_the_holder() {
        let mut c = Control {
            key: "⌥t".into(),
            ..Default::default()
        };
        c.open("s1", "t1", false, &page("p1"), 80, 24, false, 0);
        c.open("s1", "t1", false, &phone("a", true), 50, 30, false, 1);
        // At the size it opened at, or a new one.
        c.take("s1", "t1", false, &phone("a", true), None, false, 2)
            .unwrap();
        assert_eq!(holder(&c, "s1").as_deref(), Some("device:a"));
        assert_eq!(c.wants()["s1"].size, Size::Pin(50, 30));
        assert_eq!(
            c.wants()["s1"].held.as_deref(),
            Some("Controlled by a's phone · ⌥t to take over")
        );
        // Twice is once.
        let before = c.clone();
        c.take("s1", "t1", false, &phone("a", true), None, false, 3)
            .unwrap();
        assert_eq!(c, before);
        c.take("s1", "t1", false, &page("p1"), Some((120, 40)), false, 4)
            .unwrap();
        assert_eq!(c.wants()["s1"].size, Size::Pin(120, 40));
        // The terminal takes: the window follows it, its keys are its own.
        c.take("s1", "t1", false, &term(), None, true, 5).unwrap();
        assert_eq!(holder(&c, "s1").as_deref(), Some("terminal"));
        assert_eq!(c.wants()["s1"].size, Size::Unpin);
        assert_eq!(c.wants()["s1"].held, None);
        // A view-only device may not.
        assert!(c
            .take("s1", "t1", false, &phone("b", false), None, false, 6)
            .is_err());
        assert_eq!(holder(&c, "s1").as_deref(), Some("terminal"));
    }

    #[test]
    fn away_keeps_control_for_the_grace() {
        let mut c = Control::default();
        c.open("s1", "t1", false, &phone("a", true), 50, 30, false, 0);
        c.open("s1", "t1", false, &page("p1"), 80, 24, false, 0);
        c.closed("s1", &Surface::Device("a".into()), 1_000);
        assert_eq!(c.entry("s1")["away"], true);
        // Nothing changes size while it is away.
        assert_eq!(c.wants()["s1"].size, Size::Pin(50, 30));
        c.tick(1_000 + GRACE_MS - 1, &never, &no_use);
        assert_eq!(holder(&c, "s1").as_deref(), Some("device:a"));
        // Back within the grace: as if it never left.
        assert!(c.open("s1", "t1", false, &phone("a", true), 50, 30, false, 20_000));
        assert_eq!(c.entry("s1")["away"], false);
        c.tick(200_000, &never, &no_use);
        assert_eq!(holder(&c, "s1").as_deref(), Some("device:a"));
        // The terminal holding and leaving: away too, then back.
        c.take("s1", "t1", false, &term(), None, true, 300_000)
            .unwrap();
        c.tick(301_000, &never, &no_use);
        assert_eq!(c.entry("s1")["away"], true);
        c.tick(302_000, &|_| true, &no_use);
        assert_eq!(c.entry("s1")["away"], false);
    }

    #[test]
    fn release_prefers_the_terminal_then_the_last_used() {
        let mut c = Control::default();
        c.open("s1", "t1", false, &phone("a", true), 50, 30, false, 0);
        c.open("s1", "t1", false, &page("p1"), 80, 24, false, 1);
        c.open("s1", "t1", false, &page("p2"), 90, 30, false, 2);
        c.open("s1", "t1", false, &phone("ro", false), 40, 20, false, 3);
        c.closed("s1", &Surface::Device("a".into()), 10);
        // The terminal shows it: the terminal's.
        let mut t = c.clone();
        t.tick(10 + GRACE_MS, &|_| true, &no_use);
        assert_eq!(holder(&t, "s1").as_deref(), Some("terminal"));
        assert_eq!(t.wants()["s1"].size, Size::Unpin);
        // Else the one used last (never the view-only one), at its size.
        let used = |s: &Surface| match s {
            Surface::Page(p) if p == "p1" => Some(500),
            Surface::Page(p) if p == "p2" => Some(400),
            Surface::Device(_) => Some(900),
            _ => None,
        };
        let mut u = c.clone();
        u.tick(10 + GRACE_MS, &never, &used);
        assert_eq!(holder(&u, "s1").as_deref(), Some("page:p1"));
        assert_eq!(u.wants()["s1"].size, Size::Pin(80, 24));
        // Nobody used either: the newest.
        c.tick(10 + GRACE_MS, &never, &no_use);
        assert_eq!(holder(&c, "s1").as_deref(), Some("page:p2"));
    }

    #[test]
    fn nobody_left_means_free() {
        let mut c = Control::default();
        c.open("s1", "t1", false, &page("p1"), 80, 24, false, 0);
        c.open("s1", "t1", false, &phone("ro", false), 40, 20, false, 0);
        c.closed("s1", &Surface::Page("p1".into()), 5);
        c.tick(5 + GRACE_MS, &never, &no_use);
        // Only a view-only device left: free, at its size.
        assert_eq!(holder(&c, "s1"), None);
        assert_eq!(c.wants()["s1"].size, Size::Keep);
        assert_eq!(c.wants()["s1"].held, None);
        // It goes too: no entry (the window keeps its size: nothing is
        // attached that sizes it).
        c.closed("s1", &Surface::Device("ro".into()), 6);
        assert!(c.is_empty());
        // The terminal holding, its last viewer gone: no entry either.
        c.open("s2", "t2", false, &page("p1"), 80, 24, true, 0);
        c.closed("s2", &Surface::Page("p1".into()), 1);
        assert!(c.is_empty());
        // A free pane the terminal comes to: the terminal's.
        c.open("s3", "t3", false, &phone("ro", false), 40, 20, false, 0);
        c.tick(1, &|_| true, &no_use);
        assert_eq!(holder(&c, "s3").as_deref(), Some("terminal"));
    }

    #[test]
    fn a_restart_forgets() {
        let mut c = Control::default();
        c.open("s1", "t1", false, &page("p1"), 80, 24, false, 0);
        assert!(!c.is_empty());
        // A new daemon starts from nothing: every pane free, nothing to
        // pin or hold (and `sweep` lets go of what the old one held).
        let fresh = Control::default();
        assert!(fresh.wants().is_empty());
        assert!(fresh.entry("s1").is_null());
        assert_eq!(fresh.notify_line(), r#"{"notify":"control","sessions":{}}"#);
    }

    #[test]
    fn the_plugins_line_has_every_held_pane() {
        let mut c = Control::default();
        c.open("s1", "t1", false, &page("p1"), 80, 24, false, 0);
        c.open("s3", "t3", false, &phone("ro", false), 40, 20, false, 0);
        let v: Value = serde_json::from_str(&c.notify_line()).unwrap();
        assert_eq!(
            v,
            json!({"notify": "control", "sessions": {"s1": {
                "holder": "page:p1", "by": "web", "name": "a browser on mac",
                "away": false, "cols": 80, "rows": 24, "tmux_session": "t1"}}})
        );
    }

    #[test]
    fn surfaces_parse() {
        for s in ["terminal", "page:p1-ab", "device:ab12"] {
            assert_eq!(Surface::parse(s).unwrap().key(), s);
        }
        for s in ["", "page:", "phone:x", "terminal:x"] {
            assert!(Surface::parse(s).is_none(), "{s}");
        }
    }
}
