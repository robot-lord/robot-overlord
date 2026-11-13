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
the Codex of the day first.

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
needs it): back into the pane must wait for the daemon to let go of the
thread (architecture.md, the writer-lock bullet); onto the daemon works at
once.

## Owed (daemon mode)

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
- A send can be typed into a daemon-mode thread mid-turn when the turn was
  started from another Codex client (the rlord pane's composer reads idle).
  Fix: before the paste, ask the daemon (`codex_server::thread_state`)
  and treat any `Active` as "not now", `waiting` (an approval or question
  in the other client) included: the turn isn't over.
- Approval dialogs with two clients on one thread: not seen. Stop and
  archive send Escape when `codex::dialog_shown` is false; whether an
  approval raised in the other client shows in the rlord pane is untested.
- The Codex overlord on the daemon: any `-c`, `--enable`, `--disable` or
  `--search` makes Codex run embedded, and the overlord's wiring is all
  `-c`; it would need its MCP server and settings some other way.
- Residual: a daemon started by hand from an in-pane rlord session's pane
  would pass that pane's `RLORD_SESSION_ID` and `TMUX_PANE` to every
  thread's hooks, which then pass the pane check for that session.
- L9: Linux backend, socket and sandbox.
