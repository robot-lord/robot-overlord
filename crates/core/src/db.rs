use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};

use crate::types::{Session, Status};

/// Embedded, ordered migrations. `user_version` tracks how many have run.
const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

const SESSION_COLS: &str = "id, name, name_locked, agent, agent_session, agent_session_trust, \
     project_path, tmux_session, status, group_path, pinned, created_at, archived_at, \
     last_user_msg_at, last_agent_msg_at, last_state_change_at";

pub struct NewSession<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub agent: &'a str,
    pub agent_session: Option<&'a str>,
    pub project_path: &'a str,
    pub tmux_session: &'a str,
    pub launch_spec: &'a str,
    pub created_at: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct EventRow {
    pub seq: i64,
    pub ts: i64,
    pub kind: String,
    pub payload: serde_json::Value,
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
    /// Most recent agent activity first (the default).
    #[default]
    Activity,
    /// Most recent user prompt first.
    User,
    /// Most recent status change first.
    State,
    Created,
    Name,
}

impl SortMode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "activity" | "agent" => SortMode::Activity,
            "user" => SortMode::User,
            "state" | "status" => SortMode::State,
            "created" | "age" => SortMode::Created,
            "name" => SortMode::Name,
            _ => return None,
        })
    }
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Activity => "activity",
            SortMode::User => "user",
            SortMode::State => "state",
            SortMode::Created => "created",
            SortMode::Name => "name",
        }
    }
    pub fn next(self) -> Self {
        match self {
            SortMode::Activity => SortMode::User,
            SortMode::User => SortMode::State,
            SortMode::State => SortMode::Created,
            SortMode::Created => SortMode::Name,
            SortMode::Name => SortMode::Activity,
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
    /// Free text: matches name, project, group, or any tag.
    pub search: Option<String>,
    pub sort: SortMode,
    pub reverse: bool,
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

/// Accept friendly status spellings (`waiting` → `waiting_input`).
pub fn normalize_status(s: &str) -> String {
    match s.trim() {
        "waiting" | "wait" | "input" => "waiting_input".to_string(),
        "run" | "busy" => "working".to_string(),
        other => other.to_string(),
    }
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
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
                                   status, launch_spec, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'starting', ?7, ?8)",
            params![
                s.id,
                s.name,
                s.agent,
                s.agent_session,
                s.project_path,
                s.tmux_session,
                s.launch_spec,
                s.created_at
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
        let by_name: Vec<Session> = self.query_sessions("name = ?1", r)?;
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
        Ok(sessions)
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
            // Every word must match somewhere (name/project/group/tag).
            for word in text.split_whitespace() {
                conds.push(
                    "(name LIKE '%' || ? || '%' OR project_path LIKE '%' || ? || '%' \
                      OR group_path LIKE '%' || ? || '%' \
                      OR EXISTS (SELECT 1 FROM tags t WHERE t.session_id = sessions.id \
                                 AND t.tag LIKE '%' || ? || '%'))"
                        .into(),
                );
                for _ in 0..4 {
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
            SortMode::Activity => (
                "COALESCE(last_agent_msg_at, last_user_msg_at, created_at)",
                "DESC",
            ),
            SortMode::User => ("COALESCE(last_user_msg_at, created_at)", "DESC"),
            SortMode::State => ("COALESCE(last_state_change_at, created_at)", "DESC"),
            SortMode::Created => ("created_at", "DESC"),
            SortMode::Name => ("name COLLATE NOCASE", "ASC"),
        };
        let dir = match (natural, q.reverse) {
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
        Ok(sessions)
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

    pub fn launch_spec(&self, id: &str) -> Result<String> {
        Ok(self.conn.query_row(
            "SELECT launch_spec FROM sessions WHERE id = ?1",
            [id],
            |r| r.get(0),
        )?)
    }

    /// Update status; stamps last_state_change_at and returns true iff changed.
    pub fn set_status(&self, id: &str, status: Status, ts: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE sessions SET status = ?2, last_state_change_at = ?3 \
             WHERE id = ?1 AND status != ?2",
            params![id, status.as_str(), ts],
        )?;
        Ok(n > 0)
    }

    pub fn set_agent_session(&self, id: &str, agent_session: &str, trust: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET agent_session = ?2, agent_session_trust = ?3 WHERE id = ?1",
            params![id, agent_session, trust],
        )?;
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

    // ---- transcripts ----

    pub fn upsert_transcript_source(&self, session_id: &str, source_path: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO transcripts (session_id, source_path) VALUES (?1, ?2) \
             ON CONFLICT(session_id) DO UPDATE SET source_path = excluded.source_path",
            params![session_id, source_path],
        )?;
        Ok(())
    }
}

fn row_to_session(r: &rusqlite::Row) -> rusqlite::Result<Session> {
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
        tags: Vec::new(),
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
            tmux_session: &format!("rlord_{id}"),
            launch_spec: "{}",
            created_at: 1000,
        })
        .unwrap();
    }

    #[test]
    fn migrations_apply_and_tables_exist() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), 1);
        for table in ["sessions", "tags", "events", "messages", "transcripts"] {
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
    fn migrate_is_idempotent_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(Db::open(&path).unwrap());
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 1);
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
}
