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

    /// Resolve a user-supplied ref: exact id, exact name, then unique id prefix.
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
            1 => Ok(by_prefix.into_iter().next().unwrap()),
            0 => bail!("no session matches '{r}'"),
            n => bail!("id prefix '{r}' is ambiguous ({n} sessions)"),
        }
    }

    fn get_session_where(&self, cond: &str, arg: &str) -> Result<Option<Session>> {
        Ok(self.query_sessions(cond, arg)?.into_iter().next())
    }

    fn query_sessions(&self, cond: &str, arg: &str) -> Result<Vec<Session>> {
        let sql = format!("SELECT {SESSION_COLS} FROM sessions WHERE {cond}");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([arg], row_to_session)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn list_sessions(&self, include_archived: bool) -> Result<Vec<Session>> {
        let where_clause = if include_archived {
            ""
        } else {
            "WHERE archived_at IS NULL"
        };
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions {where_clause} \
             ORDER BY pinned DESC, COALESCE(last_agent_msg_at, created_at) DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_session)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
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
            tmux_session: &format!("ovl_{id}"),
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
    fn ref_resolution_ambiguity() {
        let db = Db::open_in_memory().unwrap();
        mk(&db, "01abc", "x1");
        mk(&db, "01abd", "x2");
        assert!(db.resolve_ref("01ab").is_err()); // ambiguous prefix
        assert!(db.resolve_ref("01abc").is_ok());
        assert!(db.resolve_ref("zzz").is_err());
    }
}
