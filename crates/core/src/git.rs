//! Git worktree workspaces: one worktree per session under
//! `<repo>/<workspace_dir>/<sanitized-branch>` (other-tool's layout).

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
    let common = if Path::new(&common).is_absolute() {
        PathBuf::from(common)
    } else {
        PathBuf::from(repo_root(path)?).join(common)
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

/// Directory name for a branch: slashes flattened (other-tool convention).
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

/// Git repositories under `roots`, up to `depth` levels down. A repo is a
/// directory containing `.git`. We keep descending below a repo: a dotfiles
/// repo in `$HOME` must not hide everything under it, and vendored/nested
/// repos are real repos too (worktrees live under hidden dirs and are
/// skipped). Sorted, deduplicated.
pub fn find_repos(roots: &[String], depth: u8) -> Vec<String> {
    let mut found = Vec::new();
    for root in roots {
        let root = crate::config::expand_tilde(root);
        let Ok(root) = std::fs::canonicalize(&root) else {
            continue;
        };
        walk(&root, depth, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

fn walk(dir: &Path, depth: u8, out: &mut Vec<String>) {
    if dir.join(".git").exists() {
        out.push(dir.to_string_lossy().into_owned());
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
        if ft.is_dir() {
            walk(&entry.path(), depth - 1, out);
        }
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

    #[test]
    fn find_repos_scans_roots() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |rel: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(&p).unwrap();
            git(&["-C", p.to_str().unwrap(), "init", "-q"]).unwrap();
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
        let found = find_repos(&roots, 3);
        let canon = |p: &std::path::Path| {
            std::fs::canonicalize(p)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        assert!(found.contains(&canon(&a)));
        assert!(found.contains(&canon(&b)));
        assert!(found.contains(&canon(dir.path())));
        assert_eq!(found.len(), 3, "{found:?}");
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
