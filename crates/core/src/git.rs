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

/// Create the worktree: a new branch off the repo's HEAD, or a checkout of
/// `branch` if it already exists. Errors if git refuses (e.g. the branch is
/// checked out elsewhere).
pub fn worktree_add(repo: &str, branch: &str, dir: &Path) -> Result<()> {
    if dir.exists() {
        bail!("workspace directory already exists: {}", dir.display());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dir_s = dir.to_string_lossy();
    if branch_exists(repo, branch) {
        git(&["-C", repo, "worktree", "add", &dir_s, branch])?;
    } else {
        git(&["-C", repo, "worktree", "add", "-b", branch, &dir_s])?;
    }
    Ok(())
}

/// Whether `repo` has a local branch named `branch`.
pub fn branch_exists(repo: &str, branch: &str) -> bool {
    git(&[
        "-C",
        repo,
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch}"),
    ])
    .is_ok()
}

/// Whether some checkout of `repo` (the main one or a worktree) has
/// `branch` checked out; `git worktree add` refuses a second one.
pub fn branch_checked_out(repo: &str, branch: &str) -> bool {
    let want = format!("branch refs/heads/{branch}");
    git(&["-C", repo, "worktree", "list", "--porcelain"])
        .map(|out| out.lines().any(|l| l == want))
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
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    if let Some(p) = exclude.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut content = current;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&line);
    content.push('\n');
    std::fs::write(&exclude, content)?;
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
        worktree_add(r, "feature/one", &ws).unwrap();
        assert!(ws.join(".git").exists());
        // A worktree resolves back to the main repo root.
        assert_eq!(
            main_repo_root(ws.to_str().unwrap()).unwrap(),
            std::fs::canonicalize(&repo).unwrap().to_string_lossy()
        );
        ensure_excluded(r, ".worktrees").unwrap();
        ensure_excluded(r, ".worktrees").unwrap(); // idempotent
        let ex = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(ex.matches("/.worktrees/").count(), 1);
        assert!(git(&["-C", r, "status", "--porcelain"]).unwrap().is_empty());
        // Existing dir is refused, existing branch is checked out.
        assert!(worktree_add(r, "feature/one", &ws).is_err());
        let ws2 = workspace_path(r, ".worktrees", "feature/two");
        git(&["-C", r, "branch", "feature/two"]).unwrap();
        worktree_add(r, "feature/two", &ws2).unwrap();
    }
}
