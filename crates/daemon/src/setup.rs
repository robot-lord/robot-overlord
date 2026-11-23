//! Setup: the code folder (`[session] code_dir`), where the user keeps
//! repos and where new ones go. The repo finder scans it.
//!
//! rlord suggests one by plain code (`code_dir::suggest`) from folders it
//! can already read without walking the user's disk: Claude Code's
//! transcripts, Codex's thread index, OpenCode's sessions, rlord's own
//! sessions and repos, and the folder the user launched from. The user
//! confirms it on a form (TUI, `rlord setup`); rlord saves it
//! (`setup.apply`). The overlord only proposes (`confirm_setup`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use rlord_core::code_dir::{self, Seen, Suggestion};
use rlord_core::config::{absolute_dir, contract_tilde, expand_tilde, Config, RootsEdit};
use rlord_core::db::Db;
use rlord_core::privacy::Guard;
use rlord_core::types::now_ms;

use crate::Daemon;

/// kv row: setup has been answered (`done:<ms>` or `skipped:<ms>`). Not a
/// config key: declining must not write the config.
pub(crate) const KV: &str = "setup_code_dir";

/// How many of each source's folders are read.
const CLAUDE_DIRS: usize = 300;
const AGENT_ROWS: usize = 2000;

/// Where setup stands, from the one kv row (read once, under one short
/// lock) and the config.
struct Answered {
    /// The first-run form should open: no code folder set, and setup not
    /// answered yet.
    needed: bool,
    /// A code folder was saved through setup before (`done:`).
    done: bool,
    /// The user said "don't ask again" (`skipped:`).
    skipped: bool,
}

fn answered(d: &Daemon, cfg: &Config) -> Answered {
    let row = d.db().kv_get(KV).ok().flatten();
    answered_from(row.as_deref(), cfg)
}

fn answered_from(row: Option<&str>, cfg: &Config) -> Answered {
    Answered {
        needed: cfg.session.code_dir_path().is_none() && row.is_none(),
        done: row.is_some_and(|a| a.starts_with("done")),
        skipped: row.is_some_and(|a| a.starts_with("skipped")),
    }
}

/// The first-run form should open (for callers already holding the DB).
pub(crate) fn needed(db: &Db, cfg: &Config) -> bool {
    answered_from(db.kv_get(KV).ok().flatten().as_deref(), cfg).needed
}

/// `setup.status` (cheap: config and one kv row): `needed`, the code
/// folder (expanded, "" when unset), whether the user skipped setup or
/// saved a folder before (`done`), `repo_roots` as configured, and
/// `roots_default`: what saving rlord's own suggestion does with them
/// (`default_roots`). Every front end uses that as given.
pub(crate) fn status(d: &Daemon) -> Result<Value> {
    let cfg = d.config();
    let a = answered(d, &cfg);
    Ok(json!({
        "needed": a.needed,
        "code_dir": cfg.session.code_dir_path().unwrap_or_default(),
        "skipped": a.skipped,
        "done": a.done,
        "repo_roots": cfg.session.repo_roots,
        "roots_default": default_roots(&cfg, a.done),
        // Not chosen yet (null): the form asks.
        "auto_resume": cfg.limits.auto_resume,
    }))
}

/// What saving rlord's own suggestion does with `repo_roots`: the old
/// template's `["~"]` is dropped ("follow": only the code folder), but only
/// until a folder was saved once (`done`); anything else is left. An
/// overlord proposal that names no roots always leaves them.
pub(crate) fn default_roots(cfg: &Config, done: bool) -> &'static str {
    let roots: Vec<&str> = cfg
        .session
        .repo_roots
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .collect();
    if !done && roots == ["~"] {
        "follow"
    } else {
        "leave"
    }
}

/// Each folder as given and resolved (`/home` may be a symlink to
/// `/var/home`; macOS's temp dir `/var/folders/…` is
/// `/private/var/folders/…`), since recorded paths come either way.
fn with_resolved(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for p in paths {
        if let Ok(c) = std::fs::canonicalize(&p) {
            if c != p && !v.contains(&c) {
                v.push(c);
            }
        }
        if !v.contains(&p) {
            v.push(p);
        }
    }
    v
}

/// Every folder rlord has seen work in. rlord's own rows under a short DB
/// lock; the agents' files after it is released. Bounded; none walks the
/// user's folders.
fn collect(d: &Daemon, launch_dir: Option<&str>) -> Vec<Seen> {
    let mut seen = Vec::new();
    let mut add = |paths: Vec<String>, source: &'static str| {
        seen.extend(paths.into_iter().map(|path| Seen { path, source }));
    };
    {
        let db = d.db();
        let repos = db.list_repos().unwrap_or_default();
        add(repos.into_iter().map(|r| r.path).collect(), "rlord");
        add(db.session_folders().unwrap_or_default(), "rlord");
    }
    if let Some(r) = d.config().session.default_repo_path() {
        add(vec![r], "rlord");
    }
    add(
        rlord_core::import::claude_project_folders(CLAUDE_DIRS),
        "claude",
    );
    use rlord_core::agents::{codex, opencode};
    add(codex::thread_folders(&codex::home(), AGENT_ROWS), "codex");
    add(
        opencode::directories(&opencode::db_path(), AGENT_ROWS),
        "opencode",
    );
    if let Some(l) = launch_dir.filter(|l| !l.trim().is_empty()) {
        add(vec![l.to_string()], "launch");
    }
    seen
}

/// `setup.suggest` `{"launch_dir"?}`: rlord's suggested code folder, what
/// it rests on, alternatives, and what the finder scans.
pub(crate) fn suggest(d: &Daemon, params: &Value) -> Result<Value> {
    let seen = collect(d, params["launch_dir"].as_str());
    let cfg = d.config();
    let Answered { needed, done, .. } = answered(d, &cfg);
    let guard = Guard::new();
    // (`$HOME` and above are never offered; rlord's data and temp dirs hold
    // no projects.)
    let homes = rlord_core::privacy::home_paths();
    let is_repo = |p: &Path| is_home_repo(p, &homes, &guard);
    let s = code_dir::suggest_except(
        &seen,
        &homes,
        &with_resolved(rlord_core::import::excluded_dirs()),
        cfg.session.repo_scan_depth,
        &is_repo,
    );
    let look = Look {
        guard: &guard,
        homes: &homes,
        is_dir: &|p| p.is_dir(),
        is_repo: &is_repo,
    };
    let mut v = suggestion_value_with(&s, &cfg, needed, &look);
    v["done"] = json!(done);
    v["roots_default"] = json!(default_roots(&cfg, done));
    Ok(v)
}

/// A git repo under `$HOME` (given or resolved) that no macOS prompt
/// guards: no code folder. Nothing outside `$HOME` is looked at, so no
/// stat reaches `/net`, `/mnt` and the like.
fn is_home_repo(p: &Path, homes: &[PathBuf], guard: &Guard) -> bool {
    homes.iter().any(|h| p.starts_with(h) && p != h)
        && guard.prompts(p).is_none()
        && p.join(".git").exists()
}

/// What `suggestion_value_with` may look at.
struct Look<'a> {
    guard: &'a Guard,
    homes: &'a [PathBuf],
    /// Asked only of the folders shown, and only of those no prompt guards.
    is_dir: &'a dyn Fn(&Path) -> bool,
    /// Asked of an explicit root before it is shown.
    is_repo: &'a dyn Fn(&Path) -> bool,
}

/// `suggestion_value_with` with no homes and no repos (tests).
#[cfg(test)]
fn suggestion_value(
    s: &Suggestion,
    cfg: &Config,
    needed: bool,
    guard: &Guard,
    is_dir: &dyn Fn(&Path) -> bool,
) -> Value {
    // (The tests' folders are under `/h`.)
    let homes = [PathBuf::from("/h")];
    let look = Look {
        guard,
        homes: &homes,
        is_dir,
        is_repo: &|_| false,
    };
    suggestion_value_with(s, cfg, needed, &look)
}

/// The `setup.suggest` answer for a suggestion.
fn suggestion_value_with(s: &Suggestion, cfg: &Config, needed: bool, look: &Look) -> Value {
    let (guard, is_dir) = (look.guard, look.is_dir);
    // Only folders under `$HOME` that no prompt guards are looked at (as
    // for the repo check); any other is shown as not checked.
    let checkable =
        |p: &Path| look.homes.iter().any(|h| guard.is_within(p, h)) && guard.prompts(p).is_none();
    let under = |dir: &str| {
        s.projects
            .iter()
            .filter(|p| Path::new(p).starts_with(dir))
            .count()
    };
    // A single explicit root that isn't `~` outranks the vote, if it is
    // there (a deleted one mustn't push out real candidates); one macOS
    // guards can't be checked, and is kept and flagged.
    let roots = cfg.session.repo_roots.clone();
    // (Normalized as saved folders are; never `$HOME` or above, in any
    // spelling, nor a system or mount parent, nor a repo.)
    let explicit = match roots.as_slice() {
        [r] if r.trim() != "~" => absolute_dir(r).ok(),
        _ => None,
    }
    .filter(|r| code_dir::suggestible(Path::new(r), look.homes))
    .filter(|r| !(look.is_repo)(Path::new(r)))
    .filter(|r| {
        let p = Path::new(r);
        !checkable(p) || is_dir(p)
    });
    let mut shown: Vec<(String, usize, &str)> = Vec::new();
    if let Some(r) = &explicit {
        shown.push((r.clone(), under(r), "repo_roots"));
    }
    for c in s.best.iter().chain(&s.alternatives) {
        let p = Path::new(&c.path);
        if shown
            .iter()
            .any(|(x, _, _)| p.starts_with(x) || Path::new(x).starts_with(p))
        {
            continue;
        }
        shown.push((c.path.clone(), c.projects, "vote"));
    }
    // A guarded folder is kept (and flagged); one that's gone is dropped.
    let shown: Vec<Value> = shown
        .into_iter()
        .filter_map(|(path, projects, from)| {
            let p = Path::new(&path);
            let prompts = guard.prompts(p).is_some();
            let checked = checkable(p);
            // (The explicit root was checked above: stat once.)
            if checked && from != "repo_roots" && !is_dir(p) {
                return None;
            }
            Some(json!({"path": path, "projects": projects, "of": s.total,
                        "from": from, "prompts": prompts, "checked": checked}))
        })
        .take(5)
        .collect();
    let mut shown = shown.into_iter();
    let effective: Vec<String> = cfg
        .session
        .finder_roots(needed)
        .iter()
        .map(|r| expand_tilde(r))
        .collect();
    json!({
        "code_dir": cfg.session.code_dir_path().unwrap_or_default(),
        "needed": needed,
        "suggestion": shown.next(),
        "alternatives": shown.collect::<Vec<_>>(),
        "sources": s.by_source,
        "roots": {"configured": roots, "effective": effective},
    })
}

/// `ui.confirm_setup` (the overlord's `confirm_setup`): open the
/// code-folder form on the user's screen with what the overlord would
/// save. The user's confirm there saves it (`setup.apply`). Cheap: the
/// form loads rlord's own suggestion (for ←/→) in the background.
pub(crate) fn confirm(d: &Daemon, params: &Value) -> Result<Value> {
    let st = status(d)?;
    let r = queue_confirm(d, params, st)?;
    crate::ui::show_list(d)?;
    Ok(r)
}

/// `confirm` up to showing the list: check the proposal and push the
/// form's UI command, with `status` (`setup.status`) for what is
/// configured now.
fn queue_confirm(d: &Daemon, params: &Value, status: Value) -> Result<Value> {
    let mut prefill = serde_json::Map::new();
    if let Some(c) = params["code_dir"].as_str().filter(|c| !c.trim().is_empty()) {
        prefill.insert("code_dir".into(), json!(absolute_dir(c)?));
    }
    // Omitted: leave the configured roots. `[]`: only the code folder.
    if let Some(a) = params["repo_roots"].as_array() {
        let roots = a
            .iter()
            .filter_map(Value::as_str)
            .filter(|r| !r.trim().is_empty())
            .map(absolute_dir)
            .collect::<Result<Vec<_>>>()?;
        prefill.insert("repo_roots".into(), json!(roots));
    }
    for k in ["description", "note"] {
        if let Some(v) = params[k].as_str().map(str::trim).filter(|v| !v.is_empty()) {
            prefill.insert(k.into(), json!(v));
        }
    }
    if let Some(c) = params["changed"].as_array() {
        prefill.insert("changed".into(), json!(c));
    }
    let prefill = Value::Object(prefill);
    d.ui_cmd(json!({"cmd": "setup", "prefill": prefill, "status": status}));
    Ok(json!({"shown": prefill,
              "note": "the form is open on the user's screen; rlord saves it when they confirm"}))
}

/// `setup.apply`: save the confirmed code folder, or `{"skip": true}`.
pub(crate) fn apply(d: &Daemon, params: &Value) -> Result<Value> {
    let path = rlord_core::paths::config_path();
    let r = apply_at(d, params, &path)?;
    if r.get("skipped").is_none() {
        d.reload_config()?;
    }
    d.changed();
    Ok(r)
}

/// `apply` writing the config at `path`; the caller reloads it. Returns
/// what was saved: the folder, the roots the finder now scans, whether the
/// folder was checked (false: macOS guards it, so it wasn't touched) and
/// created, and the config file.
fn apply_at(d: &Daemon, params: &Value, path: &Path) -> Result<Value> {
    apply_with(
        d,
        params,
        path,
        &Guard::new(),
        &rlord_core::privacy::home_paths(),
    )
}

/// `apply_at` with a given guard and homes (tests).
fn apply_with(
    d: &Daemon,
    params: &Value,
    path: &Path,
    guard: &Guard,
    homes: &[PathBuf],
) -> Result<Value> {
    if params["skip"] == true {
        d.db().kv_set(KV, &format!("skipped:{}", now_ms()))?;
        return Ok(json!({"skipped": true}));
    }
    let raw = params["code_dir"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("code_dir: the folder to save (or skip: true)")?;
    let dir = absolute_dir(raw)?;
    let roots = match &params["roots"] {
        Value::Null => RootsEdit::Leave,
        Value::String(s) if s == "leave" => RootsEdit::Leave,
        Value::String(s) if s == "follow" => RootsEdit::Follow,
        // `[]` is the same as follow: only the code folder.
        Value::Array(a) => {
            let list = a
                .iter()
                .filter_map(Value::as_str)
                .filter(|r| !r.trim().is_empty())
                .map(|r| absolute_dir(r).map(|r| contract_tilde(&r)))
                .collect::<Result<Vec<_>>>()?;
            if list.is_empty() {
                RootsEdit::Follow
            } else {
                RootsEdit::Set(list)
            }
        }
        other => bail!("roots: \"leave\", \"follow\" or a list of folders, not {other}"),
    };
    // A folder macOS guards isn't touched, not even to check it exists
    // (that would raise the prompt): it is saved as given, unchecked. Any
    // other must exist, or be created when asked.
    let checked = guard.prompts(Path::new(&dir)).is_none();
    // The same rules as the suggestion: not a system or mount parent, not
    // a repo (where it can be checked); `$HOME` only when the user
    // confirmed it (`allow_home`: the finder then scans all of it).
    let p = Path::new(&dir);
    let home = rlord_core::privacy::is_home_in(p, homes, guard.folds_case());
    if home {
        if params["allow_home"] != true {
            bail!(
                "{} is your whole home folder: the repo finder would scan all of it. \
                 Pick the folder your repos are in, or confirm home (allow_home)",
                contract_tilde(&dir)
            );
        }
    } else if !code_dir::suggestible(p, homes) {
        bail!("{dir} is a system or mount folder, not a code folder: pick the folder your repos are in");
    }
    // (A dotfiles repo at `$HOME` doesn't block a confirmed home.)
    if checked && !home && p.join(".git").exists() {
        bail!(
            "{} is a git repo: the code folder is the one that holds your repos (its parent?)",
            contract_tilde(&dir)
        );
    }
    let mut created = false;
    if checked && !Path::new(&dir).is_dir() {
        if params["create"] != true {
            bail!("{} doesn't exist (create it?)", contract_tilde(&dir));
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {dir}"))?;
        created = true;
    }
    let file = Config::set_code_dir_at(path, &contract_tilde(&dir), roots)?;
    // The form's one-time question, answered there: rlord's own config.
    if let Some(on) = params["auto_resume"].as_bool() {
        Config::set_auto_resume_at(&file, on)?;
    }
    d.db().kv_set(KV, &format!("done:{}", now_ms()))?;
    let cfg = Config::load_at(&file)?;
    let effective: Vec<String> = cfg
        .session
        .scan_roots()
        .iter()
        .map(|r| expand_tilde(r))
        .collect();
    Ok(json!({
        "code_dir": dir,
        "repo_roots": effective,
        "checked": checked,
        "created": created,
        "auto_resume": cfg.limits.auto_resume,
        "config": file.to_string_lossy(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlord_core::code_dir::Candidate;

    fn daemon() -> std::sync::Arc<Daemon> {
        Daemon::new(Db::open_in_memory().unwrap(), Config::default())
    }

    /// A symlinked folder is compared in both forms.
    #[test]
    fn folders_are_compared_resolved_too() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let v = with_resolved(vec![link.clone()]);
        assert!(v.contains(&link));
        assert!(v.contains(&std::fs::canonicalize(&real).unwrap()));
        // A folder that doesn't resolve stays as given.
        assert_eq!(
            with_resolved(vec![PathBuf::from("/no/such")]),
            [PathBuf::from("/no/such")]
        );
    }

    #[test]
    fn the_auto_resume_answer_is_saved_with_the_folder() {
        let d = daemon();
        assert_eq!(status(&d).unwrap()["auto_resume"], Value::Null);
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("code");
        std::fs::create_dir(&code).unwrap();
        let cfg = dir.path().join("config.toml");
        let p = json!({"code_dir": code, "roots": "follow", "auto_resume": true});
        let r = apply_with(&d, &p, &cfg, &Guard::new(), &[]).unwrap();
        assert_eq!(r["auto_resume"], true);
        assert_eq!(
            Config::load_at(&cfg).unwrap().limits.auto_resume,
            Some(true)
        );
        // Not answered: left unset.
        let cfg2 = dir.path().join("config2.toml");
        let p = json!({"code_dir": code, "roots": "follow"});
        apply_with(&d, &p, &cfg2, &Guard::new(), &[]).unwrap();
        assert_eq!(Config::load_at(&cfg2).unwrap().limits.auto_resume, None);
    }

    #[test]
    fn status_is_needed_until_answered() {
        let d = daemon();
        assert_eq!(status(&d).unwrap()["needed"], true);
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let r = apply_at(&d, &json!({"skip": true}), &cfg).unwrap();
        assert_eq!(r["skipped"], true);
        let st = status(&d).unwrap();
        assert_eq!(
            (st["needed"].as_bool(), st["skipped"].as_bool()),
            (Some(false), Some(true))
        );
        assert!(!cfg.exists(), "skipping must not write the config");
        // A code folder set: not needed.
        let d = daemon();
        d.update_config(|c| c.session.code_dir = "/c".into());
        assert_eq!(status(&d).unwrap()["needed"], false);
    }

    fn sugg(best: &str, alts: &[&str], projects: &[&str]) -> Suggestion {
        Suggestion {
            best: Some(Candidate {
                path: best.into(),
                projects: 3,
            }),
            alternatives: alts
                .iter()
                .map(|a| Candidate {
                    path: a.to_string(),
                    projects: 1,
                })
                .collect(),
            total: projects.len(),
            by_source: [("claude", 4)].into_iter().collect(),
            projects: projects.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn suggestion_shape_and_explicit_root() {
        let s = sugg(
            "/h/ko",
            &["/h/work", "/h/gone"],
            &["/h/ko/a", "/h/ko/b", "/h/ko/c", "/h/work/d"],
        );
        let exists = |p: &Path| p != Path::new("/h/gone");
        let v = suggestion_value(
            &s,
            &Config::default(),
            true,
            &Guard::with(None, false),
            &exists,
        );
        assert_eq!(v["suggestion"]["path"], "/h/ko");
        assert_eq!(v["suggestion"]["from"], "vote");
        assert_eq!(
            (
                v["suggestion"]["projects"].as_u64(),
                v["suggestion"]["of"].as_u64()
            ),
            (Some(3), Some(4))
        );
        // A folder that's gone is dropped.
        assert_eq!(v["alternatives"].as_array().unwrap().len(), 1);
        assert_eq!(v["sources"]["claude"], 4);
        assert_eq!(v["roots"]["configured"], json!([]));
        assert_eq!(v["needed"], true);
        // A single explicit root outranks the vote.
        let mut cfg = Config::default();
        cfg.session.repo_roots = vec!["/h/work".into()];
        let v = suggestion_value(&s, &cfg, false, &Guard::with(None, false), &exists);
        assert_eq!(v["suggestion"]["path"], "/h/work");
        assert_eq!(v["suggestion"]["from"], "repo_roots");
        assert_eq!(v["suggestion"]["projects"], 1);
        assert_eq!(v["alternatives"][0]["path"], "/h/ko");
        assert_eq!(v["roots"]["effective"], json!(["/h/work"]));
        // `["~"]` doesn't.
        cfg.session.repo_roots = vec!["~".into()];
        let v = suggestion_value(&s, &cfg, false, &Guard::with(None, false), &exists);
        assert_eq!(v["suggestion"]["path"], "/h/ko");
        // No signal.
        let v = suggestion_value(
            &Suggestion::default(),
            &Config::default(),
            true,
            &Guard::default(),
            &exists,
        );
        assert!(v["suggestion"].is_null());
    }

    /// An explicit root that is `$HOME` (any spelling), a system parent or
    /// a repo doesn't go ahead of the vote.
    #[test]
    fn an_explicit_root_must_be_suggestible() {
        let s = sugg("/h/ko", &[], &["/h/ko/a"]);
        let homes = [PathBuf::from("/h"), PathBuf::from("/real/h")];
        let guard = Guard::with(None, false);
        let look = Look {
            guard: &guard,
            homes: &homes,
            is_dir: &|_| true,
            is_repo: &|p| p == Path::new("/h/ko/app"),
        };
        for r in ["/h/", "/real/h", "/h/ko/..", "/Volumes", "/h/ko/app"] {
            let mut cfg = Config::default();
            cfg.session.repo_roots = vec![r.into()];
            let v = suggestion_value_with(&s, &cfg, false, &look);
            assert_eq!(v["suggestion"]["from"], "vote", "{r}");
        }
        let mut cfg = Config::default();
        cfg.session.repo_roots = vec!["/h/work/".into()];
        let v = suggestion_value_with(&s, &cfg, false, &look);
        assert_eq!(v["suggestion"]["path"], "/h/work");
        // Only under `$HOME` is a repo looked for.
        let g = Guard::with(None, false);
        let homes = [PathBuf::from("/nonexistent-home")];
        assert!(!is_home_repo(Path::new("/mnt/x"), &homes, &g));
    }

    /// A deleted explicit root doesn't push out candidates.
    #[test]
    fn a_missing_explicit_root_is_ignored() {
        let s = sugg("/h/ko", &["/h/work"], &["/h/ko/a", "/h/work/b"]);
        let mut cfg = Config::default();
        cfg.session.repo_roots = vec!["/h".into()];
        let exists = |p: &Path| p != Path::new("/h");
        let v = suggestion_value(&s, &cfg, false, &Guard::with(None, false), &exists);
        assert_eq!(v["suggestion"]["path"], "/h/ko");
        assert_eq!(v["suggestion"]["from"], "vote");
        assert_eq!(v["alternatives"][0]["path"], "/h/work");
    }

    #[test]
    fn the_kv_row_says_where_setup_stands() {
        let cfg = Config::default();
        let a = answered_from(None, &cfg);
        assert!(a.needed && !a.done && !a.skipped);
        let a = answered_from(Some("done:1"), &cfg);
        assert!(!a.needed && a.done && !a.skipped);
        let a = answered_from(Some("skipped:1"), &cfg);
        assert!(!a.needed && !a.done && a.skipped);
        let mut cfg = Config::default();
        cfg.session.code_dir = "/c".into();
        assert!(!answered_from(None, &cfg).needed);
    }

    /// Candidates outside `$HOME` aren't looked at: shown, not checked.
    #[test]
    fn candidates_outside_home_are_not_stated() {
        let s = sugg("/srv/code", &[], &["/srv/code/a"]);
        let v = suggestion_value(
            &s,
            &Config::default(),
            true,
            &Guard::with(None, false),
            &|p| panic!("stat-ed {}", p.display()),
        );
        assert_eq!(v["suggestion"]["path"], "/srv/code");
        assert_eq!(v["suggestion"]["checked"], false);
    }

    /// A confirmed home with a dotfiles repo in it is taken.
    #[test]
    fn a_dotfiles_repo_doesnt_block_a_confirmed_home() {
        let d = daemon();
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".git")).unwrap();
        let cfg = root.join("config.toml");
        let homes = [home.clone()];
        let r = apply_with(
            &d,
            &json!({"code_dir": home.to_string_lossy(), "allow_home": true}),
            &cfg,
            &Guard::with(None, false),
            &homes,
        )
        .unwrap();
        assert_eq!(r["code_dir"], home.to_string_lossy().as_ref());
    }

    #[test]
    fn a_guarded_candidate_is_flagged_not_stated() {
        let s = sugg("/h/Documents/code", &[], &["/h/Documents/code/a"]);
        let guard = Guard::with(Some(Path::new("/h")), false);
        let v = suggestion_value(&s, &Config::default(), true, &guard, &|p| {
            panic!("stat-ed {}", p.display())
        });
        assert_eq!(v["suggestion"]["prompts"], true);
    }

    #[test]
    fn confirm_queues_the_form_with_the_proposal() {
        let d = daemon();
        let sugg = json!({"suggestion": null, "roots": {"configured": []}});
        let r = queue_confirm(
            &d,
            &json!({"code_dir": "/h/ko", "repo_roots": ["/h/ko", "/h/proj"],
                    "description": "both", "changed": ["repo_roots"], "note": "Both",
                    "bogus": 1}),
            sugg.clone(),
        )
        .unwrap();
        assert_eq!(r["shown"]["repo_roots"], json!(["/h/ko", "/h/proj"]));
        assert!(r["shown"].get("bogus").is_none());
        // No TUI up: parked for the next one.
        let (_, cmd) = d.pending_ui.lock().unwrap().clone().unwrap();
        assert_eq!(cmd["cmd"], "setup");
        assert_eq!(cmd["prefill"]["code_dir"], "/h/ko");
        assert_eq!(cmd["status"], sugg);
        assert!(
            cmd.get("suggest").is_none(),
            "no suggestion in the MCP call"
        );
        // Relative paths and `/` are refused, saying what to pass.
        let e = queue_confirm(&d, &json!({"code_dir": "ko"}), sugg.clone()).unwrap_err();
        assert!(e.to_string().contains("full path"), "{e}");
        assert!(queue_confirm(&d, &json!({"code_dir": "/"}), sugg.clone()).is_err());
        assert!(queue_confirm(&d, &json!({"repo_roots": ["x"]}), sugg.clone()).is_err());
        // `[]` is passed on (only the code folder); omitted stays omitted.
        let r = queue_confirm(
            &d,
            &json!({"code_dir": "/h/ko", "repo_roots": []}),
            sugg.clone(),
        )
        .unwrap();
        assert_eq!(r["shown"]["repo_roots"], json!([]));
        let r = queue_confirm(&d, &json!({"code_dir": "/h/ko/"}), sugg).unwrap();
        assert!(r["shown"].get("repo_roots").is_none());
        assert_eq!(r["shown"]["code_dir"], "/h/ko");
    }

    /// The old `["~"]` is dropped by default only before setup was done;
    /// status says so for every front end.
    #[test]
    fn default_roots_rule() {
        let mut cfg = Config::default();
        assert_eq!(default_roots(&cfg, false), "leave");
        cfg.session.repo_roots = vec!["~".into()];
        assert_eq!(default_roots(&cfg, false), "follow");
        assert_eq!(default_roots(&cfg, true), "leave");
        cfg.session.repo_roots = vec!["~/ko".into()];
        assert_eq!(default_roots(&cfg, false), "leave");
        let d = Daemon::new(Db::open_in_memory().unwrap(), {
            let mut c = Config::default();
            c.session.repo_roots = vec!["~".into()];
            c
        });
        assert_eq!(status(&d).unwrap()["roots_default"], "follow");
        d.db().kv_set(KV, "done:1").unwrap();
        let st = status(&d).unwrap();
        assert_eq!(
            (st["roots_default"].as_str(), st["done"].as_bool()),
            (Some("leave"), Some(true))
        );
    }

    /// A folder macOS guards is saved as given: not checked, not created.
    /// setup.apply refuses what the suggestion never offers: a system or
    /// mount parent, a repo; `$HOME` only when confirmed.
    #[test]
    fn apply_refuses_what_is_no_code_folder() {
        let d = daemon();
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let cfg = root.join("config.toml");
        let g = Guard::with(None, false);
        let home = root.join("home");
        std::fs::create_dir_all(home.join("app/.git")).unwrap();
        let homes = [home.clone()];
        let apply = |p: Value| apply_with(&d, &p, &cfg, &g, &homes);
        let e = apply(json!({"code_dir": "/Volumes"})).unwrap_err();
        assert!(e.to_string().contains("system or mount"), "{e}");
        let e = apply(json!({"code_dir": home.join("app").to_string_lossy()})).unwrap_err();
        assert!(e.to_string().contains("is a git repo"), "{e}");
        let e = apply(json!({"code_dir": home.to_string_lossy()})).unwrap_err();
        assert!(e.to_string().contains("whole home folder"), "{e}");
        assert!(!cfg.exists(), "nothing written");
        let r = apply(json!({"code_dir": home.to_string_lossy(), "allow_home": true})).unwrap();
        assert_eq!(r["code_dir"], home.to_string_lossy().as_ref());
    }

    #[test]
    fn a_guarded_folder_is_saved_unchecked() {
        let d = daemon();
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap();
        let cfg = home.join("config.toml");
        let code = home.join("Documents/code");
        let guard = Guard::with(Some(&home), false);
        let r = apply_with(
            &d,
            &json!({"code_dir": code.to_string_lossy(), "create": true}),
            &cfg,
            &guard,
            &[],
        )
        .unwrap();
        assert_eq!(
            (r["checked"].as_bool(), r["created"].as_bool()),
            (Some(false), Some(false))
        );
        assert!(!code.exists());
    }

    #[test]
    fn apply_saves_creates_and_refuses() {
        let d = daemon();
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let code = dir.path().join("code");
        let code_s = code.to_string_lossy().into_owned();
        // Missing, not asked to create.
        let e = apply_at(&d, &json!({"code_dir": code_s}), &cfg).unwrap_err();
        assert!(e.to_string().contains("doesn't exist"), "{e}");
        let r = apply_at(
            &d,
            &json!({"code_dir": code_s, "roots": "follow", "create": true}),
            &cfg,
        )
        .unwrap();
        assert_eq!(r["created"], true);
        assert!(code.is_dir());
        assert_eq!(r["repo_roots"], json!([code_s]));
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("code_dir"), "{text}");
        assert!(d.db().kv_get(KV).unwrap().unwrap().starts_with("done:"));
        // Explicit roots: scanned besides the code folder.
        let r = apply_at(
            &d,
            &json!({"code_dir": code_s, "roots": ["/a", "/b"]}),
            &cfg,
        )
        .unwrap();
        assert_eq!(
            (r["created"].as_bool(), r["repo_roots"].clone()),
            (Some(false), json!([code_s, "/a", "/b"]))
        );
        // `[]`: only the code folder (the key goes).
        let r = apply_at(&d, &json!({"code_dir": code_s, "roots": []}), &cfg).unwrap();
        assert_eq!(r["repo_roots"], json!([code_s]));
        assert!(!std::fs::read_to_string(&cfg)
            .unwrap()
            .contains("repo_roots"));
        // A relative root is refused, and nothing is written.
        let before = std::fs::read_to_string(&cfg).unwrap();
        assert!(apply_at(&d, &json!({"code_dir": code_s, "roots": ["rel"]}), &cfg).is_err());
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
        // `/` and relative paths are refused.
        assert!(apply_at(&d, &json!({"code_dir": "/"}), &cfg).is_err());
        assert!(apply_at(&d, &json!({"code_dir": "code"}), &cfg).is_err());
        assert!(apply_at(&d, &json!({}), &cfg).is_err());
    }
}
