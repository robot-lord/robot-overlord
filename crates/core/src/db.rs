use crate::agents::transcript::{CursorState, STATS_PARSE_VERSION};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::types::{Session, Status, Subagent};

/// Embedded, ordered migrations. `user_version` tracks how many have run.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_repos_workspaces.sql"),
    include_str!("../migrations/0003_description_notes.sql"),
    include_str!("../migrations/0004_subagents.sql"),
    include_str!("../migrations/0005_overlord.sql"),
    include_str!("../migrations/0006_transcript_cursors.sql"),
    include_str!("../migrations/0007_summary.sql"),
    include_str!("../migrations/0008_usage.sql"),
    include_str!("../migrations/0009_rlord_msg.sql"),
    include_str!("../migrations/0010_session_context.sql"),
    include_str!("../migrations/0011_forked_from.sql"),
    include_str!("../migrations/0012_events_kind.sql"),
];

/// kv: the id of the active overlord (see `Db::overlord`).
pub const OVERLORD_ACTIVE: &str = "overlord_active";

const SESSION_COLS: &str = "id, name, name_locked, agent, agent_session, agent_session_trust, \
     project_path, tmux_session, status, group_path, pinned, created_at, archived_at, \
     last_user_msg_at, last_agent_msg_at, last_state_change_at, \
     repo_path, workspace_path, branch, description, notes, launch_spec, role, summary, summary_at, \
     last_rlord_msg_at, forked_from, \
     (SELECT p.name FROM sessions p WHERE p.id = sessions.forked_from)";

pub struct NewSession<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub agent: &'a str,
    pub agent_session: Option<&'a str>,
    pub project_path: &'a str,
    /// None until it first runs (imported sessions start stopped).
    pub tmux_session: Option<&'a str>,
    pub launch_spec: &'a str,
    pub created_at: i64,
    pub repo_path: Option<&'a str>,
    pub workspace_path: Option<&'a str>,
    pub branch: Option<&'a str>,
    /// '' for ordinary sessions, 'overlord' for the management session.
    pub role: &'a str,
}

#[derive(Debug, serde::Serialize)]
pub struct RepoRow {
    pub path: String,
    pub last_used_at: i64,
    pub use_count: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct EventRow {
    pub seq: i64,
    pub ts: i64,
    pub kind: String,
    pub payload: serde_json::Value,
}

/// A `session_context` row: a note the overlord wrote for a session, or a
/// link to a related session (`body` its id).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ContextRow {
    pub id: i64,
    pub at: i64,
    /// `note` or `link`.
    pub kind: String,
    pub body: String,
    pub source: String,
    /// When a hook pushed it to the agent (None: not yet).
    pub seen_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArchivedMode {
    #[default]
    Live,
    Only,
    All,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortMode {
    /// Most recently updated first (the default): the latest message either
    /// way (a resume or state change alone doesn't count).
    #[default]
    #[serde(alias = "updated")]
    Activity,
    /// Most recent user prompt first.
    User,
    /// Most recent status change first.
    State,
    Created,
    Name,
    /// Most used first, by what the Usage column shows (turns, tokens or
    /// context %; `SessionQuery::usage`). Not SQL: the daemon sorts the
    /// rows once their usage is read (`rlord_daemon::usage`).
    Usage,
}

impl SortMode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "updated" | "activity" | "agent" | "last-message" | "last_message" | "message" => {
                SortMode::Activity
            }
            "user" | "you" | "last-prompt" | "last_prompt" | "prompt" => SortMode::User,
            "state" | "status" | "changed" | "status-change" | "status_change" => SortMode::State,
            "created" | "age" => SortMode::Created,
            "name" => SortMode::Name,
            "usage" | "use" | "used" => SortMode::Usage,
            _ => return None,
        })
    }
    /// What the user reads (toolbar, launcher): which time it sorts by.
    pub fn title(self) -> &'static str {
        match self {
            SortMode::Activity => "last message",
            SortMode::User => "last prompt",
            SortMode::State => "status change",
            SortMode::Created => "created",
            SortMode::Name => "name",
            SortMode::Usage => "usage",
        }
    }

    /// The value in views, queries and the API (stable; `parse` reads it).
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Activity => "updated",
            SortMode::User => "user",
            SortMode::State => "state",
            SortMode::Created => "created",
            SortMode::Name => "name",
            SortMode::Usage => "usage",
        }
    }
    pub fn next(self) -> Self {
        match self {
            SortMode::Activity => SortMode::User,
            SortMode::User => SortMode::State,
            SortMode::State => SortMode::Created,
            SortMode::Created => SortMode::Name,
            SortMode::Name => SortMode::Usage,
            SortMode::Usage => SortMode::Activity,
        }
    }
}

/// The one query object shared by CLI, TUI, and (later) MCP.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct SessionQuery {
    pub archived: ArchivedMode,
    pub status: Vec<String>,
    pub agent: Option<String>,
    pub tag: Option<String>,
    /// Substring match on project_path.
    pub project: Option<String>,
    /// Substring match on name.
    pub name: Option<String>,
    /// Prefix match on group_path.
    pub group: Option<String>,
    /// Free text: matches name, project, group, any tag, description,
    /// notes or summary.
    pub search: Option<String>,
    pub sort: SortMode,
    pub reverse: bool,
    /// What the usage sort sorts by (and the rows' Usage column shows);
    /// None: the configured `usage`.
    pub usage: Option<crate::usage::UsageMetric>,
    /// Include the overlord session (excluded by default: clients show it
    /// as a special row, not as one of the fleet).
    pub include_overlord: bool,
}

impl SessionQuery {
    /// Apply a rich search string: `tag:wip status:waiting,idle proj:wait2
    /// agent:claude group:work sort:user free words…`. Tokens set the typed
    /// filters; everything else becomes the free-text `search`.
    pub fn apply_search_text(&mut self, text: &str) {
        let mut free: Vec<&str> = Vec::new();
        for tok in text.split_whitespace() {
            match tok.split_once(':') {
                Some(("tag" | "t", v)) if !v.is_empty() => self.tag = Some(v.to_string()),
                Some(("status" | "st" | "s", v)) if !v.is_empty() => {
                    self.status = v.split(',').map(normalize_status).collect();
                }
                Some(("agent" | "a", v)) if !v.is_empty() => self.agent = Some(v.to_string()),
                Some(("proj" | "project" | "p", v)) if !v.is_empty() => {
                    self.project = Some(v.to_string())
                }
                Some(("group" | "g", v)) if !v.is_empty() => self.group = Some(v.to_string()),
                Some(("sort" | "o", v)) if !v.is_empty() => {
                    if let Some(m) = SortMode::parse(v) {
                        self.sort = m;
                    }
                }
                _ => free.push(tok),
            }
        }
        self.search = if free.is_empty() {
            None
        } else {
            Some(free.join(" "))
        };
    }
}

/// Accept friendly status spellings: the labels people see (`needs you`,
/// `your turn`; one word in a filter: `needs`, `needs-you`, `your-turn`,
/// `turn`) and older words (`waiting` → `waiting_input`).
pub fn normalize_status(s: &str) -> String {
    match s.trim().to_lowercase().replace(['_', ' '], "-").as_str() {
        "waiting" | "wait" | "input" | "waiting-input" | "needs" | "needs-you" | "needsyou"
        | "blocked" => "waiting_input".to_string(),
        "your-turn" | "yourturn" | "turn" | "yours" => "idle".to_string(),
        "run" | "busy" => "working".to_string(),
        // Not stored: laid over `idle` by the daemon (background.rs).
        "bg" | "background" | "background-work" | "waiting-background" => "background".to_string(),
        other => other.to_string(),
    }
}

pub struct Db {
    conn: Connection,
}

/// A live session's transcript, for background-work tracking
/// (`Db::live_transcripts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTranscript {
    pub id: String,
    pub path: String,
    /// When its agent process last started (ms; 0: never seen).
    pub started_at: i64,
    /// Stopped or failed.
    pub stopped: bool,
    /// Its last status change (ms).
    pub changed_at: i64,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        // rlord's data folder and its DB are the user's alone (SQLite gives
        // the -wal and -shm files the DB file's mode).
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            crate::private::dir(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        crate::private::touch(path).with_context(|| format!("creating {}", path.display()))?;
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let mut db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        let applied: usize =
            self.conn
                .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("applying migration {}", i + 1))?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
            tracing::info!(migration = i + 1, "applied schema migration");
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    // ---- sessions ----

    pub fn insert_session(&self, s: &NewSession) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (id, name, agent, agent_session, project_path, tmux_session, \
                                   status, launch_spec, created_at, repo_path, workspace_path, \
                                   branch, role) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'starting', ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                s.id,
                s.name,
                s.agent,
                s.agent_session,
                s.project_path,
                s.tmux_session,
                s.launch_spec,
                s.created_at,
                s.repo_path,
                s.workspace_path,
                s.branch,
                s.role
            ],
        )?;
        Ok(())
    }

    /// Resolve a user-supplied ref: exact id, exact name, unique id prefix,
    /// then unique name prefix.
    pub fn resolve_ref(&self, r: &str) -> Result<Session> {
        if let Some(s) = self.get_session_where("id = ?1", r)? {
            return Ok(s);
        }
        // Its tmux session name (`rlord-brave-otter`), e.g. from inside it.
        if let Some(s) = self.get_session_where("tmux_session = ?1", r)? {
            return Ok(s);
        }
        let by_name: Vec<Session> = self.query_sessions("name = ?1", r)?;
        // Every overlord is named "overlord": the name means the active one.
        if by_name.len() > 1 && by_name.iter().all(|s| s.role == "overlord") {
            if let Some(o) = self.overlord()? {
                return Ok(o);
            }
        }
        match by_name.len() {
            1 => return Ok(by_name.into_iter().next().unwrap()),
            n if n > 1 => bail!("name '{r}' is ambiguous ({n} sessions); use the id"),
            _ => {}
        }
        let by_prefix: Vec<Session> = self.query_sessions("id LIKE ?1 || '%'", r)?;
        match by_prefix.len() {
            1 => return Ok(by_prefix.into_iter().next().unwrap()),
            0 => {}
            n => bail!("id prefix '{r}' is ambiguous ({n} sessions)"),
        }
        let by_name_prefix: Vec<Session> = self.query_sessions("name LIKE ?1 || '%'", r)?;
        match by_name_prefix.len() {
            1 => Ok(by_name_prefix.into_iter().next().unwrap()),
            0 => bail!("no session matches '{r}'"),
            n => bail!("name prefix '{r}' is ambiguous ({n} sessions)"),
        }
    }

    fn get_session_where(&self, cond: &str, arg: &str) -> Result<Option<Session>> {
        Ok(self.query_sessions(cond, arg)?.into_iter().next())
    }

    fn query_sessions(&self, cond: &str, arg: &str) -> Result<Vec<Session>> {
        let sql = format!("SELECT {SESSION_COLS} FROM sessions WHERE {cond}");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([arg], row_to_session)?;
        let mut sessions: Vec<Session> = rows.collect::<std::result::Result<_, _>>()?;
        self.attach_tags(&mut sessions)?;
        self.attach_agent_counts(&mut sessions)?;
        Ok(sessions)
    }

    /// The session (live or archived) holding an agent conversation id.
    pub fn session_by_agent_session(&self, agent_session: &str) -> Result<Option<Session>> {
        self.get_session_where("agent_session = ?1", agent_session)
    }

    /// Every agent conversation id rlord tracks, live or archived, including
    /// ones a session moved on from: a `/clear` or a fresh restart logs
    /// `fresh_conversation`, any other switch (a resume)
    /// `agent_session_changed`, each with the old id as `from`.
    pub fn agent_session_ids(&self) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT agent_session FROM sessions WHERE agent_session IS NOT NULL \
             UNION SELECT json_extract(payload, '$.from') FROM events \
             WHERE kind IN ('agent_session_changed', 'fresh_conversation') \
               AND json_extract(payload, '$.from') IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The session with this role (`overlord`), if created.
    pub fn role_session(&self, role: &str) -> Result<Option<Session>> {
        Ok(self.query_sessions("role = ?1", role)?.into_iter().next())
    }

    /// Every overlord session: at most one per agent, oldest first.
    pub fn overlords(&self) -> Result<Vec<Session>> {
        let mut all = self.query_sessions("role = ?1", "overlord")?;
        all.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(all)
    }

    /// The active overlord, if one has ever been created: the one kv
    /// `OVERLORD_ACTIVE` names, else the oldest (an install from before
    /// there could be several has one row and no kv: that row).
    pub fn overlord(&self) -> Result<Option<Session>> {
        let all = self.overlords()?;
        let active = self.kv_get(OVERLORD_ACTIVE)?;
        let at = all
            .iter()
            .position(|s| Some(&s.id) == active.as_ref())
            .unwrap_or(0);
        Ok(all.into_iter().nth(at))
    }

    /// Make overlord session `id` the active one.
    pub fn set_active_overlord(&self, id: &str) -> Result<()> {
        self.kv_set(OVERLORD_ACTIVE, id)
    }

    /// Give a session another role and name (the mastermind becoming the
    /// overlord).
    pub fn set_role(&self, id: &str, role: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET role = ?2, name = ?3 WHERE id = ?1",
            params![id, role, name],
        )?;
        Ok(())
    }

    /// Retire a session that had a role: an ordinary, stopped, archived
    /// session under `name`, with no tmux session of its own.
    pub fn retire_role(&self, id: &str, name: &str, ts: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET role = '', name = ?2, tmux_session = NULL, \
             status = 'stopped', archived_at = ?3 WHERE id = ?1",
            params![id, name, ts],
        )?;
        Ok(())
    }

    /// Run `f` in one transaction: every write it makes through this `Db`
    /// commits together if it returns `Ok`, and none of them if it fails.
    /// (Not re-entrant: `f` must not call another `atomically` or a method
    /// that opens its own transaction.)
    pub fn atomically<T>(&self, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        let tx = self.conn.unchecked_transaction()?;
        let v = f()?;
        tx.commit()?;
        Ok(v)
    }

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn kv_delete(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM kv WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Every kv entry whose key starts with `prefix`, by key.
    pub fn kv_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare(
            "SELECT key, value FROM kv WHERE substr(key, 1, length(?1)) = ?1 ORDER BY key",
        )?;
        let rows = st.query_map([prefix], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self, include_archived: bool) -> Result<Vec<Session>> {
        self.sessions(&SessionQuery {
            archived: if include_archived {
                ArchivedMode::All
            } else {
                ArchivedMode::Live
            },
            ..Default::default()
        })
    }

    /// The one list/sort/filter path (pinned sessions always float first).
    pub fn sessions(&self, q: &SessionQuery) -> Result<Vec<Session>> {
        let mut conds: Vec<String> = Vec::new();
        let mut args: Vec<String> = Vec::new();
        if !q.include_overlord {
            conds.push("role = ''".into());
        }
        match q.archived {
            ArchivedMode::Live => conds.push("archived_at IS NULL".into()),
            ArchivedMode::Only => conds.push("archived_at IS NOT NULL".into()),
            ArchivedMode::All => {}
        }
        if !q.status.is_empty() {
            let ph = vec!["?"; q.status.len()].join(",");
            conds.push(format!("status IN ({ph})"));
            args.extend(q.status.iter().cloned());
        }
        if let Some(a) = &q.agent {
            conds.push("agent = ?".into());
            args.push(a.clone());
        }
        if let Some(t) = &q.tag {
            conds.push(
                "EXISTS (SELECT 1 FROM tags t WHERE t.session_id = sessions.id AND t.tag = ?)"
                    .into(),
            );
            args.push(t.clone());
        }
        if let Some(p) = &q.project {
            conds.push("project_path LIKE '%' || ? || '%'".into());
            args.push(p.clone());
        }
        if let Some(n) = &q.name {
            conds.push("name LIKE '%' || ? || '%'".into());
            args.push(n.clone());
        }
        if let Some(g) = &q.group {
            conds.push("group_path LIKE ? || '%'".into());
            args.push(g.clone());
        }
        if let Some(text) = &q.search {
            // Every word must match somewhere (name/project/group/tag, and
            // what it's about: description, notes, summary).
            for word in text.split_whitespace() {
                conds.push(
                    "(name LIKE '%' || ? || '%' OR project_path LIKE '%' || ? || '%' \
                      OR group_path LIKE '%' || ? || '%' \
                      OR description LIKE '%' || ? || '%' OR notes LIKE '%' || ? || '%' \
                      OR summary LIKE '%' || ? || '%' \
                      OR EXISTS (SELECT 1 FROM tags t WHERE t.session_id = sessions.id \
                                 AND t.tag LIKE '%' || ? || '%'))"
                        .into(),
                );
                for _ in 0..7 {
                    args.push(word.to_string());
                }
            }
        }
        let where_clause = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        let (expr, natural) = match q.sort {
            // Messages only (the user's, the agent's, and what rlord
            // typed in): a resume or an idle notification changes the
            // state (the State sort), not the conversation.
            // (Usage: in this order until the daemon sorts by usage, which
            // keeps it among equals, reversed or not.)
            SortMode::Activity | SortMode::Usage => (
                "MAX(created_at, COALESCE(last_agent_msg_at, 0), \
                 COALESCE(last_user_msg_at, 0), COALESCE(last_rlord_msg_at, 0))",
                "DESC",
            ),
            SortMode::User => ("COALESCE(last_user_msg_at, created_at)", "DESC"),
            SortMode::State => ("COALESCE(last_state_change_at, created_at)", "DESC"),
            SortMode::Created => ("created_at", "DESC"),
            SortMode::Name => ("name COLLATE NOCASE", "ASC"),
        };
        // (Usage: the daemon reverses the usage order; equals stay most
        // recent first either way.)
        let reverse = q.reverse && q.sort != SortMode::Usage;
        let dir = match (natural, reverse) {
            ("DESC", false) | ("ASC", true) => "DESC",
            _ => "ASC",
        };
        let order = format!("{expr} {dir}");
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions {where_clause} \
             ORDER BY pinned DESC, {order}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), row_to_session)?;
        let mut sessions: Vec<Session> = rows.collect::<std::result::Result<_, _>>()?;
        self.attach_tags(&mut sessions)?;
        self.attach_fork_counts(&mut sessions)?;
        self.attach_agent_counts(&mut sessions)?;
        Ok(sessions)
    }

    fn attach_agent_counts(&self, sessions: &mut [Session]) -> Result<()> {
        if sessions.is_empty() {
            return Ok(());
        }
        let mut stmt = self.conn.prepare(
            "SELECT session_id, SUM(finished_at IS NULL), COUNT(*) FROM subagents GROUP BY session_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let counts: std::collections::HashMap<String, (i64, i64)> = rows
            .filter_map(|r| r.ok())
            .map(|(id, running, total)| (id, (running, total)))
            .collect();
        for s in sessions {
            if let Some((running, total)) = counts.get(&s.id) {
                s.agents_running = *running;
                s.agents_total = *total;
            }
        }
        Ok(())
    }

    /// Fill in how many forks each of these sessions has, live and in all
    /// (one grouped query over the forks of these sessions only: the index
    /// on `forked_from`). For list rows (`sessions`): counts, not the list.
    fn attach_fork_counts(&self, sessions: &mut [Session]) -> Result<()> {
        if sessions.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        let mut stmt = self.conn.prepare(
            "SELECT forked_from, COUNT(*), COUNT(*) - COUNT(archived_at) FROM sessions \
             WHERE forked_from IN (SELECT value FROM json_each(?1)) \
             GROUP BY forked_from",
        )?;
        let counts: std::collections::HashMap<String, (usize, usize)> = stmt
            .query_map([serde_json::to_string(&ids)?], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get::<_, i64>(1)? as usize, r.get::<_, i64>(2)? as usize),
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        for s in sessions {
            (s.fork_count, s.live_fork_count) = counts.get(&s.id).copied().unwrap_or((0, 0));
        }
        Ok(())
    }

    /// Fill in these sessions' forks, and their counts (one query, over the
    /// forks of these sessions only). For one session shown in full
    /// (`session.get`), not on list rows or every lookup (`resolve_ref`).
    pub fn attach_forks(&self, sessions: &mut [Session]) -> Result<()> {
        if sessions.is_empty() {
            return Ok(());
        }
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        let mut stmt = self.conn.prepare(
            "SELECT forked_from, id, name, archived_at IS NOT NULL FROM sessions \
             WHERE forked_from IN (SELECT value FROM json_each(?1)) \
             ORDER BY created_at, id",
        )?;
        let mut by_parent: std::collections::HashMap<String, Vec<crate::types::ForkRef>> =
            Default::default();
        let rows = stmt.query_map([serde_json::to_string(&ids)?], |r| {
            Ok((
                r.get::<_, String>(0)?,
                crate::types::ForkRef {
                    id: r.get(1)?,
                    name: r.get(2)?,
                    archived: r.get(3)?,
                },
            ))
        })?;
        for row in rows {
            let (parent, fork) = row?;
            by_parent.entry(parent).or_default().push(fork);
        }
        for s in sessions {
            s.forks = by_parent.remove(&s.id).unwrap_or_default();
            s.fork_count = s.forks.len();
            s.live_fork_count = s.forks.iter().filter(|f| !f.archived).count();
        }
        Ok(())
    }

    fn attach_tags(&self, sessions: &mut [Session]) -> Result<()> {
        if sessions.is_empty() {
            return Ok(());
        }
        let mut stmt = self
            .conn
            .prepare("SELECT session_id, tag FROM tags ORDER BY tag")?;
        let mut by_session: std::collections::HashMap<String, Vec<String>> = Default::default();
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (sid, tag) = row?;
            by_session.entry(sid).or_default().push(tag);
        }
        for s in sessions {
            if let Some(t) = by_session.remove(&s.id) {
                s.tags = t;
            }
        }
        Ok(())
    }

    // ---- meta-context mutations ----

    /// Rename; an explicit rename locks the name against agent-side sync.
    pub fn rename(&self, id: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET name = ?2, name_locked = 1 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// Remove a session that never started (a fork whose pane didn't come
    /// up): its row, and with it its tags, background rows, transcript and
    /// sub-agent rows (cascade); its events; its kv keys (`<what>:<id>`).
    /// Sessions that ran are archived, never deleted.
    pub fn delete_session(&self, id: &str) -> Result<()> {
        self.atomically(|| {
            self.conn
                .execute("DELETE FROM events WHERE session_id = ?1", params![id])?;
            self.conn.execute(
                "DELETE FROM kv WHERE key LIKE '%:' || ?1 ESCAPE '\\'",
                params![id
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")],
            )?;
            self.conn
                .execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    /// Record that session `id` was forked from session `parent`.
    pub fn set_forked_from(&self, id: &str, parent: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET forked_from = ?2 WHERE id = ?1",
            params![id, parent],
        )?;
        Ok(())
    }

    /// Whether any session (live or archived) is called `name`.
    pub fn name_taken(&self, name: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sessions WHERE name = ?1)",
            params![name],
            |r| r.get(0),
        )?)
    }

    pub fn set_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET pinned = ?2 WHERE id = ?1",
            params![id, pinned as i64],
        )?;
        Ok(())
    }

    pub fn set_group(&self, id: &str, group_path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET group_path = ?2 WHERE id = ?1",
            params![id, group_path],
        )?;
        Ok(())
    }

    pub fn set_description(&self, id: &str, description: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET description = ?2 WHERE id = ?1",
            params![id, description],
        )?;
        Ok(())
    }

    /// What the session did (`at`: when it was made; None clears it).
    pub fn set_summary(&self, id: &str, summary: &str, at: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET summary = ?2, summary_at = ?3 WHERE id = ?1",
            params![id, summary, at],
        )?;
        Ok(())
    }

    pub fn set_notes(&self, id: &str, notes: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET notes = ?2 WHERE id = ?1",
            params![id, notes],
        )?;
        Ok(())
    }

    pub fn add_tag(&self, id: &str, tag: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO tags (session_id, tag) VALUES (?1, ?2)",
            params![id, tag],
        )?;
        Ok(())
    }

    pub fn remove_tag(&self, id: &str, tag: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM tags WHERE session_id = ?1 AND tag = ?2",
            params![id, tag],
        )?;
        Ok(())
    }

    /// Every tag on live (unarchived) sessions with its session count, most
    /// used first; the overlord is never tagged.
    pub fn tag_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.tag, COUNT(*) FROM tags t JOIN sessions s ON s.id = t.session_id \
             WHERE s.archived_at IS NULL AND s.role = '' \
             GROUP BY t.tag ORDER BY COUNT(*) DESC, t.tag",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Live, non-overlord sessions with no tags.
    pub fn untagged_count(&self) -> Result<usize> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM sessions s WHERE s.archived_at IS NULL AND s.role = '' \
             AND NOT EXISTS (SELECT 1 FROM tags t WHERE t.session_id = s.id)",
            [],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }

    // ---- transcript read positions ----

    /// The most recently saved positions (at most `limit`) of the current
    /// parse version.
    pub fn transcript_cursors(&self, limit: usize) -> Result<Vec<(PathBuf, CursorState)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, ino, offset, input_tokens, output_tokens, first_at, last_at, ended, \
                    turns, tokens, context, window, model \
             FROM transcript_cursors WHERE version = ?1 ORDER BY rowid DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![STATS_PARSE_VERSION, limit as i64], cursor_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Save moved positions (one transaction). Re-saving moves a row to the
    /// newest (rowid order is recency).
    pub fn save_transcript_cursors(&self, cursors: &[(PathBuf, CursorState)]) -> Result<()> {
        if cursors.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        for (p, c) in cursors {
            tx.execute(
                "INSERT OR REPLACE INTO transcript_cursors \
                   (path, ino, offset, input_tokens, output_tokens, first_at, last_at, ended, \
                    version, turns, tokens, context, window, model) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    p.to_string_lossy(),
                    c.ino as i64,
                    c.offset as i64,
                    c.input_tokens,
                    c.output_tokens,
                    c.first,
                    c.last,
                    c.ended as i64,
                    STATS_PARSE_VERSION,
                    c.turns as i64,
                    c.tokens,
                    c.context as i64,
                    c.window.map(|w| w as i64),
                    c.model
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Startup upkeep: drop positions from older counting logic, for
    /// transcripts that are gone, and past the newest `keep`. Returns how
    /// many rows went. One transaction, set-based but for the file checks.
    /// (Sub-agent totals counted by older logic aren't touched: their
    /// `counted` marks them, and the next count from the transcript
    /// overwrites them; an archived session's when it is next viewed.)
    pub fn prune_transcript_cursors(&self, keep: usize) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut gone = tx.execute(
            "DELETE FROM transcript_cursors WHERE version != ?1",
            [STATS_PARSE_VERSION],
        )?;
        let paths: Vec<String> = {
            let mut stmt = tx.prepare("SELECT path FROM transcript_cursors")?;
            let it = stmt.query_map([], |r| r.get(0))?;
            it.collect::<std::result::Result<_, _>>()?
        };
        {
            let mut del = tx.prepare("DELETE FROM transcript_cursors WHERE path = ?1")?;
            for p in paths.iter().filter(|p| !Path::new(p).exists()) {
                gone += del.execute([p])?;
            }
        }
        gone += tx.execute(
            "DELETE FROM transcript_cursors WHERE rowid NOT IN \
             (SELECT rowid FROM transcript_cursors ORDER BY rowid DESC LIMIT ?1)",
            [keep as i64],
        )?;
        tx.commit()?;
        Ok(gone)
    }

    /// Forget all read positions, and mark every sub-agent's token totals
    /// for a recount: the next read of its transcript (from the start)
    /// overwrites them (`rlord reset --rescan`). Totals of agents whose
    /// transcript is gone stay as they are (nothing to recount them from).
    pub fn clear_transcript_cursors(&self) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("UPDATE subagents SET counted = 0", [])?;
        tx.execute("DELETE FROM transcript_cursors", [])?;
        tx.commit()?;
        Ok(())
    }

    /// Saved positions (current parse version) of these transcripts, to
    /// read forward from when one fell out of memory (`seed_cursors`).
    pub fn transcript_cursors_for(&self, paths: &[&str]) -> Result<Vec<(PathBuf, CursorState)>> {
        let mut out = Vec::new();
        for chunk in paths.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut stmt = self.conn.prepare(&format!(
                "SELECT path, ino, offset, input_tokens, output_tokens, first_at, last_at, ended, \
                        turns, tokens, context, window, model \
                 FROM transcript_cursors WHERE version = {STATS_PARSE_VERSION} \
                   AND path IN ({marks})"
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), cursor_row)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    /// The user's favourite groups, in their order (top of the group list).
    pub fn group_order(&self) -> Result<Vec<String>> {
        Ok(self
            .kv_get("group_order")?
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default())
    }

    /// Replace the favourite groups (trimmed, `#` dropped, first wins).
    pub fn set_group_order(&self, order: &[String]) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        for t in order {
            let t = t.trim().trim_start_matches('#');
            if !t.is_empty() && !out.iter().any(|o| o == t) {
                out.push(t.to_string());
            }
        }
        self.kv_set("group_order", &serde_json::to_string(&out)?)?;
        Ok(out)
    }

    /// Rename a tag on every session (merging into `to` where it exists).
    /// Returns how many sessions carried it.
    pub fn rename_tag(&self, from: &str, to: &str) -> Result<usize> {
        if from == to {
            // Nothing to move; the DELETE below would drop the tag everywhere.
            return Ok(self.conn.query_row(
                "SELECT COUNT(*) FROM tags WHERE tag = ?1",
                [from],
                |r| r.get::<_, i64>(0),
            )? as usize);
        }
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO tags (session_id, tag) \
             SELECT session_id, ?2 FROM tags WHERE tag = ?1",
            params![from, to],
        )?;
        let removed = self
            .conn
            .execute("DELETE FROM tags WHERE tag = ?1", [from])?;
        // A renamed favourite keeps its place (merging into one already
        // there keeps the earlier place).
        let order = self.group_order()?;
        if order.iter().any(|t| t == from) {
            let renamed: Vec<String> = order
                .into_iter()
                .map(|t| if t == from { to.to_string() } else { t })
                .collect();
            self.set_group_order(&renamed)?;
        }
        Ok(n.max(removed))
    }

    pub fn launch_spec(&self, id: &str) -> Result<String> {
        Ok(self.conn.query_row(
            "SELECT launch_spec FROM sessions WHERE id = ?1",
            [id],
            |r| r.get(0),
        )?)
    }

    /// Whether any session (archived included) uses this tmux session name.
    pub fn tmux_name_taken(&self, name: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM sessions WHERE tmux_session = ?1 LIMIT 1",
                [name],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn set_tmux_session(&self, id: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET tmux_session = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    pub fn set_launch_spec(&self, id: &str, spec: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET launch_spec = ?2 WHERE id = ?1",
            params![id, spec],
        )?;
        Ok(())
    }

    /// Update status; stamps last_state_change_at and returns true iff changed.
    pub fn set_status(&self, id: &str, status: Status, ts: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE sessions SET status = ?2, last_state_change_at = ?3 \
             WHERE id = ?1 AND status != ?2",
            params![id, status.as_str(), ts],
        )?;
        if n > 0 && status == Status::Stopped {
            // A sub-agent can't outlive its session's agent process.
            self.close_open_subagents(Some(id), ts)?;
        }
        Ok(n > 0)
    }

    /// Mark sub-agents still open as finished, for one stopped session or
    /// (None) every stopped session. Claude doesn't always record an agent's
    /// end, and one can't keep running once its session has stopped.
    pub fn close_open_subagents(&self, session: Option<&str>, ts: i64) -> Result<usize> {
        // Finished when the session stopped (never before the agent started).
        let finish = "MAX(COALESCE(started_at, 0), COALESCE( \
             (SELECT last_state_change_at FROM sessions s WHERE s.id = subagents.session_id), ?1))";
        let stopped = Status::Stopped.as_str();
        let n = match session {
            Some(id) => self.conn.execute(
                &format!(
                    "UPDATE subagents SET finished_at = {finish} \
                     WHERE finished_at IS NULL AND session_id = ?2"
                ),
                params![ts, id],
            )?,
            None => self.conn.execute(
                &format!(
                    "UPDATE subagents SET finished_at = {finish} \
                     WHERE finished_at IS NULL \
                       AND session_id IN (SELECT id FROM sessions WHERE status = ?2)"
                ),
                params![ts, stopped],
            )?,
        };
        Ok(n)
    }

    pub fn set_agent_session(&self, id: &str, agent_session: &str, trust: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET agent_session = ?2, agent_session_trust = ?3 WHERE id = ?1",
            params![id, agent_session, trust],
        )?;
        Ok(())
    }

    /// Forget a session's conversation (it starts a new one whose id the
    /// agent assigns: bound again when the agent reports it) and where its
    /// transcript was.
    pub fn clear_agent_session(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET agent_session = NULL, agent_session_trust = 'minted' \
             WHERE id = ?1",
            params![id],
        )?;
        self.conn
            .execute("DELETE FROM transcripts WHERE session_id = ?1", params![id])?;
        Ok(())
    }

    pub fn set_archived(&self, id: &str, ts: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET archived_at = ?2 WHERE id = ?1",
            params![id, ts],
        )?;
        Ok(())
    }

    pub fn touch_last_user_msg(&self, id: &str, ts: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_user_msg_at = ?2 WHERE id = ?1",
            params![id, ts],
        )?;
        Ok(())
    }

    /// A kv value that is a JSON object (`{}` when unset or unreadable).
    pub fn kv_map(&self, key: &str) -> Result<serde_json::Map<String, serde_json::Value>> {
        Ok(self
            .kv_get(key)?
            .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default())
    }

    /// Store a JSON object under `key`; an empty one deletes the key.
    pub fn kv_set_map(
        &self,
        key: &str,
        m: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<()> {
        if m.is_empty() {
            self.kv_delete(key)
        } else {
            self.kv_set(key, &serde_json::Value::Object(m.clone()).to_string())
        }
    }

    /// A prompt rlord itself typed into the session (never
    /// `last_user_msg_at`: that is the user's own).
    pub fn touch_last_rlord_msg(&self, id: &str, ts: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_rlord_msg_at = ?2 WHERE id = ?1",
            params![id, ts],
        )?;
        Ok(())
    }

    pub fn touch_last_agent_msg(&self, id: &str, ts: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_agent_msg_at = ?2 WHERE id = ?1",
            params![id, ts],
        )?;
        Ok(())
    }

    /// Sessions the reconciler must check on daemon startup.
    pub fn running_sessions(&self) -> Result<Vec<Session>> {
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE archived_at IS NULL \
             AND status != 'stopped' AND tmux_session IS NOT NULL"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_session)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---- repos ----

    pub fn touch_repo(&self, path: &str, ts: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO repos (path, last_used_at, use_count) VALUES (?1, ?2, 1) \
             ON CONFLICT(path) DO UPDATE SET last_used_at = excluded.last_used_at, \
                                             use_count = use_count + 1",
            params![path, ts],
        )?;
        Ok(())
    }

    /// Known repos, most recently used first.
    pub fn list_repos(&self) -> Result<Vec<RepoRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, last_used_at, use_count FROM repos ORDER BY last_used_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RepoRow {
                path: r.get(0)?,
                last_used_at: r.get(1)?,
                use_count: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The folders live (non-archived) sessions run in: each one's
    /// `project_path` and `repo_path`. Distinct, unordered.
    pub fn session_folders(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT project_path FROM sessions WHERE archived_at IS NULL AND project_path != '' \
             UNION SELECT repo_path FROM sessions WHERE archived_at IS NULL \
             AND repo_path IS NOT NULL AND repo_path != ''",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---- subagents ----

    /// Insert or update a sub-agent; only provided (Some / non-empty) fields
    /// overwrite, so hook and disk sources can each contribute what they know.
    /// Token totals keep the larger value (a transcript that went or was
    /// cut short doesn't lower them), except over a count from older
    /// counting logic (`upsert_subagent_counted`).
    pub fn upsert_subagent(&self, a: &Subagent) -> Result<()> {
        self.upsert_subagent_as(a, 0)
    }

    /// `upsert_subagent` for totals counted from the agent's transcript
    /// (`a.transcript_path`) by the current `transcript_stats`: they replace
    /// totals counted by an older parse version, even when lower (a recount
    /// after a counting fix). Without a transcript path, as `upsert_subagent`.
    pub fn upsert_subagent_counted(&self, a: &Subagent) -> Result<()> {
        self.upsert_subagent_as(a, STATS_PARSE_VERSION)
    }

    fn upsert_subagent_as(&self, a: &Subagent, counted: i64) -> Result<()> {
        let counted = if a.transcript_path.is_some() {
            counted
        } else {
            0
        };
        self.conn.execute(
            "INSERT INTO subagents (agent_id, session_id, agent_type, description, tool_use_id, \
                                    spawn_depth, background, started_at, finished_at, \
                                    transcript_path, result, input_tokens, output_tokens, counted) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
             ON CONFLICT(agent_id) DO UPDATE SET \
               agent_type = CASE WHEN excluded.agent_type != '' THEN excluded.agent_type ELSE agent_type END, \
               description = CASE WHEN excluded.description != '' THEN excluded.description ELSE description END, \
               tool_use_id = COALESCE(excluded.tool_use_id, tool_use_id), \
               spawn_depth = MAX(spawn_depth, excluded.spawn_depth), \
               background = MAX(background, excluded.background), \
               started_at = COALESCE(started_at, excluded.started_at), \
               finished_at = CASE \
                 WHEN excluded.finished_at IS NOT NULL THEN excluded.finished_at \
                 WHEN excluded.input_tokens + excluded.output_tokens > input_tokens + output_tokens \
                   THEN NULL \
                 ELSE finished_at END, \
               transcript_path = COALESCE(excluded.transcript_path, transcript_path), \
               result = CASE WHEN excluded.result != '' THEN excluded.result ELSE result END, \
               input_tokens = CASE WHEN excluded.counted > counted THEN excluded.input_tokens \
                 ELSE MAX(input_tokens, excluded.input_tokens) END, \
               output_tokens = CASE WHEN excluded.counted > counted THEN excluded.output_tokens \
                 ELSE MAX(output_tokens, excluded.output_tokens) END, \
               counted = MAX(counted, excluded.counted)",
            params![
                a.agent_id,
                a.session_id,
                a.agent_type,
                a.description,
                a.tool_use_id,
                a.spawn_depth,
                a.background as i64,
                a.started_at,
                a.finished_at,
                a.transcript_path,
                a.result,
                a.input_tokens,
                a.output_tokens,
                counted
            ],
        )?;
        Ok(())
    }

    /// A session's sub-agents whose token totals weren't counted by the
    /// current parse version (a recount may lower them).
    pub fn subagents_to_recount(
        &self,
        session_id: &str,
    ) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT agent_id FROM subagents WHERE session_id = ?1 AND counted < ?2")?;
        let rows = stmt.query_map(params![session_id, STATS_PARSE_VERSION], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn subagents(&self, session_id: &str) -> Result<Vec<Subagent>> {
        let mut stmt = self.conn.prepare(
            "SELECT agent_id, session_id, agent_type, description, tool_use_id, spawn_depth, \
                    background, started_at, finished_at, transcript_path, result, \
                    input_tokens, output_tokens \
             FROM subagents WHERE session_id = ?1 ORDER BY started_at, agent_id",
        )?;
        let rows = stmt.query_map([session_id], |r| {
            Ok(Subagent {
                agent_id: r.get(0)?,
                session_id: r.get(1)?,
                agent_type: r.get(2)?,
                description: r.get(3)?,
                tool_use_id: r.get(4)?,
                spawn_depth: r.get(5)?,
                background: r.get::<_, i64>(6)? != 0,
                started_at: r.get(7)?,
                finished_at: r.get(8)?,
                transcript_path: r.get(9)?,
                result: r.get(10)?,
                input_tokens: r.get(11)?,
                output_tokens: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn subagent_exists(&self, agent_id: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM subagents WHERE agent_id = ?1",
            [agent_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn transcript_source(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT source_path FROM transcripts WHERE session_id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .ok())
    }

    // ---- events ----

    pub fn insert_event(&self, session_id: &str, ts: i64, kind: &str, payload: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events (session_id, ts, kind, payload) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, ts, kind, payload],
        )?;
        Ok(())
    }

    pub fn list_events(&self, session_id: &str, limit: i64) -> Result<Vec<EventRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, kind, payload FROM events WHERE session_id = ?1 \
             ORDER BY seq DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit], |r| {
            Ok(EventRow {
                seq: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                payload: serde_json::from_str(&r.get::<_, String>(3)?)
                    .unwrap_or(serde_json::Value::Null),
            })
        })?;
        let mut v: Vec<EventRow> = rows.collect::<std::result::Result<_, _>>()?;
        v.reverse(); // chronological
        Ok(v)
    }

    // ---- session context (notes and links from the overlord) ----

    /// Add a note (`kind` `note`) or a link (`link`, `body` the linked
    /// session's id). A link that is already there isn't added again.
    /// Returns whether a row was added.
    pub fn context_add(
        &self,
        session_id: &str,
        at: i64,
        kind: &str,
        body: &str,
        source: &str,
    ) -> Result<bool> {
        // A link once only. A note the same as one not handed over yet, or
        // as the latest note: not again.
        let dup_sql = if kind == "link" {
            "SELECT EXISTS (SELECT 1 FROM session_context \
             WHERE session_id = ?1 AND kind = 'link' AND body = ?2)"
        } else {
            "SELECT EXISTS (SELECT 1 FROM session_context \
             WHERE session_id = ?1 AND kind = 'note' AND body = ?2 \
             AND (seen_at IS NULL OR id = (SELECT max(id) FROM session_context \
                  WHERE session_id = ?1 AND kind = 'note')))"
        };
        let dup: bool = self
            .conn
            .query_row(dup_sql, params![session_id, body], |r| r.get(0))?;
        if dup {
            return Ok(false);
        }
        self.conn.execute(
            "INSERT INTO session_context (session_id, at, kind, body, source) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, at, kind, body, source],
        )?;
        Ok(true)
    }

    /// Whether a session has any notes or links (without loading them).
    pub fn context_exists(&self, session_id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM session_context WHERE session_id = ?1)",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    /// The sessions that link to `session_id` (its related sessions'
    /// owners).
    pub fn context_linkers(&self, session_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT session_id FROM session_context WHERE kind = 'link' AND body = ?1",
        )?;
        let rows = stmt.query_map(params![session_id], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Record that the hook answer `ack` carried rows `ids` of a session
    /// (at `at`): the latest answer that carried a row is the one whose
    /// acknowledgement marks it seen.
    pub fn context_mark_sent(
        &self,
        session_id: &str,
        upto: i64,
        only_unseen: bool,
        ack: &str,
        at: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE session_context SET sent_ack = ?4, sent_at = ?5 \
             WHERE session_id = ?1 AND id <= ?2 AND (?3 = 0 OR seen_at IS NULL)",
            params![session_id, upto, only_unseen, ack, at],
        )?;
        Ok(())
    }

    /// The answer `ack` was acknowledged: the rows it was the latest to
    /// carry, not seen yet, are seen at `at`. Returns how many.
    pub fn context_ack(&self, session_id: &str, ack: &str, at: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE session_context SET seen_at = ?3 \
             WHERE session_id = ?1 AND sent_ack = ?2 AND seen_at IS NULL",
            params![session_id, ack, at],
        )?)
    }

    /// Every row of a session some answer carried, not seen yet, is seen
    /// at `at` (rows no answer carried stay unseen).
    pub fn context_mark_sent_seen(&self, session_id: &str, at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE session_context SET seen_at = ?2 \
             WHERE session_id = ?1 AND sent_ack IS NOT NULL AND seen_at IS NULL",
            params![session_id, at],
        )?;
        Ok(())
    }

    /// The ids of the sessions a session links to, oldest first.
    pub fn context_links(&self, session_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT body FROM session_context WHERE session_id = ?1 AND kind = 'link' ORDER BY id",
        )?;
        let rows = stmt.query_map(params![session_id], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// A session's notes and links, oldest first.
    pub fn context_rows(&self, session_id: &str) -> Result<Vec<ContextRow>> {
        self.context_where(session_id, "")
    }

    /// Those no hook has pushed to the agent yet, oldest first.
    pub fn context_unseen(&self, session_id: &str) -> Result<Vec<ContextRow>> {
        self.context_where(session_id, "AND seen_at IS NULL")
    }

    fn context_where(&self, session_id: &str, and: &str) -> Result<Vec<ContextRow>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id, at, kind, body, source, seen_at FROM session_context \
             WHERE session_id = ?1 {and} ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(ContextRow {
                id: r.get(0)?,
                at: r.get(1)?,
                kind: r.get(2)?,
                body: r.get(3)?,
                source: r.get(4)?,
                seen_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Mark a session's rows up to `upto` (a row id) as pushed at `at`.
    #[cfg(test)]
    pub fn context_mark_seen(&self, session_id: &str, upto: i64, at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE session_context SET seen_at = ?3 \
             WHERE session_id = ?1 AND id <= ?2 AND seen_at IS NULL",
            params![session_id, upto, at],
        )?;
        Ok(())
    }

    /// Rewrite a session's notes from `source` (a fork's own note, when it
    /// is made again): new text and time, not handed to the agent yet.
    /// Returns how many were rewritten.
    pub fn context_renote(
        &self,
        session_id: &str,
        source: &str,
        body: &str,
        at: i64,
    ) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE session_context SET body = ?3, at = ?4, seen_at = NULL, sent_ack = NULL, \
             sent_at = NULL WHERE session_id = ?1 AND kind = 'note' AND source = ?2",
            params![session_id, source, body, at],
        )?)
    }

    /// Copy session `from`'s notes and links to session `to` (a fork),
    /// keeping when each was added and its source, unseen (the fork's own
    /// delivery mode decides how they reach it); not which hook answer
    /// carried them. `except`: row ids left out. Returns how many were
    /// copied.
    pub fn context_copy(&self, from: &str, to: &str, except: &[i64]) -> Result<usize> {
        let skip = serde_json::to_string(except)?;
        Ok(self.conn.execute(
            "INSERT INTO session_context (session_id, at, kind, body, source) \
             SELECT ?2, at, kind, body, source FROM session_context \
             WHERE session_id = ?1 AND id NOT IN (SELECT value FROM json_each(?3)) \
             ORDER BY id",
            params![from, to, skip],
        )?)
    }

    /// Mark every row of a session not yet pushed as pushed at `at`.
    pub fn context_mark_all_seen(&self, session_id: &str, at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE session_context SET seen_at = ?2 WHERE session_id = ?1 AND seen_at IS NULL",
            params![session_id, at],
        )?;
        Ok(())
    }

    // ---- transcripts ----

    /// Agent conversation ids a session had before its current one (a
    /// `/clear`, a resume or a fresh restart starts a new one), newest first.
    pub fn past_agent_sessions(&self, id: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT payload FROM events WHERE session_id = ?1 \
             AND kind IN ('agent_session_changed', 'fresh_conversation') \
             ORDER BY ts DESC",
        )?;
        let rows = st.query_map([id], |r| r.get::<_, String>(0))?;
        let mut out: Vec<String> = Vec::new();
        for p in rows.flatten() {
            if let Some(f) = serde_json::from_str::<serde_json::Value>(&p)
                .ok()
                .and_then(|v| v["from"].as_str().map(str::to_string))
            {
                if !out.contains(&f) {
                    out.push(f);
                }
            }
        }
        Ok(out)
    }

    /// Each time a session moved to another conversation, oldest first:
    /// (when, event kind, payload `{from, to, reason?}`). A fresh restart
    /// or a `/clear` is a `fresh_conversation`; any other switch (a resume)
    /// an `agent_session_changed`.
    pub fn conversation_changes(&self, id: &str) -> Result<Vec<(i64, String, serde_json::Value)>> {
        let mut st = self.conn.prepare(
            "SELECT ts, kind, payload FROM events WHERE session_id = ?1 \
             AND kind IN ('agent_session_changed', 'fresh_conversation') \
             ORDER BY seq ASC",
        )?;
        let rows = st.query_map([id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        Ok(rows
            .flatten()
            .map(|(ts, kind, p)| {
                (
                    ts,
                    kind,
                    serde_json::from_str(&p).unwrap_or(serde_json::Value::Null),
                )
            })
            .collect())
    }

    /// Every session's transcript file (session id, path).
    pub fn transcript_sources(&self) -> Result<Vec<(String, String)>> {
        let mut st = self
            .conn
            .prepare("SELECT session_id, source_path FROM transcripts")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Live (unarchived) sessions with a transcript, the overlords included:
    /// whether they are stopped or failed and since when (the caller allows
    /// a grace: a `/clear` stops a session for a moment), and when its agent
    /// process last started (0: never seen): rlord starting it (a `launched` /
    /// `imported` / `forked` / `resumed` / `restarted` / `reforked` event;
    /// a fork's copied transcript holds the original's launches, which
    /// don't run in it), or the agent's own
    /// `SessionStart` hook with `start` `startup`, always a new process,
    /// which also catches a restart done outside rlord. Not `resume`:
    /// Claude Code sends it for an in-process `/resume` too, which may keep
    /// the tasks (a new process is certain only when rlord resumed it);
    /// not `clear` or `compact`. Cheap: ids, paths and look-ups on
    /// `idx_events_session_kind`.
    pub fn live_transcripts(&self) -> Result<Vec<LiveTranscript>> {
        let mut st = self.conn.prepare(
            "SELECT s.id, t.source_path, \
                    COALESCE((SELECT MAX(e.ts) FROM events e WHERE e.session_id = s.id \
                              AND (e.kind IN ('launched', 'imported', 'forked', 'resumed', \
                                                'restarted', 'reforked') \
                                   OR (e.kind = 'session_start' AND \
                                       json_extract(e.payload, '$.start') = 'startup'))), 0) \
             , s.status IN ('stopped', 'error'), COALESCE(s.last_state_change_at, 0) \
             FROM sessions s JOIN transcripts t ON t.session_id = s.id \
             WHERE s.archived_at IS NULL",
        )?;
        let rows = st.query_map([], |r| {
            Ok(LiveTranscript {
                id: r.get(0)?,
                path: r.get(1)?,
                started_at: r.get(2)?,
                stopped: r.get(3)?,
                changed_at: r.get(4)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// These sessions' transcript files (session id, path).
    pub fn transcript_sources_for(&self, ids: &[&str]) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(500) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut st = self.conn.prepare(&format!(
                "SELECT session_id, source_path FROM transcripts WHERE session_id IN ({marks})"
            ))?;
            let rows = st.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            out.extend(rows.filter_map(|r| r.ok()));
        }
        Ok(out)
    }

    pub fn upsert_transcript_source(&self, session_id: &str, source_path: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO transcripts (session_id, source_path) VALUES (?1, ?2) \
             ON CONFLICT(session_id) DO UPDATE SET source_path = excluded.source_path",
            params![session_id, source_path],
        )?;
        Ok(())
    }
}

/// A `transcript_cursors` row (path, ino, offset, input_tokens,
/// output_tokens, first_at, last_at, ended, turns, tokens, context, window,
/// model).
fn cursor_row(r: &rusqlite::Row) -> rusqlite::Result<(PathBuf, CursorState)> {
    Ok((
        PathBuf::from(r.get::<_, String>(0)?),
        CursorState {
            ino: r.get::<_, i64>(1)? as u64,
            offset: r.get::<_, i64>(2)? as u64,
            input_tokens: r.get(3)?,
            output_tokens: r.get(4)?,
            first: r.get(5)?,
            last: r.get(6)?,
            ended: r.get::<_, i64>(7)? != 0,
            turns: r.get::<_, i64>(8)? as u64,
            tokens: r.get(9)?,
            context: r.get::<_, i64>(10)? as u64,
            window: r.get::<_, Option<i64>>(11)?.map(|w| w as u64),
            model: r.get(12)?,
        },
    ))
}

fn row_to_session(r: &rusqlite::Row) -> rusqlite::Result<Session> {
    let spec = serde_json::from_str::<serde_json::Value>(&r.get::<_, String>(21)?).ok();
    Ok(Session {
        id: r.get(0)?,
        name: r.get(1)?,
        name_locked: r.get::<_, i64>(2)? != 0,
        agent: r.get(3)?,
        agent_session: r.get(4)?,
        agent_session_trust: r.get(5)?,
        project_path: r.get(6)?,
        tmux_session: r.get(7)?,
        status: parse_status(&r.get::<_, String>(8)?),
        group_path: r.get(9)?,
        pinned: r.get::<_, i64>(10)? != 0,
        created_at: r.get(11)?,
        archived_at: r.get(12)?,
        last_user_msg_at: r.get(13)?,
        last_agent_msg_at: r.get(14)?,
        last_state_change_at: r.get(15)?,
        agents_running: 0,
        agents_total: 0,
        repo_path: r.get(16)?,
        workspace_path: r.get(17)?,
        branch: r.get(18)?,
        description: r.get(19)?,
        notes: r.get(20)?,
        model: spec
            .as_ref()
            .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
            .filter(|m| !m.is_empty()),
        codex_daemon: spec.as_ref().is_some_and(crate::agents::codex::on_daemon),
        tags: Vec::new(),
        role: r.get(22)?,
        summary: r.get(23)?,
        summary_at: r.get(24)?,
        last_rlord_msg_at: r.get(25)?,
        forked_from: r.get(26)?,
        forked_from_name: r.get(27)?,
        forks: Vec::new(),
        fork_count: 0,
        live_fork_count: 0,
    })
}

fn parse_status(s: &str) -> Status {
    match s {
        "starting" => Status::Starting,
        "working" => Status::Working,
        "waiting_input" => Status::WaitingInput,
        "idle" => Status::Idle,
        "error" => Status::Error,
        _ => Status::Stopped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(db: &Db, id: &str, name: &str) {
        db.insert_session(&NewSession {
            id,
            name,
            agent: "claude",
            agent_session: Some("uuid-1"),
            project_path: "/tmp/p",
            tmux_session: Some(&format!("rlord_{id}")),
            launch_spec: "{}",
            created_at: 1000,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "",
        })
        .unwrap();
    }

    #[test]
    fn a_cleared_conversation_stays_tracked() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01a", "a");
        db.insert_event(
            "01a",
            2,
            "fresh_conversation",
            r#"{"from":"pre-clear","to":"uuid-1","reason":"clear"}"#,
        )
        .unwrap();
        db.insert_event(
            "01a",
            3,
            "agent_session_changed",
            r#"{"from":"resumed-away","to":"uuid-1"}"#,
        )
        .unwrap();
        let ids = db.agent_session_ids().unwrap();
        for id in ["uuid-1", "pre-clear", "resumed-away"] {
            assert!(ids.contains(id), "{id} tracked: {ids:?}");
        }
    }

    #[test]
    fn rename_tag_to_itself_keeps_it() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01a", "a");
        db.add_tag("01a", "web").unwrap();
        assert_eq!(db.rename_tag("web", "web").unwrap(), 1);
        assert_eq!(db.tag_counts().unwrap(), vec![("web".to_string(), 1)]);
        assert_eq!(db.untagged_count().unwrap(), 0);
        db.rename_tag("web", "frontend").unwrap();
        assert_eq!(db.tag_counts().unwrap(), vec![("frontend".to_string(), 1)]);
    }

    #[test]
    fn group_order_survives_renames() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01a", "a");
        db.add_tag("01a", "web").unwrap();
        let o = db
            .set_group_order(&["#work".into(), "web".into(), " work ".into()])
            .unwrap();
        assert_eq!(o, ["work", "web"]);
        db.rename_tag("web", "frontend").unwrap();
        assert_eq!(db.group_order().unwrap(), ["work", "frontend"]);
        db.rename_tag("frontend", "work").unwrap(); // merge: keeps the first place
        assert_eq!(db.group_order().unwrap(), ["work"]);
    }

    #[test]
    fn stopping_a_session_closes_its_open_subagents() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01a", "a");
        db.set_status("01a", Status::Working, 1).unwrap();
        let mut a = crate::types::Subagent {
            agent_id: "ag1".into(),
            session_id: "01a".into(),
            agent_type: "Explore".into(),
            description: String::new(),
            tool_use_id: None,
            spawn_depth: 1,
            background: false,
            started_at: Some(5),
            finished_at: None,
            transcript_path: None,
            result: String::new(),
            input_tokens: 0,
            output_tokens: 0,
        };
        db.upsert_subagent(&a).unwrap();
        a.agent_id = "ag2".into();
        a.finished_at = Some(7);
        db.upsert_subagent(&a).unwrap();
        db.set_status("01a", Status::Stopped, 100).unwrap();
        let got: Vec<Option<i64>> = db
            .subagents("01a")
            .unwrap()
            .iter()
            .map(|a| a.finished_at)
            .collect();
        assert!(
            got.contains(&Some(100)) && got.contains(&Some(7)),
            "{got:?}"
        );
        assert_eq!(db.close_open_subagents(None, 200).unwrap(), 0);
        // Finished, then new usage (resumed): open again.
        a.finished_at = None;
        a.input_tokens = 50;
        db.upsert_subagent(&a).unwrap();
        let ag2 = db
            .subagents("01a")
            .unwrap()
            .into_iter()
            .find(|x| x.agent_id == "ag2")
            .unwrap();
        assert_eq!(ag2.finished_at, None);
    }

    #[test]
    fn transcript_cursors_round_trip_and_clear() {
        let db = Db::open_in_memory().unwrap();
        let c = CursorState {
            ino: 7,
            offset: 1234,
            input_tokens: 10,
            output_tokens: 2,
            first: Some(1),
            last: Some(9),
            ended: true,
            turns: 3,
            tokens: 4567,
            context: 890,
            window: Some(272_000),
            model: Some("gpt-5".into()),
        };
        db.save_transcript_cursors(&[(PathBuf::from("/t/a.jsonl"), c.clone())])
            .unwrap();
        db.save_transcript_cursors(&[(PathBuf::from("/t/a.jsonl"), c.clone())])
            .unwrap();
        assert_eq!(
            db.transcript_cursors(10).unwrap(),
            vec![(PathBuf::from("/t/a.jsonl"), c.clone())]
        );
        // Its file doesn't exist: pruned at startup.
        assert_eq!(db.prune_transcript_cursors(10).unwrap(), 1);
        assert!(db.transcript_cursors(10).unwrap().is_empty());
        // --rescan keeps totals of agents whose transcript is gone.
        db.save_transcript_cursors(&[(PathBuf::from("/t/b.jsonl"), c)])
            .unwrap();
        db.clear_transcript_cursors().unwrap();
        assert!(db.transcript_cursors(10).unwrap().is_empty());
    }

    #[test]
    fn a_recount_replaces_totals_from_older_counting() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01aaa", "alpha");
        let dir = tempfile::tempdir().unwrap();
        let t = dir.path().join("agent-a1.jsonl");
        std::fs::write(&t, "").unwrap();
        let a = Subagent {
            agent_id: "a1".into(),
            session_id: "01aaa".into(),
            agent_type: "Explore".into(),
            description: String::new(),
            tool_use_id: None,
            spawn_depth: 1,
            background: false,
            started_at: Some(100),
            finished_at: Some(200),
            transcript_path: Some(t.to_string_lossy().into_owned()),
            result: String::new(),
            input_tokens: 200,
            output_tokens: 40,
        };
        // Saved by older counting (it double-counted): `counted` 0, as the
        // migration leaves every existing row. An old-version position for
        // its transcript is pruned without touching the totals.
        db.upsert_subagent(&a).unwrap();
        let old = CursorState::default();
        db.save_transcript_cursors(&[(t.clone(), old)]).unwrap();
        db.conn
            .execute("UPDATE transcript_cursors SET version = 1", [])
            .unwrap();
        assert_eq!(db.prune_transcript_cursors(10).unwrap(), 1);
        let tokens = |db: &Db| {
            let a = &db.subagents("01aaa").unwrap()[0];
            (a.input_tokens, a.output_tokens)
        };
        assert_eq!(tokens(&db), (200, 40), "kept until recounted");
        assert_eq!(db.subagents_to_recount("01aaa").unwrap().len(), 1);
        // A hook without counts can't lower them.
        db.upsert_subagent(&Subagent {
            input_tokens: 0,
            output_tokens: 0,
            ..a.clone()
        })
        .unwrap();
        assert_eq!(tokens(&db), (200, 40));
        // The recount (current logic, from its transcript) wins, though lower.
        let recount = Subagent {
            input_tokens: 100,
            output_tokens: 20,
            ..a.clone()
        };
        db.upsert_subagent_counted(&recount).unwrap();
        assert_eq!(tokens(&db), (100, 20));
        assert!(db.subagents_to_recount("01aaa").unwrap().is_empty());
        // Counted now: back to keeping the larger value.
        db.upsert_subagent_counted(&Subagent {
            input_tokens: 50,
            ..recount.clone()
        })
        .unwrap();
        assert_eq!(tokens(&db), (100, 20));
        // --rescan marks it for a recount again.
        db.clear_transcript_cursors().unwrap();
        db.upsert_subagent_counted(&Subagent {
            input_tokens: 90,
            ..recount
        })
        .unwrap();
        assert_eq!(tokens(&db), (90, 20));
        // With no transcript to count from, nothing is replaced.
        db.clear_transcript_cursors().unwrap();
        db.upsert_subagent_counted(&Subagent {
            input_tokens: 0,
            output_tokens: 0,
            transcript_path: None,
            ..a
        })
        .unwrap();
        assert_eq!(tokens(&db), (90, 20));
    }

    #[test]
    fn migrations_apply_and_tables_exist() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), 12);
        for table in [
            "sessions",
            "tags",
            "events",
            "messages",
            "transcripts",
            "repos",
        ] {
            let n: i64 = db
                .conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {table}");
        }
        assert!(db.list_sessions(true).unwrap().is_empty());
    }

    #[test]
    fn context_rows_are_kept_marked_and_go_with_their_session() {
        let db = Db::open_in_memory().unwrap();
        for id in ["a", "b"] {
            db.insert_session(&NewSession {
                id,
                name: id,
                agent: "claude",
                agent_session: None,
                project_path: "/tmp",
                tmux_session: None,
                launch_spec: "{}",
                created_at: 1,
                repo_path: None,
                workspace_path: None,
                branch: None,
                role: "",
            })
            .unwrap();
        }
        assert!(db
            .context_add("a", 10, "note", "first", "overlord")
            .unwrap());
        assert!(db.context_add("a", 11, "link", "b", "overlord").unwrap());
        // The same link once only.
        assert!(!db.context_add("a", 12, "link", "b", "overlord").unwrap());
        assert!(db
            .context_add("a", 13, "note", "second", "overlord")
            .unwrap());
        let rows = db.context_rows("a").unwrap();
        assert_eq!(
            rows.iter().map(|r| r.body.as_str()).collect::<Vec<_>>(),
            ["first", "b", "second"]
        );
        assert!(db.context_rows("b").unwrap().is_empty());
        assert!(db.context_exists("a").unwrap() && !db.context_exists("b").unwrap());
        assert_eq!(db.context_linkers("b").unwrap(), ["a"]);
        // The same note again while it is unseen, or as the latest: no.
        assert!(!db
            .context_add("a", 14, "note", "second", "overlord")
            .unwrap());
        assert!(!db
            .context_add("a", 14, "note", "first", "overlord")
            .unwrap());
        assert_eq!(db.context_links("a").unwrap(), ["b"]);
        db.context_mark_seen("a", rows[1].id, 20).unwrap();
        let unseen = db.context_unseen("a").unwrap();
        assert_eq!(unseen.len(), 1);
        assert_eq!(unseen[0].body, "second");
        assert_eq!(db.context_rows("a").unwrap()[0].seen_at, Some(20));
        // All the rest at once; what was seen keeps its time.
        db.context_mark_all_seen("a", 30).unwrap();
        assert!(db.context_unseen("a").unwrap().is_empty());
        let rows = db.context_rows("a").unwrap();
        assert_eq!((rows[0].seen_at, rows[2].seen_at), (Some(20), Some(30)));
        // Rows go with their session.
        db.conn
            .execute("DELETE FROM sessions WHERE id = 'a'", [])
            .unwrap();
        assert!(db.context_rows("a").unwrap().is_empty());
    }

    #[test]
    fn a_fork_knows_its_parent_and_takes_a_copy_of_its_background() {
        let db = Db::open_in_memory().unwrap();
        for (id, at) in [("p", 1), ("f", 2), ("g", 3), ("x", 4)] {
            db.insert_session(&NewSession {
                id,
                name: &format!("n-{id}"),
                agent: "claude",
                agent_session: None,
                project_path: "/tmp",
                tmux_session: None,
                launch_spec: "{}",
                created_at: at,
                repo_path: None,
                workspace_path: None,
                branch: None,
                role: "",
            })
            .unwrap();
        }
        db.set_forked_from("f", "p").unwrap();
        db.set_forked_from("g", "p").unwrap();
        let f = db.resolve_ref("f").unwrap();
        assert_eq!(f.forked_from.as_deref(), Some("p"));
        assert_eq!(f.forked_from_name.as_deref(), Some("n-p"));
        let p = db.resolve_ref("p").unwrap();
        assert_eq!(
            (p.forked_from.as_deref(), p.forked_from_name.as_deref()),
            (None, None)
        );
        let fork = |id: &str, archived| crate::types::ForkRef {
            id: id.into(),
            name: format!("n-{id}"),
            archived,
        };
        // Not on a plain lookup (every hook makes one): for one session
        // shown in full, with its counts.
        assert!(p.forks.is_empty());
        db.set_archived("g", Some(5)).unwrap();
        let mut one = [p];
        db.attach_forks(&mut one).unwrap();
        assert_eq!(one[0].forks, [fork("f", false), fork("g", true)]);
        assert_eq!((one[0].fork_count, one[0].live_fork_count), (2, 1));
        // A list row: the counts only, never the list.
        let listed = db.sessions(&SessionQuery::default()).unwrap();
        let p_row = listed.iter().find(|s| s.id == "p").unwrap();
        assert_eq!((p_row.fork_count, p_row.live_fork_count), (2, 1));
        assert!(p_row.forks.is_empty());
        assert!(serde_json::to_value(p_row).unwrap().get("forks").is_none());
        db.set_archived("g", None).unwrap();
        let x = listed.iter().find(|s| s.id == "x").unwrap();
        assert_eq!((x.fork_count, x.live_fork_count), (0, 0));
        // Listed with its parent's name too.
        let listed = db
            .sessions(&SessionQuery::default())
            .unwrap()
            .into_iter()
            .find(|s| s.id == "g")
            .unwrap();
        assert_eq!(listed.forked_from_name.as_deref(), Some("n-p"));
        assert!(db.name_taken("n-f").unwrap() && !db.name_taken("n-q").unwrap());

        db.context_add("p", 10, "note", "seen", "overlord").unwrap();
        db.context_add("p", 11, "link", "x", "overlord").unwrap();
        db.context_add("p", 12, "note", "unseen", "overlord")
            .unwrap();
        let rows = db.context_rows("p").unwrap();
        db.context_mark_seen("p", rows[0].id, 20).unwrap();
        db.context_mark_sent("p", rows[2].id, true, "p.ack", 21)
            .unwrap();
        let rows = db.context_rows("p").unwrap();
        // Everything but the link, unseen (the fork's mode hands them over).
        assert_eq!(db.context_copy("p", "f", &[rows[1].id]).unwrap(), 2);
        let copied = db.context_rows("f").unwrap();
        assert_eq!(
            copied
                .iter()
                .map(|r| (r.at, r.body.as_str(), r.source.as_str(), r.seen_at))
                .collect::<Vec<_>>(),
            [
                (10, "seen", "overlord", None),
                (12, "unseen", "overlord", None)
            ]
        );
        // Not which answer carried it (that was the parent's).
        assert_eq!(db.context_ack("f", "p.ack", 30).unwrap(), 0);
        // The parent's own rows are untouched.
        assert_eq!(db.context_rows("p").unwrap(), rows);
    }

    /// A session that never started goes with everything that names it,
    /// and nothing else.
    #[test]
    fn a_session_that_never_started_is_removed_whole() {
        let db = Db::open_in_memory().unwrap();
        for id in ["gone", "kept"] {
            db.insert_session(&NewSession {
                id,
                name: id,
                agent: "claude",
                agent_session: Some(id),
                project_path: "/tmp",
                tmux_session: None,
                launch_spec: "{}",
                created_at: 1,
                repo_path: None,
                workspace_path: None,
                branch: None,
                role: "",
            })
            .unwrap();
            db.add_tag(id, "t").unwrap();
            db.insert_event(id, 1, "launched", "{}").unwrap();
            db.context_add(id, 1, "note", "n", "fork").unwrap();
            db.upsert_transcript_source(id, "/t.jsonl").unwrap();
            db.kv_set(&format!("context_on:{id}"), "1").unwrap();
        }
        db.kv_set("fork_name:gone-ish", "1").unwrap();
        db.delete_session("gone").unwrap();
        assert!(db.resolve_ref("gone").is_err());
        assert!(db.list_events("gone", 10).unwrap().is_empty());
        assert!(db.context_rows("gone").unwrap().is_empty());
        assert_eq!(db.transcript_source("gone").unwrap(), None);
        assert_eq!(db.kv_get("context_on:gone").unwrap(), None);
        // Others' rows and keys stay (one ending in another id too).
        let kept = db.resolve_ref("kept").unwrap();
        assert_eq!(kept.tags, ["t"]);
        assert_eq!(db.list_events("kept", 10).unwrap().len(), 1);
        assert_eq!(db.context_rows("kept").unwrap().len(), 1);
        assert!(db.kv_get("context_on:kept").unwrap().is_some());
        assert!(db.kv_get("fork_name:gone-ish").unwrap().is_some());
    }

    #[test]
    fn a_new_db_and_its_folder_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("robot-overlord");
        let path = dir.join("state.db");
        let db = Db::open(&path).unwrap();
        db.kv_set("k", "v").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        // SQLite makes these with the DB file's mode.
        for side in ["-wal", "-shm"] {
            let p = dir.join(format!("state.db{side}"));
            assert_eq!(mode(&p), 0o600, "{side}");
        }
    }

    #[test]
    fn migrate_is_idempotent_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(Db::open(&path).unwrap());
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 12);
    }

    #[test]
    fn subagent_upsert_merges_sources() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01aaa", "alpha");
        let base = Subagent {
            agent_id: "a1".into(),
            session_id: "01aaa".into(),
            agent_type: "Explore".into(),
            description: String::new(),
            tool_use_id: None,
            spawn_depth: 1,
            background: false,
            started_at: Some(100),
            finished_at: None,
            transcript_path: None,
            result: String::new(),
            input_tokens: 0,
            output_tokens: 0,
        };
        db.upsert_subagent(&base).unwrap(); // hook start
        db.upsert_subagent(&Subagent {
            description: "Find X".into(),
            tool_use_id: Some("toolu_1".into()),
            background: true,
            started_at: None,
            ..base.clone()
        })
        .unwrap(); // meta.json
        let s = db.resolve_ref("alpha").unwrap();
        assert_eq!((s.agents_running, s.agents_total), (1, 1));
        db.upsert_subagent(&Subagent {
            finished_at: Some(500),
            result: "/path".into(),
            output_tokens: 260,
            ..base.clone()
        })
        .unwrap(); // hook stop
        let a = &db.subagents("01aaa").unwrap()[0];
        assert_eq!(a.description, "Find X");
        assert_eq!(a.started_at, Some(100));
        assert_eq!(a.finished_at, Some(500));
        assert!(a.background);
        assert_eq!(a.output_tokens, 260);
        let s = db.resolve_ref("alpha").unwrap();
        assert_eq!((s.agents_running, s.agents_total), (0, 1));
    }

    #[test]
    fn description_and_notes() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01aaa", "alpha");
        db.set_description("01aaa", "fixing the digest job")
            .unwrap();
        db.set_notes("01aaa", "line one\nline two").unwrap();
        let s = db.resolve_ref("alpha").unwrap();
        assert_eq!(s.description, "fixing the digest job");
        assert_eq!(s.notes.lines().count(), 2);
        assert_eq!(s.model, None); // launch_spec "{}"
    }

    #[test]
    fn repos_are_mru_ordered() {
        let db = Db::open_in_memory().unwrap();
        db.touch_repo("/a", 100).unwrap();
        db.touch_repo("/b", 200).unwrap();
        db.touch_repo("/a", 300).unwrap();
        let r = db.list_repos().unwrap();
        assert_eq!(r[0].path, "/a");
        assert_eq!(r[0].use_count, 2);
        assert_eq!(r[1].path, "/b");
    }

    #[test]
    fn lifecycle_and_derived_timestamps() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01aaa", "alpha");
        let s = db.resolve_ref("alpha").unwrap();
        assert_eq!(s.id, "01aaa");
        assert_eq!(s.status, Status::Starting);
        assert_eq!(s.agent_session_trust, "minted");

        assert!(db.set_status("01aaa", Status::Working, 2000).unwrap());
        assert!(!db.set_status("01aaa", Status::Working, 2001).unwrap()); // unchanged
        db.touch_last_user_msg("01aaa", 2000).unwrap();
        db.touch_last_agent_msg("01aaa", 3000).unwrap();
        let s = db.resolve_ref("01a").unwrap(); // unique prefix
        assert_eq!(s.status, Status::Working);
        assert_eq!(s.last_user_msg_at, Some(2000));
        assert_eq!(s.last_agent_msg_at, Some(3000));
        assert_eq!(s.last_state_change_at, Some(2000));

        db.insert_event("01aaa", 2000, "prompt_submitted", "{}")
            .unwrap();
        db.insert_event("01aaa", 3000, "agent_stopped", r#"{"x":1}"#)
            .unwrap();
        let evs = db.list_events("01aaa", 10).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].kind, "prompt_submitted"); // chronological

        db.set_archived("01aaa", Some(4000)).unwrap();
        assert!(db.list_sessions(false).unwrap().is_empty());
        assert_eq!(db.list_sessions(true).unwrap().len(), 1);
    }

    #[test]
    fn query_filters_and_sorts() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01aaa", "alpha");
        mk(&db, "01bbb", "beta");
        mk(&db, "01ccc", "gamma");
        db.touch_last_agent_msg("01aaa", 5000).unwrap();
        db.touch_last_agent_msg("01bbb", 9000).unwrap();
        db.set_pinned("01ccc", true).unwrap();
        db.add_tag("01aaa", "wip").unwrap();
        db.add_tag("01aaa", "backend").unwrap();
        db.set_status("01bbb", Status::WaitingInput, 1).unwrap();

        // Pinned floats first; then activity order.
        let all = db.sessions(&SessionQuery::default()).unwrap();
        let ids: Vec<&str> = all.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["01ccc", "01bbb", "01aaa"]);
        assert_eq!(all[2].tags, ["backend", "wip"]);

        // Tag filter.
        let tagged = db
            .sessions(&SessionQuery {
                tag: Some("wip".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(tagged.len(), 1);
        assert_eq!(tagged[0].id, "01aaa");

        // Status filter.
        let waiting = db
            .sessions(&SessionQuery {
                status: vec!["waiting_input".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].id, "01bbb");

        // Name sort.
        let named = db
            .sessions(&SessionQuery {
                sort: SortMode::Name,
                ..Default::default()
            })
            .unwrap();
        // gamma is pinned so still first; then alpha, beta.
        let ids: Vec<&str> = named.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["01ccc", "01aaa", "01bbb"]);

        // Archived views.
        db.set_archived("01aaa", Some(1)).unwrap();
        assert_eq!(db.sessions(&SessionQuery::default()).unwrap().len(), 2);
        let only = db
            .sessions(&SessionQuery {
                archived: ArchivedMode::Only,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].id, "01aaa");

        // Rename locks the name.
        db.rename("01bbb", "beta-renamed").unwrap();
        let s = db.resolve_ref("beta-renamed").unwrap();
        assert!(s.name_locked);
    }

    #[test]
    fn rich_search_text() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01aaa", "alpha");
        mk(&db, "01bbb", "beta");
        db.add_tag("01aaa", "wip").unwrap();
        db.set_status("01bbb", Status::WaitingInput, 1).unwrap();
        db.set_group("01bbb", "work/backend").unwrap();

        let mut q = SessionQuery::default();
        q.apply_search_text("tag:wip");
        assert_eq!(db.sessions(&q).unwrap()[0].id, "01aaa");

        let mut q = SessionQuery::default();
        q.apply_search_text("status:waiting");
        assert_eq!(q.status, ["waiting_input"]);
        // The words people see are accepted too.
        for (word, stored) in [
            ("needs", "waiting_input"),
            ("needs-you", "waiting_input"),
            ("waiting_input", "waiting_input"),
            ("your-turn", "idle"),
            ("turn", "idle"),
            ("idle", "idle"),
            ("working", "working"),
            ("bg", "background"),
        ] {
            assert_eq!(normalize_status(word), stored, "{word}");
        }
        assert_eq!(db.sessions(&q).unwrap()[0].id, "01bbb");

        let mut q = SessionQuery::default();
        q.apply_search_text("group:work sort:name");
        assert_eq!(q.sort, SortMode::Name);
        assert_eq!(db.sessions(&q).unwrap().len(), 1);

        // Free text matches tags and project too; every word must match.
        let mut q = SessionQuery::default();
        q.apply_search_text("wip /tmp");
        assert_eq!(q.search.as_deref(), Some("wip /tmp"));
        assert_eq!(db.sessions(&q).unwrap().len(), 1);
        let mut q = SessionQuery::default();
        q.apply_search_text("zzz");
        assert!(db.sessions(&q).unwrap().is_empty());

        // Reverse flips the order.
        let fwd = db
            .sessions(&SessionQuery {
                sort: SortMode::Name,
                ..Default::default()
            })
            .unwrap();
        let rev = db
            .sessions(&SessionQuery {
                sort: SortMode::Name,
                reverse: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(fwd[0].id, rev[1].id);
    }

    #[test]
    fn ref_resolution_ambiguity() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01abc", "x1");
        mk(&db, "01abd", "x2");
        assert!(db.resolve_ref("01ab").is_err()); // ambiguous prefix
        assert!(db.resolve_ref("01abc").is_ok());
        assert!(db.resolve_ref("zzz").is_err());
    }

    #[test]
    fn overlord_hidden_from_queries_and_kv_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01a", "a");
        db.insert_session(&NewSession {
            id: "01o",
            name: "overlord",
            agent: "claude",
            agent_session: None,
            project_path: "/tmp/o",
            tmux_session: Some("rlord_overlord"),
            launch_spec: "{}",
            created_at: 1000,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role: "overlord",
        })
        .unwrap();
        let ids: Vec<String> = db
            .sessions(&SessionQuery::default())
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, ["01a"]);
        let all = db
            .sessions(&SessionQuery {
                include_overlord: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(db.overlord().unwrap().unwrap().id, "01o");

        assert_eq!(db.kv_get("k").unwrap(), None);
        db.kv_set("k", "1").unwrap();
        db.kv_set("k", "2").unwrap();
        assert_eq!(db.kv_get("k").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn live_transcripts_know_when_the_agent_last_started() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01aaa", "a");
        db.upsert_transcript_source("01aaa", "/t/a.jsonl").unwrap();
        // Stopped: listed as such, with since when.
        db.set_status("01aaa", Status::Stopped, 7).unwrap();
        let l = &db.live_transcripts().unwrap()[0];
        assert!(l.stopped);
        assert_eq!(l.changed_at, 7);
        db.set_status("01aaa", Status::Idle, 8).unwrap();
        assert!(!db.live_transcripts().unwrap()[0].stopped);
        assert_eq!(db.live_transcripts().unwrap()[0].started_at, 0);
        db.insert_event("01aaa", 100, "restarted", "{}").unwrap();
        db.insert_event(
            "01aaa",
            200,
            "session_start",
            r#"{"hook":"SessionStart","start":"startup"}"#,
        )
        .unwrap();
        // A compaction, a /clear or an in-process /resume may keep the
        // same process.
        db.insert_event(
            "01aaa",
            250,
            "session_start",
            r#"{"hook":"SessionStart","start":"resume"}"#,
        )
        .unwrap();
        db.insert_event(
            "01aaa",
            300,
            "session_start",
            r#"{"hook":"SessionStart","start":"compact"}"#,
        )
        .unwrap();
        db.insert_event("01aaa", 400, "session_start", r#"{"hook":"SessionStart"}"#)
            .unwrap();
        assert_eq!(db.live_transcripts().unwrap()[0].started_at, 200);
        db.insert_event("01aaa", 500, "reforked", "{}").unwrap();
        assert_eq!(db.live_transcripts().unwrap()[0].started_at, 500);
        // A fork starts a process of its own.
        db.insert_event("01aaa", 600, "forked", "{}").unwrap();
        assert_eq!(db.live_transcripts().unwrap()[0].started_at, 600);
    }
}
