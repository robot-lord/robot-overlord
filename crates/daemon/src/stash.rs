//! Delivering to a Claude session past a draft, with Claude Code's own
//! prompt stash (`chat:stash`, Ctrl+S): with text in the input, Ctrl+S
//! stashes it and clears the box ("draft stashed" shows on its status
//! line). Claude Code puts the stashed draft back by itself once the
//! next turn is over (seen live, 2.1.331: "draft restored" about 2s
//! after the turn).
//!
//! The user's decision: rather than hold a message for as long as a
//! Claude session's input holds unsent text, rlord stashes the draft and
//! types the message; Claude Code restores the draft. Codex and OpenCode
//! have no such stash: a draft still holds them.
//!
//! The rule: never lose a draft; a held message is fine. The stash is
//! one slot (stashing again overwrites: checked live), and rlord never
//! presses Ctrl+S to restore (a press after the auto-restore would stash
//! the draft again). So:
//!
//! - **Stash** (`plan`, from `send::type_into`) only when the input box
//!   is recognised (`claude::input_box`, shared with `dialog_shown`), the
//!   screen is at its prompt, no "draft stashed" shows, no record of
//!   rlord's is live for the session, the whole box has sat unchanged for
//!   `DRAFT_IDLE` with no one typing at it, rlord isn't backing off there,
//!   and Ctrl+S alone is the stash key (`keybindings.json`, read only,
//!   last). After the press the screen is polled (`poll_stash`): a
//!   recognised, empty box at the prompt is the stash; it is recorded,
//!   then the message typed. A dialog or unrecognised screen: recorded
//!   (it may have happened), nothing typed. The text still there: back
//!   off.
//! - **While a record is live** every message to that session holds
//!   (`Kind::Stash`), empty box or not, so no paste races the restore.
//! - **The record clears** (`tick`) when the draft's first line shows back
//!   in the box, when the user sends a prompt of their own after rlord's
//!   message (they dealt with the box), or on `rlord stash-dismiss`. A
//!   minute after the turn ended without any of those, the row shows `⎘`
//!   ("draft may still be stashed"); nothing is pressed.
//! - A session stopped, errored, archived or gone with a record:
//!   `draft_lost_with_session` (with the first line), and it is forgotten.
//!
//! Events: `draft_stashed`, `draft_restored` (`by`: seen, user_prompt),
//! `draft_restore_dismissed`, `draft_lost_with_session`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use rlord_core::agents::claude::input_box;
use rlord_core::agents::{strip_ansi, Agent};
use rlord_core::tmux;
use rlord_core::types::{now_ms, Session, Status};

use crate::Daemon;

/// kv key: drafts rlord stashed, until it sees them dealt with,
/// `{session id: {"first_line", "at", "sent_at"?, "stale"?}}`.
pub const STASH_KEY: &str = "draft_stash";

/// How long a draft must sit unchanged before rlord stashes it.
pub const DRAFT_IDLE: Duration = Duration::from_secs(30);

/// After the turn rlord's message started has ended, how long before the
/// row shows the draft may still be stashed.
const STALE_AFTER_MS: i64 = 60_000;

/// Time for Claude to redraw after a key.
pub const REDRAW: Duration = Duration::from_millis(400);

/// A hash of each session's input box as first seen, and when: how long
/// it has sat unchanged.
static DRAFTS: Mutex<Option<HashMap<String, (u64, Instant)>>> = Mutex::new(None);

/// How long session `id`'s input box has held `whole` (every line of it)
/// unchanged; zero the first time it is seen.
pub(crate) fn draft_age(id: &str, whole: &str) -> Duration {
    let mut h = DefaultHasher::new();
    whole.hash(&mut h);
    let h = h.finish();
    let mut g = DRAFTS.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    match m.get(id) {
        Some((seen, at)) if *seen == h => at.elapsed(),
        _ => {
            m.insert(id.to_string(), (h, Instant::now()));
            Duration::ZERO
        }
    }
}

/// Session `id`'s box was seen empty, or someone is at it: its draft's
/// age starts again.
pub(crate) fn forget_draft(id: &str) {
    let mut g = DRAFTS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = g.as_mut() {
        m.remove(id);
    }
}

/// Keep only the drafts of sessions `idle` says are at "your turn".
pub(crate) fn prune_drafts(idle: impl Fn(&str) -> bool) {
    let ids: Vec<String> = {
        let g = DRAFTS.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    };
    for id in ids.into_iter().filter(|id| !idle(id)) {
        forget_draft(&id);
    }
}

/// After a Ctrl+S that didn't take, how long before rlord tries the
/// stash in that session again.
pub const UNTOUCHED_BACKOFF: Duration = Duration::from_secs(600);

/// Sessions where the stash's Ctrl+S didn't take, and when.
static UNTOUCHED: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// The stash's Ctrl+S didn't take in session `id`: not again for a while.
pub(crate) fn back_off(id: &str) {
    let mut g = UNTOUCHED.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(HashMap::new)
        .insert(id.to_string(), Instant::now());
}

/// Whether session `id` is in its back-off after a Ctrl+S that didn't take.
pub(crate) fn backing_off(id: &str) -> bool {
    let g = UNTOUCHED.lock().unwrap_or_else(|e| e.into_inner());
    g.as_ref()
        .and_then(|m| m.get(id))
        .is_some_and(|t| t.elapsed() < UNTOUCHED_BACKOFF)
}

/// Claude Code's keybindings file (read only).
fn keybindings_path() -> PathBuf {
    if cfg!(test) {
        // Tests never read the user's own.
        return PathBuf::from("/nonexistent/rlord-test/keybindings.json");
    }
    rlord_core::agents::claude::config_dir().join("keybindings.json")
}

/// Whether Ctrl+S is Claude Code's stash key, and its only one.
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

/// Whether Ctrl+S stashes, and only it, given the keybindings file's
/// text (None: no file, the defaults). No when the JSON doesn't parse,
/// when `ctrl+s` (or a chord starting with it) is bound in `Chat` or
/// `Global` to anything but `chat:stash` (null unbinds it), or when
/// `chat:stash` is bound to any other key (the user moved it).
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
    let all = || {
        blocks
            .iter()
            .filter_map(|b| Some((b["context"].as_str()?, b["bindings"].as_object()?)))
            .flat_map(|(c, m)| m.iter().map(move |(k, a)| (c, k, a)))
    };
    let ctrl_s_kept = all()
        .filter(|(c, k, _)| matches!(*c, "Chat" | "Global") && is_ctrl_s(k))
        .all(|(_, _, a)| a.as_str() == Some("chat:stash"));
    // A chord starting with Ctrl+S makes Claude wait for its next key.
    let chord = all().any(|(c, k, a)| {
        matches!(c, "Chat" | "Global") && starts_with_ctrl_s(k) && !is_ctrl_s(k) && !a.is_null()
    });
    let elsewhere = all().any(|(_, k, a)| a.as_str() == Some("chat:stash") && !is_ctrl_s(k));
    ctrl_s_kept && !chord && !elsewhere
}

/// `ctrl+s` as the keybindings file may spell it (any case, `control`).
fn is_ctrl_s(key: &str) -> bool {
    let k = key.trim().to_ascii_lowercase();
    matches!(k.as_str(), "ctrl+s" | "control+s")
}

/// A key or chord whose first keystroke is Ctrl+S.
fn starts_with_ctrl_s(key: &str) -> bool {
    key.split_whitespace().next().is_some_and(is_ctrl_s)
}

/// Every line inside Claude's input box (plain), for a draft's age.
pub(crate) fn input_region(screen: &str) -> Option<String> {
    let b = input_box(screen)?;
    let lines: Vec<&str> = screen.lines().collect();
    Some(b.lines(&lines).join("\n"))
}

/// Whether Claude's screen (plain) shows a stashed draft, read only on
/// its status lines: below the input box's bottom rule, and the line
/// right above its top rule when that line starts with the notice (seen
/// live: "draft stashed · ✔ Update installed"). Of "draft stashed" and
/// "draft restored", the last one wins. None: the box isn't recognised.
pub(crate) fn stash_shown(screen: &str) -> Option<bool> {
    let b = input_box(screen)?;
    let lines: Vec<&str> = screen.lines().collect();
    let mut status: Vec<String> = Vec::new();
    if let Some(above) = b.top.checked_sub(1).map(|i| lines[i].trim().to_lowercase()) {
        if above.starts_with("draft stashed") || above.starts_with("draft restored") {
            status.push(above);
        }
    }
    status.extend(lines[b.bottom + 1..].iter().map(|l| l.to_lowercase()));
    let tail = status.join("\n");
    Some(
        match (tail.rfind("draft stashed"), tail.rfind("draft restored")) {
            (Some(s), Some(r)) => s > r,
            (Some(_), None) => true,
            _ => false,
        },
    )
}

/// What to do with a Claude session's draft when a message is due.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Stash,
    /// Not now: why (said to the overlord, and on the row).
    Hold(String),
}

/// Whether to stash a draft (`age`: how long the box has sat unchanged)
/// on this screen (plain). `backoff`: a Ctrl+S that didn't take was
/// pressed there lately. `key_ok`: Ctrl+S is the stash key and only it
/// (read last, only when everything else says yes). (A live record of
/// rlord's holds every message before this is asked: `held_by_record`.)
pub(crate) fn plan(
    screen: &str,
    backoff: bool,
    age: Duration,
    key_ok: impl FnOnce() -> bool,
) -> Plan {
    if backoff {
        return Plan::Hold(format!(
            "its input holds unsent text (a draft), and Claude Code's prompt stash (Ctrl+S) \
             didn't take it lately; rlord tries again after {} minutes",
            UNTOUCHED_BACKOFF.as_secs() / 60
        ));
    }
    if input_box(screen).is_none() || !Agent::Claude.at_prompt(screen) {
        return Plan::Hold(
            "its input holds unsent text (a draft), and rlord can't read Claude Code's input box \
             there, so it doesn't stash"
                .into(),
        );
    }
    if stash_shown(screen) != Some(false) {
        return Plan::Hold(
            "its input holds unsent text (a draft), and Claude Code's prompt stash already holds \
             another one (stashing again would lose it)"
                .into(),
        );
    }
    if age < DRAFT_IDLE {
        return Plan::Hold(format!(
            "its input holds unsent text (a draft) the user may still be writing; rlord stashes \
             a draft only once it has sat unchanged for {}s (with when_idle the message waits \
             for that)",
            DRAFT_IDLE.as_secs()
        ));
    }
    if !key_ok() {
        return Plan::Hold(
            "its input holds unsent text (a draft), and Claude Code's prompt stash isn't left on \
             Ctrl+S alone there (keybindings.json), so rlord doesn't set the draft aside"
                .into(),
        );
    }
    Plan::Stash
}

/// What the screen after the stash's Ctrl+S proved.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum AfterStash {
    /// A recognised, empty box at the prompt: stashed. Record it, then
    /// type the message.
    Stashed,
    /// The text is still in the box: nothing was stashed. Back off.
    Untouched,
    /// A dialog, an unrecognised or unreadable screen: it may have been
    /// stashed. Record it; type nothing.
    Unclear,
}

/// One look after the press: Some(definite) or None (keep looking).
fn look_after_stash(screen: Option<&str>) -> (Option<AfterStash>, AfterStash) {
    let Some(ansi) = screen else {
        return (None, AfterStash::Unclear);
    };
    let plain = strip_ansi(ansi);
    if input_box(&plain).is_none() || !Agent::Claude.at_prompt(&plain) {
        return (None, AfterStash::Unclear);
    }
    match crate::remote::input_text(ansi) {
        None => (Some(AfterStash::Stashed), AfterStash::Stashed),
        Some(_) => (None, AfterStash::Untouched),
    }
}

/// Captures after the press, `REDRAW` apart.
pub(crate) const POLLS: usize = 5;

/// What `type_into` does after the stash's Ctrl+S.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AfterPress {
    /// Record the stash (before anything is typed).
    pub record: bool,
    /// Mark it stale at once (`⎘`): nothing will be typed, so no turn
    /// will end to restore it.
    pub stale: bool,
    /// Type the message.
    pub paste: bool,
    /// Back off stashing in that session.
    pub back_off: bool,
}

/// `pressed`: the Ctrl+S key went to tmux. `step`: what the polling saw
/// (only when pressed).
pub(crate) fn after_press(pressed: bool, step: AfterStash) -> AfterPress {
    let no = AfterPress {
        record: false,
        stale: false,
        paste: false,
        back_off: false,
    };
    match (pressed, step) {
        // The key never went: nothing was stashed.
        (false, _) => no,
        (true, AfterStash::Untouched) => AfterPress {
            back_off: true,
            ..no
        },
        (true, AfterStash::Unclear) => AfterPress {
            record: true,
            stale: true,
            ..no
        },
        (true, AfterStash::Stashed) => AfterPress {
            record: true,
            paste: true,
            ..no
        },
    }
}

/// Mark session `id`'s record stale now (`⎘`): the message after the
/// stash didn't go in, so Claude has no turn's end to restore it at.
pub(crate) fn mark_stale(d: &Daemon, id: &str) -> Result<()> {
    let db = d.db();
    let mut m = db.kv_map(STASH_KEY)?;
    if let Some(e) = m.get_mut(id).and_then(Value::as_object_mut) {
        e.insert("stale".into(), json!(true));
        db.kv_set_map(STASH_KEY, &m)?;
    }
    drop(db);
    d.changed();
    Ok(())
}

/// Read the screen after the stash's Ctrl+S, up to `POLLS` captures
/// (`capture`: the next screen with attributes, None when unreadable),
/// until it is definite. Otherwise the last look decides.
pub(crate) fn poll_stash(mut capture: impl FnMut() -> Option<String>) -> AfterStash {
    let mut last = AfterStash::Unclear;
    for _ in 0..POLLS {
        let (done, now) = look_after_stash(capture().as_deref());
        if let Some(d) = done {
            return d;
        }
        last = now;
    }
    last
}

/// Whitespace collapsed, trimmed.
fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether the stashed draft (its first visual row, `first_line`) shows
/// back in the box: the first row equal, or the whole box's text (its
/// rows joined, as the line may now wrap differently) starting with it.
pub(crate) fn draft_back(screen_ansi: &str, first_line: &str) -> bool {
    let want = norm(first_line);
    if want.is_empty() {
        return false;
    }
    if crate::remote::input_text(screen_ansi).is_some_and(|r| norm(&r) == want) {
        return true;
    }
    let plain = strip_ansi(screen_ansi);
    let Some(region) = input_region(&plain) else {
        return false;
    };
    let text = norm(region.trim_start().trim_start_matches('❯'));
    // Only a whole row's worth: it must end at a word boundary.
    text == want || text.starts_with(&format!("{want} "))
}

/// What a live record's session says now.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Check {
    Keep,
    /// The user dealt with the box: cleared.
    Cleared(&'static str),
    /// Show `⎘`: a minute after the turn ended and nothing cleared it.
    Stale,
    /// The session can't restore it any more: logged, forgotten.
    Lost,
}

/// A record (`entry`) against its session (`s`, None: gone) and whether
/// the draft is seen back (`back`), at `now`.
pub(crate) fn check(entry: &Value, s: Option<&Session>, back: bool, now: i64) -> Check {
    let Some(s) = s.filter(|s| {
        let restarted = s.status == Status::Starting
            && s.last_state_change_at
                .is_some_and(|t| t >= entry["at"].as_i64().unwrap_or(0));
        s.archived_at.is_none()
            && !restarted
            && !matches!(s.status, Status::Stopped | Status::Error)
    }) else {
        return Check::Lost;
    };
    if back {
        return Check::Cleared("seen");
    }
    let sent = entry["sent_at"]
        .as_i64()
        .or(entry["at"].as_i64())
        .unwrap_or(0);
    if s.last_user_msg_at
        .is_some_and(|t| t > sent + crate::limits::OWN_PROMPT_MS)
    {
        return Check::Cleared("user_prompt");
    }
    let ended = s.status == Status::Idle
        && s.last_state_change_at
            .is_some_and(|t| t > sent && now - t >= STALE_AFTER_MS);
    if ended && entry["stale"] != true {
        return Check::Stale;
    }
    Check::Keep
}

/// The stash's Ctrl+S emptied (or may have emptied) session `id`'s box:
/// record its first row before anything is typed.
pub(crate) fn stashed(d: &Daemon, id: &str, first_line: &str) -> Result<()> {
    let db = d.db();
    let now = now_ms();
    let mut m = db.kv_map(STASH_KEY)?;
    m.insert(id.to_string(), json!({"first_line": first_line, "at": now}));
    db.kv_set_map(STASH_KEY, &m)?;
    db.insert_event(
        id,
        now,
        "draft_stashed",
        &json!({"chars": first_line.chars().count()}).to_string(),
    )?;
    forget_draft(id);
    Ok(())
}

/// rlord's message went in after the stash: the time the user's own
/// next prompt is compared with.
pub(crate) fn sent(d: &Daemon, id: &str) -> Result<()> {
    let db = d.db();
    let mut m = db.kv_map(STASH_KEY)?;
    if let Some(e) = m.get_mut(id).and_then(Value::as_object_mut) {
        e.insert("sent_at".into(), json!(now_ms()));
        db.kv_set_map(STASH_KEY, &m)?;
    }
    Ok(())
}

/// Why a message to session `id` holds: a record of rlord's stash is
/// live there (whatever the box shows: a paste must not race Claude's
/// own restore). (A DB error holds too.)
pub(crate) fn held_by_record(d: &Daemon, id: &str) -> Option<String> {
    let live = d
        .db()
        .kv_map(STASH_KEY)
        .map_or(true, |m| m.contains_key(id));
    live.then(|| {
        "a draft of the user's that rlord stashed there isn't seen back yet (Claude Code \
         restores it after the turn; once it shows in the box, the user sends a prompt, or \
         `rlord stash-dismiss <session>`, messages go again)"
            .to_string()
    })
}

/// `session.stash_dismiss {ref}`: the user says the stashed draft is
/// dealt with: its record goes, and messages to the session go again.
pub fn dismiss(d: &Daemon, params: &Value) -> Result<Value> {
    let r = params
        .get("ref")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing 'ref'"))?;
    let s = d.db().resolve_ref(r)?;
    let _session = d.sessions.lock(&s.id);
    let had = d.db().kv_map(STASH_KEY)?.contains_key(&s.id);
    if had {
        d.db().insert_event(
            &s.id,
            now_ms(),
            "draft_restore_dismissed",
            &json!({}).to_string(),
        )?;
        forget(d, &s.id)?;
    }
    Ok(json!({"session": s.name, "dismissed": had}))
}

/// Annotate list rows with `draft_stash`: "pending" (a record is live:
/// messages hold) or "stale" (`⎘`: it may still be stashed), else null.
pub fn annotate(d: &Daemon, rows: &mut [Value]) {
    let m = d.db().kv_map(STASH_KEY).unwrap_or_default();
    for row in rows {
        let id = row["id"].as_str().unwrap_or("");
        row["draft_stash"] = match m.get(id) {
            Some(e) if e["stale"] == true => json!("stale"),
            Some(_) => json!("pending"),
            None => Value::Null,
        };
    }
}

/// Look at each live record: clear, mark or forget it.
pub(crate) fn tick(d: &Daemon) -> Result<()> {
    let m = d.db().kv_map(STASH_KEY)?;
    for (id, entry) in m {
        if let Err(e) = look(d, &id, &entry) {
            tracing::debug!(session = %id, error = %e, "checking a stashed draft");
        }
    }
    Ok(())
}

fn look(d: &Daemon, id: &str, entry: &Value) -> Result<()> {
    let s = d.db().resolve_ref(id).ok();
    let first_line = entry["first_line"].as_str().unwrap_or_default();
    let back = s
        .as_ref()
        .and_then(|s| crate::send::agent_pane(s).ok())
        .and_then(|p| tmux::capture_pane_id_ansi(&p).ok())
        .is_some_and(|screen| draft_back(&screen, first_line));
    let now = now_ms();
    match check(entry, s.as_ref(), back, now) {
        Check::Keep => Ok(()),
        Check::Cleared(by) => {
            d.db()
                .insert_event(id, now, "draft_restored", &json!({"by": by}).to_string())?;
            forget(d, id)
        }
        Check::Stale => {
            let db = d.db();
            let mut m = db.kv_map(STASH_KEY)?;
            if let Some(e) = m.get_mut(id).and_then(Value::as_object_mut) {
                e.insert("stale".into(), json!(true));
                db.kv_set_map(STASH_KEY, &m)?;
            }
            drop(db);
            d.changed();
            Ok(())
        }
        Check::Lost => {
            if s.is_some() {
                d.db().insert_event(
                    id,
                    now,
                    "draft_lost_with_session",
                    &json!({"first_line": first_line}).to_string(),
                )?;
            }
            tracing::warn!(session = %id, "a stashed draft's session went away");
            forget(d, id)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude's idle screen with `input` in the box (lines split on
    /// `\n`), `above` on the line over the top rule and `status` on the
    /// line under the bottom one.
    fn screen_with(above: &str, input: &str, status: &str) -> String {
        let rule = "─".repeat(60);
        let mut v = vec![
            "● Draft stashed is a phrase in the transcript, far above.".to_string(),
            String::new(),
            above.to_string(),
            rule.clone(),
        ];
        for (i, l) in input.split('\n').enumerate() {
            v.push(if i == 0 {
                format!("❯ {l}")
            } else {
                format!("  {l}")
            });
        }
        v.extend([
            rule,
            format!("  {status}"),
            "  -- INSERT -- ⏵⏵ auto mode on".into(),
        ]);
        v.join("\n")
    }

    fn screen(input: &str, status: &str) -> String {
        screen_with("● Done.", input, status)
    }

    /// A permission dialog in place of the box.
    const DIALOG: &str = "● Bash(ls)\n Do you want to proceed?\n ❯ 1. Yes\n   2. No";

    #[test]
    fn the_stash_indicator_is_read_on_the_status_lines_only() {
        assert_eq!(
            stash_shown(&screen("", "Opus 5.5 | Context 6%")),
            Some(false)
        );
        assert_eq!(stash_shown(&screen("", "draft stashed")), Some(true));
        assert_eq!(
            stash_shown(&screen("", "draft restored · draft stashed")),
            Some(true)
        );
        assert_eq!(
            stash_shown(&screen("half", "draft stashed · draft restored")),
            Some(false)
        );
        assert_eq!(
            stash_shown(&screen_with(
                "draft stashed · ✔ Update installed",
                "",
                "Opus 5.5"
            )),
            Some(true)
        );
        assert_eq!(
            stash_shown(&screen_with(
                "● The status line then reads draft stashed",
                "",
                "Opus 5.5"
            )),
            Some(false)
        );
        assert_eq!(stash_shown("no prompt at all\ndraft stashed"), None);
        assert_eq!(stash_shown("❯ text without rules\ndraft stashed"), None);
    }

    /// The shared reader agrees with `dialog_shown`: a dialog has no box.
    #[test]
    fn one_input_box_reader() {
        assert!(input_box(&screen("half", "Opus")).is_some());
        assert!(input_box(DIALOG).is_none());
        // A dialog between rules is still no box (no rule right above its `❯`).
        let ruled = format!(
            "{0}\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n{0}",
            "─".repeat(40)
        );
        assert!(rlord_core::agents::claude::dialog_shown(&ruled));
        assert!(input_box(&ruled).is_none());
        assert_eq!(input_region(DIALOG), None);
        let two = screen("first line\nsecond line", "Opus");
        assert_eq!(
            input_region(&two).as_deref(),
            Some("❯ first line\n  second line")
        );
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
        assert!(!stash_bound(Some(&chat("ctrl+k", "\"chat:stash\""))));
        assert!(!stash_bound(Some(&chat(
            "ctrl+s ctrl+x",
            "\"chat:submit\""
        ))));
        assert!(stash_bound(Some(&chat("ctrl+s ctrl+x", "null"))));
        assert!(stash_bound(Some(&chat(
            "ctrl+x ctrl+s",
            "\"chat:sendNow\""
        ))));
        let global =
            r#"{"bindings": [{"context": "Global", "bindings": {"ctrl+s": "app:redraw"}}]}"#;
        assert!(!stash_bound(Some(global)));
        let other = r#"{"bindings": [{"context": "Agents", "bindings": {"ctrl+s": null}}]}"#;
        assert!(stash_bound(Some(other)));
        assert!(!stash_bound(Some("{not json")));
        assert!(!stash_key_ok_at(Path::new("/")));
        assert!(stash_key_ok_at(Path::new(
            "/nonexistent/rlord/keybindings.json"
        )));
    }

    /// Rule A.
    #[test]
    fn a_draft_is_stashed_only_when_safe() {
        let idle = screen("half a thought", "Opus 5.5");
        let old = DRAFT_IDLE + Duration::from_secs(1);
        let yes = || true;
        assert_eq!(plan(&idle, false, old, yes), Plan::Stash);
        assert!(matches!(plan(&idle, false, old, || false), Plan::Hold(_)));
        assert!(matches!(plan(&idle, true, old, yes), Plan::Hold(_)));
        assert!(matches!(
            plan(&screen("half", "draft stashed"), false, old, yes),
            Plan::Hold(_)
        ));
        assert!(matches!(
            plan("❯ half, no box", false, old, yes),
            Plan::Hold(_)
        ));
        let Plan::Hold(why) = plan(DIALOG, false, old, yes) else {
            panic!("a dialog must hold");
        };
        assert!(why.contains("can't read"), "{why}");
        // The keybindings file is read only once everything else says yes.
        let read = std::cell::Cell::new(false);
        let _ = plan(&idle, false, Duration::from_secs(3), || {
            read.set(true);
            true
        });
        assert!(!read.get(), "read before the age check");
    }

    /// Rule G: a young draft's refusal points at when_idle, claims no
    /// delivery.
    #[test]
    fn a_young_draft_says_when_idle() {
        let idle = screen("half a thought", "Opus 5.5");
        let Plan::Hold(why) = plan(&idle, false, Duration::from_secs(3), || true) else {
            panic!("a fresh draft must hold");
        };
        assert!(why.contains("when_idle"), "{why}");
        assert!(!why.contains("delivers"), "{why}");
    }

    /// Rule B.
    #[test]
    fn after_the_stash_press_the_screen_decides() {
        let polls = |v: Vec<Option<String>>| {
            let mut it = v.into_iter();
            poll_stash(|| it.next().flatten())
        };
        let draft = || Some(screen("half a thought", "Opus"));
        assert_eq!(
            polls(vec![draft(), Some(screen("", "Opus"))]),
            AfterStash::Stashed
        );
        assert_eq!(polls(vec![draft(); POLLS]), AfterStash::Untouched);
        // A dialog came up: may have happened, not typed.
        assert_eq!(
            polls(vec![Some(DIALOG.to_string()); POLLS]),
            AfterStash::Unclear
        );
        // Unrecognised (no box) though "empty": not counted as stashed.
        assert_eq!(
            polls(vec![Some("❯ \nno rules".to_string()); POLLS]),
            AfterStash::Unclear
        );
        assert_eq!(polls(vec![draft(), None]), AfterStash::Unclear);
        assert_eq!(polls(vec![]), AfterStash::Unclear);
    }

    #[test]
    fn untouched_backs_off() {
        assert!(!backing_off("t-back"));
        back_off("t-back");
        assert!(backing_off("t-back"));
    }

    #[test]
    fn a_draft_ages_only_while_unchanged() {
        assert_eq!(draft_age("t-age", "one"), Duration::ZERO);
        std::thread::sleep(Duration::from_millis(5));
        assert!(draft_age("t-age", "one") > Duration::ZERO);
        assert_eq!(draft_age("t-age", "one\ntwo"), Duration::ZERO);
        forget_draft("t-age");
        assert_eq!(draft_age("t-age", "one\ntwo"), Duration::ZERO);
        std::thread::sleep(Duration::from_millis(5));
        prune_drafts(|_| true);
        assert!(draft_age("t-age", "one\ntwo") > Duration::ZERO);
        prune_drafts(|id| id != "t-age");
        assert_eq!(draft_age("t-age", "one\ntwo"), Duration::ZERO);
    }

    /// Rule E, first clause: the first row back, or the text rewrapped.
    #[test]
    fn the_draft_is_seen_back() {
        let line = "fix the  login bug";
        assert!(draft_back(
            &screen("fix the login bug", "draft restored"),
            line
        ));
        // Rewrapped: the first row is now shorter, the text the same.
        assert!(draft_back(
            &screen("fix the\nlogin bug and more", "x"),
            line
        ));
        assert!(!draft_back(&screen("fix the login", "x"), line));
        assert!(!draft_back(&screen("fix the login bugs", "x"), line));
        assert!(!draft_back(&screen("", "draft stashed"), line));
        assert!(!draft_back(DIALOG, line));
    }

    fn session(status: Status, changed: i64, user: Option<i64>) -> Session {
        let db = rlord_core::db::Db::open_in_memory().unwrap();
        db.insert_session(&rlord_core::db::NewSession {
            id: "s1",
            name: "x",
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
        let mut s = db.resolve_ref("s1").unwrap();
        s.status = status;
        s.last_state_change_at = Some(changed);
        s.last_user_msg_at = user;
        s
    }

    /// Rules E and F.
    #[test]
    fn a_record_clears_only_on_what_proves_it() {
        let e = json!({"first_line": "x", "at": 1_000, "sent_at": 2_000});
        let idle = session(Status::Idle, 3_000, None);
        assert_eq!(check(&e, Some(&idle), true, 4_000), Check::Cleared("seen"));
        let typed = session(
            Status::Working,
            3_000,
            Some(2_000 + crate::limits::OWN_PROMPT_MS + 1),
        );
        assert_eq!(
            check(&e, Some(&typed), false, 4_000),
            Check::Cleared("user_prompt")
        );
        // A prompt before rlord's message doesn't count.
        let before = session(Status::Idle, 3_000, Some(1_500));
        assert_eq!(check(&e, Some(&before), false, 4_000), Check::Keep);
        // rlord's own message read back as a prompt (within OWN_PROMPT_MS): not the user's.
        let echo = session(Status::Working, 3_000, Some(2_500));
        assert_eq!(check(&e, Some(&echo), false, 4_000), Check::Keep);
        // A minute after the turn ended: the mark, nothing else.
        assert_eq!(
            check(&e, Some(&idle), false, 3_000 + STALE_AFTER_MS),
            Check::Stale
        );
        let stale = json!({"first_line": "x", "at": 1_000, "sent_at": 2_000, "stale": true});
        assert_eq!(
            check(&stale, Some(&idle), false, 3_000 + STALE_AFTER_MS),
            Check::Keep
        );
        let working = session(Status::Working, 3_000, None);
        assert_eq!(check(&e, Some(&working), false, 999_999), Check::Keep);
        // Rule F.
        assert_eq!(check(&e, None, false, 4_000), Check::Lost);
        let stopped = session(Status::Stopped, 3_000, None);
        assert_eq!(check(&e, Some(&stopped), false, 4_000), Check::Lost);
        let errored = session(Status::Error, 3_000, None);
        assert_eq!(check(&e, Some(&errored), true, 4_000), Check::Lost);
        // Restarted after the stash: the process (and its stash) is gone.
        let restarted = session(Status::Starting, 3_000, None);
        assert_eq!(check(&e, Some(&restarted), false, 4_000), Check::Lost);
        // Starting from before the stash: not a restart since.
        let early = session(Status::Starting, 500, None);
        assert_eq!(check(&e, Some(&early), false, 4_000), Check::Keep);
    }

    /// Rules 1, 2 and 8: what follows the stash's Ctrl+S.
    #[test]
    fn after_the_press() {
        let p = |pressed, step| after_press(pressed, step);
        // The key never went: no record, nothing typed.
        let none = p(false, AfterStash::Stashed);
        assert!(!none.record && !none.paste && !none.stale);
        let ok = p(true, AfterStash::Stashed);
        assert!(ok.record && ok.paste && !ok.stale);
        // Unclear: recorded and `⎘` at once, nothing typed.
        let unclear = p(true, AfterStash::Unclear);
        assert!(unclear.record && unclear.stale && !unclear.paste);
        let untouched = p(true, AfterStash::Untouched);
        assert!(untouched.back_off && !untouched.record && !untouched.paste);
    }
}
