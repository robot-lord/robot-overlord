//! rlord's MCP tool surface. The protocol itself is spoken by the `rlord mcp`
//! stdio shim; it forwards `tools/list` → `mcp.tools` and `tools/call` →
//! `mcp.call` here. Every tool is a thin mapping onto daemon methods, with
//! parameters whitelisted (internal knobs like `extra_args`/`role` are never
//! reachable from a tool call).

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use rlord_core::db::SortMode;
use rlord_core::timefmt::rel_age;
use rlord_core::tmux;

use crate::Daemon;

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required},
    })
}

fn s(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}
fn b(desc: &str) -> Value {
    json!({"type": "boolean", "description": desc})
}

const SESSION_REF: &str = "Session id, id prefix, or name";

/// The tools `rlord mcp` serves without `--overlord`: look-ups that change
/// nothing (no session, tag, view, kv watermark or UI state), start no
/// session, model call or agent CLI. Any session may add this server.
/// Left out though they only read: `get_session_defaults` and
/// `list_agents` (they warm the agent install cache or list models by
/// running agent CLIs), `triage` and `scan_agent_sessions` (overlord
/// upkeep and import prep).
pub const READ_ONLY_TOOLS: &[&str] = &[
    "list_sessions",
    "get_session",
    "read_screen",
    "get_ui_state",
    "search_history",
    "read_history",
    "recall",
    "list_groups",
    "find_repos",
    "keys_list",
];

/// The tools served: all of them for the overlord, else the read-only set.
pub fn tools_for(overlord: bool) -> Value {
    let all = tools();
    if overlord {
        return all;
    }
    Value::Array(
        all.as_array()
            .into_iter()
            .flatten()
            .filter(|t| READ_ONLY_TOOLS.contains(&t["name"].as_str().unwrap_or("")))
            .cloned()
            .collect(),
    )
}

pub fn tools() -> Value {
    let sort = json!({"type": "string", "enum": ["updated", "user", "state", "created", "name", "usage"],
                      "description": "updated = last message (from the user, the agent, or one you sent it; a resume or status flip alone does not count); user = the user's own last prompt; state = last status change (started, stopped, finished, needs you); shown to the user as last message / last prompt / status change; usage = most used first, by the usage measure (see `usage`)"});
    let usage = json!({"type": "string", "enum": ["turns", "tokens", "context"],
                       "description": "The usage measure: turns (prompts the user typed), tokens (taken in new or written), context (% of the context window in use). Default: the user's configured one."});
    let mut new_session_props = json!({
        "repo": s("Git repo path to work in (see find_repos). Omit to use the configured default repo."),
        "project": s("Plain folder instead of a repo (no worktree)"),
        "name": s("Short display name; also derives the branch"),
        "branch": s("Branch / worktree name (default: derived from name)"),
        "agent": s("Coding agent to run: claude, codex or opencode (default: the configured session agent). Only when the user asks for one."),
        "model": s("Model, one of that agent's (default: the configured model for the agent; see get_session_defaults)"),
        "workspace": b("Give the session its own git worktree (default: configured, normally true)"),
        "prompt": s("The user's words, verbatim: their request as they put it, nothing of yours. It is the session's first prompt. Your notes go in `context`."),
        "context": s("Background from you, kept apart from the user's words: facts and pointers the session can't find itself (answers the user gave you, the right folder or files, a decision). Not new requirements. The session is shown it labelled as yours, not the user's. At most 8000 characters."),
        "related": {"type": "array", "items": {"type": "string"},
                    "description": "Sessions this work builds on (id, id prefix or name; at most 8, and 16 per session in all): listed in its background with their folder, status and description. Not the overlord."},
    });
    new_session_props["prefer"] = json!({"type": "string", "enum": ["local", "remote"],
        "description": "Only with `base`, after a launch refused because that branch's local and remote sides have diverged: the side the user chose"});
    new_session_props["base"] = s("Where a new branch starts. Omit: the user's configured default ([session] base: the latest default branch, or \"head\"). \"latest\": the latest default branch, fetched, whatever the config says. A branch name, or <remote>/<branch> for the remote's: its newest commit, to build on it (refused if the local and remote branch have diverged: ask the user, then pass `prefer`). \"head\": whatever the repo's main checkout has now. Ignored when `branch` already exists or there is no worktree. The answer's `base` says which commit was used.");
    let mut create_props = new_session_props.clone();
    create_props["go"] = b("Switch the user's terminal to the new session (default false)");
    create_props["remote_control"] = b("Turn on Remote Control (continue it from the Claude app) once launched; only when the user asks. Claude Code sessions only.");
    json!([
        tool("list_sessions",
            "List agent sessions with their meta-context (status, model, repo/branch, tags, description, recent activity). Status is one of: working; needs you (blocked mid-turn on the user: a permission prompt or a question); your turn (its turn is over, the next prompt is the user's); background work (its turn is over but background work it started still runs, `async_tasks` subagents, `async_labels` says what: the user is waiting on the agent; it carries on by itself when they finish. Background shells (dev servers, watchers) don't change the status: `async_shells` lists them); stopped; starting; error; limited (the agent stopped on a usage limit: `limited_until` says when it resets, `limit_reset` that the limit just reset and within a minute it can be sent to again (it carries on by itself only if `resume_scheduled`), `resume_scheduled` whether resume_after_limit will pick it up). The overlord itself is not listed.",
            json!({
                "query": s("Search: free words match name/project/tag; status:needs (needs you) status:turn (your turn) status:working status:bg (waiting on background work) status:limited (stopped on a usage limit) tag:x proj:x agent:x filter"),
                "archived": {"type": "string", "enum": ["live", "only", "all"], "description": "default live"},
                "sort": sort.clone(),
                "usage": usage,
                "reverse": b("Reverse the sort"),
            }), &[]),
        tool("get_session",
            "Everything about one session: details, notes, background (your notes for its agent and related sessions), recent events, sub-agents.",
            json!({"session": s(SESSION_REF)}), &["session"]),
        tool("read_screen",
            "The text currently on a session's terminal (its live pane), last N lines.",
            json!({"session": s(SESSION_REF), "lines": {"type": "integer", "description": "default 60"}}),
            &["session"]),
        tool("get_ui_state",
            "What the user is looking at: which session their terminal shows, whether your overlay is open, the list's current and default view.",
            json!({}), &[]),
        tool("goto_session",
            "Switch the user's terminal to a session (hides your overlay). Stopped sessions are resumed first.",
            json!({"session": s(SESSION_REF)}), &["session"]),
        tool("show_list",
            "Show the user the rlord session list (hides your overlay).",
            json!({}), &[]),
        tool("search_history",
            "Search what sessions said and did: their summaries, descriptions, notes and full transcripts (archived ones too). Returns matching sessions, best first, each with a few short snippets and the transcript line of each. Bounded; plain text search, every word must appear. Not your own conversations: those are `recall`.",
            json!({
                "query": s("Words to find (all must appear in one message)"),
                "session": s("Only this session (id, id prefix, or name): for \"what happened in X?\". The archived overlord-front-desk (the overlord's old front desk) is searched only when named here"),
                "scope": {"type": "string", "enum": ["rlord", "all"], "description": "rlord's sessions (default), or also agent conversations (Claude Code, Codex, OpenCode) started outside rlord"},
                "limit": {"type": "integer", "description": "Sessions to return (default 8)"},
            }), &["query"]),
        tool("read_history",
            "Read a session's conversation around a search_history hit (or its last messages): text only, tool output clipped, about 12k characters at most.",
            json!({
                "session": s(SESSION_REF),
                "transcript": s("A hit's transcript path from search_history: an outside_rlord one (instead of session), or a session's older one (with session)"),
                "line": {"type": "integer", "description": "A hit's line; omit for the last messages"},
                "before": {"type": "integer", "description": "Messages before the line (default 6)"},
                "after": {"type": "integer", "description": "Messages from the line on (default 14)"},
            }), &[]),
        tool("summarize_sessions",
            "Write archive summaries (what each session was for, did, left unfinished) in the background, one cheap model call each. Archiving makes one automatically; use this for sessions archived without one, or to redo one.",
            json!({
                "sessions": {"type": "array", "items": {"type": "string"}, "description": "Session refs (at most 20)"},
                "force": b("Replace an existing summary"),
            }), &["sessions"]),
        tool("keys_list",
            "Every key rlord binds (global keys, the list's keys) as set now, each with what stands in its way: taken (won't work as set) or shadows (rlord takes it from Claude Code, the shell or the user's tmux).",
            json!({}), &[]),
        tool("open_keys",
            "Open the key screen on the user's list (hides your overlay), optionally at one key. The user presses the new key there: rlord tests it reaches rlord, checks it, and saves it to config.toml when they confirm.",
            json!({"key": s("Key id from keys_list (e.g. tmux.back, keys.quit); omit for the whole list")}), &[]),
        tool("set_list_view",
            "Set the session list's tab, filter and sort, optionally save it as the default view, and show it to the user. The list has two tabs: 'sessions' (flat) and 'groups' (sessions by tag: tags are the groups).",
            json!({
                "tab": {"type": "string", "enum": ["sessions", "groups"], "description": "Which tab to show"},
                "group": s("On the groups tab, the group (tag) to open; \"\" = untagged. Implies tab groups."),
                "query": s("Filter text (same syntax as list_sessions query); \"\" clears it"),
                "sort": sort,
                "usage": {"type": "string", "enum": ["turns", "tokens", "context", "default"],
                          "description": "What the list's Usage column shows (and sort usage sorts by): turns, tokens or context %. Remembered across restarts, like the user's own pick; \"default\" forgets the pick (config usage again). Only when the user asks."},
                "reverse": b("Reverse the sort"),
                "archived": {"type": "string", "enum": ["live", "only"], "description": "Show live or archived sessions"},
                "select": s("Session to highlight in the list"),
                "save_as_default": b("Also make this the default view the list opens with"),
                "show": b("Take the user to the list (default true)"),
            }), &[]),
        tool("find_repos",
            "Find git repos to launch sessions in: known repos (most recent first, default marked) plus repos discovered on disk. `scanning: true` means the first disk scan of the roots is still running, so only known repos are listed: ask again shortly if the repo isn't there. `skipped`: folders the scan stayed out of (a macOS privacy prompt, another disk): a repo in one is found by its path, not by scanning.",
            json!({"query": s("Words that must appear in the path")}), &[]),
        tool("repo_latest",
            "The latest committed code of a repo, in a read-only folder rlord keeps in the repo (`<repo>/.worktrees/.rlord-latest`, a detached checkout; never the user's own, never a session's). Plain code, no AI: fetches the one branch (cached for a minute; offline is reported, not an error), then moves that folder: with no `ref`, to the default branch's commit as the remote has it, even when the local branch is ahead (a `local_ahead` warning says so); for `ref`, to whichever of the local and remote branch is ahead. One folder per repo: a call for another ref moves it, so whoever reads it should check `commit` (or `git -C <path> rev-parse HEAD`). Anything edited in it is thrown away on the next move. `diverged`: both sides are returned and nothing moves; ask the user, then pass `prefer`. Answers with `path` (read there), `ref`, `commit`, `moved` (from where, how many commits), `fetch` (fetched; cached; running: ask again shortly; offline; failed; missing: not on the remote; no_remote), `local` (the user's branch: ahead/behind, where it is checked out, how many files are uncommitted there) and `warnings`. `tree: \"preparing\"` (no `path`): its folder is still being made the first time; call again shortly. A folder that is not a git repo comes back as it is. Name the commit when you answer (\"on main at abc1234\"). Uncommitted work is only in the user's or a session's own folder (`local` says where and, for a `ref`, how much): ask that session, and read its folder only when the user asks about what it hasn't committed. A session in an existing folder (no worktree) is never updated by rlord: to give it the latest code to read, put this `path` and `commit` in its `context`. Pass on a warning that changes the answer, in a few words (\"offline: as of the last fetch\").",
            json!({
                "repo": s("Repo path (see find_repos), or any folder inside it"),
                "ref": s("A branch name, e.g. a session's `branch`; <remote>/<branch> for the remote's. Omit for the default branch"),
                "prefer": {"type": "string", "enum": ["local", "remote"],
                           "description": "Only after a `diverged` answer: the side the user chose"},
            }), &["repo"]),
        tool("get_session_defaults",
            "The defaults new sessions get (agent, model, model choices, workspace, default repo), `code_dir` (the folder the user keeps repos in and where new ones go; \"\" = not set yet, see confirm_setup), `repo_roots` as the user configured them, `scan_roots` (what the repo finder scans right now: a readout, never pass it to confirm_setup as repo_roots), and under `agents` each agent rlord can run: whether it's installed, its default model and its model choices.",
            json!({}), &[]),
        tool("list_agents",
            "The coding agents rlord knows (Claude Code, Codex, OpenCode): whether each is installed (and its version), whether rlord can run sessions on it (`too_old`: installed, but a version rlord can't run), whether rlord's hooks are installed (and, for Codex, trusted; null for OpenCode, which needs none), what's left to do, and for one that's missing the install command to suggest for this machine. Plain look-ups, no AI. rlord never installs an agent: the user runs the command.",
            json!({}), &[]),
        tool("recall",
            "Your own earlier conversations (you restart on a fresh one after sitting idle, and the user may clear you). With `query`: the conversations that mention every word, newest first, with short dated snippets. Without: a digest of your previous conversation (what the user asked, what you answered, clipped). Plain look-up, bounded; nothing of it is in your context until you ask.",
            json!({
                "query": s("Words to find in your earlier conversations (all must appear in one message); omit for a digest of the previous one"),
                "conversation": s("A conversation id from a recall search: digest that one instead of the previous"),
                "limit": {"type": "integer", "description": "Conversations to return for a query (default 6)"},
            }), &[]),
        tool("confirm_setup",
            "Open the code-folder form on the user's screen (hides your overlay) for them to confirm: the folder they keep repos in (`code_dir`: the repo finder scans it and new repos go there). Pass what you would save; with nothing passed the form shows rlord's own suggestion, worked out from the projects it has seen. Nothing is saved until they confirm there, and rlord saves it then: you don't act again.",
            json!({
                "code_dir": s("The folder as you would save it; answering a setup request, the one the description names (it wins over the hints). Omit: rlord's suggestion"),
                "repo_roots": {"type": "array", "items": {"type": "string"},
                               "description": "More folders for the repo finder to scan besides code_dir, which it always scans. Omit to leave the user's configured ones as they are; [] to scan only code_dir. Answering a setup request: pass the roots choice exactly as the request's roots line says, unless the description asks for something else. Never pass get_session_defaults' scan_roots here"},
                "description": s("The user's description, verbatim, when answering a setup request"),
                "changed": {"type": "array", "items": {"type": "string", "enum": ["code_dir", "repo_roots"]},
                            "description": "Hints you changed from what the user sent"},
                "note": s("One line for the top of the form, e.g. \"Scanning both ~/ko and ~/proj, new repos in ~/ko\""),
            }), &[]),
        tool("propose_session",
            "Open the new-session form pre-filled on the user's screen for them to review and launch. Prefer this over create_session unless told to just do it.",
            new_session_props, &[]),
        tool("confirm_new_session",
            "Reopen the user's new-session form with your corrections when their description contradicts the hints they sent (or left as defaults): their description verbatim, the hints as you would launch them, which ones you changed and why. Nothing launches until they send it back; that message says `confirmed: yes`.",
            json!({
                "description": s("The user's description, verbatim"),
                "request_id": s("The `request_id:` of the new-session request you're answering (omit if none)"),
                "repo": s("Repo as you would launch it (omit: you choose at launch)"),
                "project": s("Plain folder instead of a repo"),
                "agent": s("Coding agent as you would launch it: claude, codex or opencode (omit: the one the user sent)"),
                "model": s("Model as you would launch it (\"\" for the agent default)"),
                "workspace": b("Own git worktree"),
                "tags": {"type": "array", "items": {"type": "string"}},
                "prefer": {"type": "string", "enum": ["local", "remote"],
                           "description": "With `base`, when that branch's local and remote sides have diverged: the side the user chose"},
                "base": s("Where its new branch would start, as you would launch it: \"latest\" (the latest default branch), a branch or <remote>/<branch> to build on, or \"head\" (omit: what the user sent, kept; \"\": the configured default)"),
                "changed": {"type": "array", "items": {"type": "string", "enum": ["repo", "agent", "model", "worktree", "base", "tags"]},
                            "description": "Hints you changed from what the user sent"},
                "note": s("One line for the top of the form, e.g. \"Updated to match your description: repo is myapp\""),
            }), &["description", "changed", "note"]),
        tool("create_session",
            "Create and launch a new agent session immediately.",
            create_props, &[]),
        tool("fork_session",
            "Start a new session that goes on from a copy of another session's whole conversation, on the same agent, in the same folder (or a new worktree). The original is left as it is. For trying another approach or branching off. Not for work that only needs to know about a session: launch a new one with `related` for that. The answer is the new session, with `forked_from` and `warnings` (shared folder, a working original, a nearly full context window) to pass on.",
            json!({
                "session": s(SESSION_REF),
                "prompt": s("The user's words for what the fork should do first, verbatim (optional: without it the fork opens idle at its input)"),
                "context": s("Your own notes for the fork, as for create_session (optional). The fork already gets the original's background and a note saying what it was forked from."),
                "related": {"type": "array", "items": {"type": "string"},
                            "description": "More related sessions for its background, as for create_session (the original is linked already)"},
                "name": s("Its name (default: <original>-fork, then -fork-2, …)"),
                "description": s("Its one-line description (default: the original's)"),
                "model": s("Another model than the original's, one of that agent's (default: the original's)"),
                "worktree": b("Work in a new git worktree branched from the original's last commit instead of its folder (default false). Claude Code and Codex only; uncommitted changes don't come along. Only when the user asks, or the two must not touch the same files."),
                "branch": s("The new worktree's branch, with worktree only (default: derived from the name)"),
                "go": b("Switch the user's terminal to the fork (default false)"),
            }), &["session"]),
        tool("update_session",
            "Edit a session's meta-context. Only the fields given change.",
            json!({
                "session": s(SESSION_REF),
                "name": s("New name"),
                "description": s("One-line description"),
                "notes": s("Replace the free-form notes"),
                "add_context": s("Add a note to the session's background (for its agent, labelled as yours, not the user's): a fact or pointer it needs. The answer's `background_delivery` says when the agent gets it: next_prompt (with its next prompt), next_start (at its next start), file_only (written to its background file only). Not next_prompt and it must know now: also pass it with send_to_session. At most 8000 characters."),
                "add_related": {"type": "array", "items": {"type": "string"},
                                "description": "Add related sessions to its background (refs; at most 8 per call, 16 per session in all)"},
                "add_tags": {"type": "array", "items": {"type": "string"}},
                "remove_tags": {"type": "array", "items": {"type": "string"}},
                "pinned": b("Pin to the top of the list"),
            }), &["session"]),
        tool("list_groups",
            "The groups (tags) on live sessions with how many sessions each has, plus how many are untagged.",
            json!({}), &[]),
        tool("organize",
            "Organize many sessions in one call: per session add/remove tags (tags are the groups), rename, describe. Only the fields given change.",
            json!({"changes": {"type": "array", "items": {"type": "object", "properties": {
                "session": s(SESSION_REF),
                "add_tags": {"type": "array", "items": {"type": "string"}},
                "remove_tags": {"type": "array", "items": {"type": "string"}},
                "name": s("New name"),
                "description": s("One-line description"),
            }, "required": ["session"]}}}), &["changes"]),
        tool("rename_group",
            "Rename a group (tag) on every session that has it; merges into an existing group of the new name.",
            json!({"from": s("Current tag"), "to": s("New tag (one word)")}), &["from", "to"]),
        tool("remote_control",
            "Remote Control: continue a Claude Code session from the Claude app or claude.ai while it keeps running here. `on` true/false turns it on or off: remembered across restarts, and a running session is restarted (conversation resumed) with or without it. If the session has work going, nothing changes and the answer is `needs_confirm` with what a restart would interrupt: ask the user, then call again with `restart`. Without `on` it reports state, url and whether this machine can use it at all (and why not).",
            json!({
                "session": s(&format!("{SESSION_REF}; omit for every session with it on")),
                "on": b("Turn it on (true) or off (false); omit to just report"),
                "restart": {"type": "string", "enum": ["now", "when_idle"],
                            "description": "After needs_confirm, the user's pick: restart now (interrupting), or as soon as its work is done"},
            }), &[]),
        tool("set_group_order",
            "Set the user's favourite groups, in order: they head the groups list (★); the rest follow, biggest first. Replaces the whole list, so pass every favourite you want to keep (get_ui_state / list_groups shows the current order).",
            json!({"order": {"type": "array", "items": {"type": "string"}, "description": "Tags, most important first; [] clears"}}),
            &["order"]),
        tool("triage",
            "Cheap first pass for upkeep (no AI, no screen reads): live sessions changed since you last reviewed them, newest first, with what their meta-context is missing (flags: default_name, no_description, untagged, waiting_on_user) and excerpts from their transcripts (first_prompt, last_prompt, last_reply, title). Editing a session with update_session/organize marks it reviewed.",
            json!({
                "all": b("List every live session, not just changed ones"),
                "limit": {"type": "integer", "description": "Max sessions (default 20)"},
            }), &[]),
        tool("mark_reviewed",
            "Mark sessions reviewed without changing them (they looked fine), so triage stops listing them until they change again.",
            json!({
                "sessions": {"type": "array", "items": {"type": "string"}, "description": "Session refs"},
                "all": b("Mark every live session reviewed"),
            }), &[]),
        tool("scan_agent_sessions",
            "Find agent conversations on this machine that rlord doesn't track yet (Claude Code's and Codex's, each with its agent; OpenCode's aren't importable; read from the agents' own transcript files; no AI), most recently active first, for importing existing work. Each has its folder (and whether it still exists or can be recreated), repo/branch, title, first/latest prompt, latest reply, PR link, when it started and was last active, and whether it may be open elsewhere right now.",
            json!({
                "since_days": {"type": "integer", "description": "Only conversations with an exchange in the last N days (default 30)"},
                "limit": {"type": "integer", "description": "Max conversations (default 30)"},
                "query": s("Words that must all appear in the folder, branch, title or prompts"),
                "ids": {"type": "array", "items": {"type": "string"}, "description": "Only these conversation ids (any age)"},
            }), &[]),
        tool("import_sessions",
            "Import conversations found by scan_agent_sessions as stopped rlord sessions that resume them, with name, description, notes and tags. Per-conversation results.",
            json!({"sessions": {"type": "array", "items": {"type": "object", "properties": {
                "id": s("Conversation id from scan_agent_sessions"),
                "name": s("Short display name"),
                "description": s("One-line description"),
                "notes": s("Durable context (PR link, what's left)"),
                "tags": {"type": "array", "items": {"type": "string"}},
                "archive": b("Import as archived (history only; required when its folder is gone and can't be recreated)"),
                "recreate_worktree": b("Its folder is gone but recreatable: recreate the worktree there"),
            }, "required": ["id"]}}}), &["sessions"]),
        tool("send_to_session",
            "Pass a message to an existing session: typed into its agent's prompt and submitted, as if the user had typed it. Use it to hand a session work or a follow-up. Only a session whose turn is over (your turn) takes it now; a working one or one that needs the user is refused unless `when_idle` (then it is delivered when that turn ends; one pending message per session, a newer one replaces it). Stopped, starting and archived sessions are refused; so is one stopped on a usage limit (with `when_idle`, held until the limit resets). Right before typing rlord also checks the session's screen: while a dialog is up there, its input holds the user's unsent text, it was typed into in the last few seconds, or the user is typing in it, it is not typed into (queued with `when_idle`, else refused with the reason).",
            json!({
                "session": s(SESSION_REF),
                "text": s("The message, complete: the session sees only this"),
                "when_idle": b("Session mid-turn: deliver when its turn is over instead of refusing (default false)"),
            }), &["session", "text"]),
        tool("resume_after_limit",
            "Pick sessions up when their usage limit resets. A session the agent stopped on a usage limit shows `status: limited` with `limited_until`. This schedules one resume per session: once its limit has reset (plus a minute) the session is sent the message, typed in as send_to_session types, unless it was used since the limit hit (the user typed, or it answered): then it is left alone. If the limit hits again the resume waits for the next reset. Scheduled resumes survive an rlord restart. Only when the user asks; nothing is resumed automatically. Without `sessions`: every session limited right now. With `cancel: true` it calls scheduled resumes off instead (without `sessions`: all of them); the limit stays shown.",
            json!({
                "sessions": {"type": "array", "items": {"type": "string"},
                             "description": "Sessions (id, id prefix or name). Omit for every limited session."},
                "message": s("What to send (default: the user's configured resume message, \"continue\")"),
                "cancel": b("Cancel the scheduled resume instead of scheduling one (default false)"),
            }), &[]),
        tool("session_action",
            "Lifecycle: restart (resumes the conversation), stop (resumable), archive (stop + hide), unarchive.",
            json!({
                "session": s(SESSION_REF),
                "action": {"type": "string", "enum": ["restart", "stop", "archive", "unarchive"]},
            }), &["session", "action"]),
    ])
}

fn arg_str<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn session_ref(args: &Value) -> Result<String> {
    arg_str(args, "session")
        .map(str::to_string)
        .context("missing 'session'")
}

/// Labels of a `[{label, started_at}]` task list.
fn labels(tasks: &Value) -> Value {
    tasks.as_array().map_or(json!([]), |t| {
        t.iter().map(|t| t["label"].clone()).collect()
    })
}

/// Background work, as list_sessions and get_session show it (read from
/// the annotated session `s`): `async_tasks` (how many subagents run
/// while its turn is over) with `async_labels` (what), and
/// `async_shells` (`{count, labels}`, information only).
fn background_fields(s: &Value) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    if let Some(n) = s["async_tasks"].as_u64() {
        out.push(("async_tasks", json!(n)));
        out.push(("async_labels", labels(&s["async_work"])));
    }
    if let Some(n) = s["async_shells"]["count"].as_u64() {
        out.push((
            "async_shells",
            json!({"count": n, "labels": labels(&s["async_shells"]["tasks"])}),
        ));
    }
    out
}

/// The status fields every overlord tool gives a session (from its
/// annotated row `s`): `status` in words, background work
/// (`background_fields`), and a usage limit (`limited_until`,
/// `limit_reset`: its reset time just passed, `resume_scheduled`).
fn status_fields(s: &Value) -> Vec<(&'static str, Value)> {
    let mut out = vec![("status", status_word(s))];
    out.extend(background_fields(s));
    if !s["limit"].is_null() {
        out.push(("limited_until", s["limit"]["until"].clone()));
        out.push(("limit_reset", s["limit"]["reset"].clone()));
        out.push(("resume_scheduled", s["limit"]["resume"].clone()));
    }
    out
}

/// An annotated row as the overlord gets it (`get_session`, `triage`):
/// the `status_fields` in place of the raw annotations.
pub(crate) fn overlord_status(s: &mut Value) {
    let fields = status_fields(s);
    if let Some(o) = s.as_object_mut() {
        for k in [
            "shown_status",
            "async_tasks",
            "async_work",
            "async_shells",
            "limit",
        ] {
            o.remove(k);
        }
    }
    for (k, v) in fields {
        s[k] = v;
    }
}

/// The status as the overlord sees it: the daemon's `shown_status`
/// (`background` and `limited` as they are, the rest in the words people
/// see: working, needs you, your turn, stopped).
fn status_word(s: &Value) -> Value {
    match s["shown_status"]
        .as_str()
        .or(s["status"].as_str())
        .unwrap_or("")
    {
        "background" => json!("background work"),
        "limited" => json!("limited"),
        st => json!(rlord_core::types::Status::label_of(st)),
    }
}

/// The compact row the overlord sees per session.
fn summarize(s: &Value) -> Value {
    let ts = |k: &str| s[k].as_i64().map(|t| rel_age(Some(t)));
    let mut row = json!({
        "id": s["id"], "name": s["name"],
        "agent": s["agent"], "model": s["model"],
        "repo": s["repo_path"], "dir": s["project_path"], "branch": s["branch"],
        "tags": s["tags"], "description": s["description"],
        "pinned": s["pinned"], "archived": !s["archived_at"].is_null(),
        "last_agent_msg": ts("last_agent_msg_at"), "last_user_msg": ts("last_user_msg_at"),
        // A prompt rlord typed in (your send_to_session, a resume): not
        // the user's.
        "last_rlord_msg": ts("last_rlord_msg_at"),
        "subagents_running": s["agents_running"],
        // Turns, tokens, context % (null while unknown; "loading" while
        // rlord reads it).
        "usage": match rlord_core::usage::Usage::from_json(&s["usage"]) {
            Some(u) => json!({
                "turns": u.turns, "tokens": u.tokens, "context_pct": u.context_pct(),
            }),
            None if s["usage_loading"] == true => json!("loading"),
            None => Value::Null,
        },
        // Remote Control: on / connecting / failed / … (absent when off).
        "remote": s["remote"]["state"], "remote_url": s["remote"]["url"],
    });
    // A fork: what it was forked from.
    if let Some(from) = s["forked_from"].as_str() {
        row["forked_from"] = json!({"id": from, "name": s["forked_from_name"]});
    }
    // How many live sessions were forked from it (the row's count;
    // get_session lists them all, archived ones too).
    let live_forks = s["live_fork_count"].as_u64().unwrap_or(0);
    if live_forks > 0 {
        row["live_forks"] = json!(live_forks);
    }
    for (k, v) in status_fields(s) {
        row[k] = v;
    }
    // Archived: what it did (get_session has it all).
    if !s["archived_at"].is_null() {
        let sum = s["summary"].as_str().unwrap_or("");
        row["summary"] = if sum.is_empty() {
            if s["summarizing"] == true {
                json!("(being written)")
            } else {
                Value::Null
            }
        } else {
            json!(rlord_core::agents::transcript::clip(sum, 200))
        };
    }
    row
}

/// The compact row the overlord sees per importable conversation.
fn summarize_candidate(c: &Value) -> Value {
    let ts = |k: &str| c[k].as_i64().map(|t| rel_age(Some(t)));
    let mut row = json!({
        "id": c["agent_session"], "agent": c["agent"], "dir": c["cwd"],
        "repo": c["repo"], "branch": c["branch"], "worktree": c["worktree"],
        "title": c["title"], "first_prompt": c["first_prompt"],
        "last_prompt": c["last_prompt"], "last_reply": c["last_reply"],
        "pr": c["pr_url"], "started": ts("started_at"), "last_active": ts("last_active_at"),
    });
    if c["dir_exists"] == false {
        row["dir_gone"] = json!(true);
        row["recreatable"] = c["recreatable"].clone();
    }
    if c["open_elsewhere"] == true || c["recently_written"] == true {
        row["maybe_open_elsewhere"] = json!(true);
    }
    if c["maybe_held_by_codex_server"] == true {
        row["maybe_held_by_codex_app"] = json!(true);
    }
    row
}

/// What `find_repos` hands the overlord from a `repo.find` result: the
/// repos matching `query` (at most 40), whether the scan is still running,
/// the folders it stayed out of (at most 10, `skipped_more` counts the
/// rest), and a note when the finder scans nothing.
fn shape_find_repos(r: &Value, query: &str) -> Value {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let repos: Vec<Value> = r["repos"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|x| {
                    let p = x["path"].as_str().unwrap_or("").to_lowercase();
                    words.iter().all(|w| p.contains(w.as_str()))
                })
                .take(40)
                .map(|x| {
                    json!({"path": x["path"], "default": x["default"],
                           "last_used": x["last_used_at"].as_i64().map(|t| rel_age(Some(t)))})
                })
                .collect()
        })
        .unwrap_or_default();
    let mut out = json!({"repos": repos});
    if r["scanning"].as_bool() == Some(true) {
        out["scanning"] = true.into();
    }
    let skipped = r["skipped"].as_array().cloned().unwrap_or_default();
    if !skipped.is_empty() {
        out["skipped"] = skipped
            .iter()
            .take(10)
            .map(|k| json!({"path": k["path"], "why": k["text"]}))
            .collect();
        let more =
            skipped.len().saturating_sub(10) as u64 + r["skipped_more"].as_u64().unwrap_or(0);
        if more > 0 {
            out["skipped_more"] = more.into();
        }
    }
    if r["roots"].as_array().is_some_and(|a| a.is_empty()) {
        out["note"] = json!(
            "no code folder set: only repos already used are listed (confirm_setup sets one)"
        );
    }
    out
}

/// A full commit id, short.
fn short_commit(v: &Value) -> Value {
    match v.as_str() {
        Some(c) => json!(rlord_core::latest::short(c)),
        None => v.clone(),
    }
}

/// Drop nulls, empty strings, lists and objects, all the way down.
fn prune_empty(v: &mut Value) {
    if let Value::Object(m) = v {
        for x in m.values_mut() {
            prune_empty(x);
        }
        m.retain(|_, x| {
            !(x.is_null()
                || x.as_array().is_some_and(Vec::is_empty)
                || x.as_object().is_some_and(Map::is_empty)
                || x.as_str() == Some(""))
        });
    }
}

/// `*_at` times as ages (`committed_at` → `committed: "2h ago"`) and
/// commits short, in one level of `v`.
fn ages_and_commits(v: &mut Value) {
    let Value::Object(m) = v else {
        return;
    };
    for (k, out) in [("committed_at", "committed"), ("fetched_at", "fetched")] {
        if let Some(t) = m.remove(k).and_then(|t| t.as_i64()) {
            m.insert(out.into(), json!(format!("{} ago", rel_age(Some(t)))));
        }
    }
    for k in ["commit", "from"] {
        if let Some(c) = m.get(k).map(short_commit) {
            m.insert(k.into(), c);
        }
    }
}

/// What `repo_latest` hands the overlord from `repo.latest`: short
/// commits, ages, nothing empty.
fn shape_repo_latest(r: &Value) -> Value {
    let mut v = r.clone();
    ages_and_commits(&mut v);
    for k in ["moved", "local", "remote_side"] {
        if let Some(x) = v.get_mut(k) {
            ages_and_commits(x);
        }
    }
    // (The RPC's name for the remote side: `remote` is the remote's name.)
    if let Some(side) = v.as_object_mut().and_then(|m| m.remove("remote_side")) {
        v["remote_branch"] = side;
    }
    prune_empty(&mut v);
    v
}

/// A launch's `base` for the overlord.
fn shape_base(b: &Value) -> Value {
    let mut v = b.clone();
    ages_and_commits(&mut v);
    prune_empty(&mut v);
    v
}

/// What `confirm_new_session` passes on: the description, the hints as
/// the overlord would launch them (`base` and its `prefer` included),
/// which it changed, and its note.
fn confirm_params(args: &Value) -> Value {
    let mut p = Map::new();
    for k in [
        "description",
        "request_id",
        "repo",
        "project",
        "agent",
        "model",
        "workspace",
        "base",
        "prefer",
        "tags",
        "changed",
        "note",
    ] {
        if let Some(v) = args.get(k) {
            p.insert(k.into(), v.clone());
        }
    }
    // (A side only with the base it is for.)
    if p.get("base")
        .and_then(Value::as_str)
        .is_none_or(|b| b.trim().is_empty())
    {
        p.remove("prefer");
    }
    Value::Object(p)
}

fn params_wants_remote(args: &Value) -> bool {
    args.get("remote_control").and_then(Value::as_bool) == Some(true)
}

/// Copy whitelisted session-creation fields.
fn new_session_params(args: &Value) -> Value {
    let mut p = Map::new();
    for k in [
        "repo", "project", "name", "branch", "agent", "model", "prompt", "context", "base",
        "prefer",
    ] {
        if let Some(v) = arg_str(args, k) {
            p.insert(k.into(), json!(v));
        }
    }
    if let Some(w) = args.get("workspace").and_then(Value::as_bool) {
        p.insert("workspace".into(), json!(w));
    }
    let related = crate::methods::ref_list(args, "related");
    if !related.is_empty() {
        p.insert("related".into(), json!(related));
    }
    // (A side only with the base it is for.)
    if !p.contains_key("base") {
        p.remove("prefer");
    }
    Value::Object(p)
}

/// How session `id`'s agent can look things up itself
/// (`session_tools::lookup_kind`: `mcp`, `cli` or `none`).
fn lookup_of(d: &Daemon, id: &str) -> Value {
    // (One lock: the DB mutex isn't re-entrant.)
    let db = d.db();
    match db.resolve_ref(id) {
        Ok(s) => json!(crate::session_tools::lookup_kind(
            &db,
            &rlord_core::paths::data_dir(),
            &s
        )),
        Err(_) => Value::Null,
    }
}

pub fn call(d: &Daemon, name: &str, args: &Value) -> Result<Value> {
    match name {
        "list_sessions" => {
            let mut q = rlord_core::db::SessionQuery::default();
            if let Some(sort) = arg_str(args, "sort") {
                q.sort = SortMode::parse(sort).with_context(|| format!("unknown sort '{sort}'"))?;
            }
            q.apply_search_text(arg_str(args, "query").unwrap_or(""));
            let list = d.rpc(
                "session.list",
                json!({
                    "archived": arg_str(args, "archived").unwrap_or("live"),
                    "sort": q.sort.label(),
                    "usage": arg_str(args, "usage"),
                    "reverse": args.get("reverse").and_then(Value::as_bool).unwrap_or(false),
                    "status": q.status, "tag": q.tag, "agent": q.agent,
                    "project": q.project, "group": q.group, "search": q.search,
                }),
            )?;
            let rows: Vec<Value> = list
                .as_array()
                .map(|a| a.iter().map(summarize).collect())
                .unwrap_or_default();
            Ok(json!({"count": rows.len(), "sessions": rows}))
        }
        "get_session" => {
            let r = session_ref(args)?;
            let mut s = d.rpc("session.get", json!({"ref": r}))?;
            let id = s["id"].clone();
            s["recent_events"] = d.rpc("session.log", json!({"ref": id, "limit": 15}))?;
            s["subagents"] = d.rpc("session.subagents", json!({"ref": id}))?;
            s["background"] = d.rpc("session.context", json!({"ref": id}))?;
            s["lookup"] = lookup_of(d, id.as_str().unwrap_or(""));
            // The status as list_sessions shows it.
            overlord_status(&mut s);
            // Right after rlord starts, sub-agent data may still be loading.
            if d.warm.is_loading(id.as_str().unwrap_or("")) {
                s["subagents_loading"] = json!(true);
            }
            Ok(s)
        }
        "read_screen" => {
            let s = d.rpc("session.get", json!({"ref": session_ref(args)?}))?;
            let t = s["tmux_session"]
                .as_str()
                .filter(|t| tmux::has_session(t))
                .context("session is not running (no live screen)")?;
            let n = args.get("lines").and_then(Value::as_u64).unwrap_or(60) as usize;
            let text = tmux::capture_pane(t)?;
            let lines: Vec<&str> = text.trim_end().lines().collect();
            let tail = lines[lines.len().saturating_sub(n)..].join("\n");
            Ok(json!({"session": s["name"], "status": status_word(&s), "screen": tail}))
        }
        "get_ui_state" => d.rpc("ui.state", json!({})),
        "goto_session" => d.rpc("ui.goto", json!({"ref": session_ref(args)?})),
        "show_list" => d.rpc("ui.show_list", json!({})),
        "keys_list" => d.rpc("keys.list", json!({})),
        "search_history" => d.rpc("history.search", args.clone()),
        "read_history" => d.rpc("history.read", args.clone()),
        "summarize_sessions" => d.rpc("session.summarize", args.clone()),
        "open_keys" => d.rpc("ui.keys", args.clone()),
        "set_list_view" => {
            let mut p = Map::new();
            for k in [
                "query", "sort", "usage", "archived", "select", "tab", "group",
            ] {
                if let Some(v) = args.get(k).and_then(Value::as_str) {
                    p.insert(k.into(), json!(v));
                }
            }
            for k in ["reverse", "save_as_default", "show"] {
                if let Some(v) = args.get(k).and_then(Value::as_bool) {
                    p.insert(k.into(), json!(v));
                }
            }
            d.rpc("ui.view.set", Value::Object(p))
        }
        "find_repos" => {
            let r = d.rpc("repo.find", json!({}))?;
            Ok(shape_find_repos(&r, arg_str(args, "query").unwrap_or("")))
        }
        "repo_latest" => {
            let r = d.rpc(
                "repo.latest",
                json!({"repo": arg_str(args, "repo").context("missing repo")?,
                       "ref": arg_str(args, "ref"), "prefer": arg_str(args, "prefer")}),
            )?;
            Ok(shape_repo_latest(&r))
        }
        // (Asked before most launches: not the moment to run a command
        // for a model list. The form and `list_agents` do.)
        "get_session_defaults" => d.rpc("session.defaults", json!({"no_listing": true})),
        "recall" => d.rpc(
            "overlord.recall",
            json!({"query": arg_str(args, "query"),
                   "conversation": arg_str(args, "conversation"),
                   "limit": args.get("limit")}),
        ),
        "list_agents" => d.rpc("agents.list", json!({})),
        "propose_session" => d.rpc("ui.propose_session", new_session_params(args)),
        "confirm_setup" => {
            let mut p = Map::new();
            for k in ["code_dir", "repo_roots", "description", "changed", "note"] {
                if let Some(v) = args.get(k) {
                    p.insert(k.into(), v.clone());
                }
            }
            d.rpc("ui.confirm_setup", Value::Object(p))
        }
        "confirm_new_session" => d.rpc("ui.confirm_new_session", confirm_params(args)),
        "create_session" => {
            // Remote Control from launch (the flag), when this machine can
            // have it; otherwise the session launches anyway, with a note.
            let mut params = new_session_params(args);
            // Its background says the overlord launched it.
            params["by_overlord"] = json!(true);
            let mut remote_note = None;
            if args.get("remote_control").and_then(Value::as_bool) == Some(true) {
                let a = d.rpc("remote.availability", json!({"force": true}))?;
                if a["available"] == true {
                    params["remote_control"] = json!(true);
                } else {
                    remote_note = Some(json!({"state": "unavailable", "reason": a["reason"]}));
                }
            }
            let s = d.rpc("session.new", params)?;
            let mut out = summarize(&s);
            out["lookup"] = lookup_of(d, s["id"].as_str().unwrap_or(""));
            if !s["base"].is_null() {
                out["base"] = shape_base(&s["base"]);
            }
            if let Some(n) = remote_note {
                out["remote"] = n;
            } else if params_wants_remote(args) {
                out["remote"] = json!("connects as it starts");
            }
            if args.get("go").and_then(Value::as_bool) == Some(true) {
                // The session exists either way; a failed switch is a note,
                // not a failed launch.
                match d.rpc("ui.goto", json!({"ref": s["id"]})) {
                    Ok(_) => out["user_switched_to_it"] = json!(true),
                    Err(e) => out["switch_failed"] = json!(format!("{e:#}")),
                }
            }
            Ok(out)
        }
        "fork_session" => {
            let mut params = json!({"ref": session_ref(args)?});
            for k in [
                "prompt",
                "context",
                "related",
                "name",
                "description",
                "model",
                "branch",
            ] {
                if let Some(v) = args.get(k) {
                    params[k] = v.clone();
                }
            }
            if args.get("worktree").and_then(Value::as_bool) == Some(true) {
                params["workspace"] = json!(true);
            }
            let s = d.rpc("session.fork", params)?;
            let mut out = summarize(&s);
            out["lookup"] = lookup_of(d, s["id"].as_str().unwrap_or(""));
            out["warnings"] = s["warnings"].clone();
            if args.get("go").and_then(Value::as_bool) == Some(true) {
                // The fork exists either way; a failed switch is a note.
                match d.rpc("ui.goto", json!({"ref": s["id"]})) {
                    Ok(_) => out["user_switched_to_it"] = json!(true),
                    Err(e) => out["switch_failed"] = json!(format!("{e:#}")),
                }
            }
            Ok(out)
        }
        "update_session" => {
            let r = session_ref(args)?;
            let id = d.rpc("session.get", json!({"ref": r}))?["id"].clone();
            // Background checked first (once): a refused one leaves the
            // session as it was. Added last, after the other edits.
            let related = crate::methods::ref_list(args, "add_related");
            let checked = if arg_str(args, "add_context").is_some() || !related.is_empty() {
                let db = d.db();
                let s = db.resolve_ref(id.as_str().unwrap_or(""))?;
                crate::methods::refuse_overlord(&s, "given background")?;
                let (note, links) = crate::context::check(
                    &db,
                    arg_str(args, "add_context"),
                    &related,
                    Some(s.id.as_str()),
                )?;
                Some((note, links))
            } else {
                None
            };
            if let Some(n) = arg_str(args, "name") {
                d.rpc("session.rename", json!({"ref": id, "name": n}))?;
            }
            if let Some(v) = args.get("description").and_then(Value::as_str) {
                d.rpc("session.describe", json!({"ref": id, "description": v}))?;
            }
            if let Some(v) = args.get("notes").and_then(Value::as_str) {
                d.rpc("session.notes", json!({"ref": id, "notes": v}))?;
            }
            let mut delivery = None;
            if let Some((note, links)) = checked {
                // (In-process: the checked values never travel over RPC.)
                let hooks = d.first_background_hooks(id.as_str().unwrap_or(""));
                let r = {
                    let db = d.db();
                    let s = db.resolve_ref(id.as_str().unwrap_or(""))?;
                    crate::methods::context_store(
                        &db,
                        &s,
                        note,
                        &links,
                        &rlord_core::paths::data_dir(),
                        hooks,
                    )?
                };
                d.changed();
                delivery = Some(r["delivery"].clone());
            }
            let add = args.get("add_tags").cloned().unwrap_or(json!([]));
            let remove = args.get("remove_tags").cloned().unwrap_or(json!([]));
            if add.as_array().is_some_and(|a| !a.is_empty())
                || remove.as_array().is_some_and(|a| !a.is_empty())
            {
                d.rpc(
                    "session.tag",
                    json!({"ref": id, "add": add, "remove": remove}),
                )?;
            }
            if let Some(p) = args.get("pinned").and_then(Value::as_bool) {
                d.rpc("session.pin", json!({"ref": id, "pinned": p}))?;
            }
            // An overlord edit is a review: triage stops listing it until it
            // changes again.
            d.rpc("overlord.mark_reviewed", json!({"sessions": [id]}))?;
            let mut out = summarize(&d.rpc("session.get", json!({"ref": id}))?);
            // When the background added reaches the agent.
            if let Some(v) = delivery {
                out["background_delivery"] = v;
            }
            Ok(out)
        }
        "list_groups" => d.rpc("tag.list", json!({})),
        "remote_control" => match (
            arg_str(args, "session"),
            args.get("on").and_then(Value::as_bool),
        ) {
            (Some(r), Some(on)) => d.rpc(
                "remote.set",
                json!({"ref": r, "on": on, "restart": arg_str(args, "restart")}),
            ),
            (r, None) => d.rpc("remote.status", json!({"ref": r})),
            (None, Some(_)) => bail!("which session? (pass `session`)"),
        },
        "set_group_order" => d.rpc(
            "tag.order",
            json!({"order": args.get("order").cloned().unwrap_or(json!([]))}),
        ),
        "rename_group" => d.rpc(
            "tag.rename",
            json!({"from": arg_str(args, "from").context("missing 'from'")?,
                   "to": arg_str(args, "to").context("missing 'to'")?}),
        ),
        "organize" => {
            let changes = args
                .get("changes")
                .and_then(Value::as_array)
                .context("missing 'changes'")?;
            let mut done = Vec::new();
            for c in changes {
                // Each change is an update_session; report per session.
                match call(d, "update_session", c) {
                    Ok(s) => done.push(json!({"session": s["name"], "tags": s["tags"]})),
                    Err(e) => {
                        done.push(json!({"session": c["session"], "error": format!("{e:#}")}))
                    }
                }
            }
            Ok(json!({"results": done}))
        }
        "triage" => {
            let mut p = Map::new();
            if let Some(v) = args.get("all").and_then(Value::as_bool) {
                p.insert("all".into(), json!(v));
            }
            if let Some(v) = args.get("limit").and_then(Value::as_u64) {
                p.insert("limit".into(), json!(v));
            }
            d.rpc("overlord.triage", Value::Object(p))
        }
        "mark_reviewed" => d.rpc(
            "overlord.mark_reviewed",
            json!({"sessions": args.get("sessions").cloned().unwrap_or(json!([])),
                   "all": args.get("all").and_then(Value::as_bool).unwrap_or(false)}),
        ),
        "scan_agent_sessions" => {
            let mut p = Map::new();
            for k in ["since_days", "limit", "query", "ids"] {
                if let Some(v) = args.get(k) {
                    p.insert(k.into(), v.clone());
                }
            }
            let r = d.rpc("session.import_scan", Value::Object(p))?;
            let rows: Vec<Value> = r["candidates"]
                .as_array()
                .map(|a| a.iter().map(summarize_candidate).collect())
                .unwrap_or_default();
            let mut out = json!({"count": rows.len(), "conversations": rows});
            if r["rejected"].as_array().is_some_and(|a| !a.is_empty()) {
                out["not_importable"] = r["rejected"].clone();
            }
            Ok(out)
        }
        "import_sessions" => {
            let items = args
                .get("sessions")
                .and_then(Value::as_array)
                .context("missing 'sessions'")?;
            let mut results = Vec::new();
            for it in items {
                let id = it["id"].as_str().unwrap_or("").trim();
                let mut p = Map::new();
                p.insert("agent_session".into(), json!(id));
                for k in [
                    "name",
                    "description",
                    "notes",
                    "tags",
                    "archive",
                    "recreate_worktree",
                ] {
                    if let Some(v) = it.get(k) {
                        p.insert(k.into(), v.clone());
                    }
                }
                match d.rpc("session.import", Value::Object(p)) {
                    Ok(s) => {
                        // Imported with its meta-context set: already reviewed.
                        // Best effort: the import itself succeeded either way.
                        let _ = d.rpc("overlord.mark_reviewed", json!({"sessions": [s["id"]]}));
                        let mut row = json!({"id": id, "session": s["name"],
                                             "archived": !s["archived_at"].is_null()});
                        if !s["warning"].is_null() {
                            row["warning"] = s["warning"].clone();
                        }
                        results.push(row);
                    }
                    Err(e) => results.push(json!({"id": id, "error": format!("{e:#}")})),
                }
            }
            Ok(json!({"results": results}))
        }
        "send_to_session" => d.rpc(
            "session.send",
            json!({"ref": session_ref(args)?,
                   "text": args.get("text").and_then(Value::as_str).context("missing 'text'")?,
                   "when_idle": args.get("when_idle").and_then(Value::as_bool).unwrap_or(false)}),
        ),
        "resume_after_limit" => {
            let mut p = Map::new();
            for k in ["sessions", "message", "cancel"] {
                if let Some(v) = args.get(k).filter(|v| !v.is_null()) {
                    p.insert(k.into(), v.clone());
                }
            }
            d.rpc("limits.resume", Value::Object(p))
        }
        "session_action" => {
            let r = session_ref(args)?;
            let method = match arg_str(args, "action") {
                Some("restart") => "session.restart",
                Some("stop") => "session.stop",
                Some("archive") => "session.archive",
                Some("unarchive") => "session.unarchive",
                other => bail!("unknown action {other:?}"),
            };
            let s = d.rpc("session.get", json!({"ref": r}))?;
            if crate::overlord::is_overlord_role(s["role"].as_str().unwrap_or(""))
                && method != "session.restart"
            {
                bail!("the overlord can't stop or archive itself");
            }
            d.rpc(method, json!({"ref": s["id"]}))
        }
        other => bail!("unknown tool '{other}'"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A list row says what it was forked from and how many live forks it
    /// has; `get_session` has the whole list.
    #[test]
    fn a_row_counts_its_live_forks() {
        let s = json!({"id": "01p", "name": "api", "status": "idle", "archived_at": null,
                       "forked_from": "01g", "forked_from_name": "base",
                       "fork_count": 2, "live_fork_count": 1});
        let row = summarize(&s);
        assert_eq!(row["live_forks"], 1);
        assert!(row.get("forks").is_none());
        assert_eq!(row["forked_from"], json!({"id": "01g", "name": "base"}));
        assert!(
            summarize(&json!({"id": "x", "fork_count": 1, "live_fork_count": 0}))
                .get("live_forks")
                .is_none()
        );
    }

    /// Every tool, classified. A new tool must be placed here: read-only
    /// only if it changes nothing and starts nothing.
    const MUTATING: &[&str] = &[
        "goto_session",
        "show_list",
        "summarize_sessions",
        "open_keys",
        "set_list_view",
        "get_session_defaults",
        "list_agents",
        "propose_session",
        "confirm_new_session",
        "confirm_setup",
        "create_session",
        "fork_session",
        "update_session",
        "remote_control",
        "set_group_order",
        "rename_group",
        "organize",
        "triage",
        "mark_reviewed",
        "scan_agent_sessions",
        "import_sessions",
        "send_to_session",
        "resume_after_limit",
        "session_action",
        // (Fetches, and moves rlord's tree in the repo.)
        "repo_latest",
    ];

    #[test]
    fn repo_latest_is_shaped_for_the_overlord() {
        let now = rlord_core::types::now_ms();
        let r = json!({
            "repo": "/r", "git": true, "path": "/r/.worktrees/.rlord-latest", "ref": "main",
            "default_branch": true, "remote": "origin", "commit": "abcdef1234567890",
            "subject": "Fix", "committed_at": now - 7_200_000, "source": "remote",
            "fetch": "fetched", "fetched_at": now,
            "moved": {"from": "9f8e7d6c5b4a", "commits": 14},
            "local": {"ahead": 0, "behind": 3, "checked_out_at": "/r", "uncommitted": null},
            "warnings": [],
        });
        let v = shape_repo_latest(&r);
        assert_eq!(v["commit"], "abcdef1");
        assert_eq!(v["committed"], "2h ago");
        assert_eq!(v["fetched"], "0s ago");
        assert_eq!(v["moved"], json!({"from": "9f8e7d6", "commits": 14}));
        assert!(v.get("warnings").is_none() && v.get("committed_at").is_none());
        assert!(v["local"].get("uncommitted").is_none());
        // Diverged: both sides, short.
        let r = json!({"git": true, "ref": "x", "remote": "origin", "diverged": true,
                       "local": {"commit": "1111111111", "ahead": 2},
                       "remote_side": {"commit": "2222222222", "ahead": 5}, "hint": "ask"});
        let v = shape_repo_latest(&r);
        assert_eq!(v["local"]["commit"], "1111111");
        assert_eq!(v["remote_branch"], json!({"commit": "2222222", "ahead": 5}));
        assert_eq!(v["remote"], "origin");
        assert_eq!(
            shape_base(&json!({"commit": "abcdef1234", "label": "origin/main", "warnings": []})),
            json!({"commit": "abcdef1", "label": "origin/main"})
        );
    }

    #[test]
    fn find_repos_reports_skipped_folders_and_no_roots() {
        let skipped: Vec<Value> = (0..12)
            .map(|i| json!({"path": format!("/x/{i}"), "why": "privacy", "text": "macOS asks permission"}))
            .collect();
        let r = json!({
            "repos": [{"path": "/h/ko/a", "default": false, "last_used_at": null},
                      {"path": "/h/b", "default": false, "last_used_at": null}],
            "roots": ["/h"], "skipped": skipped, "skipped_more": 3,
        });
        let out = shape_find_repos(&r, "ko");
        assert_eq!(out["repos"].as_array().unwrap().len(), 1);
        assert_eq!(out["skipped"].as_array().unwrap().len(), 10);
        assert_eq!(out["skipped"][0]["why"], "macOS asks permission");
        assert_eq!(out["skipped_more"], 5);
        assert!(out.get("note").is_none());
        let out = shape_find_repos(&json!({"repos": [], "roots": []}), "");
        assert!(out["note"].as_str().unwrap().contains("no code folder"));
        assert!(out.get("skipped").is_none());
    }

    /// `prefer` without a `base` goes nowhere (create, propose, confirm).
    #[test]
    fn prefer_goes_only_with_a_base() {
        let a = json!({"prompt": "x", "prefer": "local"});
        assert!(new_session_params(&a).get("prefer").is_none());
        assert!(
            confirm_params(&json!({"description": "d", "prefer": "local"}))
                .get("prefer")
                .is_none()
        );
        let b = json!({"prompt": "x", "base": "feat", "prefer": "local"});
        assert_eq!(new_session_params(&b)["prefer"], "local");
    }

    /// `confirm_new_session` passes `base` and `prefer` on to the form,
    /// and nothing it doesn't know.
    #[test]
    fn confirm_new_session_forwards_base_and_prefer() {
        let p = confirm_params(&json!({
            "description": "d", "base": "feat", "prefer": "local",
            "changed": ["base"], "note": "n", "extra_args": ["x"],
        }));
        assert_eq!(
            p,
            json!({"description": "d", "base": "feat", "prefer": "local",
                   "changed": ["base"], "note": "n"})
        );
    }

    #[test]
    fn new_session_fields_are_whitelisted() {
        let p = new_session_params(&json!({
            "prompt": "fix it", "context": "repo is /r", "related": ["auth", 3, " "],
            "extra_args": ["--dangerously-skip-permissions"], "role": "overlord",
            "tmux_name": "x", "by_overlord": false,
            // (`base_resolved` only from the daemon.)
            "base": "feat", "base_resolved": {"commit": "x"},
            "location_resolved": {"repo": "/x", "project": "/x"},
        }));
        assert_eq!(
            p,
            json!({"prompt": "fix it", "context": "repo is /r", "related": ["auth"], "base": "feat"})
        );
    }

    #[test]
    fn read_only_tools_are_an_explicit_allow_list() {
        let all: Vec<String> = tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for name in &all {
            let ro = READ_ONLY_TOOLS.contains(&name.as_str());
            let rw = MUTATING.contains(&name.as_str());
            assert!(
                ro != rw,
                "{name}: classify it in READ_ONLY_TOOLS or MUTATING"
            );
        }
        for name in READ_ONLY_TOOLS.iter().chain(MUTATING) {
            assert!(all.iter().any(|n| n == name), "{name} is no tool");
        }
        for t in rlord_core::agents::SEND_TOOLS {
            assert!(!READ_ONLY_TOOLS.contains(t));
        }
        let served = tools_for(false);
        assert_eq!(served.as_array().unwrap().len(), READ_ONLY_TOOLS.len());
        assert_eq!(tools_for(true), tools());
    }
}
