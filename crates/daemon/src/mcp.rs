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

pub fn tools() -> Value {
    let sort = json!({"type": "string", "enum": ["updated", "user", "state", "created", "name", "usage"],
                      "description": "updated = last message (from the user or the agent; a resume or status flip alone does not count); user = the user's last prompt; state = last status change (started, stopped, finished, needs you); shown to the user as last message / last prompt / status change; usage = most used first, by the usage measure (see `usage`)"});
    let usage = json!({"type": "string", "enum": ["turns", "tokens", "context"],
                       "description": "The usage measure: turns (prompts the user typed), tokens (taken in new or written), context (% of the context window in use). Default: the user's configured one."});
    let new_session_props = json!({
        "repo": s("Git repo path to work in (see find_repos). Omit to use the configured default repo."),
        "project": s("Plain folder instead of a repo (no worktree)"),
        "name": s("Short display name; also derives the branch"),
        "branch": s("Branch / worktree name (default: derived from name)"),
        "agent": s("Coding agent to run: claude, codex or opencode (default: the configured session agent). Only when the user asks for one."),
        "model": s("Model, one of that agent's (default: the configured model for the agent; see get_session_defaults)"),
        "workspace": b("Give the session its own git worktree (default: configured, normally true)"),
        "prompt": s("First prompt for the agent: the full task, it has no other context"),
    });
    let mut create_props = new_session_props.clone();
    create_props["go"] = b("Switch the user's terminal to the new session (default false)");
    create_props["remote_control"] = b("Turn on Remote Control (continue it from the Claude app) once launched; only when the user asks. Claude Code sessions only.");
    json!([
        tool("list_sessions",
            "List agent sessions with their meta-context (status, model, repo/branch, tags, description, recent activity). Status is one of: working; needs you (blocked mid-turn on the user: a permission prompt or a question); your turn (its turn is over, the next prompt is the user's); stopped; starting; error; limited (the agent stopped on a usage limit: `limited_until` says when it resets, `resume_scheduled` whether resume_after_limit will pick it up). The overlord itself is not listed.",
            json!({
                "query": s("Search: free words match name/project/tag; status:needs (needs you) status:turn (your turn) status:working tag:x proj:x agent:x filter"),
                "archived": {"type": "string", "enum": ["live", "only", "all"], "description": "default live"},
                "sort": sort.clone(),
                "usage": usage,
                "reverse": b("Reverse the sort"),
            }), &[]),
        tool("get_session",
            "Everything about one session: details, notes, recent events, sub-agents.",
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
            "Find git repos to launch sessions in: known repos (most recent first, default marked) plus repos discovered on disk.",
            json!({"query": s("Words that must appear in the path")}), &[]),
        tool("get_session_defaults",
            "The defaults new sessions get (agent, model, model choices, workspace, default repo), and under `agents` each agent rlord can run: whether it's installed, its default model and its model choices.",
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
                "changed": {"type": "array", "items": {"type": "string", "enum": ["repo", "agent", "model", "worktree", "tags"]},
                            "description": "Hints you changed from what the user sent"},
                "note": s("One line for the top of the form, e.g. \"Updated to match your description: repo is myapp\""),
            }), &["description", "changed", "note"]),
        tool("create_session",
            "Create and launch a new agent session immediately.",
            create_props, &[]),
        tool("update_session",
            "Edit a session's meta-context. Only the fields given change.",
            json!({
                "session": s(SESSION_REF),
                "name": s("New name"),
                "description": s("One-line description"),
                "notes": s("Replace the free-form notes"),
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
            "Find agent conversations on this machine that rlord doesn't track yet (Claude Code's so far; read from its own transcript files; no AI), most recently active first, for importing existing work. Each has its folder (and whether it still exists or can be recreated), repo/branch, title, first/latest prompt, latest reply, PR link, when it started and was last active, and whether it may be open elsewhere right now.",
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
            "Pick sessions up when their usage limit resets. A session the agent stopped on a usage limit shows `status: limited` with `limited_until`. This schedules one resume per session: once its limit has reset (plus a minute) the session is sent the message, typed in as send_to_session types, unless it was used since the limit hit (the user typed, or it answered): then it is left alone. If the limit hits again the resume waits for the next reset. Scheduled resumes survive an rlord restart. Only when the user asks; nothing is resumed automatically. Without `sessions`: every session limited right now.",
            json!({
                "sessions": {"type": "array", "items": {"type": "string"},
                             "description": "Sessions (id, id prefix or name). Omit for every limited session."},
                "message": s("What to send (default: the user's configured resume message, \"continue\")"),
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

/// The compact row the overlord sees per session.
fn summarize(s: &Value) -> Value {
    let ts = |k: &str| s[k].as_i64().map(|t| rel_age(Some(t)));
    let mut row = json!({
        "id": s["id"], "name": s["name"],
        // As people see it: working, needs you, your turn, stopped.
        "status": rlord_core::types::Status::label_of(s["status"].as_str().unwrap_or("")),
        "agent": s["agent"], "model": s["model"],
        "repo": s["repo_path"], "dir": s["project_path"], "branch": s["branch"],
        "tags": s["tags"], "description": s["description"],
        "pinned": s["pinned"], "archived": !s["archived_at"].is_null(),
        "last_agent_msg": ts("last_agent_msg_at"), "last_user_msg": ts("last_user_msg_at"),
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
    // Stopped on a usage limit: that, rather than the status under it.
    if !s["limit"].is_null() {
        row["status"] = json!("limited");
        row["limited_until"] = s["limit"]["until"].clone();
        row["resume_scheduled"] = s["limit"]["resume"].clone();
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
    row
}

fn params_wants_remote(args: &Value) -> bool {
    args.get("remote_control").and_then(Value::as_bool) == Some(true)
}

/// Copy whitelisted session-creation fields.
fn new_session_params(args: &Value) -> Value {
    let mut p = Map::new();
    for k in [
        "repo", "project", "name", "branch", "agent", "model", "prompt",
    ] {
        if let Some(v) = arg_str(args, k) {
            p.insert(k.into(), json!(v));
        }
    }
    if let Some(w) = args.get("workspace").and_then(Value::as_bool) {
        p.insert("workspace".into(), json!(w));
    }
    Value::Object(p)
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
            Ok(json!({"session": s["name"], "status": s["status"], "screen": tail}))
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
            let words: Vec<String> = arg_str(args, "query")
                .unwrap_or("")
                .split_whitespace()
                .map(str::to_lowercase)
                .collect();
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
            Ok(json!({"repos": repos}))
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
        "confirm_new_session" => {
            let mut p = Map::new();
            for k in [
                "description",
                "request_id",
                "repo",
                "project",
                "agent",
                "model",
                "workspace",
                "tags",
                "changed",
                "note",
            ] {
                if let Some(v) = args.get(k) {
                    p.insert(k.into(), v.clone());
                }
            }
            d.rpc("ui.confirm_new_session", Value::Object(p))
        }
        "create_session" => {
            // Remote Control from launch (the flag), when this machine can
            // have it; otherwise the session launches anyway, with a note.
            let mut params = new_session_params(args);
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
        "update_session" => {
            let r = session_ref(args)?;
            let id = d.rpc("session.get", json!({"ref": r}))?["id"].clone();
            if let Some(n) = arg_str(args, "name") {
                d.rpc("session.rename", json!({"ref": id, "name": n}))?;
            }
            if let Some(v) = args.get("description").and_then(Value::as_str) {
                d.rpc("session.describe", json!({"ref": id, "description": v}))?;
            }
            if let Some(v) = args.get("notes").and_then(Value::as_str) {
                d.rpc("session.notes", json!({"ref": id, "notes": v}))?;
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
            Ok(summarize(&d.rpc("session.get", json!({"ref": id}))?))
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
            for k in ["sessions", "message"] {
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
