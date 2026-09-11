use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::types::{Session, Status};

/// Embedded, ordered migrations. `user_version` tracks how many have run.
const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

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

    pub fn list_sessions(&self, include_archived: bool) -> Result<Vec<Session>> {
        let where_clause = if include_archived {
            ""
        } else {
            "WHERE archived_at IS NULL"
        };
        let sql = format!(
            "SELECT id, name, name_locked, agent, agent_session, project_path, \
                    tmux_session, status, group_path, pinned, created_at, archived_at, \
                    last_user_msg_at, last_agent_msg_at, last_state_change_at \
             FROM sessions {where_clause} \
             ORDER BY pinned DESC, COALESCE(last_agent_msg_at, created_at) DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(Session {
                id: r.get(0)?,
                name: r.get(1)?,
                name_locked: r.get::<_, i64>(2)? != 0,
                agent: r.get(3)?,
                agent_session: r.get(4)?,
                project_path: r.get(5)?,
                tmux_session: r.get(6)?,
                status: parse_status(&r.get::<_, String>(7)?),
                group_path: r.get(8)?,
                pinned: r.get::<_, i64>(9)? != 0,
                created_at: r.get(10)?,
                archived_at: r.get(11)?,
                last_user_msg_at: r.get(12)?,
                last_agent_msg_at: r.get(13)?,
                last_state_change_at: r.get(14)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
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
}
