//! Git worktree workspaces: one worktree per session under
//! `<repo>/<workspace_dir>/<sanitized-branch>`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

fn git(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .context("running git (is git installed?)")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.iter()
                .find(|a| !(a.starts_with('-') || Path::new(a).is_absolute()))
                .unwrap_or(&"?"),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Top-level directory of the repo containing `path` (also works from inside a
/// worktree, returning that worktree's root), or None when not in a repo.
pub fn repo_root(path: &str) -> Option<String> {
    git(&["-C", path, "rev-parse", "--show-toplevel"])
        .ok()
        .filter(|s| !s.is_empty())
}

/// The main repository root for `path`, resolving worktrees back to the
/// primary checkout so all session worktrees nest under one `.worktrees/`.
pub fn main_repo_root(path: &str) -> Option<String> {
    let common = git(&["-C", path, "rev-parse", "--git-common-dir"]).ok()?;
    // A relative answer is relative to the folder we asked from (`-C`),
    // not the top level: from a subfolder it is e.g. `../.git`.
    let common = if Path::new(&common).is_absolute() {
        PathBuf::from(common)
    } else {
        PathBuf::from(path).join(common)
    };
    let root = common.parent()?.to_path_buf();
    std::fs::canonicalize(root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Branch-safe form of a session name: anything git refuses becomes `-`.
pub fn sanitize_branch(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in name.trim().chars() {
        let ok = c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/');
        let c = if ok { c } else { '-' };
        if c == '-' {
            if !last_dash && !out.is_empty() {
                out.push('-');
            }
            last_dash = true;
        } else {
            out.push(c);
            last_dash = false;
        }
    }
    let out = out
        .trim_matches(|c| c == '-' || c == '/' || c == '.')
        .to_string();
    if out.is_empty() {
        "session".to_string()
    } else {
        out
    }
}

/// Directory name for a branch: slashes flattened.
pub fn workspace_dirname(branch: &str) -> String {
    branch.replace('/', "-")
}

pub fn workspace_path(repo: &str, workspace_dir: &str, branch: &str) -> PathBuf {
    Path::new(repo)
        .join(workspace_dir)
        .join(workspace_dirname(branch))
}

/// Create the worktree at `dir` on `branch`. With no `start`: a new branch
/// off the repo's HEAD, or a checkout of `branch` if it already exists.
/// With `start` (a fork's: its parent's last commit; a new session's
/// base, `latest.rs`): a new branch started there, with no upstream
/// (`--no-track`: a start at a private ref never becomes it), and an
/// existing branch is refused. Errors if git refuses (e.g.
/// the branch is checked out elsewhere), or the folder exists.
pub fn worktree_add(repo: &str, branch: &str, dir: &Path, start: Option<&str>) -> Result<()> {
    if dir.exists() {
        bail!("workspace directory already exists: {}", dir.display());
    }
    let exists = branch_exists(repo, branch);
    // (What was there before: a failure removes only what this call made.)
    let was_registered = worktree_registered(repo, dir);
    if exists && start.is_some() {
        bail!("branch {branch} already exists");
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dir_s = dir.to_string_lossy();
    let made = match (exists, start) {
        (true, _) => git(&["-C", repo, "worktree", "add", &dir_s, branch]),
        (false, None) => git(&["-C", repo, "worktree", "add", "-b", branch, &dir_s]),
        (false, Some(start)) => git(&[
            "-C",
            repo,
            "worktree",
            "add",
            "--no-track",
            "-b",
            branch,
            &dir_s,
            start,
        ]),
    };
    if let Err(e) = made {
        // git makes a new branch before its folder: a failure after that
        // leaves the branch behind (and maybe this worktree, registered).
        // Only what this call made is removed: a registration at `dir`
        // that wasn't there before, then a branch that wasn't (`branch -D`
        // refuses one checked out anywhere). No repo-wide prune.
        if !was_registered && worktree_registered(repo, dir) {
            let _ = git(&["-C", repo, "worktree", "remove", "--force", &dir_s]);
        }
        if !exists && branch_exists(repo, branch) {
            let _ = git(&["-C", repo, "branch", "-D", branch]);
        }
        return Err(e);
    }
    Ok(())
}

/// Whether `repo` lists a worktree at `dir`.
fn worktree_registered(repo: &str, dir: &Path) -> bool {
    let want = canonical_path(dir);
    git(&["-C", repo, "worktree", "list", "--porcelain"]).is_ok_and(|out| {
        out.lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .any(|w| canonical_path(Path::new(w)) == want)
    })
}

/// `p` with symlinks resolved, also when it is missing: its nearest
/// existing ancestor resolved, the rest joined back on.
fn canonical_path(p: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(p) {
        return real;
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            canonical_path(parent).join(name)
        }
        _ => p.to_path_buf(),
    }
}

/// The commit checked out in the folder `dir` (its HEAD).
pub fn head_commit(dir: &str) -> Result<String> {
    git(&["-C", dir, "rev-parse", "--verify", "HEAD"])
}

/// The branch checked out in `dir` (None: detached, or not a repo).
pub fn checked_out_branch(dir: &str) -> Option<String> {
    git(&["-C", dir, "symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
}

/// Whether the checkout at `dir` has anything not committed: changed
/// files, and untracked ones not ignored (a new worktree has neither).
/// None when git can't say.
pub fn has_changes(dir: &str) -> Option<bool> {
    git(&[
        "-C",
        dir,
        "status",
        "--porcelain",
        "--untracked-files=normal",
    ])
    .ok()
    .map(|out| !out.is_empty())
}

/// Undo a fork's `worktree_add`: remove the worktree at `dir` (with whatever is
/// in it) and its branch `branch`. For a worktree rlord just made and
/// nothing ran in yet. Both are tried, whatever the first does (twice
/// `--force`: even a locked worktree goes); the first error is returned.
pub fn worktree_discard(repo: &str, branch: &str, dir: &Path) -> Result<()> {
    let dir_s = dir.to_string_lossy();
    let removed = git(&[
        "-C", repo, "worktree", "remove", "--force", "--force", &dir_s,
    ]);
    let deleted = git(&["-C", repo, "branch", "-D", branch]);
    removed?;
    deleted?;
    Ok(())
}

/// The names of `repo`'s local branches (one `git for-each-ref`); empty
/// when git can't say.
pub fn branches(repo: &str) -> std::collections::HashSet<String> {
    git(&[
        "-C",
        repo,
        "for-each-ref",
        "--format=%(refname:short)",
        "refs/heads",
    ])
    .map(|out| out.lines().map(str::to_string).collect())
    .unwrap_or_default()
}

/// Whether `repo` has a local branch named `branch`.
pub fn branch_exists(repo: &str, branch: &str) -> bool {
    crate::latest::commit_of(
        &crate::latest::GitEnv::default(),
        Path::new(repo),
        &format!("refs/heads/{branch}"),
    )
    .is_some()
}

/// Whether some checkout of `repo` (the main one or a worktree) has
/// `branch` checked out; `git worktree add` refuses a second one.
pub fn branch_checked_out(repo: &str, branch: &str) -> bool {
    crate::latest::worktree_list(&crate::latest::GitEnv::default(), Path::new(repo))
        .map(|l| l.iter().any(|e| e.branch.as_deref() == Some(branch)))
        .unwrap_or(false)
}

/// Keep the workspace dir out of `git status` without touching the tracked
/// .gitignore: append to `.git/info/exclude` once.
pub fn ensure_excluded(repo: &str, workspace_dir: &str) -> Result<()> {
    let git_dir = git(&["-C", repo, "rev-parse", "--git-common-dir"])?;
    let git_dir = if Path::new(&git_dir).is_absolute() {
        PathBuf::from(git_dir)
    } else {
        Path::new(repo).join(git_dir)
    };
    let exclude = git_dir.join("info").join("exclude");
    let line = format!("/{}/", workspace_dir.trim_matches('/'));
    // One at a time (two launches into one repo would both add the line),
    // and appended in one write, never rewriting what is there.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    if let Some(p) = exclude.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut add = String::new();
    if !current.is_empty() && !current.ends_with('\n') {
        add.push('\n');
    }
    add.push_str(&line);
    add.push('\n');
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)?
        .write_all(add.as_bytes())?;
    Ok(())
}

/// Directories never descended into when scanning for repos: hidden dirs
/// (including our own `.worktrees`) and the usual dependency/build trees.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "venv",
    "__pycache__",
    "Library",
    "snap",
    "go",
    "dist",
    "build",
];

/// A folder the repo scan stayed out of, and why.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Skipped {
    pub path: String,
    pub why: crate::privacy::Why,
}

/// What a repo scan found, and the folders it stayed out of (at most
/// `MAX_SKIPPED`; `skipped_more` counts the rest).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    pub repos: Vec<String>,
    pub skipped: Vec<Skipped>,
    pub skipped_more: usize,
}

impl From<Vec<String>> for Scan {
    fn from(repos: Vec<String>) -> Scan {
        Scan {
            repos,
            ..Scan::default()
        }
    }
}

/// How many skipped folders a scan lists.
pub const MAX_SKIPPED: usize = 50;

/// Git repositories under `roots`, up to `depth` levels down. A repo is a
/// directory containing `.git`. We keep descending below a repo: a dotfiles
/// repo in `$HOME` must not hide everything under it, and vendored/nested
/// repos are real repos too (worktrees live under hidden dirs and are
/// skipped). Sorted, deduplicated.
///
/// The scan stays out of folders where a read could block (and reports
/// them in `skipped`): on macOS the ones the system guards with a privacy
/// prompt (`privacy::Guard`: reading one waits on a dialog, so a scan of
/// `~` once sat for an hour), and on every platform another filesystem
/// than the root's (a network mount, a removable disk), like `find -xdev`.
/// Symlinks are never followed. A root is scanned whatever it is (the user
/// named it: the code folder and each `repo_roots` entry), on its own
/// device, including inside its own protected folder; a root nested in
/// another, or named twice, is walked once. Nothing else is entered: not a
/// protected folder met on the way, not another filesystem, whatever runs
/// there (`privacy::Guard` says why). A folder whose metadata can't be
/// read is skipped silently.
///
/// Known limits: on macOS a root of `/` skips `/Users` and the other
/// firmlinked folders (another device); a btrfs subvolume or ZFS dataset
/// nested under a root is skipped and reported. Naming the folder in
/// `repo_roots` scans it.
pub fn scan_repos(roots: &[String], depth: u8) -> Scan {
    // (Two spellings of one folder, a symlink to another root, or another
    // case of it on macOS: walked once.)
    let guard = crate::privacy::Guard::new();
    let mut canon: Vec<PathBuf> = Vec::new();
    for r in roots {
        if let Ok(c) = std::fs::canonicalize(crate::config::expand_tilde(r)) {
            if !canon.iter().any(|x| guard.same(x, &c)) {
                canon.push(c);
            }
        }
    }
    scan_in(&canon, depth, guard, &device)
}

/// `scan_repos` over canonical roots, with a given guard and device
/// look-up (tests).
fn scan_in(
    roots: &[PathBuf],
    depth: u8,
    guard: crate::privacy::Guard,
    dev_of: &dyn Fn(&Path) -> Option<u64>,
) -> Scan {
    let ctx = Walk {
        guard,
        dev_of,
        roots: roots.to_vec(),
    };
    let mut out = Scan::default();
    for root in roots {
        walk(root, root, depth, dev_of(root), &ctx, &mut out);
    }
    finish(out, ctx.guard.folds_case())
}

/// Sort and dedupe (ignoring ASCII case where paths do, `fold`), and cap
/// the skipped list.
fn finish(mut out: Scan, fold: bool) -> Scan {
    let key = |p: &str| {
        if fold {
            p.to_ascii_lowercase()
        } else {
            p.to_string()
        }
    };
    out.repos.sort_by_key(|p| key(p));
    out.repos.dedup_by(|a, b| key(a) == key(b));
    out.skipped.sort_by_key(|s| (key(&s.path), s.why));
    out.skipped
        .dedup_by(|a, b| key(&a.path) == key(&b.path) && a.why == b.why);
    if out.skipped.len() > MAX_SKIPPED {
        out.skipped_more = out.skipped.len() - MAX_SKIPPED;
        out.skipped.truncate(MAX_SKIPPED);
    }
    out
}

/// The device a folder is on (not following a symlink).
fn device(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).ok().map(|m| m.dev())
}

/// What a walk checks each folder against.
struct Walk<'a> {
    guard: crate::privacy::Guard,
    dev_of: &'a dyn Fn(&Path) -> Option<u64>,
    /// Every root: one nested in another is walked as its own root only.
    roots: Vec<PathBuf>,
}

/// `root`: the root this walk started from. `dev`: the device of the
/// filesystem being walked (None: unknown, no check).
fn walk(dir: &Path, root: &Path, depth: u8, dev: Option<u64>, ctx: &Walk, out: &mut Scan) {
    use crate::privacy::Why;
    if dir.join(".git").exists() {
        out.repos.push(dir.to_string_lossy().into_owned());
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        // Don't follow symlinks: loops and duplicates aren't worth it.
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let path = entry.path();
        if ctx.roots.iter().any(|r| ctx.guard.same(r, &path)) {
            continue;
        }
        // A skipped folder that holds a root isn't reported: the root
        // (what the user named in it) is walked on its own.
        let report = |out: &mut Scan, why| {
            if !ctx.roots.iter().any(|r| ctx.guard.is_within(r, &path)) {
                out.skipped.push(Skipped {
                    path: path.to_string_lossy().into_owned(),
                    why,
                });
            }
        };
        // By name, before anything touches it.
        if let Some(why) = ctx.guard.prompts_within(&path, root) {
            report(out, why);
            continue;
        }
        // Unreadable, or gone since it was listed: nothing to report.
        let Some(child) = (ctx.dev_of)(&path) else {
            continue;
        };
        if dev.is_some_and(|d| d != child) {
            report(out, Why::OtherFilesystem);
            continue;
        }
        walk(&path, root, depth - 1, dev, ctx, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_branch("myapp-login"), "myapp-login");
        assert_eq!(sanitize_branch("Fix login bug!"), "Fix-login-bug");
        assert_eq!(sanitize_branch("feature/x y"), "feature/x-y");
        assert_eq!(sanitize_branch("   "), "session");
        assert_eq!(workspace_dirname("feature/x-y"), "feature-x-y");
    }

    fn init_repo(p: &Path) {
        std::fs::create_dir_all(p).unwrap();
        git(&["-C", p.to_str().unwrap(), "init", "-q"]).unwrap();
    }

    /// A walk with this guard and devices, as `scan_repos` runs it.
    fn walk_with(
        root: &Path,
        guard: crate::privacy::Guard,
        dev_of: &dyn Fn(&Path) -> Option<u64>,
    ) -> Scan {
        let ctx = Walk {
            guard,
            dev_of,
            roots: vec![root.to_path_buf()],
        };
        let mut out = Scan::default();
        walk(root, root, 3, dev_of(root), &ctx, &mut out);
        finish(out, false)
    }

    #[test]
    fn a_fork_worktree_starts_at_the_parents_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_repo(&repo);
        let r = repo.to_str().unwrap();
        let commit = |msg: &str| {
            git(&[
                "-C",
                r,
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                msg,
            ])
            .unwrap()
        };
        commit("one");
        let first = head_commit(r).unwrap();
        commit("two");
        std::fs::write(repo.join("f.txt"), "x").unwrap();
        git(&["-C", r, "add", "f.txt"]).unwrap();
        assert_eq!(has_changes(r), Some(true));
        let wt = repo.join(".worktrees").join("x-fork");
        worktree_add(r, "x-fork", &wt, Some(&first)).unwrap();
        let w = wt.to_str().unwrap();
        assert_eq!(head_commit(w).unwrap(), first);
        // Nothing uncommitted was carried over.
        assert_eq!(has_changes(w), Some(false));
        assert!(!wt.join("f.txt").exists());
        // The branch and the folder are taken now.
        let e = worktree_add(r, "x-fork", &repo.join("other"), Some(&first)).unwrap_err();
        assert!(e.to_string().contains("already exists"), "{e}");
        assert!(worktree_add(r, "y-fork", &wt, Some(&first)).is_err());
        // A failure after git made the branch (its folder can't be made)
        // leaves no branch behind.
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        // (Root writes there anyway: nothing fails, nothing to check.)
        if std::fs::create_dir(locked.join("probe")).is_err() {
            let e = worktree_add(r, "z-fork", &locked.join("z"), Some(&first));
            assert!(e.is_err());
            assert!(!branch_exists(r, "z-fork"));
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        // The user's own registration of a folder that is gone ("missing
        // but already registered"): git refuses the add, and that
        // registration survives (only this call's new branch goes).
        let gone = repo.join(".worktrees").join("gone");
        worktree_add(r, "gone-branch", &gone, Some(&first)).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();
        assert!(worktree_add(r, "new-branch", &gone, Some(&first)).is_err());
        assert!(worktree_registered(r, &gone));
        assert!(branch_exists(r, "gone-branch"));
        assert!(!branch_exists(r, "new-branch"));
        // A missing folder under a symlinked path still matches.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        assert!(worktree_registered(
            r,
            &link.join(".worktrees").join("gone")
        ));
        // (Tidied up for what follows.)
        git(&["-C", r, "worktree", "prune"]).unwrap();
        git(&["-C", r, "branch", "-D", "gone-branch"]).unwrap();
        assert_eq!(
            has_changes(&dir.path().join("none").to_string_lossy()),
            None
        );
        // A file not added yet counts too (it doesn't come along either).
        std::fs::write(wt.join("new.txt"), "x").unwrap();
        assert_eq!(has_changes(w), Some(true));
        assert_eq!(
            branches(r),
            ["x-fork".to_string(), current_branch(r)]
                .into_iter()
                .collect()
        );
        // Undone: the worktree and its branch are gone, even locked.
        git(&["-C", r, "worktree", "lock", w]).unwrap();
        worktree_discard(r, "x-fork", &wt).unwrap();
        assert!(!wt.exists() && !branch_exists(r, "x-fork"));
        // A worktree that can't be removed still has its branch deleted
        // (the error said).
        let wt2 = repo.join(".worktrees").join("z-fork");
        worktree_add(r, "z-fork", &wt2, Some(&first)).unwrap();
        let elsewhere = repo.join(".worktrees").join("not-a-worktree");
        std::fs::create_dir_all(&elsewhere).unwrap();
        git(&[
            "-C",
            r,
            "worktree",
            "remove",
            "--force",
            wt2.to_str().unwrap(),
        ])
        .unwrap();
        assert!(worktree_discard(r, "z-fork", &elsewhere).is_err());
        assert!(!branch_exists(r, "z-fork"));
    }

    /// The branch HEAD is on in `r`.
    fn current_branch(r: &str) -> String {
        git(&["-C", r, "rev-parse", "--abbrev-ref", "HEAD"]).unwrap()
    }

    #[test]
    fn scan_repos_scans_roots() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |rel: &str| {
            let p = dir.path().join(rel);
            init_repo(&p);
            p
        };
        // The scan root is itself a repo (a dotfiles home) — must not stop there.
        git(&["-C", dir.path().to_str().unwrap(), "init", "-q"]).unwrap();
        let a = mk("ko/alpha");
        let b = mk("code/deep/beta");
        mk("node_modules/skipped");
        mk("ko/.hidden/skipped");
        mk("too/deep/for/depth/gamma");
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let found = scan_repos(&roots, 3);
        let canon = |p: &std::path::Path| {
            std::fs::canonicalize(p)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        assert!(found.repos.contains(&canon(&a)));
        assert!(found.repos.contains(&canon(&b)));
        assert!(found.repos.contains(&canon(dir.path())));
        assert_eq!(found.repos.len(), 3, "{found:?}");
        assert!(found.skipped.is_empty(), "{found:?}");
    }

    #[test]
    fn scan_skips_protected_dirs_unless_named() {
        use crate::privacy::{Guard, Why};
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for rel in ["Documents/code/alpha", "src/beta"] {
            init_repo(&root.join(rel));
        }
        let same = |_: &Path| Some(1);
        let found = walk_with(&root, Guard::with(Some(&root), false), &same);
        assert_eq!(found.repos, vec![root.join("src/beta").to_string_lossy()]);
        assert_eq!(
            found.skipped,
            vec![Skipped {
                path: root.join("Documents").to_string_lossy().into_owned(),
                why: Why::MacPrivacy
            }]
        );
        // Named as a root itself, it is scanned.
        let docs = root.join("Documents");
        let found = walk_with(&docs, Guard::with(Some(&root), false), &same);
        assert_eq!(
            found.repos,
            vec![root.join("Documents/code/alpha").to_string_lossy()]
        );
        assert!(found.skipped.is_empty());
    }

    #[test]
    fn scan_stays_on_the_roots_filesystem() {
        use crate::privacy::{Guard, Why};
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        init_repo(&root.join("local/a"));
        init_repo(&root.join("mnt/b"));
        init_repo(&root.join("mnt/deep/c"));
        let mnt = root.join("mnt");
        let dev = |p: &Path| Some(if p.starts_with(&mnt) { 2 } else { 1 });
        let found = walk_with(&root, Guard::with(None, false), &dev);
        assert_eq!(found.repos, vec![root.join("local/a").to_string_lossy()]);
        assert_eq!(
            found.skipped,
            vec![Skipped {
                path: mnt.to_string_lossy().into_owned(),
                why: Why::OtherFilesystem
            }]
        );
        // The mount named as a root: walked on its own device, two levels
        // deep too.
        let found = scan_in(
            &[root.clone(), mnt.clone()],
            3,
            Guard::with(None, false),
            &dev,
        );
        assert_eq!(found.repos.len(), 3, "{found:?}");
        assert!(found.skipped.is_empty(), "{found:?}");
    }

    /// Metadata that can't be read (or a folder gone since it was
    /// listed): skipped without a report.
    #[test]
    fn unreadable_folders_are_skipped_silently() {
        use crate::privacy::Guard;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        init_repo(&root.join("gone/a"));
        init_repo(&root.join("here/b"));
        let gone = root.join("gone");
        let dev = |p: &Path| (!p.starts_with(&gone)).then_some(1);
        let found = walk_with(&root, Guard::with(None, false), &dev);
        assert_eq!(found.repos, vec![root.join("here/b").to_string_lossy()]);
        assert!(found.skipped.is_empty(), "{found:?}");
    }

    /// `repo_roots = ["~"]` with `code_dir` in `~/Documents`: the code
    /// folder is a root, so it is scanned; the rest of `~/Documents` is
    /// not (and is reported).
    #[test]
    fn a_named_code_folder_under_a_protected_one_is_scanned() {
        use crate::privacy::Guard;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        init_repo(&root.join("Documents/code/alpha"));
        init_repo(&root.join("src/beta"));
        let code = root.join("Documents/code");
        let same = |_: &Path| Some(1);
        // (The daemon adds the code folder to the roots.)
        init_repo(&root.join("Documents/other/gamma"));
        let found = scan_in(
            &[root.clone(), code],
            3,
            Guard::with(Some(&root), true),
            &same,
        );
        let want: Vec<String> = ["Documents/code/alpha", "src/beta"]
            .iter()
            .map(|r| root.join(r).to_string_lossy().into_owned())
            .collect();
        assert_eq!(found.repos, want);
        // (`~/Documents` holds the root: not reported.)
        assert!(found.skipped.is_empty(), "{found:?}");
    }

    /// A code folder on another device below a root: the mount stays
    /// skipped, and the code folder is scanned as its own root, once.
    #[test]
    fn a_named_code_folder_on_another_device_is_scanned() {
        use crate::privacy::Guard;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        init_repo(&root.join("ext/code/alpha"));
        init_repo(&root.join("ext/other/beta"));
        let ext = root.join("ext");
        let code = ext.join("code");
        let dev = |p: &Path| Some(if p.starts_with(&ext) { 7 } else { 1 });
        let found = scan_in(
            &[root.clone(), code.clone()],
            3,
            Guard::with(None, false),
            &dev,
        );
        assert_eq!(
            found.repos,
            vec![root.join("ext/code/alpha").to_string_lossy()]
        );
        // (The mount holds the root: not reported.)
        assert!(found.skipped.is_empty(), "{found:?}");
        // A root nested in another (same device) is walked once.
        let same = |_: &Path| Some(1);
        let found = scan_in(
            &[root.clone(), code.clone()],
            3,
            Guard::with(None, false),
            &same,
        );
        assert_eq!(found.repos.len(), 2, "{found:?}");
    }

    #[test]
    fn scan_never_follows_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        init_repo(&elsewhere.path().join("r"));
        std::os::unix::fs::symlink(elsewhere.path(), root.join("link")).unwrap();
        let found = scan_repos(&[root.to_string_lossy().into_owned()], 3);
        assert!(found.repos.is_empty(), "{found:?}");
    }

    #[test]
    fn skipped_is_capped_and_counted() {
        use crate::privacy::Why;
        let skipped = (0..60)
            .map(|i| Skipped {
                path: format!("/x/{i:02}"),
                why: Why::OtherFilesystem,
            })
            .collect();
        let out = finish(
            Scan {
                skipped,
                ..Scan::default()
            },
            false,
        );
        assert_eq!(out.skipped.len(), MAX_SKIPPED);
        assert_eq!(out.skipped_more, 10);
        assert_eq!(out.skipped[0].path, "/x/00");
    }

    /// One folder in two cases is one repo where paths ignore case.
    #[test]
    fn results_dedupe_by_the_case_rule() {
        let scan = || Scan {
            repos: vec!["/a/Code/x".into(), "/a/code/x".into()],
            ..Scan::default()
        };
        assert_eq!(finish(scan(), true).repos.len(), 1);
        assert_eq!(finish(scan(), false).repos.len(), 2);
    }

    #[test]
    fn worktree_lifecycle_in_temp_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let r = repo.to_str().unwrap();
        git(&["-C", r, "init", "-q", "-b", "main"]).unwrap();
        git(&[
            "-C",
            r,
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ])
        .unwrap();
        assert_eq!(
            repo_root(r).unwrap(),
            std::fs::canonicalize(&repo).unwrap().to_string_lossy()
        );

        let ws = workspace_path(r, ".worktrees", "feature/one");
        assert!(ws.ends_with(".worktrees/feature-one"));
        worktree_add(r, "feature/one", &ws, None).unwrap();
        assert!(ws.join(".git").exists());
        // A worktree resolves back to the main repo root.
        assert_eq!(
            main_repo_root(ws.to_str().unwrap()).unwrap(),
            std::fs::canonicalize(&repo).unwrap().to_string_lossy()
        );
        ensure_excluded(r, ".worktrees").unwrap();
        // Idempotent, and at once from many threads still one line.
        ensure_excluded(r, ".worktrees").unwrap();
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| ensure_excluded(r, ".rlord-many").unwrap());
            }
        });
        let ex = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(ex.matches("/.worktrees/").count(), 1);
        assert_eq!(ex.matches("/.rlord-many/").count(), 1);
        assert!(git(&["-C", r, "status", "--porcelain"]).unwrap().is_empty());
        // Existing dir is refused, existing branch is checked out.
        assert!(worktree_add(r, "feature/one", &ws, None).is_err());
        let ws2 = workspace_path(r, ".worktrees", "feature/two");
        git(&["-C", r, "branch", "feature/two"]).unwrap();
        worktree_add(r, "feature/two", &ws2, None).unwrap();
    }
}
