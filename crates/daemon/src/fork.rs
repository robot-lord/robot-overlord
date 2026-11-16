//! `session.fork`: a new session that goes on from a copy of another
//! session's whole conversation, on the same agent, in the same folder or
//! a new worktree. The original is left as it is. Plain code, no AI.
//!
//! A **user fork** is its own session (its own tmux session, its own
//! `RLORD_SESSION_ID`, its own row with `forked_from`). It is not the
//! archive summary's fork (`claude::summary_argv`, `codex::summary_argv`,
//! OpenCode's archived throwaway session), which never becomes a session.
//!
//! Per agent (`Agent::fork`): Claude `--resume <parent> --fork-session`
//! with a new id rlord chooses; Codex `codex fork <parent>`, always in the
//! pane, bound to the thread whose rollout names the parent; OpenCode made
//! by its server first (`POST /api/session/<id>/fork`), then opened.
//!
//! Modelled on `import`: the slow and outside parts (the agent's hooks
//! check, OpenCode's fork call, `git worktree add`, the parent's usage) run
//! without the DB lock; reading and inserting under it.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use rlord_core::agents::{claude, codex, install, opencode, Agent, Fork, Launch};
use rlord_core::config::Config;
use rlord_core::db::{ContextRow, Db};
use rlord_core::types::{now_ms, Session, Status};
use rlord_core::{git, names, paths};

use crate::context::{self, ForkFolder, NewRow};
use crate::methods::{self, p_str, spec_args, Background, Start};
use crate::Daemon;

/// At or over this much of its context window, the fork is warned about.
const FULL_PCT: u64 = 70;

/// kv key: a Claude or Codex fork whose conversation may not exist yet
/// (`{"parent_conversation": …}`): a restart forks again instead of
/// resuming (`methods::session_restart`), until it does.
pub(crate) fn pending_key(id: &str) -> String {
    format!("fork_pending:{id}")
}

/// What a fork asks for, read and checked under the lock.
#[derive(Debug, Clone)]
pub(crate) struct Want {
    pub parent: Session,
    pub agent: Agent,
    /// The parent's conversation id.
    pub conversation: String,
    /// The parent's launch spec.
    pub spec: Value,
    pub prompt: Option<String>,
    /// Asked for by name (else the default, `names::fork_name`).
    pub named: bool,
    pub name: String,
    pub description: Option<String>,
    /// Another model than the parent's.
    pub model: Option<String>,
    /// The overlord's note and related sessions (checked).
    pub note: Option<String>,
    pub related: Vec<String>,
    /// A new worktree: (repo, branch, folder).
    pub worktree: Option<(String, String, std::path::PathBuf)>,
}

/// What was done outside the lock.
#[derive(Debug, Clone, Default)]
pub(crate) struct Made {
    /// OpenCode: the fork its server made.
    pub opencode_id: Option<String>,
    /// rlord's hooks work for the agent (its background then goes by hook).
    pub hooks: bool,
    /// How full the parent's context window is (%), when known.
    pub context_pct: Option<u64>,
    /// The parent's folder has changes not committed (a worktree fork
    /// doesn't carry them).
    pub uncommitted: bool,
}

/// `session.fork` params: `ref` (the parent), `prompt`, `name`,
/// `description`, `model`, `context`, `related`, `by_overlord`,
/// `workspace` (a new worktree) and `branch`.
pub fn fork(d: &Daemon, params: &Value) -> Result<Value> {
    let cfg = d.config();
    let want = want(&d.db(), &cfg, params)?;
    // Not installed: say how to get it, rather than fail in the pane.
    if let Some(why) = install::missing(want.agent) {
        bail!("{why}");
    }
    let mut made = Made {
        hooks: context::hooks_work(want.agent),
        ..Default::default()
    };
    // (Read as `session.get` reads it.)
    let mut rows = [serde_json::to_value(&want.parent)?];
    crate::usage::annotate(d, &mut rows, true);
    made.context_pct = rows[0]["usage"]["context_pct"].as_u64();
    if want.agent == Agent::OpenCode {
        made.opencode_id = Some(opencode_fork(&want.conversation)?);
    }
    if let Some((repo, branch, dir)) = &want.worktree {
        made.uncommitted = git::has_changes(&want.parent.project_path) == Some(true);
        let start = git::head_commit(&want.parent.project_path)?;
        git::ensure_excluded(repo, &cfg.session.workspace_dir)?;
        git::worktree_add_from(repo, branch, dir, &start)?;
    }
    finish(&d.db(), &want, &made, &paths::data_dir())
}

/// OpenCode's fork of `conversation`, by its server when one answers, else
/// by OpenCode's own client (which starts the server, as a launch does).
fn opencode_fork(conversation: &str) -> Result<String> {
    let svc = rlord_core::agents::opencode_service::Service::read(&opencode::service_file());
    let r = match svc.filter(|s| s.answers()) {
        Some(svc) => svc.fork(conversation),
        None => {
            let bin = install::resolved("opencode")
                .unwrap_or_else(|| std::path::PathBuf::from("opencode"));
            opencode::fork_via_cli(&bin, conversation)
        }
    };
    r.map_err(|e| match e.to_string().as_str() {
        opencode::NOTHING_TO_FORK => anyhow::anyhow!("nothing to fork yet: {}", e),
        _ => anyhow::anyhow!("OpenCode didn't fork it: {e}"),
    })
}

/// Read the parent and check the fork can be made (see "States of the
/// parent" in docs/architecture.md).
pub(crate) fn want(db: &Db, cfg: &Config, params: &Value) -> Result<Want> {
    let p_opt = |k: &str| {
        params[k]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    let parent = db.resolve_ref(p_str(params, "ref")?)?;
    methods::refuse_overlord(&parent, "forked")?;
    let agent = Agent::named(&parent.agent)?;
    let nothing = || anyhow::anyhow!("nothing to fork yet: {} has no conversation", parent.name);
    let conversation = parent.agent_session.clone().ok_or_else(nothing)?;
    if parent.agent_session_trust == "scavenged" {
        bail!(
            "{}'s conversation id was guessed, not confirmed: rlord won't fork it",
            parent.name
        );
    }
    if !std::path::Path::new(&parent.project_path).is_dir() {
        bail!(
            "{}'s folder is gone ({}): unarchive and restore it first",
            parent.name,
            parent.project_path
        );
    }
    // A conversation the agent never wrote (Claude writes its transcript
    // with the first message).
    let written = match db.transcript_source(&parent.id)? {
        Some(t) if agent != Agent::OpenCode => std::path::Path::new(&t).exists(),
        Some(_) => true,
        None if agent == Agent::Claude => {
            claude::transcript_path(&parent.project_path, &conversation).exists()
        }
        None => true,
    };
    if !written {
        return Err(nothing());
    }
    let spec: Value = serde_json::from_str(&db.launch_spec(&parent.id)?).unwrap_or(json!({}));
    // (Checked as for a new session: the parent's own links are copied,
    // and the oldest of them dropped past the cap: `rows`.)
    let (note, mut related) = context::check(
        db,
        p_opt("context").as_deref(),
        &methods::ref_list(params, "related"),
        None,
    )?;
    related.retain(|r| *r != parent.id);
    let named = p_opt("name");
    let name = match &named {
        Some(n) => n.clone(),
        None => default_name(db, &parent.name),
    };
    let worktree = if params["workspace"] == true {
        if !agent.forks_elsewhere() {
            bail!(
                "{} can't fork into a new worktree: its fork stays in its parent's folder. \
                 Fork it in the same folder",
                agent.label()
            );
        }
        let repo = parent
            .repo_path
            .clone()
            .filter(|_| git::repo_root(&parent.project_path).is_some())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} isn't in a git repo: fork it in the same folder",
                    parent.name
                )
            })?;
        let branch = p_opt("branch").unwrap_or_else(|| git::sanitize_branch(&name));
        let dir = git::workspace_path(&repo, &cfg.session.workspace_dir, &branch);
        Some((repo, branch, dir))
    } else {
        None
    };
    Ok(Want {
        agent,
        conversation,
        spec,
        prompt: p_opt("prompt"),
        named: named.is_some(),
        name,
        description: p_opt("description"),
        model: p_opt("model"),
        note,
        related,
        worktree,
        parent,
    })
}

/// `<parent>-fork`, `-fork-2`, …: the first name no session has.
fn default_name(db: &Db, parent: &str) -> String {
    names::fork_name(parent, |n| db.name_taken(n).unwrap_or(false))
}

/// What to say about the fork, by plain code: a live parent shares its
/// folder; a working one's last step isn't in the copy; a nearly full
/// context window; what a new worktree leaves behind.
pub(crate) fn warnings(want: &Want, made: &Made) -> Vec<String> {
    let p = &want.parent;
    let mut out = Vec::new();
    if p.status == Status::Working {
        out.push(format!(
            "{} is working: the fork has its conversation up to its last finished step",
            p.name
        ));
    }
    let live = p.archived_at.is_none() && p.status != Status::Stopped;
    if want.worktree.is_none() && live {
        out.push(format!(
            "it shares its folder with {} ({}): the two work on the same files",
            p.name,
            p.status.label()
        ));
    }
    if made.uncommitted {
        out.push(format!(
            "{} has changes not committed: the new worktree doesn't have them",
            p.name
        ));
    }
    if let Some(pct) = made.context_pct.filter(|p| *p >= FULL_PCT) {
        out.push(format!("the fork starts with a context window {pct}% full"));
    }
    out
}

/// The fork's background: the parent's rows to leave out (its oldest links
/// past `LINKS_MAX`, never the link to the parent), and the rows to add
/// after the copied ones (the fork's note and its link to the parent,
/// then the overlord's note and related sessions).
pub(crate) fn rows(
    want: &Want,
    parent_rows: &[ContextRow],
    folder: &ForkFolder,
    now: i64,
) -> (Vec<i64>, Vec<NewRow>) {
    let p = &want.parent;
    let mut add = vec![
        NewRow {
            kind: "note",
            body: context::fork_note(&p.name, &p.id, now, folder),
            source: "fork",
        },
        NewRow {
            kind: "link",
            body: p.id.clone(),
            source: "fork",
        },
    ];
    add.extend(context::rows_from(
        want.note.as_slice(),
        &want.related,
        "overlord",
    ));
    // Links already copied aren't added again.
    let copied_links: Vec<&ContextRow> = parent_rows.iter().filter(|r| r.kind == "link").collect();
    let mut seen: Vec<String> = copied_links.iter().map(|c| c.body.clone()).collect();
    add.retain(|r| {
        if r.kind != "link" || !seen.contains(&r.body) {
            if r.kind == "link" {
                seen.push(r.body.clone());
            }
            return true;
        }
        false
    });
    let new_links = add.iter().filter(|r| r.kind == "link").count();
    let over = (copied_links.len() + new_links).saturating_sub(context::LINKS_MAX);
    let except = copied_links.iter().take(over).map(|r| r.id).collect();
    (except, add)
}

/// The fork's launch spec: the parent's, without what is per session
/// (Remote Control, Codex's daemon host), with the fork's prompt and
/// model.
fn spec_of(want: &Want) -> Value {
    let mut spec = want.spec.clone();
    if let Some(o) = spec.as_object_mut() {
        for k in ["remote_control", "remote_control_off", "codex_host"] {
            o.remove(k);
        }
    }
    spec["agent"] = json!(want.agent.name());
    spec["prompt"] = json!(want.prompt);
    if let Some(m) = &want.model {
        spec["model"] = json!(m);
    }
    spec
}

/// The fork's command: the agent's own fork, or (OpenCode) opening the
/// fork its server made.
pub(crate) fn launch(
    want: &Want,
    made: &Made,
    project: &str,
    first: Option<&str>,
    args: &[String],
) -> Result<Launch> {
    let spec_model = want.spec["model"].as_str().filter(|m| !m.is_empty());
    match want.agent.fork(
        &want.conversation,
        project,
        first,
        want.model.as_deref().or(spec_model),
        args,
    ) {
        Fork::Native(l) => Ok(l),
        Fork::Server => {
            let id = made
                .opencode_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("the fork wasn't made"))?;
            Ok(want.agent.open_fork(id, first, want.model.as_deref(), args))
        }
        Fork::Seeded => bail!("forking isn't supported for {} yet", want.agent.label()),
    }
}

/// Insert and start the fork, under the lock.
pub(crate) fn finish(db: &Db, want: &Want, made: &Made, base: &std::path::Path) -> Result<Value> {
    // Checked again: the parent may have gone, or the name been taken,
    // while the lock was off.
    let parent = db.resolve_ref(&want.parent.id)?;
    let name = if !want.named && db.name_taken(&want.name)? {
        default_name(db, &parent.name)
    } else {
        want.name.clone()
    };
    let agent = want.agent;
    let id = rlord_core::types::new_id();
    let now = now_ms();
    let (project, folder, repo, workspace, branch) = match &want.worktree {
        Some((repo, branch, dir)) => {
            let dir = dir.to_string_lossy().into_owned();
            (
                dir.clone(),
                ForkFolder::Worktree(dir.clone(), branch.clone()),
                Some(repo.clone()),
                Some(dir),
                Some(branch.clone()),
            )
        }
        None => (
            parent.project_path.clone(),
            ForkFolder::Shared,
            parent.repo_path.clone(),
            parent.workspace_path.clone(),
            parent.branch.clone(),
        ),
    };
    let parent_rows = db.context_rows(&parent.id)?;
    let (except, add) = rows(want, &parent_rows, &folder, now);
    let copied: Vec<ContextRow> = parent_rows
        .iter()
        .filter(|r| !except.contains(&r.id))
        .cloned()
        .collect();
    // A fork always has background: at least its note and link.
    let mode = context::delivery(agent, made.hooks);
    let bg_file = context::file_path(base, &id);
    let text = context::launch_file_text_rows(db, &name, &copied, &add, now);
    let carried = match context::write_file(base, &id, &text) {
        Ok(_) => Some(mode),
        Err(e) => {
            tracing::warn!(session = %id, error = %e, "writing the background file");
            (mode == context::Delivery::Hook).then_some(mode)
        }
    };
    let spec = spec_of(want);
    let mut args = spec_args(agent, &spec);
    args.extend(context::launch_args(carried, &bg_file));
    let first = context::launch_prompt(want.prompt.as_deref(), carried, &bg_file);
    let launch = launch(want, made, &project, first.as_deref(), &args)?;
    let tmux_name = methods::tmux_name_for(db, &name, None);
    let launch_spec = spec.to_string();
    methods::start(
        db,
        Start {
            id: &id,
            name: &name,
            agent,
            launch_spec: &launch_spec,
            launch,
            first: first.as_deref(),
            project_path: &project,
            repo: repo.as_deref(),
            workspace_path: workspace.as_deref(),
            branch: branch.as_deref(),
            role: "",
            tmux_name: &tmux_name,
            daemon: false,
            forked_from: Some(&parent.id),
            background: Some(Background {
                mode,
                carried,
                copy_from: Some((&parent.id, &except)),
                rows: add,
            }),
            event: (
                "forked",
                json!({"from": parent.id, "from_conversation": want.conversation,
                       "workspace": workspace, "branch": branch}),
            ),
        },
    )?;
    after_start(db, want, &parent, &id)?;
    tracing::info!(session = %id, parent = %parent.id, name = %name, agent = agent.name(), "forked session");
    let mut out = serde_json::to_value(db.resolve_ref(&id)?)?;
    out["warnings"] = json!(warnings(want, made));
    Ok(out)
}

/// What a fork takes from its parent besides its conversation (group,
/// tags, description; not its notes), and how its conversation is found.
pub(crate) fn after_start(db: &Db, want: &Want, parent: &Session, id: &str) -> Result<()> {
    if !parent.group_path.is_empty() {
        db.set_group(id, &parent.group_path)?;
    }
    for t in &parent.tags {
        db.add_tag(id, t)?;
    }
    let description = want.description.as_deref().unwrap_or(&parent.description);
    if !description.is_empty() {
        db.set_description(id, description)?;
    }
    if !want.agent.mints_id() {
        // Its thread is the one forked from the parent's (`bind_fork`).
        db.kv_set(&methods::bind_fork_key(id), &want.conversation)?;
    }
    if want.agent != Agent::OpenCode {
        // (OpenCode's fork exists before its pane starts.)
        db.kv_set(
            &pending_key(id),
            &json!({"parent_conversation": want.conversation}).to_string(),
        )?;
    }
    Ok(())
}

/// A restart of a fork whose conversation doesn't exist yet: the parent's
/// conversation to fork again (and the key cleared once the fork exists).
/// None: an ordinary restart (resume).
pub(crate) fn pending(db: &Db, s: &Session) -> Result<Option<String>> {
    let Some(v) = db.kv_get(&pending_key(&s.id))? else {
        return Ok(None);
    };
    let exists = match Agent::parse(&s.agent) {
        Some(Agent::Claude) => match (s.agent_session.as_deref(), db.transcript_source(&s.id)?) {
            (None, _) => false,
            (Some(_), Some(t)) => std::path::Path::new(&t).exists(),
            (Some(sid), None) => claude::transcript_path(&s.project_path, sid).exists(),
        },
        Some(Agent::Codex) => s.agent_session.is_some(),
        _ => true,
    };
    if exists {
        db.kv_delete(&pending_key(&s.id))?;
        return Ok(None);
    }
    let parent = serde_json::from_str::<Value>(&v)
        .ok()
        .and_then(|v| v["parent_conversation"].as_str().map(str::to_string));
    if parent.is_none() {
        db.kv_delete(&pending_key(&s.id))?;
    }
    Ok(parent)
}

/// The command that forks again for a restart of a fork that never
/// materialized (`pending`): Claude under the fork's own id, Codex anew.
pub(crate) fn refork_argv(
    agent: Agent,
    s: &Session,
    parent_conversation: &str,
    model: Option<&str>,
    extra: &[String],
) -> Result<Vec<String>> {
    match agent {
        Agent::Claude => Ok(claude::fork_argv(
            parent_conversation,
            s.agent_session.as_deref(),
            None,
            model,
            extra,
        )),
        Agent::Codex => Ok(codex::fork_argv(parent_conversation, None, model, extra)),
        Agent::OpenCode => bail!("an OpenCode fork exists from the start"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlord_core::db::NewSession;

    /// A session in the test DB, its folder `dir` (which exists or not).
    fn mk(db: &Db, id: &str, agent: &str, conv: Option<&str>, dir: &str, role: &str) {
        db.insert_session(&NewSession {
            id,
            name: &format!("n-{id}"),
            agent,
            agent_session: conv,
            project_path: dir,
            tmux_session: None,
            launch_spec: &json!({"agent": agent, "model": "m-1", "prompt": "first",
                                 "extra_args": [], "remote_control": "x",
                                 "codex_host": "daemon"})
            .to_string(),
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role,
        })
        .unwrap();
    }

    fn err(db: &Db, params: Value) -> String {
        want(db, &Config::default(), &params)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn what_cant_be_forked_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();
        let db = Db::open_in_memory().unwrap();
        mk(&db, "o", "claude", Some("c-o"), here, "overlord");
        assert!(err(&db, json!({"ref": "o"})).contains("overlord"));
        // No conversation yet.
        mk(&db, "a", "codex", None, here, "");
        assert!(err(&db, json!({"ref": "a"})).contains("nothing to fork yet"));
        // Claude's transcript was never written.
        mk(&db, "b", "claude", Some("c-b"), here, "");
        let absent = dir.path().join("c-b.jsonl");
        db.upsert_transcript_source("b", absent.to_str().unwrap())
            .unwrap();
        assert!(err(&db, json!({"ref": "b"})).contains("nothing to fork yet"));
        std::fs::write(&absent, "{}\n").unwrap();
        assert!(want(&db, &Config::default(), &json!({"ref": "b"})).is_ok());
        // A guessed id.
        mk(&db, "s", "codex", Some("t-s"), here, "");
        db.set_agent_session("s", "t-s", "scavenged").unwrap();
        assert!(err(&db, json!({"ref": "s"})).contains("guessed"));
        // Its folder is gone.
        mk(
            &db,
            "g",
            "codex",
            Some("t-g"),
            "/nonexistent/rlord-fork-test",
            "",
        );
        assert!(err(&db, json!({"ref": "g"})).contains("folder is gone"));
        // A new worktree: never for OpenCode; only in a repo.
        mk(&db, "oc", "opencode", Some("ses_1"), here, "");
        assert!(err(&db, json!({"ref": "oc", "workspace": true})).contains("same folder"));
        mk(&db, "c", "codex", Some("t-c"), here, "");
        assert!(err(&db, json!({"ref": "c", "workspace": true})).contains("git repo"));
        // The overlord is no related session.
        assert!(err(&db, json!({"ref": "c", "related": ["o"]})).contains("overlord"));
    }

    #[test]
    fn a_fork_is_named_after_its_parent_and_says_what_it_shares() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();
        let db = Db::open_in_memory().unwrap();
        mk(&db, "p", "codex", Some("t-p"), here, "");
        let w = want(&db, &Config::default(), &json!({"ref": "p"})).unwrap();
        assert_eq!((w.name.as_str(), w.named), ("n-p-fork", false));
        mk(&db, "f", "codex", None, here, "");
        db.rename("f", "n-p-fork").unwrap();
        let w = want(&db, &Config::default(), &json!({"ref": "p"})).unwrap();
        assert_eq!(w.name, "n-p-fork-2");
        let w = want(
            &db,
            &Config::default(),
            &json!({"ref": "p", "name": " mine ", "prompt": " try a queue ", "model": "m-2"}),
        )
        .unwrap();
        assert_eq!(
            (
                w.name.as_str(),
                w.named,
                w.prompt.as_deref(),
                w.model.as_deref()
            ),
            ("mine", true, Some("try a queue"), Some("m-2"))
        );
        // Its launch spec: the parent's, less what is per session.
        let spec = spec_of(&w);
        assert_eq!(spec["model"], "m-2");
        assert_eq!(spec["prompt"], "try a queue");
        assert!(spec.get("remote_control").is_none() && spec.get("codex_host").is_none());

        // Warnings, by plain code.
        let mut w = want(&db, &Config::default(), &json!({"ref": "p"})).unwrap();
        w.parent.status = Status::Working;
        let made = Made {
            context_pct: Some(82),
            ..Default::default()
        };
        let says = warnings(&w, &made).join("\n");
        assert!(says.contains("last finished step"), "{says}");
        assert!(says.contains("shares its folder"), "{says}");
        assert!(says.contains("82% full"), "{says}");
        w.parent.status = Status::Stopped;
        assert!(warnings(&w, &Made::default()).is_empty());
        w.parent.status = Status::Idle;
        w.parent.archived_at = Some(5);
        assert!(warnings(&w, &Made::default()).is_empty());
    }

    #[test]
    fn a_fork_takes_its_parents_background_and_adds_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();
        let db = Db::open_in_memory().unwrap();
        mk(&db, "p", "claude", Some("c-p"), here, "");
        let t = dir.path().join("c-p.jsonl");
        std::fs::write(&t, "{}\n").unwrap();
        db.upsert_transcript_source("p", t.to_str().unwrap())
            .unwrap();
        // The parent links to 16 sessions already.
        for i in 0..16 {
            let id = format!("r{i:02}");
            mk(&db, &id, "claude", None, here, "");
            db.context_add("p", 10 + i, "link", &id, "overlord")
                .unwrap();
        }
        db.context_add("p", 40, "note", "use the staging DB", "overlord")
            .unwrap();
        mk(&db, "x", "claude", None, here, "");
        let w = want(
            &db,
            &Config::default(),
            &json!({"ref": "p", "context": "the user wants a queue", "related": ["x", "r05", "p"]}),
        )
        .unwrap();
        assert_eq!(w.related, ["x", "r05"], "not the parent itself");
        let parent_rows = db.context_rows("p").unwrap();
        let (except, add) = rows(&w, &parent_rows, &ForkFolder::Shared, 1_800_000_000_000);
        // Its note and its link to the parent first, then the overlord's
        // note and the related session not already linked.
        let got: Vec<(&str, &str)> = add.iter().map(|r| (r.kind, r.source)).collect();
        assert_eq!(
            got,
            [
                ("note", "fork"),
                ("link", "fork"),
                ("note", "overlord"),
                ("link", "overlord")
            ]
        );
        assert!(add[0]
            .body
            .starts_with("Forked from session \"n-p\" (p) at "));
        assert!(add[0].body.contains("same folder"), "{}", add[0].body);
        assert_eq!((add[1].body.as_str(), add[3].body.as_str()), ("p", "x"));
        // 16 copied + 2 new: the two oldest copied links are left out.
        let first_two: Vec<i64> = parent_rows.iter().take(2).map(|r| r.id).collect();
        assert_eq!(except, first_two);

        // Copied as the fork starts.
        mk(&db, "f", "claude", Some("c-f"), here, "");
        db.context_copy("p", "f", &except).unwrap();
        context::store_rows(&db, "f", &add, 50).unwrap();
        let links = db.context_links("f").unwrap();
        assert_eq!(links.len(), context::LINKS_MAX);
        assert!(links.contains(&"p".to_string()) && !links.contains(&"r00".to_string()));
        let notes: Vec<String> = db
            .context_rows("f")
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == "note")
            .map(|r| r.source)
            .collect();
        assert_eq!(notes, ["overlord", "fork", "overlord"]);
        // The parent's own background is left as it was.
        assert_eq!(db.context_rows("p").unwrap(), parent_rows);

        // The file the fork starts with names the parent.
        let text = context::launch_file_text_rows(&db, "n-p-fork", &parent_rows[2..], &add, 50);
        assert!(text.contains("Forked from session"), "{text}");
        assert!(text.contains("use the staging DB"), "{text}");
        assert!(text.contains("- n-p ("), "{text}");

        // A worktree fork's note says where it works.
        let note = context::fork_note(
            "n-p",
            "p",
            0,
            &ForkFolder::Worktree("/r/.worktrees/x".into(), "x".into()),
        );
        assert!(
            note.contains("own worktree, /r/.worktrees/x (branch x)"),
            "{note}"
        );
    }

    #[test]
    fn a_fork_copies_group_tags_and_description_but_not_notes() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();
        let db = Db::open_in_memory().unwrap();
        for (id, agent, conv) in [
            ("p", "codex", Some("t-p")),
            ("q", "opencode", Some("ses_1")),
        ] {
            mk(&db, id, agent, conv, here, "");
            db.set_group(id, "work/api").unwrap();
            db.add_tag(id, "auth").unwrap();
            db.set_description(id, "Fix the login flow").unwrap();
            db.set_notes(id, "the user's own notes").unwrap();
        }
        mk(&db, "f", "codex", None, here, "");
        let w = want(&db, &Config::default(), &json!({"ref": "p"})).unwrap();
        after_start(&db, &w, &w.parent, "f").unwrap();
        let f = db.resolve_ref("f").unwrap();
        assert_eq!(f.group_path, "work/api");
        assert_eq!(f.tags, ["auth"]);
        assert_eq!(f.description, "Fix the login flow");
        assert_eq!(f.notes, "");
        // Codex: bound to the thread forked from the parent's; pending
        // until it is.
        assert_eq!(
            db.kv_get(&methods::bind_fork_key("f")).unwrap().as_deref(),
            Some("t-p")
        );
        assert_eq!(pending(&db, &f).unwrap().as_deref(), Some("t-p"));
        db.set_agent_session("f", "t-f", "hook").unwrap();
        let f = db.resolve_ref("f").unwrap();
        assert_eq!(pending(&db, &f).unwrap(), None);
        assert_eq!(db.kv_get(&pending_key("f")).unwrap(), None, "cleared");
        // OpenCode's fork exists before its pane: nothing pending; a
        // description given wins.
        mk(&db, "g", "opencode", Some("ses_2"), here, "");
        let w = want(
            &db,
            &Config::default(),
            &json!({"ref": "q", "description": "Try a queue"}),
        )
        .unwrap();
        after_start(&db, &w, &w.parent, "g").unwrap();
        assert_eq!(db.resolve_ref("g").unwrap().description, "Try a queue");
        assert_eq!(db.kv_get(&pending_key("g")).unwrap(), None);
        assert_eq!(db.kv_get(&methods::bind_fork_key("g")).unwrap(), None);
    }

    #[test]
    fn each_agent_forks_its_own_way() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();
        let db = Db::open_in_memory().unwrap();
        for (id, agent, conv) in [
            ("cl", "claude", "c-1"),
            ("cx", "codex", "t-1"),
            ("oc", "opencode", "ses_1"),
        ] {
            mk(&db, id, agent, Some(conv), here, "");
            let t = dir.path().join(format!("{conv}.jsonl"));
            std::fs::write(&t, "{}\n").unwrap();
            db.upsert_transcript_source(id, t.to_str().unwrap())
                .unwrap();
        }
        let args = |w: &Want| spec_args(w.agent, &spec_of(w));
        let w = want(&db, &Config::default(), &json!({"ref": "cl"})).unwrap();
        let l = launch(&w, &Made::default(), here, Some("go"), &args(&w)).unwrap();
        assert_eq!(
            &l.argv[..4],
            ["claude", "--resume", "c-1", "--fork-session"]
        );
        assert_eq!(l.argv.last().unwrap(), "go");
        if claude::FORK_MINTS_ID {
            let new = l.session_id.clone().unwrap();
            assert_ne!(new, "c-1");
            assert_eq!(&l.argv[4..6], ["--session-id", new.as_str()]);
            assert!(l.transcript.unwrap().ends_with(format!("{new}.jsonl")));
        }
        // The parent's model, and no Remote Control.
        assert!(l.argv.windows(2).any(|a| a == ["--model", "m-1"]));
        assert!(!l.argv.iter().any(|a| a == "--remote-control"));

        let w = want(&db, &Config::default(), &json!({"ref": "cx"})).unwrap();
        let l = launch(&w, &Made::default(), here, None, &args(&w)).unwrap();
        assert_eq!(&l.argv[..3], ["codex", "fork", "t-1"]);
        // Always in the pane, whatever the parent's host.
        assert!(l.argv.contains(&"--no-daemon".to_string()), "{:?}", l.argv);
        assert_eq!(l.session_id, None);

        let w = want(
            &db,
            &Config::default(),
            &json!({"ref": "oc", "model": "a/b"}),
        )
        .unwrap();
        assert!(launch(&w, &Made::default(), here, None, &[]).is_err());
        let made = Made {
            opencode_id: Some("ses_2fork".into()),
            ..Default::default()
        };
        let l = launch(&w, &made, here, Some("go"), &[]).unwrap();
        assert_eq!(l.argv, ["opencode", "-s", "ses_2fork", "--prompt", "go"]);
        assert_eq!(l.session_id.as_deref(), Some("ses_2fork"));
        assert_eq!(l.deferred.unwrap().model, "a/b");
    }

    #[test]
    fn a_fork_that_never_started_forks_again_on_restart() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();
        let db = Db::open_in_memory().unwrap();
        mk(&db, "f", "claude", Some("c-new"), here, "");
        let t = dir.path().join("c-new.jsonl");
        db.upsert_transcript_source("f", t.to_str().unwrap())
            .unwrap();
        db.kv_set(&pending_key("f"), r#"{"parent_conversation":"c-p"}"#)
            .unwrap();
        let s = db.resolve_ref("f").unwrap();
        assert_eq!(pending(&db, &s).unwrap().as_deref(), Some("c-p"));
        let argv = refork_argv(Agent::Claude, &s, "c-p", Some("m"), &[]).unwrap();
        assert_eq!(
            argv,
            [
                "claude",
                "--resume",
                "c-p",
                "--fork-session",
                "--session-id",
                "c-new",
                "--model",
                "m"
            ]
        );
        // Once its transcript exists it resumes as any session does.
        std::fs::write(&t, "{}\n").unwrap();
        assert_eq!(pending(&db, &s).unwrap(), None);
        assert_eq!(db.kv_get(&pending_key("f")).unwrap(), None);
        // Not a session that was never a fork.
        mk(&db, "g", "codex", None, here, "");
        assert_eq!(pending(&db, &db.resolve_ref("g").unwrap()).unwrap(), None);
        assert_eq!(
            refork_argv(
                Agent::Codex,
                &db.resolve_ref("g").unwrap(),
                "t-p",
                None,
                &[]
            )
            .unwrap(),
            ["codex", "fork", "t-p", "--no-alt-screen"]
        );
    }

    /// A Codex fork runs in its own pane with its own RLORD_SESSION_ID:
    /// its first SessionStart (`source: "fork"`) binds it and gets its
    /// background, though the same event by thread binds no daemon-mode
    /// session waiting in the folder (`codex_gate::starts_fresh`).
    #[test]
    fn a_pane_fork_binds_on_its_session_start() {
        let db = Db::open_in_memory().unwrap();
        db.insert_session(&NewSession {
            id: "01fork",
            name: "api-fork",
            agent: "codex",
            agent_session: None,
            project_path: "/src/app",
            tmux_session: Some("rlord_01fork_absent"),
            launch_spec: r#"{"agent":"codex"}"#,
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
        db.set_status("01fork", Status::Starting, 1).unwrap();
        db.kv_set(&methods::bind_fork_key("01fork"), "t-parent")
            .unwrap();
        db.kv_set(&methods::bind_since_key("01fork"), "1").unwrap();
        context::turn_on(&db, "01fork").unwrap();
        context::store_rows(
            &db,
            "01fork",
            &[NewRow {
                kind: "note",
                body: "Forked from session \"api\"".into(),
                source: "fork",
            }],
            2,
        )
        .unwrap();
        context::record_mode(&db, "01fork", context::Delivery::Hook).unwrap();
        let r = methods::hook_emit_with(
            &db,
            &json!({"instance": "01fork", "agent": "codex", "event": "SessionStart",
                    "payload": {"session_id": "t-fork", "hook_event_name": "SessionStart",
                                "source": "fork", "cwd": "/src/app",
                                "transcript_path": "/c/rollout-x-t-fork.jsonl"}}),
            &|_, _| rlord_core::agents::Indexed::User,
        )
        .unwrap();
        let s = db.resolve_ref("01fork").unwrap();
        assert_eq!(s.agent_session.as_deref(), Some("t-fork"));
        assert_eq!(s.agent_session_trust, "hook");
        let reply = r["reply"].to_string();
        assert!(reply.contains("Forked from session"), "{r}");
        // Bound: nothing left to find it by.
        assert_eq!(db.kv_get(&methods::bind_fork_key("01fork")).unwrap(), None);
    }
}
