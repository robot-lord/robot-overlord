//! Delivering to a Claude session past a draft, with Claude Code's own
//! prompt stash (`chat:stash`, Ctrl+S): with text in the input, Ctrl+S
//! stashes it and clears the box ("draft stashed" shows on its status
//! line); pressed again on an empty box, it puts the draft back as it was.
//!
//! The user's decision: rather than hold a message for as long as a
//! Claude session's input holds unsent text, rlord stashes the draft,
//! types the message, and restores the draft once the turn is over.
//! Codex and OpenCode have no such stash: a draft still holds them.
//!
//! What was checked live (Claude Code 2.1.327 and 2.1.331): the stash is
//! ONE slot (stashing again overwrites, and the first draft is lost);
//! Claude Code restores the stashed draft by itself about 2s after the
//! delivered turn ("draft restored"), so a Ctrl+S after that stashes it
//! again (that is what "draft restored · draft stashed" with an empty
//! box was). The guiding rule: never lose a draft; a held message is
//! fine. So:
//!
//! - **Stash** (`plan`, from `send::type_into`) only when nothing is
//!   stashed (no "draft stashed" on the status lines, no record of
//!   rlord's, failed ones included), no back-off after a Ctrl+S that
//!   didn't take, the whole box has sat unchanged for `DRAFT_IDLE` with
//!   no one typing at it, and Ctrl+S alone is the stash key
//!   (`keybindings.json`, read only, last: `stash_bound`). After the
//!   press the screen is polled (`poll_stash`): the box going empty
//!   proves the stash; it is recorded, then the message typed.
//! - **Restore** (`tick`, from `send`'s slow tick): mostly Claude's own
//!   auto-restore is seen. Ctrl+S only with the box empty and the latest
//!   status "draft stashed", the turn over a while, and a fresh look
//!   right before (`restore_step`). Never with text in the box. Nothing
//!   stashed and our draft not in the box: dealt with, dropped quietly
//!   (`draft_restore_unknown`). Still stashed after presses: it gives up
//!   (`draft_restore_failed`, row mark `⎘`), and that record blocks new
//!   stashes until the draft shows back or `rlord stash-dismiss`.
//!
//! Events: `draft_stashed`, `draft_restored`, `draft_restore_failed`,
//! `draft_restore_unknown`, `draft_restore_dismissed`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
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
/// when `ctrl+s` is bound in `Chat` or `Global` to anything but
/// `chat:stash` (null unbinds it), or when `chat:stash` is bound to any
/// other key (the user moved it: leave it alone).
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

/// A `─` rule line (the input box's top and bottom).
fn is_rule(line: &str) -> bool {
    line.trim_start().starts_with("───")
}

/// Claude's input box on a plain screen: (top rule, bottom rule) line
/// indexes around the last `❯` line. None: not recognised.
fn input_box(lines: &[&str]) -> Option<(usize, usize)> {
    let input = lines
        .iter()
        .rposition(|l| l.trim_start().starts_with('❯'))?;
    let top = lines[..input].iter().rposition(|l| is_rule(l))?;
    let bottom = input + lines[input..].iter().position(|l| is_rule(l))?;
    Some((top, bottom))
}

/// Every line inside Claude's input box (plain), for a draft's age.
pub(crate) fn input_region(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let (top, bottom) = input_box(&lines)?;
    Some(lines[top + 1..bottom].join("\n"))
}

/// Whether Claude's screen (plain) shows a stashed draft, read only on
/// its status lines: below the input box's bottom rule, and the line
/// right above its top rule when that line starts with the notice (seen
/// live: "draft stashed · ✔ Update installed"). Of "draft stashed" and
/// "draft restored", the last one wins ("draft restored · draft
/// stashed", seen live after a restore that didn't take, is still
/// stashed). None: the box isn't recognised.
pub(crate) fn stash_shown(screen: &str) -> Option<bool> {
    let lines: Vec<&str> = screen.lines().collect();
    let (top, bottom) = input_box(&lines)?;
    let mut status: Vec<String> = Vec::new();
    if let Some(above) = top.checked_sub(1).map(|i| lines[i].trim().to_lowercase()) {
        if above.starts_with("draft stashed") || above.starts_with("draft restored") {
            status.push(above);
        }
    }
    status.extend(lines[bottom + 1..].iter().map(|l| l.to_lowercase()));
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

/// What rlord knows of its own earlier stash in a session.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Earlier {
    None,
    /// Stashed, restore not done yet.
    Pending,
    /// Its restore gave up (`⎘`): the draft may still be in the stash.
    Failed,
}

/// Whether to stash a draft (`age`: how long the box has sat unchanged)
/// on this screen (plain). `earlier`: rlord's own earlier stash there.
/// `backoff`: a Ctrl+S that didn't take was pressed there lately.
/// `key_ok`: Ctrl+S is the stash key and only it (read last, only when
/// everything else says yes).
pub(crate) fn plan(
    screen: &str,
    earlier: Earlier,
    backoff: bool,
    age: Duration,
    key_ok: impl FnOnce() -> bool,
) -> Plan {
    if earlier == Earlier::Failed {
        return Plan::Hold(
            "its input holds unsent text (a draft), and a draft rlord stashed earlier couldn't \
             be put back (⎘ on its row): the user restores it with Ctrl+S in the session, or \
             dismisses it with `rlord stash-dismiss <session>`"
                .into(),
        );
    }
    if backoff {
        return Plan::Hold(format!(
            "its input holds unsent text (a draft), and Claude Code's prompt stash (Ctrl+S) \
             didn't take it lately; rlord tries again after {} minutes",
            UNTOUCHED_BACKOFF.as_secs() / 60
        ));
    }
    match stash_shown(screen) {
        None => {
            return Plan::Hold(
                "its input holds unsent text (a draft), and rlord can't read where Claude Code \
                 shows its prompt stash, so it doesn't stash"
                    .into(),
            )
        }
        Some(true) => {
            return Plan::Hold(
                "its input holds unsent text (a draft), and Claude Code's prompt stash already \
                 holds another one (stashing again would lose it)"
                    .into(),
            )
        }
        Some(false) if earlier == Earlier::Pending => {
            return Plan::Hold(
                "its input holds unsent text (a draft), and a draft rlord stashed earlier isn't \
                 back yet (stashing again could lose it)"
                    .into(),
            )
        }
        Some(false) => {}
    }
    if age < DRAFT_IDLE {
        return Plan::Hold(format!(
            "its input holds unsent text (a draft) the user may still be writing; once it has \
             sat unchanged for {}s rlord stashes it (Ctrl+S), delivers, and restores it",
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

/// After an Ctrl+S that didn't take, how long before rlord tries the
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

/// What the screen after the stash's Ctrl+S proved.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum AfterStash {
    /// The box went from text to empty: stashed, whatever the indicator
    /// shows. Record it, then type the message.
    Stashed,
    /// The text is still in the box: nothing was stashed. Back off.
    Untouched,
    /// No capture could be read: record it (the restore only presses on
    /// an empty box, and clears the record when it sees the draft), and
    /// don't type.
    Unclear,
}

/// Captures after the press, `REDRAW` apart.
pub(crate) const POLLS: usize = 5;

/// Read the screen after the stash's Ctrl+S, up to `POLLS` captures
/// (`capture`: the next screen with attributes, None when unreadable),
/// until it is definite: an empty box at any capture is stashed; text in
/// the last readable capture is untouched.
pub(crate) fn poll_stash(mut capture: impl FnMut() -> Option<String>) -> AfterStash {
    let mut last = AfterStash::Unclear;
    for _ in 0..POLLS {
        match capture() {
            Some(s) if crate::remote::input_text(&s).is_none() => return AfterStash::Stashed,
            Some(_) => last = AfterStash::Untouched,
            None => {}
        }
    }
    last
}

/// Whether the input (its first line, as `remote::input_text` reads it)
/// is the stashed draft back: the whole first line, trimmed.
fn same_draft(input: &str, first_line: &str) -> bool {
    let a = input.trim();
    !a.is_empty() && a == first_line.trim()
}

/// The next step of a restore.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Restore {
    /// Press Ctrl+S now.
    Press,
    /// Not now (text in the box, a dialog up, the turn not over, the
    /// screen not recognised).
    Wait,
    /// The draft is back (someone restored it).
    Back,
    /// Nothing shows stashed and our draft isn't in the box: Claude Code
    /// restored it by itself (seen live) and the user moved on, or the
    /// user dealt with it. Drop the record quietly; never press.
    Gone,
}

/// What to do for a stashed draft whose first line is `first_line`, on
/// a screen whose input holds `input` (`remote::input_text`), shows the
/// stash or not (None: unrecognised), is at its prompt or not, with the
/// turn over a while (`settled`) or not. Pressed only with the box empty
/// and the latest status "draft stashed": Claude Code may restore the
/// draft by itself after the turn (seen live), and a press then would
/// stash it again.
pub(crate) fn restore_step(
    input: Option<&str>,
    shown: Option<bool>,
    at_prompt: bool,
    settled: bool,
    first_line: &str,
) -> Restore {
    match (input, shown) {
        (Some(t), Some(false)) if same_draft(t, first_line) => Restore::Back,
        _ if !(at_prompt && settled) => Restore::Wait,
        (_, Some(false)) => Restore::Gone,
        (None, Some(true)) => Restore::Press,
        // Text in the box (a press would stash over it), or the screen
        // not recognised.
        _ => Restore::Wait,
    }
}

/// What a restore's press did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Pressed {
    /// The draft is back.
    Back,
    /// Still stashed: try again later.
    Retry,
    /// Nothing stashed any more and not our draft back: the stash was
    /// gone already (or held something else). Drop the record quietly.
    Unknown,
}

/// After a restore's press: the input then (`remote::input_text`) and
/// whether the stash still shows.
pub(crate) fn after_press(input: Option<&str>, shown: Option<bool>, first_line: &str) -> Pressed {
    match input {
        Some(t) if same_draft(t, first_line) => Pressed::Back,
        Some(_) => Pressed::Unknown,
        None if shown != Some(false) => Pressed::Retry,
        None => Pressed::Unknown,
    }
}

/// Backoff before the next press after `tries` that didn't take.
fn backoff_ms(tries: u64) -> i64 {
    (2_000i64 << tries.min(5)).min(60_000)
}

/// A draft (first line `draft`) was most likely stashed in session `id`:
/// remember it for the restore, and log it.
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

/// rlord's own earlier stash in session `id`. A failed one still blocks:
/// rlord can't see what the stash holds, only its record. (A DB error
/// reads as pending: it blocks too.)
pub(crate) fn earlier(d: &Daemon, id: &str) -> Earlier {
    match d.db().kv_map(STASH_KEY) {
        Ok(m) => match m.get(id) {
            None => Earlier::None,
            Some(e) if e["failed"] == true => Earlier::Failed,
            Some(_) => Earlier::Pending,
        },
        Err(_) => Earlier::Pending,
    }
}

/// `session.stash_dismiss {ref}`: the user says the draft rlord couldn't
/// put back is dealt with (`⎘`): its record goes, and stashing in that
/// session may resume.
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
    if gone {
        return forget(d, id);
    }
    let failed = entry["failed"] == true;
    let first_line = entry["first_line"].as_str().unwrap_or_default().to_string();
    let s = s.expect("checked above");
    let pane = crate::send::agent_pane(&s)?;
    let look = || -> Result<(Option<String>, Option<bool>, bool)> {
        let ansi = tmux::capture_pane_id_ansi(&pane)?;
        let plain = strip_ansi(&ansi);
        Ok((
            crate::remote::input_text(&ansi),
            stash_shown(&plain),
            Agent::Claude.at_prompt(&plain),
        ))
    };
    let (input, shown, at_prompt) = look()?;
    if failed {
        // It blocks new stashes until the draft shows back (or the user
        // dismisses it, `dismiss`).
        if input.as_deref().is_some_and(|i| same_draft(i, &first_line)) {
            return restored(d, id, "other");
        }
        return Ok(());
    }
    let t = s.tmux_session.as_deref().unwrap_or_default();
    let settled = s.status == Status::Idle
        && s.last_state_change_at
            .is_none_or(|t| now - t >= RESTORE_IDLE_MS)
        && crate::send::settled_since_typed(id)
        && !crate::send::user_typing(t);
    let late = now - entry["at"].as_i64().unwrap_or(now) > RESTORE_FOR_MS;
    match restore_step(input.as_deref(), shown, at_prompt, settled, &first_line) {
        Restore::Back => restored(d, id, "other"),
        Restore::Gone => unknown(d, id),
        Restore::Wait if late => give_up(d, id, "no moment to restore it came in time"),
        Restore::Wait => Ok(()),
        Restore::Press => {
            if now < entry["next_at"].as_i64().unwrap_or(0) {
                return Ok(());
            }
            let after = {
                let _typing = crate::send::typing();
                // A fresh look right before the key: Claude may have
                // restored it by itself meanwhile.
                let (input, shown, at_prompt) = look()?;
                if restore_step(input.as_deref(), shown, at_prompt, settled, &first_line)
                    != Restore::Press
                {
                    return Ok(());
                }
                let _ = tmux::cancel_mode(&pane);
                tmux::send_keys(&pane, &["C-s"])?;
                std::thread::sleep(REDRAW);
                tmux::capture_pane_id_ansi(&pane)?
            };
            let input = crate::remote::input_text(&after);
            let shown = stash_shown(&strip_ansi(&after));
            match after_press(input.as_deref(), shown, &first_line) {
                Pressed::Back => restored(d, id, "rlord"),
                Pressed::Unknown => unknown(d, id),
                Pressed::Retry => {
                    let tries = entry["tries"].as_u64().unwrap_or(0) + 1;
                    if tries >= RESTORE_TRIES || late {
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

/// The press brought nothing back and nothing shows stashed: dropped
/// quietly (no mark).
fn unknown(d: &Daemon, id: &str) -> Result<()> {
    d.db().insert_event(
        id,
        now_ms(),
        "draft_restore_unknown",
        &json!({"reason": "Ctrl+S brought nothing back and nothing shows stashed"}).to_string(),
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

    #[test]
    fn the_stash_indicator_is_read_on_the_status_lines_only() {
        assert_eq!(
            stash_shown(&screen("", "Opus 5.5 | Context 6%")),
            Some(false)
        );
        assert_eq!(stash_shown(&screen("", "draft stashed")), Some(true));
        // Seen live after a restore that didn't take.
        assert_eq!(
            stash_shown(&screen("", "draft restored · draft stashed")),
            Some(true)
        );
        assert_eq!(
            stash_shown(&screen("half", "draft stashed · draft restored")),
            Some(false)
        );
        // Seen live: the notice right above the top rule.
        assert_eq!(
            stash_shown(&screen_with(
                "draft stashed · ✔ Update installed",
                "",
                "Opus 5.5"
            )),
            Some(true)
        );
        // A reply ending with the words, right above the box, isn't it.
        assert_eq!(
            stash_shown(&screen_with(
                "● The status line then reads draft stashed",
                "",
                "Opus 5.5"
            )),
            Some(false)
        );
        // No box recognised: unknown, not "nothing stashed".
        assert_eq!(stash_shown("no prompt at all\ndraft stashed"), None);
        assert_eq!(stash_shown("❯ text without rules\ndraft stashed"), None);
    }

    #[test]
    fn the_whole_box_is_the_draft() {
        let two = screen("first line\nsecond line", "Opus");
        assert_eq!(
            input_region(&two).as_deref(),
            Some("❯ first line\n  second line")
        );
        assert_eq!(input_region("no box"), None);
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
        // The stash on another key: the user moved it, left alone.
        assert!(!stash_bound(Some(&chat("ctrl+k", "\"chat:stash\""))));
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
    fn chords_on_ctrl_s_mean_no_stash() {
        let chat = |k: &str, v: &str| {
            format!(r#"{{"bindings": [{{"context": "Chat", "bindings": {{"{k}": {v}}}}}]}}"#)
        };
        assert!(!stash_bound(Some(&chat(
            "ctrl+s ctrl+x",
            "\"chat:submit\""
        ))));
        // Unbinding such a chord is fine.
        assert!(stash_bound(Some(&chat("ctrl+s ctrl+x", "null"))));
        assert!(stash_bound(Some(&chat(
            "ctrl+x ctrl+s",
            "\"chat:sendNow\""
        ))));
    }

    #[test]
    fn a_draft_is_stashed_only_when_safe() {
        let idle = screen("half a thought", "Opus 5.5");
        let old = DRAFT_IDLE + Duration::from_secs(1);
        let yes = || true;
        let e = Earlier::None;
        assert_eq!(plan(&idle, e, false, old, yes), Plan::Stash);
        assert!(matches!(
            plan(&idle, e, false, old, || false),
            Plan::Hold(_)
        ));
        assert!(matches!(
            plan(&idle, Earlier::Pending, false, old, yes),
            Plan::Hold(_)
        ));
        // A failed restore blocks: rlord can't see what the stash holds.
        let Plan::Hold(why) = plan(&idle, Earlier::Failed, false, old, yes) else {
            panic!("a failed restore must block");
        };
        assert!(why.contains("stash-dismiss"), "{why}");
        // After a Ctrl+S that didn't take: backs off.
        assert!(matches!(plan(&idle, e, true, old, yes), Plan::Hold(_)));
        assert!(matches!(
            plan(&screen("half", "draft stashed"), e, false, old, yes),
            Plan::Hold(_)
        ));
        assert!(matches!(
            plan("❯ half, no box", e, false, old, yes),
            Plan::Hold(_)
        ));
        let Plan::Hold(why) = plan(&idle, e, false, Duration::from_secs(3), yes) else {
            panic!("a fresh draft must hold");
        };
        assert!(why.contains("still be writing"));
        // The keybindings file is read only once everything else says yes.
        let read = std::cell::Cell::new(false);
        let key = || {
            read.set(true);
            true
        };
        let _ = plan(&idle, e, false, Duration::from_secs(3), key);
        assert!(!read.get(), "read before the age check");
    }

    #[test]
    fn after_the_stash_press_the_screen_decides() {
        let polls = |v: Vec<Option<String>>| {
            let mut it = v.into_iter();
            poll_stash(|| it.next().flatten())
        };
        let draft = || Some(screen("half a thought", "Opus"));
        // Empty at any look, indicator or not: stashed.
        assert_eq!(
            polls(vec![draft(), Some(screen("", "Opus"))]),
            AfterStash::Stashed
        );
        assert_eq!(
            polls(vec![Some(screen("", "draft stashed"))]),
            AfterStash::Stashed
        );
        // Still there after every look: untouched.
        assert_eq!(polls(vec![draft(); POLLS]), AfterStash::Untouched);
        // Still there, then unreadable: untouched (last readable).
        assert_eq!(polls(vec![draft(), None]), AfterStash::Untouched);
        assert_eq!(polls(vec![]), AfterStash::Unclear);
    }

    #[test]
    fn untouched_backs_off() {
        assert!(!backing_off("t-back"));
        back_off("t-back");
        assert!(backing_off("t-back"));
    }

    #[test]
    fn the_whole_first_line_must_match() {
        assert!(same_draft("fix the login bug", " fix the login bug "));
        assert!(!same_draft("fix the login", "fix the login bug"));
        assert!(!same_draft(
            "fix the login bug, and more",
            "fix the login bug"
        ));
        assert!(!same_draft("", ""));
    }

    #[test]
    fn a_draft_ages_only_while_unchanged() {
        assert_eq!(draft_age("t-age", "one"), Duration::ZERO);
        std::thread::sleep(Duration::from_millis(5));
        assert!(draft_age("t-age", "one") > Duration::ZERO);
        // A change on a later line of the box counts too.
        assert_eq!(draft_age("t-age", "one\ntwo"), Duration::ZERO);
        forget_draft("t-age");
        assert_eq!(draft_age("t-age", "one\ntwo"), Duration::ZERO);
        std::thread::sleep(Duration::from_millis(5));
        prune_drafts(|_| true);
        assert!(draft_age("t-age", "one\ntwo") > Duration::ZERO);
        prune_drafts(|id| id != "t-age");
        assert_eq!(draft_age("t-age", "one\ntwo"), Duration::ZERO);
    }

    #[test]
    fn restore_presses_only_on_a_stash_it_sees() {
        let line = "fix the login bug";
        let (yes, no) = (Some(true), Some(false));
        assert_eq!(restore_step(None, yes, true, true, line), Restore::Press);
        // No indicator, empty box: Claude restored it by itself (seen
        // live) and the user moved on. Never pressed: dropped.
        assert_eq!(restore_step(None, no, true, true, line), Restore::Gone);
        assert_eq!(restore_step(None, None, true, true, line), Restore::Wait);
        assert_eq!(restore_step(None, yes, true, false, line), Restore::Wait);
        assert_eq!(restore_step(None, no, true, false, line), Restore::Wait);
        assert_eq!(restore_step(None, yes, false, true, line), Restore::Wait);
        // Text in the box with the stash shown: never pressed.
        assert_eq!(
            restore_step(Some("new text"), yes, true, true, line),
            Restore::Wait
        );
        // Restored and edited (no stash shows): gone.
        assert_eq!(
            restore_step(Some("new text"), no, true, true, line),
            Restore::Gone
        );
        assert_eq!(
            restore_step(Some(line), no, true, false, line),
            Restore::Back
        );
        // Back but the stash shows still: not sure, so wait.
        assert_eq!(
            restore_step(Some(line), yes, true, true, line),
            Restore::Wait
        );
    }

    #[test]
    fn a_press_that_brings_nothing_back_drops_quietly() {
        let line = "fix the login bug";
        assert_eq!(after_press(Some(line), Some(false), line), Pressed::Back);
        assert_eq!(after_press(None, Some(true), line), Pressed::Retry);
        assert_eq!(after_press(None, None, line), Pressed::Retry);
        assert_eq!(after_press(None, Some(false), line), Pressed::Unknown);
        assert_eq!(
            after_press(Some("other"), Some(false), line),
            Pressed::Unknown
        );
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_ms(0), 2_000);
        assert_eq!(backoff_ms(1), 4_000);
        assert_eq!(backoff_ms(9), 60_000);
    }
}
