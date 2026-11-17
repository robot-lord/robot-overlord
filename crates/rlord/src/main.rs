use std::io::Read;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use rlord_core::agents::{transcript, Agent};
use rlord_core::config::Config;
use rlord_core::timefmt::{fmt_ts, rel_age};
use rlord_core::{paths, tmux};
use rlord_daemon::{autostart, client_call, methods, spool_hook_event, Daemon};

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
    /// Run the daemon in the foreground (normally started on demand, or by launchd)
    Daemon,
    /// Check daemon liveness (never starts one)
    Ping,
    /// Search what sessions said and did: summaries, descriptions, notes and
    /// transcripts (archived sessions too). Every word must appear.
    Search {
        #[arg(required = true)]
        words: Vec<String>,
        /// Also agent conversations (Claude Code, Codex, OpenCode) started outside rlord
        #[arg(long)]
        everywhere: bool,
        /// Sessions to show
        #[arg(long, default_value_t = 8)]
        limit: u64,
        #[arg(long)]
        json: bool,
    },
    /// Read a session's conversation (around a `rlord search` hit's line, or
    /// its last messages): text only, tool output clipped.
    History {
        /// Session (id, prefix or name), or an outside transcript's path
        session: String,
        /// The hit's line (`rlord search` shows it as `:<line>`)
        #[arg(long)]
        around: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// List sessions. QUERY uses the search syntax: free words match
    /// name/project/group/tag; `status:needs` (needs you) `status:turn` (your
    /// turn) `tag:wip` `proj:x` `agent:x`
    /// `group:x` `sort:user` set filters.
    Ls {
        query: Vec<String>,
        /// Show only archived sessions
        #[arg(long, conflicts_with = "all")]
        archived: bool,
        /// Show live and archived sessions
        #[arg(long)]
        all: bool,
        /// Filter by status (comma-separated: working, needs-you, your-turn,
        /// stopped, error, starting; the stored names waiting_input and idle
        /// work too)
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
        /// Sort: activity (default), user, state, created, name, usage
        #[arg(long, default_value = "updated")]
        sort: String,
        /// Reverse the sort order
        #[arg(long)]
        reverse: bool,
        /// What the USAGE column shows (and `--sort usage` sorts by): turns,
        /// tokens or context (%); default: the list's (the measure last
        /// picked there, else `usage` in config.toml)
        #[arg(long)]
        usage: Option<String>,
        /// Raw JSON output
        #[arg(long)]
        json: bool,
    },
    /// Create and launch a new agent session (defaults from [session] in config)
    New {
        /// First prompt to hand the agent (optional)
        prompt: Vec<String>,
        /// Git repo to work in (default: the repo containing the current
        /// directory, else the configured default_repo)
        #[arg(long, conflicts_with = "project")]
        repo: Option<String>,
        /// Plain folder, no repo/workspace
        #[arg(long)]
        project: Option<String>,
        /// Don't create a dedicated worktree (work directly in the repo)
        #[arg(long)]
        no_workspace: bool,
        /// Branch / workspace name (default: derived from the session name)
        #[arg(long)]
        branch: Option<String>,
        /// Model passed to the agent (default: [session].model, or
        /// codex_model for Codex, opencode_model for OpenCode: provider/model)
        #[arg(long)]
        model: Option<String>,
        /// Coding agent to run: claude, codex or opencode (default: [session].agent)
        #[arg(long)]
        agent: Option<String>,
        /// Display name (default: <repo>-<id suffix>)
        #[arg(long)]
        name: Option<String>,
        /// Attach immediately after launch
        #[arg(long, short = 'a')]
        attach: bool,
    },
    /// Fork a session: a new session that goes on from a copy of its whole
    /// conversation, on the same agent, in the same folder (or a new
    /// worktree). The original is left as it is
    Fork {
        /// The session to fork (id, id prefix, name or name prefix)
        r#ref: String,
        /// What the fork should do first (optional: it opens idle)
        prompt: Vec<String>,
        /// Display name (default: <original>-fork)
        #[arg(long)]
        name: Option<String>,
        /// Another model than the original's (default: the original's)
        #[arg(long)]
        model: Option<String>,
        /// Work in a new worktree branched from the original's last commit
        /// (Claude Code and Codex; uncommitted changes don't come along)
        #[arg(long)]
        worktree: bool,
        /// The new worktree's branch (default: derived from the name)
        #[arg(long, requires = "worktree")]
        branch: Option<String>,
        /// Attach immediately after it starts
        #[arg(long, short = 'a')]
        attach: bool,
        #[arg(long)]
        json: bool,
    },
    /// List git repos rlord has used, most recent first
    Repos,
    /// The latest committed code of a repo, in rlord's read-only tree
    /// (`<repo>/.worktrees/.rlord-latest`): fetches the branch, moves the
    /// tree to its newest commit, prints the path and commit. Anything
    /// edited in the tree is thrown away when it moves.
    Latest {
        /// The repo, or any folder in it (default: the current folder)
        repo: Option<String>,
        /// A branch (default: the default branch); <remote>/<branch> for the remote's
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// After a "diverged" answer: the side to use (local or remote)
        #[arg(long)]
        prefer: Option<String>,
        /// List the trees rlord keeps
        #[arg(long, conflicts_with_all = ["git_ref", "prefer", "prune"])]
        list: bool,
        /// Remove the tree and rlord's private refs of REPO (default: the
        /// current folder's repo)
        #[arg(long, conflicts_with_all = ["git_ref", "prefer"])]
        prune: bool,
        /// With --prune: every repo rlord keeps a tree or refs for
        #[arg(long, requires = "prune", conflicts_with = "repo")]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Set your code folder: where your repos live and new ones go (the
    /// repo finder scans it). Without PATH, rlord suggests one from the
    /// projects it has seen and asks.
    Setup {
        /// The folder (created if missing, except one macOS guards, which
        /// is saved unchecked); no prompts
        path: Option<String>,
        /// Leave `[session] repo_roots` as it is (by default an old
        /// `repo_roots = ["~"]` switches to scanning the code folder)
        #[arg(long)]
        keep_roots: bool,
        /// Take your home folder itself as the code folder without asking
        /// (the repo finder then scans all of it)
        #[arg(long)]
        allow_home: bool,
        /// Print the suggestion (no PATH) or what was saved, as JSON; never
        /// prompts
        #[arg(long)]
        json: bool,
    },
    /// Attach the terminal to a session (ref = id, id prefix, or name)
    Attach {
        r#ref: String,
    },
    /// Restart one agent session, resuming its conversation
    /// (to restart rlord itself: `rlord reset`)
    Restart {
        /// The session (id, id prefix, or name)
        r#ref: Option<String>,
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
    /// Set a session's one-line description
    Describe {
        r#ref: String,
        text: Vec<String>,
    },
    /// Edit a session's notes in $EDITOR (or set them with --set / --file)
    Notes {
        r#ref: String,
        #[arg(long, conflicts_with = "file")]
        set: Option<String>,
        #[arg(long)]
        file: Option<String>,
    },
    /// Remote Control: continue a Claude Code session from the Claude app or
    /// claude.ai. `rlord remote` lists what's connected (and whether this
    /// machine can use it); `rlord remote <session> on|off` turns it on or
    /// off (remembered across restarts).
    Remote {
        r#ref: Option<String>,
        /// on / off
        state: Option<String>,
        /// Restart now even if the session has work going
        #[arg(long, conflicts_with = "when_idle")]
        now: bool,
        /// Restart as soon as its work is done
        #[arg(long)]
        when_idle: bool,
    },
    /// Show everything known about a session
    Show {
        r#ref: String,
    },
    /// The coding agents on this machine (Claude Code, Codex, OpenCode):
    /// installed? rlord's hooks in place? and how to install one that's
    /// missing
    Agents {
        /// (Moved: `rlord subagents <session>`.)
        #[arg(hide = true)]
        r#ref: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List the sub-agents a session has spawned
    Subagents {
        r#ref: String,
        #[arg(long)]
        json: bool,
    },
    /// Show a session's event log
    Log {
        r#ref: String,
        #[arg(long, short = 'n', default_value = "50")]
        limit: i64,
    },
    /// Import agent conversations started outside rlord (found in the
    /// agents' own files). With no IDs, list what could be imported; with
    /// IDs (or unique prefixes), import them as stopped sessions that resume
    /// them. To have the overlord pick, name and group them, ask it to
    /// "import my existing sessions".
    Import {
        /// Conversation ids (or unique prefixes) to import
        ids: Vec<String>,
        /// Only list conversations active within this window (30d, 2w, 12h)
        #[arg(long, default_value = "30d")]
        since: String,
        /// Max conversations to list
        #[arg(long, default_value_t = 50)]
        limit: u64,
        /// Session name (importing a single conversation)
        #[arg(long)]
        name: Option<String>,
        /// Import as archived (history only)
        #[arg(long)]
        archive: bool,
        /// Recreate a removed worktree in its old place (its branch must exist)
        #[arg(long)]
        recreate_worktree: bool,
        /// Raw JSON output
        #[arg(long)]
        json: bool,
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
    Claude(AgentCmd),
    /// Codex integration management
    #[command(subcommand)]
    Codex(AgentCmd),
    /// Install rlord's hook handler for every agent that is installed
    /// (Claude Code, Codex): how rlord sees what each session is doing.
    /// (OpenCode needs none: rlord follows its background server.)
    InstallHooks,
    /// The overlord: the session you talk to in order to drive rlord
    #[command(subcommand)]
    Overlord(OverlordCmd),
    /// Pick sessions up when their usage limit resets: each session that
    /// stopped on a usage limit is sent a message ("continue") once the
    /// limit has reset, unless you used it meanwhile. Without sessions:
    /// every limited one. `--cancel` calls scheduled resumes off.
    ResumeAfterLimit {
        /// Sessions (id, prefix or name)
        sessions: Vec<String>,
        /// What to send (default: `[limits] resume_message`)
        #[arg(long, short)]
        message: Option<String>,
        /// Cancel the scheduled resume instead (without sessions: all)
        #[arg(long, conflicts_with = "message")]
        cancel: bool,
        #[arg(long)]
        json: bool,
    },
    /// Install the newest release from GitHub (no compiling), then reset a
    /// running rlord onto it. Same as scripts/update.sh.
    Update {
        /// Release tag to install, e.g. v0.18.0 (default: the newest on the
        /// channel)
        tag: Option<String>,
        /// Reinstall even if already on that version; replace a dev symlink
        #[arg(long)]
        force: bool,
        /// `stable`, or `dev` to include development builds (default:
        /// `[update] channel` in config.toml)
        #[arg(long)]
        channel: Option<String>,
    },
    /// Unstick rlord: restart the daemon, close overlays, resync session
    /// state, reload rlord's tmux config and key bindings, restart the list.
    /// Agent sessions keep running.
    Reset {
        /// Also kill rlord's whole tmux server and start clean. Every session
        /// stops — none are lost: Enter in the list resumes each one.
        #[arg(long)]
        hard: bool,
        /// Don't ask for confirmation (--hard)
        #[arg(long, short = 'y')]
        yes: bool,
        /// Also re-read every sub-agent transcript from the start (saved read
        /// positions and token totals are recounted)
        #[arg(long)]
        rescan: bool,
    },
    /// MCP server over stdio: read-only look-ups any agent session can
    /// add (`claude mcp add rlord -- rlord mcp`); `--overlord` serves the
    /// full toolset
    Mcp {
        /// Serve every tool (sending to sessions, stopping, renaming, UI
        /// control). Only rlord's overlord config passes it.
        #[arg(long)]
        overlord: bool,
        /// The daemon's socket, as the daemon resolved it. The overlord's
        /// MCP config always passes it: an agent may start this server with
        /// another environment (OpenCode's shared server), where the XDG
        /// dirs would name another socket. Given, no daemon is autostarted.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },
    /// The overlord overlay itself (runs inside the tmux popup)
    #[command(hide = true)]
    Overlay {
        #[arg(long)]
        token: Option<u64>,
    },
}

#[derive(Subcommand)]
enum OverlordCmd {
    /// Open rlord with the overlord shown (starts it if needed)
    Open,
    /// Show the overlord's state and whether its model is the latest
    Status,
    /// Switch the overlord to another model (restarts it; the conversation resumes)
    Model { model: String },
    /// Restart the overlord (resumes its conversation)
    Restart,
    /// Send the overlord a message, as if typed into it (text, or stdin)
    Ask { text: Vec<String> },
    /// Make another agent's overlord the active one: `rlord overlord switch
    /// codex`. Each agent has its own overlord and conversation; the first
    /// switch to one starts it. Without an agent: list them.
    Switch {
        /// claude, codex or opencode
        agent: Option<String>,
    },
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
    /// Import your tmux config as a profile agent sessions can use (your
    /// prefix, bindings and status bar, per session; the list and the
    /// overlord keep rlord's tmux). Re-run after changing your config.
    Import {
        /// Show what would be imported; change nothing
        #[arg(long)]
        dry_run: bool,
        /// Delete the imported profile
        #[arg(long, conflicts_with = "dry_run")]
        remove: bool,
    },
    /// Show or set whether a session uses your tmux profile
    Profile {
        /// on | off | toggle | default (follow `[tmux] profile`); omit to show
        state: Option<String>,
        /// Session (default: the one you're in)
        #[arg(name = "REF")]
        r#ref: Option<String>,
    },
}

#[derive(Subcommand)]
enum NavCmd {
    /// Back to the rlord TUI, or detach when attached from outside tmux
    Home,
    Next,
    Prev,
    /// Toggle the overlord overlay (Alt-O; Option-O on a Mac)
    Overlord,
    /// The overlord, for a key still bound from when it had a second
    /// session (the mastermind): the overlord is one session now
    #[command(hide = true)]
    Mastermind,
}

#[derive(Subcommand)]
enum HookCmd {
    /// Forward a hook payload from stdin to the daemon
    Emit { agent: String },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Install rlord's hook handler into the agent's hooks (Claude Code's
    /// settings.json, Codex's hooks.json): how rlord sees what its
    /// sessions are doing
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
            // The TUI always lives in rlord's own tmux server: that is what
            // lets rlord (and the overlord) move the user between it and
            // agent sessions. From a plain terminal (or the user's own tmux,
            // where this nests) open — or rejoin — its home session.
            if !tmux::inside_rlord() {
                use std::os::unix::process::CommandExt;
                ensure_daemon_current();
                // This run's tmux setup: `RLORD_TMUX_PROFILE=default` shows
                // rlord's own, as a new user gets it (unset: back to config).
                let env = std::env::var("RLORD_TMUX_PROFILE").ok();
                if let Err(e) = call("tmux.profile_mode", json!({"set": true, "env": env})) {
                    eprintln!("rlord: {e:#}");
                }
                tmux::write_conf()?;
                let cmd = format!("exec {} tui", tmux::shell_quote(&methods::rlord_bin()));
                let err = tmux::command()
                    .args(["new-session", "-A", "-s", "rlord", &cmd])
                    .exec();
                return Err(err).context("exec tmux failed");
            }
            if let Ok(conflicts) =
                tmux::bind_nav_keys(&methods::rlord_bin(), &cfg.tmux.bindings(), cfg.tmux.mouse)
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
                rlord_core::private::dir(dir)?;
            }
            std::fs::write(&path, Config::default_toml())?;
            println!("wrote {}", path.display());
            Ok(())
        }
        Command::Import {
            ids,
            since,
            limit,
            name,
            archive,
            recreate_worktree,
            json,
        } => import_cmd(ids, &since, limit, name, archive, recreate_worktree, json),
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
            let conflicts = tmux::bind_nav_keys(&methods::rlord_bin(), &b, cfg.tmux.mouse)?;
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
        Command::Tmux(TmuxCmd::Import { dry_run, remove }) => tmux_import(dry_run, remove),
        Command::Tmux(TmuxCmd::Profile { state, r#ref }) => tmux_profile_cmd(state, r#ref),
        Command::Tmux(TmuxCmd::Unbind) => {
            let cfg = Config::load()?;
            tmux::unbind_nav_keys(&cfg.tmux.bindings())?;
            println!("navigation bindings removed");
            Ok(())
        }
        Command::Nav(NavCmd::Home) => nav_home(),
        Command::Nav(NavCmd::Next) => nav_step(1),
        Command::Nav(NavCmd::Prev) => nav_step(-1),
        Command::Nav(NavCmd::Overlord | NavCmd::Mastermind) => {
            if let Err(e) = nav_overlord() {
                let _ = tmux::display_message(&format!("rlord: {e:#}"));
            }
            Ok(())
        }
        Command::Mcp { overlord, socket } => mcp_stdio(overlord, socket),
        Command::Update {
            tag,
            force,
            channel,
        } => update(tag, force, channel),
        Command::Reset { hard, yes, rescan } => reset(hard, yes, rescan),
        Command::Overlay { token } => rlord_tui::run_overlay(token),
        Command::Overlord(OverlordCmd::Open) => {
            if tmux::inside_rlord() {
                nav_overlord()
            } else {
                // Not in tmux: the overlay needs a tmux terminal to pop up
                // over, so start (or rejoin) rlord there first.
                eprintln!(
                    "rlord: run `rlord`, then press {}-O (or c)",
                    rlord_core::keys::alt_name()
                );
                Ok(())
            }
        }
        Command::Overlord(OverlordCmd::Status) => {
            let st = call("overlord.status", json!({}))?;
            if st["exists"] != true {
                println!(
                    "no overlord yet — `rlord overlord open` (or start `rlord`) creates one ({} / {})",
                    st["default_agent"].as_str().unwrap_or("?"),
                    st["default_model"].as_str().unwrap_or("?")
                );
                return Ok(());
            }
            let f = &st["freshness"];
            println!(
                "overlord {} · {} · model {} (running: {})",
                st["status"].as_str().unwrap_or("?"),
                st["agent"].as_str().unwrap_or("?"),
                st["model"].as_str().unwrap_or("(agent default)"),
                st["observed_model"].as_str().unwrap_or("not yet observed"),
            );
            if f["outdated"] == true {
                println!(
                    "  OUTDATED: {} — `rlord overlord model {}`",
                    f["note"].as_str().unwrap_or(""),
                    f["suggest"].as_str().unwrap_or("?")
                );
            } else if let Some(n) = f["note"].as_str() {
                println!("  note: {n}");
            } else if let Some(l) = f["latest"].as_str() {
                println!(
                    "  up to date (latest {} is {l})",
                    f["family"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
        Command::Overlord(OverlordCmd::Model { model }) => {
            let s = call("overlord.set_model", json!({"model": model}))?;
            println!(
                "overlord now on {} (restarted; conversation resumed)",
                s["model"].as_str().unwrap_or("the agent default")
            );
            Ok(())
        }
        Command::Overlord(OverlordCmd::Restart) => {
            let s = call("overlord.ensure", json!({}))?;
            call("session.restart", json!({"ref": s["id"]}))?;
            println!("overlord restarted");
            Ok(())
        }
        Command::Overlord(OverlordCmd::Ask { text }) => {
            let text = if text.is_empty() {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                buf
            } else {
                text.join(" ")
            };
            call("overlord.ask", json!({"text": text}))?;
            println!(
                "sent to the overlord ({}-O to see it)",
                rlord_core::keys::alt_name()
            );
            Ok(())
        }
        Command::Overlord(OverlordCmd::Switch { agent }) => {
            let Some(agent) = agent else {
                let st = call("overlord.status", json!({}))?;
                for o in st["overlords"].as_array().into_iter().flatten() {
                    let state = match (
                        o["installed"] == true,
                        o["exists"] == true,
                        o["running"] == true,
                    ) {
                        (false, _, _) => "not installed",
                        (_, false, _) => "not started yet",
                        (_, _, true) => "running",
                        _ => "stopped",
                    };
                    println!(
                        "{} {:<9} {state}",
                        if o["active"] == true { "*" } else { " " },
                        o["agent"].as_str().unwrap_or("?"),
                    );
                }
                println!("switch with `rlord overlord switch <agent>` (* is the active one)");
                return Ok(());
            };
            let s = call("overlord.switch", json!({"agent": agent}))?;
            let agent = s["agent"].as_str().unwrap_or(&agent);
            println!(
                "{}",
                match (s["switched"] == true, s["created"] == true) {
                    (false, _) => format!("the overlord is already on {agent}"),
                    (true, true) => format!("the overlord is now on {agent} (a new one, started)"),
                    (true, false) =>
                        format!("the overlord is now on {agent} (its conversation as you left it)"),
                }
            );
            Ok(())
        }
        Command::ResumeAfterLimit {
            sessions,
            message,
            cancel,
            json,
        } => {
            let mut p = json!({});
            if cancel {
                p["cancel"] = json!(true);
            }
            if !sessions.is_empty() {
                p["sessions"] = json!(sessions);
            }
            if let Some(m) = message {
                p["message"] = json!(m);
            }
            let r = call("limits.resume", p)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            for s in r["cancelled"].as_array().into_iter().flatten() {
                println!("{}: resume cancelled", s.as_str().unwrap_or("?"));
            }
            for s in r["scheduled"].as_array().into_iter().flatten() {
                println!(
                    "{}: \"{}\" at {}",
                    s["session"].as_str().unwrap_or("?"),
                    r["message"].as_str().unwrap_or(""),
                    s["at"].as_str().unwrap_or("?")
                );
            }
            for s in r["skipped"].as_array().into_iter().flatten() {
                println!(
                    "{}: skipped ({})",
                    s["session"].as_str().unwrap_or("?"),
                    s["why"].as_str().unwrap_or("")
                );
            }
            if let Some(n) = r["note"].as_str() {
                println!("{n}");
            }
            Ok(())
        }
        Command::Daemon => run_daemon(),
        Command::Ping => {
            let r = client_call(&paths::socket_path(), "ping", Value::Null)?;
            println!(
                "pong (daemon v{})",
                r.get("version").and_then(Value::as_str).unwrap_or("?")
            );
            Ok(())
        }
        Command::Search {
            words,
            everywhere,
            limit,
            json,
        } => {
            let v = call(
                "history.search",
                json!({"query": words.join(" "), "scope": if everywhere { "all" } else { "rlord" },
                       "limit": limit}),
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            print_search(&v);
            Ok(())
        }
        Command::History {
            session,
            around,
            json,
        } => {
            let mut p = if session.ends_with(".jsonl") {
                json!({"transcript": session})
            } else {
                json!({"session": session})
            };
            if let Some(l) = around {
                p["line"] = json!(l);
            }
            let v = call("history.read", p)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            for m in v["messages"].as_array().into_iter().flatten() {
                println!(
                    "── {} :{}  {}",
                    m["who"].as_str().unwrap_or(""),
                    m["line"],
                    m["at"].as_str().unwrap_or("")
                );
                println!("{}\n", m["text"].as_str().unwrap_or(""));
            }
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
            usage,
            json,
        } => {
            // Unasked, the daemon picks (the list's saved measure, else
            // config) and says which it used.
            let usage = usage
                .map(|u| {
                    rlord_core::usage::UsageMetric::parse(&u)
                        .with_context(|| format!("unknown usage '{u}' (turns|tokens|context)"))
                })
                .transpose()?;
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
            let mut params = json!({
                "archived": archived_mode, "status": q.status, "tag": q.tag,
                "agent": q.agent, "project": q.project, "name": q.name,
                "group": q.group, "search": q.search, "sort": q.sort.label(),
                "reverse": q.reverse,
            });
            if let Some(u) = usage {
                params["usage"] = json!(u.label());
            }
            let r = call("session.list", params)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                // The measure the daemon used (an empty list: any).
                let used = r[0]["usage_metric"]
                    .as_str()
                    .and_then(rlord_core::usage::UsageMetric::parse)
                    .or(usage)
                    .unwrap_or_default();
                print_session_table(&r, used);
            }
            Ok(())
        }
        Command::New {
            prompt,
            repo,
            project,
            no_workspace,
            branch,
            model,
            agent,
            name,
            attach,
        } => {
            let prompt = if prompt.is_empty() {
                None
            } else {
                Some(prompt.join(" "))
            };
            // No location given: the repo around cwd, else a plain cwd folder
            // when no default_repo is configured (the daemon applies that).
            let (repo, project) = match (repo, project) {
                (None, None) => {
                    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
                    match rlord_core::git::main_repo_root(&cwd) {
                        Some(root) => (Some(root), None),
                        None if Config::load()?.session.default_repo_path().is_some() => {
                            (None, None)
                        }
                        None => (None, Some(cwd)),
                    }
                }
                other => other,
            };
            let r = call(
                "session.new",
                json!({
                    "repo": repo, "project": project, "name": name, "prompt": prompt,
                    "branch": branch, "model": model, "agent": agent,
                    "workspace": if no_workspace { Some(false) } else { None },
                }),
            )?;
            let id = r["id"].as_str().unwrap_or("?");
            match r["workspace_path"].as_str() {
                Some(ws) => println!(
                    "launched {} ({}) in workspace {} [{}]",
                    r["name"].as_str().unwrap_or("?"),
                    id,
                    ws,
                    r["branch"].as_str().unwrap_or("?")
                ),
                None => println!("launched {} ({})", r["name"].as_str().unwrap_or("?"), id),
            }
            if attach {
                attach_tmux(r["tmux_session"].as_str().context("no tmux session")?)?;
            }
            Ok(())
        }
        Command::Fork {
            r#ref,
            prompt,
            name,
            model,
            worktree,
            branch,
            attach,
            json,
        } => {
            let prompt = (!prompt.is_empty()).then(|| prompt.join(" "));
            let r = call(
                "session.fork",
                json!({"ref": r#ref, "prompt": prompt, "name": name, "model": model,
                       "workspace": worktree, "branch": branch}),
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print!("{}", fork_lines(&r));
            }
            if attach {
                attach_tmux(r["tmux_session"].as_str().context("no tmux session")?)?;
            }
            Ok(())
        }
        Command::Setup {
            path,
            keep_roots,
            allow_home,
            json,
        } => setup_cmd(path, keep_roots, allow_home, json),
        Command::Repos => {
            let r = call("repo.list", Value::Null)?;
            for repo in r.as_array().map(|v| v.as_slice()).unwrap_or_default() {
                println!(
                    "{} {:<6} {}",
                    if repo["default"].as_bool().unwrap_or(false) {
                        "*"
                    } else {
                        " "
                    },
                    rel_age(repo["last_used_at"].as_i64()),
                    repo["path"].as_str().unwrap_or("?")
                );
            }
            Ok(())
        }
        Command::Latest {
            repo,
            git_ref,
            prefer,
            list,
            prune,
            all,
            json,
        } => {
            let prune = prune.then_some(all);
            latest_cmd(repo, git_ref, prefer, list, prune, json).map(drop)
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
            let Some(r#ref) = r#ref else {
                anyhow::bail!(
                    "`rlord restart <session>` restarts one agent session; \
                     to restart rlord itself use `rlord reset` (or `rlord reset --hard`)"
                );
            };
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
            print_turn(&r);
            Ok(())
        }
        Command::Archive { r#ref } => {
            let r = call("session.archive", json!({"ref": r#ref}))?;
            println!("archived {}", r["archived"].as_str().unwrap_or("?"));
            print_turn(&r);
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
        Command::Describe { r#ref, text } => {
            let text = text.join(" ");
            call(
                "session.describe",
                json!({"ref": r#ref, "description": text}),
            )?;
            println!("description set");
            Ok(())
        }
        Command::Notes { r#ref, set, file } => {
            let s = call("session.get", json!({"ref": r#ref}))?;
            let id = s["id"].as_str().unwrap_or("").to_string();
            let notes = match (set, file) {
                (Some(t), _) => t,
                (None, Some(f)) => std::fs::read_to_string(f)?,
                (None, None) => {
                    let current = s["notes"].as_str().unwrap_or("").to_string();
                    let path = std::env::temp_dir().join(format!("rlord-notes-{id}.md"));
                    std::fs::write(&path, &current)?;
                    let editor = std::env::var("VISUAL")
                        .or_else(|_| std::env::var("EDITOR"))
                        .unwrap_or_else(|_| "vi".into());
                    let st = std::process::Command::new("sh")
                        .arg("-c")
                        .arg(format!("{editor} \"$1\""))
                        .arg("rlord")
                        .arg(&path)
                        .status()?;
                    if !st.success() {
                        anyhow::bail!("editor exited with {st}; notes not saved");
                    }
                    let new = std::fs::read_to_string(&path)?;
                    let _ = std::fs::remove_file(&path);
                    new
                }
            };
            call("session.notes", json!({"ref": id, "notes": notes}))?;
            println!("notes saved");
            Ok(())
        }
        Command::Agents {
            r#ref: Some(r),
            json,
        }
        | Command::Subagents { r#ref: r, json } => {
            // (`rlord agents <session>` was this before `rlord agents`
            // listed the coding agents.)
            if std::env::args().nth(1).as_deref() == Some("agents") {
                eprintln!("note: sub-agents moved to `rlord subagents {r}`");
            }
            let r = call("session.subagents", json!({"ref": r}))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print_agents(&r);
            }
            Ok(())
        }
        Command::Agents { r#ref: None, json } => {
            // Asked of the daemon: its PATH is the one sessions launch with.
            let r = call("agents.list", json!({}))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print_coding_agents(&r);
            }
            Ok(())
        }
        Command::Remote {
            r#ref,
            state,
            now,
            when_idle,
        } => {
            let print = |name: &str, r: &Value| {
                let st = r["state"].as_str().unwrap_or("off");
                let extra = r["url"]
                    .as_str()
                    .or(r["reason"].as_str())
                    .map(|x| format!("  {x}"))
                    .unwrap_or_default();
                println!("{name:<28} {st}{extra}");
            };
            let avail = |a: &Value| {
                if a["available"] == false {
                    println!(
                        "Remote Control isn't available here: {}",
                        a["reason"].as_str().unwrap_or("unknown")
                    );
                } else if let Some(n) = a["note"].as_str() {
                    println!("note: {n}");
                }
            };
            match (r#ref, state.as_deref()) {
                (Some(r), Some(s @ ("on" | "off"))) => {
                    let restart = if now {
                        json!("now")
                    } else if when_idle {
                        json!("when_idle")
                    } else {
                        Value::Null
                    };
                    let v = call(
                        "remote.set",
                        json!({"ref": r, "on": s == "on", "restart": restart}),
                    )?;
                    if v["needs_confirm"] == true {
                        let why: Vec<&str> = v["work_going"]
                            .as_array()
                            .map(|a| a.iter().filter_map(Value::as_str).collect())
                            .unwrap_or_default();
                        println!(
                            "{} has work going: {}. Restarting it would interrupt that.",
                            v["name"].as_str().unwrap_or("it"),
                            why.join("; ")
                        );
                        println!("Run again with --now, or --when-idle to restart once it's done.");
                        return Ok(());
                    }
                    print(v["name"].as_str().unwrap_or("?"), &v["remote"]);
                    if let Some(n) = v["note"].as_str().filter(|n| !n.is_empty()) {
                        println!("{n}");
                    }
                }
                (_, Some(other)) => {
                    anyhow::bail!("usage: rlord remote [session] [on|off] (got '{other}')")
                }
                (r, None) => {
                    let v = call("remote.status", json!({"ref": r, "force": true}))?;
                    avail(&v["availability"]);
                    match v["sessions"].as_array() {
                        Some(list) if list.is_empty() => {
                            println!("no session has Remote Control on")
                        }
                        Some(list) => {
                            for x in list {
                                print(x["name"].as_str().unwrap_or("?"), &x["remote"]);
                            }
                        }
                        None => print(v["name"].as_str().unwrap_or("?"), &v["remote"]),
                    }
                }
            }
            Ok(())
        }
        Command::Show { r#ref } => {
            let s = call("session.get", json!({"ref": r#ref}))?;
            let g = |k: &str| s[k].as_str().unwrap_or("-").to_string();
            let t = |k: &str| {
                s[k].as_i64()
                    .map(|v| format!("{} ({})", fmt_ts(v), rel_age(Some(v))))
                    .unwrap_or_else(|| "-".into())
            };
            println!("{} ({})", g("name"), g("id"));
            println!(
                "  agent      {} · model {} · {}",
                g("agent"),
                g("model"),
                g("status")
            );
            println!("  repo       {}  branch {}", g("repo_path"), g("branch"));
            println!("  dir        {}", g("project_path"));
            println!("  created    {}", t("created_at"));
            println!("  user msg   {}", t("last_user_msg_at"));
            println!("  rlord msg  {}", t("last_rlord_msg_at"));
            println!("  agent msg  {}", t("last_agent_msg_at"));
            println!("  state      {}", t("last_state_change_at"));
            println!("  tags       {}   group {}", tags_of(&s), g("group_path"));
            println!("  desc       {}", g("description"));
            print!("{}", lineage_lines(&s));
            let notes = s["notes"].as_str().unwrap_or("");
            if !notes.is_empty() {
                println!("  notes");
                for l in notes.lines() {
                    println!("    {l}");
                }
            }
            let agents = call("session.subagents", json!({"ref": g("id")}))?;
            if agents.as_array().is_some_and(|a| !a.is_empty()) {
                println!("  agents");
                print_agents(&agents);
            }
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
        Command::Claude(AgentCmd::InstallHooks) => install_hooks(&[Agent::Claude]),
        Command::Codex(AgentCmd::InstallHooks) => install_hooks(&[Agent::Codex]),
        Command::InstallHooks => {
            // (OpenCode has no hooks: rlord follows its server's events.)
            let found: Vec<Agent> = Agent::ALL
                .into_iter()
                .filter(|a| a.uses_hooks() && a.installed())
                .collect();
            if found.is_empty() {
                anyhow::bail!("neither `claude` nor `codex` is on PATH: `rlord agents` shows how to install one");
            }
            install_hooks(&found)
        }
    }
}

/// Merge rlord's hook command into each agent's hooks file, and say what
/// the user still has to do (Codex: trust them).
fn install_hooks(agents: &[Agent]) -> Result<()> {
    let bin = methods::rlord_bin();
    for (i, agent) in agents.iter().enumerate() {
        let changed = agent.install_hooks(&bin)?;
        if agents.len() > 1 {
            if i > 0 {
                println!();
            }
            println!("{}:", agent.label());
        }
        println!(
            "{} ({})",
            if changed {
                "hooks installed"
            } else {
                "hooks already installed"
            },
            agent.hooks_file().display()
        );
        match agent {
            Agent::Codex => {
                let trusted = rlord_core::agents::install::codex_hooks_trusted(
                    &rlord_core::agents::codex::home(),
                );
                if trusted == Some(true) && !changed {
                    println!("trusted in Codex: yes");
                } else {
                    if changed {
                        println!(
                            "\nrlord's hooks are new or changed: Codex needs you to trust them \
                             (again, if you had before)."
                        );
                    }
                    if let Some(next) = agent.hooks_next_step() {
                        println!("\n{next}");
                    }
                }
            }
            Agent::Claude | Agent::OpenCode => {}
        }
    }
    Ok(())
}

fn tmux_import(dry_run: bool, remove: bool) -> Result<()> {
    use rlord_core::tmux_profile;
    if remove {
        tmux_profile::remove()?;
        tmux::write_conf()?;
        println!("tmux profile removed");
    } else {
        let cfg = Config::load()?;
        let reserved: Vec<&str> = cfg.tmux.bindings().iter().map(|(k, _)| *k).collect();
        eprintln!("reading your tmux config on a scratch server…");
        let p = tmux_profile::import(&reserved)?;
        print!("{}", p.report.render());
        if dry_run {
            println!("(dry run: nothing written)");
            return Ok(());
        }
        tmux_profile::write(&p)?;
        println!(
            "wrote {} (report: {})",
            tmux_profile::keys_path().display(),
            tmux_profile::report_path().display()
        );
    }
    // No autostart: a daemon that starts later applies the profile itself.
    match call_running("tmux.reload", json!({})) {
        Ok(_) => println!("rlord's tmux reloaded; sessions updated"),
        Err(e) => println!("not applied yet ({e:#}): it applies when rlord's daemon starts"),
    }
    Ok(())
}

fn tmux_profile_cmd(state: Option<String>, r: Option<String>) -> Result<()> {
    // One argument that isn't a state is the session: `rlord tmux profile beta`.
    let (state, r) = match (state, r) {
        (Some(s), None) if !matches!(s.as_str(), "on" | "off" | "toggle" | "default") => {
            (None, Some(s))
        }
        x => x,
    };
    // Which setup agent sessions get, and why.
    let m = call("tmux.profile_mode", json!({}))?;
    println!(
        "tmux profile: {} ({})",
        m["mode"].as_str().unwrap_or("?"),
        m["why"].as_str().unwrap_or("?")
    );
    if m["why"] == "RLORD_TMUX_PROFILE" {
        println!("RLORD_TMUX_PROFILE overrides per-session choices");
    }
    let current = tmux::current_session()
        // rlord's tmux sessions: `<name>-rlord` (older: `rlord-…`).
        .filter(|s| s.ends_with("-rlord") || s.starts_with("rlord-"));
    let r = match (r, current) {
        (Some(r), _) => r,
        // resolve_ref accepts the tmux session name too.
        (None, Some(c)) => c,
        (None, None) if state.is_none() => return Ok(()),
        (None, None) => anyhow::bail!("not in an rlord session: give a session REF"),
    };
    let mut params = json!({"ref": r});
    if let Some(s) = &state {
        params["set"] = json!(s);
    }
    let v = call("tmux.profile", params)?;
    let on = v["on"].as_bool().unwrap_or(false);
    println!(
        "{}: your tmux profile {}{}",
        v["name"].as_str().unwrap_or("?"),
        if on { "on" } else { "off" },
        if v["explicit"] == true {
            ""
        } else {
            " (default)"
        }
    );
    if v["imported"] != true {
        println!("no profile imported yet — run `rlord tmux import`");
    }
    Ok(())
}

/// RPC to the daemon, starting it (or replacing an older-version one) on the
/// first call of this process.
/// `30d` / `2w` / `12h` / `45` (days) to hours.
fn parse_since_hours(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num
        .parse()
        .with_context(|| format!("bad --since '{s}' (try 30d, 2w, 12h)"))?;
    Ok(match unit {
        "h" => n,
        "" | "d" => n * 24,
        "w" => n * 24 * 7,
        _ => anyhow::bail!("bad --since '{s}' (try 30d, 2w, 12h)"),
    })
}

fn import_cmd(
    ids: Vec<String>,
    since: &str,
    limit: u64,
    name: Option<String>,
    archive: bool,
    recreate_worktree: bool,
    json_out: bool,
) -> Result<()> {
    if ids.is_empty() {
        let r = call(
            "session.import_scan",
            json!({"since_hours": parse_since_hours(since)?, "limit": limit}),
        )?;
        if json_out {
            println!("{}", serde_json::to_string_pretty(&r["candidates"])?);
            return Ok(());
        }
        let list = r["candidates"].as_array().cloned().unwrap_or_default();
        if list.is_empty() {
            println!("nothing to import: no untracked conversations active in the last {since}");
            return Ok(());
        }
        let home = std::env::var("HOME").unwrap_or_default();
        for c in &list {
            let dir = c["cwd"].as_str().unwrap_or("");
            let dir = match dir.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => dir.to_string(),
            };
            let what = c["title"]
                .as_str()
                .or(c["first_prompt"].as_str())
                .unwrap_or("");
            let mut flags = Vec::new();
            if c["dir_exists"] == false {
                flags.push(if c["recreatable"] == true {
                    "folder gone, recreatable"
                } else {
                    "folder gone"
                });
            }
            if c["open_elsewhere"] == true || c["recently_written"] == true {
                flags.push("maybe open elsewhere");
            }
            let flags = if flags.is_empty() {
                String::new()
            } else {
                format!("  [{}]", flags.join(", "))
            };
            println!(
                "{}  {:>9}  {}{}\n          {}",
                &c["agent_session"].as_str().unwrap_or("")[..8],
                rel_age(c["last_active_at"].as_i64()),
                dir,
                flags,
                transcript::clip(what, 100),
            );
        }
        println!(
            "\n{} conversation(s). Import with `rlord import <id>...`, or ask the overlord to pick and organize them.",
            list.len()
        );
        return Ok(());
    }
    if name.is_some() && ids.len() > 1 {
        anyhow::bail!("--name needs a single conversation id");
    }
    let mut failed = 0;
    for id in &ids {
        let full = resolve_conversation(id)?;
        let mut p = json!({"agent_session": full, "archive": archive,
                           "recreate_worktree": recreate_worktree});
        if let Some(n) = &name {
            p["name"] = json!(n);
        }
        match call("session.import", p) {
            Ok(s) => {
                println!(
                    "imported {} as {}{}",
                    &full[..8],
                    s["name"].as_str().unwrap_or("?"),
                    if archive { " (archived)" } else { "" }
                );
                if let Some(w) = s["warning"].as_str() {
                    eprintln!("  warning: {w}");
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("{}: {e:#}", &full[..8]);
            }
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} of {} not imported", ids.len());
    }
    Ok(())
}

/// A full conversation id from an id or unique prefix.
fn resolve_conversation(id: &str) -> Result<String> {
    if id.len() == 36 {
        return Ok(id.to_string());
    }
    // Matched by prefix in the daemon, at any age: the listing's window and
    // limit shouldn't decide what a prefix finds.
    let r = call("session.import_scan", json!({"ids": [id], "limit": 200}))?;
    let hits: Vec<String> = r["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["agent_session"].as_str())
        .map(str::to_string)
        .collect();
    match hits.as_slice() {
        [one] => Ok(one.clone()),
        [] if r["rejected"][0]["reason"]
            .as_str()
            .is_some_and(|w| !w.starts_with("rlord already tracks")) =>
        {
            anyhow::bail!(
                "'{id}' can't be imported: {}",
                r["rejected"][0]["reason"].as_str().unwrap_or("")
            )
        }
        [] => {
            let all = call("session.list", json!({"archived": "all"}))?;
            let owner = all.as_array().into_iter().flatten().find(|s| {
                s["agent_session"]
                    .as_str()
                    .is_some_and(|a| a.starts_with(id))
            });
            match owner {
                Some(s) => anyhow::bail!(
                    "'{id}' is already in rlord as '{}'",
                    s["name"].as_str().unwrap_or("?")
                ),
                None => anyhow::bail!("no conversation starts with '{id}'"),
            }
        }
        _ => anyhow::bail!(
            "'{id}' matches {} conversations; use more of the id",
            hits.len()
        ),
    }
}

/// scripts/update.sh, compiled in so updating needs no checkout.
const UPDATE_SH: &str = include_str!("../update.sh");

fn update(tag: Option<String>, force: bool, channel: Option<String>) -> Result<()> {
    // update.sh installs into ~/.local/bin: over a package's or Homebrew's
    // copy that would leave two, the older one shadowing or shadowed.
    if std::env::var_os("RLORD_INSTALL_DIR").is_none() {
        let exe = std::env::current_exe().and_then(|e| e.canonicalize());
        if let Some(how) = exe.as_deref().ok().and_then(installed_by) {
            anyhow::bail!(
                "this rlord ({}) came from {how}; `rlord update` only updates the copy \
                 get.sh installs in ~/.local/bin",
                exe.as_deref()
                    .map(|e| e.display().to_string())
                    .unwrap_or_default()
            );
        }
    }
    let channel = channel.unwrap_or_else(|| {
        Config::load()
            .map(|c| c.update.channel)
            .unwrap_or_else(|_| "stable".into())
    });
    let mut cmd = std::process::Command::new("bash");
    cmd.arg("-c")
        .arg(UPDATE_SH)
        .arg("rlord-update")
        .env("RLORD_CHANNEL", channel.trim());
    if force {
        cmd.arg("--force");
    }
    cmd.args(tag);
    let st = cmd.status().context("running bash")?;
    if !st.success() {
        anyhow::bail!("update failed");
    }
    Ok(())
}

/// `rlord search`, for a terminal.
fn print_search(v: &Value) {
    let rows = v["sessions"].as_array().cloned().unwrap_or_default();
    let outside = v["outside_rlord"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() && outside.is_empty() {
        println!("nothing found");
        return;
    }
    let hits = |h: &Value| {
        for x in h.as_array().into_iter().flatten() {
            println!(
                "    :{:<6} {:<5} {}",
                x["line"].as_u64().unwrap_or(0),
                x["who"].as_str().unwrap_or(""),
                x["snippet"].as_str().unwrap_or("")
            );
            // A hit in an older (pre-/clear) transcript: read it by path.
            if let Some(t) = x["transcript"].as_str() {
                println!("           in {t}");
            }
        }
    };
    for r in &rows {
        let fields: Vec<&str> = r["matched_in"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        println!(
            "{}{}  ({} in transcript{})",
            r["session"].as_str().unwrap_or("?"),
            if r["archived"] == true {
                " [archived]"
            } else {
                ""
            },
            r["transcript_hits"],
            if fields.is_empty() {
                String::new()
            } else {
                format!("; matches its {}", fields.join(", "))
            }
        );
        if let Some(a) = r["about"].as_str().filter(|a| !a.is_empty()) {
            println!("    {a}");
        }
        hits(&r["hits"]);
    }
    if v["more_sessions"].as_u64().unwrap_or(0) > 0 {
        println!("… {} more (--limit)", v["more_sessions"]);
    }
    for r in &outside {
        println!(
            "outside rlord: {}  ({} hits)\n    {}",
            r["folder"].as_str().unwrap_or(""),
            r["transcript_hits"],
            r["transcript"].as_str().unwrap_or("")
        );
        hits(&r["hits"]);
    }
    println!("\nread around a hit: rlord history <session or transcript path> --around <line>");
}

/// `rlord setup [PATH] [--keep-roots] [--allow-home] [--json]`.
fn setup_cmd(
    path: Option<String>,
    keep_roots: bool,
    allow_home_flag: bool,
    json_out: bool,
) -> Result<()> {
    use rlord_core::config::contract_tilde;
    use std::io::{IsTerminal, Write};
    let cwd = std::env::current_dir()?;
    // (A cheap look-up: the suggestion reads every agent's files, so it is
    // asked for only when there is no PATH.)
    let status = call("setup.status", json!({}))?;
    let configured: Vec<String> = status["repo_roots"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let print_saved = |r: &Value| {
        if json_out {
            println!("{}", serde_json::to_string_pretty(r).unwrap_or_default());
            return;
        }
        let roots: Vec<String> = r["repo_roots"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(contract_tilde)
                    .collect()
            })
            .unwrap_or_default();
        println!(
            "code folder: {}{} · the repo finder scans {}",
            contract_tilde(r["code_dir"].as_str().unwrap_or("")),
            if r["created"] == true {
                " (created)"
            } else {
                ""
            },
            roots.join(", ")
        );
        if r["checked"] == false {
            println!(
                "not checked: macOS guards this folder; create it yourself if it doesn't exist"
            );
        }
        println!("saved to {}", r["config"].as_str().unwrap_or(""));
    };
    let ask = |q: &str| -> Result<String> {
        print!("{q}");
        std::io::stdout().flush()?;
        let mut a = String::new();
        std::io::stdin().read_line(&mut a)?;
        Ok(a.trim().to_string())
    };
    let terminal = std::io::stdin().is_terminal();
    // `$HOME` itself: the finder would scan all of it. Said, and confirmed
    // at a terminal; refused otherwise.
    let home_ok = |folder: &str| -> Result<bool> {
        if !is_home(folder) {
            return Ok(false);
        }
        if allow_home_flag {
            return Ok(true);
        }
        let warn = format!(
            "{} is your whole home folder: the repo finder scans all of it.",
            contract_tilde(folder)
        );
        // (`--json` never prompts.)
        if !terminal || json_out {
            anyhow::bail!("{warn} Pass --allow-home to use it anyway.");
        }
        if !yes_no(&ask(&format!("{warn} Use it anyway? [y/N] "))?, false) {
            anyhow::bail!("aborted");
        }
        Ok(true)
    };
    if let Some(p) = path {
        let p = absolute_folder(&p, &cwd)?;
        let allow_home = home_ok(&p)?;
        let roots = roots_edit(&status["roots_default"], keep_roots);
        let r = call(
            "setup.apply",
            json!({"code_dir": p, "roots": roots, "create": true, "allow_home": allow_home}),
        )?;
        print_saved(&r);
        return Ok(());
    }
    // (Nothing to ask without a terminal: say so before reading every
    // agent's files for a suggestion.)
    if !json_out && !terminal {
        anyhow::bail!("pass the folder: rlord setup <path> (or --json for rlord's suggestion)");
    }
    let launch = rlord_core::git::main_repo_root(&cwd.to_string_lossy());
    let sugg = call("setup.suggest", json!({"launch_dir": launch}))?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&sugg)?);
        return Ok(());
    }
    let label = |c: &Value| {
        let sources = rlord_core::code_dir::sources_of(&sugg["sources"]);
        let mut l = format!(
            "{}: {}",
            contract_tilde(c["path"].as_str().unwrap_or("")),
            rlord_core::code_dir::basis(
                c["projects"].as_u64().unwrap_or(0) as usize,
                c["of"].as_u64().unwrap_or(0) as usize,
                &sources
            )
        );
        if c["from"] == "repo_roots" {
            l.push_str(" (your repo_roots)");
        }
        if c["prompts"] == true {
            l.push_str(" (macOS will ask permission for it once)");
        } else if c["checked"] == false {
            l.push_str(" (not checked)");
        }
        l
    };
    let current = sugg["code_dir"].as_str().unwrap_or("");
    if !current.is_empty() {
        println!("code folder now: {}", contract_tilde(current));
    }
    let best = sugg["suggestion"]
        .as_object()
        .map(|_| contract_tilde(sugg["suggestion"]["path"].as_str().unwrap_or("")));
    match &best {
        Some(_) => println!("suggested: {}", label(&sugg["suggestion"])),
        None => println!("rlord found no projects yet to suggest a code folder from"),
    }
    for a in sugg["alternatives"].as_array().into_iter().flatten() {
        println!("       or: {}", label(a));
    }
    let prompt = match &best {
        Some(b) => format!("Code folder [{b}]: "),
        None => "Code folder: ".to_string(),
    };
    let Some(folder) = pick_folder(&ask(&prompt)?, best.as_deref()) else {
        println!("aborted: no folder given");
        return Ok(());
    };
    let folder = absolute_folder(&folder, &cwd)?;
    let allow_home = home_ok(&folder)?;
    let mut roots = roots_edit(&status["roots_default"], keep_roots);
    let differs = rlord_core::config::roots_beyond_code_dir(&configured, &folder);
    if !keep_roots && differs {
        let default_yes = roots == "follow";
        let roots_was = roots.clone();
        let q = roots_question(
            status["code_dir"].as_str().unwrap_or(""),
            &configured,
            &folder,
            default_yes,
        );
        roots = if yes_no(&ask(&q)?, default_yes) {
            json!("follow")
        } else if default_yes {
            json!("leave")
        } else {
            roots_was
        };
    }
    let guarded = rlord_core::privacy::Guard::new()
        .prompts(std::path::Path::new(&folder))
        .is_some();
    if !guarded && !std::path::Path::new(&folder).is_dir() {
        let q = format!("Create {}? [Y/n] ", contract_tilde(&folder));
        if !yes_no(&ask(&q)?, true) {
            println!("aborted");
            return Ok(());
        }
    }
    let r = call(
        "setup.apply",
        json!({"code_dir": folder, "roots": roots, "create": true, "allow_home": allow_home}),
    )?;
    print_saved(&r);
    Ok(())
}

/// `folder` is `$HOME` (as given or resolved).
fn is_home(folder: &str) -> bool {
    rlord_core::privacy::is_home(std::path::Path::new(folder))
}

/// The question about `repo_roots`: everything the finder scans now (the
/// code folder and `repo_roots`), and exactly what "y" changes.
fn roots_question(
    code_dir: &str,
    configured: &[String],
    folder: &str,
    default_yes: bool,
) -> String {
    use rlord_core::config::contract_tilde;
    let now: Vec<String> = std::iter::once(code_dir)
        .filter(|c| !c.is_empty())
        .map(contract_tilde)
        .chain(configured.iter().cloned())
        .collect();
    format!(
        "The repo finder scans {} now. y: remove repo_roots ({}) from the config, so it \
         scans only {}. [{}] ",
        now.join(", "),
        configured.join(", "),
        contract_tilde(folder),
        if default_yes { "Y/n" } else { "y/N" }
    )
}

/// What `rlord setup` does with `repo_roots`: the daemon's
/// `roots_default` (`setup.status`) as given, or leave them with
/// `--keep-roots`.
fn roots_edit(default: &Value, keep_roots: bool) -> Value {
    if keep_roots || default.is_null() {
        json!("leave")
    } else {
        default.clone()
    }
}

/// The folder typed at the prompt: empty takes the suggestion (None when
/// there is none).
fn pick_folder(answer: &str, suggestion: Option<&str>) -> Option<String> {
    match answer.trim() {
        "" => suggestion.map(str::to_string),
        a => Some(a.to_string()),
    }
}

/// A yes/no answer; empty or anything else takes the default.
fn yes_no(answer: &str, default: bool) -> bool {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    }
}

/// A folder as typed, made absolute: `~` expanded, a relative path taken
/// from `cwd`, then checked as every folder rlord saves is
/// (`config::absolute_dir`: not `/`, trailing `/` trimmed).
fn absolute_folder(p: &str, cwd: &std::path::Path) -> Result<String> {
    let e = rlord_core::config::expand_tilde(p.trim());
    let abs = if std::path::Path::new(&e).is_absolute() {
        e
    } else {
        cwd.join(e).to_string_lossy().into_owned()
    };
    rlord_core::config::absolute_dir(&abs)
}

/// `rlord latest`: one RPC each (`repo.latest`, `_list`, `_prune`).
/// `repo.latest_prune`'s params for `rlord latest --prune [REPO] [--all]`:
/// REPO (made absolute against `cwd`), else the current folder's repo;
/// every repo only with `--all`.
fn prune_params(repo: Option<String>, all: bool, cwd: &std::path::Path) -> Value {
    if all {
        return json!({"all": true});
    }
    let p = std::path::PathBuf::from(rlord_core::config::expand_tilde(
        repo.as_deref().unwrap_or("."),
    ));
    let p = if p.is_absolute() { p } else { cwd.join(p) };
    json!({"repo": p.to_string_lossy()})
}

/// `prune`: Some(all) for `--prune`.
fn latest_cmd(
    repo: Option<String>,
    git_ref: Option<String>,
    prefer: Option<String>,
    list: bool,
    prune: Option<bool>,
    json: bool,
) -> Result<Value> {
    let abs = |r: String| -> Result<String> {
        let p = std::path::PathBuf::from(rlord_core::config::expand_tilde(&r));
        Ok(if p.is_absolute() {
            p
        } else {
            std::env::current_dir()?.join(p)
        }
        .to_string_lossy()
        .into_owned())
    };
    let v = if list {
        call("repo.latest_list", json!({}))?
    } else if let Some(all) = prune {
        call(
            "repo.latest_prune",
            prune_params(repo.clone(), all, &std::env::current_dir()?),
        )?
    } else {
        let repo = abs(repo.unwrap_or_else(|| ".".into()))?;
        call(
            "repo.latest",
            json!({"repo": repo, "ref": git_ref, "prefer": prefer}),
        )?
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(v);
    }
    let short = |c: &Value| {
        c.as_str()
            .map(rlord_core::latest::short)
            .unwrap_or("-")
            .to_string()
    };
    if list {
        for t in v["trees"].as_array().map(Vec::as_slice).unwrap_or_default() {
            println!(
                "{:<6} {:<20} {}  {}",
                rel_age(t["used_at"].as_i64()),
                t["ref"].as_str().unwrap_or("-"),
                short(&t["commit"]),
                t["path"].as_str().or(t["repo"].as_str()).unwrap_or("?")
            );
        }
    } else if prune.is_some() {
        for r in v["removed"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            println!("removed {}", r.as_str().unwrap_or("?"));
        }
        for f in v["failed"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            eprintln!(
                "{}: {}",
                f["repo"].as_str().unwrap_or("?"),
                f["error"].as_str().unwrap_or("")
            );
        }
    } else if v["git"] == false {
        println!("{} (not a git repo)", v["path"].as_str().unwrap_or("?"));
    } else if v["diverged"] == true {
        let side = |s: &Value| {
            format!(
                "{} {} ({} ahead)",
                short(&s["commit"]),
                s["subject"].as_str().unwrap_or(""),
                s["ahead"]
                    .as_u64()
                    .map(|n| n.to_string())
                    .unwrap_or("?".into())
            )
        };
        println!("{} has diverged:", v["ref"].as_str().unwrap_or("?"));
        println!("  local:  {}", side(&v["local"]));
        println!("  remote: {}", side(&v["remote_side"]));
        println!("pass --prefer local or --prefer remote");
    } else {
        println!("{}", v["path"].as_str().unwrap_or("?"));
        println!(
            "{} at {} ({}) {}; fetch: {}",
            v["ref"].as_str().unwrap_or("?"),
            short(&v["commit"]),
            v["source"].as_str().unwrap_or("?"),
            v["subject"].as_str().unwrap_or(""),
            v["fetch"].as_str().unwrap_or("?")
        );
    }
    for w in v["warnings"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        eprintln!("warning: {}", w.as_str().unwrap_or(""));
    }
    Ok(v)
}

fn call(method: &str, params: Value) -> Result<Value> {
    static ENSURED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let socket = paths::socket_path();
    if ENSURED.get().is_none() {
        autostart::ensure_daemon(&socket)?;
        ENSURED.set(()).ok();
    }
    client_call(&socket, method, params)
}

/// RPC to an already-running daemon only (never starts one), for callers
/// that tolerate a missing daemon and mustn't block on starting it.
fn call_running(method: &str, params: Value) -> Result<Value> {
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

/// Start the daemon, or restart it if it runs an older build than this
/// binary (after a rebuild/upgrade a stale daemon and a new UI disagree
/// about everything). Not fatal: the TUI retries and reports errors itself.
fn ensure_daemon_current() {
    if let Err(e) = autostart::ensure_daemon(&paths::socket_path()) {
        eprintln!("rlord: {e:#}");
    }
}

fn run_daemon() -> Result<()> {
    // Pin both at startup, before any rebuild or reinstall.
    rlord_daemon::build_id();
    rlord_daemon::exe_id();
    // One daemon per user: a second one (a lost autostart race, or launchd
    // starting its job while an autostarted daemon runs) exits 0 quietly, so
    // launchd's KeepAlive doesn't respawn it in a loop. Checked before the
    // runtime exists, so a loser doesn't spin up worker threads to exit.
    let Some(_lock) = autostart::acquire_instance_lock()? else {
        eprintln!("rlord: another daemon is already running");
        return Ok(());
    };
    let rt = tokio::runtime::Runtime::new()?;
    let r = rt.block_on(async {
        let db = rlord_core::db::Db::open(&paths::db_path())?;
        let daemon = Daemon::new(db, Config::load()?);
        daemon.startup()?;
        // Load every live session's sub-agent data in the background.
        rlord_daemon::warm::start(&daemon);
        rlord_daemon::summary::start(&daemon);
        // Restart an idle overlord on a fresh conversation (context hygiene).
        rlord_daemon::overlord::start_fresh_keeper(&daemon);
        // Remove rlord's read-only trees of repos that are gone or unused.
        rlord_daemon::repo_latest::start_sweeper(&daemon);
        rlord_daemon::remote::start(&daemon);
        // Sessions whose agent isn't reporting through hooks (Codex, untrusted).
        rlord_daemon::discover::start(&daemon);
        // Messages for sessions, held until their turn is over.
        rlord_daemon::send::start(&daemon);
        rlord_daemon::limits::start(&daemon);
        // OpenCode sessions: status from its background server's events.
        rlord_daemon::opencode::start(&daemon);
        // Where each agent's command is (may ask the login shell): known
        // before the first launch needs it.
        std::thread::spawn(rlord_core::agents::install::warm);
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
    });
    // Exit now: don't wait on in-flight blocking work (a stuck tmux call
    // would otherwise hold a restart hostage).
    rt.shutdown_timeout(std::time::Duration::from_millis(300));
    r
}

/// Fast path for agent hooks: read payload from stdin, forward to the daemon,
/// spool on failure. Exits 0 in all cases — a hook must never break the agent.
fn hook_emit(agent: &str) -> Result<()> {
    let instance = std::env::var("RLORD_SESSION_ID")
        .ok()
        .filter(|v| !v.is_empty());
    // No session named: an agent rlord doesn't run, unless it is a thread
    // on Codex's background server, whose hooks run there without the
    // pane's environment. Only threads the gate lists go on (one small
    // read; nothing for every other Codex thread on the machine).
    let gate = match &instance {
        Some(_) => None,
        None if agent == rlord_core::agents::Agent::Codex.name() => {
            match rlord_core::agents::codex_gate::read_from(&rlord_core::agents::codex_gate::path())
            {
                Some(g) if !g.is_empty() => Some(g),
                _ => return Ok(()),
            }
        }
        None => return Ok(()), // not an rlord-managed session
    };
    // Only what rlord uses goes on (not a tool's whole input and output);
    // stdin cut off at the limit still yields the event's name.
    let mut raw = Vec::new();
    std::io::stdin().take(1 << 20).read_to_end(&mut raw).ok();
    let payload = rlord_daemon::slim_hook_payload(&String::from_utf8_lossy(&raw));
    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let params = match gate {
        // (The daemon's TMUX_PANE is whichever pane started it: not sent.)
        Some(g) => {
            if !g.admits(&event, &payload) {
                return Ok(());
            }
            json!({"instance": null, "by": "thread", "agent": agent, "event": event,
                   "payload": payload})
        }
        None => {
            // Which pane this agent runs in: the daemon accepts a session's
            // hooks only from its own pane (a forked or background copy of
            // the agent inherits RLORD_SESSION_ID but isn't in it).
            let pane = std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty());
            let socket = std::env::var("TMUX")
                .ok()
                .and_then(|t| t.split(',').next().map(str::to_string));
            json!({"instance": instance, "agent": agent, "event": event,
                   "payload": payload, "pane": pane, "tmux": socket})
        }
    };
    // Bounded: a stalled daemon never holds up the agent's tool call.
    let (sent, reply) = rlord_daemon::send_hook_event_for_reply(&paths::socket_path(), &params);
    if sent == rlord_daemon::HookSent::Unreachable {
        spool_hook_event(&params).ok();
    }
    // The daemon's answer for the agent (the session's background at
    // SessionStart / UserPromptSubmit): printed as the hook's output. Slow
    // or unreachable: nothing, as before.
    // Acknowledged only once printed: the daemon then counts it handed
    // over (unacknowledged, the next prompt gets it again).
    if let Some(r) = reply {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{}", r.output)
            .and_then(|_| out.flush())
            .is_ok()
        {
            if let Some(ack) = &r.ack {
                rlord_daemon::send_hook_ack(&paths::socket_path(), ack);
            }
        }
    }
    Ok(())
}

/// `nav home`: switch this client to the TUI's session, or detach when the
/// TUI isn't inside tmux (no home recorded / home gone).
fn nav_home() -> Result<()> {
    // Overlay up on this terminal → Option-D just closes it.
    if let Some(client) = tmux::current_client() {
        // No autostart: with no daemon there's no overlay, and a key press
        // must not stall while one starts.
        if call_running("ui.overlay.close_on", json!({"client": client}))
            .is_ok_and(|r| r["closed"] == true)
        {
            return Ok(());
        }
    }
    match tmux::home() {
        Some(h) if tmux::has_session(&h) => tmux::switch_client(&h),
        _ => tmux::detach_client(),
    }
}

/// `nav overlord` (Option-O): in the TUI, toggle the overlord overlay;
/// anywhere else, bring the user to the TUI with it open.
fn nav_overlord() -> Result<()> {
    // Say where the key was pressed (run-shell runs in that client's
    // context), rather than making the daemon guess which terminal it was.
    let r = call(
        "ui.overlord",
        json!({"client": tmux::current_client(), "from": tmux::current_session()}),
    )?;
    if r["overlord"] == "already here" {
        let back = Config::load().map(|c| c.tmux.back).unwrap_or_default();
        let back = tmux::key_label(if back.trim().is_empty() { "M-d" } else { &back });
        let _ = tmux::display_message(&format!(
            "you're in the overlord · {back}: back to the list"
        ));
    }
    Ok(())
}

/// `rlord reset [--hard]`.
fn reset(hard: bool, yes: bool, rescan: bool) -> Result<()> {
    if hard {
        let running = call("session.list", json!({"archived": "live"}))
            .ok()
            .and_then(|v| v.as_array().cloned())
            .map(|a| a.iter().filter(|s| s["status"] != "stopped").count());
        if !yes {
            use std::io::Write;
            print!(
                "This stops {} running session(s) — conversations are kept; Enter in the list \
                 resumes each. Continue? [y/N] ",
                running
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "all".into())
            );
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !matches!(answer.trim(), "y" | "Y" | "yes") {
                println!("aborted");
                return Ok(());
            }
        }
        tmux::kill_server()?;
        println!("killed rlord's tmux server");
        let legacy = tmux::cleanup_default_server();
        if !legacy.is_empty() {
            println!(
                "removed {} old rlord session(s) from your default tmux server",
                legacy.len()
            );
        }
    }
    restart_daemon()?;
    let r = call("admin.reset", json!({"rescan": rescan}))?;
    println!(
        "daemon restarted · {} session(s) running, {} stopped (resumable)",
        r["running"], r["stopped"]
    );
    if hard && !tmux::inside_rlord() {
        println!("run `rlord` to open it");
    }
    Ok(())
}

/// Restart the daemon (via launchd when its agent owns it).
fn restart_daemon() -> Result<()> {
    autostart::restart_daemon(&paths::socket_path())
}

/// `rlord mcp`: a minimal MCP server over stdio (JSON-RPC, one message per
/// line). Protocol handling lives here; tools live in the daemon
/// (`mcp.tools` / `mcp.call`), so this stays a thin, stateless bridge.
/// Without `overlord` it is read-only (the daemon serves and accepts only
/// `mcp::READ_ONLY_TOOLS`) and never starts a daemon.
fn mcp_stdio(overlord: bool, socket: Option<std::path::PathBuf>) -> Result<()> {
    use std::io::{BufRead, Write};
    let caller = std::env::var("RLORD_SESSION_ID")
        .ok()
        .filter(|v| !v.is_empty());
    // The daemon that wrote the overlord's config: only that one, never one
    // started here from whatever environment this process has.
    let call = |method: &str, params: Value| match &socket {
        Some(s) => client_call(s, method, params),
        None if overlord => call(method, params),
        None => call_running(method, params),
    };
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // notification (e.g. notifications/initialized)
        };
        let method = msg["method"].as_str().unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let result: std::result::Result<Value, (i64, String)> = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "rlord", "version": env!("CARGO_PKG_VERSION")},
                "instructions": if overlord {
                    "Robot Overlord control surface: inspect agent sessions and drive the rlord UI (navigate, set the list view, propose or create sessions)."
                } else {
                    "Robot Overlord look-ups (read-only): list rlord's agent sessions, read their screens, search and read their history."
                },
            })),
            "ping" => Ok(json!({})),
            "tools/list" => call("mcp.tools", json!({"overlord": overlord}))
                .map(|t| json!({"tools": t}))
                .map_err(|e| (-32603, format!("{e:#}"))),
            "tools/call" => {
                let r = call(
                    "mcp.call",
                    json!({"name": params["name"], "arguments": params.get("arguments").cloned().unwrap_or(json!({})),
                           "caller": caller, "overlord": overlord}),
                );
                Ok(match r {
                    Ok(v) => json!({"content": [{"type": "text",
                        "text": serde_json::to_string_pretty(&v).unwrap_or_default()}]}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}],
                                     "isError": true}),
                })
            }
            other => Err((-32601, format!("method not found: {other}"))),
        };
        let resp = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        writeln!(out, "{resp}")?;
        out.flush()?;
    }
    Ok(())
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
    // Inside rlord's tmux already → switch the client instead of nesting.
    let err = if tmux::inside_rlord() {
        tmux::command()
            .args(["switch-client", "-t", &target])
            .exec()
    } else {
        tmux::command()
            .args(["attach-session", "-t", &target])
            .exec()
    };
    Err(err).context("exec tmux failed")
}

/// What stopping a session on Codex's background server did to its
/// running turn, if it had one.
fn print_turn(r: &Value) {
    match r["turn"].as_str() {
        Some("interrupted") => println!("  its running turn was interrupted first"),
        Some(_) => println!(
            "  {}",
            r["note"]
                .as_str()
                .unwrap_or("its turn may still be running")
        ),
        None => {}
    }
}

/// `rlord agents`: one block per coding agent.
fn print_coding_agents(v: &Value) {
    for a in v["agents"].as_array().into_iter().flatten() {
        let yes_no = |k: &str| match a[k].as_bool() {
            Some(true) => "yes",
            Some(false) => "no",
            None => "-",
        };
        let state = if a["installed"] == true {
            a["version"].as_str().unwrap_or("installed").to_string()
        } else {
            "not installed".to_string()
        };
        println!(
            "{:<12} {state}{}",
            a["label"].as_str().unwrap_or("?"),
            if a["supported"] == false {
                "  (rlord support coming)"
            } else {
                ""
            }
        );
        if let Some(d) = a["daemon_version"].as_str() {
            println!("  its background server runs {d} (sessions on it run that version)");
        }
        if a["hooks_outdated"] == true {
            println!("  rlord hooks: out of date — run `rlord install-hooks` again");
        } else if let Some(why) = a["too_old"].as_str() {
            println!("  {why}");
        } else if a["installed"] == true && a["supported"] == true && a["hooks_installed"].is_null()
        {
            println!("  no hooks needed: rlord follows its background server's events");
        } else if a["installed"] == true && a["supported"] == true {
            let trusted = match a["hooks_trusted"].as_bool() {
                Some(_) => format!(" · trusted in Codex: {}", yes_no("hooks_trusted")),
                None => String::new(),
            };
            println!("  rlord hooks: {}{trusted}", yes_no("hooks_installed"));
        }
        if let Some(cmd) = a["install"]["command"].as_str() {
            let what = if a["installed"] == true {
                "upgrade"
            } else {
                "install"
            };
            println!("  to {what} it, run:  {cmd}");
            for alt in a["install"]["alternatives"]
                .as_array()
                .into_iter()
                .flatten()
            {
                println!(
                    "    or ({}):  {}",
                    alt["via"].as_str().unwrap_or("?"),
                    alt["command"].as_str().unwrap_or("?")
                );
            }
            println!("    more: {}", a["install"]["docs"].as_str().unwrap_or(""));
        }
        if let Some(next) = a["next"].as_str().filter(|_| a["installed"] == true) {
            println!("  next: {next}");
        }
    }
    println!(
        "\nrlord never installs an agent for you. After installing Claude Code or Codex: \
         rlord install-hooks"
    );
}

fn print_agents(list: &Value) {
    let Some(rows) = list.as_array() else {
        return;
    };
    if rows.is_empty() {
        println!("no sub-agents");
        return;
    }
    for a in rows {
        let started = a["started_at"].as_i64();
        let finished = a["finished_at"].as_i64();
        let state = match (started, finished) {
            (Some(s), Some(f)) => format!("done in {}s", ((f - s) as f64 / 1000.0).round()),
            (Some(s), None) => format!("running {}", rel_age(Some(s))),
            _ => "unknown".into(),
        };
        println!(
            "    {:<14} {:<40} {:<14} {:>6} tok  {}",
            a["agent_type"].as_str().unwrap_or("?"),
            truncate(a["description"].as_str().unwrap_or(""), 40),
            state,
            a["output_tokens"].as_i64().unwrap_or(0) + a["input_tokens"].as_i64().unwrap_or(0),
            if a["background"].as_bool().unwrap_or(false) {
                "bg"
            } else {
                ""
            }
        );
        let result = a["result"].as_str().unwrap_or("").trim();
        if !result.is_empty() {
            println!(
                "      ↳ {}",
                truncate(result.lines().next().unwrap_or(""), 100)
            );
        }
    }
}

fn compact(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Object(m) if m.is_empty() => String::new(),
        other => other.to_string(),
    }
}

fn print_session_table(list: &Value, usage: rlord_core::usage::UsageMetric) {
    let Some(rows) = list.as_array() else {
        println!("{list}");
        return;
    };
    if rows.is_empty() {
        println!("no sessions");
        return;
    }
    println!(
        "{:<8} {:<24} {:<16} {:>7}  {:>5}  {:<16} PROJECT",
        "ID", "NAME", "STATUS", "UPDATED", "USAGE", "TAGS"
    );
    for s in rows {
        let get = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("-");
        let id = get("id");
        let pin = if s["pinned"].as_bool().unwrap_or(false) {
            "*"
        } else {
            " "
        };
        let used = match rlord_core::usage::Usage::from_json(&s["usage"]) {
            Some(u) => u.cell(usage),
            // (Still being read by the daemon's warm-up.)
            None if s["usage_loading"] == true => format!("{:>4}", "…"),
            None => rlord_core::usage::cell(None, usage),
        };
        println!(
            "{:<8}{}{:<24} {:<16} {:>7}  {:>5}  {:<16} {}",
            &id[..id.len().min(6)],
            pin,
            truncate(get("name"), 23),
            // Stopped on a usage limit: that, and until when.
            match s["limit"]["until"].as_str() {
                Some(_) if s["limit"]["reset"] == true => "limit reset".to_string(),
                Some(until) => truncate(&format!("till {until}"), 16),
                None => rlord_core::types::Status::label_of(get("status")).to_string(),
            },
            rel_age(
                [
                    "last_agent_msg_at",
                    "last_user_msg_at",
                    "last_rlord_msg_at",
                    "last_state_change_at",
                    "created_at"
                ]
                .iter()
                .filter_map(|k| s[*k].as_i64())
                .max()
            ),
            used,
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

/// How rlord was installed when it wasn't by get.sh or `rlord update`
/// (which own ~/.local/bin), and how to update it that way. `exe` is the
/// canonical path of the running binary.
fn installed_by(exe: &std::path::Path) -> Option<&'static str> {
    let p = exe.to_string_lossy();
    if p.contains("/Cellar/") {
        Some("Homebrew: update it with `brew upgrade rlord`")
    } else if p.starts_with("/usr/bin/") {
        Some(
            "a .deb or .rpm package: install the newer release's package the same way \
             (https://github.com/robot-lord/robot-overlord/releases)",
        )
    } else if p.contains("/.cargo/bin/") {
        Some("cargo: update it with `cargo install robot-overlord`")
    } else {
        None
    }
}

/// What `rlord fork` prints: the fork, then each warning on its own line
/// (`rlord_core::forked`, as the TUI says it).
fn fork_lines(r: &Value) -> String {
    let f = rlord_core::forked::forked(r, true);
    let mut out = format!("{}\n", f.headline);
    for w in f.warnings {
        out.push_str(&format!("  note: {w}\n"));
    }
    out
}

/// `rlord show`'s lineage: what a session was forked from, and its forks.
fn lineage_lines(s: &Value) -> String {
    let mut out = String::new();
    if let Some(from) = s["forked_from"].as_str() {
        let name = s["forked_from_name"].as_str().unwrap_or("(gone)");
        let when = s["created_at"]
            .as_i64()
            .map(|v| format!(" · {}", fmt_ts(v)))
            .unwrap_or_default();
        out.push_str(&format!("  forked from {name} ({from}){when}\n"));
    }
    let forks: Vec<&str> = s["forks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["name"].as_str())
        .collect();
    if !forks.is_empty() {
        out.push_str(&format!("  forks      {}\n", forks.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn latest_prune_is_the_current_repo_unless_all() {
        use serde_json::json;
        let cwd = std::path::Path::new("/w/app/sub");
        assert_eq!(
            super::prune_params(None, false, cwd),
            json!({"repo": "/w/app/sub/."})
        );
        assert_eq!(
            super::prune_params(Some("../other".into()), false, cwd),
            json!({"repo": "/w/app/sub/../other"})
        );
        assert_eq!(
            super::prune_params(Some("/r".into()), false, cwd),
            json!({"repo": "/r"})
        );
        assert_eq!(super::prune_params(None, true, cwd), json!({"all": true}));
    }

    use super::*;
    use std::path::Path;

    #[test]
    fn fork_parses_and_prints() {
        let cli = Cli::try_parse_from([
            "rlord",
            "fork",
            "api",
            "try",
            "a",
            "queue",
            "--name",
            "q",
            "--worktree",
            "-a",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Fork {
                r#ref,
                prompt,
                name,
                worktree,
                attach,
                branch,
                ..
            }) => {
                assert_eq!(r#ref, "api");
                assert_eq!(prompt.join(" "), "try a queue");
                assert_eq!(name.as_deref(), Some("q"));
                assert!(worktree && attach && branch.is_none());
            }
            _ => panic!("not a fork"),
        }
        // A branch only goes with a new worktree.
        assert!(Cli::try_parse_from(["rlord", "fork", "api", "--branch", "b"]).is_err());
        let r = json!({"id": "01f", "name": "api-fork", "forked_from_name": "api",
                       "repo_path": "/r", "workspace_path": "/r", "branch": "main",
                       "warnings": ["it shares its folder with api (working)"]});
        assert_eq!(
            fork_lines(&r),
            "forked api → api-fork (01f)\n  note: it shares its folder with api (working)\n"
        );
        // In its parent's worktree: nothing to say about it.
        let r = json!({"id": "01f", "name": "x", "forked_from_name": "api", "repo_path": "/r",
                       "workspace_path": "/r/.worktrees/api", "branch": "api",
                       "new_worktree": false, "warnings": []});
        assert_eq!(fork_lines(&r), "forked api → x (01f)\n");
        // A worktree of its own, in the parent's subfolder of it.
        let r = json!({"id": "01f", "name": "x", "forked_from_name": "api", "repo_path": "/r",
                       "workspace_path": "/r/.worktrees/x", "project_path": "/r/.worktrees/x/web",
                       "branch": "x", "new_worktree": true, "warnings": []});
        assert_eq!(
            fork_lines(&r),
            "forked api → x (01f) in a new worktree /r/.worktrees/x/web [x]\n"
        );
        let s = json!({"forked_from": "01p", "forked_from_name": "api",
                       "forks": [{"id": "01g", "name": "x-fork"}]});
        assert_eq!(
            lineage_lines(&s),
            "  forked from api (01p)\n  forks      x-fork\n"
        );
        assert_eq!(lineage_lines(&json!({"forks": []})), "");
    }

    #[test]
    fn setup_prompt_helpers() {
        assert_eq!(
            roots_question("/nope/ko", &["~".into()], "/nope/code", true),
            "The repo finder scans /nope/ko, ~ now. y: remove repo_roots (~) from the config, \
             so it scans only /nope/code. [Y/n] "
        );
        assert_eq!(roots_edit(&json!("follow"), false), "follow");
        assert_eq!(roots_edit(&json!("follow"), true), "leave");
        assert_eq!(roots_edit(&json!("leave"), false), "leave");
        assert_eq!(roots_edit(&Value::Null, false), "leave");
        assert_eq!(pick_folder("", Some("~/ko")).as_deref(), Some("~/ko"));
        assert_eq!(pick_folder("  ~/x ", Some("~/ko")).as_deref(), Some("~/x"));
        assert_eq!(pick_folder("", None), None);
        assert!(yes_no("", true) && !yes_no("", false));
        assert!(yes_no("Y", false) && !yes_no("no", true));
        assert_eq!(absolute_folder("/a/", Path::new("/c")).unwrap(), "/a");
        assert_eq!(absolute_folder("b", Path::new("/c")).unwrap(), "/c/b");
        assert!(absolute_folder("/", Path::new("/c")).is_err());
    }

    #[test]
    fn knows_package_managed_installs() {
        assert!(installed_by(Path::new("/usr/bin/rlord")).is_some_and(|h| h.contains(".deb")));
        assert!(
            installed_by(Path::new("/opt/homebrew/Cellar/rlord/1.0.0/bin/rlord"))
                .is_some_and(|h| h.contains("brew upgrade"))
        );
        assert!(installed_by(Path::new("/home/me/.cargo/bin/rlord")).is_some());
        assert_eq!(installed_by(Path::new("/home/me/.local/bin/rlord")), None);
        assert_eq!(installed_by(Path::new("/usr/local/bin/rlord")), None);
        // A dev symlink resolves into the checkout.
        assert_eq!(
            installed_by(Path::new("/src/rlord/target/release/rlord")),
            None
        );
    }
}
