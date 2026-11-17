//! The latest committed code of a repo (`repo_latest`), and where a new
//! session's branch starts. Git plumbing and the pure decisions; the
//! daemon (`repo_latest.rs`) keeps the state, the locks and the
//! background fetch.
//!
//! What this writes in the user's repo, and nothing else:
//!
//! - private refs, `refs/rlord/latest/<remote>/<branch>`, and the objects
//!   a fetch brings into the shared store;
//! - one detached worktree, always `<repo>/.worktrees/.rlord-latest`, which
//!   only rlord moves (`checkout --detach --force` and `clean -fdx`, run
//!   inside that tree after checking it is that tree).
//!
//! Never: the user's checkouts, local branches, remote-tracking refs
//! (`refs/remotes/*`: a fetch here passes `--refmap=`), index, stash,
//! uncommitted work, hooks (`core.hooksPath=/dev/null`) or config.
//!
//! Every git call goes through `run`: a time limit, no prompt (no
//! terminal, no askpass, ssh in batch mode), no hooks, no auto gc.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// Local plumbing (rev-parse, status, update-ref).
pub const PLUMBING: Duration = Duration::from_secs(10);
/// A checkout or `worktree add` (a partial clone may fetch blobs).
pub const CHECKOUT: Duration = Duration::from_secs(600);
/// `ls-remote` for the default branch.
pub const LS_REMOTE: Duration = Duration::from_secs(8);
/// A fetch is killed only past this: a slow first fetch killed early would
/// lose its progress and time out the same way every time.
pub const HARD_LIMIT: Duration = Duration::from_secs(300);
/// What `run` keeps of each stream.
const CLIP: usize = 64 * 1024;
/// What a listing that must be whole keeps of stdout (`worktree list`):
/// more is an error, not a partial list.
const LIST_CLIP: usize = 4 * 1024 * 1024;
/// The tree's folder name, under the repo's workspace dir. It starts with
/// a dot: no session's folder can have it (`git::sanitize_branch` trims
/// leading dots, and git refuses a branch component that starts with
/// one), so the tree is never a session's worktree.
pub const TREE_NAME: &str = ".rlord-latest";
/// Where the private refs live.
pub const REF_PREFIX: &str = "refs/rlord/latest/";

/// Config every call gets: no prompts, no hooks, no background upkeep, no
/// submodules.
const CONFIG: &[&str] = &[
    "core.askPass=",
    "credential.interactive=false",
    "core.hooksPath=/dev/null",
    "gc.auto=0",
    "maintenance.auto=false",
    "core.fsmonitor=false",
    "submodule.recurse=false",
    "fetch.recurseSubmodules=false",
];

/// Variables a caller's environment must not hand to git: they would
/// point it at another repo than the one named.
const UNSET: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "SSH_ASKPASS",
    "GIT_ASKPASS",
];

/// Extra environment for every git call (tests point git at their own
/// config and home).
#[derive(Debug, Clone, Default)]
pub struct GitEnv {
    pub extra: Vec<(String, String)>,
}

impl GitEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.extra
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var(key).ok())
            .filter(|v| !v.is_empty())
    }

    pub fn with(&self, key: &str, value: String) -> GitEnv {
        let mut e = self.clone();
        e.extra.push((key.to_string(), value));
        e
    }
}

/// What a git call printed.
#[derive(Debug, Clone)]
pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
    /// stdout was cut at its cap, or not read to its end.
    pub clipped: bool,
}

impl Output {
    pub fn text(&self) -> &str {
        self.stdout.trim()
    }
}

/// `git -C <dir> <args>`, at most `limit`. An error only when git can't
/// start or runs past `limit` (it and what it started are killed); a
/// failed command is `ok: false`.
pub fn run(env: &GitEnv, dir: &Path, args: &[&str], limit: Duration) -> Result<Output> {
    run_capped(env, dir, args, limit, CLIP)
}

/// `run`, keeping up to `cap` bytes of stdout.
fn run_capped(
    env: &GitEnv,
    dir: &Path,
    args: &[&str],
    limit: Duration,
    cap: usize,
) -> Result<Output> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    for c in CONFIG {
        cmd.args(["-c", c]);
    }
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("LC_ALL", "C");
    for v in UNSET {
        cmd.env_remove(v);
    }
    for (k, v) in &env.extra {
        cmd.env(k, v);
    }
    // Its own process group, so ssh goes too when it is killed.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = cmd.spawn().context("running git (is git installed?)")?;
    let streams = Streams::start(child.stdout.take(), child.stderr.take(), cap);
    // Waited on in its own thread; this one blocks on its answer, at most
    // `limit`.
    // It waits for the exit without reaping (`WNOWAIT`), then reaps under
    // `reaped`: while unreaped, `pid` (and its group) can't be reused, so
    // the kill below never reaches another process.
    let pid = child.id() as i32;
    let reaped = Arc::new(Mutex::new(false));
    let (tx, exited) = mpsc::channel();
    let r = reaped.clone();
    std::thread::spawn(move || {
        loop {
            // SAFETY: waitid fills `info`; WNOWAIT leaves the child to reap.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if rc == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
        }
        let mut done = r.lock().unwrap_or_else(|e| e.into_inner());
        let st = child.wait();
        *done = true;
        drop(done);
        let _ = tx.send(st);
    });
    let status = match exited.recv_timeout(limit) {
        Ok(st) => st?,
        Err(_) => {
            let done = reaped.lock().unwrap_or_else(|e| e.into_inner());
            if !*done {
                // SAFETY: plain syscall on the group git leads
                // (`process_group`), its leader not reaped yet.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            drop(done);
            // (Reaped by the waiting thread.)
            let _ = exited.recv_timeout(Duration::from_secs(5));
            bail!(
                "git {} took longer than {}s",
                args.first().unwrap_or(&"?"),
                limit.as_secs()
            );
        }
    };
    let (stdout, stderr, clipped) = streams.finish();
    Ok(Output {
        ok: status.success(),
        stdout,
        stderr,
        clipped,
    })
}

/// stdout and stderr, each read on its own thread and clipped (stdout at
/// the call's cap, stderr at `CLIP`; the rest is read and dropped, so git
/// never blocks on a full pipe).
struct Streams {
    out: Arc<Mutex<Vec<u8>>>,
    err: Arc<Mutex<Vec<u8>>>,
    /// Bytes of stdout dropped.
    out_dropped: Arc<std::sync::atomic::AtomicBool>,
    done: mpsc::Receiver<()>,
}

impl Streams {
    fn start<A, B>(out: Option<A>, err: Option<B>, cap: usize) -> Streams
    where
        A: Read + Send + 'static,
        B: Read + Send + 'static,
    {
        let (tx, done) = mpsc::channel();
        fn read_into<R: Read>(
            r: Option<R>,
            buf: &Mutex<Vec<u8>>,
            cap: usize,
            dropped: &std::sync::atomic::AtomicBool,
        ) {
            let Some(mut r) = r else {
                return;
            };
            let mut chunk = [0u8; 8192];
            while let Ok(n) = r.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
                let room = cap.saturating_sub(b.len()).min(n);
                if room < n {
                    dropped.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                b.extend_from_slice(&chunk[..room]);
            }
        }
        let s = Streams {
            out: Arc::default(),
            err: Arc::default(),
            out_dropped: Arc::default(),
            done,
        };
        let (b, t, f) = (s.out.clone(), tx.clone(), s.out_dropped.clone());
        std::thread::spawn(move || {
            read_into(out, &b, cap, &f);
            let _ = t.send(());
        });
        let b = s.err.clone();
        std::thread::spawn(move || {
            read_into(err, &b, CLIP, &std::sync::atomic::AtomicBool::default());
            let _ = tx.send(());
        });
        s
    }

    /// What was read. Waits at most a second, for both streams together,
    /// for their end: a process git left behind (an ssh master) may hold
    /// one open.
    fn finish(self) -> (String, String, bool) {
        let deadline = Instant::now() + Duration::from_secs(1);
        // (A stream not read to its end is as good as clipped.)
        let mut whole = true;
        for _ in 0..2 {
            let left = deadline.saturating_duration_since(Instant::now());
            if self.done.recv_timeout(left).is_err() {
                whole = false;
                break;
            }
        }
        let text = |b: &Mutex<Vec<u8>>| {
            String::from_utf8_lossy(&b.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
        };
        let dropped = self.out_dropped.load(std::sync::atomic::Ordering::Relaxed);
        (text(&self.out), text(&self.err), dropped || !whole)
    }
}

/// The installed git's (major, minor), asked once. None: unknown.
pub fn git_version() -> Option<(u32, u32)> {
    static V: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        run(&GitEnv::default(), Path::new("/"), &["--version"], PLUMBING)
            .ok()
            .and_then(|o| parse_version(&o.stdout))
    })
}

/// `git version 2.39.5 (Apple Git-154)` → (2, 39).
pub fn parse_version(s: &str) -> Option<(u32, u32)> {
    let v = s.trim().strip_prefix("git version ")?;
    let mut n = v.split(|c: char| !c.is_ascii_digit());
    Some((n.next()?.parse().ok()?, n.next()?.parse().ok()?))
}

/// Whether the installed git is at least `(major, minor)` (unknown: yes).
fn git_at_least(v: (u32, u32)) -> bool {
    git_version().is_none_or(|have| have >= v)
}

/// `--end-of-options` where git has it (2.24); names passed after it are
/// checked anyway (`check_branch`, rlord's own paths).
fn end_of_options() -> Option<&'static str> {
    git_at_least((2, 24)).then_some("--end-of-options")
}

/// `run`, its output on success, else an error with git's first line.
fn git(env: &GitEnv, dir: &Path, args: &[&str], limit: Duration) -> Result<String> {
    let o = run(env, dir, args, limit)?;
    if !o.ok {
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&"?"),
            first_line(&o.stderr)
        );
    }
    Ok(o.text().to_string())
}

/// The first non-empty line, clipped to 200 characters.
pub fn first_line(s: &str) -> String {
    let l = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    crate::agents::transcript::clip(l, 199)
}

/// A commit id as shown: its first 7 characters.
pub fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

// ---------------------------------------------------------------------------
// Pure decisions.

/// How a fetch went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    /// Fetched now.
    Fetched,
    /// Fetched under a minute ago: not repeated.
    Cached,
    /// Still running; the last known commit is used.
    Running,
    /// The remote can't be reached.
    Offline,
    /// Anything else (git's first line).
    Failed(String),
    /// The branch is not on the remote.
    Missing,
    /// The repo has no remote to fetch from.
    NoRemote,
}

impl Fetch {
    pub fn id(&self) -> &'static str {
        match self {
            Fetch::Fetched => "fetched",
            Fetch::Cached => "cached",
            Fetch::Running => "running",
            Fetch::Offline => "offline",
            Fetch::Failed(_) => "failed",
            Fetch::Missing => "missing",
            Fetch::NoRemote => "no_remote",
        }
    }

    /// The remote's branch is known as of now (or under a minute ago).
    pub fn fresh(&self) -> bool {
        matches!(self, Fetch::Fetched | Fetch::Cached)
    }
}

/// A failed fetch, from what git printed.
pub fn classify(stderr: &str) -> Fetch {
    const OFFLINE: &[&str] = &[
        "Could not resolve host",
        "Could not resolve hostname",
        "Temporary failure in name resolution",
        "Failed to connect",
        "Connection refused",
        "Connection timed out",
        "Connection reset",
        "Operation timed out",
        "timed out",
        "Network is unreachable",
        "No route to host",
    ];
    if stderr.contains("couldn't find remote ref") {
        Fetch::Missing
    } else if OFFLINE.iter().any(|p| stderr.contains(p)) {
        Fetch::Offline
    } else {
        Fetch::Failed(first_line(stderr))
    }
}

/// Where a commit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Remote,
    Local,
    Same,
}

impl Source {
    pub fn id(self) -> &'static str {
        match self {
            Source::Remote => "remote",
            Source::Local => "local",
            Source::Same => "same",
        }
    }
}

/// The local branch against the remote's: counts from `rev-list
/// --left-right --count L...R`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relation {
    Same,
    LocalBehind(u32),
    LocalAhead(u32),
    /// (local only, remote only)
    Diverged(u32, u32),
    /// Couldn't be worked out (a shallow clone, a missing object).
    Unknown,
}

/// The side the user chose after a `diverged` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefer {
    Local,
    Remote,
}

impl Prefer {
    pub fn parse(s: &str) -> Option<Prefer> {
        match s {
            "local" => Some(Prefer::Local),
            "remote" => Some(Prefer::Remote),
            _ => None,
        }
    }
}

/// Which commit to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    Use {
        commit: String,
        source: Source,
        /// Local commits not on the remote (None: not known).
        ahead: Option<u32>,
        /// Remote commits not in the local branch.
        behind: Option<u32>,
        /// The two had diverged; `prefer` or the default branch's rule
        /// chose.
        diverged: bool,
    },
    /// Diverged, and nothing says which: the caller asks the user.
    Diverged {
        local_ahead: Option<u32>,
        remote_ahead: Option<u32>,
    },
}

/// The rule. The default branch (`default_branch`): the remote's commit
/// whenever there is one, even when the local branch is ahead (the
/// caller warns `local_ahead`). A named branch: the newer of the local and
/// remote one; when they diverged, `prefer`, else ask.
pub fn pick(
    local: Option<&str>,
    remote: Option<&str>,
    rel: &Relation,
    prefer: Option<Prefer>,
    default_branch: bool,
) -> Option<Pick> {
    let chosen = |commit: &str, source, ahead, behind, diverged| Pick::Use {
        commit: commit.to_string(),
        source,
        ahead,
        behind,
        diverged,
    };
    let (l, r) = match (local, remote) {
        (None, None) => return None,
        (Some(l), None) => return Some(chosen(l, Source::Local, None, None, false)),
        (None, Some(r)) => return Some(chosen(r, Source::Remote, None, None, false)),
        (Some(l), Some(r)) => (l, r),
    };
    if l == r {
        return Some(chosen(l, Source::Same, Some(0), Some(0), false));
    }
    // The default branch, or the remote's asked for (`<remote>/<b>`, or
    // `prefer: remote`): the remote's commit, however the local one stands.
    if (default_branch && prefer != Some(Prefer::Local)) || prefer == Some(Prefer::Remote) {
        let (ahead, behind, diverged) = match rel {
            Relation::Same => (Some(0), Some(0), false),
            Relation::LocalBehind(n) => (Some(0), Some(*n), false),
            Relation::LocalAhead(n) => (Some(*n), Some(0), false),
            Relation::Diverged(a, b) => (Some(*a), Some(*b), true),
            Relation::Unknown => (None, None, false),
        };
        return Some(chosen(r, Source::Remote, ahead, behind, diverged));
    }
    let (a, b) = match rel {
        Relation::Same => return Some(chosen(l, Source::Same, Some(0), Some(0), false)),
        Relation::LocalBehind(n) => {
            return Some(chosen(r, Source::Remote, Some(0), Some(*n), false))
        }
        Relation::LocalAhead(n) => return Some(chosen(l, Source::Local, Some(*n), Some(0), false)),
        Relation::Diverged(a, b) => (Some(*a), Some(*b)),
        // Couldn't compare (a shallow clone, a missing object): not a
        // divergence to ask about. The remote's, unless the user chose
        // the local one; the caller warns.
        Relation::Unknown => {
            return Some(match prefer {
                Some(Prefer::Local) => chosen(l, Source::Local, None, None, false),
                _ => chosen(r, Source::Remote, None, None, false),
            })
        }
    };
    // (The remote's side and the default branch were answered above.)
    match prefer {
        Some(Prefer::Local) => Some(chosen(l, Source::Local, a, b, true)),
        _ => Some(Pick::Diverged {
            local_ahead: a,
            remote_ahead: b,
        }),
    }
}

/// The branch `ls-remote --symref <remote> HEAD` names.
pub fn parse_symref(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let (target, name) = l.strip_prefix("ref: ")?.split_once('\t')?;
        (name.trim() == "HEAD")
            .then(|| target.strip_prefix("refs/heads/"))
            .flatten()
            .filter(|b| !b.is_empty())
            .map(str::to_string)
    })
}

/// The ssh command for a fetch: the user's (`GIT_SSH_COMMAND`, else
/// `core.sshCommand`, else `ssh`) in batch mode with a connect timeout.
/// None (leave it as it is): `ssh.variant` names another program, or the
/// command isn't ssh.
pub fn batch_ssh(base: &str, variant: Option<&str>) -> Option<String> {
    if variant.is_some_and(|v| !matches!(v, "ssh" | "auto" | "")) {
        return None;
    }
    let program = base.split_whitespace().next()?;
    let name = Path::new(program).file_name()?.to_str()?;
    (name == "ssh" || variant == Some("ssh"))
        .then(|| format!("{base} -oBatchMode=yes -oConnectTimeout=10"))
}

/// A branch name rlord passes to git: never an option, a valid ref name.
pub fn check_branch(name: &str) -> Result<()> {
    if name.is_empty() || name.starts_with('-') || name.starts_with("refs/") {
        bail!("not a branch name: {name:?}");
    }
    let o = run(
        &GitEnv::default(),
        Path::new("/"),
        &["check-ref-format", &format!("refs/heads/{name}")],
        PLUMBING,
    )?;
    if !o.ok {
        bail!("not a branch name: {name:?}");
    }
    Ok(())
}

/// The private ref a fetch of `remote`'s `branch` goes to.
pub fn private_ref(remote: &str, branch: &str) -> String {
    format!("{REF_PREFIX}{remote}/{branch}")
}

/// The folder the tree is in, under the repo: always the repo's own
/// `.worktrees` (not `[session] workspace_dir`, which may point outside
/// the repo, where sibling repos' trees would collide).
pub const TREE_DIR: &str = ".worktrees";

/// The tree's path in `repo`.
pub fn tree_path(repo: &Path) -> PathBuf {
    repo.join(TREE_DIR).join(TREE_NAME)
}

/// The tree's path as git reports worktree paths (resolved: a symlinked
/// `.worktrees` is followed), so it compares equal to git's own. With
/// `fs_ok` the folder above it is resolved; without (a guarded folder,
/// where rlord stats nothing), git is asked from inside the tree, and
/// the path is taken as it is when that fails (no tree yet).
pub fn tree_path_seen(env: &GitEnv, repo: &Path, fs_ok: bool) -> PathBuf {
    let dir = tree_path(repo);
    if fs_ok {
        if let Some(Ok(p)) = dir.parent().map(std::fs::canonicalize) {
            return p.join(TREE_NAME);
        }
        return dir;
    }
    run(env, &dir, &["rev-parse", "--show-toplevel"], PLUMBING)
        .ok()
        .filter(|o| o.ok)
        .map(|o| PathBuf::from(o.text()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()) == Some(TREE_NAME))
        .unwrap_or(dir)
}

/// The path of the checkout that has `branch`.
pub fn parse_checked_out(list: &[Entry], branch: &str) -> Option<String> {
    list.iter()
        .find(|e| e.branch.as_deref() == Some(branch))
        .map(|e| e.path.clone())
}

/// One worktree in `git worktree list --porcelain`, as git's records
/// have it (its admin dir: right even when the folder is broken).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub bare: bool,
    /// Detached HEAD (no branch checked out).
    pub detached: bool,
    pub branch: Option<String>,
    /// Git calls it prunable: its folder is gone or broken.
    pub prunable: bool,
}

/// Every worktree in `git worktree list --porcelain`, the main one first.
pub fn parse_worktrees(list: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for l in list.lines() {
        if let Some(p) = l.strip_prefix("worktree ") {
            out.push(Entry {
                path: p.to_string(),
                ..Entry::default()
            });
            continue;
        }
        let Some(e) = out.last_mut() else {
            continue;
        };
        if l == "bare" {
            e.bare = true;
        } else if l == "detached" {
            e.detached = true;
        } else if let Some(b) = l.strip_prefix("branch refs/heads/") {
            e.branch = Some(b.to_string());
        } else if l.starts_with("prunable") {
            e.prunable = true;
        }
    }
    out
}

/// The entry at `dir`, if git has one.
pub fn entry_at<'a>(list: &'a [Entry], dir: &Path) -> Option<&'a Entry> {
    list.iter().find(|e| Path::new(&e.path) == dir)
}

// ---------------------------------------------------------------------------
// Git operations.

/// What a path is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located {
    NotGit,
    Repo(Repo),
}

/// A repo, found from any folder in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// Its identity: the common git dir (shared by all its worktrees).
    pub common: PathBuf,
    /// Where rlord works from: the main worktree (git's first entry), or
    /// for a bare repo its own folder. Never assumed to be the common
    /// dir's parent (`--separate-git-dir`, a bare repo's worktrees).
    pub root: PathBuf,
    pub bare: bool,
    /// `git worktree list`, as `locate` read it (the main one first).
    pub worktrees: Vec<Entry>,
}

/// The repo `path` is in. Git only: nothing reads the folder itself.
/// Works on git before 2.31 (no `--path-format`): a relative common dir
/// is made absolute by git itself, asked from that dir.
pub fn locate(env: &GitEnv, path: &Path) -> Result<Located> {
    let o = run(
        env,
        path,
        &[
            "rev-parse",
            "--git-dir",
            "--git-common-dir",
            "--absolute-git-dir",
        ],
        PLUMBING,
    )?;
    if !o.ok {
        if o.stderr.contains("not a git repository") {
            return Ok(Located::NotGit);
        }
        bail!(
            "git can't read {}: {}",
            path.display(),
            first_line(&o.stderr)
        );
    }
    let mut l = o.stdout.lines();
    let (dir_rel, common_rel) = (l.next().unwrap_or(""), l.next().unwrap_or(""));
    let gitdir = PathBuf::from(l.next().unwrap_or(""));
    let common = if dir_rel == common_rel {
        gitdir.clone()
    } else {
        let c = Path::new(common_rel);
        let c = if c.is_absolute() {
            c.to_path_buf()
        } else {
            path.join(c)
        };
        PathBuf::from(git(
            env,
            &c,
            &["rev-parse", "--absolute-git-dir"],
            PLUMBING,
        )?)
    };
    let worktrees = worktree_list(env, path)?;
    let main = worktrees.first().context("git listed no worktree")?;
    let root = if main.bare || Path::new(&main.path) != common {
        PathBuf::from(&main.path)
    } else if gitdir == common {
        // `--separate-git-dir`: git names the main worktree by its git
        // dir. Asked from the main checkout, that checkout is the root.
        PathBuf::from(git(env, path, &["rev-parse", "--show-toplevel"], PLUMBING)?)
    } else {
        bail!(
            "can't tell where the main checkout of {} is (its git dir is kept apart): \
             pass the main checkout's path",
            common.display()
        );
    };
    let bare = main.bare;
    Ok(Located::Repo(Repo {
        common,
        root,
        bare,
        worktrees,
    }))
}

/// `git worktree list --porcelain`, parsed. A list too long to read
/// whole is an error, never a partial list.
pub fn worktree_list(env: &GitEnv, repo: &Path) -> Result<Vec<Entry>> {
    worktree_list_capped(env, repo, LIST_CLIP)
}

fn worktree_list_capped(env: &GitEnv, repo: &Path, cap: usize) -> Result<Vec<Entry>> {
    let o = run_capped(
        env,
        repo,
        &["worktree", "list", "--porcelain"],
        PLUMBING,
        cap,
    )?;
    if !o.ok {
        bail!("git worktree failed: {}", first_line(&o.stderr));
    }
    if o.clipped {
        bail!("git worktree list printed more than {cap} bytes: not read");
    }
    Ok(parse_worktrees(&o.stdout))
}

/// The remote to fetch from: `origin`, else the only one. None with why.
pub fn remote_of(env: &GitEnv, repo: &Path) -> Result<std::result::Result<String, String>> {
    let out = git(env, repo, &["remote"], PLUMBING)?;
    let remotes: Vec<&str> = out
        .lines()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .collect();
    Ok(if remotes.contains(&"origin") {
        Ok("origin".into())
    } else {
        match remotes.as_slice() {
            [one] => Ok(one.to_string()),
            [] => Err("no remote: the local branch, not fetched".into()),
            _ => Err("several remotes and none is origin: the local branch, not fetched".into()),
        }
    })
}

/// The commit `rev` names, if any.
pub fn commit_of(env: &GitEnv, repo: &Path, rev: &str) -> Option<String> {
    let o = run(
        env,
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ],
        PLUMBING,
    )
    .ok()?;
    (o.ok && !o.text().is_empty()).then(|| o.text().to_string())
}

/// How the default branch was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultFrom {
    /// `refs/remotes/<remote>/HEAD`.
    RemoteHead,
    /// rlord's record (under a day old).
    Cached,
    /// Asked the remote (`ls-remote --symref`): worth recording.
    LsRemote,
    /// `main` or `master`, whichever exists.
    Guess,
    /// The main checkout's branch.
    Current,
}

/// When `default_branch` may ask the remote (`ls-remote --symref`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskRemote {
    /// Before guessing `main`/`master`, at most this long (the tool).
    First(Duration),
    /// A launch: before guessing `main`/`master`, at most this long
    /// (zero: not at all), and never the checkout's current branch
    /// (whatever the user last checked out is no default branch: the
    /// caller falls back to HEAD and says so).
    Launch(Duration),
    /// Never (it failed a moment ago).
    Never,
}

/// The default branch; `failed` says whether asking the remote failed,
/// whatever the result (the caller remembers that a while). First that answers: the remote's HEAD as the
/// repo knows it, `cached`, then by `ask`: the remote, `main`/`master`,
/// the main checkout's branch. `skip_head`: not the first two (they named
/// a branch the remote no longer has).
#[allow(clippy::too_many_arguments)]
pub fn default_branch(
    env: &GitEnv,
    renv: &GitEnv,
    repo: &Path,
    remote: Option<&str>,
    cached: Option<&str>,
    skip_head: bool,
    ask: AskRemote,
    failed: &mut bool,
) -> Result<(String, DefaultFrom)> {
    if let (Some(r), false) = (remote, skip_head) {
        let head = format!("refs/remotes/{r}/HEAD");
        if let Ok(o) = run(env, repo, &["symbolic-ref", "--quiet", &head], PLUMBING) {
            let prefix = format!("refs/remotes/{r}/");
            if let Some(b) = o.text().strip_prefix(&prefix).filter(|b| !b.is_empty()) {
                if o.ok {
                    return Ok((b.to_string(), DefaultFrom::RemoteHead));
                }
            }
        }
    }
    if let Some(c) = cached.filter(|c| !c.is_empty() && !skip_head) {
        return Ok((c.to_string(), DefaultFrom::Cached));
    }
    *failed = false;
    let mut ask_remote = |limit: Duration| -> Option<String> {
        let r = remote?;
        if limit.is_zero() {
            return None;
        }
        let o = run(renv, repo, &["ls-remote", "--symref", r, "HEAD"], limit);
        // (Killed only because the caller's deadline was near: no
        // failure of the remote's, not remembered as one.)
        let cut = o.is_err() && limit < LS_REMOTE;
        let b = o
            .ok()
            .filter(|o| o.ok)
            .and_then(|o| parse_symref(&o.stdout));
        *failed = b.is_none() && !cut;
        b
    };
    if let AskRemote::First(limit) | AskRemote::Launch(limit) = ask {
        if let Some(b) = ask_remote(limit) {
            return Ok((b, DefaultFrom::LsRemote));
        }
    }
    for b in ["main", "master"] {
        let mut refs = vec![format!("refs/heads/{b}")];
        if let Some(r) = remote {
            refs.insert(0, format!("refs/remotes/{r}/{b}"));
        }
        if refs.iter().any(|x| commit_of(env, repo, x).is_some()) {
            return Ok((b.to_string(), DefaultFrom::Guess));
        }
    }
    if let AskRemote::Launch(_) = ask {
        bail!("can't tell the default branch: the remote didn't say, and no main or master");
    }
    let current = run(
        env,
        repo,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        PLUMBING,
    )
    .ok()
    .filter(|o| o.ok && !o.text().is_empty())
    .map(|o| o.text().to_string());
    if let Some(b) = current {
        return Ok((b, DefaultFrom::Current));
    }
    bail!("can't tell the default branch: pass `ref`")
}

/// `env` with ssh in batch mode for talking to a remote (`batch_ssh`).
/// Worked out once per call and passed to what talks to the remote.
pub fn remote_env(env: &GitEnv, repo: &Path) -> GitEnv {
    let cfg = |k: &str| {
        run(env, repo, &["config", "--get", k], PLUMBING)
            .ok()
            .filter(|o| o.ok)
            .map(|o| o.text().to_string())
            .filter(|v| !v.is_empty())
    };
    let base = match env
        .get("GIT_SSH_COMMAND")
        .or_else(|| cfg("core.sshCommand"))
    {
        Some(b) => b,
        // `GIT_SSH` is the user's program: left as it is.
        None if env.get("GIT_SSH").is_some() => return env.clone(),
        None => "ssh".to_string(),
    };
    match batch_ssh(&base, cfg("ssh.variant").as_deref()) {
        Some(cmd) => env.with("GIT_SSH_COMMAND", cmd),
        None => env.clone(),
    }
}

/// Fetch `remote`'s `branch` into its private ref (never the user's
/// `refs/remotes/*`, no tags, no `FETCH_HEAD`). Missing on the remote:
/// the private ref is deleted.
pub fn fetch_branch(env: &GitEnv, renv: &GitEnv, repo: &Path, remote: &str, branch: &str) -> Fetch {
    let private = private_ref(remote, branch);
    // An old private ref for `a` blocks one for `a/b` (and the other way
    // round), as for any refs: those go first.
    let mine = format!("{REF_PREFIX}{remote}/");
    if let Ok(out) = git(
        env,
        repo,
        &["for-each-ref", "--format=%(refname)", &mine],
        PLUMBING,
    ) {
        for r in out.lines().filter(|r| nested(r, &private)) {
            let _ = run(env, repo, &["update-ref", "-d", r], PLUMBING);
        }
    }
    let spec = format!("+refs/heads/{branch}:{private}");
    let mut args = vec![
        "fetch",
        "--quiet",
        "--no-tags",
        "--no-recurse-submodules",
        "--no-prune",
        "--refmap=",
    ];
    // (Git before 2.29 writes FETCH_HEAD whatever: accepted.)
    if git_at_least((2, 29)) {
        args.push("--no-write-fetch-head");
    }
    args.extend(end_of_options());
    args.extend([remote, spec.as_str()]);
    let mut tries = 0;
    loop {
        tries += 1;
        let o = match run(renv, repo, &args, HARD_LIMIT) {
            Ok(o) => o,
            Err(e) => return Fetch::Failed(first_line(&e.to_string())),
        };
        if o.ok {
            return Fetch::Fetched;
        }
        // Another daemon fetching the same ref: once more.
        if o.stderr.contains("cannot lock ref") && tries < 2 {
            continue;
        }
        let f = classify(&o.stderr);
        if f == Fetch::Missing {
            let _ = run(env, repo, &["update-ref", "-d", &private], PLUMBING);
        }
        return f;
    }
}

/// Whether two refs can't both exist: one is a directory of the other.
pub fn nested(a: &str, b: &str) -> bool {
    let under = |x: &str, y: &str| x.strip_prefix(y).is_some_and(|rest| rest.starts_with('/'));
    under(a, b) || under(b, a)
}

/// How `local` stands against `remote`.
pub fn relation(env: &GitEnv, repo: &Path, local: &str, remote: &str) -> Relation {
    if local == remote {
        return Relation::Same;
    }
    let range = format!("{local}...{remote}");
    let o = match run(
        env,
        repo,
        &["rev-list", "--left-right", "--count", &range, "--"],
        PLUMBING,
    ) {
        Ok(o) if o.ok => o,
        _ => return Relation::Unknown,
    };
    let mut n = o.text().split_whitespace().map(str::parse::<u32>);
    match (n.next(), n.next()) {
        (Some(Ok(0)), Some(Ok(0))) => Relation::Same,
        (Some(Ok(0)), Some(Ok(b))) => Relation::LocalBehind(b),
        (Some(Ok(a)), Some(Ok(0))) => Relation::LocalAhead(a),
        (Some(Ok(a)), Some(Ok(b))) => Relation::Diverged(a, b),
        _ => Relation::Unknown,
    }
}

/// Commits in `to` not in `from` (None: not known).
pub fn count_between(env: &GitEnv, repo: &Path, from: &str, to: &str) -> Option<u32> {
    let o = run(
        env,
        repo,
        &["rev-list", "--count", &format!("{from}..{to}"), "--"],
        PLUMBING,
    )
    .ok()?;
    o.ok.then(|| o.text().parse().ok()).flatten()
}

/// Whether `a` is an ancestor of `b`.
pub fn is_ancestor(env: &GitEnv, repo: &Path, a: &str, b: &str) -> bool {
    run(env, repo, &["merge-base", "--is-ancestor", a, b], PLUMBING).is_ok_and(|o| o.ok)
}

/// A commit's subject (clipped at 100) and commit time (ms).
pub fn commit_info(env: &GitEnv, repo: &Path, commit: &str) -> Option<(String, i64)> {
    let o = run(
        env,
        repo,
        &["show", "-s", "--format=%ct %s", commit, "--"],
        PLUMBING,
    )
    .ok()?;
    let t = o.ok.then(|| o.text().to_string())?;
    let (ct, subject) = t.split_once(' ').unwrap_or((&t, ""));
    Some((
        crate::agents::transcript::clip(subject, 99),
        ct.parse::<i64>().ok()? * 1000,
    ))
}

/// When `rev` last moved, from its reflog (ms), if it keeps one.
pub fn reflog_time(env: &GitEnv, repo: &Path, rev: &str) -> Option<i64> {
    let o = run(
        env,
        repo,
        &["log", "-g", "-1", "--date=unix", "--format=%gd", rev, "--"],
        PLUMBING,
    )
    .ok()?;
    let t = o.ok.then(|| o.text().to_string())?;
    let secs = t
        .rsplit_once('{')?
        .1
        .strip_suffix('}')?
        .parse::<i64>()
        .ok()?;
    Some(secs * 1000)
}

/// Whether the repo is a shallow clone.
pub fn is_shallow(env: &GitEnv, repo: &Path) -> bool {
    run(
        env,
        repo,
        &["rev-parse", "--is-shallow-repository"],
        PLUMBING,
    )
    .is_ok_and(|o| o.ok && o.text() == "true")
}

/// The user's checkout that has `branch`, and with `count` how many files
/// there are not committed (capped at 999; None: not asked, or git
/// couldn't say). Read-only (`GIT_OPTIONAL_LOCKS=0`: their index isn't
/// rewritten).
pub fn checked_out(
    env: &GitEnv,
    list: &[Entry],
    branch: &str,
    count: bool,
) -> Option<(String, Option<u32>)> {
    let at = parse_checked_out(list, branch)?;
    if !count {
        return Some((at, None));
    }
    let n = run(
        env,
        Path::new(&at),
        &["status", "--porcelain", "--untracked-files=normal"],
        PLUMBING,
    )
    .ok()
    .filter(|o| o.ok)
    .map(|o| o.stdout.lines().count().min(999) as u32);
    Some((at, n))
}

/// Whether git's `entry` is rlord's tree by construction, record or not:
/// registered at `expected` (the tree's path for this repo), named
/// `TREE_NAME` (no session folder can be), detached. A tree whose record
/// was lost (a kv reset, the other daemon's) is adopted this way;
/// anything on a branch, elsewhere, or not registered is not.
pub fn adoptable(entry: &Entry, expected: &Path) -> bool {
    Path::new(&entry.path) == expected
        && expected.file_name().and_then(|n| n.to_str()) == Some(TREE_NAME)
        && entry.detached
        && entry.branch.is_none()
        && !entry.bare
}

/// Why rlord may act on the worktree at a path: it recorded the tree
/// there, and git's own record of it is a detached worktree at that path
/// under the name rlord gives its tree. A session's worktree (always on a
/// branch, never named `TREE_NAME`) never passes.
pub fn may_touch(recorded: Option<&Path>, dir: &Path, entry: &Entry) -> Result<()> {
    if recorded != Some(dir) {
        bail!(
            "{} is a worktree rlord didn't record as its tree: left as it is \
             (remove it with `git worktree remove` if it is a leftover)",
            dir.display()
        );
    }
    if !entry.detached || entry.branch.is_some() || entry.bare {
        bail!(
            "{} has a branch checked out: not rlord's tree, left as it is",
            dir.display()
        );
    }
    if dir.file_name().and_then(|n| n.to_str()) != Some(TREE_NAME) {
        bail!("{} is not named as rlord's tree", dir.display());
    }
    Ok(())
}

/// What `dir` is, from inside: `Some(detached)` when it is that worktree
/// itself (its own top level, a git dir under the repo's `worktrees/`),
/// `None` when it verifiably isn't (a `.git` file missing or broken would
/// let git find another checkout, the user's). An error (a time limit,
/// git not running) says nothing about the tree: the caller leaves it.
fn inspect(env: &GitEnv, repo: &Repo, dir: &Path) -> Result<Option<bool>> {
    let o = tree_run(
        env,
        dir,
        &[
            "rev-parse",
            "--show-toplevel",
            "--absolute-git-dir",
            "--symbolic-full-name",
            "HEAD",
        ],
        PLUMBING,
    )?;
    if !o.ok {
        return Ok(None);
    }
    let mut l = o.stdout.lines();
    let (top, gitdir, head) = (
        l.next().unwrap_or(""),
        l.next().unwrap_or(""),
        l.next().unwrap_or(""),
    );
    let itself =
        Path::new(top) == dir && Path::new(gitdir).starts_with(repo.common.join("worktrees"));
    Ok(itself.then_some(head == "HEAD"))
}

/// `env` for git run inside rlord's tree `dir`: git looks for a repo in
/// `dir` itself and never above it (`GIT_CEILING_DIRECTORIES`), so with
/// the tree's `.git` broken or gone it fails rather than reaching the
/// user's checkout that holds `.worktrees`.
fn pinned(env: &GitEnv, dir: &Path) -> GitEnv {
    match dir.parent() {
        Some(p) => env.with("GIT_CEILING_DIRECTORIES", p.to_string_lossy().into()),
        None => env.clone(),
    }
}

/// Git run inside rlord's tree `dir`, always `pinned` to it. The only way
/// rlord runs git there: `tree_git` and `tree_head` go through it too.
fn tree_run(env: &GitEnv, dir: &Path, args: &[&str], limit: Duration) -> Result<Output> {
    run(&pinned(env, dir), dir, args, limit)
}

/// `tree_run`, its output when it succeeds, else an error.
fn tree_git(env: &GitEnv, dir: &Path, args: &[&str], limit: Duration) -> Result<String> {
    git(&pinned(env, dir), dir, args, limit)
}

/// The tree's `HEAD` commit, by `tree_run`.
fn tree_head(env: &GitEnv, dir: &Path) -> Option<String> {
    let o = tree_run(
        env,
        dir,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        PLUMBING,
    )
    .ok()?;
    (o.ok && !o.text().is_empty()).then(|| o.text().to_string())
}

/// `inspect`, as a check: it is that worktree, detached.
#[cfg(test)]
fn verify_tree(env: &GitEnv, repo: &Repo, dir: &Path) -> Result<()> {
    match inspect(env, repo, dir)? {
        Some(true) => Ok(()),
        _ => bail!("{} is not rlord's worktree as git sees it", dir.display()),
    }
}

/// Add rlord's tree at `dir`, detached at `commit`. `force`: over a
/// registration whose folder is gone.
pub fn tree_add(env: &GitEnv, repo: &Repo, dir: &Path, commit: &str, force: bool) -> Result<()> {
    let d = dir.to_string_lossy();
    let mut args = vec!["worktree", "add", "--detach"];
    if force {
        args.push("--force");
    }
    args.extend(end_of_options());
    args.extend([d.as_ref(), commit]);
    git(env, &repo.root, &args, CHECKOUT).map(drop)
}

/// Add rlord's tree where git has no worktree. A folder already there is
/// replaced only when it is recognizably rlord's (`leftover`) and rlord
/// may touch files in the repo (`fs_ok`); otherwise an error names it and
/// it is left. Returns whether a leftover was replaced.
pub fn tree_add_fresh(
    env: &GitEnv,
    repo: &Repo,
    dir: &Path,
    commit: &str,
    fs_ok: bool,
) -> Result<bool> {
    let e = match tree_add(env, repo, dir, commit, false) {
        Ok(()) => return Ok(false),
        Err(e) => e,
    };
    // (In a guarded folder nothing is looked at, not even a stat.)
    if !fs_ok {
        bail!(
            "{e:#}. If a folder is at {}, remove it, then ask again: rlord touches no \
             files in a folder macOS guards",
            dir.display()
        );
    }
    if !dir.exists() {
        return Err(e);
    }
    // The repo was moved with its tree: the tree's `.git` file names the
    // old admin dir. Git puts that right (`worktree repair`); then it is
    // moved to the commit like any tree.
    if moved_tree(dir) {
        let d = dir.to_string_lossy();
        let repaired = run(
            env,
            &repo.root,
            &["worktree", "repair", d.as_ref()],
            CHECKOUT,
        )
        .is_ok_and(|o| o.ok);
        if repaired && inspect(env, repo, dir)? == Some(true) {
            tree_git(
                env,
                dir,
                &["checkout", "--quiet", "--detach", "--force", commit],
                CHECKOUT,
            )?;
            tree_git(env, dir, &["clean", "-fdxq"], CHECKOUT)?;
            return Ok(true);
        }
    }
    if !leftover(repo, dir) {
        bail!(
            "{} is there, git has no worktree at it, and it isn't a leftover rlord tree: \
             left as it is (move it away)",
            dir.display()
        );
    }
    std::fs::remove_dir_all(dir)
        .with_context(|| format!("removing the leftover tree {}", dir.display()))?;
    tree_add(env, repo, dir, commit, false)?;
    Ok(true)
}

/// Whether `dir`'s `.git` file names an admin dir `worktrees/<name>` for
/// rlord's tree (`<name>` is `TREE_NAME`, maybe with git's number), in
/// whatever repo: the tree of a repo that was moved.
fn moved_tree(dir: &Path) -> bool {
    if dir.file_name().and_then(|n| n.to_str()) != Some(TREE_NAME) {
        return false;
    }
    let git = dir.join(".git");
    if !std::fs::symlink_metadata(&git).is_ok_and(|m| m.is_file()) {
        return false;
    }
    std::fs::read_to_string(&git).is_ok_and(|t| {
        let Some(p) = t.trim().strip_prefix("gitdir:").map(|p| gitdir_at(dir, p)) else {
            return false;
        };
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // (Git names the admin dir after the folder, its leading dot made
        // a dash: `-rlord-latest`, maybe numbered.)
        p.parent()
            .and_then(|x| x.file_name())
            .and_then(|n| n.to_str())
            == Some("worktrees")
            && name
                .trim_start_matches(['.', '-'])
                .strip_prefix(TREE_NAME.trim_start_matches('.'))
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
    })
}

/// Whether the folder at `dir` is recognizably a leftover of rlord's
/// tree: named `TREE_NAME`, and empty or holding a `.git` file that points
/// into this repo's `worktrees/`.
pub fn leftover(repo: &Repo, dir: &Path) -> bool {
    if dir.file_name().and_then(|n| n.to_str()) != Some(TREE_NAME) {
        return false;
    }
    let Ok(mut entries) = std::fs::read_dir(dir) else {
        return false;
    };
    if entries.next().is_none() {
        return true;
    }
    let git = dir.join(".git");
    if !std::fs::symlink_metadata(&git).is_ok_and(|m| m.is_file()) {
        return false;
    }
    std::fs::read_to_string(&git).is_ok_and(|t| {
        t.trim()
            .strip_prefix("gitdir:")
            .is_some_and(|p| gitdir_at(dir, p).starts_with(repo.common.join("worktrees")))
    })
}

/// A `.git` file's `gitdir:` path, resolved against the tree folder `dir`
/// when it is relative (`worktree.useRelativePaths`), `..` taken
/// lexically.
fn gitdir_at(dir: &Path, p: &str) -> PathBuf {
    use std::path::Component;
    let p = Path::new(p.trim());
    let full = if p.is_absolute() {
        p.to_path_buf()
    } else {
        dir.join(p)
    };
    let mut out = PathBuf::new();
    for c in full.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// What moving the tree did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Moved {
    /// Files changed in the tree that were thrown away.
    pub discarded: u32,
    /// The tree was broken and made again.
    pub recreated: bool,
    /// HEAD was already at the commit: nothing ran in the tree (no
    /// status either; edits there wait for the next move).
    pub unchanged: bool,
}

/// Point the tree at `commit`: `checkout --detach --force` and `clean
/// -fdx`, inside the tree, after `may_touch` (the caller's record and
/// git's) and `verify_tree` (from inside). Anything edited in it is
/// discarded. A tree that is verifiably broken (its folder gone, so git
/// calls it prunable, or not a valid worktree from inside) is removed
/// and added again. Anything else that fails (a lock, a time limit) is an
/// error, and the tree is left as it is. One that fails `may_touch` is
/// never touched.
#[allow(clippy::too_many_arguments)]
pub fn tree_move(
    env: &GitEnv,
    repo: &Repo,
    dir: &Path,
    commit: &str,
    recorded: Option<&Path>,
    complete: Option<&str>,
    entry: &Entry,
    fs_ok: bool,
) -> Result<Moved> {
    // rlord's by name and record, but a branch got checked out in it:
    // detached again (only once it is verifiably that worktree), not
    // refused forever. Unrecorded, or not itself: `may_touch` refuses.
    let detached;
    // Edits to tracked files `checkout --force` throws away, counted first.
    let mut forced = 0;
    let entry = if recorded == Some(dir)
        && dir.file_name().and_then(|n| n.to_str()) == Some(TREE_NAME)
        && entry.branch.is_some()
        && !entry.bare
        && !entry.prunable
        && inspect(env, repo, dir)?.is_some()
    {
        forced = tree_git(
            env,
            dir,
            &["status", "--porcelain", "--untracked-files=no"],
            CHECKOUT,
        )?
        .lines()
        .count() as u32;
        tree_git(
            env,
            dir,
            &["checkout", "--quiet", "--detach", "--force"],
            CHECKOUT,
        )?;
        detached = Entry {
            detached: true,
            branch: None,
            ..entry.clone()
        };
        &detached
    } else {
        entry
    };
    may_touch(recorded, dir, entry)?;
    let recreated = Moved {
        recreated: true,
        ..Moved::default()
    };
    // Its folder gone: added again over its registration (git only, so
    // this works in a guarded folder too).
    // ("prunable" may also mean the folder is there with its `.git` gone:
    // that goes the way of a broken tree, below. In a guarded folder
    // nothing is statted: git's add is tried.)
    if entry.prunable && (!fs_ok || matches!(dir.try_exists(), Ok(false))) {
        tracing::warn!(tree = %dir.display(), "rlord's tree folder is gone: adding it again");
        tree_add(env, repo, dir, commit, true)?;
        return Ok(recreated);
    }
    // Verifiably broken (not itself from inside): made again. A git error
    // (a time limit) leaves it as it is.
    let itself = inspect(env, repo, dir)
        .map_err(|e| anyhow::anyhow!("rlord's tree {} was left as it is: {e:#}", dir.display()))?;
    match itself {
        None => {
            tracing::warn!(tree = %dir.display(), "rlord's tree is broken: making it again");
            tree_remove(env, repo, dir, recorded, entry, fs_ok)?;
            tree_add(env, repo, dir, commit, true)?;
            return Ok(recreated);
        }
        // (Detached, as seen from inside, before anything runs there.)
        Some(false) => bail!(
            "{} has a branch checked out (as seen from inside): not touched",
            dir.display()
        ),
        Some(true) => {}
    }
    let in_tree = |args: &[&str]| {
        tree_git(env, dir, args, CHECKOUT).map_err(|e| {
            let e = e.to_string();
            let hint = if e.contains("index.lock") {
                " (a git command may be running there; if none is, remove its index.lock)"
            } else {
                ""
            };
            anyhow::anyhow!(
                "rlord's tree {} was left as it is: {e}{hint}",
                dir.display()
            )
        })
    };
    // Nothing to do only when rlord recorded this commit as completed: a
    // checkout killed partway can leave HEAD at the commit with files
    // missing.
    // (A big tree's status can take a while: the checkout's time limit.)
    let head = tree_head(env, dir);
    let dirty = if complete == Some(commit) && head.as_deref() == Some(commit) {
        // At the commit: one cheap status; clean, nothing to do.
        let dirty = in_tree(&["status", "--porcelain", "--untracked-files=normal"])?;
        if dirty.is_empty() {
            return Ok(Moved {
                unchanged: true,
                discarded: forced,
                ..Moved::default()
            });
        }
        dirty
    } else {
        // (Ignored files too: `clean -fdx` takes them.)
        in_tree(&[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored",
        ])?
    };
    in_tree(&["checkout", "--quiet", "--detach", "--force", commit])?;
    in_tree(&["clean", "-fdxq"])?;
    Ok(Moved {
        discarded: forced + dirty.lines().count() as u32,
        ..Moved::default()
    })
}

/// Remove rlord's tree (`worktree remove --force --force`: even with
/// edits, even locked), only after `may_touch`. A tree git won't remove
/// (its `.git` file broken) is deleted as a folder when `fs_ok` (rlord may
/// touch files in the repo: not a folder macOS guards); else it is left,
/// and an error says so. Its registration then stays until git prunes it
/// (rlord never runs `worktree prune`: it would touch the user's entries
/// too).
pub fn tree_remove(
    env: &GitEnv,
    repo: &Repo,
    dir: &Path,
    recorded: Option<&Path>,
    entry: &Entry,
    fs_ok: bool,
) -> Result<()> {
    may_touch(recorded, dir, entry)?;
    let d = dir.to_string_lossy();
    let o = run(
        env,
        &repo.root,
        &["worktree", "remove", "--force", "--force"]
            .into_iter()
            .chain(end_of_options())
            .chain([d.as_ref()])
            .collect::<Vec<_>>(),
        CHECKOUT,
    )?;
    if o.ok || entry_at(&worktree_list(env, &repo.root)?, dir).is_none() {
        return Ok(());
    }
    // (A folder is deleted only when, seen from inside, it is that
    // worktree, detached; or, not a worktree at all any more, a leftover
    // of rlord's: `leftover`.)
    // (Or registered as rlord's tree, `may_touch` above, and its `.git`
    // gone: git calls it prunable.)
    let ours = match inspect(env, repo, dir) {
        Ok(Some(true)) => true,
        Ok(None) => {
            leftover(repo, dir)
                || (entry.prunable && fs_ok && matches!(dir.join(".git").try_exists(), Ok(false)))
        }
        _ => false,
    };
    if fs_ok && ours {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => {}
        }
    }
    bail!(
        "git couldn't remove rlord's tree {d}: {}",
        first_line(&o.stderr)
    )
}

/// Delete all of the repo's private refs (`refs/rlord/latest/*`).
/// Returns how many went.
pub fn drop_refs(env: &GitEnv, repo: &Path) -> Result<usize> {
    let out = git(
        env,
        repo,
        &["for-each-ref", "--format=%(refname)", REF_PREFIX],
        PLUMBING,
    )?;
    let mut n = 0;
    for r in out.lines().filter(|r| r.starts_with(REF_PREFIX)) {
        if run(env, repo, &["update-ref", "-d", r], PLUMBING).is_ok_and(|o| o.ok) {
            n += 1;
        }
    }
    Ok(n)
}

/// What the tree's checkout leaves out, said as warnings (git only).
pub fn tree_warnings(env: &GitEnv, repo: &Path, dir: &Path) -> Vec<String> {
    let mut w = Vec::new();
    let has = |path: &str| {
        tree_run(
            env,
            dir,
            &["cat-file", "-e", &format!("HEAD:{path}")],
            PLUMBING,
        )
        .is_ok_and(|o| o.ok)
    };
    if has(".gitmodules") {
        w.push("submodules_not_checked_out: submodule folders are empty".to_string());
    }
    let lfs = tree_run(env, dir, &["show", "HEAD:.gitattributes"], PLUMBING)
        .is_ok_and(|o| o.ok && o.stdout.contains("filter=lfs"));
    if lfs {
        w.push("lfs_pointers: Git LFS files are pointers, not their content".to_string());
    }
    let sparse = run(
        env,
        repo,
        &["config", "--get", "core.sparseCheckout"],
        PLUMBING,
    )
    .is_ok_and(|o| o.ok && o.text() == "true");
    if sparse {
        w.push("sparse: the repo uses a sparse checkout; files may be missing".to_string());
    }
    w
}

/// Whether git ignores `path` (relative to the repo root).
pub fn ignored(env: &GitEnv, repo: &Path, path: &str) -> bool {
    run(env, repo, &["check-ignore", "-q", "--", path], PLUMBING).is_ok_and(|o| o.ok)
}

#[cfg(any(test, feature = "test-util"))]
pub mod testing {
    //! Temp repos for tests: a bare "remote" and a clone, with git kept
    //! off the user's config and home.
    use super::*;

    pub struct Repos {
        pub dir: tempfile::TempDir,
        pub env: GitEnv,
        pub remote: PathBuf,
        pub clone: PathBuf,
        /// Another clone that pushes to the remote (someone else).
        pub other: PathBuf,
    }

    pub fn env_in(home: &Path) -> GitEnv {
        GitEnv {
            extra: vec![
                ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                ("HOME".into(), home.to_string_lossy().into_owned()),
                ("GIT_AUTHOR_NAME".into(), "t".into()),
                ("GIT_AUTHOR_EMAIL".into(), "t@t".into()),
                ("GIT_COMMITTER_NAME".into(), "t".into()),
                ("GIT_COMMITTER_EMAIL".into(), "t@t".into()),
                ("GIT_SSH_COMMAND".into(), "false".into()),
            ],
        }
    }

    #[allow(clippy::new_without_default)]
    impl Repos {
        pub fn new() -> Repos {
            let dir = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let env = env_in(&root);
            let remote = root.join("remote.git");
            let clone = root.join("clone");
            let other = root.join("other");
            let r = Repos {
                dir,
                env,
                remote,
                clone,
                other,
            };
            r.g(&root, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
            r.g(&root, &["clone", "-q", "remote.git", "other"]);
            r.g(&r.other, &["checkout", "-q", "-b", "main"]);
            r.commit(&r.other, "one");
            r.g(&r.other, &["push", "-q", "origin", "main"]);
            r.g(&root, &["clone", "-q", "remote.git", "clone"]);
            r
        }

        pub fn root(&self) -> PathBuf {
            std::fs::canonicalize(self.dir.path()).unwrap()
        }

        /// A git command that must work; its output.
        pub fn g(&self, dir: &Path, args: &[&str]) -> String {
            let o = run(&self.env, dir, args, CHECKOUT).unwrap();
            assert!(o.ok, "git {args:?} in {}: {}", dir.display(), o.stderr);
            o.text().to_string()
        }

        pub fn commit(&self, dir: &Path, msg: &str) -> String {
            self.g(dir, &["commit", "-q", "--allow-empty", "-m", msg]);
            self.g(dir, &["rev-parse", "HEAD"])
        }

        /// Someone else pushes a commit to the remote's `branch`.
        pub fn push_remote(&self, branch: &str, msg: &str) -> String {
            self.g(&self.other, &["fetch", "-q", "--prune", "origin"]);
            let has = run(
                &self.env,
                &self.other,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("origin/{branch}"),
                ],
                PLUMBING,
            )
            .unwrap()
            .ok;
            if has {
                self.g(
                    &self.other,
                    &["checkout", "-q", "-B", branch, &format!("origin/{branch}")],
                );
            } else {
                self.g(&self.other, &["checkout", "-q", "-B", branch]);
            }
            let c = self.commit(&self.other, msg);
            self.g(&self.other, &["push", "-q", "origin", branch]);
            c
        }

        /// Everything about the user's clone a call must leave alone.
        pub fn snapshot(&self) -> String {
            let c = &self.clone;
            let refs = self.g(c, &["for-each-ref", "--format=%(refname) %(objectname)"]);
            let refs: Vec<&str> = refs
                .lines()
                .filter(|l| !l.starts_with(REF_PREFIX))
                .collect();
            format!(
                "{}\n{}\n{}\n{}\n{}",
                self.g(c, &["rev-parse", "HEAD"]),
                self.g(c, &["symbolic-ref", "-q", "HEAD"]),
                self.g(c, &["status", "--porcelain", "--untracked-files=all"]),
                refs.join("\n"),
                self.g(c, &["stash", "list"]),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Repos;
    use super::*;

    #[test]
    fn pick_follows_the_rule() {
        let p = |l, r, rel, prefer, def| pick(l, r, &rel, prefer, def);
        assert_eq!(p(None, None, Relation::Unknown, None, false), None);
        let only_l = p(Some("l"), None, Relation::Unknown, None, false).unwrap();
        assert!(
            matches!(only_l, Pick::Use { ref commit, source: Source::Local, .. } if commit == "l")
        );
        let only_r = p(None, Some("r"), Relation::Unknown, None, false).unwrap();
        assert!(matches!(
            only_r,
            Pick::Use {
                source: Source::Remote,
                ..
            }
        ));
        let same = p(Some("x"), Some("x"), Relation::Unknown, None, false).unwrap();
        assert!(matches!(
            same,
            Pick::Use {
                source: Source::Same,
                ..
            }
        ));
        assert_eq!(
            p(Some("l"), Some("r"), Relation::LocalBehind(3), None, false),
            Some(Pick::Use {
                commit: "r".into(),
                source: Source::Remote,
                ahead: Some(0),
                behind: Some(3),
                diverged: false
            })
        );
        assert_eq!(
            p(Some("l"), Some("r"), Relation::LocalAhead(2), None, false),
            Some(Pick::Use {
                commit: "l".into(),
                source: Source::Local,
                ahead: Some(2),
                behind: Some(0),
                diverged: false
            })
        );
        // Diverged, a named branch: ask.
        assert_eq!(
            p(Some("l"), Some("r"), Relation::Diverged(2, 5), None, false),
            Some(Pick::Diverged {
                local_ahead: Some(2),
                remote_ahead: Some(5)
            })
        );
        // ...unless the user chose.
        let chose = |prefer| {
            p(
                Some("l"),
                Some("r"),
                Relation::Diverged(2, 5),
                Some(prefer),
                false,
            )
        };
        assert!(
            matches!(chose(Prefer::Local), Some(Pick::Use { ref commit, diverged: true, .. }) if commit == "l")
        );
        assert!(
            matches!(chose(Prefer::Remote), Some(Pick::Use { ref commit, diverged: true, .. }) if commit == "r")
        );
        // The remote's asked for (`origin/x`, `prefer: remote`): the
        // remote's even when the local one is ahead.
        assert_eq!(
            p(
                Some("l"),
                Some("r"),
                Relation::LocalAhead(2),
                Some(Prefer::Remote),
                false
            ),
            Some(Pick::Use {
                commit: "r".into(),
                source: Source::Remote,
                ahead: Some(2),
                behind: Some(0),
                diverged: false
            })
        );
        // The default branch: the remote's, even when the local one is
        // ahead.
        assert_eq!(
            p(Some("l"), Some("r"), Relation::LocalAhead(2), None, true),
            Some(Pick::Use {
                commit: "r".into(),
                source: Source::Remote,
                ahead: Some(2),
                behind: Some(0),
                diverged: false
            })
        );
        // The default branch: the remote wins.
        assert!(matches!(
            p(Some("l"), Some("r"), Relation::Diverged(2, 5), None, true),
            Some(Pick::Use { ref commit, source: Source::Remote, diverged: true, .. }) if commit == "r"
        ));
        // Couldn't compare: not a divergence; the remote's unless the
        // user chose the local one.
        for def in [true, false] {
            assert_eq!(
                p(Some("l"), Some("r"), Relation::Unknown, None, def),
                Some(Pick::Use {
                    commit: "r".into(),
                    source: Source::Remote,
                    ahead: None,
                    behind: None,
                    diverged: false
                })
            );
        }
        assert!(matches!(
            p(Some("l"), Some("r"), Relation::Unknown, Some(Prefer::Local), false),
            Some(Pick::Use { ref commit, .. }) if commit == "l"
        ));
    }

    #[test]
    fn fetch_errors_are_classified() {
        assert_eq!(
            classify("fatal: couldn't find remote ref refs/heads/x\n"),
            Fetch::Missing
        );
        assert_eq!(
            classify("ssh: Could not resolve hostname github.com: nodename nor servname provided"),
            Fetch::Offline
        );
        assert_eq!(
            classify("fatal: unable to access 'https://x/': Failed to connect to x port 443"),
            Fetch::Offline
        );
        let f = classify("\nfatal: Authentication failed for 'https://x/'\nmore");
        assert_eq!(
            f,
            Fetch::Failed("fatal: Authentication failed for 'https://x/'".into())
        );
        assert_eq!(f.id(), "failed");
        let long = "e".repeat(500);
        let Fetch::Failed(m) = classify(&long) else {
            panic!()
        };
        assert_eq!(m.chars().count(), 200);
    }

    #[test]
    fn parsers() {
        assert_eq!(
            parse_symref("ref: refs/heads/trunk\tHEAD\nabc123\tHEAD\n"),
            Some("trunk".into())
        );
        assert_eq!(parse_symref("abc123\tHEAD\n"), None);
        let list = parse_worktrees(
            "worktree /r\nHEAD abc\nbranch refs/heads/main\n\n\
             worktree /r/.worktrees/x\nHEAD def\nbranch refs/heads/feature/x\n\n\
             worktree /r/.worktrees/.rlord-latest\nHEAD 123\ndetached\n\
             prunable gitdir file points to non-existent location\n",
        );
        assert_eq!(list.len(), 3);
        assert_eq!(
            parse_checked_out(&list, "feature/x"),
            Some("/r/.worktrees/x".into())
        );
        assert_eq!(parse_checked_out(&list, "main"), Some("/r".into()));
        assert_eq!(parse_checked_out(&list, "nope"), None);
        let tree = entry_at(&list, Path::new("/r/.worktrees/.rlord-latest")).unwrap();
        assert!(tree.detached && tree.prunable && tree.branch.is_none());
        assert!(entry_at(&list, Path::new("/r/other")).is_none());
        let bare = parse_worktrees("worktree /r.git\nbare\n\nworktree /w\nHEAD 1\ndetached\n");
        assert!(bare[0].bare);
        assert_eq!(
            tree_path(Path::new("/r")),
            PathBuf::from("/r/.worktrees/.rlord-latest")
        );
        assert_eq!(private_ref("origin", "a/b"), "refs/rlord/latest/origin/a/b");
        assert_eq!(short("abcdef123456"), "abcdef1");
        assert_eq!(short("abc"), "abc");
    }

    /// No session's folder can be the tree's: a name from
    /// `sanitize_branch` never starts with a dot, git refuses a branch
    /// that does, and `workspace_dirname` keeps the first character.
    #[test]
    fn no_session_folder_is_named_like_the_tree() {
        use crate::git::{sanitize_branch, workspace_dirname};
        for name in [
            ".rlord-latest",
            "..rlord-latest",
            " .rlord-latest",
            "-.rlord-latest",
            "/.rlord-latest",
            ".rlord/latest",
            "rlord-latest",
        ] {
            let b = sanitize_branch(name);
            assert_ne!(workspace_dirname(&b), TREE_NAME, "{name}");
            assert!(!b.starts_with('.'), "{name}");
        }
        assert!(check_branch(TREE_NAME).is_err());
        assert_ne!(workspace_dirname("x/.rlord-latest"), TREE_NAME);
    }

    #[test]
    fn a_relative_gitdir_is_resolved_against_the_tree() {
        assert_eq!(
            gitdir_at(
                Path::new("/r/.worktrees/.rlord-latest"),
                " ../../.git/worktrees/-rlord-latest\n"
            ),
            PathBuf::from("/r/.git/worktrees/-rlord-latest")
        );
        assert_eq!(
            gitdir_at(Path::new("/x"), "/r/.git/worktrees/a"),
            PathBuf::from("/r/.git/worktrees/a")
        );
    }

    #[test]
    fn git_versions() {
        assert_eq!(
            parse_version("git version 2.39.5 (Apple Git-154)\n"),
            Some((2, 39))
        );
        assert_eq!(parse_version("git version 2.28.0"), Some((2, 28)));
        assert_eq!(parse_version("git version 2.45.1.windows.1"), Some((2, 45)));
        assert_eq!(parse_version("nonsense"), None);
        assert!(git_version().is_some());
    }

    #[test]
    fn ssh_is_put_in_batch_mode_only_when_it_is_ssh() {
        assert_eq!(
            batch_ssh("ssh", None).unwrap(),
            "ssh -oBatchMode=yes -oConnectTimeout=10"
        );
        assert_eq!(
            batch_ssh("/usr/bin/ssh -i ~/.ssh/work", Some("auto")).unwrap(),
            "/usr/bin/ssh -i ~/.ssh/work -oBatchMode=yes -oConnectTimeout=10"
        );
        assert_eq!(batch_ssh("plink", None), None);
        assert_eq!(batch_ssh("ssh", Some("putty")), None);
        assert!(batch_ssh("my-wrapper", Some("ssh")).is_some());
    }

    #[test]
    fn branch_names_never_reach_git_as_options() {
        assert!(check_branch("feature/x").is_ok());
        for bad in [
            "-x",
            "--upload-pack=evil",
            "a..b",
            "",
            "refs/heads/x",
            "a b",
        ] {
            assert!(check_branch(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn runner_cuts_off_a_slow_command() {
        let dir = tempfile::tempdir().unwrap();
        let env = GitEnv {
            extra: vec![("GIT_CONFIG_NOSYSTEM".into(), "1".into())],
        };
        // `git -c alias...` can't sleep; a hook-free stand-in: a fetch
        // from a "remote" that is a slow command.
        let slow = dir.path().join("slow");
        std::fs::write(&slow, "#!/bin/sh\nsleep 5\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t = Instant::now();
        let e = run(
            &env,
            dir.path(),
            &[
                "ls-remote",
                &format!("--upload-pack={}", slow.display()),
                "/nowhere",
            ],
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(e.to_string().contains("took longer"), "{e}");
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    fn repo_of(r: &Repos, p: &Path) -> Repo {
        match locate(&r.env, p).unwrap() {
            Located::Repo(x) => x,
            Located::NotGit => panic!("{} is no repo", p.display()),
        }
    }

    fn entry(r: &Repos, repo: &Repo, dir: &Path) -> Option<Entry> {
        entry_at(&worktree_list(&r.env, &repo.root).unwrap(), dir).cloned()
    }

    fn fetch(r: &Repos, repo: &Path, branch: &str) -> Fetch {
        fetch_branch(&r.env, &r.env, repo, "origin", branch)
    }

    #[test]
    fn locate_finds_the_repo_from_anywhere() {
        let r = Repos::new();
        let sub = r.clone.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let clone = repo_of(&r, &sub);
        assert_eq!(clone.root, r.clone);
        assert_eq!(clone.common, r.clone.join(".git"));
        assert!(!clone.bare);
        let bare = repo_of(&r, &r.remote);
        assert_eq!((bare.root.clone(), bare.bare), (r.remote.clone(), true));
        assert_eq!(bare.common, r.remote);
        // A worktree of a bare repo: the bare repo.
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
        let w = repo_of(&r, &wt);
        assert_eq!(
            (w.common, w.root, w.bare),
            (bare.common.clone(), bare.root.clone(), true)
        );
        assert_eq!(w.worktrees.len(), 2);
        // `--separate-git-dir`: the checkout is the root, not the git
        // dir's parent.
        let sep_git = r.root().join("store/sep.git");
        std::fs::create_dir(r.root().join("store")).unwrap();
        let sep = r.root().join("sep");
        r.g(
            &r.root(),
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--separate-git-dir={}", sep_git.display()),
                "sep",
            ],
        );
        r.commit(&sep, "x");
        let s = repo_of(&r, &sep);
        assert_eq!((s.root.clone(), s.common.clone()), (sep.clone(), sep_git));
        // From a session worktree of it, git can't say where the main
        // checkout is: an error, never that worktree taken for it.
        let swt = sep.join(".worktrees/a");
        r.g(
            &sep,
            &["worktree", "add", "-q", "-b", "a", swt.to_str().unwrap()],
        );
        let e = locate(&r.env, &swt).unwrap_err().to_string();
        assert!(e.contains("main checkout"), "{e}");
        let plain = r.root().join("plain");
        std::fs::create_dir(&plain).unwrap();
        // (`GIT_CEILING_DIRECTORIES` keeps git from looking above it.)
        let env = r
            .env
            .with("GIT_CEILING_DIRECTORIES", r.root().to_string_lossy().into());
        assert_eq!(locate(&env, &plain).unwrap(), Located::NotGit);
    }

    /// origin/HEAD unset, nothing recorded, the remote's default `develop`
    /// and an old `master` here: a launch asks the remote and gets
    /// `develop`; it guesses `master` only with no budget or no answer.
    #[test]
    fn a_launch_asks_the_remote_before_guessing() {
        let r = Repos::new();
        let c = &r.clone;
        let e = &r.env;
        r.g(&r.remote, &["branch", "develop", "main"]);
        r.g(&r.remote, &["symbolic-ref", "HEAD", "refs/heads/develop"]);
        r.g(&r.remote, &["branch", "-D", "main"]);
        r.g(c, &["branch", "-m", "main", "master"]);
        r.g(c, &["symbolic-ref", "-d", "refs/remotes/origin/HEAD"]);
        r.g(c, &["update-ref", "-d", "refs/remotes/origin/main"]);
        let db = |ask| {
            let mut failed = false;
            let (b, from) =
                default_branch(e, e, c, Some("origin"), None, false, ask, &mut failed).unwrap();
            (b, from, failed)
        };
        assert_eq!(
            db(AskRemote::Launch(LS_REMOTE)),
            ("develop".into(), DefaultFrom::LsRemote, false)
        );
        assert_eq!(
            db(AskRemote::Launch(Duration::ZERO)),
            ("master".into(), DefaultFrom::Guess, false)
        );
        std::fs::rename(&r.remote, r.root().join("gone.git")).unwrap();
        assert_eq!(
            db(AskRemote::Launch(LS_REMOTE)),
            ("master".into(), DefaultFrom::Guess, true)
        );
    }

    #[test]
    fn a_worktree_list_past_its_cap_is_an_error() {
        let r = Repos::new();
        let c = &r.clone;
        assert!(!worktree_list_capped(&r.env, c, LIST_CLIP)
            .unwrap()
            .is_empty());
        let e = worktree_list_capped(&r.env, c, 10).unwrap_err().to_string();
        assert!(e.contains("more than 10 bytes"), "{e}");
    }

    #[test]
    fn default_branch_is_found_in_order() {
        let r = Repos::new();
        let c = &r.clone;
        let e = &r.env;
        let first = AskRemote::First(LS_REMOTE);
        let db = |repo: &Path, remote, cached, skip, ask| {
            let mut failed = false;
            let (b, from) =
                default_branch(e, e, repo, remote, cached, skip, ask, &mut failed).unwrap();
            (b, from, failed)
        };
        assert_eq!(
            db(c, Some("origin"), None, false, first),
            ("main".into(), DefaultFrom::RemoteHead, false)
        );
        // origin/HEAD gone: the cache, then the remote asked.
        r.g(c, &["symbolic-ref", "-d", "refs/remotes/origin/HEAD"]);
        assert_eq!(
            db(c, Some("origin"), Some("trunk"), false, first),
            ("trunk".into(), DefaultFrom::Cached, false)
        );
        assert_eq!(
            db(c, Some("origin"), None, false, first),
            ("main".into(), DefaultFrom::LsRemote, false)
        );
        // A launch asks the remote before guessing too.
        assert_eq!(
            db(c, Some("origin"), None, false, AskRemote::Launch(LS_REMOTE)),
            ("main".into(), DefaultFrom::LsRemote, false)
        );
        // Renamed on the remote: origin/HEAD is stale, skipped.
        r.g(&r.remote, &["branch", "-m", "main", "trunk"]);
        r.g(c, &["remote", "set-head", "origin", "main"]);
        assert_eq!(
            db(c, Some("origin"), None, true, first),
            ("trunk".into(), DefaultFrom::LsRemote, false)
        );
        // The remote unreachable: a guess, and the failure said.
        std::fs::rename(&r.remote, r.root().join("gone.git")).unwrap();
        assert_eq!(
            db(c, Some("origin"), None, true, first),
            ("main".into(), DefaultFrom::Guess, true)
        );
        assert_eq!(
            db(c, Some("origin"), None, true, AskRemote::Never),
            ("main".into(), DefaultFrom::Guess, false)
        );
        // No remote: main, then master, then the checkout's branch.
        let solo = r.root().join("solo");
        r.g(&r.root(), &["init", "-q", "-b", "master", "solo"]);
        r.commit(&solo, "x");
        assert_eq!(
            db(&solo, None, None, false, first),
            ("master".into(), DefaultFrom::Guess, false)
        );
        r.g(&solo, &["branch", "main"]);
        assert_eq!(db(&solo, None, None, false, first).0, "main");
        r.g(&solo, &["checkout", "-q", "-b", "dev"]);
        r.g(&solo, &["branch", "-D", "main", "master"]);
        assert_eq!(
            db(&solo, None, None, false, first),
            ("dev".into(), DefaultFrom::Current, false)
        );
        assert_eq!(
            remote_of(e, &solo).unwrap(),
            Err("no remote: the local branch, not fetched".into())
        );
        assert_eq!(remote_of(e, c).unwrap(), Ok("origin".into()));
    }

    #[test]
    fn fetch_goes_to_a_private_ref_only() {
        let r = Repos::new();
        let c = &r.clone;
        r.g(&r.other, &["tag", "v1"]);
        r.g(&r.other, &["push", "-q", "origin", "v1"]);
        let before = r.snapshot();
        let origin_main = r.g(c, &["rev-parse", "refs/remotes/origin/main"]);
        let newer = r.push_remote("main", "two");
        assert_eq!(fetch(&r, c, "main"), Fetch::Fetched);
        assert_eq!(
            commit_of(&r.env, c, &private_ref("origin", "main")).unwrap(),
            newer
        );
        // The user's origin/main didn't move; no FETCH_HEAD; no tags.
        assert_eq!(
            r.g(c, &["rev-parse", "refs/remotes/origin/main"]),
            origin_main
        );
        assert!(!c.join(".git/FETCH_HEAD").exists());
        assert!(r.g(c, &["tag", "-l"]).is_empty());
        assert_eq!(r.snapshot(), before);
        // A branch the remote doesn't have.
        assert_eq!(fetch(&r, c, "nope"), Fetch::Missing);
        // Unreachable remote: an error, not a hang.
        let gone = r.root().join("gone.git");
        std::fs::rename(&r.remote, &gone).unwrap();
        assert!(matches!(
            fetch(&r, c, "main"),
            Fetch::Failed(_) | Fetch::Offline
        ));
        // The last fetched commit is still there.
        assert_eq!(
            commit_of(&r.env, c, &private_ref("origin", "main")).unwrap(),
            newer
        );
    }

    /// An old private ref for `a` doesn't block fetching `a/b`.
    #[test]
    fn nested_private_refs_are_cleared_first() {
        assert!(nested("refs/rlord/latest/o/a", "refs/rlord/latest/o/a/b"));
        assert!(nested("refs/rlord/latest/o/a/b", "refs/rlord/latest/o/a"));
        assert!(!nested("refs/rlord/latest/o/a", "refs/rlord/latest/o/ab"));
        assert!(!nested("refs/rlord/latest/o/a", "refs/rlord/latest/o/a"));
        let r = Repos::new();
        let c = &r.clone;
        r.push_remote("a", "a");
        assert_eq!(fetch(&r, c, "a"), Fetch::Fetched);
        r.g(&r.remote, &["branch", "-D", "a"]);
        r.g(&r.other, &["checkout", "-q", "main"]);
        r.g(&r.other, &["branch", "-D", "a"]);
        r.push_remote("a/b", "ab");
        assert_eq!(fetch(&r, c, "a/b"), Fetch::Fetched);
        assert!(commit_of(&r.env, c, &private_ref("origin", "a/b")).is_some());
        assert!(commit_of(&r.env, c, &private_ref("origin", "a")).is_none());
    }

    #[test]
    fn relation_and_counts() {
        let r = Repos::new();
        let c = &r.clone;
        let base = r.g(c, &["rev-parse", "HEAD"]);
        let remote2 = r.push_remote("main", "r2");
        fetch(&r, c, "main");
        assert_eq!(
            relation(&r.env, c, &base, &remote2),
            Relation::LocalBehind(1)
        );
        let local2 = r.commit(c, "l2");
        assert_eq!(
            relation(&r.env, c, &local2, &remote2),
            Relation::Diverged(1, 1)
        );
        assert_eq!(relation(&r.env, c, &local2, &base), Relation::LocalAhead(1));
        assert_eq!(relation(&r.env, c, &base, &base), Relation::Same);
        assert_eq!(count_between(&r.env, c, &base, &remote2), Some(1));
        assert!(is_ancestor(&r.env, c, &base, &remote2));
        assert!(!is_ancestor(&r.env, c, &local2, &remote2));
        let (subject, at) = commit_info(&r.env, c, &remote2).unwrap();
        assert_eq!(subject, "r2");
        assert!(at > 0);
    }

    #[test]
    fn tree_is_detached_moved_and_removed_without_touching_the_user() {
        let r = Repos::new();
        let c = &r.clone;
        let repo = repo_of(&r, c);
        // Work in progress in the user's clone, and a stash.
        std::fs::write(c.join("wip.txt"), "x").unwrap();
        std::fs::write(c.join("stashed.txt"), "s").unwrap();
        r.g(c, &["add", "stashed.txt"]);
        r.g(c, &["stash", "-q"]);
        // A post-checkout hook that would leave a marker.
        let marker = r.root().join("hook-ran");
        let hook = c.join(".git/hooks/post-checkout");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let before = r.snapshot();

        let one = r.g(c, &["rev-parse", "HEAD"]);
        let dir = tree_path(c);
        let rec = Some(dir.as_path());
        assert_eq!(entry(&r, &repo, &dir), None);
        tree_add(&r.env, &repo, &dir, &one, false).unwrap();
        let e = entry(&r, &repo, &dir).unwrap();
        assert!(e.detached && !e.prunable);
        assert_eq!(commit_of(&r.env, &dir, "HEAD").unwrap(), one);
        assert!(!marker.exists(), "the user's hook ran");

        // Moved to a newer commit; edits in it are discarded.
        let two = r.push_remote("main", "two");
        fetch(&r, c, "main");
        std::fs::write(dir.join("scratch.txt"), "edit").unwrap();
        let m = tree_move(&r.env, &repo, &dir, &two, rec, None, &e, true).unwrap();
        assert_eq!(m.discarded, 1);
        assert!(!m.recreated);
        assert_eq!(commit_of(&r.env, &dir, "HEAD").unwrap(), two);
        assert!(!dir.join("scratch.txt").exists());
        // Nothing to do: nothing done.
        assert_eq!(
            tree_move(&r.env, &repo, &dir, &two, rec, Some(two.as_str()), &e, true).unwrap(),
            Moved {
                unchanged: true,
                ..Moved::default()
            }
        );

        // A lock (git may be running there): an error, and the tree is
        // left as it is, not rebuilt.
        let admin = r.g(&dir, &["rev-parse", "--absolute-git-dir"]);
        let lock = Path::new(&admin).join("index.lock");
        std::fs::write(&lock, "").unwrap();
        std::fs::write(dir.join("scratch.txt"), "edit").unwrap();
        let err = tree_move(&r.env, &repo, &dir, &one, rec, None, &e, true).unwrap_err();
        assert!(err.to_string().contains("left as it is"), "{err}");
        assert!(dir.join("scratch.txt").exists());
        assert_eq!(commit_of(&r.env, &dir, "HEAD").unwrap(), two);
        std::fs::remove_file(&lock).unwrap();
        let m = tree_move(&r.env, &repo, &dir, &one, rec, None, &e, true).unwrap();
        assert!(!m.recreated);
        assert_eq!(commit_of(&r.env, &dir, "HEAD").unwrap(), one);
        // Its folder deleted: made again.
        std::fs::remove_dir_all(&dir).unwrap();
        let e = entry(&r, &repo, &dir).unwrap();
        assert!(e.prunable);
        assert!(
            tree_move(&r.env, &repo, &dir, &two, rec, None, &e, true)
                .unwrap()
                .recreated
        );
        assert_eq!(commit_of(&r.env, &dir, "HEAD").unwrap(), two);
        // Its `.git` file broken, pointing nowhere rlord knows: never acts
        // on the user's checkout, and isn't deleted either (not
        // recognizably rlord's from inside): an error, left as it is.
        let good = std::fs::read_to_string(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git"), "gitdir: /nowhere\n").unwrap();
        let e = entry(&r, &repo, &dir).unwrap();
        assert!(tree_move(&r.env, &repo, &dir, &one, rec, None, &e, true).is_err());
        assert!(dir.exists());
        // Put right: moved again as usual.
        std::fs::write(dir.join(".git"), &good).unwrap();
        let e = entry(&r, &repo, &dir).unwrap();
        tree_move(&r.env, &repo, &dir, &one, rec, None, &e, true).unwrap();
        assert_eq!(commit_of(&r.env, &dir, "HEAD").unwrap(), one);

        assert!(!marker.exists(), "the user's hook ran");
        assert!(c.join("wip.txt").exists());
        // Removed, with the private refs.
        let e = entry(&r, &repo, &dir).unwrap();
        tree_remove(&r.env, &repo, &dir, rec, &e, true).unwrap();
        assert_eq!(entry(&r, &repo, &dir), None);
        assert!(!dir.exists());
        assert_eq!(drop_refs(&r.env, c).unwrap(), 1);
        assert_eq!(commit_of(&r.env, c, &private_ref("origin", "main")), None);
        assert_eq!(r.snapshot(), before);
        // (The hook would have run for a plain checkout.)
        r.g(
            c,
            &[
                "-c",
                "core.hooksPath=.git/hooks",
                "checkout",
                "-q",
                "-b",
                "x",
            ],
        );
        assert!(marker.exists());
    }

    #[test]
    fn only_a_detached_tree_at_its_own_path_is_adopted() {
        let list = parse_worktrees(
            "worktree /r\nHEAD a\nbranch refs/heads/main\n\n\
             worktree /r/.worktrees/.rlord-latest\nHEAD b\ndetached\n\n\
             worktree /r/.other/.rlord-latest\nHEAD c\ndetached\n\n\
             worktree /r/.wt2/.rlord-latest\nHEAD d\nbranch refs/heads/s\n",
        );
        let expected = tree_path(Path::new("/r"));
        assert!(adoptable(&list[1], &expected));
        // Another path, or on a branch: not rlord's.
        assert!(!adoptable(&list[2], &expected));
        let wt2 = Path::new("/r/.wt2").join(TREE_NAME);
        assert!(!adoptable(&list[3], &wt2));
        assert!(!adoptable(&list[0], Path::new("/r")));
    }

    /// A checkout killed partway can leave HEAD at the commit with files
    /// missing: without a completed commit on record, the move runs in
    /// full. A folder deleted under its registration is added again with
    /// git alone.
    #[test]
    fn an_unfinished_or_missing_tree_is_completed() {
        let r = Repos::new();
        let c = &r.clone;
        std::fs::write(r.other.join("f.txt"), "x").unwrap();
        r.g(&r.other, &["add", "f.txt"]);
        let head = r.commit(&r.other, "f");
        r.g(&r.other, &["push", "-q", "origin", "main"]);
        r.g(c, &["pull", "-q"]);
        let repo = repo_of(&r, c);
        let dir = tree_path(c);
        let rec = Some(dir.as_path());
        tree_add(&r.env, &repo, &dir, &head, false).unwrap();
        std::fs::remove_file(dir.join("f.txt")).unwrap();
        let e = entry(&r, &repo, &dir).unwrap();
        // Recorded as completed and clean: nothing runs but one status.
        let m = tree_move(&r.env, &repo, &dir, &head, rec, Some(&head), &e, false).unwrap();
        assert!(!m.unchanged && dir.join("f.txt").exists(), "{m:?}");
        assert_eq!(m.discarded, 1);
        let m = tree_move(&r.env, &repo, &dir, &head, rec, Some(&head), &e, false).unwrap();
        assert!(m.unchanged);
        // Not recorded as completed: checked out in full.
        std::fs::remove_file(dir.join("f.txt")).unwrap();
        let m = tree_move(&r.env, &repo, &dir, &head, rec, None, &e, false).unwrap();
        assert!(!m.unchanged);
        assert!(dir.join("f.txt").exists());
        // Its folder gone: added again over its registration, no removal
        // first (works without touching files: `fs_ok` false).
        std::fs::remove_dir_all(&dir).unwrap();
        let e = entry(&r, &repo, &dir).unwrap();
        assert!(e.prunable);
        let m = tree_move(&r.env, &repo, &dir, &head, rec, Some(&head), &e, false).unwrap();
        assert!(m.recreated && dir.join("f.txt").exists());
    }

    /// Registered, its folder there but its `.git` gone (git calls it
    /// prunable too): not force-added over (that fails every time), but
    /// removed and made again.
    #[test]
    fn a_tree_without_its_git_file_is_made_again() {
        let r = Repos::new();
        let c = &r.clone;
        let repo = repo_of(&r, c);
        let head = r.g(c, &["rev-parse", "HEAD"]);
        let dir = tree_path(c);
        let rec = Some(dir.as_path());
        tree_add(&r.env, &repo, &dir, &head, false).unwrap();
        std::fs::remove_file(dir.join(".git")).unwrap();
        let e = entry(&r, &repo, &dir).unwrap();
        assert!(e.prunable);
        let m = tree_move(&r.env, &repo, &dir, &head, rec, None, &e, true).unwrap();
        assert!(m.recreated);
        assert_eq!(commit_of(&r.env, &dir, "HEAD").unwrap(), head);
    }

    /// A folder at the tree's path that git doesn't know: replaced only
    /// when it is a leftover rlord tree and rlord may touch files there.
    #[test]
    fn a_folder_git_doesnt_know_is_replaced_only_if_its_rlords() {
        let r = Repos::new();
        let c = &r.clone;
        let repo = repo_of(&r, c);
        let head = r.g(c, &["rev-parse", "HEAD"]);
        let dir = tree_path(c);
        // A leftover: a `.git` file pointing into the repo's worktrees.
        std::fs::create_dir_all(&dir).unwrap();
        let admin = repo.common.join("worktrees").join(TREE_NAME);
        std::fs::write(dir.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        std::fs::write(dir.join("old.txt"), "x").unwrap();
        assert!(leftover(&repo, &dir));
        // Not when rlord may not touch files there.
        let e = tree_add_fresh(&r.env, &repo, &dir, &head, false).unwrap_err();
        assert!(e.to_string().contains("touches no files"), "{e}");
        assert!(dir.join("old.txt").exists());
        assert!(tree_add_fresh(&r.env, &repo, &dir, &head, true).unwrap());
        assert!(!dir.join("old.txt").exists());
        assert_eq!(commit_of(&r.env, &dir, "HEAD").unwrap(), head);
        // Something else there: never touched.
        let other = c.join(".worktrees2").join(TREE_NAME);
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("mine.txt"), "x").unwrap();
        assert!(!leftover(&repo, &other));
        let e = tree_add_fresh(&r.env, &repo, &other, &head, true).unwrap_err();
        assert!(e.to_string().contains("isn't a leftover"), "{e}");
        assert!(other.join("mine.txt").exists());
    }

    /// A worktree rlord didn't record, or one on a branch (a session's),
    /// is never moved or removed, whatever its name.
    #[test]
    fn a_session_worktree_is_never_moved_or_removed() {
        let r = Repos::new();
        let c = &r.clone;
        let repo = repo_of(&r, c);
        let head = r.g(c, &["rev-parse", "HEAD"]);
        let newer = r.push_remote("main", "newer");
        fetch(&r, c, "main");
        // A session worktree at the tree's old name, and one (made by
        // hand) at its new name, each on a branch with work in it.
        for (name, branch) in [("rlord-latest", "s1"), (TREE_NAME, "s2")] {
            let dir = c.join(".worktrees").join(name);
            r.g(
                c,
                &["worktree", "add", "-q", "-b", branch, dir.to_str().unwrap()],
            );
            std::fs::write(dir.join("work.txt"), "mine").unwrap();
            let e = entry(&r, &repo, &dir).unwrap();
            // Unrecorded: refused, moved or removed.
            assert!(tree_move(&r.env, &repo, &dir, &newer, None, None, &e, true).is_err());
            assert!(tree_remove(&r.env, &repo, &dir, None, &e, true).is_err());
            // Recorded, but not named as the tree: refused too.
            if name != TREE_NAME {
                let rec = Some(dir.as_path());
                assert!(tree_move(&r.env, &repo, &dir, &newer, rec, None, &e, true).is_err());
                assert!(tree_remove(&r.env, &repo, &dir, rec, &e, true).is_err());
            }
            assert_eq!(r.g(&dir, &["rev-parse", "HEAD"]), head);
            assert_eq!(r.g(&dir, &["symbolic-ref", "--short", "HEAD"]), branch);
            assert!(dir.join("work.txt").exists());
        }
        // rlord's own tree (by name and record) with a branch checked out
        // in it: detached again and moved, not refused forever. (Removing
        // it is still refused while the branch is there.)
        let dir = c.join(".worktrees").join(TREE_NAME);
        let rec = Some(dir.as_path());
        let e = entry(&r, &repo, &dir).unwrap();
        assert!(tree_remove(&r.env, &repo, &dir, rec, &e, true).is_err());
        // An edit to a tracked file, thrown away by the detach: counted
        // (with the untracked work.txt).
        std::fs::write(dir.join("f.txt"), "one").unwrap();
        r.g(&dir, &["add", "f.txt"]);
        let s2 = r.commit(&dir, "f");
        std::fs::write(dir.join("f.txt"), "edited").unwrap();
        let m = tree_move(&r.env, &repo, &dir, &newer, rec, None, &e, true).unwrap();
        assert_eq!(m.discarded, 2, "{m:?}");
        assert_eq!(r.g(&dir, &["rev-parse", "HEAD"]), newer);
        assert!(
            !run(&r.env, &dir, &["symbolic-ref", "-q", "HEAD"], PLUMBING)
                .unwrap()
                .ok
        );
        // (The branch is still there, at its commit.)
        assert_eq!(r.g(c, &["rev-parse", "s2"]), s2);
        // A detached worktree rlord didn't record: left too.
        let dir = c.join(".other").join(TREE_NAME);
        r.g(
            c,
            &["worktree", "add", "-q", "--detach", dir.to_str().unwrap()],
        );
        let e = entry(&r, &repo, &dir).unwrap();
        assert!(tree_remove(&r.env, &repo, &dir, None, &e, true).is_err());
        assert!(dir.exists());
    }

    /// With the tree's `.git` gone, git run in the tree never reaches the
    /// user's checkout around it: its branch, edits and untracked files
    /// stay as they are, through a direct call and a whole move.
    #[test]
    fn git_in_a_broken_tree_never_reaches_the_users_checkout() {
        let r = Repos::new();
        let c = &r.clone;
        let repo = repo_of(&r, c);
        let head = r.g(c, &["rev-parse", "HEAD"]);
        let dir = tree_path(c);
        tree_add(&r.env, &repo, &dir, &head, false).unwrap();
        // The user's work: a tracked file edited, an untracked one.
        std::fs::write(c.join("f.txt"), "one").unwrap();
        r.g(c, &["add", "f.txt"]);
        let mine = r.commit(c, "f");
        std::fs::write(c.join("f.txt"), "edited").unwrap();
        std::fs::write(c.join("new.txt"), "mine").unwrap();
        std::fs::remove_file(dir.join(".git")).unwrap();
        let o = tree_run(
            &r.env,
            &dir,
            &["checkout", "--quiet", "--detach", "--force"],
            PLUMBING,
        )
        .unwrap();
        assert!(!o.ok);
        assert!(
            !tree_run(&r.env, &dir, &["clean", "-fdxq"], PLUMBING)
                .unwrap()
                .ok
        );
        assert!(tree_git(&r.env, &dir, &["clean", "-fdxq"], PLUMBING).is_err());
        let newer = r.push_remote("main", "newer");
        fetch(&r, c, "main");
        let e = entry(&r, &repo, &dir).unwrap();
        let rec = Some(dir.as_path());
        let _ = tree_move(&r.env, &repo, &dir, &newer, rec, Some(&head), &e, true);
        let untouched = |c: &Path| {
            assert_eq!(r.g(c, &["symbolic-ref", "--short", "HEAD"]), "main");
            assert_eq!(r.g(c, &["rev-parse", "HEAD"]), mine);
            assert_eq!(std::fs::read_to_string(c.join("f.txt")).unwrap(), "edited");
            assert_eq!(std::fs::read_to_string(c.join("new.txt")).unwrap(), "mine");
        };
        untouched(c);
        // The moved-repo path (`tree_add_fresh` repairs, then checks out
        // and cleans in the tree): with the tree broken after the move,
        // and whole.
        let moved = r.root().join("moved-clone");
        std::fs::rename(c, &moved).unwrap();
        let repo = repo_of(&r, &moved);
        let dir = tree_path(&moved);
        let git_file = std::fs::read_to_string(dir.join(".git")).unwrap();
        std::fs::write(
            dir.join(".git"),
            "gitdir: /nowhere/.git/worktrees/-rlord-latest\n",
        )
        .unwrap();
        let _ = tree_add_fresh(&r.env, &repo, &dir, &newer, true);
        untouched(&moved);
        std::fs::write(dir.join(".git"), git_file).unwrap();
        assert!(tree_add_fresh(&r.env, &repo, &dir, &head, true).unwrap());
        assert_eq!(r.g(&dir, &["rev-parse", "HEAD"]), head);
        untouched(&moved);
    }

    #[test]
    fn a_folder_without_its_git_file_is_never_mistaken_for_the_tree() {
        let r = Repos::new();
        let c = &r.clone;
        let repo = repo_of(&r, c);
        let dir = tree_path(c);
        std::fs::create_dir_all(&dir).unwrap();
        // Here git would find the user's clone: not rlord's tree.
        assert!(verify_tree(&r.env, &repo, &dir).is_err());
        let head = r.g(c, &["rev-parse", "HEAD"]);
        std::fs::write(c.join("wip.txt"), "x").unwrap();
        // Not registered: adding over it fails (the folder isn't empty),
        // and nothing ran in the clone.
        std::fs::write(dir.join("f"), "").unwrap();
        assert!(tree_add(&r.env, &repo, &dir, &head, false).is_err());
        assert!(c.join("wip.txt").exists());
        assert_eq!(r.g(c, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    }

    #[test]
    fn checked_out_reports_the_users_checkout() {
        let r = Repos::new();
        let c = &r.clone;
        std::fs::write(c.join("a.txt"), "x").unwrap();
        std::fs::write(c.join("b.txt"), "x").unwrap();
        let list = worktree_list(&r.env, c).unwrap();
        let (at, n) = checked_out(&r.env, &list, "main", true).unwrap();
        assert_eq!(Path::new(&at), c.as_path());
        assert_eq!(n, Some(2));
        assert_eq!(checked_out(&r.env, &list, "other", true), None);
        // Not asked to count: no status run.
        assert_eq!(
            checked_out(&r.env, &list, "main", false),
            Some((at.clone(), None))
        );
        // (Nothing was added to its index.)
        assert!(r.g(c, &["diff", "--cached", "--name-only"]).is_empty());
    }

    #[test]
    fn shallow_clone_and_warnings() {
        let r = Repos::new();
        std::fs::write(r.other.join(".gitmodules"), "").unwrap();
        std::fs::write(r.other.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
        r.g(&r.other, &["add", "."]);
        r.commit(&r.other, "attrs");
        r.g(&r.other, &["push", "-q", "origin", "main"]);
        let shallow = r.root().join("shallow");
        r.g(
            &r.root(),
            &[
                "clone",
                "-q",
                "--depth",
                "1",
                &format!("file://{}", r.remote.display()),
                "shallow",
            ],
        );
        assert!(is_shallow(&r.env, &shallow));
        assert!(!is_shallow(&r.env, &r.clone));
        r.push_remote("main", "four");
        assert_eq!(fetch(&r, &shallow, "main"), Fetch::Fetched);
        let head = commit_of(&r.env, &shallow, &private_ref("origin", "main")).unwrap();
        let repo = repo_of(&r, &shallow);
        let dir = tree_path(&shallow);
        tree_add(&r.env, &repo, &dir, &head, false).unwrap();
        let w = tree_warnings(&r.env, &shallow, &dir).join("\n");
        assert!(w.contains("submodules_not_checked_out"), "{w}");
        assert!(w.contains("lfs_pointers"), "{w}");
        assert!(!w.contains("sparse"), "{w}");
    }
}
