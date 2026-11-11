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
    let sort = json!({"type": "string", "enum": ["updated", "user", "state", "created", "name"],
                      "description": "updated = latest message either way or status change; user = latest user prompt; state = latest status change"});
    let new_session_props = json!({
        "repo": s("Git repo path to work in (see find_repos). Omit to use the configured default repo."),
        "project": s("Plain folder instead of a repo (no worktree)"),
        "name": s("Short display name; also derives the branch"),
        "branch": s("Branch / worktree name (default: derived from name)"),
        "model": s("Model (default: the configured session model)"),
        "workspace": b("Give the session its own git worktree (default: configured, normally true)"),
        "prompt": s("First prompt for the agent: the full task, it has no other context"),
    });
    let mut create_props = new_session_props.clone();
    create_props["go"] = b("Switch the user's terminal to the new session (default false)");
    json!([
        tool("list_sessions",
            "List agent sessions with their meta-context (status, model, repo/branch, tags, description, recent activity). The overlord itself is not listed.",
            json!({
                "query": s("Search: free words match name/project/tag; status:waiting tag:x proj:x agent:x filter"),
                "archived": {"type": "string", "enum": ["live", "only", "all"], "description": "default live"},
                "sort": sort.clone(),
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
        tool("set_list_view",
            "Set the session list's tab, filter and sort, optionally save it as the default view, and show it to the user. The list has two tabs: 'sessions' (flat) and 'groups' (sessions by tag: tags are the groups).",
            json!({
                "tab": {"type": "string", "enum": ["sessions", "groups"], "description": "Which tab to show"},
                "group": s("On the groups tab, the group (tag) to open; \"\" = untagged. Implies tab groups."),
                "query": s("Filter text (same syntax as list_sessions query); \"\" clears it"),
                "sort": sort,
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
            "The defaults new sessions get (agent, model, model choices, workspace, default repo).",
            json!({}), &[]),
        tool("hand_off",
            "Overlord (front desk) only: give a request to the mastermind (the overlord's deeper session) and switch the user to it: launches and anything needing reasoning, research or a judgment call. The mastermind doesn't use this.",
            json!({
                "request": s("The user's words, verbatim"),
                "context": s("What the mastermind needs from your conversation (it sees none of it): earlier messages, sessions mentioned, what you already found"),
            }), &["request"]),
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
                "model": s("Model as you would launch it (\"\" for the agent default)"),
                "workspace": b("Own git worktree"),
                "tags": {"type": "array", "items": {"type": "string"}},
                "changed": {"type": "array", "items": {"type": "string", "enum": ["repo", "model", "worktree", "tags"]},
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
            "Find agent conversations on this machine that rlord doesn't track yet (read from the agents' own transcript files; no AI), most recently active first, for importing existing work. Each has its folder (and whether it still exists or can be recreated), repo/branch, title, first/latest prompt, latest reply, PR link, when it started and was last active, and whether it may be open elsewhere right now.",
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
    json!({
        "id": s["id"], "name": s["name"], "status": s["status"], "model": s["model"],
        "repo": s["repo_path"], "dir": s["project_path"], "branch": s["branch"],
        "tags": s["tags"], "description": s["description"],
        "pinned": s["pinned"], "archived": !s["archived_at"].is_null(),
        "last_agent_msg": ts("last_agent_msg_at"), "last_user_msg": ts("last_user_msg_at"),
        "subagents_running": s["agents_running"],
    })
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

/// Copy whitelisted session-creation fields.
fn new_session_params(args: &Value) -> Value {
    let mut p = Map::new();
    for k in ["repo", "project", "name", "branch", "model", "prompt"] {
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
        "set_list_view" => {
            let mut p = Map::new();
            for k in ["query", "sort", "archived", "select", "tab", "group"] {
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
        "get_session_defaults" => d.rpc("session.defaults", json!({})),
        "hand_off" => d.rpc(
            "overlord.hand_off",
            json!({"request": arg_str(args, "request").context("missing 'request'")?,
                   "context": arg_str(args, "context").unwrap_or("")}),
        ),
        "propose_session" => d.rpc("ui.propose_session", new_session_params(args)),
        "confirm_new_session" => {
            let mut p = Map::new();
            for k in [
                "description",
                "request_id",
                "repo",
                "project",
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
            let s = d.rpc("session.new", new_session_params(args))?;
            let mut out = summarize(&s);
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
