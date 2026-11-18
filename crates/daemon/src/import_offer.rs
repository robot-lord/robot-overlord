//! The first-run offer to import existing work. While rlord has no
//! sessions besides its overlords (initial setup), the TUI explains import
//! and offers it once. `yes` hands the overlord a fixed request that starts
//! its import flow (`overlord.md` "Importing existing work": it checks
//! which agents have conversations, proposes a list, the user approves).
//! Plain code, no AI: a kv row, the session list and, only once those
//! pass, a bounded existence check of the agents' transcripts
//! (`import::has_conversations`: the scan's own checks for Claude Code
//! and Codex, stopping at the first match).

use anyhow::{bail, Result};
use serde_json::{json, Value};

use rlord_core::import::has_conversations;
use rlord_core::types::now_ms;

use crate::{ask, overlord, Daemon};

/// kv row: the offer was answered (`yes:<ms>` or `no:<ms>`).
pub(crate) const KV: &str = "setup_import_offer";

/// What the overlord gets on `yes`. Short and agent-neutral: the
/// procedure lives in `overlord.md`.
pub(crate) const REQUEST: &str = "[rlord import request] I'm new to rlord. \
    Import my existing agent sessions: check which agents have conversations \
    rlord doesn't track yet, then propose the list for me to approve.";

/// `setup.import_offer`: `{"offer": true}` while it hasn't been answered,
/// rlord has no sessions besides its overlords (archived ones count), and
/// there are Claude Code or Codex conversations to import (OpenCode
/// isn't importable).
pub(crate) fn offer(d: &Daemon) -> Result<Value> {
    offer_with(d, &|| {
        // Only once the cheap gate passed.
        let tracked = d.db().agent_session_ids()?;
        // The window the overlord's flow scans first (`scan_agent_sessions`'s
        // default).
        let since = now_ms() - SINCE_DAYS * 24 * 3_600_000;
        Ok(has_conversations(since, &|id| tracked.contains(id)))
    })
}

/// Matches the 30 days `overlord.md` starts the import with.
const SINCE_DAYS: i64 = 30;

fn offer_with(d: &Daemon, has_conversations: &dyn Fn() -> Result<bool>) -> Result<Value> {
    let initial = {
        let db = d.db();
        // (No overlord, nobody to send it to: asked once there is one.)
        db.kv_get(KV)?.is_none()
            && db.overlord()?.is_some()
            && !db
                .list_sessions(true)?
                .iter()
                .any(|s| !overlord::is_overlord_role(&s.role))
    };
    // The file check runs without the DB lock.
    Ok(json!({"offer": initial && has_conversations()?}))
}

/// `setup.import_answer`: `{"answer": "yes"|"no"}`. Recorded either way,
/// so it isn't asked again; `yes` also sends the overlord `REQUEST`.
/// Already answered yes: nothing is sent again.
pub(crate) fn answer(d: &Daemon, params: &Value) -> Result<Value> {
    let a = match params["answer"].as_str() {
        Some(a @ ("yes" | "no")) => a,
        _ => bail!("answer: \"yes\" or \"no\""),
    };
    if d.db().kv_get(KV)?.is_some_and(|v| v.starts_with("yes")) {
        return Ok(json!({"ok": true}));
    }
    // Sent first: if it can't be, nothing is recorded and it's offered
    // again next start.
    if a == "yes" {
        ask::send(d, REQUEST)?;
    }
    d.db().kv_set(KV, &format!("{a}:{}", now_ms()))?;
    Ok(json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlord_core::config::Config;
    use rlord_core::db::Db;

    #[test]
    fn offered_until_answered() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        // No overlord yet: not offered.
        assert_eq!(offer_with(&d, &|| Ok(true)).unwrap()["offer"], false);
        // (Nothing to send to: a yes isn't recorded.)
        assert!(answer(&d, &json!({"answer": "yes"})).is_err());
        assert!(d.db().kv_get(KV).unwrap().is_none());
        assert!(answer(&d, &json!({"answer": "maybe"})).is_err());
        // (`no` sends nothing: no overlord, no model call.)
        answer(&d, &json!({"answer": "no"})).unwrap();
        assert_eq!(offer_with(&d, &|| Ok(true)).unwrap()["offer"], false);
        assert!(d.db().kv_get(KV).unwrap().unwrap().starts_with("no:"));
    }

    fn row(db: &Db, id: &str, role: &str) {
        db.insert_session(&rlord_core::db::NewSession {
            id,
            name: id,
            agent: "claude",
            agent_session: None,
            project_path: "/p",
            tmux_session: None,
            launch_spec: "{}",
            created_at: 1,
            repo_path: None,
            workspace_path: None,
            branch: None,
            role,
        })
        .unwrap();
    }

    #[test]
    fn offered_with_only_an_overlord() {
        let d = Daemon::new(Db::open_in_memory().unwrap(), Config::default());
        row(&d.db(), "o1", overlord::ROLE);
        assert_eq!(offer_with(&d, &|| Ok(true)).unwrap()["offer"], true);
        // Nothing to import: not offered.
        assert_eq!(offer_with(&d, &|| Ok(false)).unwrap()["offer"], false);
        // Already answered yes: a second yes sends nothing (no tmux here).
        d.db().kv_set(KV, "yes:1").unwrap();
        answer(&d, &json!({"answer": "yes"})).unwrap();
        d.db().kv_delete(KV).unwrap();
        row(&d.db(), "s1", "");
        assert_eq!(offer_with(&d, &|| Ok(true)).unwrap()["offer"], false);
    }

    #[test]
    fn the_request_names_the_overlords_flow() {
        assert!(REQUEST.starts_with("[rlord import request]"));
        assert!(include_str!("overlord.md").contains("[rlord import request]"));
    }
}
