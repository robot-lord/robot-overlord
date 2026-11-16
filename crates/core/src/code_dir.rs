//! Suggest the user's code folder (`[session] code_dir`) from the folders
//! rlord has seen projects in: the most common parent. Plain string work,
//! no disk access (the caller checks the few folders it will show).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

/// A folder some agent or rlord has seen work in, and who saw it:
/// `claude`, `codex`, `opencode`, `rlord` or `launch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub path: String,
    pub source: &'static str,
}

/// A possible code folder, and how many of the projects it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub projects: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Suggestion {
    /// None: no signal.
    pub best: Option<Candidate>,
    /// At most 4, best first; none is an ancestor or descendant of another
    /// suggested folder.
    pub alternatives: Vec<Candidate>,
    /// Distinct projects counted.
    pub total: usize,
    /// Distinct projects each source saw.
    pub by_source: BTreeMap<&'static str, usize>,
    /// The distinct projects counted, sorted.
    pub projects: Vec<String>,
}

/// The label a source is shown with.
pub fn source_label(source: &str) -> &str {
    match source {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "rlord" => "rlord",
        "launch" => "this folder",
        s => s,
    }
}

/// What a suggestion rests on, in words: "14 of 17 projects rlord has
/// seen (Claude Code 9, Codex 3, rlord 6)". `sources`: (source, count) in
/// any order; sources with none are left out.
pub fn basis(projects: usize, of: usize, sources: &[(String, usize)]) -> String {
    let mut parts: Vec<String> = sources
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(s, n)| format!("{} {n}", source_label(s)))
        .collect();
    parts.sort();
    let noun = if of == 1 { "project" } else { "projects" };
    let mut out = format!("{projects} of {of} {noun} rlord has seen");
    if !parts.is_empty() {
        out.push_str(&format!(" ({})", parts.join(", ")));
    }
    out
}

/// The project a seen folder belongs to: cut at the first hidden component
/// (`/a/ko/app/.worktrees/x` is `/a/ko/app`). None for a relative path.
fn project_root(p: &str) -> Option<PathBuf> {
    let p = Path::new(p.trim());
    if !p.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) if n.to_string_lossy().starts_with('.') => break,
            Component::CurDir | Component::ParentDir => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

/// Folders never suggested: `/`, `$HOME` and its ancestors, and the usual
/// homes' parents.
fn never(d: &Path, home: &Path) -> bool {
    d.parent().is_none()
        || home.starts_with(d)
        || d == Path::new("/Users")
        || d == Path::new("/home")
}

/// The most common parent of the projects in `seen` (counting each project
/// once, at most `max_depth` levels below the folder), and up to four
/// alternatives. Ties go to the deeper folder (an ancestor that covers no
/// more adds nothing), then the first by name.
pub fn suggest(seen: &[Seen], home: &Path, excluded: &[PathBuf], max_depth: u8) -> Suggestion {
    let mut projects: BTreeMap<PathBuf, BTreeSet<&'static str>> = BTreeMap::new();
    for s in seen {
        let Some(p) = project_root(&s.path) else {
            continue;
        };
        // `$HOME` itself, a folder directly in `/`, rlord's data and temp
        // dirs: never a project.
        if p == home
            || p.parent().is_none_or(|d| d.parent().is_none())
            || excluded.iter().any(|e| p.starts_with(e))
        {
            continue;
        }
        projects.entry(p).or_default().insert(s.source);
    }
    let mut by_source: BTreeMap<&'static str, usize> = BTreeMap::new();
    for sources in projects.values() {
        for s in sources {
            *by_source.entry(s).or_default() += 1;
        }
    }
    // Every proper ancestor of a project, scored.
    let mut score: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for p in projects.keys() {
        let depth = p.components().count();
        for d in p.ancestors().skip(1) {
            if never(d, home) {
                continue;
            }
            if depth - d.components().count() <= max_depth as usize {
                *score.entry(d.to_path_buf()).or_default() += 1;
            }
        }
    }
    let mut ranked: Vec<(PathBuf, usize)> = score.into_iter().collect();
    ranked.sort_by(|(a, sa), (b, sb)| {
        sb.cmp(sa)
            .then(b.components().count().cmp(&a.components().count()))
            .then(a.cmp(b))
    });
    let mut picked: Vec<(PathBuf, usize)> = Vec::new();
    for (d, n) in ranked {
        if picked.len() == 5 {
            break;
        }
        if picked
            .iter()
            .any(|(p, _)| p.starts_with(&d) || d.starts_with(p))
        {
            continue;
        }
        picked.push((d, n));
    }
    let mut picked = picked.into_iter().map(|(d, n)| Candidate {
        path: d.to_string_lossy().into_owned(),
        projects: n,
    });
    Suggestion {
        best: picked.next(),
        alternatives: picked.collect(),
        total: projects.len(),
        by_source,
        projects: projects
            .keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(paths: &[&str]) -> Vec<Seen> {
        paths
            .iter()
            .map(|p| Seen {
                path: p.to_string(),
                source: "claude",
            })
            .collect()
    }

    fn best(s: &Suggestion) -> Option<&str> {
        s.best.as_ref().map(|c| c.path.as_str())
    }

    #[test]
    fn the_most_common_parent_wins() {
        let home = Path::new("/h");
        let s = suggest(
            &seen(&[
                "/h/ko/a",
                "/h/ko/b",
                "/h/ko/org/c",
                "/h/ko/org/d",
                "/h/work/e",
            ]),
            home,
            &[],
            3,
        );
        assert_eq!(
            s.best,
            Some(Candidate {
                path: "/h/ko".into(),
                projects: 4
            })
        );
        assert_eq!(
            s.alternatives,
            vec![Candidate {
                path: "/h/work".into(),
                projects: 1
            }]
        );
        assert_eq!(s.total, 5);
        assert_eq!(s.by_source["claude"], 5);
        assert_eq!(
            basis(4, 5, &[("claude".into(), 5), ("codex".into(), 0)]),
            "4 of 5 projects rlord has seen (Claude Code 5)"
        );
    }

    #[test]
    fn worktrees_and_duplicates_count_once() {
        let home = Path::new("/h");
        let mut v = seen(&[
            "/h/ko/app/.worktrees/x",
            "/h/ko/app/.claude/worktrees/y",
            "/h/ko/app",
        ]);
        v.push(Seen {
            path: "/h/ko/app".into(),
            source: "codex",
        });
        let s = suggest(&v, home, &[], 3);
        assert_eq!(s.total, 1);
        assert_eq!(best(&s), Some("/h/ko"));
        assert_eq!((s.by_source["claude"], s.by_source["codex"]), (1, 1));
    }

    #[test]
    fn home_and_its_direct_projects_are_never_suggested() {
        let home = Path::new("/h");
        // A project directly in $HOME adds nothing; $HOME itself is no project.
        let s = suggest(&seen(&["/h/dotfiles", "/h", "/h/.config"]), home, &[], 3);
        assert_eq!(s.best, None);
        // Nor a folder directly in `/`; relative paths are dropped.
        let s = suggest(&seen(&["/srv", "rel/x"]), home, &[], 3);
        assert_eq!(s, Suggestion::default());
        assert_eq!(suggest(&[], home, &[], 3).best, None);
    }

    #[test]
    fn excluded_folders_are_dropped() {
        let home = Path::new("/h");
        let s = suggest(
            &seen(&[
                "/tmp/x/a",
                "/h/.local/share/robot-overlord/overlord",
                "/h/src/a",
            ]),
            home,
            &[PathBuf::from("/tmp")],
            3,
        );
        assert_eq!(best(&s), Some("/h/src"));
        assert_eq!(s.total, 1);
    }

    #[test]
    fn ties_go_to_the_deeper_folder() {
        let home = Path::new("/h");
        let s = suggest(&seen(&["/h/ko/org/a", "/h/ko/org/b"]), home, &[], 3);
        assert_eq!(best(&s), Some("/h/ko/org"));
        // Alternatives are neither ancestors nor descendants of the best.
        assert!(s.alternatives.is_empty());
        // Too deep below a folder: not counted for it.
        let s = suggest(&seen(&["/h/a/b/c/d/e"]), home, &[], 1);
        assert_eq!(best(&s), Some("/h/a/b/c/d"));
    }
}
