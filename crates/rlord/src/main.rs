use std::io::Read;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use rlord_core::config::Config;
use rlord_core::timefmt::{fmt_ts, rel_age};
use rlord_core::{agents::claude, paths, tmux};
use rlord_daemon::{client_call, methods, spool_hook_event, Daemon};

#[derive(Parser)]
#[command(
    name = "rlord",
    version,
    about = "Robot Overlord — session management for AI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Interactive session dashboard (the default when no subcommand is given)
    Tui,
    /// Run the daemon in the foreground (use the systemd unit for real deployments)
    Daemon,
    /// Check daemon liveness
    Ping,
    /// List sessions. QUERY uses the search syntax: free words match
    /// name/project/group/tag; `status:waiting` `tag:wip` `proj:x` `agent:x`
    /// `group:x` `sort:user` set filters.
    Ls {
        query: Vec<String>,
        /// Show only archived sessions
        #[arg(long, conflicts_with = "all")]
        archived: bool,
        /// Show live and archived sessions
        #[arg(long)]
        all: bool,
        /// Filter by status (comma-separated: working,idle,waiting_input,stopped,error,starting)
        #[arg(long, value_delimiter = ',')]
        status: Vec<String>,
        /// Filter by tag
        #[arg(long)]
        tag: Option<String>,
        /// Filter by project path substring
        #[arg(long)]
        project: Option<String>,
        /// Filter by name substring
        #[arg(long)]
        name: Option<String>,
        /// Sort: activity (default), user, state, created, name
        #[arg(long, default_value = "activity")]
        sort: String,
        /// Reverse the sort order
        #[arg(long)]
        reverse: bool,
        /// Raw JSON output
        #[arg(long)]
        json: bool,
    },
    /// Create and launch a new agent session
    New {
        /// First prompt to hand the agent (optional)
        prompt: Vec<String>,
        #[arg(long, default_value = "claude")]
        agent: String,
        /// Project directory (default: current directory)
        #[arg(long)]
        project: Option<String>,
        /// Display name (default: derived from the project dir)
        #[arg(long)]
        name: Option<String>,
        /// Attach immediately after launch
        #[arg(long, short = 'a')]
        attach: bool,
    },
    /// Attach the terminal to a session (ref = id, id prefix, or name)
    Attach {
        r#ref: String,
    },
    /// Restart a session, resuming its conversation
    Restart {
        r#ref: String,
    },
    /// Stop a session (keeps it resumable)
    Stop {
        r#ref: String,
    },
    /// Archive a session (stops it and hides it from the default list)
    Archive {
        r#ref: String,
    },
    /// Bring an archived session back to the live list (stopped, resumable)
    Unarchive {
        r#ref: String,
    },
    /// Rename a session (locks the name against agent-side renames)
    Rename {
        r#ref: String,
        name: String,
    },
    /// Add tags to a session
    Tag {
        r#ref: String,
        tags: Vec<String>,
    },
    /// Remove tags from a session
    Untag {
        r#ref: String,
        tags: Vec<String>,
    },
    /// Pin a session to the top of every list
    Pin {
        r#ref: String,
    },
    Unpin {
        r#ref: String,
    },
    /// Set a session's group path (e.g. `work/backend`)
    Group {
        r#ref: String,
        group: String,
    },
    /// Show a session's event log
    Log {
        r#ref: String,
        #[arg(long, short = 'n', default_value = "50")]
        limit: i64,
    },
    /// Configuration file management
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Manage the tmux navigation key bindings rlord installs
    #[command(subcommand)]
    Tmux(TmuxCmd),
    /// Navigation target of the tmux bindings (invoked by tmux run-shell)
    #[command(subcommand, hide = true)]
    Nav(NavCmd),
    /// Plumbing invoked by agent hook systems (fast path; never fails loudly)
    #[command(subcommand, hide = true)]
    Hook(HookCmd),
    /// Claude Code integration management
    #[command(subcommand)]
    Claude(ClaudeCmd),
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Write a commented default config (refuses to overwrite an existing one)
    Init,
    /// Print the config file path
    Path,
    /// Print the effective configuration
    Show,
}

#[derive(Subcommand)]
enum TmuxCmd {
    /// (Re)install the navigation bindings on the current tmux server
    Bind,
    /// Remove the navigation bindings
    Unbind,
}

#[derive(Subcommand)]
enum NavCmd {
    /// Back to the rlord TUI, or detach when attached from outside tmux
    Home,
    Next,
    Prev,
}

#[derive(Subcommand)]
enum HookCmd {
    /// Forward a hook payload from stdin to the daemon
    Emit { agent: String },
}

#[derive(Subcommand)]
enum ClaudeCmd {
    /// Install rlord's hook handler into Claude Code's settings.json
    InstallHooks,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command.unwrap_or(Command::Tui) {
        Command::Tui => {
            let cfg = Config::load()?;
            if let Ok(conflicts) = tmux::bind_nav_keys(&methods::rlord_bin(), &cfg.tmux.bindings())
            {
                for c in conflicts {
                    eprintln!(
                        "rlord: nav key {} skipped (already bound to: {})",
                        c.key, c.bound_to
                    );
                }
            }
            rlord_tui::run(cfg)
        }
        Command::Config(ConfigCmd::Init) => {
            let path = paths::config_path();
            if path.exists() {
                anyhow::bail!("{} already exists", path.display());
            }
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, Config::default_toml())?;
            println!("wrote {}", path.display());
            Ok(())
        }
        Command::Config(ConfigCmd::Path) => {
            println!("{}", paths::config_path().display());
            Ok(())
        }
        Command::Config(ConfigCmd::Show) => {
            print!("{}", toml::to_string_pretty(&Config::load()?)?);
            Ok(())
        }
        Command::Tmux(TmuxCmd::Bind) => {
            let cfg = Config::load()?;
            let b = cfg.tmux.bindings();
            let conflicts = tmux::bind_nav_keys(&methods::rlord_bin(), &b)?;
            for (k, v) in &b {
                match conflicts.iter().find(|c| c.key == *k) {
                    Some(c) => println!("{k:<6} SKIPPED — already bound to: {}", c.bound_to),
                    None => println!("{k:<6} → rlord nav {v}"),
                }
            }
            if !conflicts.is_empty() {
                println!(
                    "edit [tmux] in {} to pick other keys",
                    paths::config_path().display()
                );
            }
            Ok(())
        }
        Command::Tmux(TmuxCmd::Unbind) => {
            let cfg = Config::load()?;
            tmux::unbind_nav_keys(&cfg.tmux.bindings())?;
            println!("navigation bindings removed");
            Ok(())
        }
        Command::Nav(NavCmd::Home) => nav_home(),
        Command::Nav(NavCmd::Next) => nav_step(1),
        Command::Nav(NavCmd::Prev) => nav_step(-1),
        Command::Daemon => run_daemon(),
        Command::Ping => {
            let r = call("ping", Value::Null)?;
            println!(
                "pong (daemon v{})",
                r.get("version").and_then(Value::as_str).unwrap_or("?")
            );
            Ok(())
        }
        Command::Ls {
            query,
            archived,
            all,
            status,
            tag,
            project,
            name,
            sort,
            reverse,
            json,
        } => {
            let archived_mode = if archived {
                "only"
            } else if all {
                "all"
            } else {
                "live"
            };
            let mut q = rlord_core::db::SessionQuery {
                status: status
                    .iter()
                    .map(|s| rlord_core::db::normalize_status(s))
                    .collect(),
                tag,
                project,
                name,
                sort: rlord_core::db::SortMode::parse(&sort)
                    .with_context(|| format!("unknown sort '{sort}'"))?,
                reverse,
                ..Default::default()
            };
            q.apply_search_text(&query.join(" "));
            let r = call(
                "session.list",
                json!({
                    "archived": archived_mode, "status": q.status, "tag": q.tag,
                    "agent": q.agent, "project": q.project, "name": q.name,
                    "group": q.group, "search": q.search, "sort": q.sort.label(),
                    "reverse": q.reverse,
                }),
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print_session_table(&r);
            }
            Ok(())
        }
        Command::New {
            prompt,
            agent,
            project,
            name,
            attach,
        } => {
            let project = match project {
                Some(p) => p,
                None => std::env::current_dir()?.to_string_lossy().into_owned(),
            };
            let prompt = if prompt.is_empty() {
                None
            } else {
                Some(prompt.join(" "))
            };
            let r = call(
                "session.new",
                json!({"agent": agent, "project": project, "name": name, "prompt": prompt}),
            )?;
            let id = r["id"].as_str().unwrap_or("?");
            println!("launched {} ({})", r["name"].as_str().unwrap_or("?"), id);
            if attach {
                attach_tmux(r["tmux_session"].as_str().context("no tmux session")?)?;
            }
            Ok(())
        }
        Command::Attach { r#ref } => {
            let s = call("session.get", json!({"ref": r#ref}))?;
            let tmux_name = s["tmux_session"]
                .as_str()
                .context("session has no tmux session")?;
            if s["status"] == "stopped" {
                anyhow::bail!(
                    "session is stopped; `rlord restart {}` first",
                    s["id"].as_str().unwrap_or("?")
                );
            }
            attach_tmux(tmux_name)
        }
        Command::Restart { r#ref } => {
            let r = call("session.restart", json!({"ref": r#ref}))?;
            println!(
                "restarted {} ({}) — status {}",
                r["name"].as_str().unwrap_or("?"),
                r["id"].as_str().unwrap_or("?"),
                r["status"].as_str().unwrap_or("?")
            );
            Ok(())
        }
        Command::Stop { r#ref } => {
            let r = call("session.stop", json!({"ref": r#ref}))?;
            println!("stopped {}", r["stopped"].as_str().unwrap_or("?"));
            Ok(())
        }
        Command::Archive { r#ref } => {
            let r = call("session.archive", json!({"ref": r#ref}))?;
            println!("archived {}", r["archived"].as_str().unwrap_or("?"));
            Ok(())
        }
        Command::Unarchive { r#ref } => {
            let r = call("session.unarchive", json!({"ref": r#ref}))?;
            println!("unarchived {}", r["unarchived"].as_str().unwrap_or("?"));
            Ok(())
        }
        Command::Rename { r#ref, name } => {
            let r = call("session.rename", json!({"ref": r#ref, "name": name}))?;
            println!(
                "renamed {} → {}",
                r["renamed"].as_str().unwrap_or("?"),
                name
            );
            Ok(())
        }
        Command::Tag { r#ref, tags } => {
            let r = call("session.tag", json!({"ref": r#ref, "add": tags}))?;
            println!("tags: {}", tags_of(&r));
            Ok(())
        }
        Command::Untag { r#ref, tags } => {
            let r = call("session.tag", json!({"ref": r#ref, "remove": tags}))?;
            println!("tags: {}", tags_of(&r));
            Ok(())
        }
        Command::Pin { r#ref } => {
            call("session.pin", json!({"ref": r#ref, "pinned": true}))?;
            println!("pinned");
            Ok(())
        }
        Command::Unpin { r#ref } => {
            call("session.pin", json!({"ref": r#ref, "pinned": false}))?;
            println!("unpinned");
            Ok(())
        }
        Command::Group { r#ref, group } => {
            call("session.group", json!({"ref": r#ref, "group": group}))?;
            println!("group set to {group}");
            Ok(())
        }
        Command::Log { r#ref, limit } => {
            let r = call("session.log", json!({"ref": r#ref, "limit": limit}))?;
            for e in r.as_array().map(|v| v.as_slice()).unwrap_or_default() {
                println!(
                    "{}  {:<22} {}",
                    fmt_ts(e["ts"].as_i64().unwrap_or(0)),
                    e["kind"].as_str().unwrap_or("?"),
                    compact(&e["payload"])
                );
            }
            Ok(())
        }
        Command::Hook(HookCmd::Emit { agent }) => hook_emit(&agent),
        Command::Claude(ClaudeCmd::InstallHooks) => {
            let settings = claude::config_dir().join("settings.json");
            let changed = claude::install_hooks(&settings, &methods::hook_command())?;
            println!(
                "{} ({})",
                if changed {
                    "hooks installed"
                } else {
                    "hooks already installed"
                },
                settings.display()
            );
            Ok(())
        }
    }
}

fn call(method: &str, params: Value) -> Result<Value> {
    client_call(&paths::socket_path(), method, params)
}

fn tags_of(session: &Value) -> String {
    let t: Vec<&str> = session["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if t.is_empty() {
        "(none)".to_string()
    } else {
        t.join(", ")
    }
}

fn run_daemon() -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let db = rlord_core::db::Db::open(&paths::db_path())?;
        let daemon = Daemon::new(db, Config::load()?);
        daemon.startup()?;
        let listener = Daemon::bind(&paths::socket_path())?;
        let pidfile = paths::pid_path();
        std::fs::write(&pidfile, std::process::id().to_string())?;
        tracing::info!(socket = %paths::socket_path().display(), db = %paths::db_path().display(), pid = std::process::id(), "rlordd starting");
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let r = tokio::select! {
            r = daemon.serve(listener) => r,
            _ = tokio::signal::ctrl_c() => Ok(()),
            _ = term.recv() => Ok(()),
        };
        tracing::info!("shutting down");
        let _ = std::fs::remove_file(&pidfile);
        r
    })
}

/// Fast path for agent hooks: read payload from stdin, forward to the daemon,
/// spool on failure. Exits 0 in all cases — a hook must never break the agent.
fn hook_emit(agent: &str) -> Result<()> {
    let Ok(instance) = std::env::var("RLORD_SESSION_ID") else {
        return Ok(()); // not an rlord-managed session
    };
    let mut raw = String::new();
    std::io::stdin().take(1 << 20).read_to_string(&mut raw).ok();
    let payload: Value = serde_json::from_str(&raw).unwrap_or(json!({}));
    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let params = json!({"instance": instance, "agent": agent, "event": event, "payload": payload});
    if client_call(&paths::socket_path(), "hook.emit", params.clone()).is_err() {
        spool_hook_event(&params).ok();
    }
    Ok(())
}

/// `nav home`: switch this client to the TUI's session, or detach when the
/// TUI isn't inside tmux (no home recorded / home gone).
fn nav_home() -> Result<()> {
    match tmux::home() {
        Some(h) if tmux::has_session(&h) => tmux::switch_client(&h),
        _ => tmux::detach_client(),
    }
}

/// `nav next|prev`: move to the neighbouring live session in list order.
fn nav_step(dir: i64) -> Result<()> {
    let list = call("session.list", json!({"archived": "live"}))?;
    let live: Vec<String> = list
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|s| s["status"] != "stopped")
                .filter_map(|s| s["tmux_session"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if live.is_empty() {
        return Ok(());
    }
    let cur = tmux::current_session().unwrap_or_default();
    let target = match live.iter().position(|s| *s == cur) {
        Some(i) => &live[(i as i64 + dir).rem_euclid(live.len() as i64) as usize],
        None => &live[0],
    };
    tmux::switch_client(target)
}

fn attach_tmux(tmux_name: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let target = format!("={tmux_name}");
    // Inside tmux already → switch the client instead of nesting.
    let err = if std::env::var_os("TMUX").is_some() {
        std::process::Command::new("tmux")
            .args(["switch-client", "-t", &target])
            .exec()
    } else {
        std::process::Command::new("tmux")
            .args(["attach-session", "-t", &target])
            .exec()
    };
    Err(err).context("exec tmux failed")
}

fn compact(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Object(m) if m.is_empty() => String::new(),
        other => other.to_string(),
    }
}

fn print_session_table(list: &Value) {
    let Some(rows) = list.as_array() else {
        println!("{list}");
        return;
    };
    if rows.is_empty() {
        println!("no sessions");
        return;
    }
    println!(
        "{:<8} {:<24} {:<13} {:>6} {:>6}  {:<16} PROJECT",
        "ID", "NAME", "STATUS", "AGENT", "USER", "TAGS"
    );
    for s in rows {
        let get = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("-");
        let id = get("id");
        let pin = if s["pinned"].as_bool().unwrap_or(false) {
            "*"
        } else {
            " "
        };
        println!(
            "{:<8}{}{:<24} {:<13} {:>6} {:>6}  {:<16} {}",
            &id[..id.len().min(6)],
            pin,
            truncate(get("name"), 23),
            get("status"),
            rel_age(s["last_agent_msg_at"].as_i64()),
            rel_age(s["last_user_msg_at"].as_i64()),
            truncate(&tags_of(s).replace("(none)", ""), 16),
            get("project_path")
        );
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}
