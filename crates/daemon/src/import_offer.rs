//! The first-run offer to import existing work. While rlord has no
//! sessions besides its overlords (initial setup), the TUI explains import
//! and offers it once. `yes` hands the overlord a fixed request that starts
//! its import flow (`overlord.md` "Importing existing work": it checks
//! which agents have conversations, proposes a list, the user approves).
//! Plain code: one kv row and one session list; no scan, no AI.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use rlord_core::types::now_ms;

use crate::{ask, overlord, Daemon};

/// kv row: the offer was answered (`yes:<ms>` or `no:<ms>`).
pub(crate) const KV: &str = "setup_import_offer";

/// What the overlord gets on `yes`. Short and agent-neutral: the
/// procedure lives in `overlord.md`.
pub(crate) const REQUEST: &str = "[rlord import request] I'm new to rlord. \
    Import my existing agent sessions: check which agents have conversations \
    rlord doesn't track yet, then propose the list for me to approve.";

/// `setup.import_offer`: `{"offer": true}` while it hasn't been answered
/// and rlord has no sessions besides its overlords (archived ones count).
pub(crate) fn offer(d: &Daemon) -> Result<Value> {
    let db = d.db();
    // (No overlord, nobody to send it to: asked once there is one.)
    if db.kv_get(KV)?.is_some() || db.overlord()?.is_none() {
        return Ok(json!({"offer": false}));
    }
    let others = db
        .list_sessions(true)?
        .iter()
        .any(|s| !overlord::is_overlord_role(&s.role));
    Ok(json!({"offer": !others}))
}

/// `setup.import_answer`: `{"answer": "yes"|"no"}`. Recorded either way,
/// so it isn't asked again; `yes` also sends the overlord `REQUEST`.
pub(crate) fn answer(d: &Daemon, params: &Value) -> Result<Value> {
    let a = match params["answer"].as_str() {
        Some(a @ ("yes" | "no")) => a,
        _ => bail!("answer: \"yes\" or \"no\""),
    };
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
        assert_eq!(offer(&d).unwrap()["offer"], false);
        // (Nothing to send to: a yes isn't recorded.)
        assert!(answer(&d, &json!({"answer": "yes"})).is_err());
        assert!(d.db().kv_get(KV).unwrap().is_none());
        assert!(answer(&d, &json!({"answer": "maybe"})).is_err());
        // (`no` sends nothing: no overlord, no model call.)
        answer(&d, &json!({"answer": "no"})).unwrap();
        assert_eq!(offer(&d).unwrap()["offer"], false);
        assert!(d.db().kv_get(KV).unwrap().unwrap().starts_with("no:"));
    }

    #[test]
    fn the_request_names_the_overlords_flow() {
        assert!(REQUEST.starts_with("[rlord import request]"));
        assert!(include_str!("overlord.md").contains("[rlord import request]"));
    }
}
