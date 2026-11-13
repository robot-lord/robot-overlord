# rlord user guide

The README covers what rlord is and how to install it. This is the
reference: how each part works and what you can set. Keys are written the
Mac way (Option-O); on Linux that's Alt-O. Every key can be changed (`:keys`,
or ask the overlord).

Contents: [The overlord](#the-overlord) · [Creating sessions](#creating-sessions) ·
[Codex sessions](#codex-sessions) · [OpenCode sessions](#opencode-sessions) ·
[Usage limits](#usage-limits) ·
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
runs the overlord" below for what differs. You can have one on each agent
and switch between them: see "An overlord per agent" below.

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

Inside the overlay, Option-O (or Option-D) closes it, as does a click on
the ✕ at the right end of its top row. (tmux ignores clicks outside a
popup, so the status bar's hints can't be clicked while it is up.) The overlord sits at
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
`rlord overlord status|model <m>|restart|open` does the same from the CLI
(`rlord overlord switch <agent>`: see "An overlord per agent").

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
| Passing work to a session | `send_to_session` | `send_to_session` | `send_to_session` |

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
- **Passing work to a session** ("tell the web app to also fix the
  footer"): the overlord sends it with `send_to_session`, whichever agent
  runs the overlord and whichever the session runs. rlord types it into
  that session's prompt, as if you had, once its turn is over. A session
  that is working, or waiting on you, isn't typed into: the overlord says
  so, or (when you want it delivered anyway) it waits for that turn to end
  and goes in then (one waiting message per session; a newer one replaces
  it, and one sent directly drops it). Stopped and archived sessions are
  refused. Each send is in the session's log.
- Right before typing, rlord looks at the session's screen. It holds off
  (the message waits, or is refused) while a dialog is up there (a
  permission prompt, a question: Enter would pick an answer), while its
  input holds text you typed and haven't sent (Claude and Codex), within
  10s of the last thing typed into it, or while you are typing in a
  terminal showing that session (used in the last 5s; typing to the
  overlord in its overlay doesn't count). A waiting message that is held
  back says why once in the session's log. A restart, stop or archive
  of the session waits for a paste in progress, and the other way round.
- `rlord overlord ask` (and the list's `n` form) won't type into the
  overlord while it waits on you, or while its screen shows a dialog: its
  Enter would answer it. Answer it first.
- **It types into sessions without asking.** `send_to_session` runs
  unprompted, like rlord's other tools; that is what lets the overlord
  orchestrate. rlord expects to be run by its user on a machine they
  trust, or over a remote connection they trust. Little untrusted text
  reaches the overlord: it works from rlord's own data and your requests,
  not arbitrary content. Prompt injection would take a setup very
  different from the one rlord is built for. Codex's read-only sandbox
  stops the overlord's shell commands reaching rlord's tmux socket, but not
  this tool. `[overlord] confirm_sends = true` makes each send (and `resume_after_limit`) ask, for
  anyone who wants that (Claude then lists rlord's tools one by one without
  `send_to_session` and `resume_after_limit`; Codex sets both tools'
  `approval_mode = "prompt"`; OpenCode adds an ask rule for each).
- The agent of the first overlord is chosen when it is set up
  (`[overlord] agent` applies only while there is none yet). An overlord
  never changes agent; you switch to another agent's overlord instead.

### An overlord per agent

Each agent can have its own overlord, with its own conversation. Exactly
one is **active**: the one Option-O shows, and the one `n`, `rlord
overlord ask` and everything else that talks to "the overlord" reaches.
The others keep running in the background, untouched, and switching back
finds each exactly where you left it. Use it when one agent is out of
usage, or to put the same question to another.

Switching is always yours: rlord never switches by itself. An agent's
overlord is started the first time you switch to it, with that agent's
default model and effort (the configured `[overlord] model` / `effort`
when it is the configured agent).

- **In the overlay**: with more than one agent installed, the top row
  shows each agent's marker, the active one highlighted
  (`✻ claude │ >_ codex`). Click one, or press **Option-S** to go to the
  next overlord that exists (`[tmux] overlord_switch`; "" turns the key
  off, and so does giving it the same key as `back`, `next`, `prev` or
  `overlord`). The key never creates one: a click on an agent without an
  overlord, `:overlord <agent>` or the CLI does. The overlay stays open and
  shows the other overlord.
- **In the list**: right-click the overlord's row for "Switch overlord to
  Codex" (one entry per installed agent), or `:overlord codex` in the
  command launcher (`:overlord` lists them).
- **CLI**: `rlord overlord switch codex`; `rlord overlord switch` lists
  the agents and which is active.

The overlord's row shows the active one (its agent's marker before the
name). Other overlords that are running show as `+` and their marker
after its status. Each restarts on a fresh conversation after sitting
idle, as one overlord does, and each can look up what you said to any of
them (their `recall` reads all the overlords' conversations), so "as I
said earlier" still works after a switch. Each lives in its own folder
(`overlord/`, `overlord-codex/`, … under rlord's data folder).

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
| **last message** (default) | the last message from you, the agent, or the overlord sending to it (a resume or a status flip alone doesn't count) |
| **last prompt** | the last message *you* sent: what you were just working with |
| **status change** | the last time it started, stopped, finished or needed you |
| **created** | when it started |
| **name** | A–Z |
| **usage** | most used first, by what the Usage column shows |

The details card breaks the times down.

**The Usage column** shows how much each session has used, in one of three
measures, and its header names the one shown (**Turns**, **Tokens** or
**Context**):

- **turns** (default): prompts you typed;
- **tokens**: tokens the agent took in new or wrote (cache re-reads don't
  count);
- **context**: how full its context window is, e.g. ` 42%`. `–` when
  rlord doesn't know the model's window.

Switch it with `U` (turns → tokens → context), by clicking `Σ turns` in
the toolbar, with `:usage turns`, `:usage tokens` or `:usage context`, or
by asking the overlord. Until you pick one, the column shows
`usage = "turns" | "tokens" | "context"` from `config.toml` (default
turns). Once you pick one, any of those ways, rlord remembers it across
restarts of the list and the daemon, over the config. `:usage default` (or
asking the overlord to reset it) forgets the pick and goes back to the
config's `usage`.
On a narrow screen the toolbar drops the measure's name first (`Σ U`),
then its keycap with the others (`Σ`). Numbers keep to four characters
(`999`, `1.2k`, `45k`, `3.1M`), and the column keeps its width whatever it
shows, so nothing jumps. Clicking its header sorts by it (again: reverses);
the usage sort is a sort like the others, by whichever measure the column
shows (sorted by context, the header shortens to `Ctx % ▼`; the values
stay put). The details
card always has all three.

As the list narrows, its columns give way in this order: Project first,
then Groups (shrinking to a count, `#3`, before it goes), then names
shrink to their minimum (the time column's header shortens on the way),
and Usage only at the smallest widths.

Headers that can change read in a quieter blue: the time column (it
follows the sort) and the Usage column (it follows `U`). The one the list
is sorted by is bright, with its arrow; fixed headers (Name, Status,
Groups) are plain. The toolbar's sort and `Σ` controls use the same
quieter blue for what they're set to. Clicking `Σ` works even if `U` is
bound to something else (then it has no keycap). rlord reads these
from the agents' own transcripts (Claude Code and Codex JSONL, OpenCode's
database), only what was appended since it last looked; no model calls.
`rlord ls` has a USAGE column too (`--usage tokens`, `--sort usage`);
without `--usage` it shows (and sorts by) the same measure as the list.

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

**On Codex's shared background server** (`[session] codex_daemon = true`;
off by default): new Codex sessions run their conversation on Codex's
per-user background server instead of in the pane. The turn keeps going
when the pane closes, and a restart attaches to it again. The session's
details say "codex: shared server". rlord starts that server itself when
none runs, and never updates, restarts or stops it; `rlord agents` shows
its version when it differs from your `codex` (it updates itself).
Stopping or archiving such a session interrupts its running turn first,
and says so if it wouldn't stop. The overlord, and a session launched
with flags that need the pane (`-c`, `--enable`, `--disable`,
`--search`), stay in the pane. Not followed yet on the server: `/clear`
or `/new` in the session (it stays on its first thread), and "needs you"
from a sub-agent's prompt.

Not yet: importing Codex
conversations started outside rlord, sub-agent tracking, Remote Control. A session blocked
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
conversations started outside rlord, sub-agent tracking, Remote Control.

## Usage limits

When an agent stops a session because you've hit a usage limit, rlord
shows it in place of the session's status: `◷ till 3:00pm` in the list,
"limited until 3:00pm" in the details. It reads this from the agent's own
record of the limit, reset time included: Claude Code's limits, Codex's
ChatGPT-plan limits, and OpenCode's own (free, Go and Console budget
limits). A limit whose reset time the agent doesn't record (an API key
out of credit, another provider's quota) doesn't show. A session stays
limited until the limit resets or you use it again.

To have limited sessions carry on by themselves once the limit resets,
ask for it:

- tell the overlord ("I hit the limit, pick these up when it resets"),
- `rlord resume-after-limit` (every limited session), or
  `rlord resume-after-limit web-fix api-tests`,
- right-click a limited session: "Resume after limit" (or
  `:resume after limit`).

A minute after the reset time, each of those sessions is sent a short
message (`[limits] resume_message`, "continue" by default;
`--message "…"` for another), typed in the way the overlord's
`send_to_session` types. A session you used in the meantime is left
alone. If the limit hits again, it waits for the next reset. A scheduled
resume shows as `↻` next to the time and survives restarting rlord.
Nothing is resumed unless you ask.

If it is the overlord's agent that is out of usage, switch to another
agent's overlord ("An overlord per agent", above): the overlay's top row
says "out of usage until …" when the active one is limited.

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
Where its `list_width_pct` share is too narrow for a readable name, Status,
the time column and Usage, the list borrows that much from the preview,
leaving the preview at least 40 columns or 30% of the width. It never takes
less than its share. Usage needs a list of at least 59 columns (the side layout
gives the list 60 whenever the terminal has them); with the
default 50% it sits beside a readable name from about 120 columns. The
groups tab doesn't borrow: it keeps its share.

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

`a` (or `:archived`) switches the list to archived sessions. The list and
details panes sit on a faint tint (warm on most themes, a grey tint on
rlord-dark) and the list's title reads **Archived** in yellow, so it
can't pass for the live list. `esc` (or `a` again) goes back. Anything
nearer takes `esc` first: an open popup, the search filter, and inside a
group `esc` returns to the group list first.

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

## rlord's MCP server in any session

Any agent session, in rlord or not, can add rlord's MCP server to look up
other sessions. It is read-only: it lists sessions and their details,
reads their screens, searches and reads their history (`search_history`,
`read_history`, `recall`), and lists groups, repos and keys. It changes
nothing and starts nothing, not even the daemon: with no daemon running
its tools return an error. Add it like any other stdio server:

    claude mcp add rlord -- rlord mcp       # Claude Code (add --scope user for every project)
    codex mcp add rlord -- rlord mcp        # Codex (~/.codex/config.toml)
    opencode mcp add rlord -- rlord mcp     # OpenCode (add --global for every project)

Tools that change things (sending to a session, stopping, archiving,
renaming, tagging, moving the user's view, creating sessions) are only
served to the overlord, which rlord starts with `rlord mcp --overlord`.
A call to one of them on the read-only server is refused.

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
config: no prefix key, mouse on. The only keys it binds are Option-D (to
the list) and Option-O (the overlord). Agent sessions get a slim status bar:
the session's name on the left, `⌥d list · ⌥o overlord` on the right. Click
a hint to do what its key does (with `[tmux] mouse` on; a click elsewhere
on the bar is tmux's own). Your personal tmux is never
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
  slim bar of their own without it. The hints are clickable either way.
- Per session: `rlord tmux profile on|off|toggle|default [REF]`.
- **To see the setup a new user gets** without removing your profile, open
  the list with `RLORD_TMUX_PROFILE=default rlord`. That run puts every
  agent session on rlord's own tmux; the next plain `rlord` puts your
  profile back (`RLORD_TMUX_PROFILE=imported` forces it on). Running
  sessions switch at once. To make it stick, set `[tmux] profile = false`
  (takes effect on `rlord reset` or the next start).
- `rlord tmux profile` (outside a session) prints which setup is active
  and why: `RLORD_TMUX_PROFILE`, `[tmux] profile`, or no profile imported.
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
  how many groups it is in, dimmed (`#3`); the details pane and the groups
  tab always show them all. It's at most 15 cells wide, and only gets
  width the Usage column and a readable name don't need; squeezed, it
  shrinks to the count before names do. On the overlord's row the same
  column says when its conversation started fresh, shortened to `↻2h`
  when narrow (the details pane has it in full).
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
footer, each with its key; its highlight follows the pointer); the wheel moves through the list, the group
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
`status:needs`, `status:turn`, `status:working`. (A session stopped on a
usage limit shows that instead: see "Usage limits".) Run `rlord install-hooks`
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
`sort:user` set typed filters. `o` cycles sorts, `O` reverses, `U` cycles
what the Usage column shows. (macOS: make
Option send Alt, e.g. kitty `macos_option_as_alt both`.) Option-D and
Option-O are bindings in rlord's own tmux server (`rlord tmux bind|unbind`).

The footer shows only the everyday keys. `?` opens a cheat sheet of every
key; `:` opens a command launcher for the rest (`:keys`, `:sort created`,
`:archived`, `:log`, `:usage tokens`, …).
In the launcher, menus and form pickers, `ctrl-n`/`ctrl-p` and
`ctrl-j`/`ctrl-k` move up and down as the arrows do (as in fzf); plain
`j`/`k` still type into the launcher's filter.

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

Sessions survive reboots: each session's agent conversation id lives in
SQLite, so `rlord restart` brings the conversation back through the agent's
own resume (`claude --resume`, `codex resume`, OpenCode's `-s`) even after
the tmux server is gone.

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
