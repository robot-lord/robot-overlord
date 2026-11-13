**Status:** in progress (PR 1, daemon mode, built on `codex-daemon`, 0.23.0-dev.12; PR 2, Remote Control, next)

# Codex daemon mode and Codex Remote Control

Goal for 1.0.0 (Brandon's decision): a Codex session can run on Codex's
shared background server ("daemon mode"), and such sessions can be
continued from the ChatGPT app through Codex Remote Control
(`codex remote-control`, experimental). Road to 1.0 item 7 in
`public-launch.md`. Researched against codex-cli 0.167.2 on this Mac
(`--help` output, `~/.codex` layout, one read-only daemon query), the
app-server docs, and third-party write-ups of the remote-control RPCs
(marked below; not official).

## Decisions (Brandon, 2026-11-13)

1. rlord may enable Remote Control on this Mac for live checks L3 and L6.
   Pairing a phone needs Brandon (he scans/enters the code).
2. Machine switch gate: the overlord gets `needs_confirm` with
   `machine_wide`, explains in one line, and proceeds only on the user's
   explicit yes (`confirm_machine: true`). Never confirms for the user.
3. Daemon mode is plumbing: `[session] codex_daemon`, set by config or by
   Remote Control; no new-session hint or list marker (detail line only).
4. `[session] codex_daemon` defaults to **off**.
5. If L5 fails (Codex overlord can't run in daemon mode): Codex sessions
   from the phone but not the Codex overlord is acceptable for 1.0.
6. A WebSocket (or other) client dependency for the control socket is
   acceptable if L1 shows it's needed.
7. Remote Control off for one Codex session: "moves it back in-pane, and
   says so", plus offering the machine switch.
8. Sessions started from the phone not showing in rlord: later (Codex
   import), not 1.0.

## What each feature is

### Daemon mode

Codex has two ways to run a conversation ("thread"):

- **In the pane** (`codex --no-daemon`): the TUI process runs the turn
  itself. This is what rlord launches today (`codex::with_model`,
  `crates/core/src/agents/codex.rs:41-45`), because hooks then run inside
  the pane with `RLORD_SESSION_ID` and `TMUX_PANE`.
- **On the shared server** (Codex's default; feature `daemon_auto_start`
  is stable and on): one per-user app-server daemon runs every thread, and
  the TUI is a client of it.

"Daemon mode" in rlord means the second: the thread is hosted by Codex's
daemon and the rlord pane is a client ("create-then-attach").

For the user: the turn keeps running when the pane dies; reopening
re-attaches; the conversation can be open in other Codex clients; it is
the precondition for Remote Control.

### Codex Remote Control

The user pairs the ChatGPT mobile app with this machine once, then drives
Codex threads on it from the phone. Machine-wide: a switch on the daemon
plus paired devices. No per-session flag or URL known.

## How Codex exposes it (0.167.2)

| Thing | Command or file |
|---|---|
| Daemon lifecycle | `codex app-server daemon start \| restart \| stop \| version \| update \| bootstrap [--remote-control] \| enable-remote-control \| disable-remote-control` |
| Daemon state | `codex app-server daemon version` prints JSON: `status`, `backend`, `managedCodexPath`, `managedCodexVersion`, `socketPath`, `cliVersion`, `appServerVersion` |
| Remote Control | `codex remote-control [--json] [--no-daemon]`, `start`, `stop`, `pair` |
| Control socket | `~/.codex/app-server-control/app-server-control.sock` (symlink into `/private/tmp/codex-daemon-<uid>/`) |
| Talking to it | `codex app-server proxy [--sock]`; `codex --remote unix://` |
| Daemon files | `~/.codex/app-server-daemon/` (`daemon.pid`, logs, `daemon-updater.pid`), `~/.codex/packages/app-server-daemon/current/bin/codex` |
| Thread locks | `~/.codex/thread-writer-locks/` |
| Queue a message | `codex queue --thread <id> --message <text> [--resume]` |
| Login state | `codex login status` |

On this Mac a daemon runs and is newer than the CLI (`cliVersion`
0.167.2, `appServerVersion` 0.169.0); a `daemon-updater` installs newer
daemon packages on a schedule.

App-server docs (official, https://learn.chatgpt.com/docs/app-server):
JSON-RPC, `initialize`/`initialized`; `thread/start` (`cwd`, `model`,
`approvalPolicy`, `sandbox`), `thread/resume`, `thread/read`,
`thread/list`, `thread/loaded/list`; `turn/start`, `turn/interrupt`;
notifications `thread/status/changed` (`notLoaded | idle | systemError |
active`, `activeFlags` e.g. `waitingOnApproval`), `turn/started`,
`turn/completed`, `hook/started`, `hook/completed`; approval requests.

Third-party only (verify): `remoteControl/enable`, `/disable`,
`/status/read` (`disabled | connecting | connected | errored`),
`/pairing/start` (returns `pairingCode`, `manualPairingCode`,
`environmentId`, `expiresAt`), `/pairing/status` (`claimed`),
`/client/list`, `/client/revoke`, notification `remoteControl/status/changed`.

One probe: JSON lines through `codex app-server proxy` ended with
`failed to copy data from stdin to socket: Broken pipe`. Framing unknown
(WebSocket guessed).

## Claude's equivalent today

- `core/src/agents/remote.rs`: availability and live state from
  `<config>/sessions/<pid>.json`.
- `daemon/src/remote.rs`: wish in launch spec (`remote_control`, ...);
  `methods::spec_args`; `remote.set` restarts or `needs_confirm`; keeper;
  states `on | off | pending | connecting | failed | stopped | unavailable
  | disconnecting`.
- Gate `Agent::has_remote_control` (Claude only).
- Surfaces: MCP `remote_control`, `create_session.remote_control`;
  `overlord.md` "Remote Control"; TUI setup row, offer dialog, chip `⇄`,
  detail line; CLI `rlord remote`.

## Design: daemon mode (PR 1)

- **Wish per session:** launch spec key `codex_host: "daemon"` (absent =
  in-pane). `[session] codex_daemon = false` default for new Codex sessions.
- **Launch:** `with_model` gains a host arg; omit `--no-daemon` for daemon
  sessions, keep `--no-alt-screen`. Before the pane starts, check `codex
  app-server daemon version`, run `daemon start` if not running (outside
  the DB lock, bounded). Resume: `codex resume <id>` without `--no-daemon`.
- **Identity:** (1) launch-then-bind via `discover::bind` (widen
  `threads_started_in` only as far as L2 shows); (2) create-then-attach via
  new `core/src/agents/codex_server.rs` (`thread/start`, then pane runs
  `codex resume <id>`). Build 1 first; add 2 if L1 passes (dependency OK).
- **Hooks by thread id:** daemon hooks lack `RLORD_SESSION_ID`/`TMUX_PANE`.
  Gate file `<data>/codex-daemon-threads` (thread ids of running daemon
  sessions + folders of ones awaiting a thread), rewritten on bind/start/
  stop. `rlord hook emit codex` without `RLORD_SESSION_ID` reads it; exits
  if `session_id` (or for `SessionStart`, `cwd`) not listed. Never
  autostarts the daemon, never spools for unknown threads.
  `methods::hook_emit` accepts `instance: null, by: "thread"`, resolves by
  `agent=codex, agent_session=session_id, codex_host=daemon`; skips
  `foreign_hook` pane check for these, replaced by that match. Unknown
  thread `SessionStart` whose `cwd` has exactly one waiting daemon session
  binds it. In-pane sessions unchanged.
- **Status:** a dead pane is not a stopped agent: `stop_from_pane` marks a
  daemon session stopped only when its turn is over; else stays working,
  event `pane_gone_turn_running`. `discover` follows hook-confirmed daemon
  sessions with no pane.
- **Stop/archive/fresh:** interrupt first (`turn/interrupt` if client
  exists, else Escape into the pane when `codex::dialog_shown` is false,
  bounded), wait briefly, then kill pane; say so if the turn won't end.
  `session_restart` re-attaches.
- **Sending:** unchanged (types into pane). `codex queue` stays owed.
- **Codex overlord stays in-pane in PR 1** (its `-c` overrides; L5).
- **Versions:** `rlord agents`/`list_agents` show daemon `appServerVersion`
  beside CLI's when different; too-old check uses the lower. rlord never
  runs `daemon update`/`restart`.
- **TUI:** detail line "codex: shared server". No list column.
- **overlord.md/MCP:** nothing new in PR 1; say so in the PR.

## Design: Remote Control (PR 2)

- Codex session remote `on` = machine enabled+connected+paired AND session
  in daemon mode. States reuse Claude's words plus `unpaired`.
- New `core/src/agents/codex_remote.rs`: availability (Unix, codex
  installed, `codex login status` logged in — never read auth.json,
  daemon present); machine status (cheapest of `remote-control start
  --json`, `daemon version`, `remoteControl/status/read`), cached, polled
  by keeper; `enable()` = `codex remote-control start --json`; `pair()` =
  `codex remote-control pair --json`; `disable()` = `codex app-server
  daemon disable-remote-control` (not `remote-control stop`, which stops
  the daemon).
- `daemon/src/remote.rs` per agent; `has_remote_control` true for Codex.
  `remote.set {on:true}`: machine off → `needs_confirm` + `machine_wide:
  true`; only `confirm_machine: true` enables (kv
  `codex_remote_enabled_by_rlord`); in-pane → set `codex_host=daemon` and
  restart (existing work-going check, `restart: now|when_idle`); unpaired
  → `needs_pairing: true`. `{on:false}` → back in-pane (restart), answer
  says machine switch still on and offers it. `remote.machine {agent:
  "codex", on:false}`. `remote.pair` opens TUI pairing dialog, returns
  `{shown, expires_at}`; the code never goes into MCP results, logs or
  events. `create_session {remote_control:true}` for Codex → daemon mode
  from start.
- MCP: `remote_control` description agent-neutral, `confirm_machine`
  param, new tool `remote_pair`.
- overlord.md: Remote Control section rewritten agent-neutral; Codex:
  `needs_confirm`+`machine_wide` → one line, ask, never confirm for the
  user; `needs_pairing` → `remote_pair`, user reads code on screen, never
  ask for or repeat a code; off doesn't unpair; routing "pair my phone" →
  itself, `remote_pair`.
- TUI: setup row for Codex (default off) with its own line; offer dialog
  Claude only unless L5 passes; `Mode::RemotePair` (code, expiry, `^Y`
  copy, Esc); chip/detail text "in the ChatGPT app".
- CLI: `rlord remote <session> on|off` for Codex (terminal confirm),
  `rlord remote pair`, `rlord remote` lists machine status.
- Docs: guide, architecture, codex-support.md, opencode-support.md,
  public-launch.md item 7; reconcile this doc.

## Parity with Claude

| | Claude | Codex |
|---|---|---|
| Scope | per session | machine + session in daemon mode |
| Turning on | restart with `--remote-control` | enable once, pair once, restart into daemon mode |
| Link | per session URL | none known; in the ChatGPT app |
| Name in app | rlord sets it | thread title (unless L6 finds a way) |
| Off for one session | real | moves in-pane; paired phone may still resume |
| Connected | per session | per machine |
| Phone-started sessions | n/a | not in rlord (later) |
| Overlord from phone | yes | only if L5 passes |
| Work survives pane | no | yes |

## Live checks

*turn* = one real prompt by hand; *machine* = changes Codex machine state
(approved by Brandon for L3/L6).

- L1 control-socket protocol (read-only).
- L2 daemon thread in the index (`source`, `thread_source`, timing;
  `codex resume <id>` attaches). *turn*
- L3 Remote Control CLI output (`start --json`, `pair --json`, status,
  claimed, `disable-remote-control` keeps daemon). *machine*
- L4 hooks in the daemon (fire? payload `session_id`, `cwd`,
  `transcript_path`; env; hook trust). *turn*
- L5 `-c` overrides on a daemon thread; MCP server env. *turn*
- L6 what the phone sees; thread name; resuming an in-pane thread;
  approvals. *machine*, *turn*
- L7 two clients, one thread: `composer_shown`, `dialog_shown`, `draft`.
- L8 daemon restart/update mid-turn.
- L9 Linux backend/socket/sandbox.

## Risks

- The daemon is a different, self-updating Codex than the CLI.
- Hook identity moves from pane to thread id: match must be as strict
  (daemon-mode sessions only, exact bound id, folder bind only with one
  waiting session).
- Machine-wide exposure: enable only on explicit confirmed request, never
  for the overlord unasked.
- Pairing code must not pass through the model provider or rlord logs.
- "Stopped" must stay true: stop interrupts first or says it couldn't.
- Experimental surface: isolate calls in `codex_remote.rs`/
  `codex_server.rs`, bounded, failing into "unavailable: <reason>".
- `rlord hook emit` runs for non-rlord Codex threads: gate file keeps it
  to one small read.
- Daemon thread commands/MCP/hooks run in the daemon's environment.

## Test plan

No model calls; no real Codex in automated tests; tmux only on scratch
sockets (`RLORD_TMUX_SOCKET`, `RLORD_TMUX_TESTS=1`); never the user's
default server, never capture from `tmux -L rlord`.

Unit: codex.rs argv with/without `--no-daemon` (overlord always has it);
methods.rs hook_emit by thread (bind, confirm, ignore unknown, ignore
in-pane, folder bind only with one waiting, never revive stopped; pane
check still holds in-pane); hook client gate file cases; discover.rs pane
rules; codex_remote.rs parsing from L3 fixtures and every state;
remote.rs Codex set paths, pairing code never in result/event/log;
mcp/overlord tool-mention tests; TUI setup rows and pairing dialog keys.

Stand-in: `scripts/demo/bin/codex` learns `app-server daemon
version|start`, `remote-control start|pair --json`, `login status`, a
daemon-style thread (hooks without `RLORD_SESSION_ID`); scratch rlord
(`RLORD_NO_LOGIN_SHELL=1`, own XDG dirs, own socket) end to end.

By hand: L2, L4, L5, L7, L8 against real Codex in a scratch rlord, one
throwaway thread each, deleted after; L3, L6 with Remote Control enabled.
Record under "Verified live".

## Implementation steps

PR 1 daemon mode:
1. L1; record; choose identity path.
2. `codex_host`, `[session] codex_daemon`, argv, daemon check/start.
3. Gate file + hook client no-`RLORD_SESSION_ID` path.
4. `hook_emit` by thread with binding rule.
5. `discover` binding and pane-gone rules.
6. Stop/archive/fresh interrupt first.
7. Daemon version in `rlord agents`/`list_agents`.
8. TUI detail line; stand-in; tests.
9. If L1 passed: `codex_server.rs` create-then-attach, `turn/interrupt`.
10. L2, L4, L5, L7, L8 by hand; guide and architecture.

PR 2 Remote Control:
1. L3, L6 (Brandon pairs the phone); fixtures.
2. `codex_remote.rs`.
3. `daemon/src/remote.rs` per agent.
4. MCP + overlord.md same commit.
5. TUI + CLI.
6. Codex overlord in daemon mode only if L5 passed.
7. Docs; reconcile this doc.

## PR 1 as built (differs from the plan)

- **Identity: launch-then-bind only.** Create-then-attach is not possible
  on 0.167.2 / daemon 0.169.0: a thread made by `thread/start` has no
  rollout until its first turn, and `codex resume <id>` on it fails ("no
  rollout found for thread id"). So `codex_server.rs` has no
  `thread/start`; the TUI creates the thread, bound by the first
  `SessionStart` (folder rule) or the index (`source = 'vscode'`).
- **No `RLORD_SESSION_ID` in daemon-mode panes** (unset), on top of rlord
  starting the daemon with it scrubbed: a daemon the TUI autostarts
  inherits the pane's environment (seen: the running daemon has a
  `TMUX_PANE` and `TMUX` of whatever pane started it) and would hand it to
  every thread's hooks.
- **Only the session's own thread passes the gate.** Sub-agent threads on
  the daemon aren't listed, so their "needs you" isn't shown (owed).
- **Fresh** (`session_fresh`) is only used for the overlord, which stays in
  the pane, so it has no interrupt step.
- **The control-socket client** names itself `codex-tui`: see L1.
- **Too-old check**: Codex has none, so nothing to take the lower of; the
  daemon version is shown (`rlord agents`, `list_agents`
  `daemon_version`) when it differs.
- **`SessionEnd` while the pane is up is ignored** for daemon-mode
  sessions (a client leaving ends nothing; the thread outlives it).
- overlord.md and MCP: nothing new (stop/archive answers may carry `turn`
  and `note`, which explain themselves).

## Verified live

Against codex-cli 0.167.2 with a self-updated daemon 0.169.0, on this Mac
(2026-11-13). Throwaway threads in scratch tmux sockets, cheapest model
(`gpt-5.6-luna`), all deleted afterwards (`codex delete --force`).

- **L1** control socket: WebSocket over the Unix socket (HTTP upgrade →
  101, `x-codex-websocket-max-unfragmented-message-bytes: 16777216`), then
  JSON-RPC, one message per text frame, no `"jsonrpc"` field in answers.
  `initialize` (needs `clientInfo`), then `thread/loaded/list`,
  `thread/read` (status `idle | active{activeFlags} | notLoaded |
  systemError`), `thread/turns/list` (latest turn's id and `status`:
  `inProgress | completed | interrupted`), `hooks/list` (rlord's hooks
  listed `trustStatus: trusted` for a folder). Notification
  `remoteControl/status/changed` arrives right after `initialize` (status
  `disabled` here). **Side effect found:** the daemon kept the first
  `clientInfo.name` it was given after starting as the originator of
  threads it created later, for other clients too (a TUI thread was
  recorded `originator = "probe"` after a probe connected). It stays until
  the daemon restarts; rlord's client now gives `codex-tui`.
- **L2** daemon thread in the index: `source = 'vscode'`, `thread_source =
  'user'`, `cli_version` the daemon's (0.169.0), `first_user_message` the
  launch prompt, `created_at_ms` a second after launch. `codex resume <id>`
  (no `--no-daemon`) attaches and shows the turns, including one that
  finished while no pane was attached.
- **L4** hooks in the daemon: they fire (watched as a second client,
  `hook/started` / `hook/completed`: `userPromptSubmit`, `stop`), from
  `~/.codex/hooks.json`, trusted as for in-pane threads. The daemon's
  environment has no `RLORD_SESSION_ID` but does have `TMUX_PANE` / `TMUX`
  (counted, not printed). Killing the TUI client (`respawn-pane -k`) fired
  no `sessionEnd`; the thread stayed loaded, and was `notLoaded` some
  minutes after its last turn. The payload itself wasn't captured (the hook
  command is the installed rlord, which exits without `RLORD_SESSION_ID`);
  its handling is covered by the stand-in.
- **L5** `-c` on a daemon thread: any `-c`, `--enable`, `--disable` or
  `--search` makes the TUI run embedded, with the warning "Running without
  the shared background server: command-line configuration overrides (-c,
  --enable, --disable, or --search) requires embedded mode." So the Codex
  overlord (all `-c`) can't run on the daemon as wired today (Brandon's
  decision 5: acceptable for 1.0). `-m` / `--model` doesn't. Without `-c`,
  `check_for_update_on_startup=false` can't be passed: a daemon-mode pane
  can show Codex's update prompt at start, as an ordinary in-pane session
  does.
- **L7** two clients on one thread: a draft is per client; a turn sent from
  one shows in the other as it streams, but the other's composer reads as
  idle (`› Ask Codex…`, no "esc to interrupt") meanwhile, so a screen check
  can't tell a turn started elsewhere; Escape in either client interrupts
  the turn. Approval dialogs in both: not seen (no approval came up).
- **Killed mid-turn**: the turn ran on (status `active`, then `idle`); the
  next `codex resume` showed it finished.
- **End to end, scratch rlord on the real Codex** (own XDG dirs and tmux
  socket, `RLORD_NO_LOGIN_SHELL=1`, stand-in `SHELL`; its hooks can't reach
  a scratch rlord, so this is the unhooked path): launched with no
  `--no-daemon`, bound from the index (`agent_session_bound`, `codex
  thread index`), your turn / working from the transcript; `rlord stop`
  mid-turn → `turn: interrupted` in 0.3s, the daemon said `idle`;
  `rlord restart` re-attached; the tmux session killed mid-turn →
  `pane_gone_turn_running`, status working, then stopped once the turn
  ended.
- **Stand-in end to end** (`scripts/demo/bin/codex` without `--no-daemon`:
  hooks without `RLORD_SESSION_ID` / `TMUX_PANE`, `source = 'vscode'`):
  SessionStart bound by folder through the gate, then hooks by thread;
  gate file lists the bound threads; pane env has no `RLORD_SESSION_ID`;
  stop of a working session → Escape, `turn: interrupted`; restart →
  `codex resume <id>` without `--no-daemon`; pane killed with a turn open →
  `pane_gone_turn_running`, then stopped when the rollout closed the turn;
  `rlord agents` shows "its background server runs 0.169.0".

Not run:

- **L8** daemon restart/update mid-turn: needs `codex app-server daemon
  restart|update`, which rlord's checks don't run. Owed, by hand.
- **L3, L6**: PR 2.
- **L9** Linux backend/socket/sandbox.

## Owed (after PR 1)

- L8 by hand: a daemon restart or update mid-turn (what the session
  shows; `notLoaded` should read as not running, so it stops once the pane
  is gone).
- A daemon-mode session's hooks through the real daemon into rlord, once a
  release with the gate is installed (the payload fields, a first
  SessionStart bound by folder).
- `/clear` and `/new` in a daemon-mode session: the new thread isn't
  followed (the session stays on its first); the gate can't tell the
  session's TUI from another client in the same folder.
- A sub-agent's permission prompt on the daemon doesn't show "needs you"
  (its thread isn't in the gate).
- `send_to_session` still types into the pane; `codex queue` would reach a
  daemon thread without one.
- The Codex overlord on the daemon (L5: its `-c` wiring forces embedded);
  would need its MCP server and settings some other way.
- Residual: a daemon started by hand from an in-pane rlord session's pane
  would pass that pane's `RLORD_SESSION_ID` and `TMUX_PANE` to every
  thread's hooks, which then pass the pane check for that session.
