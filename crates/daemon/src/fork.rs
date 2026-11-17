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
    pub name: String,
    /// The name was chosen here (`<parent>-fork`…), not asked for: it is
    /// reserved while the fork is made (`reserve_key`).
    pub auto_name: bool,
    pub description: Option<String>,
    /// Another model than the parent's.
    pub model: Option<String>,
    /// The overlord's note and related sessions (checked).
    pub note: Option<String>,
    pub related: Vec<String>,
    /// A new worktree was asked for (`workspace`), with this branch (else
    /// one from the name).
    pub wants_worktree: bool,
    pub branch: Option<String>,
    /// Where it goes: worked out outside the lock (`plan_worktree`).
    pub worktree: Option<Worktree>,
}

/// A worktree fork's new worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Worktree {
    pub repo: String,
    pub branch: String,
    /// The new worktree.
    pub dir: std::path::PathBuf,
    /// Where the fork works: the parent's subfolder of its checkout, in
    /// the new worktree (the worktree itself when the parent works at its
    /// top).
    pub project: std::path::PathBuf,
    /// The commit it branches from: the parent's HEAD.
    pub start: String,
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

/// kv key: a default fork name being used by a fork in the making (the
/// time it was taken, ms), so two forks at once don't pick the same one
/// (and a worktree fork's branch, named after it, stays its own).
fn reserve_key(name: &str) -> String {
    format!("fork_name:{name}")
}

/// How long a reservation holds, should a fork never release it.
const RESERVED_MS: i64 = 10 * 60_000;

/// A session has the name, or a fork in the making reserved it.
fn name_unavailable(db: &Db, name: &str, now: i64) -> bool {
    db.name_taken(name).unwrap_or(false)
        || db
            .kv_get(&reserve_key(name))
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .is_some_and(|at| now - at < RESERVED_MS)
}

/// Releases a reserved name when the fork is done, whichever way.
struct Reserved<'a>(&'a Daemon, Option<String>);

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        if let Some(n) = &self.1 {
            let _ = self.0.db().kv_delete(&reserve_key(n));
        }
    }
}

/// `want`, and its default name reserved for this fork until it is done
/// (`Reserved` releases it), chosen and reserved under one lock so two
/// forks at once never pick the same one. A name asked for isn't
/// reserved.
fn want_reserved<'a>(d: &'a Daemon, params: &Value) -> Result<(Want, Reserved<'a>)> {
    let want = {
        let db = d.db();
        let want = want(&db, params)?;
        if want.auto_name {
            db.kv_set(&reserve_key(&want.name), &now_ms().to_string())?;
        }
        want
    };
    let reserved = Reserved(d, want.auto_name.then(|| want.name.clone()));
    Ok((want, reserved))
}

/// `session.fork` params: `ref` (the parent), `prompt`, `name`,
/// `description`, `model`, `context`, `related`, `workspace` (a new
/// worktree) and `branch`.
pub fn fork(d: &Daemon, params: &Value) -> Result<Value> {
    let cfg = d.config();
    let base = paths::data_dir();
    let (mut want, _reserved) = want_reserved(d, params)?;
    // One fork of a parent at a time, from its fork call to its row (the
    // agent's fork of one conversation is made once at a time; two forks'
    // worktrees and names never race). (`locks` order: this, then the DB.)
    let _forking = d.sessions.lock(&format!("fork:{}", want.parent.id));
    // Not installed: say how to get it, rather than fail in the pane.
    if let Some(why) = install::missing(want.agent) {
        bail!("{why}");
    }
    want.worktree = plan_worktree(&want, &cfg)?;
    let mut made = Made {
        hooks: context::hooks_work(want.agent),
        ..Default::default()
    };
    // (Read as `session.get` reads it.)
    let mut rows = [serde_json::to_value(&want.parent)?];
    crate::usage::annotate(d, &mut rows, true);
    made.context_pct = rows[0]["usage"]["context_pct"].as_u64();
    if want.agent == Agent::OpenCode {
        made.opencode_id = Some(opencode_fork(
            &want.conversation,
            &want.parent.project_path,
        )?);
    }
    if let Some(w) = &want.worktree {
        // The branch is named after it: a session that took the name
        // meanwhile (not a fork: those see the reservation) stops it here,
        // before anything is made. (The guard ends with its block, before
        // any early return: `Reserved` takes the DB lock when dropped.)
        let taken = {
            let db = d.db();
            db.name_taken(&want.name)
        }?;
        if want.auto_name && taken {
            bail!("{} was taken meanwhile: fork again", want.name);
        }
        made.uncommitted = git::has_changes(&want.parent.project_path) == Some(true);
        git::ensure_excluded(&w.repo, &cfg.session.workspace_dir)?;
        git::worktree_add(&w.repo, &w.branch, &w.dir, Some(&w.start))?;
        // (A subfolder that isn't committed isn't in the new worktree.)
        if !w.project.is_dir() {
            discard(w);
            bail!(
                "{} isn't in the new worktree (it isn't committed): fork it in the same folder",
                w.project.display()
            );
        }
    }
    let id = rlord_core::types::new_id();
    let mut started = false;
    let r = finish(&d.db(), &id, &want, &made, &base, &mut started);
    // Failed before its pane started: nothing runs in what was made for
    // it, so none of it stays. (After that it is the fork's: kept, and the
    // error returned.)
    if r.is_err() && !started {
        // Its row (if it got that far) with its background rows and keys.
        let gone = {
            let db = d.db();
            db.delete_session(&id)
        };
        if let Err(e) = gone {
            tracing::warn!(session = %id, error = %e, "removing a fork that didn't start");
        }
        undo(&id, &want, &made, &base);
    }
    r
}

/// Remove what a fork that never started left outside the database, best
/// effort (no lock held: it may run OpenCode's client): its background
/// file, the worktree and branch made for it, and the fork OpenCode's
/// server made.
pub(crate) fn undo(id: &str, want: &Want, made: &Made, base: &std::path::Path) {
    let _ = std::fs::remove_dir_all(context::session_dir(base, id));
    if let Some(w) = &want.worktree {
        discard(w);
    }
    // (Only an id OpenCode's own answer gave this fork: never a session
    // rlord didn't make.)
    if let Some(oc) = &made.opencode_id {
        let gone = opencode_run(
            &opencode::delete_argv(oc),
            &want.parent.project_path,
            std::time::Duration::from_secs(opencode::CREATE_LIMIT_S),
        );
        if !matches!(gone, Ok((st, _)) if st.success()) {
            tracing::warn!(opencode_session = %oc, "a fork that didn't start is left in OpenCode");
        }
    }
}

/// Remove a worktree this fork made (and its branch), best effort: the
/// fork didn't start.
fn discard(w: &Worktree) {
    if let Err(e) = git::worktree_discard(&w.repo, &w.branch, &w.dir) {
        tracing::warn!(dir = %w.dir.display(), error = %e, "removing a fork's worktree");
    }
}

/// Where a worktree fork goes, when one was asked for. Outside the lock:
/// it asks git where the parent's checkout starts, so the fork works in
/// the same subfolder of the new worktree.
pub(crate) fn plan_worktree(want: &Want, cfg: &Config) -> Result<Option<Worktree>> {
    if !want.wants_worktree {
        return Ok(None);
    }
    let p = &want.parent;
    let not_repo = || anyhow::anyhow!("{} isn't in a git repo: fork it in the same folder", p.name);
    let repo = p.repo_path.clone().ok_or_else(not_repo)?;
    let top = git::repo_root(&p.project_path).ok_or_else(not_repo)?;
    let branch = want
        .branch
        .clone()
        .unwrap_or_else(|| git::sanitize_branch(&want.name));
    let dir = git::workspace_path(&repo, &cfg.session.workspace_dir, &branch);
    let project = dir.join(sub_folder(&top, &p.project_path));
    // (A repo with nothing committed has no commit to branch from.)
    let start = git::head_commit(&p.project_path)
        .map_err(|_| anyhow::anyhow!("nothing committed yet: fork it in the same folder"))?;
    Ok(Some(Worktree {
        repo,
        branch,
        dir,
        project,
        start,
    }))
}

/// `folder` relative to `top` (the checkout it is in); empty when it is
/// `top` itself or not under it.
fn sub_folder(top: &str, folder: &str) -> std::path::PathBuf {
    let real = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| p.into());
    real(folder)
        .strip_prefix(real(top))
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default()
}

/// OpenCode's fork of `conversation`, by its server when one answers, else
/// by OpenCode's own client run in the parent's folder `dir` (which starts
/// the server, as a launch does). Only an id OpenCode answered with is
/// this fork's: a call that failed or timed out is reported as such (the
/// user forks again), never guessed from the parent's forks, which may be
/// the user's own (and `undo` would delete it).
fn opencode_fork(conversation: &str, dir: &str) -> Result<String> {
    let svc = rlord_core::agents::opencode_service::Service::read(&opencode::service_file());
    let r = match svc.filter(|s| s.answers()) {
        Some(svc) => svc.fork(conversation),
        None => opencode_run(
            &opencode::fork_api_argv(conversation),
            dir,
            opencode::FORK_LIMIT,
        )
        .and_then(|(_, text)| opencode::fork_response(&text, None)),
    };
    r.map_err(|e| match e.to_string().as_str() {
        opencode::NOTHING_TO_FORK => anyhow::anyhow!("nothing to fork yet: {e}"),
        _ => anyhow::anyhow!(
            "{e} (if OpenCode made the fork anyway, it is in OpenCode's session list; \
             rlord didn't take it)"
        ),
    })
}

/// Run OpenCode's own client (`argv`, `opencode …`, as rlord finds it)
/// in folder `dir`, for rlord (`process::run_agent`: not one of its
/// sessions, output through rlord's folder).
fn opencode_run(
    argv: &[String],
    dir: &str,
    limit: std::time::Duration,
) -> Result<(std::process::ExitStatus, String)> {
    let mut argv = argv.to_vec();
    if let Some(bin) = install::resolved("opencode") {
        argv[0] = bin.to_string_lossy().into_owned();
    }
    rlord_core::process::run_agent(
        &crate::summary::output_dir(),
        "opencode",
        &argv,
        dir,
        None,
        limit,
    )
}

/// Read the parent and check the fork can be made (what is refused:
/// "Forking a session" in docs/architecture.md).
pub(crate) fn want(db: &Db, params: &Value) -> Result<Want> {
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
    // with the first message; a Codex thread with no rollout has none).
    if !conversation_on_disk(db, &parent)? {
        bail!(
            "nothing to fork yet: {}'s conversation isn't on disk (no {} written yet)",
            parent.name,
            if agent == Agent::Codex {
                "rollout"
            } else {
                "transcript"
            }
        );
    }
    let spec: Value = serde_json::from_str(&db.launch_spec(&parent.id)?).unwrap_or(json!({}));
    // (Checked as for a new session: the parent's own links are copied,
    // and the oldest of them dropped past the cap: `rows`.)
    // (The parent itself is linked anyway: left out before the check, so
    // it never counts towards `RELATED_MAX`.)
    let refs: Vec<String> = methods::ref_list(params, "related")
        .into_iter()
        .filter(|r| db.resolve_ref(r).map_or(true, |s| s.id != parent.id))
        .collect();
    let (note, related) = context::check(db, p_opt("context").as_deref(), &refs, None)?;
    let asked = p_opt("name");
    let auto_name = asked.is_none();
    let name = asked.unwrap_or_else(|| default_name(db, &parent.name));
    let wants_worktree = params["workspace"] == true;
    // (As `rlord fork --branch` needs `--worktree`.)
    if p_opt("branch").is_some() && !wants_worktree {
        bail!("a branch is for a new worktree: ask for the worktree too, or leave the branch out");
    }
    if wants_worktree {
        if !agent.forks_elsewhere() {
            bail!(
                "{} can't fork into a new worktree: its fork stays in its parent's folder. \
                 Fork it in the same folder",
                agent.label()
            );
        }
        if parent.repo_path.is_none() {
            bail!(
                "{} isn't in a git repo: fork it in the same folder",
                parent.name
            );
        }
    }
    Ok(Want {
        agent,
        conversation,
        spec,
        prompt: p_opt("prompt"),
        name,
        auto_name,
        description: p_opt("description"),
        model: p_opt("model"),
        note,
        related,
        wants_worktree,
        branch: p_opt("branch"),
        worktree: None,
        parent,
    })
}

/// `<parent>-fork`, `-fork-2`, …: the first name no session has and no
/// fork in the making reserved.
fn default_name(db: &Db, parent: &str) -> String {
    let now = now_ms();
    names::fork_name(parent, |n| name_unavailable(db, n, now))
}

/// What to say about the fork, by plain code: a live parent shares its
/// folder; a working one's last step isn't in the copy; a nearly full
/// context window; what a new worktree leaves behind. `p`: the parent as
/// read when the fork starts (`finish`), not when it was asked for.
pub(crate) fn warnings(want: &Want, p: &Session, made: &Made) -> Vec<String> {
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

/// The fork's background: the parent's rows to leave out (its own fork
/// note, when it is a fork itself: the fork gets its own; and its oldest
/// links past `LINKS_MAX`, never one named in `related`), and the rows to
/// add after the copied ones (the fork's note and its link to the parent,
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
    let mut seen: std::collections::HashSet<String> =
        copied_links.iter().map(|c| c.body.clone()).collect();
    // (`insert` is false for a link already there: dropped.)
    add.retain(|r| r.kind != "link" || seen.insert(r.body.clone()));
    let new_links = add.iter().filter(|r| r.kind == "link").count();
    let over = (copied_links.len() + new_links).saturating_sub(context::LINKS_MAX);
    let except = parent_rows
        .iter()
        .filter(|r| r.kind == "note" && r.source == "fork")
        .chain(
            copied_links
                .iter()
                .copied()
                .filter(|r| !want.related.contains(&r.body))
                .take(over),
        )
        .map(|r| r.id)
        .collect();
    (except, add)
}

/// The fork's launch spec: the parent's, without what is per session
/// (Remote Control's name and when it was turned on, Codex's daemon
/// host), keeping a Remote Control the user turned off. Its model is the
/// one asked for, else the parent's: `claude --resume` and `codex fork`
/// don't restore a conversation's model, so it is passed at the fork and
/// at every resume, as for the parent.
fn spec_of(want: &Want) -> Value {
    let mut spec = want.spec.clone();
    // The parent's Remote Control was on: the fork's is off, explicitly.
    // `--resume` of a copied conversation must never reconnect it under
    // the parent's.
    if spec["remote_control"]
        .as_str()
        .is_some_and(|n| !n.is_empty())
    {
        spec["remote_control_off"] = json!(true);
    }
    if let Some(o) = spec.as_object_mut() {
        for k in ["remote_control", "remote_control_at", "codex_host"] {
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
    // No model asked for: the parent's (Claude's `--resume`, `codex fork`
    // don't restore it). OpenCode's fork has its parent's already: only a
    // model asked for is set (`open_fork`).
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
            want.agent
                .open_fork(id, first, want.model.as_deref(), args)
                .ok_or_else(|| anyhow::anyhow!("{} has no server fork", want.agent.label()))
        }
    }
}

/// Insert and start the fork, under the lock.
/// `started`: set once its pane is up (an error after that leaves the
/// fork, and its worktree, in place).
pub(crate) fn finish(
    db: &Db,
    id: &str,
    want: &Want,
    made: &Made,
    base: &std::path::Path,
    started: &mut bool,
) -> Result<Value> {
    // Checked again: the parent may have gone while the lock was off, or
    // a session (not a fork: those see the reservation) taken the name it
    // chose. A worktree fork's branch was named after it: that stops it
    // (its worktree is removed); any other picks the next name.
    let parent = db.resolve_ref(&want.parent.id)?;
    let name = match (want.auto_name && db.name_taken(&want.name)?, &want.worktree) {
        (false, _) => want.name.clone(),
        (true, Some(_)) => bail!("{} was taken meanwhile: fork again", want.name),
        (true, None) => default_name(db, &parent.name),
    };
    let agent = want.agent;
    let now = now_ms();
    let (project, folder, repo, workspace, branch) = match &want.worktree {
        Some(w) => {
            let dir = w.dir.to_string_lossy().into_owned();
            (
                w.project.to_string_lossy().into_owned(),
                ForkFolder::Worktree(dir.clone(), w.branch.clone()),
                Some(w.repo.clone()),
                Some(dir),
                Some(w.branch.clone()),
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
    // Only what will be stored (`kept_rows`): its file, written before the
    // rows are, shows exactly what the database will hold.
    let add = context::kept_rows(&copied, add);
    // A fork always has background: at least its note and link.
    let mode = context::delivery(agent, made.hooks);
    let bg_file = context::file_path(base, id);
    let text = context::launch_file_text_rows(db, &name, &copied, &add, now);
    let carried = match context::write_file(base, id, &text) {
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
            id,
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
            kv: fork_keys(want, id),
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
    *started = true;
    // The pane is up: the fork exists. What follows is best effort (logged),
    // so the caller's own steps after a fork (the list, its keys, its tmux
    // profile) always run.
    if let Err(e) = after_start(db, want, &parent, id) {
        tracing::warn!(session = %id, error = %e, "copying a fork's group, tags, description");
    }
    tracing::info!(session = %id, parent = %parent.id, name = %name, agent = agent.name(), "forked session");
    let mut out = match db.resolve_ref(id).map(serde_json::to_value) {
        Ok(Ok(v)) => v,
        _ => {
            tracing::warn!(session = %id, "reading back a fork that started");
            json!({"id": id, "name": name, "forked_from": parent.id,
                   "forked_from_name": parent.name})
        }
    };
    out["warnings"] = json!(warnings(want, &parent, made));
    // (Said apart from `workspace_path`, which a fork in its parent's
    // worktree has too.)
    out["new_worktree"] = json!(want.worktree.is_some());
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
    Ok(())
}

/// kv key: where a fork works, as made: `worktree` (one of its own) or
/// `shared` (its parent's folder). Its note says so again at a re-fork.
pub(crate) fn folder_key(id: &str) -> String {
    format!("fork_folder:{id}")
}

/// How fork `id`'s conversation is found and made again, and where it
/// works: written with its row, before its pane starts (`Start::kv`).
pub(crate) fn fork_keys(want: &Want, id: &str) -> Vec<(String, String)> {
    let folder = if want.worktree.is_some() {
        "worktree"
    } else {
        "shared"
    };
    let mut kv = vec![(folder_key(id), folder.to_string())];
    if !want.agent.mints_id() {
        // Its thread is the one forked from the parent's (`bind_fork`).
        kv.push((methods::bind_fork_key(id), want.conversation.clone()));
    }
    if want.agent != Agent::OpenCode {
        // (OpenCode's fork exists before its pane starts.)
        kv.push((
            pending_key(id),
            json!({"parent_conversation": want.conversation}).to_string(),
        ));
    }
    kv
}

/// A restart of a fork whose conversation doesn't exist yet: the parent's
/// conversation to fork again (and the key cleared once the fork exists).
/// None: an ordinary restart (resume).
pub(crate) fn pending(db: &Db, s: &Session) -> Result<Option<String>> {
    let Some(v) = db.kv_get(&pending_key(&s.id))? else {
        return Ok(None);
    };
    if conversation_on_disk(db, s)? {
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

/// The fork's own conversation was seen at `transcript` (a hook's
/// `transcript_path`, a thread bound from Codex's index): once that is on
/// disk a restart resumes it, so `fork_pending` goes. (Nothing for a
/// session that isn't a fork.)
pub(crate) fn conversation_seen(db: &Db, id: &str, transcript: &str) -> Result<()> {
    if db.kv_get(&pending_key(id))?.is_some() && std::path::Path::new(transcript).exists() {
        db.kv_delete(&pending_key(id))?;
    }
    Ok(())
}

/// A fork made again at a restart branches from its parent's conversation
/// as it is now: its own note's new text (None: not a fork). Nothing is
/// stored here: the restart renders its background file with it
/// (`context::refresh_file_with`) before the pane starts, and stores it
/// (`renote`) only once the pane is up, so a restart that fails leaves the
/// database as it was. Where it works is what the fork recorded when it
/// was made (`folder_key`), not a comparison with its parent as it is now.
pub(crate) fn renewed_note(db: &Db, s: &Session, at: i64) -> Result<Option<String>> {
    let Some(pid) = s.forked_from.as_deref() else {
        return Ok(None);
    };
    let name = db
        .resolve_ref(pid)
        .ok()
        .map(|p| p.name)
        .or_else(|| s.forked_from_name.clone())
        .unwrap_or_else(|| pid.to_string());
    let own_worktree = db.kv_get(&folder_key(&s.id))?.as_deref() == Some("worktree");
    let folder = match (&s.workspace_path, &s.branch) {
        (Some(w), Some(b)) if own_worktree => ForkFolder::Worktree(w.clone(), b.clone()),
        _ => ForkFolder::Shared,
    };
    Ok(Some(context::fork_note(&name, pid, at, &folder)))
}

/// Store a re-forked fork's note (`renewed_note`), unseen: after its pane
/// is up. The restart then marks seen only what that start handed over.
pub(crate) fn renote(db: &Db, s: &Session, body: &str, at: i64) -> Result<()> {
    db.context_renote(&s.id, "fork", body, at)?;
    Ok(())
}

/// A fork about to start again with a new fork of `parent`
/// (`refork_argv`): an agent that picks its own id makes a new thread,
/// bound as at the fork (`bind_fork`). Written before the pane starts, so
/// it can't be lost after; while the old thread is still bound it changes
/// nothing (only an unbound session is bound). (Claude keeps its own id:
/// nothing to do.)
pub(crate) fn refork_bind_key(db: &Db, s: &Session, parent: &str) -> Result<()> {
    if Agent::parse(&s.agent).is_some_and(|a| !a.mints_id()) {
        db.kv_set(&methods::bind_fork_key(&s.id), parent)?;
    }
    Ok(())
}

/// A fork's pane started again with a new fork (`refork_bind_key`): the
/// thread bound before (its rollout never written) goes, so the new one is
/// bound. Once the pane is up.
pub(crate) fn reforked(db: &Db, s: &Session) -> Result<()> {
    if Agent::parse(&s.agent).is_some_and(|a| !a.mints_id()) {
        db.clear_agent_session(&s.id)?;
    }
    Ok(())
}

/// Whether session `s`'s conversation is on disk: what a fork copies and a
/// resume continues. Claude: its transcript (the one its hooks named, else
/// where it files its id); Codex: a bound thread whose rollout is there
/// (bound alone isn't enough: `codex resume` / `codex fork` find no
/// conversation); OpenCode: its session (its server makes it at once).
/// The one check, for a fork's parent (`want`) and a fork's restart
/// (`pending`).
pub(crate) fn conversation_on_disk(db: &Db, s: &Session) -> Result<bool> {
    let Some(sid) = s.agent_session.as_deref() else {
        return Ok(false);
    };
    let on_disk = |t: String| std::path::Path::new(&t).exists();
    Ok(match Agent::parse(&s.agent) {
        Some(Agent::Claude) => match db.transcript_source(&s.id)? {
            Some(t) => on_disk(t),
            None => claude::transcript_path(&s.project_path, sid).exists(),
        },
        Some(Agent::Codex) => db.transcript_source(&s.id)?.is_some_and(on_disk),
        _ => true,
    })
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
        // A conversation is on disk (a test that wants none says so).
        if conv.is_some() && std::path::Path::new(dir).is_dir() {
            let t = std::path::Path::new(dir).join(format!("{id}-conversation.jsonl"));
            std::fs::write(&t, "{}\n").unwrap();
            db.upsert_transcript_source(id, t.to_str().unwrap())
                .unwrap();
        }
    }

    /// A worktree fork works in the parent's subfolder of the new
    /// worktree (and at its top when the parent does).
    #[test]
    fn a_worktree_fork_keeps_the_parents_subfolder() {
        let dir = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap().join("repo");
        std::fs::create_dir_all(repo.join("app/web")).unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        let r = repo.to_str().unwrap();
        let db = Db::open_in_memory().unwrap();
        for (id, folder) in [("sub", repo.join("app/web")), ("top", repo.clone())] {
            db.insert_session(&NewSession {
                id,
                name: id,
                agent: "codex",
                agent_session: Some("t-1"),
                project_path: folder.to_str().unwrap(),
                tmux_session: None,
                launch_spec: "{}",
                created_at: 1,
                repo_path: Some(r),
                workspace_path: None,
                branch: None,
                role: "",
            })
            .unwrap();
            // (Its rollout, outside the repo.)
            let t = dir.path().join(format!("{id}-rollout.jsonl"));
            std::fs::write(&t, "{}\n").unwrap();
            db.upsert_transcript_source(id, t.to_str().unwrap())
                .unwrap();
        }
        let cfg = Config::default();
        let plan = |id: &str| {
            let w = want(&db, &json!({"ref": id, "workspace": true})).unwrap();
            plan_worktree(&w, &cfg)
        };
        // Nothing committed yet: said plainly (not git's own words).
        let e = plan("sub").unwrap_err().to_string();
        assert_eq!(e, "nothing committed yet: fork it in the same folder");
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "one",
        ]);
        let plan = |id: &str| plan(id).unwrap().unwrap();
        let w = plan("sub");
        assert_eq!(w.start.len(), 40);
        assert_eq!(w.branch, "sub-fork");
        assert_eq!(
            w.dir,
            git::workspace_path(r, &cfg.session.workspace_dir, "sub-fork")
        );
        assert_eq!(w.project, w.dir.join("app/web"));
        let w = plan("top");
        assert_eq!(w.project, w.dir);
        // Not asked for: none.
        let w = want(&db, &json!({"ref": "top"})).unwrap();
        assert_eq!(plan_worktree(&w, &cfg).unwrap(), None);
    }

    fn err(db: &Db, params: Value) -> String {
        want(db, &params).unwrap_err().to_string()
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
        assert!(want(&db, &json!({"ref": "b"})).is_ok());
        // A Codex thread bound but with no rollout on disk: said plainly.
        mk(&db, "cx", "codex", Some("t-cx"), here, "");
        db.upsert_transcript_source("cx", dir.path().join("none.jsonl").to_str().unwrap())
            .unwrap();
        assert_eq!(
            err(&db, json!({"ref": "cx"})),
            "nothing to fork yet: n-cx's conversation isn't on disk (no rollout written yet)"
        );
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
        // A branch without a worktree: refused, as the CLI does.
        assert!(err(&db, json!({"ref": "c", "branch": "b"})).contains("new worktree"));
        // The overlord is no related session.
        assert!(err(&db, json!({"ref": "c", "related": ["o"]})).contains("overlord"));
    }

    #[test]
    fn a_fork_is_named_after_its_parent_and_says_what_it_shares() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();
        let db = Db::open_in_memory().unwrap();
        mk(&db, "p", "codex", Some("t-p"), here, "");
        let w = want(&db, &json!({"ref": "p"})).unwrap();
        assert_eq!(w.name, "n-p-fork");
        mk(&db, "f", "codex", None, here, "");
        db.rename("f", "n-p-fork").unwrap();
        let w = want(&db, &json!({"ref": "p"})).unwrap();
        assert_eq!(w.name, "n-p-fork-2");
        let w = want(
            &db,
            &json!({"ref": "p", "name": " mine ", "prompt": " try a queue ", "model": "m-2"}),
        )
        .unwrap();
        assert_eq!(
            (w.name.as_str(), w.prompt.as_deref(), w.model.as_deref()),
            ("mine", Some("try a queue"), Some("m-2"))
        );
        // Its launch spec: the parent's, less what is per session, keeping
        // a Remote Control the user turned off.
        let mut w = w;
        w.spec["remote_control_at"] = json!(5);
        w.spec["remote_control_off"] = json!(true);
        let spec = spec_of(&w);
        assert_eq!(spec["model"], "m-2");
        assert_eq!(spec["prompt"], "try a queue");
        assert!(spec.get("remote_control").is_none() && spec.get("codex_host").is_none());
        assert!(spec.get("remote_control_at").is_none());
        assert_eq!(spec["remote_control_off"], true);
        // The parent's Remote Control on (its name in the spec): the
        // fork's is off, so a Claude fork never reconnects under it.
        w.spec["remote_control_off"] = Value::Null;
        w.spec["remote_control"] = json!("rlord · api");
        let spec = spec_of(&w);
        assert!(spec.get("remote_control").is_none());
        assert_eq!(spec["remote_control_off"], true);
        let mut cl = w.clone();
        cl.agent = Agent::Claude;
        let args = spec_args(Agent::Claude, &spec_of(&cl));
        assert!(!args.iter().any(|a| a == "--remote-control"));
        assert!(
            args.iter()
                .any(|a| a.contains("\"disableRemoteControl\":true")),
            "{args:?}"
        );
        // Never on: nothing said.
        w.spec["remote_control"] = Value::Null;
        assert!(spec_of(&w)["remote_control_off"].is_null());
        // No model asked for: the parent's (stored, passed at each start).
        w.model = None;
        assert_eq!(spec_of(&w)["model"], "m-1");

        // Warnings, by plain code.
        let mut w = want(&db, &json!({"ref": "p"})).unwrap();
        w.parent.status = Status::Working;
        let made = Made {
            context_pct: Some(82),
            ..Default::default()
        };
        let says = warnings(&w, &w.parent, &made).join("\n");
        assert!(says.contains("last finished step"), "{says}");
        assert!(says.contains("shares its folder"), "{says}");
        assert!(says.contains("82% full"), "{says}");
        w.parent.status = Status::Stopped;
        assert!(warnings(&w, &w.parent, &Made::default()).is_empty());
        w.parent.status = Status::Idle;
        w.parent.archived_at = Some(5);
        assert!(warnings(&w, &w.parent, &Made::default()).is_empty());
        // Said of the parent as read when the fork starts, not as asked.
        let mut now = w.parent.clone();
        now.status = Status::Working;
        assert!(warnings(&w, &now, &Made::default())
            .join("\n")
            .contains("last finished step"));
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
        // The parent is a fork itself: its own fork note isn't the fork's.
        db.context_add("p", 41, "note", "Forked from session \"g\"", "fork")
            .unwrap();
        mk(&db, "x", "claude", None, here, "");
        // r00, the oldest link, is asked for again: it is never cut.
        let w = want(
            &db,
            &json!({"ref": "p", "context": "the user wants a queue", "related": ["x", "r00", "p"]}),
        )
        .unwrap();
        assert_eq!(w.related, ["x", "r00"], "not the parent itself");
        // The parent never counts towards `RELATED_MAX`: eight others and
        // the parent (by name) are accepted.
        let eight: Vec<String> = (1..=8).map(|i| format!("r{i:02}")).collect();
        let mut refs = eight.clone();
        refs.push("n-p".into());
        let w8 = want(&db, &json!({"ref": "p", "related": refs})).unwrap();
        assert_eq!(w8.related, eight);
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
        // The parent's fork note, then (16 copied + 2 new links) the two
        // oldest copied links not asked for: r01 and r02, not r00.
        let id_of = |body: &str| {
            parent_rows
                .iter()
                .find(|r| r.body.starts_with(body))
                .unwrap()
                .id
        };
        assert_eq!(except, [id_of("Forked from"), id_of("r01"), id_of("r02")]);

        // Copied as the fork starts.
        mk(&db, "f", "claude", Some("c-f"), here, "");
        db.context_copy("p", "f", &except).unwrap();
        context::store_rows(&db, "f", &add, 50).unwrap();
        let links = db.context_links("f").unwrap();
        assert_eq!(links.len(), context::LINKS_MAX);
        assert!(links.contains(&"p".to_string()) && links.contains(&"r00".to_string()));
        assert!(!links.contains(&"r01".to_string()) && !links.contains(&"r02".to_string()));
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
        let copied: Vec<_> = parent_rows
            .iter()
            .filter(|r| !except.contains(&r.id))
            .cloned()
            .collect();
        let text = context::launch_file_text_rows(&db, "n-p-fork", &copied, &add, 50);
        assert!(!text.contains("\"g\""), "{text}");
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

    /// The file a fork starts with shows exactly the rows stored for it:
    /// a new row the same as a copied one (or an earlier new one) is
    /// neither stored nor shown.
    #[test]
    fn a_forks_file_shows_what_is_stored() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();
        let db = Db::open_in_memory().unwrap();
        mk(&db, "p", "claude", Some("c-p"), here, "");
        mk(&db, "x", "claude", None, here, "");
        db.context_add("p", 1, "note", "use the staging DB", "overlord")
            .unwrap();
        db.context_add("p", 2, "link", "x", "overlord").unwrap();
        db.context_mark_all_seen("p", 3).unwrap();
        let copied = db.context_rows("p").unwrap();
        let new = |kind, body: &str| NewRow {
            kind,
            body: body.into(),
            source: "overlord",
        };
        let add = vec![
            new("note", "use the staging DB"),
            new("note", "and a queue"),
            new("note", "and a queue"),
            new("link", "x"),
        ];
        let kept = context::kept_rows(&copied, add.clone());
        assert_eq!(kept, [new("note", "and a queue")]);
        // Stored as at the start (every new row offered): the same.
        mk(&db, "f", "claude", Some("c-f"), here, "");
        db.context_copy("p", "f", &[]).unwrap();
        context::store_rows(&db, "f", &add, 50).unwrap();
        let stored: Vec<(String, String)> = db
            .context_rows("f")
            .unwrap()
            .into_iter()
            .map(|r| (r.kind, r.body))
            .collect();
        let shown: Vec<(String, String)> = copied
            .iter()
            .map(|r| (r.kind.clone(), r.body.clone()))
            .chain(kept.iter().map(|r| (r.kind.to_string(), r.body.clone())))
            .collect();
        assert_eq!(stored, shown);
        let text = context::launch_file_text_rows(&db, "f", &copied, &add, 50);
        assert_eq!(text.matches("and a queue").count(), 1, "{text}");
        assert_eq!(text.matches("use the staging DB").count(), 1, "{text}");
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
        let w = want(&db, &json!({"ref": "p"})).unwrap();
        after_start(&db, &w, &w.parent, "f").unwrap();
        // (As `start` writes them, with the row.)
        for (k, v) in fork_keys(&w, "f") {
            db.kv_set(&k, &v).unwrap();
        }
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
        // Bound, but no rollout on disk yet: `codex resume` would find
        // nothing, so it is forked again.
        db.set_agent_session("f", "t-f", "hook").unwrap();
        let rollout = dir.path().join("rollout-t-f.jsonl");
        db.upsert_transcript_source("f", rollout.to_str().unwrap())
            .unwrap();
        let f = db.resolve_ref("f").unwrap();
        assert_eq!(pending(&db, &f).unwrap().as_deref(), Some("t-p"));
        // Its rollout written: resumed from now on.
        std::fs::write(&rollout, "{}\n").unwrap();
        assert_eq!(pending(&db, &f).unwrap(), None);
        assert_eq!(db.kv_get(&pending_key("f")).unwrap(), None, "cleared");
        // OpenCode's fork exists before its pane: nothing pending; a
        // description given wins.
        mk(&db, "g", "opencode", Some("ses_2"), here, "");
        let w = want(&db, &json!({"ref": "q", "description": "Try a queue"})).unwrap();
        after_start(&db, &w, &w.parent, "g").unwrap();
        for (k, v) in fork_keys(&w, "g") {
            db.kv_set(&k, &v).unwrap();
        }
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
        let w = want(&db, &json!({"ref": "cl"})).unwrap();
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
        // No model asked for: the parent's (`--resume` doesn't restore
        // it), and no Remote Control.
        assert!(l.argv.windows(2).any(|a| a == ["--model", "m-1"]));
        let w = want(&db, &json!({"ref": "cl", "model": "opus"})).unwrap();
        let l = launch(&w, &Made::default(), here, None, &args(&w)).unwrap();
        assert!(l.argv.windows(2).any(|a| a == ["--model", "opus"]));
        assert!(!l.argv.iter().any(|a| a == "--remote-control"));

        let w = want(&db, &json!({"ref": "cx"})).unwrap();
        let l = launch(&w, &Made::default(), here, None, &args(&w)).unwrap();
        assert_eq!(&l.argv[..3], ["codex", "fork", "t-1"]);
        // Always in the pane, whatever the parent's host.
        assert!(l.argv.contains(&"--no-daemon".to_string()), "{:?}", l.argv);
        assert_eq!(l.session_id, None);

        let w = want(&db, &json!({"ref": "oc", "model": "a/b"})).unwrap();
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

    /// A Codex fork forked again at a restart makes a new thread: the one
    /// bound before goes, and the new one is found as at the fork. Claude
    /// keeps its own id.
    #[test]
    fn a_codex_refork_binds_its_new_thread() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();
        let db = Db::open_in_memory().unwrap();
        mk(&db, "cx", "codex", Some("t-stale"), here, "");
        db.upsert_transcript_source("cx", "/r/rollout-t-stale.jsonl")
            .unwrap();
        mk(&db, "cl", "claude", Some("c-new"), here, "");
        let cx = db.resolve_ref("cx").unwrap();
        // Before the pane: the key only; the old thread stays bound.
        refork_bind_key(&db, &cx, "t-parent").unwrap();
        assert_eq!(
            db.resolve_ref("cx").unwrap().agent_session.as_deref(),
            Some("t-stale")
        );
        // Once the pane is up: the old thread goes.
        reforked(&db, &cx).unwrap();
        let s = db.resolve_ref("cx").unwrap();
        assert_eq!(s.agent_session, None);
        assert_eq!(db.transcript_source("cx").unwrap(), None);
        assert_eq!(
            db.kv_get(&methods::bind_fork_key("cx")).unwrap().as_deref(),
            Some("t-parent")
        );
        let cl = db.resolve_ref("cl").unwrap();
        refork_bind_key(&db, &cl, "c-parent").unwrap();
        reforked(&db, &cl).unwrap();
        assert_eq!(
            db.resolve_ref("cl").unwrap().agent_session.as_deref(),
            Some("c-new")
        );
        assert_eq!(db.kv_get(&methods::bind_fork_key("cl")).unwrap(), None);
    }

    /// A fork made again at a restart: its own note names the new branch
    /// point and is handed over again; the rest of its background stays.
    /// Its file shows the new note before anything is stored; the database
    /// takes it only when `renote` runs (once the pane is up).
    #[test]
    fn a_refork_renews_its_note() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();
        let db = Db::open_in_memory().unwrap();
        mk(&db, "p", "codex", Some("t-p"), here, "");
        mk(&db, "f", "codex", None, here, "");
        db.set_forked_from("f", "p").unwrap();
        let old = context::fork_note("n-p", "p", 0, &ForkFolder::Shared);
        db.context_add("f", 0, "note", &old, "fork").unwrap();
        db.context_add("f", 0, "note", "use the staging DB", "overlord")
            .unwrap();
        db.context_mark_all_seen("f", 5).unwrap();
        let at = 1_800_000_000_000;
        let f = db.resolve_ref("f").unwrap();
        let body = renewed_note(&db, &f, at).unwrap().unwrap();
        assert_eq!(
            body,
            context::fork_note("n-p", "p", at, &ForkFolder::Shared)
        );
        // Rendered in memory: the file has it, the database doesn't yet.
        let renewed = context::Renewed {
            source: "fork",
            body: &body,
            at,
        };
        let text = context::file_text_with(&db, &f, Some(&renewed))
            .unwrap()
            .unwrap();
        assert!(text.contains(&body) && !text.contains(&old));
        let before = db.context_rows("f").unwrap();
        assert_eq!(
            (before[0].body.as_str(), before[0].seen_at),
            (old.as_str(), Some(5))
        );
        // The pane is up: stored, unseen.
        renote(&db, &f, &body, at).unwrap();
        let rows = db.context_rows("f").unwrap();
        assert_eq!(rows[0].body, body);
        assert_eq!((rows[0].at, rows[0].seen_at), (at, None));
        assert_eq!(
            (rows[1].body.as_str(), rows[1].seen_at),
            ("use the staging DB", Some(5))
        );
        // Not a fork: nothing to renew.
        assert_eq!(
            renewed_note(&db, &db.resolve_ref("p").unwrap(), at).unwrap(),
            None
        );
        // Where it works is what it recorded when made: both of these are
        // in a worktree, only one made it.
        for (id, folder) in [("own", "worktree"), ("inherited", "shared")] {
            db.insert_session(&NewSession {
                id,
                name: id,
                agent: "codex",
                agent_session: None,
                project_path: "/r/.worktrees/x",
                tmux_session: None,
                launch_spec: "{}",
                created_at: 1,
                repo_path: Some("/r"),
                workspace_path: Some("/r/.worktrees/x"),
                branch: Some("x"),
                role: "",
            })
            .unwrap();
            db.set_forked_from(id, "p").unwrap();
            db.kv_set(&folder_key(id), folder).unwrap();
        }
        let note = |id: &str| {
            renewed_note(&db, &db.resolve_ref(id).unwrap(), at)
                .unwrap()
                .unwrap()
        };
        assert!(note("own").contains("own worktree, /r/.worktrees/x (branch x)"));
        assert!(note("inherited").contains("same folder"));
    }

    /// A fork that never started leaves nothing behind outside the
    /// database: its background folder, its worktree and branch.
    #[test]
    fn a_fork_that_never_started_is_undone() {
        let dir = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "one",
        ]);
        let r = repo.to_str().unwrap();
        let db = Db::open_in_memory().unwrap();
        mk(&db, "p", "codex", Some("t-p"), r, "");
        let mut w = want(&db, &json!({"ref": "p"})).unwrap();
        let wt = Worktree {
            repo: r.into(),
            branch: "p-fork".into(),
            dir: repo.join(".worktrees/p-fork"),
            project: repo.join(".worktrees/p-fork"),
            start: git::head_commit(r).unwrap(),
        };
        git::worktree_add(r, &wt.branch, &wt.dir, Some(&wt.start)).unwrap();
        w.worktree = Some(wt.clone());
        let base = dir.path().join("data");
        context::write_file(&base, "01new", "background").unwrap();
        undo("01new", &w, &Made::default(), &base);
        assert!(!context::session_dir(&base, "01new").exists());
        assert!(!wt.dir.exists() && !git::branch_exists(r, "p-fork"));
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
            [
                "codex",
                "fork",
                "t-p",
                "--no-alt-screen",
                "-c",
                codex::FORK_CWD
            ]
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
        db.kv_set(
            &pending_key("01fork"),
            r#"{"parent_conversation":"t-parent"}"#,
        )
        .unwrap();
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
        // Its rollout isn't on disk yet: still a fork to make again.
        assert!(db.kv_get(&pending_key("01fork")).unwrap().is_some());
        // Once a hook names a rollout that is there, no longer.
        let dir = tempfile::tempdir().unwrap();
        let rollout = dir.path().join("rollout-x-t-fork.jsonl");
        std::fs::write(&rollout, "{}\n").unwrap();
        methods::hook_emit_with(
            &db,
            &json!({"instance": "01fork", "agent": "codex", "event": "UserPromptSubmit",
                    "payload": {"session_id": "t-fork", "hook_event_name": "UserPromptSubmit",
                                "transcript_path": rollout.to_str().unwrap()}}),
            &|_, _| rlord_core::agents::Indexed::User,
        )
        .unwrap();
        assert_eq!(db.kv_get(&pending_key("01fork")).unwrap(), None);
    }

    /// A default name is the fork's while it is made: a second fork at the
    /// same time takes the next one. A stale reservation doesn't count.
    #[test]
    fn a_default_name_is_reserved_while_the_fork_is_made() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_str().unwrap();
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        mk(&d.db(), "p", "codex", Some("t-p"), here, "");
        // Checking alone reserves nothing.
        let looked = want(&d.db(), &json!({"ref": "p"})).unwrap();
        assert_eq!(looked.name, "n-p-fork");
        let (first, held) = want_reserved(&d, &json!({"ref": "p"})).unwrap();
        assert!(first.auto_name);
        let (second, _held2) = want_reserved(&d, &json!({"ref": "p"})).unwrap();
        assert_eq!(
            (first.name.as_str(), second.name.as_str()),
            ("n-p-fork", "n-p-fork-2")
        );
        // Asked for by name: not reserved, not changed.
        let (named, _none) = want_reserved(&d, &json!({"ref": "p", "name": "n-p-fork"})).unwrap();
        assert!(!named.auto_name);
        assert_eq!(named.name, "n-p-fork");
        // Released when the fork is done (`Reserved`), or older than it
        // may hold: free again.
        drop(held);
        d.db()
            .kv_set(
                &reserve_key("n-p-fork-2"),
                &(now_ms() - RESERVED_MS - 1).to_string(),
            )
            .unwrap();
        assert!(!name_unavailable(&d.db(), "n-p-fork-2", now_ms()));
        assert_eq!(
            want(&d.db(), &json!({"ref": "p"})).unwrap().name,
            "n-p-fork"
        );
    }
}
