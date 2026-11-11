# Robot Overlord

Session management and orchestration for AI coding agents — a ground-up
replacement for the parts of [another tool](https://github.com)
we actually want, with full control over the design.

## Why

another tool is a tmux-based TUI for running many AI coding agent sessions. It's
useful — especially its ability to restart a session (even after a reboot) and
land right back in the agent's conversation — but it falls short in ways that
matter to us:

- **Weak session meta-context.** Last-message time / last-updated aren't
  first-class; you can't sort and pivot on them well.
- **Naming isn't sticky/ergonomic enough.**
- **AI-mediated everything.** It was clearly designed around phone-driven,
  fully AI-mediated control loops, which are expensive relative to their value.
  We want a *software product* over the sessions — tracking, sorting,
  organization done in plain code — with AI mediation only where it earns its
  cost.
- **Cross-session agent control is possible but clunky.** "Go tell that other
  agent to do X" / "go get context from that other agent" should be first-class
  and cheap.

## Goals

1. **Session management + meta-context first.** Rich, queryable per-session
   metadata: name (sticky), agent type, project, status, created / last-sent /
   last-activity times, archived state. Sort, filter, group, pivot.
2. **Durable resume.** Restart any session — including across machine reboots —
   retaining full conversation context (leaning on each agent's native resume).
3. **First-class inter-session communication.** An agent in one session can
   message, query, or task another session through a cheap, well-defined
   product surface (not an expensive agent-relay chain).
4. **Organization.** Running vs. archived, groups, and honest views of what
   exists and what state it's in.
5. **Narrow, deep agent support.** Claude Code first-class; OpenAI Codex
   supported; the integration layer generic enough to add others, but we will
   not chase every agent at the expense of doing Claude well.

## Non-goals (for now)

- Phone-first control and always-on AI mediation loops.
- Support for every agent CLI in existence.

## Shape (decided)

- **Rust**, terminal-first (TUI), **tmux** as the process substrate.
- A resident daemon (`rlordd`) owns all state and behavior; CLI, TUI, and a
  **first-class MCP server** are thin clients over one Unix-socket API — this
  is the fix for other-tool's cold-start CLI sluggishness.

See `docs/architecture.md` for the full plan and phased milestones.

## Quickstart

```bash
ln -sfn "$PWD/target/debug/rlord" ~/.local/bin/rlord   # dev install: tracks every build
scripts/dev-daemon.sh                        # build + (re)start the daemon (dev loop)
./target/debug/rlord claude install-hooks    # merge-adds hooks to ~/.claude/settings.json
rlord                                        # TUI dashboard (default command)
rlord new --project ~/src/myrepo "first prompt"   # launch a Claude session in tmux
rlord ls --sort user --tag wip               # query: status/tag/project/name filters, 5 sorts
rlord attach <ref>                           # ref = id, id prefix, name, or name prefix
rlord stop <ref> && rlord restart <ref>      # resume with full conversation context
rlord rename/tag/untag/pin/archive/log <ref> # meta-context
```

For a persistent daemon, run `scripts/install-launchd.sh` (launchd user agent;
`uninstall` removes it). `scripts/dev-daemon.sh` then restarts it via launchd.

## The overlord

The primary way to drive rlord is the **overlord**: one long-running agent
session (your tool and model, `opus` by default) that you talk to in order to
run the fleet. It has rlord's MCP tools, pre-allowed, and they act on the real
UI you're looking at:

- launch sessions: open the new-session form **pre-filled** for you to confirm
  (`propose_session`), or create one directly and send you there or not
  (`create_session`, `go`);
- search and jump: switch your terminal to a session (`goto_session`) or to the
  list (`show_list`);
- shape the list: set the current filter/sort, and save it as the default view
  (`set_list_view`);
- inspect and edit: sessions' meta-context, recent events, live screen, names,
  tags, notes, lifecycle.

**Option-O** opens it as an overlay floating over whatever you're looking at:
an agent session or the list. Option-O again, or Option-D, closes it. `enter` on the magenta ◆ row at the top of the list opens the
overlord full screen, like any session. Its status bar turns magenta
(`◆ OVERLORD`), Option-O does nothing there, and Option-D goes back to the
list. The overlay is a tmux
popup running rlord's own renderer of the overlord's live pane, so everything
you type goes to the overlord. When the overlord moves you somewhere (a
session, the list, a pre-filled form) it closes itself first. `rlord` always
runs its TUI inside tmux (it starts a `rlord` tmux session if you're outside
one).

Quitting the list takes **Shift-Q** plus a confirmation. `q` and Ctrl-C only show
that hint.

First start with no overlord shows a setup dialog where everything is defaulted,
so `enter` launches it. On every start rlord checks the overlord's model: it
reads what the agent actually ran from its transcript and compares it with a
per-tool table of latest models (`crates/core/src/models.rs`). If you're on an
older Opus, a dialog offers to switch and restart, and the conversation resumes.
`rlord overlord status|model <m>|restart|open` does the same from the CLI.

`enter` on a stopped session resumes it and attaches.

The list's UPDATED column (and the default sort) is the session's last
update: the latest message either way, or status change. The details card
breaks it down.

Add standing instructions in `~/.config/robot-overlord/overlord.md`. They're
appended to the overlord's built-in instructions, which are written to its
folder as `CLAUDE.md`, `AGENTS.md` and `GEMINI.md` on each start. Your usual
global files (`~/.claude/CLAUDE.md` and friends) still apply.

## Creating sessions

Every session is created with the defaults under `[session]` in your config:
the coding tool (`claude`), the **model** (`--model`, default `opus`; the
picker offers `models`), and **workspace = true** — each session gets its own
git worktree at `<repo>/.worktrees/<branch>` (other-tool's layout; the dir is
`workspace_dir`, excluded via `.git/info/exclude`, never touching your
`.gitignore`). Set `default_repo` and the form opens on it.

In the TUI, `n` asks the **overlord** to launch a session. Write what you
want, as long as you like, and adjust the defaults (repo, or "let the
overlord pick"; model; worktree; tags, prefilled with the group you're
viewing). `Enter` adds a new line in the
request; `shift-enter` (or `ctrl-s`) sends it. The overlord names the session and launches it.
It may ask a quick question or do a little groundwork first (its overlay
opens so you see it); say "launch as is" in the request to skip that. The
prompt it writes starts with your request word for word, plus any context
it found. From
anywhere: `rlord overlord ask "…"` (or pipe text in) sends the overlord any
message.

`N` opens the **manual** form, which launches directly, with the repo preselected (your default repo,
else the repo around the current directory). `shift-enter`, `ctrl-s` or `Enter` on
Launch launches; `Enter` on the repo line opens the repo list — default first (★), then
every repo you've used, most recent first — plus "find another repo…" (a
filter-as-you-type finder over every git repo under `repo_roots`, default
`["~"]` to depth 3, MRU first; a typed path that is a repo is offered too)
and "no repo — plain folder…". `Tab`/`↑↓` move between fields (name, model,
workspace toggle, branch — auto-derived from the name until you edit it —
first prompt). From the CLI: `rlord new [--repo R | --project DIR]
[--no-workspace] [--branch B] [--model M] [--name N] [prompt…]`;
`rlord repos` lists known repos.

## The right-hand side: preview + details

The preview is the session's live pane, in colour (captured with ANSI
attributes), refreshed every `preview_refresh_ms`; `l` swaps it for the event
log. Under it sits the **details** card — agent/model/status, repo and branch,
working dir, the four timestamps, tags and group — plus two fields you write:
a one-line **description** (`D`, or `rlord describe <ref> <text>`) and
free-form **notes** (`E` opens `$EDITOR`; `rlord notes <ref> [--set|--file]`).
`rlord show <ref>` prints the same card. The list always wins on space: it
keeps its width and the preview drops out first when the terminal is narrow.

**Sub-agents** are tracked from outside the pane: Claude Code's `SubagentStart`
/ `SubagentStop` hooks plus the per-agent `meta.json` and transcript Claude
writes under `~/.claude/projects/<slug>/<session>/subagents/`. The details
card lists each one (type, description, running/done + duration, result line,
tokens); the session list shows `⚙N` while any are running; `rlord agents
<ref>` prints them. Sessions that ran before the hooks existed are backfilled
from disk on first view.

## rlord's tmux

rlord runs its own private tmux server (`tmux -L rlord`) with a generated
config: no status bar, no prefix key, mouse on. The only keys it binds are
Option-D (to the list) and Option-O (the overlord). Your personal tmux is never
touched. Starting `rlord` from inside your own tmux nests cleanly. Put your
own tweaks for rlord's tmux in `~/.local/share/robot-overlord/tmux.local.conf`.

**Your tmux, per session.** `rlord tmux import` turns your tmux config into a
profile that agent sessions use: your prefix, key bindings, status bar,
splits and windows. The list page and the overlord keep rlord's plain tmux,
and one session's profile never affects another.

- The import asks tmux what your config produced, by loading it on a scratch
  server. So `if-shell`, `source-file` and plugins all come through.
- It prints a report of everything it left out, and why.
- Left out: rlord's own keys (Option-D/O stay rlord's), `kill-server`,
  session-restore plugins (resurrect, continuum), and server-wide options
  that would change the list page.
- `--dry-run` shows the report and writes nothing. `--remove` deletes the
  profile. Re-run the import after you change your config.
- The profile is on for agent sessions by default (`[tmux] profile`).
- Per session: `rlord tmux profile on|off|toggle|default [REF]`.
- rlord tracks the agent's pane, so splits never confuse the preview or
  status.

**When it gets stuck:** `rlord reset` restarts the daemon, closes overlays,
resyncs session state, reloads the tmux config and keys, and restarts the
list. Agent sessions keep running. `rlord reset --hard` also kills rlord's
whole tmux server. Every session stops, but no conversation is lost: `enter`
in the list resumes each one. It also removes rlord sessions and bindings that
older versions left on your default tmux server.

## Sessions and groups

The list has two tabs (`Tab` switches):

- **sessions**: every session, flat.
- **groups**: **tags are the groups**.
  - A column on the left lists each tag with its session count, then
    "untagged". The selected group's sessions sit next to it.
  - `←`/`→` move between the column and the sessions. `enter` on a group
    opens it.
  - `R` on a group renames that tag on every session.
  - A session with two tags appears in both groups.
- **Filter bar:** above both tabs, a line always shows the current filter,
  sort and live/archived, with the keys that change them.
- **Editing tags:** `#` edits the selected session's tags (space-separated).
- **The overlord** is expected to do most of the organizing:
  - `organize` tags, renames and describes many sessions in one call;
  - `list_groups` lists groups and counts;
  - `rename_group` renames a group;
  - `set_list_view` opens a tab or group (`tab`, `group`) and can save it as
    your default.

## Navigation and keys

The TUI is the hub. `enter` on a session switches your tmux client into its
pane; from inside **any** agent pane, **Option-D** (`M-d`) brings you back to the
TUI. It's a home-row key, because you press it constantly. Stopped sessions
resume on `enter`. Quit the list with Shift-Q, then confirm.

Jumping is search-driven, not cycling: `/` filters as you type and `Enter`
attaches to the top match (`Tab` keeps the filter, ↑/↓ move within matches);
digits `1`–`9` attach to the Nth listed session. The search box and
`rlord ls <query>` share one syntax — free words match name/project/tag,
and `status:waiting` `tag:wip` `proj:wait2` `agent:claude`
`sort:user` set typed filters. `o` cycles sorts, `O` reverses. (macOS: make
Option send Alt, e.g. kitty `macos_option_as_alt both`.) Option-D and
Option-O are bindings in rlord's own tmux server (`rlord tmux bind|unbind`).

Everything is configurable in `~/.config/robot-overlord/config.toml`
(`rlord config init` writes a commented template; `rlord config show` prints
the effective values): the layout (`side` — list left, live preview right — or
`stack`), the preview refresh rate, every TUI key under `[keys]`, and the tmux
bindings under `[tmux]` (set one to `""` to disable it).

Sessions survive reboots: the minted Claude session id lives in SQLite, so
`rlord restart` brings the conversation back via `claude --resume` even
after the tmux server is gone.

## Docs

- `docs/architecture.md` — the architecture plan: components, data model,
  adapter contract, status engine, messaging design, MCP tool surface, and
  the phase-by-phase build plan.
- `docs/research-other-tool.md` — baseline research: how another tool actually
  works, which of its features are its own machinery vs. delegation to agent
  CLIs, and what that implies for our design.
