# rlord user guide

The README covers what rlord is and how to install it. This is the
reference: how each part works and what you can set. Keys are written the
Mac way (Option-O); on Linux that's Alt-O. Every key can be changed (`:keys`,
or ask the overlord).

Contents: [The overlord](#the-overlord) · [Creating sessions](#creating-sessions) ·
[The right-hand side](#the-right-hand-side-preview--details) ·
[Bringing in existing sessions](#bringing-in-existing-sessions) ·
[Archived sessions and past work](#archived-sessions-and-past-work) ·
[Remote Control](#remote-control-from-your-phone) · [rlord's tmux](#rlords-tmux) ·
[Sessions and groups](#sessions-and-groups) · [Themes](#themes) ·
[Navigation and keys](#navigation-and-keys) · [Updating](#updating) ·
[The daemon](#the-daemon)

## The overlord

The primary way to drive rlord is the **overlord**: one long-running agent
session with rlord's MCP tools (pre-allowed; they act on the real UI you're
looking at). **Option-O** opens it as an overlay over whatever you're
looking at; say what you want.

It runs on any of the agents rlord runs: Claude Code, Codex or OpenCode
(`[overlord] agent`, or the setup dialog's first row). See "Which agent
runs the overlord" below for what differs.

- **A strong model at medium effort** (`opus` / `medium` by default on
  Claude Code; Codex's own default model at `medium`; OpenCode's default model, which
  has no effort setting). It does
  light work itself, in seconds: find and jump to sessions (`goto_session`,
  `show_list`), shape the list (`set_list_view`, groups), edit names, tags,
  notes and descriptions, answer from rlord's data ("what's waiting on
  me?"). Anything heavier it delegates: routine passes over many sessions
  go to cheaper subagents (tidy, import), research goes to a subagent, and
  work in a repo becomes a new session. An `[overlord] effort` you set
  yourself wins over the default (`low` for quicker answers, `high` for
  deeper ones).
- **Starting a session**: just tell the overlord ("start a session in ai to
  fix the flaky login tests"). The list's `n` form is a thin wrapper around
  the same thing, with optional hints (describe → confirm via
  `confirm_new_session`); `ctrl-e` in it opens the description in your
  `$EDITOR` (e.g. vim).
- **It starts fresh after sitting idle.** After 15 idle minutes (while
  you're not looking at it, and not while its last turn asked you
  something) rlord restarts it on a fresh conversation, so its context
  doesn't grow without end (`[overlord] fresh_after_mins`; 0 = never).
  Nothing is carried over: when you refer to something from before, it
  looks it up in its own earlier conversations (`recall`).
- **You can see that.** The overlord's row and its overlay's title say when
  the current conversation started and how many came before ("fresh 2h ago ·
  14 earlier"). With its row selected, the details pane lists the earlier
  conversations (when each ended, its turns, how it ended: an idle restart,
  your own `/clear`, a restart; its first prompt). `[` and `]` (or a click)
  show one in the preview; `enter` on one continues it (the overlord goes
  back to that conversation, and the current one joins the list); `esc`
  returns to the live pane.

Inside the overlay, Option-O (or Option-D) closes it. The overlord sits at
the top of the list (◆ overlord); `enter` opens it full screen with its own
lavender bar. When it moves you somewhere (a session, the list, a form) it
closes itself first. `rlord` always runs its TUI inside tmux (it starts a
`rlord` tmux session if you're outside one).

Quitting the list takes **Shift-Q** plus a confirmation. `q` and Ctrl-C only show
that hint.

First start with no overlord shows a setup dialog, everything defaulted, so
`enter` launches it. Its rows can be changed with ←/→: the **agent** (the
installed ones that can run the overlord; changing it resets the next two
to that agent's defaults), the **model**, the **effort**, and **Remote
Control**. What you pick is saved to `config.toml` under `[overlord]`, so
later starts reuse it; change it there any time. On every start rlord
checks the overlord's model: it reads what the agent actually ran from its
transcript and compares it with a per-tool table of latest models
(`crates/core/src/models.rs`). If it is on an older model, a dialog offers
to switch and restart, and the conversation resumes.
`rlord overlord status|model <m>|restart|open` does the same from the CLI.

### Which agent runs the overlord

Each agent gets the same thing in its own terms: rlord's tools without a
prompt, reading without a prompt, and a prompt for anything that writes or
runs a command.

| | Claude Code | Codex | OpenCode |
|---|---|---|---|
| rlord's tools | `--mcp-config` + `--allowedTools` | `-c mcp_servers.rlord.*`, approved by default | `opencode.json` in its folder (`mcp.servers.rlord`) |
| Everything else | asks (reading is allowed) | read-only sandbox; writes and anything outside it ask | asks (reading is allowed) |
| Instructions | `CLAUDE.md` | `AGENTS.md` | `AGENTS.md` |
| Subagents (tidy, import) | `.claude/agents` | `.codex/agents` | `.opencode/agents` |
| Effort | `--effort` | `model_reasoning_effort` | none |
| Remote Control | yes | no | no |
| Messaging another session | yes (Claude sessions) | no: it takes you there instead | no: it takes you there instead |

- Nothing is written to your own agent config: Codex gets its settings as
  command-line overrides (its folder is trusted for that run only), and
  OpenCode reads the config file in the overlord's own folder.
- **Codex** runs its hooks only once you've trusted them (`/hooks`); without
  that the overlord still works, and its status comes from Codex's own
  files a little later. Codex reports a conversation only with its first
  prompt, so a just-started overlord shows "your turn" once its screen is
  up.
- **OpenCode** runs the overlord's turns in its shared background server,
  like any OpenCode session.
- The agent is chosen when the overlord is first set up. An existing
  overlord keeps its agent: `[overlord] agent` applies only while there is
  none yet. Moving an existing overlord to another agent isn't built.

**Coming from the two-session overlord** (a front desk plus a "mastermind",
before 0.21): nothing to do. The mastermind's conversation carries on as
the overlord's, and the old front desk is stopped and archived (as
`overlord-front-desk`). `mastermind_model`, `mastermind_effort` and
`mastermind_fresh_after_mins` in `config.toml` still work as the overlord's
`model`, `effort` and `fresh_after_mins`; a `model` or `effort` you set
yourself wins over them. With no effort set, it runs at the default
(medium).
`[tmux] mastermind` is ignored.

`enter` on a stopped session resumes it and attaches.

Sorts (`o` cycles them, `O` reverses; the time column follows the sort):

| Sort | Newest first by |
|---|---|
| **last message** (default) | the last message from you or the agent (a resume or a status flip alone doesn't count) |
| **last prompt** | the last message *you* sent: what you were just working with |
| **status change** | the last time it started, stopped, finished or needed you |
| **created** | when it started |
| **name** | A–Z |
| **usage** | most used first, by what the Usage column shows |

The details card breaks the times down.

**The Usage column** shows how much each session has used, in one of three
measures:

- **turns** (default): prompts you typed;
- **tokens**: tokens the agent took in new or wrote (cache re-reads don't
  count);
- **context**: how full its context window is, e.g. ` 42%`. `–` when
  rlord doesn't know the model's window.

Set the default with `usage = "turns" | "tokens" | "context"` in
`config.toml`; switch it for now with `:usage turns`, `:usage tokens` or
`:usage context`, or ask the overlord. Numbers keep to four characters
(`999`, `1.2k`, `45k`, `3.1M`), so the column doesn't jump. Clicking its
header sorts by it (again: reverses), and the usage sort follows whichever
measure the column shows. The details card always has all three. In a
narrow list the Usage column goes before Groups does. rlord reads these
from the agents' own transcripts (Claude Code and Codex JSONL, OpenCode's
database), only what was appended since it last looked; no model calls.
`rlord ls` has a USAGE column too (`--usage tokens`, `--sort usage`).

Add standing instructions in `~/.config/robot-overlord/overlord.md`. They're
appended to the overlord's built-in instructions, which are written to its
folder as `CLAUDE.md`, `AGENTS.md` and `GEMINI.md` on each start. Your usual
global files (`~/.claude/CLAUDE.md` and friends) still apply.

## Creating sessions

Every session is created with the defaults under `[session]` in your config:
the coding tool (`claude`), the **model** (`--model`, default `opus`; the
picker offers `models`), and **workspace = true** — each session gets its own
git worktree at `<repo>/.worktrees/<branch>` (the dir is
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

## Codex sessions

A session runs on Claude Code, the Codex CLI or OpenCode (next section).
Pick per session: the
`Agent` hint in the new-session form, "start a Codex session to …" to the
overlord, or `rlord new --agent codex`. `[session] agent` sets the default,
`codex_model` Codex's default model (empty: Codex's own); the model picker
offers Codex's own model list.

No Codex yet? `rlord agents` (or asking the overlord, or picking Codex in
the form) gives the install command for your machine to run yourself;
rlord doesn't install agents. Sessions run inline (`--no-alt-screen`), so
tmux scrollback and copy-mode search work as they do for Claude.

Set up once:

```sh
rlord codex install-hooks    # or `rlord install-hooks` for every agent
```

then start `codex`, type `/hooks`, and trust the `rlord hook emit codex`
entries. Codex runs a hook only after you've trusted it. Until you do,
Codex sessions still work: rlord finds each conversation in Codex's own
index and reads working / your turn from its transcript, a few seconds
late and without "needs you".

What works: launch, attach, stop, resume (`codex resume`), fresh
conversation, status, the list and its `agent:codex` filter, preview,
search and history (also over Codex conversations started outside rlord,
with `--everywhere`), triage excerpts, archive summaries (written by Codex
itself, read-only and without saving anything; `[archive]
codex_summary_model`).

The overlord can run on Codex (see "Which agent runs the overlord").

Not yet: importing Codex
conversations started outside rlord, passing a message to a Codex session
from the overlord, sub-agent tracking, Remote Control. A session blocked
on a permission prompt shows "needs you" only with trusted hooks.

## OpenCode sessions

A session can also run on [OpenCode](https://opencode.ai): the `Agent`
hint in the form, "start an OpenCode session to …" to the overlord, or
`rlord new --agent opencode`. It needs **OpenCode v2** (`rlord agents`
says when yours is older, with the command that upgrades it). `[session]
opencode_model` sets its default model (`provider/model`; empty:
OpenCode's own choice); the model picker offers what `opencode models`
lists.

There is nothing to set up: no hooks. OpenCode v2 runs one background
server for all your sessions and its screen is a client of it, so rlord
follows the server instead: working, needs you (a permission request or a
question) and your turn come from its event stream, for the sessions rlord
launched. If the server isn't running, the session's screen starts it.

What works: launch, attach, stop, resume, fresh conversation, status, the
list and its `agent:opencode` filter, preview, search and history (also
over OpenCode conversations started outside rlord, with `--everywhere`),
triage excerpts, archive summaries (asked of OpenCode's server from the
session's own context, adding nothing to the conversation; when it is too
big, from excerpts on `[archive] opencode_summary_model`).

Good to know:

- OpenCode's screen is full-screen, so **tmux has no scrollback for it**:
  scroll inside OpenCode. rlord's search and history read OpenCode's own
  database instead.
- OpenCode's screen takes no model flag and creates a session only with
  its first message, so rlord creates the session first (through
  OpenCode's own `opencode api`), with the model you picked, and then
  opens the screen on it. That step gets ten seconds. If it fails, what
  OpenCode said shows in the pane for a moment, and the session still
  starts, on OpenCode's default model; rlord then sets the model once it
  can, and the session's log (`rlord log`) says so, or says that it
  couldn't.
- Resuming keeps the model the conversation has in OpenCode (you may have
  changed it there), not the one it was launched with. Only a conversation
  OpenCode doesn't have is created again, with the launch's model.
- rlord follows the conversation it launched. A new session or tab you
  open inside OpenCode's screen isn't followed.
- A turn runs in the server: closing the screen stops the rlord session,
  not necessarily the turn.

The overlord can run on OpenCode (see "Which agent runs the overlord").

Not yet: importing OpenCode
conversations started outside rlord, passing a message to an OpenCode
session from the overlord, sub-agent tracking, Remote Control.

## The right-hand side: preview + details

The preview is the session's live pane, in colour (captured with ANSI
attributes), refreshed every `preview_refresh_ms`; `l` swaps it for the event
log. Under it sits the **details** card — agent/model/status, repo and branch,
working dir, the four timestamps, tags and group (all of them, wrapped
over as many lines as they need) — plus two fields you write:
a one-line **description** (`D`, or `rlord describe <ref> <text>`) and
free-form **notes** (`E` opens `$EDITOR`; `rlord notes <ref> [--set|--file]`).
`rlord show <ref>` prints the same card. The list always wins on space: it
keeps its width and the preview drops out first when the terminal is narrow.

**Sub-agents** are tracked from outside the pane: Claude Code's `SubagentStart`
/ `SubagentStop` hooks plus the per-agent `meta.json` and transcript Claude
writes under `~/.claude/projects/<slug>/<session>/subagents/`. The details
card lists each one (type, description, running/done + duration, result line,
tokens); the session list shows `⚙N` while any are running; `rlord subagents
<ref>` prints them. Sessions that ran before the hooks existed are backfilled
from disk on first view.

## Bringing in existing sessions

rlord can adopt agent conversations you started elsewhere: in a terminal,
another session manager, or the desktop app. It finds them in the agents' own
files (`~/.claude/projects/`), so it doesn't matter what started them.

- **Ask the overlord**: "import my sessions from the last month". It proposes
  which to import, archive or skip, with names and groups. Nothing is
  imported until you approve the list.
- **By hand**: `rlord import` lists conversations rlord doesn't track yet,
  most recently active first (`--since 30d`, `2w` or `12h`, measured by the
  last exchange, not when it started). `rlord import <id>...` imports them;
  `--name`, `--archive` and `--recreate-worktree` are optional.
- An imported conversation is a stopped session. Opening it resumes the
  conversation in its original folder.
- If its folder is gone but the branch still exists, the worktree can be
  recreated in place. Otherwise it can only be imported as archived history.
- Close a conversation in its old home before resuming it in rlord. Two
  copies running at once fork the transcript. The listing flags
  conversations that look open elsewhere.

Only Claude Code conversations for now.

## Archived sessions and past work

Archiving a session (`x`, or asking the overlord) keeps it as a record: it
can't be opened again until it's unarchived (`u`), and where a live session
shows its pane, an archived one shows **what it did**: a short summary
written when it was archived, plus its notes and dates.

The summary is written once, in the background, on a cheap model (`[archive]
summary_model`, Haiku by default). Claude reads a fork of the whole
conversation (`--resume … --fork-session`, nothing saved, no tools; the
session itself is untouched); a conversation bigger than
`summary_max_tokens` is summarized from its first prompt and last messages
instead. `:summarize` (or the overlord's `summarize_sessions`) writes or
redoes one.

**Searching past work.** The list's search matches descriptions, notes and
summaries too. To search what sessions actually said and did:

    rlord search auth refresh            # every word, in one message
    rlord search auth refresh --everywhere   # also Claude conversations outside rlord
    rlord history <session> --around <line>  # read around a hit

It searches each session's transcripts in place (including the ones before
a `/clear`), with ripgrep's engine built in: nothing to install, and a
search over hundreds of MB takes well under a second. The overlord has the
same as `search_history` / `read_history` ("we did this before, find it";
with one session, "what happened in X?"), and uses them to carry earlier
decisions into a new session. Its own conversations aren't in these
results: it reads those with `recall`. Any session can run `rlord search`
itself.

## Remote Control (from your phone)

Claude Code's [Remote Control](https://code.claude.com/docs/en/remote-control)
lets you continue a session from the Claude app (Code tab) or claude.ai while
it keeps running on this machine. rlord manages it per session:

- **Ask the overlord**: "turn on remote control for the web app". It
  replies with the link, or with the reason it can't. The overlord has it
  too (offered once, and in the first-run setup): rlord keeps it running so
  it's always there when you're away ("rlord overlord" in the app).
- **By hand**: `rlord remote` lists what's connected; `rlord remote <session>
  on|off [--now|--when-idle]` turns it on or off.
- It's a launch flag, so turning it on or off restarts the session (the
  conversation resumes); rlord never types into a session. If the session
  has work going (its turn, sub-agents, a background command, a prompt
  waiting on you, unsent text), it asks first: restart now, or as soon as the work is done. It's
  remembered: every later restart, resume and fresh start connects it (or,
  turned off, keeps it off: a plain resume would reconnect it).
- A connection that drops (e.g. a long time offline) shows as failed;
  turning it on again restarts the session to reconnect.
- The list marks connected sessions with ⇄ (red when it can't connect).

It needs a claude.ai login on a Pro, Max, Team or Enterprise plan, against
Anthropic's own API: not an API key, Bedrock, Vertex, Foundry or a custom
`ANTHROPIC_BASE_URL`. rlord checks this up front and says why when it's not
available. On Team and Enterprise an admin must allow it first; rlord can't
see that setting, so a session that never connects is reported as such
after a few tries. While connected, the conversation is also stored on
Anthropic's servers.

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
- The overlord keeps its own bar but gets your keys (prefix, root bindings,
  copy mode) when a profile is imported.
- Agent sessions show rlord's keys on their status bar (`⌥d list · ⌥o
  overlord`): in front of your status-right with the profile on, or on a
  slim bar of their own without it.
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

The list has two tabs (`Tab` switches). `H` / `L` step left / right along
sessions tab ⇄ group list ⇄ inside a group (`sessions_tab` / `groups_tab`
in the config).

- **sessions**: every session, flat.
- **groups**: **tags are the groups**.
  - A column on the left lists each tag with its session count, then
    "untagged". The selected group's sessions sit next to it.
  - `←`/`→` (or `h`/`l`) move between the column and the sessions; `esc`
    also goes back. `enter` on a group opens it.
  - **Favourites:** `p` on a group makes it a favourite (★). Favourites head
    the column in your order (`J`/`K` move one); the rest follow, biggest
    first. Use them for the few groups you actually work in, like `work` or
    the feature you're on.
  - `R` on a group renames that tag on every session (a favourite keeps its
    place).
  - A session with two tags appears in both groups.
- **Filter bar:** above both tabs, a line always shows the current filter,
  sort and live/archived, with the keys that change them.
- **Editing tags:** `#` edits the selected session's tags (space-separated).
- **The list's Groups column** shows a session's tags when they fit, else
  how many groups it is in ("3 groups"); the details pane and the groups
  tab always show them all.
- **The overlord** is expected to do most of the organizing:
  - `organize` tags, renames and describes many sessions in one call;
  - `list_groups` lists groups and counts;
  - `rename_group` renames a group;
  - `set_group_order` sets your favourites ("put work and auth-fix first");
  - `set_list_view` opens a tab or group (`tab`, `group`) and can save it as
    your default.

## Themes

Set `theme` in `~/.config/robot-overlord/config.toml` (`rlord config init`
writes a commented one):

- **A family follows your system's light/dark mode**, switching live (checked
  every few seconds): `rlord` (default), `catppuccin`, `tokyonight`,
  `gruvbox`, `solarized`.
- **Or pin one variant:** `rlord-dark`, `rlord-light`, `catppuccin-mocha`,
  `catppuccin-latte`, `tokyonight-night`, `tokyonight-day`, `gruvbox-dark`,
  `gruvbox-light`, `solarized-dark`, `solarized-light`, `nord`, `dracula`.

`rlord-dark` uses your terminal's background; `rlord-light` paints a light
one, so it reads right even if your terminal stays dark. Both use the
256-color palette. The others paint their own background and need a truecolor terminal (kitty,
iTerm2, Ghostty, WezTerm). The overlord's bar and overlay border take the
theme's accent color. Restart rlord (`rlord reset`) after changing `theme`.

**Unstick or recount:** `rlord reset` restarts the daemon and reloads the
list and tmux config (agent sessions keep running). `--rescan` also
re-reads every sub-agent transcript from the start: rlord saves how far it
has read each one and reads forward after that, so this is only needed if
counts look wrong.

## Navigation and keys

**The mouse works everywhere a key does.** Click a row to select it,
double-click to attach, right-click for its menu (the same actions as the
footer, each with its key); the wheel moves through the list, the group
column and the preview. Tabs, search, sort and live/archived in the header
are clickable, as is every keycap in the footer and in dialogs, form
fields and their choices, and the `:` launcher's items. rlord's tmux has
mouse support on (`[tmux] mouse = false` turns it off), so in agent panes
the wheel scrolls the pane's scrollback, and selecting text by dragging
needs Shift held (Option in some macOS terminals). With `rlord tmux
import`, your own `mouse` setting applies in agent sessions.

A session's status means one thing: **working**; **needs you** (blocked
mid-turn on a permission prompt or a question, until you answer); **your
turn** (it finished; the next prompt is yours); **stopped**. Filter with
`status:needs`, `status:turn`, `status:working`. Run `rlord install-hooks`
again after updating rlord: "needs you" clearing when the agent moves on
uses hooks added later (and Codex asks you to trust changed hooks again).

The TUI is the hub. `enter` on a session switches your tmux client into its
pane; from inside **any** agent pane, **Option-D** (`M-d`) brings you back to the
TUI. It's a home-row key, because you press it constantly. Stopped sessions
resume on `enter`. Quit the list with Shift-Q, then confirm. Archiving from
the list (`x`, the right-click menu, `:archive`) asks first: `y` or `enter`
archives, `n` or `esc` doesn't. (The overlord archives without asking you
twice: you already told it to.)

Jumping is search-driven, not cycling: `/` filters as you type and `Enter`
attaches to the top match (`Tab` keeps the filter, ↑/↓ move within matches);
digits `1`–`9` attach to the Nth listed session. The search box and
`rlord ls <query>` share one syntax — free words match name/project/tag,
and `status:needs` `tag:wip` `proj:wait2` `agent:claude`
`sort:user` set typed filters. `o` cycles sorts, `O` reverses. (macOS: make
Option send Alt, e.g. kitty `macos_option_as_alt both`.) Option-D and
Option-O are bindings in rlord's own tmux server (`rlord tmux bind|unbind`).

The footer shows only the everyday keys. `?` opens a cheat sheet of every
key; `:` opens a command launcher for the rest (`:keys`, `:sort created`,
`:archived`, `:log`, `:usage tokens`, …).

**Changing keys:** ask the overlord ("change my keys", "Option-D is
awkward"), or `:keys`. The key screen lists every key rlord binds and what
stands in each one's way: another rlord key or your tmux prefix (taken), or
something rlord takes it from in every session: Claude Code's keys
(including `~/.claude/keybindings.json`), your shell's line editing, your
imported tmux bindings (shadows). To change one, press the new key: that
tests it actually reaches rlord. A key your terminal, macOS or another app
takes never arrives; Option typing `∂` instead of sending Option-D is caught
and the terminal setting to fix it named. Saved keys go into `config.toml`
(the rest of the file stays as you wrote it) and apply at once.

Everything is configurable in `~/.config/robot-overlord/config.toml`
(`rlord config init` writes a commented template; `rlord config show` prints
the effective values): the layout (`side` — list left, live preview right — or
`stack`), the preview refresh rate, every TUI key under `[keys]`, and the tmux
bindings under `[tmux]` (set one to `""` to disable it).

Sessions survive reboots: the minted Claude session id lives in SQLite, so
`rlord restart` brings the conversation back via `claude --resume` even
after the tmux server is gone.

## Updating

```sh
rlord update                 # newest stable release (then `rlord reset`s itself)
rlord update v0.18.0         # a specific one
```

Homebrew and distro packages update the usual way (`brew upgrade`, `apt`,
`dnf`). Development builds (`X.Y.Z-dev.N`, macOS only) come out between
stable releases; to get them, set in `~/.config/robot-overlord/config.toml`:

```toml
[update]
channel = "dev"
```

## The daemon

You never start it by hand: like tmux, the first `rlord` command that finds
no daemon starts one in the background (logging to
`~/Library/Logs/rlordd.log` on macOS, `~/.local/state/robot-overlord/rlordd.log`
on Linux), and a daemon from an older build is restarted on the current
binary. To have the system own it instead (started at login):
`scripts/install-launchd.sh` (macOS) or `scripts/install-systemd.sh` (Linux).
