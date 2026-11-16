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

/// The first-run form should open: no code folder set, and the user hasn't
/// answered setup yet.
pub(crate) fn needed(db: &Db, cfg: &Config) -> bool {
    cfg.session.code_dir_path().is_none() && db.kv_get(KV).ok().flatten().is_none()
}

/// `setup.status` (cheap: config and one kv row): `needed`, the code
/// folder (expanded, "" when unset), whether the user skipped setup or
/// saved a folder before (`done`), `repo_roots` as configured, and
/// `roots_default`: what saving rlord's own suggestion does with them
/// (`default_roots`). Every front end uses that as given.
pub(crate) fn status(d: &Daemon) -> Result<Value> {
    let cfg = d.config();
    let db = d.db();
    let done = done(&db);
    Ok(json!({
        "needed": needed(&db, &cfg),
        "code_dir": cfg.session.code_dir_path().unwrap_or_default(),
        "skipped": db.kv_get(KV)?.is_some_and(|a| a.starts_with("skipped")),
        "done": done,
        "repo_roots": cfg.session.repo_roots,
        "roots_default": default_roots(&cfg, done),
    }))
}

/// A code folder was saved through setup before (kv `done:`).
fn done(db: &Db) -> bool {
    db.kv_get(KV)
        .ok()
        .flatten()
        .is_some_and(|a| a.starts_with("done"))
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

/// `$HOME` as given and resolved (`/home` may be a symlink to
/// `/var/home`): the suggestion never offers either or anything above.
fn homes() -> Vec<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let mut v = vec![home.clone()];
    if let Ok(c) = std::fs::canonicalize(&home) {
        if c != home {
            v.push(c);
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
    let (needed, done) = {
        let db = d.db();
        (needed(&db, &cfg), done(&db))
    };
    let s = code_dir::suggest(
        &seen,
        &homes(),
        &rlord_core::import::excluded_dirs(),
        cfg.session.repo_scan_depth,
    );
    let mut v = suggestion_value(&s, &cfg, needed, &Guard::new(), &|p| p.is_dir());
    v["done"] = json!(done);
    v["roots_default"] = json!(default_roots(&cfg, done));
    Ok(v)
}

/// The `setup.suggest` answer for a suggestion. `is_dir` is asked only of
/// the folders shown, and only of those no prompt guards.
fn suggestion_value(
    s: &Suggestion,
    cfg: &Config,
    needed: bool,
    guard: &Guard,
    is_dir: &dyn Fn(&Path) -> bool,
) -> Value {
    let under = |dir: &str| {
        s.projects
            .iter()
            .filter(|p| Path::new(p).starts_with(dir))
            .count()
    };
    // A single explicit root that isn't `~` outranks the vote.
    let roots = cfg.session.repo_roots.clone();
    let explicit = match roots.as_slice() {
        [r] if r.trim() != "~" && !r.trim().is_empty() => Some(expand_tilde(r.trim())),
        _ => None,
    };
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
            let prompts = guard.prompts(Path::new(&path)).is_some();
            if !prompts && !is_dir(Path::new(&path)) {
                return None;
            }
            Some(json!({"path": path, "projects": projects, "of": s.total,
                        "from": from, "prompts": prompts}))
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
    apply_with(d, params, path, &Guard::new())
}

/// `apply_at` with a given guard (tests).
fn apply_with(d: &Daemon, params: &Value, path: &Path, guard: &Guard) -> Result<Value> {
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
    let mut created = false;
    if checked && !Path::new(&dir).is_dir() {
        if params["create"] != true {
            bail!("{} doesn't exist (create it?)", contract_tilde(&dir));
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {dir}"))?;
        created = true;
    }
    let file = Config::set_code_dir_at(path, &contract_tilde(&dir), roots)?;
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
