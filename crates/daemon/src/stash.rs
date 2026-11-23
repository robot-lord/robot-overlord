//! Delivering to a Claude session past a draft, with Claude Code's own
//! prompt stash (`chat:stash`, Ctrl+S): with text in the input, Ctrl+S
//! stashes it and clears the box ("draft stashed" shows below it);
//! pressed again on an empty box, it puts the draft back as it was.
//!
//! The user's decision: rather than hold a message for as long as a
//! Claude session's input holds unsent text, rlord stashes the draft,
//! types the message, and restores the draft once the turn is over.
//! Codex and OpenCode have no such stash: a draft still holds them.
//!
//! What was checked live (Claude Code 2.1.327): the stash is ONE slot
//! (stashing again overwrites, and the first draft is lost), and Ctrl+S
//! about a second after Enter didn't restore (the status read
//! "draft restored · draft stashed", the box stayed empty, the stash
//! stayed); once the turn had settled it restored. So:
//!
//! - **Stash** (`plan`, from `send::type_into`) only when Ctrl+S is the
//!   stash key (`~/.claude/keybindings.json` read, never written:
//!   `stash_key_ok`), nothing is stashed already (no "draft stashed" on
//!   screen, no restore of ours pending), and the draft has sat unchanged
//!   for `DRAFT_IDLE` (so text being written is never taken). After the
//!   press, the box must be empty and "draft stashed" showing, or nothing
//!   is typed.
//! - **Restore** (`tick`, from `send`'s slow tick) only when the box is
//!   empty, "draft stashed" shows, the screen is at its prompt, and the
//!   session's turn has been over a while (`restore_step`). Never with
//!   text in the box: that press would stash over the draft. Retried with
//!   backoff; after `RESTORE_TRIES` presses or `RESTORE_FOR_MS`, or when
//!   the indicator is gone without the draft back, it gives up: logged
//!   (`draft_restore_failed`) and marked on the session's row ("draft
//!   stashed: Ctrl+S to restore", `annotate`).
//!
//! Events: `draft_stashed`, `draft_restored`, `draft_restore_failed`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use rlord_core::agents::{strip_ansi, Agent};
use rlord_core::tmux;
use rlord_core::types::{now_ms, Status};

use crate::Daemon;

/// kv key: drafts rlord stashed and hasn't restored yet, `{session id:
/// {"first_line", "at", "tries", "next_at", "failed"?, "failed_at"?}}`.
pub const STASH_KEY: &str = "draft_stash";

/// How long a draft must sit unchanged before rlord stashes it.
pub const DRAFT_IDLE: Duration = Duration::from_secs(30);

/// How long the session's turn must have been over before a restore.
const RESTORE_IDLE_MS: i64 = 3_000;

/// Presses of Ctrl+S that didn't bring the draft back before giving up.
const RESTORE_TRIES: u64 = 6;

/// How long after the stash a restore is tried at all.
const RESTORE_FOR_MS: i64 = 30 * 60_000;

/// How long a failed restore's mark stays on the row (unless the draft
/// shows back first).
const FAILED_KEEP_MS: i64 = 2 * 3_600_000;

/// Time for Claude to redraw after a key.
pub const REDRAW: Duration = Duration::from_millis(400);

/// The draft first seen in each session's input, and when: how long it
/// has sat unchanged.
static DRAFTS: Mutex<Option<HashMap<String, (String, Instant)>>> = Mutex::new(None);

/// How long session `id`'s input has held `text` unchanged (zero the
/// first time it is seen).
pub(crate) fn draft_age(id: &str, text: &str) -> Duration {
    let mut g = DRAFTS.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    match m.get(id) {
        Some((t, at)) if t == text => at.elapsed(),
        _ => {
            m.insert(id.to_string(), (text.to_string(), Instant::now()));
            Duration::ZERO
        }
    }
}

/// Session `id`'s input holds no draft: forget the one seen.
pub(crate) fn forget_draft(id: &str) {
    let mut g = DRAFTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = g.as_mut() {
        m.remove(id);
    }
}

/// Claude Code's keybindings file (read only).
fn keybindings_path() -> PathBuf {
    if cfg!(test) {
        // Tests never read the user's own.
        return PathBuf::from("/nonexistent/rlord-test/keybindings.json");
    }
    rlord_core::agents::claude::config_dir().join("keybindings.json")
}

/// Whether Ctrl+S is Claude Code's stash key per its keybindings file.
pub(crate) fn stash_key_ok() -> bool {
    stash_key_ok_at(&keybindings_path())
}

fn stash_key_ok_at(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(text) => stash_bound(Some(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => stash_bound(None),
        Err(_) => false,
    }
}

/// Whether Ctrl+S stashes, given the keybindings file's text (None: no
/// file, the defaults). Unreadable JSON, or `ctrl+s` bound in the `Chat`
/// or `Global` context to anything but `chat:stash` (null unbinds it):
/// no. (Another key bound to `chat:stash` doesn't take Ctrl+S away.)
pub(crate) fn stash_bound(file: Option<&str>) -> bool {
    let Some(text) = file else {
        return true;
    };
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let Some(blocks) = v["bindings"].as_array() else {
        return v.get("bindings").is_none();
    };
    blocks
        .iter()
        .filter(|b| matches!(b["context"].as_str(), Some("Chat" | "Global")))
        .filter_map(|b| b["bindings"].as_object())
        .flat_map(|m| m.iter())
        .filter(|(k, _)| is_ctrl_s(k))
        .all(|(_, act)| act.as_str() == Some("chat:stash"))
}

/// `ctrl+s` as the keybindings file may spell it (any case, `control`).
fn is_ctrl_s(key: &str) -> bool {
    let k = key.trim().to_ascii_lowercase();
    matches!(k.as_str(), "ctrl+s" | "control+s")
}

/// Whether Claude's screen (plain) shows a stashed draft: below the
/// input line (from just above it), the last of "draft stashed" / "draft
/// restored" is "stashed". ("draft restored · draft stashed" was seen
/// live after a restore that didn't take: still stashed.)
pub(crate) fn stash_shown(screen: &str) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    let Some(input) = lines.iter().rposition(|l| l.trim_start().starts_with('❯')) else {
        return false;
    };
    let tail = lines[input.saturating_sub(2)..].join("\n").to_lowercase();
    match (tail.rfind("draft stashed"), tail.rfind("draft restored")) {
        (Some(s), Some(r)) => s > r,
        (Some(_), None) => true,
        _ => false,
    }
}

/// What to do with a Claude session's draft when a message is due.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Stash,
    /// Not now: why (said to the overlord, and on the row).
    Hold(String),
}

/// Whether to stash a draft (`age`: how long it has sat unchanged) on
/// this screen (plain). `key_ok`: Ctrl+S is the stash key. `pending`: a
/// draft rlord stashed earlier isn't back yet.
pub(crate) fn plan(screen: &str, key_ok: bool, pending: bool, age: Duration) -> Plan {
    if !key_ok {
        return Plan::Hold(
            "its input holds unsent text (a draft), and Ctrl+S isn't Claude Code's prompt stash \
             key there (keybindings.json), so rlord can't set the draft aside"
                .into(),
        );
    }
    if pending || stash_shown(screen) {
        return Plan::Hold(
            "its input holds unsent text (a draft), and Claude Code's prompt stash already holds \
             another one (stashing again would lose it)"
                .into(),
        );
    }
    if age < DRAFT_IDLE {
        return Plan::Hold(format!(
            "its input holds unsent text (a draft) the user may still be writing; once it has \
             sat unchanged for {}s rlord stashes it (Ctrl+S), delivers, and restores it",
            DRAFT_IDLE.as_secs()
        ));
    }
    Plan::Stash
}

/// Whether the input (its first line, as `remote::input_text` reads it)
/// is the stashed draft back.
fn same_draft(input: &str, first_line: &str) -> bool {
    let a: String = input.trim().chars().take(24).collect();
    let b: String = first_line.trim().chars().take(24).collect();
    let n = a.chars().count().min(b.chars().count());
    n > 0 && a.chars().take(n).eq(b.chars().take(n))
}

/// The next step of a restore.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Restore {
    /// Press Ctrl+S now.
    Press,
    /// Not now (text in the box, a dialog up, the turn not over).
    Wait,
    /// The draft is back (someone restored it).
    Back,
    /// No stash shows and the box is empty: it can't be restored by rlord.
    Lost,
}

/// What to do for a stashed draft whose first line is `first_line`, on
/// a screen whose input holds `input` (`remote::input_text`), shows the
/// stash or not, is at its prompt or not, with the turn over a while
/// (`settled`) or not.
pub(crate) fn restore_step(
    input: Option<&str>,
    shown: bool,
    at_prompt: bool,
    settled: bool,
    first_line: &str,
) -> Restore {
    match input {
        Some(t) if !shown && same_draft(t, first_line) => Restore::Back,
        // Text in the box: a press would stash over the draft.
        Some(_) => Restore::Wait,
        None if !(at_prompt && settled) => Restore::Wait,
        None if shown => Restore::Press,
        None => Restore::Lost,
    }
}

/// Backoff before the next press after `tries` that didn't take.
fn backoff_ms(tries: u64) -> i64 {
    (2_000i64 << tries.min(5)).min(60_000)
}

/// A draft was stashed in session `id` and the message typed: remember
/// it for the restore, and log it.
pub(crate) fn stashed(d: &Daemon, id: &str, draft: &str) -> Result<()> {
    let db = d.db();
    let now = now_ms();
    let mut m = db.kv_map(STASH_KEY)?;
    m.insert(
        id.to_string(),
        json!({"first_line": draft, "at": now, "tries": 0, "next_at": now}),
    );
    db.kv_set_map(STASH_KEY, &m)?;
    db.insert_event(
        id,
        now,
        "draft_stashed",
        &json!({"chars": draft.chars().count()}).to_string(),
    )?;
    forget_draft(id);
    Ok(())
}

/// Whether a draft rlord stashed in session `id` isn't back yet (failed
/// ones included: their stash may still hold it).
pub(crate) fn pending(d: &Daemon, id: &str) -> bool {
    d.db().kv_map(STASH_KEY).is_ok_and(|m| m.contains_key(id))
}

/// Annotate list rows with `draft_stash`: "pending" (rlord restores it
/// after the turn) or "failed" (the user restores it: Ctrl+S), else null.
pub fn annotate(d: &Daemon, rows: &mut [Value]) {
    let m = d.db().kv_map(STASH_KEY).unwrap_or_default();
    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        row["draft_stash"] = match m.get(id) {
            Some(e) if e["failed"] == true => json!("failed"),
            Some(_) => json!("pending"),
            None => Value::Null,
        };
    }
}

/// Restore the drafts rlord stashed, where it can now.
pub(crate) fn tick(d: &Daemon) -> Result<()> {
    let ids: Vec<String> = d.db().kv_map(STASH_KEY)?.keys().cloned().collect();
    for id in ids {
        if let Err(e) = restore(d, &id) {
            tracing::debug!(session = %id, error = %e, "restoring a stashed draft");
        }
    }
    Ok(())
}

/// One look at session `id`'s stashed draft, under its lock.
fn restore(d: &Daemon, id: &str) -> Result<()> {
    let _session = d.sessions.lock(id);
    let now = now_ms();
    let (s, entry) = {
        let db = d.db();
        let Some(entry) = db.kv_map(STASH_KEY)?.get(id).cloned() else {
            return Ok(());
        };
        (db.resolve_ref(id).ok(), entry)
    };
    let gone = s.as_ref().is_none_or(|s| {
        s.archived_at.is_some() || matches!(s.status, Status::Stopped | Status::Error)
    });
    let failed = entry["failed"] == true;
    let first_line = entry["first_line"].as_str().unwrap_or_default().to_string();
    if gone || (failed && now - entry["failed_at"].as_i64().unwrap_or(0) > FAILED_KEEP_MS) {
        return forget(d, id);
    }
    let s = s.expect("checked above");
    let Some(t) = s.tmux_session.as_deref().filter(|t| tmux::has_session(t)) else {
        return Ok(());
    };
    let pane = tmux::pane_id(t)?;
    let ansi = tmux::capture_pane_id_ansi(&pane)?;
    let plain = strip_ansi(&ansi);
    let input = crate::remote::input_text(&ansi);
    let shown = stash_shown(&plain);
    if failed {
        // The mark stays until the draft shows back.
        if input.as_deref().is_some_and(|i| same_draft(i, &first_line)) {
            return forget(d, id);
        }
        return Ok(());
    }
    let settled = s.status == Status::Idle
        && s.last_state_change_at
            .is_none_or(|t| now - t >= RESTORE_IDLE_MS)
        && crate::send::settled_since_typed(id)
        && !crate::send::user_typing(t);
    let at_prompt = Agent::Claude.at_prompt(&plain);
    match restore_step(input.as_deref(), shown, at_prompt, settled, &first_line) {
        Restore::Back => restored(d, id, "other"),
        Restore::Lost => give_up(d, id, "the stash no longer shows and the box is empty"),
        Restore::Wait => {
            if now - entry["at"].as_i64().unwrap_or(now) > RESTORE_FOR_MS {
                return give_up(d, id, "no moment to restore it came in time");
            }
            Ok(())
        }
        Restore::Press => {
            if now < entry["next_at"].as_i64().unwrap_or(0) {
                return Ok(());
            }
            {
                let _typing = crate::send::typing();
                tmux::send_keys(&pane, &["C-s"])?;
                std::thread::sleep(REDRAW);
            }
            let after = tmux::capture_pane_id_ansi(&pane)?;
            if crate::remote::input_text(&after).is_some_and(|i| same_draft(&i, &first_line)) {
                return restored(d, id, "rlord");
            }
            let tries = entry["tries"].as_u64().unwrap_or(0) + 1;
            if tries >= RESTORE_TRIES || now - entry["at"].as_i64().unwrap_or(now) > RESTORE_FOR_MS
            {
                return give_up(d, id, "Ctrl+S didn't bring it back");
            }
            let db = d.db();
            let mut m = db.kv_map(STASH_KEY)?;
            if let Some(e) = m.get_mut(id).and_then(Value::as_object_mut) {
                e.insert("tries".into(), json!(tries));
                e.insert("next_at".into(), json!(now + backoff_ms(tries)));
            }
            db.kv_set_map(STASH_KEY, &m)
        }
    }
}

fn forget(d: &Daemon, id: &str) -> Result<()> {
    let db = d.db();
    let mut m = db.kv_map(STASH_KEY)?;
    if m.remove(id).is_some() {
        db.kv_set_map(STASH_KEY, &m)?;
        drop(db);
        d.changed();
    }
    Ok(())
}

fn restored(d: &Daemon, id: &str, by: &str) -> Result<()> {
    d.db().insert_event(
        id,
        now_ms(),
        "draft_restored",
        &json!({"by": by}).to_string(),
    )?;
    forget(d, id)
}

fn give_up(d: &Daemon, id: &str, why: &str) -> Result<()> {
    let db = d.db();
    let now = now_ms();
    let mut m = db.kv_map(STASH_KEY)?;
    if let Some(e) = m.get_mut(id).and_then(Value::as_object_mut) {
        e.insert("failed".into(), json!(true));
        e.insert("failed_at".into(), json!(now));
    }
    db.kv_set_map(STASH_KEY, &m)?;
    db.insert_event(
        id,
        now,
        "draft_restore_failed",
        &json!({"reason": why}).to_string(),
    )?;
    drop(db);
    tracing::info!(session = %id, why, "a stashed draft wasn't restored");
    d.changed();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude's idle screen with `input` in the box and `status` on the
    /// line under it.
    fn screen(input: &str, status: &str) -> String {
        let rule = "─".repeat(60);
        [
            "● Draft stashed is a phrase in the transcript, far above.".to_string(),
            String::new(),
            "● Done.".into(),
            rule.clone(),
            format!("❯ {input}"),
            rule,
            format!("  {status}"),
            "  -- INSERT -- ⏵⏵ auto mode on".into(),
        ]
        .join("\n")
    }

    #[test]
    fn the_stash_indicator_is_read_below_the_box() {
        assert!(!stash_shown(&screen("", "Opus 5.5 | Context 6%")));
        assert!(stash_shown(&screen("", "draft stashed")));
        // Seen live after a restore that didn't take.
        assert!(stash_shown(&screen("", "draft restored · draft stashed")));
        assert!(!stash_shown(&screen(
            "half",
            "draft stashed · draft restored"
        )));
        assert!(!stash_shown("no prompt at all\ndraft stashed"));
    }

    #[test]
    fn ctrl_s_is_the_stash_key_unless_rebound() {
        assert!(stash_bound(None));
        assert!(stash_bound(Some(r#"{"bindings": []}"#)));
        let chat = |k: &str, v: &str| {
            format!(r#"{{"bindings": [{{"context": "Chat", "bindings": {{"{k}": {v}}}}}]}}"#)
        };
        assert!(stash_bound(Some(&chat(
            "ctrl+e",
            "\"chat:externalEditor\""
        ))));
        assert!(stash_bound(Some(&chat("ctrl+s", "\"chat:stash\""))));
        assert!(!stash_bound(Some(&chat("ctrl+s", "null"))));
        assert!(!stash_bound(Some(&chat("Ctrl+S", "\"chat:submit\""))));
        // Moving the stash elsewhere leaves Ctrl+S's default alone.
        assert!(stash_bound(Some(&chat("ctrl+k", "\"chat:stash\""))));
        let global =
            r#"{"bindings": [{"context": "Global", "bindings": {"ctrl+s": "app:redraw"}}]}"#;
        assert!(!stash_bound(Some(global)));
        let other = r#"{"bindings": [{"context": "Agents", "bindings": {"ctrl+s": null}}]}"#;
        assert!(stash_bound(Some(other)));
        assert!(!stash_bound(Some("{not json")));
        assert!(
            !stash_key_ok_at(Path::new("/")),
            "a directory can't be read"
        );
        assert!(stash_key_ok_at(Path::new(
            "/nonexistent/rlord/keybindings.json"
        )));
    }

    #[test]
    fn a_draft_is_stashed_only_when_safe() {
        let idle = screen("half a thought", "Opus 5.5");
        let old = DRAFT_IDLE + Duration::from_secs(1);
        assert_eq!(plan(&idle, true, false, old), Plan::Stash);
        assert!(matches!(plan(&idle, false, false, old), Plan::Hold(_)));
        assert!(matches!(plan(&idle, true, true, old), Plan::Hold(_)));
        assert!(matches!(
            plan(&screen("half", "draft stashed"), true, false, old),
            Plan::Hold(_)
        ));
        let Plan::Hold(why) = plan(&idle, true, false, Duration::from_secs(3)) else {
            panic!("a fresh draft must hold");
        };
        assert!(why.contains("still be writing"));
    }

    #[test]
    fn a_draft_ages_only_while_unchanged() {
        assert_eq!(draft_age("t-age", "one"), Duration::ZERO);
        std::thread::sleep(Duration::from_millis(5));
        assert!(draft_age("t-age", "one") > Duration::ZERO);
        assert_eq!(draft_age("t-age", "one two"), Duration::ZERO);
        forget_draft("t-age");
        assert_eq!(draft_age("t-age", "one two"), Duration::ZERO);
    }

    #[test]
    fn restore_never_presses_over_text() {
        let line = "fix the login bug";
        assert_eq!(restore_step(None, true, true, true, line), Restore::Press);
        assert_eq!(restore_step(None, true, true, false, line), Restore::Wait);
        assert_eq!(restore_step(None, true, false, true, line), Restore::Wait);
        assert_eq!(
            restore_step(Some("new text"), true, true, true, line),
            Restore::Wait
        );
        assert_eq!(
            restore_step(Some("new text"), false, true, true, line),
            Restore::Wait
        );
        assert_eq!(
            restore_step(Some(line), false, true, true, line),
            Restore::Back
        );
        // Back but the stash shows still: not sure, so wait.
        assert_eq!(
            restore_step(Some(line), true, true, true, line),
            Restore::Wait
        );
        assert_eq!(restore_step(None, false, true, true, line), Restore::Lost);
        assert_eq!(restore_step(None, false, true, false, line), Restore::Wait);
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_ms(0), 2_000);
        assert_eq!(backoff_ms(1), 4_000);
        assert_eq!(backoff_ms(9), 60_000);
    }
}
