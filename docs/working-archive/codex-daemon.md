# Codex daemon mode and Codex Remote Control: residue

Daemon mode shipped in 0.23.0-dev.12 (PR #30); how it works is in
`docs/architecture.md` ("Codex on its shared background server") and
`docs/guide.md` ("Codex sessions"). The full working doc is in git history
(`docs/working/codex-daemon.md`, removed 2026-11-13).

## Codex Remote Control: deferred

Brandon's decision (2026-11-13): rlord doesn't support Codex Remote Control
until Codex makes it official. The CLI is still `[experimental]` and the
control socket's `remoteControl/*` methods are undocumented. Remote Control
stays Claude Code only, and the Codex item is past 1.0.

Starting point when it comes back: branch `codex-remote` (pushed, not
merged). It has a machine switch that needs the user's explicit yes, phone
pairing with the code shown only in the user's terminal (`remote_pair`),
sessions moved onto the daemon and back, a stand-in control socket for
tests (`codex_server::fake`), and docs. Re-check everything below against
the Codex of the day first. Where the branch's own working doc differs
(it was written before MFA was on and before the lock was timed twice),
this file is newer. Rebasing it onto main conflicts on
`docs/working/codex-daemon.md` (deleted here): keep it deleted, take what
is needed from this file, and bump to the next free version. Rules to
keep (Brandon's): the machine switch
goes on only on the user's explicit yes (`confirm_machine`), never
confirmed by the overlord for them; the pairing code never goes into MCP
results, logs or events, only to the user's own terminal; turning it off
for a session moves it back into its pane and says the phone may still
reach it, offering the machine switch.

Live checks named here (from the removed doc): L3 the Remote Control CLI
and socket calls; L6 what a paired phone sees (never run).

What L3 found (codex-cli 0.167.2, daemon 0.169.0, macOS):

- `codex remote-control start`, `codex app-server daemon
  enable-remote-control` and `disable-remote-control` all **restart the
  daemon**, which cuts off every turn running on it. `remote-control pair`
  doesn't.
- Over the control socket, with `capabilities.experimentalApi: true` at
  `initialize`, `remoteControl/enable` and `remoteControl/disable` switch it
  **without a restart** and persist it
  (`~/.codex/app-server-daemon/settings.json`, `remoteControlEnabled`).
  `remoteControl/status/read` gives `disabled | connecting | connected |
  errored` with `serverName`, `installationId`, `environmentId`; the same
  object arrives as `remoteControl/status/changed` right after
  `initialize`, without the capability.
- Enabling enrolls the machine with ChatGPT first. Without multi-factor
  authentication on the account it fails at once: HTTP 403
  `{"detail":"Multi-factor authentication required"}`, and the switch stays
  off. With MFA it went `connecting` → `connected` in about a second.
- `remoteControl/pairing/start` needs the switch on;
  `remoteControl/pairing/status` takes `pairingCode` or
  `manualPairingCode`; `remoteControl/client/list {environmentId}` gives
  `{data: [], nextCursor}`. Not seen: a pairing code's shape, `claimed`,
  what the phone shows (L6 was never run; no phone was paired).
- `thread/name/set` exists (could name a thread after its rlord session).

For moving a session between the pane and the daemon (Remote Control
needs it; main never does):

- The daemon holds a thread's writer lock after its last client leaves,
  until it unloads the thread (`thread/read` says `notLoaded`; seen from
  under a minute to several minutes: poll, don't wait a fixed time).
  `codex resume --no-daemon <id>` before then shows "This conversation is
  open in another app".
- Onto the daemon (an in-pane thread resumed there) works at once.

## After launch (owed, daemon mode)

The checks that keep Road to 1.0 item 7 open are listed there
(`docs/working/public-launch.md`). These become issues with the rest of
its "After launch (tracked)" list:

- A message can be typed into a daemon-mode thread mid-turn when another
  Codex client started that turn (architecture.md, "Another client on the
  same thread"). Fix: in `send::type_message` (sends, deliveries and the
  limit resume; `ask.rs` types into the overlord with `type_into`
  directly), for daemon-mode sessions only, ask
  the daemon (`codex_server::thread_state`) and treat any `Active` as "not
  now", `waiting` included. `thread_state` is a handshake, `initialize`
  and `thread/read`, each bounded only between 3s reads, so a stalling
  daemon can hold the session's lock well past 10s: give it a much
  shorter limit there.
- Approval dialogs with two clients on one thread: not seen. Stop and
  archive send Escape when `codex::dialog_shown` is false; whether an
  approval raised in the other client shows in the rlord pane is untested.
- `/clear` and `/new` in a daemon-mode session: the new thread isn't
  followed (the session stays on its first); the gate can't tell the
  session's TUI from another client in the same folder.
- A sub-agent's permission prompt on the daemon doesn't show "needs you"
  (its thread isn't in the gate).
- `send_to_session` still types into the pane; `codex queue` would reach a
  daemon thread without one.
- The Codex overlord on the daemon: any flag `codex::needs_embedded`
  matches makes Codex run embedded, and the overlord's wiring is all
  `-c`; it would need its MCP server and settings some other way.
- Linux: the daemon's backend, socket and sandbox (unchecked; Linux fixes
  come after 1.0, Road to 1.0 item 1).
- Check live what environment a daemon thread's commands and MCP servers
  get when rlord started the daemon (architecture.md states the expected
  one).
- A daemon started by hand from an in-pane rlord session's pane would pass
  that pane's `RLORD_SESSION_ID` and `TMUX_PANE` to every thread's hooks,
  which then pass the pane check for that session.
- A daemon started by a daemon-mode pane's own TUI carries that pane's
  `TMUX` / `TMUX_PANE` into every thread's commands and MCP servers. Ways
  rlord's own start doesn't happen first: `ensure_running`'s one-a-minute
  throttle also starts after a success (throttle only failures);
  `ui.goto` restarts a session whose tmux session is gone without asking
  (`starts_on_codex_daemon` checks only Stopped ones); and `daemon start`
  runs without the Codex folder the pane adds to PATH, so a Codex that
  needs it (npm under nvm, rlord as a service) fails there and the pane
  starts the daemon.
- `starts_on_codex_daemon` for `session.new` ignores the overlord role and
  `codex::needs_embedded`, so rlord starts (and waits on) Codex's daemon
  for sessions that will run in the pane; `session_new` has the real test.
