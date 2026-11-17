//! `repo_latest`: the latest committed code of a repo, in one read-only
//! tree rlord keeps per repo (always `<repo>/.worktrees/.rlord-latest`), and
//! the commit a new session's branch starts from. Plain code, no model
//! call; the git side is `rlord_core::latest`.
//!
//! - **Fetch.** One branch at a time into a private ref, on its own
//!   thread. A success under `fresh` old is reused. A caller waits at most
//!   `tool_wait` (the tool) or `launch_wait` (a new session) and otherwise
//!   goes on with the last known commit (`fetch: running`); the next call
//!   within `fresh` picks the result up. One fetch per (repo, remote,
//!   branch) at a time; only `latest::HARD_LIMIT` kills one.
//! - **One tree per repo**, moved to whichever ref is asked for: callers
//!   learn of a move from the commit and ref in every answer. It is moved
//!   or removed only when rlord recorded it at that path and git has it as
//!   a detached worktree (`latest::may_touch`): never a session's.
//! - **Locks.** Resolving (fetch, pick) takes no lock: fetches are one
//!   per key, and records are updated field by field in one DB hold. A
//!   lock per repo (`Latest::moves`, keyed by its common git dir) is held
//!   while its tree is moved or removed, so those queue per repo. A
//!   launch only resolves: it never waits on a tree. The DB is taken
//!   briefly under the move lock, never the other way round (locks.rs).
//! - **Records.** kv `repo_latest:<the repo's common git dir>`:
//!   where rlord works from, the tree, its ref and commit, when the repo
//!   and the tree were last used, the default branch as last asked of the
//!   remote, when each branch was last fetched. The daily sweep works from
//!   these; it never scans for repos.
//! - **macOS privacy.** In a repo inside a folder macOS guards (and not
//!   under a scan root) rlord runs only git: it reads no file there
//!   itself, says so in a warning, and the unprompted sweep skips it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use rlord_core::config::{expand_tilde, Config};
use rlord_core::latest::{self, short, Fetch, GitEnv, Located, Pick, Prefer, Repo, Source};
use rlord_core::privacy::Guard;
use rlord_core::types::now_ms;

use crate::locks::KeyLocks;
use crate::Daemon;

/// kv key prefix of the records: one key per repo, `repo_latest:<common
/// git dir>`.
const KV: &str = "repo_latest:";
/// A failed `ls-remote` for the default branch isn't tried again for this
/// long.
const LS_RETRY_MS: i64 = 5 * 60 * 1000;
/// A recorded default branch is asked of the remote again after this.
const DEFAULT_TTL_MS: i64 = 24 * 3600 * 1000;
/// A tree unused this long is removed by the sweep.
const UNUSED_MS: i64 = 30 * 24 * 3600 * 1000;
/// A private ref not fetched this long is dropped on the repo's next use.
const REF_UNUSED_MS: i64 = 14 * 24 * 3600 * 1000;

/// Timings, set by the daemon (`default`) and by tests.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// A fetch younger than this is not repeated, and a result no caller
    /// waited for is handed over only this long.
    pub fresh: Duration,
    /// How long the tool waits on a fetch.
    pub tool_wait: Duration,
    /// How long a new session's launch waits on one.
    pub launch_wait: Duration,
    /// A failed fetch (offline, failed, missing) isn't tried again for
    /// this long...
    pub failed_fresh: Duration,
    /// ...or this long for a named branch (a launch's base, the tool's
    /// `ref`: it may have just been pushed).
    pub named_retry: Duration,
    /// How long, in all, a launch waits on the first fetch of a named base
    /// that is only on the remote, before it refuses ("still fetching").
    pub named_base_wait: Duration,
    /// The tree job's own wait, even when a slow fetch spent the call's
    /// budget: a tree already right, or a cheap move, still answers.
    pub tree_min_wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            fresh: Duration::from_secs(60),
            tool_wait: Duration::from_secs(8),
            launch_wait: Duration::from_secs(4),
            failed_fresh: Duration::from_secs(120),
            named_retry: Duration::from_secs(10),
            named_base_wait: Duration::from_secs(20),
            tree_min_wait: Duration::from_secs(2),
        }
    }
}

struct Settings {
    env: GitEnv,
    timing: Timing,
    guard: Guard,
}

/// One (repo, remote, branch)'s fetch.
#[derive(Default)]
struct Slot {
    running: bool,
    /// The last outcome and when it finished.
    last: Option<(Instant, Fetch)>,
    /// No caller has seen `last` (they stopped waiting before it came).
    unseen: bool,
    /// The last success, and when (wall clock, ms).
    ok_at: Option<Instant>,
    ok_ms: Option<i64>,
}

type Fetches = Arc<(Mutex<HashMap<String, Slot>>, Condvar)>;

/// The daemon's `repo_latest` state.
pub struct Latest {
    fetches: Fetches,
    settings: RwLock<Settings>,
    /// One move or removal of a repo's tree at a time (key: its common
    /// git dir).
    pub(crate) moves: KeyLocks,
    /// Tree jobs (adding or moving a repo's tree can take minutes), by
    /// repo: run on their own thread, the call waiting a bounded time.
    jobs: Jobs,
}

/// What a tree job (making or moving a repo's tree) did, for the answer.
#[derive(Clone)]
struct TreeDone {
    moved: Option<Value>,
    warnings: Vec<String>,
}

/// A repo's tree jobs: whether one runs, for which request (the ref and
/// commit it moves the tree to), its generation, and the results of
/// recent ones by generation (read by every waiter, kept a short while).
#[derive(Default)]
struct JobSlot {
    running: bool,
    request: String,
    gen: u64,
    done: HashMap<u64, (Instant, std::result::Result<TreeDone, String>)>,
}

/// How long a tree job's result stays readable for its waiters.
const JOB_RESULT_KEEP: Duration = Duration::from_secs(30);

type Jobs = Arc<(Mutex<HashMap<String, JobSlot>>, Condvar)>;

/// How a tree job stands, for the caller.
enum Job {
    Done(std::result::Result<TreeDone, String>),
    Running,
}

impl Default for Latest {
    fn default() -> Self {
        Latest {
            fetches: Arc::default(),
            settings: RwLock::new(Settings {
                env: GitEnv::default(),
                timing: Timing::default(),
                guard: Guard::new(),
            }),
            moves: KeyLocks::default(),
            jobs: Arc::default(),
        }
    }
}

impl Latest {
    fn settings(&self) -> std::sync::RwLockReadGuard<'_, Settings> {
        self.settings.read().unwrap_or_else(|e| e.into_inner())
    }
    fn env(&self) -> GitEnv {
        self.settings().env.clone()
    }
    fn timing(&self) -> Timing {
        self.settings().timing
    }
    fn guard(&self) -> Guard {
        self.settings().guard.clone()
    }

    #[cfg(test)]
    pub(crate) fn set_for_test(&self, env: GitEnv, timing: Timing, guard: Guard) {
        *self.settings.write().unwrap() = Settings { env, timing, guard };
    }

    /// Forget the fetches of the repo `key` for `dropped`, the record's
    /// `<remote>/<branch>` keys whose private refs were just deleted, and
    /// only those.
    pub fn forget_refs(&self, key: &str, dropped: &[String]) {
        if dropped.is_empty() {
            return;
        }
        let (lock, _) = &*self.fetches;
        let mut map = lock.lock().unwrap_or_else(|e| e.into_inner());
        for k in dropped {
            if let Some((r, b)) = k.split_once('/') {
                let slot = format!("{key}\0{r}\0{b}");
                if map.get(&slot).is_some_and(|s| !s.running) {
                    map.remove(&slot);
                }
            }
        }
    }

    /// When `key`'s last successful fetch finished (wall clock, ms).
    fn ok_ms(&self, key: &str) -> Option<i64> {
        let (lock, _) = &*self.fetches;
        let map = lock.lock().unwrap_or_else(|e| e.into_inner());
        map.get(key).and_then(|s| s.ok_ms)
    }

    /// Forget every fetch of the repo whose keys start with `prefix` (its
    /// common git dir): its private refs were deleted.
    pub fn forget(&self, prefix: &str) {
        let (lock, _) = &*self.fetches;
        let mut map = lock.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|k, s| s.running || !k.starts_with(prefix));
    }

    /// Run `fetch` for `key` on its own thread unless one succeeded under
    /// `fresh` ago (`Cached`; only while its private ref is there,
    /// `have_ref`) or is running; wait at most `wait` for it.
    /// A result that came after its callers stopped waiting is handed to
    /// the next caller, if it is under `fresh` old; an older one is
    /// dropped and the fetch runs again.
    pub fn fetch<F>(
        &self,
        key: &str,
        wait: Duration,
        have_ref: bool,
        named: bool,
        fetch: F,
    ) -> Fetch
    where
        F: FnOnce() -> Fetch + Send + 'static,
    {
        let Timing {
            fresh,
            failed_fresh,
            named_retry,
            ..
        } = self.timing();
        // A named branch asked for again (a launch's base, the tool's
        // `ref`): its failure is tried again sooner (it may have just been
        // pushed).
        let failed_fresh = if named {
            failed_fresh.min(named_retry)
        } else {
            failed_fresh
        };
        let (lock, done) = &*self.fetches;
        let mut map = lock.lock().unwrap_or_else(|e| e.into_inner());
        let slot = map.entry(key.to_string()).or_default();
        if !slot.running {
            if std::mem::take(&mut slot.unseen) {
                // (A failure only while it still counts: for a named
                // branch, `named_retry`.)
                let unseen = slot.last.as_ref().filter(|(at, f)| {
                    let failed = matches!(f, Fetch::Offline | Fetch::Failed(_) | Fetch::Missing);
                    at.elapsed() < if failed { failed_fresh } else { fresh }
                });
                if let Some((_, f)) = unseen {
                    return f.clone();
                }
            }
            if have_ref && slot.ok_at.is_some_and(|t| t.elapsed() < fresh) {
                return Fetch::Cached;
            }
            // A failure a moment ago: the same answer, not another try.
            if let Some((at, f)) = &slot.last {
                let failed = matches!(f, Fetch::Offline | Fetch::Failed(_) | Fetch::Missing);
                if failed && at.elapsed() < failed_fresh {
                    return f.clone();
                }
            }
            slot.running = true;
            let state = self.fetches.clone();
            let k = key.to_string();
            let spawned = std::thread::Builder::new()
                .name("repo-fetch".into())
                .spawn(move || run_fetch(state, k, fetch));
            if spawned.is_err() {
                slot.running = false;
                return Fetch::Failed("couldn't start a thread".into());
            }
        }
        let mut map = done
            .wait_timeout_while(map, wait, |m| m.get(key).is_some_and(|s| s.running))
            .unwrap_or_else(|e| e.into_inner())
            .0;
        // (Forgotten meanwhile, its refs deleted: its outcome is gone with
        // it, so nothing can be said of it; never `cached`.)
        let Some(slot) = map.get_mut(key) else {
            return Fetch::Failed("the fetch's outcome was dropped with its refs".into());
        };
        if slot.running {
            return Fetch::Running;
        }
        slot.unseen = false;
        match &slot.last {
            Some((_, f)) => f.clone(),
            None => Fetch::Failed("the fetch left no outcome".into()),
        }
    }
}

impl Latest {
    /// Run `job` (making or moving the tree to `request`) on its own
    /// thread and wait at most `wait` for it. A job for another request
    /// already running is waited out first; one for this request is
    /// joined. A result is only ever handed to the calls waiting on that
    /// job: a later call runs its own (cheap when the tree is already
    /// right), so no old result, nor an old error, is handed back.
    fn tree_job<F>(&self, key: &str, request: &str, wait: Duration, job: F) -> Job
    where
        F: FnOnce() -> std::result::Result<TreeDone, String> + Send + 'static,
    {
        let deadline = Instant::now() + wait;
        let (lock, cv) = &*self.jobs;
        let mut map = lock.lock().unwrap_or_else(|e| e.into_inner());
        // Another request's job: waited out first.
        loop {
            let slot = map.entry(key.to_string()).or_default();
            if !slot.running || slot.request == request {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Job::Running;
            }
            map = cv
                .wait_timeout(map, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        let slot = map.entry(key.to_string()).or_default();
        slot.done
            .retain(|_, (at, _)| at.elapsed() < JOB_RESULT_KEEP);
        // Joined (the same request running), or started.
        let mine = if slot.running {
            slot.gen
        } else {
            slot.gen += 1;
            slot.running = true;
            slot.request = request.to_string();
            let gen = slot.gen;
            let state = self.jobs.clone();
            let k = key.to_string();
            let spawned = std::thread::Builder::new()
                .name("repo-tree".into())
                .spawn(move || {
                    // (Its slot freed even if `job` panics.)
                    struct Done(
                        Jobs,
                        String,
                        u64,
                        Option<std::result::Result<TreeDone, String>>,
                    );
                    impl Drop for Done {
                        fn drop(&mut self) {
                            let (lock, cv) = &*self.0;
                            let mut map = lock.lock().unwrap_or_else(|e| e.into_inner());
                            let slot = map.entry(self.1.clone()).or_default();
                            if slot.gen == self.2 {
                                slot.running = false;
                            }
                            let r = self
                                .3
                                .take()
                                .unwrap_or_else(|| Err("the tree job panicked".into()));
                            slot.done.insert(self.2, (Instant::now(), r));
                            cv.notify_all();
                        }
                    }
                    let mut d = Done(state, k, gen, None);
                    d.3 = Some(job());
                });
            if spawned.is_err() {
                slot.running = false;
                return Job::Done(Err("couldn't start a thread".into()));
            }
            gen
        };
        let left = deadline.saturating_duration_since(Instant::now());
        let map = cv
            .wait_timeout_while(map, left, |m| {
                m.get(key).is_some_and(|s| !s.done.contains_key(&mine))
            })
            .unwrap_or_else(|e| e.into_inner())
            .0;
        // (Read, not taken: every waiter of this job gets it.)
        match map.get(key).and_then(|s| s.done.get(&mine)) {
            Some((_, r)) => Job::Done(r.clone()),
            None => Job::Running,
        }
    }

    #[cfg(test)]
    pub(crate) fn hold_add_for_test(&self, key: &str) {
        let (lock, _) = &*self.jobs;
        let mut map = lock.lock().unwrap();
        let slot = map.entry(key.to_string()).or_default();
        slot.running = true;
        slot.request = "another".into();
    }
}

/// One fetch, on its own thread. Frees its slot even if it panics.
fn run_fetch<F: FnOnce() -> Fetch>(state: Fetches, key: String, fetch: F) {
    struct Done(Fetches, String, Option<Fetch>);
    impl Drop for Done {
        fn drop(&mut self) {
            let (lock, done) = &*self.0;
            let mut map = lock.lock().unwrap_or_else(|e| e.into_inner());
            let slot = map.entry(self.1.clone()).or_default();
            let f = self
                .2
                .take()
                .unwrap_or_else(|| Fetch::Failed("the fetch panicked".into()));
            let now = Instant::now();
            if f == Fetch::Fetched {
                slot.ok_at = Some(now);
                slot.ok_ms = Some(now_ms());
            }
            slot.running = false;
            slot.last = Some((now, f));
            // (Cleared by a caller still waiting, who sees it now.)
            slot.unseen = true;
            done.notify_all();
        }
    }
    let mut d = Done(state, key, None);
    d.2 = Some(fetch());
}

/// What rlord has recorded about one repo (keyed by its common git dir).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rec {
    /// Where rlord works from (`latest::Repo::root`).
    pub root: String,
    /// rlord's tree (absent: none made).
    pub tree: Option<String>,
    /// The ref and commit it was last moved to.
    #[serde(rename = "ref")]
    pub branch: Option<String>,
    pub commit: Option<String>,
    /// Last used at all (the tool, or a launch's base).
    pub used_at: i64,
    /// The tree last moved or read through the tool.
    pub tree_used_at: i64,
    /// The default branch as the remote last said, and when.
    pub default_branch: Option<String>,
    pub default_branch_at: i64,
    /// When asking the remote for it last failed.
    pub ls_failed_at: i64,
    /// What the tree's checkout leaves out (`latest::tree_warnings`), as
    /// of its last move.
    pub tree_warnings: Vec<String>,
    /// `<remote>/<branch>` → when rlord last fetched it (ms).
    pub fetched: BTreeMap<String, i64>,
}

fn records(d: &Daemon) -> BTreeMap<String, Rec> {
    d.db()
        .kv_prefix(KV)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| {
            Some((
                k.strip_prefix(KV)?.to_string(),
                serde_json::from_str(&v).ok()?,
            ))
        })
        .collect()
}

fn record(d: &Daemon, key: &str) -> Rec {
    d.db()
        .kv_get(&format!("{KV}{key}"))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// Change one repo's record in one DB hold (false from `f`: drop it), so
/// callers that don't share a lock never undo each other's fields.
fn update(d: &Daemon, key: &str, f: impl FnOnce(&mut Rec) -> bool) {
    let k = format!("{KV}{key}");
    let db = d.db();
    let mut rec: Rec = db
        .kv_get(&k)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default();
    let r = if f(&mut rec) {
        serde_json::to_string(&rec)
            .map_err(anyhow::Error::from)
            .and_then(|v| db.kv_set(&k, &v))
    } else {
        db.kv_delete(&k)
    };
    if let Err(e) = r {
        tracing::warn!(error = %e, "saving repo_latest records");
    }
}

fn key_of(repo: &Repo) -> String {
    repo.common.to_string_lossy().into_owned()
}

/// What a call asks for.
struct Want<'a> {
    path: &'a str,
    branch: Option<&'a str>,
    prefer: Option<Prefer>,
    /// How long to wait on a fetch.
    wait: Duration,
    /// Everything the call waits on (`ls-remote`, fetches, a retry, the
    /// tree job) ends by then.
    deadline: Option<Instant>,
    /// A launch's base (vs the tool): the remote is asked for the default
    /// branch only when nothing local answers, never the checkout's own.
    launch: bool,
}

impl Want<'_> {
    /// What's left of the deadline (none: `limit`).
    fn left(&self, limit: Duration) -> Duration {
        match self.deadline {
            Some(t) => limit.min(t.saturating_duration_since(Instant::now())),
            None => limit,
        }
    }
}

/// A repo resolved to a branch and its newest commit.
struct Found {
    branch: String,
    /// The branch is the default one (no `ref` asked).
    is_default: bool,
    remote: Option<String>,
    fetch: Fetch,
    fetched_at: Option<i64>,
    local: Option<String>,
    remote_commit: Option<String>,
    pick: Pick,
    /// rlord may read files in the repo itself (not a guarded folder).
    fs_ok: bool,
    warnings: Vec<String>,
}

/// A named branch not here yet whose fetch is still running (a launch
/// waits on it longer: `Timing::named_base_wait`).
#[derive(Debug)]
struct StillFetching(String);

impl std::fmt::Display for StillFetching {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StillFetching {}

/// What a resolve learned, for the record.
#[derive(Default)]
struct Learned {
    /// The default branch, as the remote just said.
    default_branch: Option<String>,
    /// Asking the remote for the default branch failed.
    ls_failed: bool,
    /// `<remote>/<branch>` the remote doesn't have.
    missing: Option<String>,
}

impl Learned {
    fn apply(self, rec: &mut Rec, repo: &Repo, now: i64) {
        rec.root = repo.root.to_string_lossy().into_owned();
        rec.used_at = now;
        if let Some(b) = self.default_branch {
            rec.default_branch = Some(b);
            rec.default_branch_at = now;
        }
        if self.ls_failed {
            rec.ls_failed_at = now;
        }
        if let Some(k) = self.missing {
            rec.fetched.remove(&k);
        }
    }
}

/// `<remote>/<branch>` split when `remote` is a remote and no local branch
/// has the whole name.
fn split_remote(env: &GitEnv, root: &Path, remote: Option<&str>, name: &str) -> Option<String> {
    let r = remote?;
    let rest = name.strip_prefix(r)?.strip_prefix('/')?;
    let whole_local = latest::commit_of(env, root, &format!("refs/heads/{name}")).is_some();
    (!rest.is_empty() && !whole_local).then(|| rest.to_string())
}

/// Whether `root` is in a folder macOS guards and not under a scan root
/// (rlord then reads nothing there itself).
fn guarded(d: &Daemon, cfg: &Config, root: &Path) -> bool {
    let guard = d.latest.guard();
    let roots: Vec<PathBuf> = cfg.session.scan_roots().iter().map(PathBuf::from).collect();
    guard.prompts(root).is_some() && !roots.iter().any(|r| guard.is_within(root, r))
}

/// Find the branch and its newest commit (fetching it). No lock: `snap`
/// is the record as it was; what was learned comes back for `update`.
fn resolve(
    d: &Daemon,
    cfg: &Config,
    repo: &Repo,
    want: &Want,
    snap: &Rec,
) -> Result<(Found, Learned)> {
    let env = d.latest.env();
    let root = repo.root.as_path();
    let mut learned = Learned::default();
    let mut warnings = Vec::new();
    let is_guarded = guarded(d, cfg, root);
    if is_guarded {
        warnings.push(
            "guarded_folder: the repo is in a folder macOS guards; rlord ran only git there"
                .to_string(),
        );
    }
    let remote = match latest::remote_of(&env, root)? {
        Ok(r) => Some(r),
        Err(why) => {
            warnings.push(why);
            None
        }
    };
    // (Worked out once: what talks to the remote uses it.)
    let renv = match remote {
        Some(_) => latest::remote_env(&env, root),
        None => env.clone(),
    };
    let now = now_ms();
    let mut prefer = want.prefer;
    // `<remote>/<b>`: the remote's asked for, never the local one instead.
    let mut explicit_remote = false;
    let (mut branch, from) = match want.branch {
        Some(b) => {
            let b = match split_remote(&env, root, remote.as_deref(), b) {
                // (They named the remote's: on a divergence, it wins.)
                Some(rest) => {
                    if prefer == Some(Prefer::Local) {
                        warnings.push(format!(
                            "prefer_ignored: `{b}` asks for the remote's; prefer: local is ignored"
                        ));
                    }
                    prefer = Some(Prefer::Remote);
                    explicit_remote = true;
                    rest
                }
                None => b.to_string(),
            };
            latest::check_branch(&b)?;
            (b, None)
        }
        None => {
            let cached = snap
                .default_branch
                .as_deref()
                .filter(|_| now - snap.default_branch_at < DEFAULT_TTL_MS);
            let mut failed = false;
            let found = latest::default_branch(
                &env,
                &renv,
                root,
                remote.as_deref(),
                cached,
                false,
                ask(want, snap, now),
                &mut failed,
            );
            learned.ls_failed |= failed;
            // (Recorded even when nothing answered: not asked again soon.)
            if failed && found.is_err() {
                update(d, &key_of(repo), |r| {
                    r.root = repo.root.to_string_lossy().into_owned();
                    r.ls_failed_at = now;
                    true
                });
            }
            let (b, from) = found?;
            (b, Some(from))
        }
    };
    if from == Some(latest::DefaultFrom::LsRemote) {
        learned.default_branch = Some(branch.clone());
    }
    // Nothing found the default branch: the checkout's own is shown, and
    // never presented as the default (no remote-wins rule, a warning).
    let mut current_only = from == Some(latest::DefaultFrom::Current);
    let fetch_one = |branch: &str| -> Fetch {
        let Some(r) = remote.clone() else {
            return Fetch::NoRemote;
        };
        let key = format!("{}\0{r}\0{branch}", repo.common.display());
        let (env, renv, root, b) = (
            env.clone(),
            renv.clone(),
            root.to_path_buf(),
            branch.to_string(),
        );
        // (Its thread records a success itself, whether or not anyone is
        // still waiting for it.)
        let (me, rkey) = (d.me.clone(), key_of(repo));
        let have_ref = latest::commit_of(&env, &root, &latest::private_ref(&r, &b)).is_some();
        let named = want.branch.is_some();
        d.latest
            .fetch(&key, want.left(want.wait), have_ref, named, move || {
                let f = latest::fetch_branch(&env, &renv, &root, &r, &b);
                if let (Fetch::Fetched, Some(d)) = (&f, me.upgrade()) {
                    let at = now_ms();
                    update(&d, &rkey, |rec| {
                        rec.fetched.insert(format!("{r}/{b}"), at);
                        true
                    });
                }
                f
            })
    };
    let mut fetch = fetch_one(&branch);
    // The default branch the remote doesn't have (renamed): the remote
    // asked, whatever found it (its HEAD, the record, a guess), within
    // what is left of the budget.
    if fetch == Fetch::Missing && from.is_some() {
        let mut failed = false;
        // (Not within `LS_RETRY_MS` of a failed ask.)
        let ask_again = if now - snap.ls_failed_at < LS_RETRY_MS {
            latest::AskRemote::Never
        } else {
            latest::AskRemote::First(want.left(latest::LS_REMOTE))
        };
        let retry = latest::default_branch(
            &env,
            &renv,
            root,
            remote.as_deref(),
            None,
            true,
            ask_again,
            &mut failed,
        );
        learned.ls_failed |= failed;
        // A launch never takes the checkout's own branch for the default
        // one: it bails below (and starts at HEAD, saying why).
        let retry = retry
            .ok()
            .filter(|(_, f)| !want.launch || *f != latest::DefaultFrom::Current);
        if let Some((b, f)) = retry {
            if b != branch {
                branch = b;
                current_only = f == latest::DefaultFrom::Current;
                if f == latest::DefaultFrom::LsRemote {
                    learned.default_branch = Some(branch.clone());
                }
                fetch = fetch_one(&branch);
            }
        }
    }
    // A launch never starts at a default branch the remote says is gone.
    if fetch == Fetch::Missing && from.is_some() && want.launch {
        bail!(
            "{} has no branch {branch} (the default branch renamed?)",
            remote.as_deref().unwrap_or("the remote")
        );
    }
    let is_default = want.branch.is_none() && !current_only;
    if current_only {
        warnings.push(format!(
            "no_default_branch: couldn't find the default branch; showing the checkout's \
             branch {branch}"
        ));
    }
    let key = |r: &str| format!("{r}/{branch}");
    let mut fetched_at = remote
        .as_deref()
        .and_then(|r| snap.fetched.get(&key(r)).copied());
    if fetch.fresh() {
        // (When it really finished: it may have been handed over later,
        // or, cached, be a while ago.)
        let k = remote
            .as_deref()
            .map(|r| format!("{}\0{r}\0{branch}", repo.common.display()));
        let ok = k.and_then(|k| d.latest.ok_ms(&k));
        fetched_at = match fetch {
            Fetch::Fetched => ok.or(Some(now)),
            _ => ok.or(fetched_at),
        };
    }
    let local = latest::commit_of(&env, root, &format!("refs/heads/{branch}"));
    // The remote's side: rlord's private ref or the user's own
    // `refs/remotes/<r>/<b>` (their own fetch may be newer), whichever is.
    let mut remote_commit = None;
    if let (Some(r), false) = (&remote, fetch == Fetch::Missing) {
        let mine = latest::commit_of(&env, root, &latest::private_ref(r, &branch));
        let theirs_ref = format!("refs/remotes/{r}/{branch}");
        let theirs = latest::commit_of(&env, root, &theirs_ref);
        let fresh_ms = d.latest.timing().fresh.as_millis() as i64;
        // (Fresh: this call's fetch went through, and recently.)
        let mine_fresh = fetch.fresh() && fetched_at.is_some_and(|t| now - t < fresh_ms);
        let use_theirs = match (&mine, &theirs) {
            (None, Some(_)) => true,
            // Just fetched: the remote as it is now, even behind the user's
            // own (a force-push back to an older commit).
            (Some(_), _) if mine_fresh => false,
            (Some(m), Some(t)) if m != t => match latest::relation(&env, root, m, t) {
                // (`relation(mine, theirs)`: "behind" is theirs newer.)
                latest::Relation::LocalBehind(_) => true,
                latest::Relation::LocalAhead(_) | latest::Relation::Same => false,
                // Diverged or not comparable: rlord's when just fetched.
                _ => !mine_fresh,
            },
            _ => false,
        };
        if use_theirs {
            remote_commit = theirs;
            fetched_at = latest::reflog_time(&env, root, &theirs_ref);
            let why = if mine.is_some() {
                "newer than rlord's"
            } else {
                "not fetched by rlord yet"
            };
            warnings.push(format!(
                "theirs: {r}/{branch} as you last fetched it ({why}); rlord's fetch: {}",
                fetch.id()
            ));
        } else {
            remote_commit = mine;
        }
    }
    if fetch == Fetch::Missing {
        if let Some(r) = &remote {
            learned.missing = Some(key(r));
            warnings.push(format!("missing: {r} has no branch {branch}"));
        }
    }
    match &fetch {
        Fetch::Offline | Fetch::Running => warnings.push(format!(
            "{}: as of the last fetch{}",
            fetch.id(),
            fetched_at
                .map(|t| format!(" ({} ago)", rlord_core::timefmt::rel_age(Some(t))))
                .unwrap_or_default()
        )),
        Fetch::Failed(why) => warnings.push(format!("fetch_failed: {why}")),
        _ => {}
    }
    if remote_commit.is_none() && (local.is_none() || explicit_remote) {
        let r = remote.as_deref().unwrap_or("the remote");
        let here = if explicit_remote {
            String::new()
        } else {
            format!(", and there is no local {branch}")
        };
        match &fetch {
            Fetch::Running => {
                return Err(anyhow::Error::new(StillFetching(format!(
                    "still fetching {branch} from {r}: try again shortly"
                ))))
            }
            Fetch::Offline => bail!("offline: can't fetch {branch} from {r}{here}"),
            Fetch::Failed(why) => bail!("fetching {branch} from {r} failed ({why}){here}"),
            Fetch::Missing if explicit_remote => bail!("{r} has no branch {branch}"),
            _ => bail!(
                "no branch {branch} in {}{}",
                root.display(),
                remote
                    .as_deref()
                    .map(|r| format!(" or on {r}"))
                    .unwrap_or_default()
            ),
        }
    }
    let rel = match (&local, &remote_commit) {
        (Some(l), Some(r)) => latest::relation(&env, root, l, r),
        _ => latest::Relation::Unknown,
    };
    if rel == latest::Relation::Unknown && local.is_some() && remote_commit.is_some() {
        warnings.push(format!(
            "unknown: couldn't compare the local {branch} with the remote's; using the {}",
            if prefer == Some(Prefer::Local) {
                "local one"
            } else {
                "remote's"
            }
        ));
    }
    let pick = latest::pick(
        local.as_deref(),
        remote_commit.as_deref(),
        &rel,
        prefer,
        is_default,
    )
    .expect("one side exists");
    let diverged = matches!(
        pick,
        Pick::Diverged { .. } | Pick::Use { diverged: true, .. }
    );
    if diverged && latest::is_shallow(&env, root) {
        warnings.push("shallow: a shallow clone; the two may not really have diverged".into());
    }
    if let (
        Pick::Use {
            source: Source::Remote,
            ahead: Some(n),
            diverged: false,
            ..
        },
        true,
    ) = (&pick, is_default || prefer == Some(Prefer::Remote))
    {
        if *n > 0 {
            warnings.push(format!(
                "local_ahead: your local {branch} has {n} commit(s) not on {}; this is the remote's",
                remote.as_deref().unwrap_or("the remote")
            ));
        }
    }
    if let (
        Pick::Use {
            diverged: true,
            source,
            ..
        },
        true,
    ) = (&pick, prefer.is_none())
    {
        warnings.push(format!(
            "diverged: the local {branch} and {}'s have diverged; using the {}",
            remote.as_deref().unwrap_or("remote"),
            if *source == Source::Local {
                "local one"
            } else {
                "remote's"
            }
        ));
    }
    let found = Found {
        branch,
        is_default,
        remote,
        fetch,
        fetched_at,
        local,
        remote_commit,
        pick,
        fs_ok: !is_guarded,
        warnings,
    };
    Ok((found, learned))
}

/// How the default branch may ask the remote: before guessing
/// `main`/`master` (a launch within its deadline, and never by the
/// checkout's branch); nobody within `LS_RETRY_MS` of a failure.
fn ask(want: &Want, snap: &Rec, now: i64) -> latest::AskRemote {
    let failed = now - snap.ls_failed_at < LS_RETRY_MS;
    match (want.launch, failed) {
        // (A launch never falls back to the checkout's branch, asked or
        // not.)
        (true, true) => latest::AskRemote::Launch(Duration::ZERO),
        (true, false) => latest::AskRemote::Launch(want.left(latest::LS_REMOTE)),
        (false, true) => latest::AskRemote::Never,
        (false, false) => latest::AskRemote::First(want.left(latest::LS_REMOTE)),
    }
}

/// The repo `path` is in.
fn locate(d: &Daemon, path: &str) -> Result<(PathBuf, Located)> {
    let p = PathBuf::from(expand_tilde(path));
    if p.as_os_str().is_empty() {
        bail!("missing repo");
    }
    let l = latest::locate(&d.latest.env(), &p)?;
    Ok((p, l))
}

/// A side of a diverged branch, for the answer.
fn side(env: &GitEnv, root: &Path, commit: Option<&str>, ahead: Option<u32>) -> Value {
    let info = commit.and_then(|c| latest::commit_info(env, root, c));
    json!({"commit": commit, "subject": info.as_ref().map(|i| &i.0),
           "committed_at": info.map(|i| i.1), "ahead": ahead})
}

/// The common part of an answer.
fn answer(env: &GitEnv, repo: &Repo, f: &Found) -> Value {
    let root = repo.root.as_path();
    let mut v = json!({
        "repo": root, "git": true, "ref": f.branch, "default_branch": f.is_default,
        "remote": f.remote, "fetch": f.fetch.id(), "fetched_at": f.fetched_at,
    });
    match &f.pick {
        Pick::Use {
            commit,
            source,
            ahead,
            behind,
            ..
        } => {
            let info = latest::commit_info(env, root, commit);
            v["commit"] = json!(commit);
            v["subject"] = json!(info.as_ref().map(|i| &i.0));
            v["committed_at"] = json!(info.map(|i| i.1));
            v["source"] = json!(source.id());
            v["local"] = json!({"ahead": ahead, "behind": behind});
        }
        Pick::Diverged {
            local_ahead,
            remote_ahead,
        } => {
            v["diverged"] = json!(true);
            v["local"] = side(env, root, f.local.as_deref(), *local_ahead);
            v["remote_side"] = side(env, root, f.remote_commit.as_deref(), *remote_ahead);
            v["hint"] = json!("ask the user which, then pass prefer");
        }
    }
    v
}

/// `repo.latest`: resolve, then move rlord's tree there.
pub fn latest_rpc(d: &Daemon, params: Value) -> Result<Value> {
    let wait = d.latest.timing().tool_wait;
    let prefer = match params["prefer"].as_str().filter(|s| !s.is_empty()) {
        Some(p) => {
            Some(Prefer::parse(p).ok_or_else(|| anyhow::anyhow!("prefer: local or remote"))?)
        }
        None => None,
    };
    let want = Want {
        path: params["repo"].as_str().unwrap_or(""),
        branch: params["ref"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        prefer,
        wait,
        // (The tool's whole call: its budget, across all it waits on.)
        deadline: Some(Instant::now() + wait),
        launch: false,
    };
    latest_in(d, &want)
}

fn latest_in(d: &Daemon, want: &Want) -> Result<Value> {
    let repo = match locate(d, want.path)? {
        (p, Located::NotGit) => {
            return Ok(json!({"git": false, "path": p,
                             "warnings": ["not a git repository: read it as it is"]}))
        }
        (_, Located::Repo(r)) => r,
    };
    let key = key_of(&repo);
    let cfg = d.config();
    let env = d.latest.env();
    let (mut f, learned) = resolve(d, &cfg, &repo, want, &record(d, &key))?;
    let now = now_ms();
    update(d, &key, |r| {
        learned.apply(r, &repo, now);
        true
    });
    let mut v = answer(&env, &repo, &f);
    let Pick::Use { commit, .. } = &f.pick else {
        v["warnings"] = json!(f.warnings);
        return Ok(v);
    };
    let commit = commit.clone();
    // The tree's path as git reports it (a symlinked `.worktrees` too).
    let dir = latest::tree_path_seen(&env, &repo.root, f.fs_ok);
    if latest::entry_at(&repo.worktrees, &dir).is_none() {
        exclude(&env, &repo, &dir, &f, &mut v)?;
    }
    // Adding or moving the tree: on its own thread under the repo's move
    // lock (it can take minutes); this call waits a bounded time.
    let job = {
        let me = d.me.clone();
        let (k, repo, dir, commit) = (key.clone(), repo.clone(), dir.clone(), commit.clone());
        let (branch, remote, fs_ok) = (f.branch.clone(), f.remote.clone(), f.fs_ok);
        move || -> std::result::Result<TreeDone, String> {
            let d = me.upgrade().ok_or("the daemon is stopping")?;
            move_tree(
                &d,
                &k,
                &repo,
                &dir,
                &commit,
                &branch,
                remote.as_deref(),
                fs_ok,
            )
            .map_err(|e| format!("{e:#}"))
        }
    };
    // (By ref and commit: a call joining a running job records the same
    // ref it asked for.)
    let request = format!("{}\0{commit}", f.branch);
    let tree_wait = want.left(want.wait).max(d.latest.timing().tree_min_wait);
    match d.latest.tree_job(&key, &request, tree_wait, job) {
        Job::Done(Ok(t)) => {
            v["path"] = json!(dir);
            if let Some(m) = t.moved {
                v["moved"] = m;
            }
            f.warnings.extend(t.warnings);
        }
        Job::Done(Err(e)) => bail!("{e}"),
        Job::Running => {
            f.warnings.push(
                "preparing: rlord's tree for this repo is still being made; call again shortly"
                    .into(),
            );
            v["tree"] = json!("preparing");
        }
    }
    // How much is uncommitted where the user has it: for a named branch
    // (work the tree can't show); the default branch only says where.
    let count = want.branch.is_some();
    if let Some((at, n)) = latest::checked_out(&env, &repo.worktrees, &f.branch, count) {
        v["local"]["checked_out_at"] = json!(at);
        v["local"]["uncommitted"] = json!(n);
    }
    v["warnings"] = json!(f.warnings);
    Ok(v)
}

/// Add or move the repo's tree to `commit`, under its move lock (a tree
/// job's thread), and record it once done. One add at most per call.
#[allow(clippy::too_many_arguments)]
fn move_tree(
    d: &Daemon,
    key: &str,
    repo: &Repo,
    dir: &Path,
    commit: &str,
    branch: &str,
    remote: Option<&str>,
    fs_ok: bool,
) -> Result<TreeDone> {
    let _move = d.latest.moves.lock(key);
    let env = d.latest.env();
    let rec = record(d, key);
    let list = latest::worktree_list(&env, &repo.root)?;
    let mut warnings = Vec::new();
    // In progress: the commit is recorded as completed only once the add
    // or move succeeds (one killed partway may leave HEAD at the commit
    // with files missing).
    let complete = rec
        .commit
        .clone()
        .filter(|_| rec.tree.as_deref() == dir.to_str());
    let mut recorded = rec.tree.as_deref().map(PathBuf::from);
    let (moved, unchanged) = match latest::entry_at(&list, dir) {
        None => {
            // (Recorded as the tree only once it is added.)
            update(d, key, |r| {
                r.commit = None;
                true
            });
            let replaced = latest::tree_add_fresh(&env, repo, dir, commit, fs_ok)
                .map_err(|e| anyhow::anyhow!("making rlord's tree at {}: {e:#}", dir.display()))?;
            if replaced {
                warnings.push(
                    "recreated: a leftover of rlord's tree was there and was replaced".into(),
                );
            }
            (Some(json!({"created": true})), false)
        }
        Some(e) => {
            // No record of it (a kv reset, the other daemon's): rlord's by
            // construction when `adoptable`; recorded below.
            if recorded.as_deref() != Some(dir) && latest::adoptable(e, dir) {
                recorded = Some(dir.to_path_buf());
                warnings.push("adopted: rlord's tree had no record here; it does now".into());
            }
            // A branch checked out in rlord's recorded tree: it is detached
            // and moved, so nothing on record counts as completed.
            let had_branch = e.branch.is_some() && recorded.as_deref() == Some(dir);
            if had_branch || latest::may_touch(recorded.as_deref(), dir, e).is_ok() {
                update(d, key, |r| {
                    r.tree = Some(dir.to_string_lossy().into_owned());
                    r.commit = None;
                    true
                });
            }
            let complete = complete.as_deref().filter(|_| !had_branch);
            let m = latest::tree_move(
                &env,
                repo,
                dir,
                commit,
                recorded.as_deref(),
                complete,
                e,
                fs_ok,
            )?;
            if m.discarded > 0 {
                warnings.push(format!(
                    "discarded: edits in rlord's tree (at least {} file(s)) were thrown away \
                     (it is read-only)",
                    m.discarded
                ));
            }
            if m.recreated {
                warnings.push("recreated: rlord's tree was broken and was made again".into());
            }
            let was = rec.commit.clone().zip(rec.branch.clone());
            (
                moved_since(&env, &repo.root, was, branch, commit),
                m.unchanged,
            )
        }
    };
    // What the checkout leaves out: worked out once per move, kept.
    let tree_warnings = if unchanged {
        rec.tree_warnings.clone()
    } else {
        latest::tree_warnings(&env, &repo.root, dir)
    };
    warnings.extend(tree_warnings.iter().cloned());
    let dropped = drop_old_refs(&env, &repo.root, &rec, remote, branch);
    d.latest.forget_refs(key, &dropped);
    let now = now_ms();
    update(d, key, |r| {
        r.tree = Some(dir.to_string_lossy().into_owned());
        r.branch = Some(branch.to_string());
        r.commit = Some(commit.to_string());
        r.tree_used_at = now;
        r.used_at = now;
        r.tree_warnings = tree_warnings;
        for k in &dropped {
            r.fetched.remove(k);
        }
        true
    });
    Ok(TreeDone { moved, warnings })
}

/// Keep the tree out of the user's `git status`: `/.worktrees/` in the
/// repo's `info/exclude`, as for session worktrees. Nothing to do for a
/// bare repo (no status). In a guarded repo rlord writes no file: a note
/// if git doesn't ignore it already.
fn exclude(env: &GitEnv, repo: &Repo, dir: &Path, f: &Found, v: &mut Value) -> Result<()> {
    let Some(rel) = dir
        .parent()
        .and_then(|p| p.strip_prefix(&repo.root).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.is_empty() && !repo.bare)
    else {
        return Ok(());
    };
    if latest::ignored(env, &repo.root, &format!("{rel}/{}", latest::TREE_NAME)) {
        return Ok(());
    }
    if f.fs_ok {
        rlord_core::git::ensure_excluded(&repo.root.to_string_lossy(), &rel)?;
    } else {
        v["exclude_note"] = json!(format!(
            "/{rel}/ is not ignored: add it to .git/info/exclude"
        ));
    }
    Ok(())
}

/// How the tree moved since `was` (its commit and ref). None: unchanged,
/// or no record of where it was.
fn moved_since(
    env: &GitEnv,
    root: &Path,
    was: Option<(String, String)>,
    branch: &str,
    commit: &str,
) -> Option<Value> {
    let (old, old_ref) = was?;
    if old == commit && old_ref == branch {
        return None;
    }
    let mut m = json!({"from": old});
    if old_ref != branch {
        m["from_ref"] = json!(old_ref);
    } else if latest::is_ancestor(env, root, &old, commit) {
        m["commits"] = json!(latest::count_between(env, root, &old, commit));
    } else if latest::commit_of(env, root, &old).is_none() {
        // (Its object is gone: whether it was rewritten can't be told.)
        m["rewritten"] = json!("unknown");
    } else {
        m["rewritten"] = json!(true);
    }
    Some(m)
}

/// Drop private refs of branches not fetched for `REF_UNUSED_MS` (they
/// keep objects alive), except `current`'s. Returns the record keys gone.
fn drop_old_refs(
    env: &GitEnv,
    root: &Path,
    rec: &Rec,
    remote: Option<&str>,
    current: &str,
) -> Vec<String> {
    let now = now_ms();
    // (Keys are `<remote>/<branch>`: exactly the private ref's tail, so
    // the same branch under another remote is never touched.)
    let keep = remote.map(|r| format!("{r}/{current}"));
    rec.fetched
        .iter()
        .filter(|(k, at)| now - **at > REF_UNUSED_MS && Some(*k) != keep.as_ref())
        .filter(|(k, _)| {
            let r = format!("{}{k}", latest::REF_PREFIX);
            latest::run(env, root, &["update-ref", "-d", &r], latest::PLUMBING).is_ok_and(|o| o.ok)
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// Put `launch_base`'s answer in `session.new`'s params, over whatever a
/// caller put there.
pub fn set_launch_base(d: &Daemon, params: &mut Value) -> Result<()> {
    // (None given is no params.)
    if params.is_null() {
        *params = json!({});
    }
    if !params.is_object() {
        bail!("session.new takes an object of params");
    }
    params["location_resolved"] = Value::Null;
    // (Worked out once, here: `session_new` takes it as is.)
    if let Ok((repo, project, branch)) = crate::methods::new_location(&d.config(), params) {
        params["location_resolved"] = json!({"repo": repo, "project": project, "branch": branch});
    }
    params["base_resolved"] = launch_base(d, params)?;
    Ok(())
}

/// `base_resolved` for `session.new`, worked out before the DB lock:
/// null when the session gets no new branch in a worktree; `{"head":
/// true}` to start at the main checkout's HEAD (`[session] base = "head"`,
/// `base: "head"`, or the latest couldn't be found: `warnings` say why);
/// else the commit and where it came from. A named `base` that has
/// diverged, or doesn't exist, is an error: the launch is refused.
pub fn launch_base(d: &Daemon, params: &Value) -> Result<Value> {
    let cfg = d.config();
    let Some(repo) = params["location_resolved"]["repo"]
        .as_str()
        .map(str::to_string)
    else {
        return Ok(Value::Null);
    };
    let field = |k: &str| params[k].as_str().map(str::trim).filter(|v| !v.is_empty());
    let want_ws = params["workspace"]
        .as_bool()
        .unwrap_or(cfg.session.workspace);
    if !want_ws {
        return Ok(Value::Null);
    }
    // (`new_location`'s rule; none: named after a new id, so new.)
    let branch = params["location_resolved"]["branch"].as_str();
    if branch.is_some_and(|b| rlord_core::git::branch_exists(&repo, b)) {
        return Ok(Value::Null);
    }
    let named = field("base").filter(|b| !b.eq_ignore_ascii_case("latest"));
    match named {
        Some(b) if b.eq_ignore_ascii_case("head") => {
            return Ok(json!({"head": true, "why": "asked for"}))
        }
        None if field("base").is_none() && cfg.session.base_is_head() => {
            return Ok(json!({"head": true, "why": "[session] base = \"head\""}))
        }
        _ => {}
    }
    let wait = d.latest.timing().launch_wait;
    // The side the user chose for a base that had diverged: only with a
    // named base (never for the default branch, which is the remote's).
    let prefer = match field("prefer").filter(|_| named.is_some()) {
        Some(p) => {
            Some(Prefer::parse(p).ok_or_else(|| anyhow::anyhow!("prefer: local or remote"))?)
        }
        None => None,
    };
    let want = Want {
        path: &repo,
        branch: named,
        prefer,
        wait,
        deadline: Some(Instant::now() + wait),
        launch: true,
    };
    match base(d, &cfg, &want) {
        Ok(v) => Ok(v),
        Err(e) if named.is_some() => Err(e),
        Err(e) => Ok(json!({"head": true,
                            "why": format!("the latest couldn't be found: {e:#}"),
                            "warnings": [format!("{e:#}")]})),
    }
}

/// A launch's base: resolve only (no tree, no move lock).
fn base(d: &Daemon, cfg: &Config, want: &Want) -> Result<Value> {
    let repo = match locate(d, want.path)? {
        (p, Located::NotGit) => bail!("{} is not a git repository", p.display()),
        (_, Located::Repo(r)) => r,
    };
    let key = key_of(&repo);
    // A named base only on the remote whose first fetch is still running:
    // waited on up to `named_base_wait` in all, then refused, saying so.
    let until = Instant::now() + d.latest.timing().named_base_wait;
    let (f, learned) = loop {
        match resolve(d, cfg, &repo, want, &record(d, &key)) {
            Err(e) if want.branch.is_some() && e.is::<StillFetching>() => {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(e);
                }
                // (The same fetch, still running: waited on, not repeated.)
                let longer = Want {
                    wait: left,
                    deadline: Some(until),
                    ..*want
                };
                if let Ok(found) = resolve(d, cfg, &repo, &longer, &record(d, &key)) {
                    break found;
                }
                if Instant::now() >= until {
                    return Err(e);
                }
            }
            r => break r?,
        }
    };
    let now = now_ms();
    update(d, &key, |r| {
        learned.apply(r, &repo, now);
        true
    });
    let env = d.latest.env();
    let v = answer(&env, &repo, &f);
    if v["diverged"] == true {
        let s = |x: &Value| {
            format!(
                "{} \"{}\" ({} ahead)",
                short(x["commit"].as_str().unwrap_or("")),
                x["subject"].as_str().unwrap_or(""),
                x["ahead"]
                    .as_u64()
                    .map(|n| n.to_string())
                    .unwrap_or("?".into())
            )
        };
        let r = f.remote.as_deref().unwrap_or("the remote");
        bail!(
            "base {} has diverged: local {}, {r} {}. Ask the user which, then pass prefer \"local\" or \"remote\" with it",
            f.branch,
            s(&v["local"]),
            s(&v["remote_side"])
        );
    }
    let label = match (&f.remote, v["source"].as_str()) {
        (Some(r), Some("remote" | "same")) => format!("{r}/{}", f.branch),
        _ => f.branch.clone(),
    };
    Ok(json!({
        "commit": v["commit"], "ref": f.branch, "label": label, "remote": f.remote,
        "source": v["source"], "fetch": v["fetch"], "fetched_at": v["fetched_at"],
        "subject": v["subject"], "committed_at": v["committed_at"],
        "warnings": f.warnings,
    }))
}

/// The note a session launched with background gets about where its
/// branch started (`source = 'base'`). None: no base to speak of.
pub fn base_note(base: &Value, now: i64) -> Option<String> {
    if base["head"] == true {
        let at = base["commit"]
            .as_str()
            .map(|c| format!(" at {}", short(c)))
            .unwrap_or_default();
        let why = base["why"].as_str().unwrap_or("as configured");
        return Some(format!(
            "Your branch started from the main checkout's HEAD{at} ({why}), not from \
             the latest default branch: it may be behind."
        ));
    }
    let commit = short(base["commit"].as_str()?);
    let label = base["label"].as_str().unwrap_or("the default branch");
    let ago = |t: i64| {
        if now - t < 120_000 {
            "just now".to_string()
        } else {
            format!("{} ago", rlord_core::timefmt::rel_age_at(t, now))
        }
    };
    let what = match (base["subject"].as_str(), base["committed_at"].as_i64()) {
        (Some(s), Some(t)) => format!(" (\"{s}\", committed {})", ago(t)),
        _ => String::new(),
    };
    let fetched_at = base["fetched_at"].as_i64();
    Some(match base["fetch"].as_str().unwrap_or("") {
        "fetched" | "cached" => format!(
            "Your branch started from {label} at {commit}{what}, fetched {}.",
            fetched_at.map(ago).unwrap_or_else(|| "just now".into())
        ),
        "no_remote" => format!(
            "Your branch started from {label} at {commit}{what}. The repo has no remote: this is the local branch."
        ),
        // (Only here: there is nothing newer to be behind.)
        "missing" => format!(
            "Your branch started from {label} at {commit}{what}, the local branch: it isn't on the remote."
        ),
        why => format!(
            "Your branch started from {label} at {commit}{what} as last fetched{} ({}): it may be behind.",
            fetched_at.map(|t| format!(" {}", ago(t))).unwrap_or_default(),
            match why {
                "running" => "the fetch was still running",
                "offline" => "offline",
                _ => "the fetch failed",
            }
        ),
    })
}

/// `repo.latest_list`: the recorded trees.
pub fn list_rpc(d: &Daemon) -> Result<Value> {
    let rows: Vec<Value> = records(d)
        .into_iter()
        .map(|(key, r)| {
            json!({"repo": if r.root.is_empty() { key.clone() } else { r.root.clone() },
                   "git_dir": key, "path": r.tree, "ref": r.branch, "commit": r.commit,
                   "used_at": r.used_at, "tree_used_at": r.tree_used_at,
                   "default_branch": r.default_branch,
                   "fetched": r.fetched.keys().collect::<Vec<_>>()})
        })
        .collect();
    Ok(json!({"trees": rows}))
}

/// `repo.latest_prune`: remove the tree and private refs of `repo`, or of
/// every recorded repo, and their records (the user asked: guarded ones
/// too). A tree on a branch, or one rlord neither recorded nor can adopt,
/// is left.
pub fn prune_rpc(d: &Daemon, params: Value) -> Result<Value> {
    let only = match params["repo"].as_str().filter(|s| !s.is_empty()) {
        Some(p) => match locate(d, p)? {
            (p, Located::NotGit) => bail!("{} is not a git repository", p.display()),
            (_, Located::Repo(r)) => Some(r),
        },
        // Every repo: only when asked for in so many words.
        None if params["all"] == true => None,
        None => bail!("pass a repo, or all: true to prune every repo"),
    };
    let mut keys: Vec<(String, String)> = records(d)
        .into_iter()
        .map(|(k, r)| {
            let root = root_of(&k, &r);
            (k, root)
        })
        .collect();
    if let Some(r) = &only {
        let key = key_of(r);
        keys.retain(|(k, _)| *k == key);
        // No record: its tree and refs all the same.
        if keys.is_empty() {
            keys.push((key, r.root.to_string_lossy().into_owned()));
        }
    }
    let (mut removed, mut left, mut failed) = (Vec::new(), Vec::new(), Vec::new());
    for (key, root) in keys {
        match remove(d, &key, &root, Remove::All) {
            Ok(None) => removed.push(json!(root)),
            Ok(Some((path, why))) => left.push(json!({"repo": root, "path": path, "why": why})),
            Err(e) => failed.push(json!({"repo": root, "error": format!("{e:#}")})),
        }
    }
    Ok(json!({"removed": removed, "left": left, "failed": failed}))
}

fn root_of(key: &str, rec: &Rec) -> String {
    if rec.root.is_empty() {
        key.to_string()
    } else {
        rec.root.clone()
    }
}

/// What `remove` takes away.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Remove {
    /// The tree, the private refs and the record (prune).
    All,
    /// The tree only; the record keeps its refs and cache (the sweep).
    Tree,
}

/// Remove a repo's tree (the recorded one, or an unrecorded one
/// `latest::adoptable` says is rlord's), only after `latest::may_touch`;
/// with `Remove::All` its private refs and record too. All under the
/// repo's move lock, the record read and written there. A repo that is
/// gone: its record only. In a guarded folder only git runs (no stat, no
/// file touched). A folder left at the tree's path (not rlord's to
/// remove) comes back with its path and why.
fn remove(d: &Daemon, key: &str, root: &str, what: Remove) -> Result<Option<(String, String)>> {
    let _move = d.latest.moves.lock(key);
    let rec = record(d, key);
    let env = d.latest.env();
    let fs_ok = !guarded(d, &d.config(), Path::new(root));
    let gone = |d: &Daemon| {
        update(d, key, |_| false);
        Ok(None)
    };
    let repo = match latest::locate(&env, Path::new(root)) {
        Ok(Located::Repo(r)) => r,
        Ok(Located::NotGit) => return gone(d),
        // (Only "not found" is gone, and only where rlord may stat; any
        // other error keeps the record.)
        Err(_) if fs_ok && matches!(Path::new(key).try_exists(), Ok(false)) => return gone(d),
        Err(e) => return Err(e),
    };
    let list = &repo.worktrees;
    let live = latest::tree_path_seen(&env, &repo.root, fs_ok);
    // The tree: the recorded one, or one rlord can adopt.
    let current: Option<PathBuf> = match rec.tree.as_deref() {
        Some(t) => Some(PathBuf::from(t)),
        None => latest::entry_at(list, &live)
            .is_some_and(|e| latest::adoptable(e, &live))
            .then(|| live.clone()),
    };
    // (Refused, or git couldn't: with `All`, said as left with why, and
    // the refs and record go all the same.)
    let mut refused = None;
    if let Some(t) = current.as_deref() {
        if let Some(e) = latest::entry_at(list, t) {
            if let Err(e) = latest::tree_remove(&env, &repo, t, Some(t), e, fs_ok) {
                if what != Remove::All {
                    return Err(e);
                }
                refused = Some((t.to_string_lossy().into_owned(), format!("{e:#}")));
            }
        }
    }
    if what == Remove::All {
        latest::drop_refs(&env, &repo.root)?;
        d.latest.forget(&format!("{key}\0"));
        update(d, key, |_| false);
    } else {
        update(d, key, |r| {
            r.tree = None;
            r.branch = None;
            r.commit = None;
            true
        });
    }
    // Still at the tree's path, not rlord's to remove: said, with its path
    // and why, never "removed".
    if what != Remove::All {
        return Ok(None);
    }
    if refused.is_some() {
        return Ok(refused);
    }
    let path = live.to_string_lossy().into_owned();
    let not_ours = "not rlord's tree as git sees it: left in place";
    Ok(if fs_ok {
        matches!(live.try_exists(), Ok(true)).then(|| (path, not_ours.to_string()))
    } else {
        // (Guarded: git's list, no stat.)
        latest::worktree_list(&env, &repo.root)
            .ok()
            .filter(|l| latest::entry_at(l, &live).is_some())
            .map(|_| {
                (
                    path,
                    format!("{not_ours} (in a folder macOS guards: only git was used)"),
                )
            })
    })
}

/// The daily sweep, from the records only: a repo that is gone loses its
/// record; a tree unused for `UNUSED_MS`, and any old tree left over, is
/// removed (the record keeps its refs and cache). Skips a repo in a
/// guarded folder: nothing touches one unprompted (`rlord latest
/// --prune` does).
pub fn sweep(d: &Daemon) -> Value {
    let cfg = d.config();
    let now = now_ms();
    let (mut gone, mut trees, mut skipped) = (Vec::new(), Vec::new(), Vec::new());
    for (key, rec) in records(d) {
        let root = root_of(&key, &rec);
        // Guarded first: nothing in a guarded folder is touched, not even
        // a stat, unprompted (`rlord latest --prune` does).
        if guarded(d, &cfg, Path::new(&root)) {
            skipped.push(root);
            continue;
        }
        match Path::new(&key).try_exists() {
            Ok(true) => {}
            Ok(false) => {
                let _move = d.latest.moves.lock(&key);
                update(d, &key, |_| false);
                gone.push(root);
                continue;
            }
            // (Can't tell: kept.)
            Err(_) => continue,
        }
        // Private refs not fetched for long, made by the tool or by
        // launch bases alike: dropped, with what was known of them.
        {
            let _move = d.latest.moves.lock(&key);
            let rec = record(d, &key);
            let env = d.latest.env();
            let keep = rec.branch.clone().unwrap_or_default();
            // (The tree's own ref is kept by `<remote>/<branch>`.)
            let remote = latest::remote_of(&env, Path::new(&root))
                .ok()
                .and_then(|r| r.ok());
            let dropped = drop_old_refs(&env, Path::new(&root), &rec, remote.as_deref(), &keep);
            if !dropped.is_empty() {
                d.latest.forget_refs(&key, &dropped);
                update(d, &key, |r| {
                    for k in &dropped {
                        r.fetched.remove(k);
                    }
                    true
                });
            }
        }
        let stale = rec.tree.is_some() && now - rec.tree_used_at > UNUSED_MS;
        if stale {
            match remove(d, &key, &root, Remove::Tree) {
                Ok(_) => trees.push(root),
                Err(e) => tracing::warn!(repo = root, error = %e, "removing rlord's tree"),
            }
        }
    }
    json!({"gone": gone, "trees_removed": trees, "skipped": skipped})
}

/// Sweep shortly after the daemon starts, then once a day.
pub fn start_sweeper(d: &Arc<Daemon>) {
    let d = Arc::downgrade(d);
    let _ = std::thread::Builder::new()
        .name("repo-latest-sweep".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(120));
            loop {
                let Some(daemon) = d.upgrade() else {
                    return;
                };
                let v = sweep(&daemon);
                drop(daemon);
                tracing::info!(result = %v, "repo_latest sweep");
                std::thread::sleep(Duration::from_secs(24 * 3600));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlord_core::db::Db;
    use rlord_core::latest::testing::Repos;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn timing(wait_ms: u64) -> Timing {
        Timing {
            fresh: Duration::from_secs(60),
            tool_wait: Duration::from_millis(wait_ms),
            launch_wait: Duration::from_millis(wait_ms),
            failed_fresh: Duration::from_secs(120),
            named_retry: Duration::from_secs(10),
            named_base_wait: Duration::from_millis(wait_ms),
            tree_min_wait: Duration::from_millis(wait_ms),
        }
    }

    /// `session.new`'s pre-lock step, as `Daemon::call` runs it: the base.
    fn launch_base(d: &Daemon, p: &Value) -> Result<Value> {
        let mut p = p.clone();
        set_launch_base(d, &mut p)?;
        Ok(p["base_resolved"].clone())
    }

    fn daemon(r: &Repos) -> Arc<Daemon> {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        d.latest
            .set_for_test(r.env.clone(), timing(30_000), Guard::with(None, false));
        d
    }

    fn copy_records(from: &Daemon, to: &Daemon) {
        for (k, v) in from.db().kv_prefix(KV).unwrap() {
            to.db().kv_set(&k, &v).unwrap();
        }
    }

    /// A new daemon (no fetch cache) with `d`'s records.
    fn again(r: &Repos, d: &Daemon) -> Arc<Daemon> {
        let d2 = daemon(r);
        copy_records(d, &d2);
        d2
    }

    fn call(d: &Daemon, repo: &Path, args: Value) -> Value {
        let mut p = args;
        p["repo"] = json!(repo);
        latest_rpc(d, p).unwrap()
    }

    #[test]
    fn a_fresh_fetch_is_not_repeated() {
        let l = Latest::default();
        let n = Arc::new(AtomicUsize::new(0));
        let count = |n: &Arc<AtomicUsize>| {
            let n = n.clone();
            move || {
                n.fetch_add(1, Ordering::SeqCst);
                Fetch::Fetched
            }
        };
        assert_eq!(
            l.fetch("k", Duration::from_secs(5), true, false, count(&n)),
            Fetch::Fetched
        );
        assert_eq!(
            l.fetch("k", Duration::from_secs(5), true, false, count(&n)),
            Fetch::Cached
        );
        assert_eq!(n.load(Ordering::SeqCst), 1);
        // A failure is kept a short while: the same answer, no new try.
        assert_eq!(
            l.fetch("f", Duration::from_secs(5), true, false, || Fetch::Offline),
            Fetch::Offline
        );
        assert_eq!(
            l.fetch("f", Duration::from_secs(5), true, false, || panic!(
                "tried again at once"
            )),
            Fetch::Offline
        );
        // ...and tried again after it.
        l.set_for_test(
            GitEnv::default(),
            Timing {
                failed_fresh: Duration::ZERO,
                ..timing(10)
            },
            Guard::with(None, false),
        );
        assert_eq!(
            l.fetch("f", Duration::from_secs(5), true, false, || Fetch::Fetched),
            Fetch::Fetched
        );
    }

    #[test]
    fn a_slow_fetch_is_not_waited_on_and_its_result_is_picked_up() {
        let l = Latest::default();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let t = Instant::now();
        let f = l.fetch("k", Duration::from_millis(50), true, false, move || {
            let _ = rx.recv();
            Fetch::Fetched
        });
        assert_eq!(f, Fetch::Running);
        assert!(t.elapsed() < Duration::from_secs(2));
        // Still running: no second fetch.
        assert_eq!(
            l.fetch("k", Duration::from_millis(10), true, false, || panic!(
                "a second fetch"
            )),
            Fetch::Running
        );
        tx.send(()).unwrap();
        let mut got = Fetch::Running;
        for _ in 0..200 {
            got = l.fetch("k", Duration::from_millis(50), true, false, || {
                panic!("refetched")
            });
            if got != Fetch::Running {
                break;
            }
        }
        assert_eq!(got, Fetch::Fetched);
        assert_eq!(
            l.fetch("k", Duration::from_millis(50), true, false, || panic!(
                "refetched"
            )),
            Fetch::Cached
        );
        // A panicking fetch frees its slot.
        assert!(matches!(
            l.fetch("p", Duration::from_secs(5), true, false, || panic!("boom")),
            Fetch::Failed(_)
        ));
    }

    #[test]
    fn calls_for_one_repo_queue_and_others_dont_wait() {
        let l = Arc::new(Latest::default());
        let g = l.moves.lock("/a");
        let l2 = l.clone();
        let t = std::thread::spawn(move || {
            let start = Instant::now();
            drop(l2.moves.lock("/b"));
            let b = start.elapsed();
            let _g = l2.moves.lock("/a");
            (b, start.elapsed())
        });
        std::thread::sleep(Duration::from_millis(150));
        drop(g);
        let (b, a) = t.join().unwrap();
        assert!(b < Duration::from_millis(100));
        assert!(a >= Duration::from_millis(140));
    }

    #[test]
    fn the_tree_follows_the_latest_and_the_user_is_untouched() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        std::fs::write(c.join("wip.txt"), "x").unwrap();
        let before = r.snapshot();
        let one = r.g(c, &["rev-parse", "HEAD"]);
        let two = r.push_remote("main", "two");
        let v = call(&d, &c.join("."), json!({}));
        assert_eq!(v["git"], true);
        assert_eq!(v["ref"], "main");
        assert_eq!(v["default_branch"], true);
        assert_eq!(v["commit"], two.as_str());
        assert_eq!(v["source"], "remote");
        assert_eq!(v["fetch"], "fetched");
        assert_eq!(v["moved"], json!({"created": true}));
        assert_eq!(v["local"]["behind"], 1);
        // (The default branch: where it is checked out, no status.)
        assert_eq!(v["local"]["checked_out_at"], json!(c));
        assert!(v["local"]["uncommitted"].is_null(), "{v}");
        let path = PathBuf::from(v["path"].as_str().unwrap());
        assert_eq!(path, c.join(".worktrees/.rlord-latest"));
        assert_eq!(r.g(&path, &["rev-parse", "HEAD"]), two);
        // Kept out of the user's status.
        assert_eq!(r.snapshot(), before);
        // Again, nothing new: cached, not moved.
        let v = call(&d, c, json!({}));
        assert_eq!(v["fetch"], "cached");
        assert!(v.get("moved").is_none(), "{v}");
        // A push: moved forward by the count (the cache is skipped by a
        // new Latest with the same records).
        let three = r.push_remote("main", "three");
        let four = r.push_remote("main", "four");
        let d2 = again(&r, &d);
        let v = call(&d2, c, json!({}));
        assert_eq!(v["commit"], four.as_str());
        assert_eq!(v["moved"], json!({"from": two, "commits": 2}));
        let _ = three;
        // A force-push: rewritten.
        r.g(&r.other, &["reset", "-q", "--hard", &one]);
        let five = r.commit(&r.other, "five");
        r.g(&r.other, &["push", "-q", "-f", "origin", "main"]);
        let d3 = again(&r, &d2);
        let v = call(&d3, c, json!({}));
        assert_eq!(v["commit"], five.as_str());
        assert_eq!(v["moved"]["rewritten"], true);
        assert_eq!(r.snapshot(), before);
        // A branch: the same tree, moved there; the move says from where.
        let feat = r.push_remote("feat", "feat");
        let v = call(&d3, c, json!({"ref": "feat"}));
        assert_eq!(v["commit"], feat.as_str());
        assert_eq!(v["path"], json!(path));
        assert_eq!(v["moved"]["from_ref"], "main");
        assert_eq!(r.g(&path, &["rev-parse", "HEAD"]), feat);
        // `origin/feat` names the remote's.
        let v = call(&d3, c, json!({"ref": "origin/feat"}));
        assert_eq!(v["ref"], "feat");
        assert_eq!(r.snapshot(), before);
        // Listed, then pruned: tree and private refs gone.
        let l = list_rpc(&d3).unwrap();
        assert_eq!(l["trees"][0]["ref"], "feat");
        let p = prune_rpc(&d3, json!({"all": true})).unwrap();
        assert_eq!(p["removed"].as_array().unwrap().len(), 1, "{p}");
        assert!(!path.exists());
        assert!(r.g(c, &["for-each-ref", latest::REF_PREFIX]).is_empty());
        assert_eq!(list_rpc(&d3).unwrap()["trees"], json!([]));
        assert_eq!(r.snapshot(), before);
    }

    /// The RPCs and the MCP tool, through the daemon's own dispatch.
    #[test]
    fn rpcs_and_the_tool_are_wired() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        let v = crate::mcp::call(&d, "repo_latest", &json!({"repo": c})).unwrap();
        assert_eq!(v["ref"], "main");
        assert_eq!(v["commit"].as_str().unwrap().len(), 7);
        assert_eq!(v["moved"], json!({"created": true}));
        let l = d.rpc("repo.latest_list", json!({})).unwrap();
        assert_eq!(l["trees"].as_array().unwrap().len(), 1);
        let p = d.rpc("repo.latest_prune", json!({"repo": c})).unwrap();
        assert_eq!(p["removed"], json!([c]));
        assert!(!c.join(".worktrees/.rlord-latest").exists());
    }

    #[test]
    fn local_ahead_and_diverged_branches() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        // An unpushed commit: the local one.
        let mine = r.commit(c, "mine");
        let v = call(&d, c, json!({"ref": "main"}));
        assert_eq!(v["commit"], mine.as_str());
        assert_eq!(v["source"], "local");
        assert_eq!(v["local"]["ahead"], 1);
        let path = PathBuf::from(v["path"].as_str().unwrap());
        // Diverged: no tree is moved, both sides come back.
        let theirs = r.push_remote("main", "theirs");
        let d = again(&r, &d);
        let v = call(&d, c, json!({"ref": "main"}));
        assert_eq!(v["diverged"], true, "{v}");
        assert!(v.get("path").is_none());
        assert_eq!(v["local"]["commit"], mine.as_str());
        assert_eq!(v["remote_side"]["commit"], theirs.as_str());
        assert_eq!(v["remote_side"]["subject"], "theirs");
        assert_eq!(r.g(&path, &["rev-parse", "HEAD"]), mine);
        // The user chose.
        let v = call(&d, c, json!({"ref": "main", "prefer": "remote"}));
        assert_eq!(v["commit"], theirs.as_str());
        // The default branch: the remote wins, with a warning.
        let v = call(&d, c, json!({}));
        assert_eq!(v["commit"], theirs.as_str());
        assert!(v["warnings"].to_string().contains("diverged"), "{v}");
        // A branch nowhere: an error.
        let e = latest_rpc(&d, json!({"repo": c, "ref": "nope"})).unwrap_err();
        assert!(e.to_string().contains("no branch nope"), "{e}");
        assert!(latest_rpc(&d, json!({"repo": c, "ref": "-x"})).is_err());
    }

    #[test]
    fn no_remote_unreachable_remote_and_not_a_repo() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        let v = call(&d, c, json!({}));
        let first = v["commit"].clone();
        // The remote is gone: the last known commit, a warning.
        std::fs::rename(&r.remote, r.root().join("moved.git")).unwrap();
        let d = again(&r, &d);
        let v = call(&d, c, json!({}));
        assert_eq!(v["commit"], first);
        assert!(
            matches!(v["fetch"].as_str(), Some("failed" | "offline")),
            "{v}"
        );
        assert!(!v["warnings"].as_array().unwrap().is_empty());
        // No remote at all.
        let solo = r.root().join("solo");
        r.g(&r.root(), &["init", "-q", "-b", "main", "solo"]);
        let c1 = r.commit(&solo, "x");
        let v = call(&d, &solo, json!({}));
        assert_eq!(v["fetch"], "no_remote");
        assert_eq!(v["commit"], c1.as_str());
        // Not a repo.
        let plain = r.root().join("plain");
        std::fs::create_dir(&plain).unwrap();
        d.latest.set_for_test(
            r.env
                .with("GIT_CEILING_DIRECTORIES", r.root().to_string_lossy().into()),
            timing(30_000),
            Guard::with(None, false),
        );
        let v = call(&d, &plain, json!({}));
        assert_eq!(v["git"], false);
        assert_eq!(v["path"], json!(plain));
    }

    #[test]
    fn launch_base_picks_the_latest_default_branch() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        let newer = r.push_remote("main", "newer");
        let p = json!({"repo": c, "name": "fix it"});
        let b = launch_base(&d, &p).unwrap();
        assert_eq!(b["commit"], newer.as_str(), "{b}");
        assert_eq!(b["label"], "origin/main");
        assert_eq!(b["fetch"], "fetched");
        let note = base_note(&b, now_ms()).unwrap();
        assert!(
            note.starts_with(&format!(
                "Your branch started from origin/main at {} (\"newer\", committed",
                &newer[..7]
            )),
            "{note}"
        );
        assert!(note.ends_with("fetched just now."), "{note}");
        // An existing branch, no worktree, or `head`: nothing / head.
        r.g(c, &["branch", "fix-it"]);
        assert_eq!(launch_base(&d, &p).unwrap(), Value::Null);
        let p2 = json!({"repo": c, "name": "other", "workspace": false});
        assert_eq!(launch_base(&d, &p2).unwrap(), Value::Null);
        let p3 = json!({"repo": c, "name": "other", "base": "head"});
        assert_eq!(
            launch_base(&d, &p3).unwrap(),
            json!({"head": true, "why": "asked for"})
        );
        // A named base that has diverged: refused, both sides said.
        r.g(c, &["checkout", "-q", "-b", "feat"]);
        r.commit(c, "mine");
        r.g(c, &["checkout", "-q", "main"]);
        r.push_remote("feat", "theirs");
        let p4 = json!({"repo": c, "name": "stack", "base": "feat"});
        let e = launch_base(&d, &p4).unwrap_err().to_string();
        assert!(
            e.contains("diverged") && e.contains("mine") && e.contains("theirs"),
            "{e}"
        );
        let p5 = json!({"repo": c, "name": "stack", "base": "origin/feat"});
        assert_eq!(launch_base(&d, &p5).unwrap()["label"], "origin/feat");
        // The remote unreachable: still a base, never a refusal.
        std::fs::rename(&r.remote, r.root().join("moved.git")).unwrap();
        let d = daemon(&r);
        let b = launch_base(&d, &json!({"repo": c, "name": "x2"})).unwrap();
        assert!(b["commit"].is_string(), "{b}");
        let note = base_note(&b, now_ms()).unwrap();
        assert!(note.contains("it may be behind"), "{note}");
    }

    #[test]
    fn a_new_branch_starts_at_the_base_and_an_existing_one_as_it_is() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        let repo = c.to_string_lossy().into_owned();
        let head = r.g(c, &["rev-parse", "HEAD"]);
        let newer = r.push_remote("main", "newer");
        // A caller's own base_resolved never survives.
        let mut p = json!({"repo": c, "name": "fix", "base_resolved": {"commit": head}});
        set_launch_base(&d, &mut p).unwrap();
        assert_eq!(p["base_resolved"]["commit"], newer.as_str());
        let mut q = json!({"repo": c, "workspace": false, "base_resolved": {"commit": head}});
        set_launch_base(&d, &mut q).unwrap();
        assert_eq!(q["base_resolved"], Value::Null);

        let dir = c.join(".worktrees/fix");
        let used = crate::methods::new_worktree(&repo, "fix", &dir, &p["base_resolved"]).unwrap();
        assert_eq!(used["label"], "origin/main");
        assert_eq!(r.g(&dir, &["rev-parse", "HEAD"]), newer);
        // Not tracking rlord's private ref.
        let up = run_ok(&r, &dir, &["rev-parse", "--abbrev-ref", "@{upstream}"]);
        assert!(!up, "the new branch has an upstream");
        // An existing branch: no base (`launch_base` gives none), checked
        // out as it is. A start given for one (it appeared since) is
        // refused, never checked out somewhere else.
        r.g(c, &["branch", "old", &head]);
        let dir2 = c.join(".worktrees/old");
        let e = crate::methods::new_worktree(&repo, "old", &dir2, &p["base_resolved"]).unwrap_err();
        assert!(e.to_string().contains("already exists"), "{e}");
        assert!(!dir2.exists());
        let q = json!({"repo": c, "name": "old"});
        assert_eq!(launch_base(&d, &q).unwrap(), Value::Null);
        let used = crate::methods::new_worktree(&repo, "old", &dir2, &Value::Null).unwrap();
        assert_eq!(used, Value::Null);
        assert_eq!(r.g(&dir2, &["rev-parse", "HEAD"]), head);
        // `head`: today's start, the main checkout's HEAD.
        let dir3 = c.join(".worktrees/h");
        let used = crate::methods::new_worktree(&repo, "h", &dir3, &json!({"head": true})).unwrap();
        // (With the commit it started at, for its note.)
        assert_eq!(used, json!({"head": true, "commit": head}));
        assert_eq!(r.g(&dir3, &["rev-parse", "HEAD"]), head);
    }

    fn run_ok(r: &Repos, dir: &Path, args: &[&str]) -> bool {
        latest::run(&r.env, dir, args, latest::PLUMBING).unwrap().ok
    }

    #[test]
    fn launch_base_with_base_head_configured_fetches_nothing() {
        let r = Repos::new();
        let mut cfg = Config::default();
        cfg.session.base = "head".into();
        let d = Daemon::new(Db::open_in_memory().unwrap(), cfg);
        d.latest
            .set_for_test(r.env.clone(), timing(30_000), Guard::with(None, false));
        let p = json!({"repo": r.clone, "name": "x"});
        assert_eq!(
            launch_base(&d, &p).unwrap(),
            json!({"head": true, "why": "[session] base = \"head\""})
        );
        assert!(r
            .g(&r.clone, &["for-each-ref", latest::REF_PREFIX])
            .is_empty());
    }

    #[test]
    fn notes_for_each_fetch_outcome() {
        let now = 1_800_000_000_000;
        let b = |fetch: &str, fetched_at: Option<i64>| {
            json!({"commit": "abcdef1234", "label": "origin/main", "subject": "Fix",
                   "committed_at": now - 3 * 3600 * 1000, "fetch": fetch,
                   "fetched_at": fetched_at})
        };
        // The HEAD fallback still gets a note: where, and why.
        let n = base_note(
            &json!({"head": true, "commit": "abcdef1234", "why": "[session] base = \"head\""}),
            now,
        )
        .unwrap();
        assert!(
            n.contains("main checkout's HEAD at abcdef1 ([session] base = \"head\")"),
            "{n}"
        );
        assert_eq!(base_note(&json!({"fetch": "fetched"}), now), None);
        // A local-only branch: nothing to be behind.
        let n = base_note(&b("missing", None), now).unwrap();
        assert!(
            n.contains("isn't on the remote") && !n.contains("behind"),
            "{n}"
        );
        let n = base_note(&b("offline", Some(now - 3 * 86_400_000)), now).unwrap();
        assert!(n.contains("abcdef1 (\"Fix\", committed"), "{n}");
        assert!(
            n.contains("as last fetched") && n.contains("(offline): it may be behind"),
            "{n}"
        );
        let n = base_note(&b("no_remote", None), now).unwrap();
        assert!(n.contains("no remote"), "{n}");
    }

    fn put_rec(d: &Daemon, key: &Path, rec: Rec) {
        update(d, &key.to_string_lossy(), |r| {
            *r = rec;
            true
        });
    }

    fn key(c: &Path) -> String {
        c.join(".git").to_string_lossy().into_owned()
    }

    #[test]
    fn the_sweep_drops_gone_repos_and_skips_guarded_ones() {
        let r = Repos::new();
        let d = daemon(&r);
        let home = r.root().join("home");
        let gone = r.root().join("gone/.git");
        let guarded = home.join("Documents/app/.git");
        // (The guarded repo is there: only its folder is guarded.)
        std::fs::create_dir_all(&guarded).unwrap();
        for k in [&gone, &guarded] {
            let root = k.parent().unwrap().to_string_lossy().into_owned();
            put_rec(
                &d,
                k,
                Rec {
                    root,
                    used_at: 1,
                    ..Rec::default()
                },
            );
        }
        d.latest.set_for_test(
            r.env.clone(),
            timing(30_000),
            Guard::with(Some(&home), false),
        );
        let v = sweep(&d);
        assert_eq!(v["gone"], json!([gone.parent().unwrap()]));
        assert_eq!(v["skipped"], json!([guarded.parent().unwrap()]));
        assert_eq!(records(&d).len(), 1);

        // A tree unused for a month goes; its refs and record stay.
        let d = daemon(&r);
        let c = &r.clone;
        let v = call(&d, c, json!({}));
        let path = PathBuf::from(v["path"].as_str().unwrap());
        let k = key(c);
        update(&d, &k, |rec| {
            rec.tree_used_at -= UNUSED_MS + 1000;
            true
        });
        let v = sweep(&d);
        assert_eq!(v["trees_removed"], json!([c]));
        assert!(!path.exists());
        assert!(!r.g(c, &["for-each-ref", latest::REF_PREFIX]).is_empty());
        let rec = record(&d, &k);
        assert_eq!((rec.tree, rec.root.is_empty()), (None, false));
        assert!(!rec.fetched.is_empty());
        // A launch-only record (no tree) is kept, however old.
        update(&d, &k, |rec| {
            rec.used_at = 1;
            true
        });
        sweep(&d);
        assert_eq!(records(&d).len(), 1);
    }

    /// A result no caller waited for is handed over only while fresh;
    /// an older one is dropped and the fetch runs again.
    #[test]
    fn a_stale_unseen_result_is_not_reported_as_fresh() {
        let l = Latest::default();
        l.set_for_test(
            GitEnv::default(),
            Timing {
                fresh: Duration::from_millis(100),
                failed_fresh: Duration::from_millis(100),
                ..timing(10)
            },
            Guard::with(None, false),
        );
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let f = l.fetch("k", Duration::from_millis(10), true, false, move || {
            let _ = rx.recv();
            Fetch::Offline
        });
        assert_eq!(f, Fetch::Running);
        tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let f = l.fetch("k", Duration::from_secs(5), true, false, move || {
            n2.fetch_add(1, Ordering::SeqCst);
            Fetch::Fetched
        });
        assert_eq!(f, Fetch::Fetched);
        assert_eq!(n.load(Ordering::SeqCst), 1);
        // Two waiters on one fetch both see its outcome.
        let l = Arc::new(Latest::default());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let l2 = l.clone();
        let t = std::thread::spawn(move || {
            l2.fetch("w", Duration::from_secs(5), true, false, move || {
                let _ = rx.recv();
                Fetch::Offline
            })
        });
        std::thread::sleep(Duration::from_millis(50));
        let l3 = l.clone();
        let t2 = std::thread::spawn(move || {
            l3.fetch("w", Duration::from_secs(5), true, false, || {
                panic!("a second fetch")
            })
        });
        std::thread::sleep(Duration::from_millis(50));
        tx.send(()).unwrap();
        assert_eq!(t.join().unwrap(), Fetch::Offline);
        assert_eq!(t2.join().unwrap(), Fetch::Offline);
    }

    /// A session's worktree, at the tree's old name or on a branch at its
    /// new one, is never moved or removed.
    #[test]
    fn session_worktrees_are_left_alone() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        let head = r.g(c, &["rev-parse", "HEAD"]);
        r.push_remote("main", "newer");
        // A session named `rlord-latest`: its worktree at the old name.
        let old = c.join(".worktrees/rlord-latest");
        r.g(
            c,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "rlord-latest",
                old.to_str().unwrap(),
            ],
        );
        std::fs::write(old.join("work.txt"), "mine").unwrap();
        let v = call(&d, c, json!({}));
        let path = PathBuf::from(v["path"].as_str().unwrap());
        assert_eq!(path, c.join(".worktrees/.rlord-latest"));
        assert_eq!(r.g(&old, &["rev-parse", "HEAD"]), head);
        assert!(old.join("work.txt").exists());
        // The record pointed at the session's worktree (it never is, but
        // say it were): a move and a prune leave it as it is.
        let k = key(c);
        update(&d, &k, |rec| {
            rec.tree = Some(old.to_string_lossy().into_owned());
            true
        });
        prune_rpc(&d, json!({"repo": c})).unwrap();
        assert!(old.join("work.txt").exists());
        assert_eq!(
            r.g(&old, &["symbolic-ref", "--short", "HEAD"]),
            "rlord-latest"
        );
        // A worktree on a branch at the new name, unrecorded: refused.
        r.g(
            c,
            &["worktree", "remove", "--force", path.to_str().unwrap()],
        );
        r.g(
            c,
            &["worktree", "add", "-q", "-b", "s2", path.to_str().unwrap()],
        );
        std::fs::write(path.join("work.txt"), "mine").unwrap();
        let e = latest_rpc(&d, json!({"repo": c})).unwrap_err().to_string();
        assert!(e.contains("left as it is"), "{e}");
        // Prune leaves it (unrecorded, on a branch).
        let p = prune_rpc(&d, json!({"repo": c})).unwrap();
        assert!(path.join("work.txt").exists(), "{p}");
        assert_eq!(r.g(&path, &["symbolic-ref", "--short", "HEAD"]), "s2");
        // Recorded as rlord's tree (by name and record): a branch checked
        // out in it is detached again, not refused forever.
        update(&d, &k, |rec| {
            rec.tree = Some(path.to_string_lossy().into_owned());
            true
        });
        let v = latest_rpc(&d, json!({"repo": c})).unwrap();
        assert_eq!(v["path"], json!(path));
        assert!(!run_ok(&r, &path, &["symbolic-ref", "-q", "HEAD"]));
    }

    /// A tree whose record was lost is adopted and moved, not refused;
    /// prune finds it too.
    #[test]
    fn a_tree_with_no_record_is_adopted() {
        let r = Repos::new();
        let c = &r.clone;
        let v = call(&daemon(&r), c, json!({}));
        let path = PathBuf::from(v["path"].as_str().unwrap());
        r.push_remote("main", "two");
        // A daemon with no records (a kv reset).
        let d = daemon(&r);
        let v = call(&d, c, json!({}));
        assert_eq!(v["path"], json!(path), "{v}");
        assert!(v["warnings"].to_string().contains("adopted"), "{v}");
        assert_eq!(
            r.g(&path, &["rev-parse", "HEAD"]),
            v["commit"].as_str().unwrap()
        );
        assert_eq!(
            record(&d, &key(c)).tree,
            Some(path.to_string_lossy().into_owned())
        );
        // Prune with no record finds it as well.
        let d = daemon(&r);
        let p = prune_rpc(&d, json!({"repo": c})).unwrap();
        assert_eq!(p["removed"], json!([c]), "{p}");
        assert!(!path.exists());
    }

    /// `workspace_dir = "../wt"` (shared by sibling repos): each repo's
    /// tree is still in its own `.worktrees`, so they never collide.
    #[test]
    fn sibling_repos_never_share_a_tree() {
        let r = Repos::new();
        let mut cfg = Config::default();
        cfg.session.workspace_dir = "../wt".into();
        let d = Daemon::new(Db::open_in_memory().unwrap(), cfg);
        d.latest
            .set_for_test(r.env.clone(), timing(30_000), Guard::with(None, false));
        let a = call(&d, &r.clone, json!({}));
        let b = call(&d, &r.other, json!({}));
        assert_eq!(a["path"], json!(r.clone.join(".worktrees/.rlord-latest")));
        assert_eq!(b["path"], json!(r.other.join(".worktrees/.rlord-latest")));
        assert!(!r.root().join("wt").exists());
        // One kv key per repo.
        assert_eq!(d.db().kv_prefix(KV).unwrap().len(), 2);
    }

    #[test]
    fn a_launch_waits_at_most_its_budget_and_a_late_fetch_is_recorded() {
        let r = Repos::new();
        let c = &r.clone;
        r.g(c, &["symbolic-ref", "-d", "refs/remotes/origin/HEAD"]);
        let slow = r.root().join("slow-upload-pack");
        std::fs::write(&slow, "#!/bin/sh\nsleep 1\nexec git-upload-pack \"$@\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        r.g(
            c,
            &["config", "remote.origin.uploadpack", slow.to_str().unwrap()],
        );
        let d = daemon(&r);
        d.latest
            .set_for_test(r.env.clone(), timing(300), Guard::with(None, false));
        let t = Instant::now();
        let b = launch_base(&d, &json!({"repo": c, "name": "x"})).unwrap();
        assert!(
            t.elapsed() < Duration::from_millis(900),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(b["fetch"], "running", "{b}");
        assert!(b["commit"].is_string(), "{b}");
        let mut got = false;
        for _ in 0..100 {
            if !record(&d, &key(c)).fetched.is_empty() {
                got = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(got, "the late fetch wasn't recorded");
    }

    /// A named base not here yet whose fetch is still running: said so,
    /// not "no such branch".
    #[test]
    fn a_named_base_still_fetching_says_so() {
        let r = Repos::new();
        let c = &r.clone;
        r.push_remote("feat", "feat");
        let slow = r.root().join("slow-upload-pack");
        std::fs::write(&slow, "#!/bin/sh\nsleep 1\nexec git-upload-pack \"$@\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        r.g(
            c,
            &["config", "remote.origin.uploadpack", slow.to_str().unwrap()],
        );
        // (The clone's own origin/feat would answer: not fetched yet.)
        let d = daemon(&r);
        d.latest
            .set_for_test(r.env.clone(), timing(200), Guard::with(None, false));
        let e = launch_base(&d, &json!({"repo": c, "name": "s", "base": "feat"}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("still fetching feat from origin"), "{e}");
    }

    /// The default branch is the remote's even when the local one is
    /// ahead; a `local_ahead` warning says so. A named one: the newer.
    #[test]
    fn the_default_branch_is_the_remotes() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        let theirs = r.g(c, &["rev-parse", "HEAD"]);
        let mine = r.commit(c, "mine");
        let v = call(&d, c, json!({}));
        assert_eq!(v["commit"], theirs.as_str(), "{v}");
        assert!(v["warnings"].to_string().contains("local_ahead"), "{v}");
        // (The default branch: where it is checked out, no status.)
        assert!(
            v["local"].get("uncommitted").is_none_or(Value::is_null),
            "{v}"
        );
        let v = call(&d, c, json!({"ref": "main"}));
        assert_eq!(v["commit"], mine.as_str());
        assert!(!v["local"]["uncommitted"].is_null(), "{v}");
    }

    /// Prune reports a git error as failed and keeps the record; only a
    /// repo that is gone loses it.
    #[test]
    fn prune_keeps_the_record_on_a_git_error() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        call(&d, c, json!({}));
        // The record points at a root git can't read, the repo still there.
        let k = key(c);
        update(&d, &k, |rec| {
            rec.root = r.root().join("nowhere").to_string_lossy().into_owned();
            true
        });
        let p = prune_rpc(&d, json!({"all": true})).unwrap();
        assert_eq!(p["failed"].as_array().unwrap().len(), 1, "{p}");
        assert!(!record(&d, &k).root.is_empty());
    }

    /// `origin/<b>`: the remote's commit even when the local branch is
    /// ahead, with a `local_ahead` warning.
    #[test]
    fn a_remote_prefix_means_the_remotes() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        let theirs = r.push_remote("feat", "theirs");
        r.g(c, &["fetch", "-q", "origin"]);
        r.g(c, &["branch", "feat", "origin/feat"]);
        r.g(c, &["checkout", "-q", "feat"]);
        let mine = r.commit(c, "mine");
        r.g(c, &["checkout", "-q", "main"]);
        let v = call(&d, c, json!({"ref": "origin/feat"}));
        assert_eq!(v["commit"], theirs.as_str(), "{v}");
        assert!(v["warnings"].to_string().contains("local_ahead"), "{v}");
        // Plain `feat`: the newer, the local one.
        let v = call(&d, c, json!({"ref": "feat"}));
        assert_eq!(v["commit"], mine.as_str());
    }

    /// A tree whose record has no completed commit (an add or move killed
    /// partway) is checked out in full, even with HEAD at the commit.
    #[test]
    fn an_unfinished_tree_is_completed() {
        let r = Repos::new();
        let c = &r.clone;
        std::fs::write(r.other.join("f.txt"), "x").unwrap();
        r.g(&r.other, &["add", "f.txt"]);
        r.commit(&r.other, "f");
        r.g(&r.other, &["push", "-q", "origin", "main"]);
        let d = daemon(&r);
        let v = call(&d, c, json!({}));
        let path = PathBuf::from(v["path"].as_str().unwrap());
        std::fs::remove_file(path.join("f.txt")).unwrap();
        // Completed on record: one status finds the edit; put right.
        let v = call(&d, c, json!({}));
        assert!(path.join("f.txt").exists(), "{v}");
        assert!(v["warnings"].to_string().contains("discarded"), "{v}");
        std::fs::remove_file(path.join("f.txt")).unwrap();
        // Killed partway: no completed commit on record.
        update(&d, &key(c), |rec| {
            rec.commit = None;
            true
        });
        let v = call(&d, c, json!({}));
        assert!(path.join("f.txt").exists(), "{v}");
        assert!(record(&d, &key(c)).commit.is_some());
    }

    /// A deleted private ref is never reported as `cached`: it is fetched
    /// again.
    #[test]
    fn a_deleted_private_ref_is_fetched_again() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        assert_eq!(call(&d, c, json!({}))["fetch"], "fetched");
        assert_eq!(call(&d, c, json!({}))["fetch"], "cached");
        latest::drop_refs(&r.env, c).unwrap();
        assert_eq!(call(&d, c, json!({}))["fetch"], "fetched");
        // Prune forgets the repo's fetches too.
        prune_rpc(&d, json!({"repo": c})).unwrap();
        assert_eq!(call(&d, c, json!({}))["fetch"], "fetched");
    }

    /// A launch never takes the checkout's current branch for the default
    /// one: with no origin/HEAD, no main/master and the remote not
    /// answering, it starts at HEAD, and says so.
    #[test]
    fn a_launch_never_takes_the_checkouts_branch_for_the_default() {
        let r = Repos::new();
        let c = &r.clone;
        r.g(c, &["symbolic-ref", "-d", "refs/remotes/origin/HEAD"]);
        r.g(c, &["checkout", "-q", "-b", "wip"]);
        r.g(c, &["branch", "-D", "main"]);
        r.g(c, &["update-ref", "-d", "refs/remotes/origin/main"]);
        std::fs::rename(&r.remote, r.root().join("gone.git")).unwrap();
        let d = daemon(&r);
        let b = launch_base(&d, &json!({"repo": c, "name": "x"})).unwrap();
        assert_eq!(b["head"], true, "{b}");
        assert!(
            b["why"].as_str().unwrap().contains("couldn't be found"),
            "{b}"
        );
        assert!(base_note(&b, now_ms())
            .unwrap()
            .contains("main checkout's HEAD"));
    }

    /// A named base that had diverged: refused, then launched from the
    /// side the user chose (`prefer`).
    #[test]
    fn a_diverged_base_launches_from_the_chosen_side() {
        let r = Repos::new();
        let c = &r.clone;
        let theirs = r.push_remote("feat", "theirs");
        r.g(c, &["fetch", "-q", "origin"]);
        r.g(c, &["branch", "feat", "origin/feat~0"]);
        r.g(c, &["checkout", "-q", "feat"]);
        r.g(c, &["reset", "-q", "--hard", "HEAD~1"]);
        let mine = r.commit(c, "mine");
        r.g(c, &["checkout", "-q", "main"]);
        let d = daemon(&r);
        let p = |prefer: Option<&str>| json!({"repo": c, "name": "s", "base": "feat", "prefer": prefer});
        let e = launch_base(&d, &p(None)).unwrap_err().to_string();
        assert!(e.contains("diverged") && e.contains("prefer"), "{e}");
        assert_eq!(
            launch_base(&d, &p(Some("local"))).unwrap()["commit"],
            mine.as_str()
        );
        assert_eq!(
            launch_base(&d, &p(Some("remote"))).unwrap()["commit"],
            theirs.as_str()
        );
    }

    /// A named branch's failed fetch is tried again sooner than the
    /// default branch's.
    #[test]
    fn a_named_branchs_failure_is_retried_soon() {
        let l = Latest::default();
        l.set_for_test(
            GitEnv::default(),
            Timing {
                named_retry: Duration::from_millis(50),
                ..timing(10)
            },
            Guard::with(None, false),
        );
        let w = Duration::from_secs(5);
        assert_eq!(
            l.fetch("n", w, false, true, || Fetch::Missing),
            Fetch::Missing
        );
        assert_eq!(
            l.fetch("d", w, false, false, || Fetch::Missing),
            Fetch::Missing
        );
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(
            l.fetch("n", w, false, true, || Fetch::Fetched),
            Fetch::Fetched
        );
        assert_eq!(
            l.fetch("d", w, false, false, || panic!(
                "retried the default branch"
            )),
            Fetch::Missing
        );
    }

    /// The sweep that can't remove the live tree keeps it recorded as the
    /// tree.
    #[test]
    fn a_live_tree_that_wont_go_stays_the_tree() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        call(&d, c, json!({}));
        let path = c.join(".worktrees/.rlord-latest");
        // (Made unremovable: put on a branch, as a session's would be.)
        r.g(&path, &["checkout", "-q", "-b", "taken"]);
        update(&d, &key(c), |rec| {
            rec.tree_used_at = 1;
            true
        });
        sweep(&d);
        let rec = record(&d, &key(c));
        assert_eq!(rec.tree, Some(path.to_string_lossy().into_owned()));
        assert!(path.exists());
    }

    /// A tree is recorded only once it is added: a foreign folder in its
    /// place leaves no record of a tree.
    #[test]
    fn the_tree_is_recorded_only_once_added() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        let path = c.join(".worktrees/.rlord-latest");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("mine.txt"), "x").unwrap();
        assert!(latest_rpc(&d, json!({"repo": c})).is_err());
        assert_eq!(record(&d, &key(c)).tree, None);
        assert!(path.join("mine.txt").exists());
    }

    /// A launch's cheap checks run before its base: a refused one fetches
    /// nothing. Its branch comes from the location's one rule.
    #[test]
    fn a_refused_launch_fetches_nothing_and_the_branch_has_one_rule() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        let p = json!({"repo": c, "name": "x", "context": "y".repeat(9000)});
        let e = crate::methods::precheck_new(&d.db(), &d.config(), &p).unwrap_err();
        assert!(e.to_string().contains("8000"), "{e}");
        let (_, _, branch) =
            crate::methods::new_location(&d.config(), &json!({"repo": c, "name": "Fix it!"}))
                .unwrap();
        assert_eq!(branch.as_deref(), Some("Fix-it"));
        let (_, _, branch) = crate::methods::new_location(
            &d.config(),
            &json!({"repo": c, "name": "x", "branch": "b/1"}),
        )
        .unwrap();
        assert_eq!(branch.as_deref(), Some("b/1"));
        // An existing branch named by the rule: no base.
        r.g(c, &["branch", "Fix-it"]);
        assert_eq!(
            launch_base(&d, &json!({"repo": c, "name": "Fix it!"})).unwrap(),
            Value::Null
        );
    }

    /// A launch that can't tell the default branch (the remote asked and
    /// failing) still records the failure.
    #[test]
    fn a_launchs_failed_ls_remote_is_recorded() {
        let r = Repos::new();
        let c = &r.clone;
        r.g(c, &["symbolic-ref", "-d", "refs/remotes/origin/HEAD"]);
        r.g(c, &["checkout", "-q", "-b", "wip"]);
        r.g(c, &["branch", "-D", "main"]);
        r.g(c, &["update-ref", "-d", "refs/remotes/origin/main"]);
        std::fs::rename(&r.remote, r.root().join("gone.git")).unwrap();
        let d = daemon(&r);
        let b = launch_base(&d, &json!({"repo": c, "name": "x"})).unwrap();
        assert_eq!(b["head"], true, "{b}");
        let rec = record(&d, &key(c));
        assert!(rec.ls_failed_at > 0);
        // (Never a record without its repo's root.)
        assert_eq!(rec.root, c.to_string_lossy());
    }

    /// An unseen failure of a named branch older than `named_retry` is
    /// not handed back: the fetch runs again.
    #[test]
    fn an_unseen_named_failure_is_retried() {
        let l = Latest::default();
        l.set_for_test(
            GitEnv::default(),
            Timing {
                named_retry: Duration::from_millis(50),
                ..timing(10)
            },
            Guard::with(None, false),
        );
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let f = l.fetch("n", Duration::from_millis(10), false, true, move || {
            let _ = rx.recv();
            Fetch::Missing
        });
        assert_eq!(f, Fetch::Running);
        tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let f = l.fetch("n", Duration::from_secs(5), false, true, || Fetch::Fetched);
        assert_eq!(f, Fetch::Fetched);
    }

    /// An add cut off partway (git's registration left, locked as it
    /// was being made, files missing, no record): adopted and completed.
    #[test]
    fn a_half_added_tree_is_completed() {
        let r = Repos::new();
        let c = &r.clone;
        std::fs::write(r.other.join("f.txt"), "x").unwrap();
        r.g(&r.other, &["add", "f.txt"]);
        let head = r.commit(&r.other, "f");
        r.g(&r.other, &["push", "-q", "origin", "main"]);
        r.g(c, &["fetch", "-q", "origin"]);
        let path = c.join(".worktrees/.rlord-latest");
        r.g(
            c,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                path.to_str().unwrap(),
                &head,
            ],
        );
        std::fs::remove_file(path.join("f.txt")).unwrap();
        let admin = r.g(&path, &["rev-parse", "--absolute-git-dir"]);
        std::fs::write(Path::new(&admin).join("locked"), "initializing").unwrap();
        let d = daemon(&r);
        let v = call(&d, c, json!({}));
        assert_eq!(v["path"], json!(path), "{v}");
        assert!(path.join("f.txt").exists(), "{v}");
        assert!(record(&d, &key(c)).commit.is_some());
    }

    /// A named base only on the remote, whose first fetch takes longer
    /// than the 4 s launch wait: waited on (up to `named_base_wait`), not
    /// refused.
    #[test]
    fn a_named_base_is_waited_on_longer() {
        let r = Repos::new();
        let c = &r.clone;
        let feat = r.push_remote("feat", "feat");
        let slow = r.root().join("slow-upload-pack");
        std::fs::write(&slow, "#!/bin/sh\nsleep 1\nexec git-upload-pack \"$@\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        r.g(
            c,
            &["config", "remote.origin.uploadpack", slow.to_str().unwrap()],
        );
        let d = daemon(&r);
        d.latest.set_for_test(
            r.env.clone(),
            Timing {
                named_base_wait: Duration::from_secs(10),
                ..timing(200)
            },
            Guard::with(None, false),
        );
        let b = launch_base(&d, &json!({"repo": c, "name": "s", "base": "feat"})).unwrap();
        assert_eq!(b["commit"], feat.as_str(), "{b}");
    }

    /// `origin/<b>` when the remote has no such branch: refused, never the
    /// local branch of that name instead.
    #[test]
    fn an_explicit_remote_branch_never_falls_back_to_the_local_one() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        r.g(c, &["branch", "only-here"]);
        let e = latest_rpc(&d, json!({"repo": c, "ref": "origin/only-here"}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("origin has no branch only-here"), "{e}");
        // Plain `only-here`: the local one.
        assert!(latest_rpc(&d, json!({"repo": c, "ref": "only-here"})).is_ok());
    }

    /// The sweep never stats a repo in a guarded folder, gone or not: it
    /// is skipped, its record kept.
    #[test]
    fn the_sweep_never_stats_a_guarded_repo() {
        let r = Repos::new();
        let d = daemon(&r);
        let home = r.root().join("home");
        let gone_guarded = home.join("Documents/gone/.git");
        put_rec(
            &d,
            &gone_guarded,
            Rec {
                root: gone_guarded
                    .parent()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                used_at: 1,
                ..Rec::default()
            },
        );
        d.latest.set_for_test(
            r.env.clone(),
            timing(30_000),
            Guard::with(Some(&home), false),
        );
        let v = sweep(&d);
        assert_eq!(v["gone"], json!([]));
        assert_eq!(v["skipped"].as_array().unwrap().len(), 1);
        assert_eq!(records(&d).len(), 1);
    }

    /// No default branch to be found (no remote, no main/master): the
    /// checkout's branch is shown, never as the default, with a warning.
    #[test]
    fn the_checkouts_branch_is_never_presented_as_the_default() {
        let r = Repos::new();
        let d = daemon(&r);
        let solo = r.root().join("solo");
        r.g(&r.root(), &["init", "-q", "-b", "dev", "solo"]);
        r.commit(&solo, "x");
        let v = call(&d, &solo, json!({}));
        assert_eq!(v["ref"], "dev");
        assert_eq!(v["default_branch"], false, "{v}");
        assert!(
            v["warnings"]
                .to_string()
                .contains("couldn't find the default branch"),
            "{v}"
        );
    }

    /// `origin/<b>` with `prefer: local`: the remote's, and a warning that
    /// `prefer` was ignored.
    #[test]
    fn prefer_local_is_ignored_for_an_explicit_remote() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        let theirs = r.push_remote("feat", "theirs");
        r.g(c, &["fetch", "-q", "origin"]);
        r.g(c, &["branch", "feat", "origin/feat"]);
        r.g(c, &["checkout", "-q", "feat"]);
        r.commit(c, "mine");
        r.g(c, &["checkout", "-q", "main"]);
        let v = call(&d, c, json!({"ref": "origin/feat", "prefer": "local"}));
        assert_eq!(v["commit"], theirs.as_str(), "{v}");
        assert!(v["warnings"].to_string().contains("prefer_ignored"), "{v}");
    }

    /// rlord's private ref is stale and the user's own `origin/main` is
    /// newer (they fetched since, rlord can't now): the newer one.
    #[test]
    fn the_users_newer_remote_ref_wins_over_a_stale_private_one() {
        let r = Repos::new();
        let c = &r.clone;
        let d = daemon(&r);
        let first = call(&d, c, json!({}))["commit"].clone();
        let newer = r.push_remote("main", "newer");
        r.g(c, &["fetch", "-q", "origin"]);
        std::fs::rename(&r.remote, r.root().join("gone.git")).unwrap();
        let d = again(&r, &d);
        let v = call(&d, c, json!({}));
        assert_ne!(v["commit"], first);
        assert_eq!(v["commit"], newer.as_str(), "{v}");
        assert!(
            v["warnings"].to_string().contains("as you last fetched it"),
            "{v}"
        );
    }

    /// The default branch renamed on the remote (the clone still says
    /// main): a launch asks the remote and starts at the new one, and
    /// records it.
    #[test]
    fn a_launch_follows_a_renamed_default_branch() {
        let r = Repos::new();
        let c = &r.clone;
        r.g(&r.remote, &["branch", "-m", "main", "trunk"]);
        r.g(&r.remote, &["symbolic-ref", "HEAD", "refs/heads/trunk"]);
        let trunk = r.g(&r.remote, &["rev-parse", "trunk"]);
        let d = daemon(&r);
        let b = launch_base(&d, &json!({"repo": c, "name": "x"})).unwrap();
        assert_eq!(b["ref"], "trunk", "{b}");
        assert_eq!(b["commit"], trunk.as_str());
        assert_eq!(record(&d, &key(c)).default_branch.as_deref(), Some("trunk"));
    }

    /// Pruning every repo needs `all`; a repo alone is that repo.
    #[test]
    fn pruning_every_repo_must_be_asked_for() {
        let r = Repos::new();
        let d = daemon(&r);
        call(&d, &r.clone, json!({}));
        let e = prune_rpc(&d, json!({})).unwrap_err().to_string();
        assert!(e.contains("all"), "{e}");
        assert_eq!(records(&d).len(), 1);
        let p = prune_rpc(&d, json!({"all": true})).unwrap();
        assert_eq!(p["removed"].as_array().unwrap().len(), 1, "{p}");
    }

    /// An old private ref goes by remote and branch: the same branch under
    /// another remote stays.
    #[test]
    fn old_private_refs_go_by_remote_and_branch() {
        let r = Repos::new();
        let c = &r.clone;
        let head = r.g(c, &["rev-parse", "HEAD"]);
        for x in ["origin/feat", "upstream/feat"] {
            r.g(
                c,
                &["update-ref", &format!("{}{x}", latest::REF_PREFIX), &head],
            );
        }
        let rec = Rec {
            fetched: [("origin/feat".to_string(), 1)].into_iter().collect(),
            ..Rec::default()
        };
        let dropped = drop_old_refs(&r.env, c, &rec, Some("origin"), "main");
        assert_eq!(dropped, vec!["origin/feat".to_string()]);
        assert!(latest::commit_of(&r.env, c, &latest::private_ref("origin", "feat")).is_none());
        assert!(latest::commit_of(&r.env, c, &latest::private_ref("upstream", "feat")).is_some());
    }

    /// The default branch gone from the remote, the remote not to be asked
    /// again yet, no main/master here: a launch never takes the checkout's
    /// own branch; it starts at HEAD and says why.
    #[test]
    fn a_launch_retry_never_takes_the_checkouts_branch() {
        let r = Repos::new();
        let c = &r.clone;
        r.g(&r.remote, &["branch", "-m", "main", "trunk"]);
        r.g(&r.remote, &["symbolic-ref", "HEAD", "refs/heads/trunk"]);
        r.g(c, &["checkout", "-q", "-b", "wip"]);
        r.g(c, &["branch", "-D", "main"]);
        r.g(c, &["update-ref", "-d", "refs/remotes/origin/main"]);
        let d = daemon(&r);
        // (origin/HEAD still says main; the remote failed a moment ago.)
        put_rec(
            &d,
            &c.join(".git"),
            Rec {
                root: c.to_string_lossy().into_owned(),
                ls_failed_at: now_ms(),
                ..Rec::default()
            },
        );
        let b = launch_base(&d, &json!({"repo": c, "name": "x"})).unwrap();
        assert_eq!(b["head"], true, "{b}");
        assert!(b["why"].as_str().unwrap().contains("no branch main"), "{b}");
    }

    /// `prefer` without a named base is ignored at launch: the default
    /// branch is the remote's even with an unpushed local commit.
    #[test]
    fn a_launch_ignores_prefer_without_a_base() {
        let r = Repos::new();
        let c = &r.clone;
        let theirs = r.g(c, &["rev-parse", "HEAD"]);
        r.commit(c, "unpushed");
        let d = daemon(&r);
        let b = launch_base(&d, &json!({"repo": c, "name": "x", "prefer": "local"})).unwrap();
        assert_eq!(b["commit"], theirs.as_str(), "{b}");
    }

    /// The repo moved with its tree: the tree's `.git` names the old
    /// place. Git repairs it and the tree is moved as usual.
    #[test]
    fn a_moved_repos_tree_is_repaired() {
        let r = Repos::new();
        let d = daemon(&r);
        call(&d, &r.clone, json!({}));
        let moved = r.root().join("moved-clone");
        std::fs::rename(&r.clone, &moved).unwrap();
        let newer = r.push_remote("main", "newer");
        let d = daemon(&r);
        let v = call(&d, &moved, json!({}));
        assert_eq!(
            v["path"],
            json!(moved.join(".worktrees/.rlord-latest")),
            "{v}"
        );
        assert_eq!(v["commit"], newer.as_str());
        assert_eq!(
            r.g(
                &moved.join(".worktrees/.rlord-latest"),
                &["rev-parse", "HEAD"]
            ),
            newer
        );
    }

    /// Prune with a folder at the tree's path that isn't rlord's (on a
    /// branch, unrecorded): left in place, said with its path, not
    /// "removed".
    #[test]
    fn prune_says_what_it_left_in_place() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        let path = c.join(".worktrees/.rlord-latest");
        r.g(
            c,
            &["worktree", "add", "-q", "-b", "s", path.to_str().unwrap()],
        );
        let p = prune_rpc(&d, json!({"repo": c})).unwrap();
        assert_eq!(p["removed"], json!([]), "{p}");
        assert_eq!(p["left"][0]["path"], json!(path), "{p}");
        assert!(path.exists());
    }

    /// `session.new` params that aren't an object: an error, no panic.
    #[test]
    fn launch_base_refuses_params_that_are_not_an_object() {
        let r = Repos::new();
        let d = daemon(&r);
        for mut p in [json!([1, 2]), json!("x"), json!(3)] {
            assert!(set_launch_base(&d, &mut p).is_err());
        }
        let mut p = Value::Null;
        set_launch_base(&d, &mut p).unwrap();
        assert!(p.is_object(), "{p}");
    }

    /// After a force-push back to an older commit, rlord's just-fetched
    /// private ref is the truth, even behind the user's own origin/main.
    #[test]
    fn a_fresh_fetch_wins_after_a_rewind() {
        let r = Repos::new();
        let c = &r.clone;
        let old = r.g(c, &["rev-parse", "HEAD"]);
        r.push_remote("main", "later");
        r.g(c, &["fetch", "-q", "origin"]);
        // The remote rewound to the old commit.
        r.g(&r.other, &["reset", "-q", "--hard", &old]);
        r.g(&r.other, &["push", "-q", "-f", "origin", "main"]);
        let d = daemon(&r);
        let v = call(&d, c, json!({}));
        assert_eq!(v["fetch"], "fetched");
        assert_eq!(v["commit"], old.as_str(), "{v}");
    }

    /// The first tree still being made: the answer says so, with no path.
    #[test]
    fn a_first_tree_still_being_made_is_said() {
        let r = Repos::new();
        let c = &r.clone;
        let d = daemon(&r);
        d.latest
            .set_for_test(r.env.clone(), timing(50), Guard::with(None, false));
        d.latest.hold_add_for_test(&key(c));
        let v = latest_rpc(&d, json!({"repo": c})).unwrap();
        assert_eq!(v["tree"], "preparing", "{v}");
        assert!(v.get("path").is_none());
        assert!(v["warnings"].to_string().contains("call again"), "{v}");
        assert!(!c.join(".worktrees/.rlord-latest").exists());
    }

    /// The sweep drops long-unfetched private refs of every recorded repo,
    /// launch-only ones included.
    #[test]
    fn the_sweep_drops_old_private_refs() {
        let r = Repos::new();
        let c = &r.clone;
        let d = daemon(&r);
        launch_base(&d, &json!({"repo": c, "name": "x"})).unwrap();
        assert!(latest::commit_of(&r.env, c, &latest::private_ref("origin", "main")).is_some());
        update(&d, &key(c), |rec| {
            for at in rec.fetched.values_mut() {
                *at = 1;
            }
            true
        });
        sweep(&d);
        assert!(latest::commit_of(&r.env, c, &latest::private_ref("origin", "main")).is_none());
        assert!(record(&d, &key(c)).fetched.is_empty());
    }

    /// The sweep keeps the tree's own private ref (by remote and branch)
    /// however old, and drops the others.
    #[test]
    fn the_sweep_keeps_the_trees_own_ref() {
        let r = Repos::new();
        let c = &r.clone;
        let d = daemon(&r);
        call(&d, c, json!({}));
        let head = r.g(c, &["rev-parse", "HEAD"]);
        r.g(
            c,
            &["update-ref", &latest::private_ref("origin", "feat"), &head],
        );
        update(&d, &key(c), |rec| {
            rec.fetched.insert("origin/feat".into(), 1);
            for at in rec.fetched.values_mut() {
                *at = 1;
            }
            true
        });
        sweep(&d);
        assert!(latest::commit_of(&r.env, c, &latest::private_ref("origin", "main")).is_some());
        assert!(latest::commit_of(&r.env, c, &latest::private_ref("origin", "feat")).is_none());
    }

    /// A symlinked `.worktrees` (git lists the real path): the tree is
    /// found as git has it, made once and then moved, never looped on.
    #[test]
    fn a_symlinked_worktrees_folder_is_followed() {
        let r = Repos::new();
        let c = &r.clone;
        let real = r.root().join("real-worktrees");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, c.join(".worktrees")).unwrap();
        let d = daemon(&r);
        let v = call(&d, c, json!({}));
        assert_eq!(v["path"], json!(real.join(".rlord-latest")), "{v}");
        assert_eq!(v["moved"], json!({"created": true}));
        let newer = r.push_remote("main", "newer");
        let d = again(&r, &d);
        let v = call(&d, c, json!({}));
        assert_eq!(v["commit"], newer.as_str(), "{v}");
        assert_eq!(v["path"], json!(real.join(".rlord-latest")));
        assert!(
            v.get("moved").is_some_and(|m| m.get("created").is_none()),
            "{v}"
        );
    }

    /// A repo in a guarded folder: the first tree is made with git alone,
    /// the exclude note survives into the answer, and prune removes it
    /// with git alone too.
    #[test]
    fn a_guarded_repo_gets_its_tree_and_its_note() {
        let r = Repos::new();
        std::fs::create_dir(r.root().join("Documents")).unwrap();
        r.g(&r.root(), &["clone", "-q", "remote.git", "Documents/app"]);
        let app = r.root().join("Documents/app");
        let d = daemon(&r);
        d.latest.set_for_test(
            r.env.clone(),
            timing(30_000),
            Guard::with(Some(&r.root()), false),
        );
        let v = call(&d, &app, json!({}));
        assert!(v["exclude_note"].is_string(), "{v}");
        assert!(v["path"].is_string(), "{v}");
        assert!(v["warnings"].to_string().contains("guarded_folder"), "{v}");
        let p = prune_rpc(&d, json!({"repo": app})).unwrap();
        assert_eq!(p["removed"].as_array().unwrap().len(), 1, "{p}");
        assert!(!app.join(".worktrees/.rlord-latest").exists());
    }

    /// A move still running (another call's): the answer says the tree is
    /// being prepared, with no path; a later call moves it.
    #[test]
    fn a_move_still_running_is_said() {
        let r = Repos::new();
        let c = &r.clone;
        let d = daemon(&r);
        call(&d, c, json!({}));
        d.latest
            .set_for_test(r.env.clone(), timing(50), Guard::with(None, false));
        d.latest.hold_add_for_test(&key(c));
        let v = latest_rpc(&d, json!({"repo": c})).unwrap();
        assert_eq!(v["tree"], "preparing", "{v}");
        assert!(v.get("path").is_none());
    }

    /// Two calls waiting on one tree job both get its result (read, not
    /// taken).
    #[test]
    fn every_waiter_of_a_tree_job_gets_its_result() {
        let l = Arc::new(Latest::default());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let l1 = l.clone();
        let a = std::thread::spawn(move || {
            l1.tree_job("k", "main\0c1", Duration::from_secs(5), move || {
                let _ = rx.recv();
                Ok(TreeDone {
                    moved: Some(json!({"created": true})),
                    warnings: vec!["w".into()],
                })
            })
        });
        std::thread::sleep(Duration::from_millis(50));
        let l2 = l.clone();
        let b = std::thread::spawn(move || {
            l2.tree_job("k", "main\0c1", Duration::from_secs(5), || {
                panic!("a second job for the same request")
            })
        });
        std::thread::sleep(Duration::from_millis(50));
        tx.send(()).unwrap();
        for t in [a, b] {
            match t.join().unwrap() {
                Job::Done(Ok(d)) => assert_eq!(d.warnings, ["w"]),
                _ => panic!("a waiter got no result"),
            }
        }
    }

    /// The tool's whole call is capped at its wait, a slow remote or not.
    #[test]
    fn the_tools_call_is_capped_in_all() {
        let r = Repos::new();
        let c = &r.clone;
        r.g(c, &["symbolic-ref", "-d", "refs/remotes/origin/HEAD"]);
        let slow = r.root().join("slow-upload-pack");
        std::fs::write(&slow, "#!/bin/sh\nsleep 2\nexec git-upload-pack \"$@\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        r.g(
            c,
            &["config", "remote.origin.uploadpack", slow.to_str().unwrap()],
        );
        let d = daemon(&r);
        d.latest
            .set_for_test(r.env.clone(), timing(300), Guard::with(None, false));
        let t = Instant::now();
        let _ = latest_rpc(&d, json!({"repo": c}));
        assert!(
            t.elapsed() < Duration::from_millis(1500),
            "{:?}",
            t.elapsed()
        );
    }

    /// A Fetched outcome handed to a later call keeps when it really
    /// finished.
    #[test]
    fn a_handed_over_fetch_keeps_its_finish_time() {
        let r = Repos::new();
        let c = &r.clone;
        let slow = r.root().join("slow-upload-pack");
        std::fs::write(&slow, "#!/bin/sh\nsleep 1\nexec git-upload-pack \"$@\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        r.g(
            c,
            &["config", "remote.origin.uploadpack", slow.to_str().unwrap()],
        );
        let d = daemon(&r);
        d.latest
            .set_for_test(r.env.clone(), timing(100), Guard::with(None, false));
        let first = latest_rpc(&d, json!({"repo": c})).unwrap();
        assert_eq!(first["fetch"], "running", "{first}");
        std::thread::sleep(Duration::from_millis(1800));
        let before = now_ms();
        d.latest
            .set_for_test(r.env.clone(), timing(5_000), Guard::with(None, false));
        let v = latest_rpc(&d, json!({"repo": c})).unwrap();
        assert_eq!(v["fetch"], "fetched", "{v}");
        assert!(v["fetched_at"].as_i64().unwrap() <= before, "{v}");
    }

    /// Prune of a recorded tree it may not touch (a branch checked out in
    /// it, unrecorded name): left, with why; the refs and record go.
    #[test]
    fn a_refused_tree_is_left_and_the_rest_pruned() {
        let r = Repos::new();
        let c = &r.clone;
        let d = daemon(&r);
        call(&d, c, json!({}));
        let other = c.join(".worktrees/not-rlords");
        r.g(
            c,
            &["worktree", "add", "-q", "-b", "x", other.to_str().unwrap()],
        );
        update(&d, &key(c), |rec| {
            rec.tree = Some(other.to_string_lossy().into_owned());
            true
        });
        let p = prune_rpc(&d, json!({"repo": c})).unwrap();
        assert_eq!(p["left"][0]["path"], json!(other), "{p}");
        assert!(p["left"][0]["why"].is_string());
        assert!(other.exists());
        assert!(records(&d).is_empty());
        assert!(r.g(c, &["for-each-ref", latest::REF_PREFIX]).is_empty());
    }

    /// Two refs at one commit: each call's own ref is recorded (tree jobs
    /// go by ref and commit).
    #[test]
    fn the_recorded_ref_is_the_callers() {
        let r = Repos::new();
        let c = &r.clone;
        let head = r.g(c, &["rev-parse", "HEAD"]);
        r.g(
            &r.other,
            &["push", "-q", "origin", &format!("{head}:refs/heads/feat")],
        );
        let d = daemon(&r);
        call(&d, c, json!({}));
        assert_eq!(record(&d, &key(c)).branch.as_deref(), Some("main"));
        let v = call(&d, c, json!({"ref": "feat"}));
        assert_eq!(v["ref"], "feat", "{v}");
        assert_eq!(record(&d, &key(c)).branch.as_deref(), Some("feat"));
    }

    /// A slow fetch spends the call's budget: an existing tree still
    /// answers with its path (the tree job has a wait of its own), never
    /// "preparing".
    #[test]
    fn a_slow_fetch_never_makes_an_existing_tree_preparing() {
        let r = Repos::new();
        let c = &r.clone;
        let d = daemon(&r);
        let first = call(&d, c, json!({}));
        let slow = r.root().join("slow-upload-pack");
        std::fs::write(&slow, "#!/bin/sh\nsleep 1\nexec git-upload-pack \"$@\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        r.g(
            c,
            &["config", "remote.origin.uploadpack", slow.to_str().unwrap()],
        );
        let d = again(&r, &d);
        d.latest.set_for_test(
            r.env.clone(),
            Timing {
                tree_min_wait: Duration::from_secs(5),
                ..timing(200)
            },
            Guard::with(None, false),
        );
        let v = latest_rpc(&d, json!({"repo": c})).unwrap();
        assert_eq!(v["fetch"], "running", "{v}");
        assert!(v.get("tree").is_none(), "{v}");
        assert_eq!(v["path"], first["path"], "{v}");
        assert_eq!(v["commit"], first["commit"]);
    }

    /// A failed `ls-remote` is remembered: not asked again for a while.
    #[test]
    fn a_failed_ls_remote_is_not_retried_at_once() {
        let r = Repos::new();
        let c = &r.clone;
        r.g(c, &["symbolic-ref", "-d", "refs/remotes/origin/HEAD"]);
        std::fs::rename(&r.remote, r.root().join("gone.git")).unwrap();
        let d = daemon(&r);
        let v = call(&d, c, json!({}));
        assert_eq!(v["ref"], "main");
        let rec = record(&d, &key(c));
        assert!(rec.ls_failed_at > 0);
        let want = Want {
            path: "",
            branch: None,
            prefer: None,
            wait: Duration::ZERO,
            deadline: None,
            launch: false,
        };
        assert_eq!(ask(&want, &rec, now_ms()), latest::AskRemote::Never);
        assert_eq!(
            ask(&want, &rec, rec.ls_failed_at + LS_RETRY_MS + 1),
            latest::AskRemote::First(latest::LS_REMOTE)
        );
    }

    /// A launch resolves without the move lock: a tree being moved never
    /// holds it up. It records that the repo was used.
    #[test]
    fn a_launch_never_waits_on_the_tree() {
        let r = Repos::new();
        let d = daemon(&r);
        let c = &r.clone;
        let k = key(c);
        let _moving = d.latest.moves.lock(&k);
        let t = Instant::now();
        let b = launch_base(&d, &json!({"repo": c, "name": "x"})).unwrap();
        assert!(b["commit"].is_string(), "{b}");
        assert!(t.elapsed() < Duration::from_secs(5));
        let rec = record(&d, &k);
        assert!(rec.used_at > 0 && rec.tree.is_none());
        assert_eq!(rec.root, c.to_string_lossy());
    }

    /// Records are keyed by the common git dir: the repo is the same
    /// from a worktree of a bare repo.
    #[test]
    fn a_bare_repo_and_its_worktree_are_one_repo() {
        let r = Repos::new();
        let d = daemon(&r);
        let wt = r.root().join("bare-wt");
        r.g(
            &r.remote,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                wt.to_str().unwrap(),
                "main",
            ],
        );
        let v = call(&d, &wt, json!({"ref": "main"}));
        assert_eq!(v["repo"], json!(r.remote));
        let l = list_rpc(&d).unwrap();
        assert_eq!(l["trees"][0]["git_dir"], json!(r.remote));
    }
}
