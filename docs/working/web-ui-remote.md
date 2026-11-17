**Status:** in progress (slice 1 steps 1–2 being built; server placement being redesigned)

# rlord web UI and end-to-end encrypted relay

Follows `web-ui-spike.md` (a local PTY bridge drives the overlord cleanly).
Two components: the **web UI** and the **transit** (a relay). Built in
sections.

## User's spec (as given)

### Web UI

- A true web UI. What the list/overlord TUI drives now becomes a web
  interface. tmux is wrapped by the server process: the user never needs
  to know about it.
- The UI shows what the overlord can express (sessions, groups, state…)
  and connects you to the right pane, using what rlord already knows
  (the overlord interface, the daemon's RPC). Pane bytes come from the
  existing tmux pieces (the spike's bridge).

### Transit (relay)

- Zero-trust, end-to-end encrypted. A hosted relay run by the user, and
  the same server easy for anyone to self-host on their own
  infrastructure while staying E2E encrypted.
- Two sets of keys:
  1. **Content key(s):** held only by the rlord process and the devices
     the user adds; never sent to the relay. Shared by QR code: rlord
     shows it, the phone app scans it and keeps it on the device.
  2. **Identity key(s):** registered with the relay for auth and to prove
     you're talking to the right server/instance. Established seamlessly.
     The phone's identity key is also registered by rlord at pairing (via
     the same QR flow).
- Transport: TLS or mTLS, configurable.
- Push-only on both sides: rlord and the phone only make outbound
  connections. The relay routes opaque messages to the right rlord
  instance or device; it never knows the content.
- Efficient polling: a tiny "do I have something for you?" signal
  (announce/lookup by either side) once trust is established; heavier
  transfer only after that.

## Notes and proposed refinements (Claude)

- **Per-device keys, not one shared content key.** The QR carries rlord's
  public key plus a one-time pairing secret; the phone makes its own
  keypair and they run an authenticated handshake (Noise `XX`/`IK` with
  the pairing secret as PSK, Rust: `snow`). Gives forward secrecy and
  revoking one lost phone without re-keying the rest. Same QR UX.
- **One keypair can do both jobs** per party (Noise static key = content
  identity) while the relay identity stays separate, as specced, so the
  relay can't link relay auth to content keys. mTLS client certs can be
  the relay identity keys: one mechanism for "TLS or mTLS".
- **Two traffic shapes:** an async **mailbox** (overlord messages, state
  changes, "something for you") and a **live channel** (a pane's byte
  stream, low latency). Polling suits the first only; a live pane needs a
  relayed websocket carrying encrypted frames.
- **Phone wake-ups need the platform push service** (APNs/FCM) once the
  app is backgrounded; polling is killed by iOS. The push carries nothing
  but "wake up". A self-hosted relay then needs its own APNs/FCM
  credentials, or forwards wake-ups through a small push gateway (the
  Matrix/ntfy pattern).
- **Metadata still leaks to the relay:** who talks to whom, when, how
  much. Opaque mailbox ids and padded frames reduce it; worth stating
  plainly in docs.
- **Browser clients are the weak spot of E2E:** a web app served by the
  relay can be swapped for one that steals keys. The web UI's JS must come
  from rlord itself (local) or from a signed native/installed app, never
  from the relay.
- **Self-hosting:** relay as one static binary (`rlord relay`) plus a
  container image; config is a domain, TLS cert (or ACME), and a storage
  path. No database beyond a small embedded store for mailboxes.

## Open questions

Decided 2026-11-17 (user):

- **Phone client:** a native app (keys in the secure enclave/keystore,
  APNs/FCM wake-ups).
- **Remote scope:** both, chat first: mailbox for overlord chat and
  session state first, live pane streaming over a relayed channel after.
- **Key model:** per-device keys with a Noise handshake; the QR carries
  rlord's public key and a one-time pairing secret.
- **First slice:** the local web UI, served by the rlord daemon on
  localhost. No relay yet.

## Progress

- Spike: local PTY bridge works (`web-ui-spike.md`, `spikes/web-ui/`).
- Slice 1 planned (see below). Plan by a Fable planning agent.
- 2026-11-17, user: the web server will not live in the daemon. It becomes
  a separate opt-in executable (a plugin, downloaded only on opt-in); the
  main `rlord` binary keeps a thin `rlord web` command and no HTTP,
  websocket or network code. The plan below still puts it in the daemon
  (steps 3 to 12) and needs revising before those steps. Steps 1 and 2
  (tmux and `send.rs`, in core/daemon) stand.
- Step 1 done: the view session holds on tmux 3.8 (macOS), 3.2a (Ubuntu
  22.04) and 3.4 (Ubuntu 24.04, CI's). All seven behaviours pass
  (`crates/daemon/tests/web_tmux.rs`, gated, socket `rlord-test-web`). The
  PTY in the test is plain `openpty` via `libc`, no PTY crate. Found on
  the way: tmux exits with its last session, so a test (or the sweep) can't
  rely on bindings or window ids surviving a moment with no sessions.
- Step 2 done (`core/src/tmux.rs`, `daemon/src/send.rs`,
  `daemon/src/overlord.rs`): `VIEW_PREFIX`, `WEB_TABLE`, `view_command`
  (checks the target exists, then one attached invocation),
  `bind_web_table` (root's pane mouse bindings), `view_shows` (window-id
  check), `sweep_views`; `list_clients` drops view clients; `kill_session`
  kills the target's views first (found by session group, so a renamed
  target still finds them). Every session-ending path goes through
  `kill_session` (stop, archive, the overlord migration) or kills the
  server (`rlord reset --hard`). Gated test `view_helpers`, also on tmux
  3.2a and 3.4. Differences from the plan:
  - `user_active` counts a browser through tmux itself, not a registry
    of web input: `tmux::view_clients(t)` returns the view clients of
    `t`'s group as clients of `t`, with tmux's `client_activity`. That
    works whichever process runs the bridge (see the placement change
    above), with no new IPC.
  - The overlord's idle fresh start (`overlord.rs`, "someone is looking
    at it") also counts a browser's view, for the same reason the
    terminal check exists.
  - Owed to the bridge: call `bind_web_table` before attaching a view
    (root changes on reload and goes with the server), and
    `sweep_views` on its start. Neither is called by the daemon.

## Slice 1 plan: local web UI

Planned 2026-11-17. Nothing here is built. The tmux behaviours marked
"verify" were reasoned from tmux's documentation and source, not run:
step 1 proves them on a scratch socket before anything depends on them.

### Decisions

#### Where it lives, how it starts, how the user opens it

- **In the daemon process**, as a module tree in the daemon crate:
  `crates/daemon/src/web/`. Not a subcommand process and not a new crate.
  The daemon already knows the active overlord and when it switches, owns
  the change broadcast (`Daemon::changed`), and holds the session locks the
  pane bridge must respect. A module keeps `pub(crate)` access to `Daemon`
  and adds nothing to the crates.io publish list (`xtask/src/publish.rs`).
  Embedded assets must sit inside the crate directory, because
  `cargo publish` packages only that.
- Layout:
  - `web/mod.rs`: lifecycle (`Web` state on `Daemon`, `start`, `stop`,
    `status`), the token.
  - `web/api.rs`: the message layer. No HTTP or websocket types in it.
  - `web/http.rs`: axum routes, Host/Origin checks, headers, static files.
  - `web/pane.rs`: the PTY bridge, view sessions, the registry of open
    panes.
  - `web/assets.rs` and `web/assets/`: the embedded page, scripts, styles
    and vendored xterm.js.
- **Off by default.** The daemon has never opened a TCP port
  (`docs/architecture.md`, "Internal API"); doing so silently on upgrade
  changes its security posture. Config:

  ```toml
  [web]
  enabled = false   # true: start the listener with the daemon
  port = 47811      # 127.0.0.1 only; 0 = any free port
  ```

  A fixed port matters: the browser stores the token per origin, and the
  origin includes the port.
- **Start and stop.** `web::start` runs in `run_daemon`
  (`crates/rlord/src/main.rs`) when `enabled`. Otherwise the first
  `rlord web` starts it for this daemon run. The listener shuts down
  gracefully on `web.stop` and with the daemon. A port already in use is
  logged and reported by `web.status`; it never stops the daemon. A config
  change applies on the next daemon start or `rlord web`.
- **Opening it.** New CLI command:
  - `rlord web`: start if needed, open the browser at the token URL
    (`open` on macOS, `xdg-open` elsewhere), print the URL if that fails.
  - `rlord web --print`: print the URL only.
  - `rlord web --stop`: stop the listener.
  - `rlord web --new-token`: replace the token and close every open web
    connection.
  These call new daemon methods `web.status`, `web.start`, `web.stop`,
  `web.rotate` over the Unix socket (in `Daemon::call_ui`, own locking).
  They are never reachable from the web API or MCP. A `:web` command in
  the list is the last, optional step.

#### Auth, headers, assets

- **Bind** `127.0.0.1` only (not `::1`, not `0.0.0.0`).
- **Token.** 32 random bytes from `/dev/urandom`, hex. Stored in
  `<data dir>/web.token` with `rlord_core::private::write` (0600), reused
  across restarts so open pages reconnect.
- **No cookies.** Cookies for `127.0.0.1` are shared by every port, so any
  other local web server the browser visits would receive ours. The token
  travels in the URL fragment: `http://127.0.0.1:<port>/#t=<token>`. A
  fragment is never sent to the server or in a Referer. The page stores it
  in `localStorage` (per origin, so per port) and removes it from the
  address bar with `history.replaceState`.
- **Websockets authenticate in their first frame** (`{"auth":"<token>"}`),
  constant-time compare, 5 second deadline; failure closes with code 4401.
  A browser can't set headers on a websocket, and "first frame
  authenticates" is the shape the relay's Noise handshake will have.
- **Static files need no token**: open-source bytes, no secrets. Everything
  that reads or changes state goes over a websocket. No REST endpoints, so
  no CSRF surface beyond the websocket upgrade.
- **Host check on every request**: exactly `127.0.0.1:<port>`, else 404
  (DNS rebinding). **Origin check on every upgrade**: exactly
  `http://127.0.0.1:<port>`, else 403. `localhost` is refused on purpose.
- **Headers** on every response:
  - `Content-Security-Policy: default-src 'none'; script-src 'self';
    style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src
    'self'; connect-src 'self' ws://127.0.0.1:<port>; base-uri 'none';
    form-action 'none'; frame-ancestors 'none'`
  - `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`,
    `Cross-Origin-Opener-Policy: same-origin`,
    `Cross-Origin-Resource-Policy: same-origin`, `Cache-Control: no-store`.
  `style-src 'unsafe-inline'` only because xterm.js creates `<style>`
  elements at run time. Scripts stay strict.
- **Limits**: 16 control sockets, 32 pane sockets, 64 KiB per pane input
  frame, 1 MiB per control frame.
- **Assets embedded** with `include_bytes!`, from a fixed table (path,
  content type, bytes). No CDN. `RLORD_WEB_ASSETS=<dir>` reads them from
  disk for development.
- **xterm.js vendored**: `@xterm/xterm` 5.5.0 and `@xterm/addon-fit` 0.10.0
  (the spike's versions), minified `xterm.js`, `xterm.css`, `addon-fit.js`
  plus `LICENSE`, under `crates/daemon/src/web/assets/vendor/xterm/`. MIT,
  compatible with `MIT OR Apache-2.0`. `cargo xtask vendor-web` downloads
  the npm tarballs at pinned versions and checks pinned SHA-512 sums (in
  `xtask/src/tools.rs`). A test checks committed files against the sums.
  No npm, no bundler.

#### Pane streaming: a tmux client per view, through a view session

**Chosen: the spike's bridge (one tmux client on a PTY per websocket),
attached to a throwaway grouped session instead of the target itself.**

Rejected:

- **Control mode (`tmux -C`).** Bare pane, no status row, no key tables.
  But the bridge would rebuild terminal state for a view joining
  mid-stream (alternate screen, cursor-key mode, mouse modes, bracketed
  paste), decode `%output`, re-encode input via `send-keys -H`, and add
  flow control. The daemon has no control-mode client today.
- **capture-pane frames plus `send-keys`** (the TUI overlay,
  `tmux::capture_frame`). A browser terminal sends raw bytes, not named
  keys; scrollback and mouse need hand-written handling again; whole-screen
  frames are the wrong shape for a relayed byte channel.

The view session:

- Per pane websocket, one tmux command in the PTY creates a session
  grouped with the target and attaches: `new-session -t <target> -s
  webview-<n>`, then in the same invocation `set-option status off`,
  `set-option key-table rlord-web`, `set-option destroy-unattached on`.
- **Status row gone**: `status` is a session option.
- **Navigation gone**: the `rlord-web` key table has no bindings, and tmux
  doesn't fall back to root from a session's default table, so nav keys,
  the user's profile and the prefix are unreachable (as the key screen does,
  `tmux::set_key_table`). The table gets copies of root's pane mouse
  bindings (`Wheel*Pane`, `Mouse*Pane`, double/triple click).
- **Cleanup**: `destroy-unattached` removes it when the client exits. On
  start, kill leftover `webview-*` sessions with no client.

What the view session forces elsewhere (step 2):

- `tmux::list_clients` (`crates/core/src/tmux.rs`) must drop view-session
  clients, or `tmux::user_client` picks the browser as "the user's
  terminal" and `ui.goto` / `show_list` move it.
- `send::user_active` (`daemon/src/send.rs`) must count recent web input,
  or rlord pastes a message while the user types in the browser.
- `tmux::kill_session` must kill grouped view sessions too: a window lives
  while any session links it, so a stopped agent would keep running.
- `new-session -t <name>` on a missing name creates a shell; tmux refuses
  a command next to `-t`. Check `has_session` first, and after attaching
  confirm the view session's window id equals the target's.

Sizing:

- Rule stays tmux's `window-size latest`.
- The page opens a pane websocket only while its view is visible and
  closes it shortly after the tab is hidden: one client per visible view,
  background tabs never take the size.
- The TUI overlay pins the overlord's window to its popup
  (`tmux::resize_window`); a web view meanwhile shows it cropped or padded.
  Accepted in this slice, as is two visible views taking turns.

Other bridge rules:

- Opening a pane never starts anything. Not running: typed "not running",
  page shows Resume. A reconnect loop can't respawn agents.
- Target resolved on every open: `overlord` means `overlord::session`,
  else a session id via `resolve_ref`. A `changed` event showing a new
  active overlord makes the page reopen the pane.
- PTY command from `tmux::command()` (socket, config file, locale, `TMUX`
  removed), with `TERM=xterm-256color`, `COLORTERM=truecolor`.
- Copy: xterm.js selection with the modifier held
  (`macOptionClickForcesSelection`). OSC 52 is a follow-up.

#### The API layer (what the relay will reuse)

`web/api.rs` knows nothing of HTTP:

- **Requests.** `{"id", "op", "args"}` in, `{"id", "ok"}` or `{"id",
  "error": {"code", "message"}}` out, via `api::handle(&Daemon, Request)`.
  `op` from a fixed allow-list mapped to daemon methods; not a
  pass-through. `admin.reset`, `mcp.*`, `hook.*`, `web.*`, `config.*` and
  `ui.*` are off it.
- **Events.** `api::events(&Daemon)` wraps `Daemon::notify`: `{"event":
  "changed"}`, at most one per 100 ms; a lagged receiver counts as a
  change. `ui` commands not forwarded in this slice.
- **Pane messages.** `Open {target, cols, rows}`, `Input(bytes)`,
  `Resize`, `Close` in; `Opened`, `Output(bytes)`, `Closed {reason}` out.

`snapshot` returns everything the list needs in one answer (rows for the
asked view, same query object as `session.list`; status labels; groups with
counts and favourites; active overlord and roster) and a `rev`. A client
passing its last `rev` gets `{"unchanged": true}`. Grouping, sorting and
labels are computed in Rust; the page only draws.

| Layer | Slice 1 | Relay later |
| --- | --- | --- |
| Requests + events | one control websocket, JSON text frames | the mailbox; `changed` + `rev` is the "anything for me?" signal |
| Pane messages | one websocket per pane; binary = bytes, text = control | the relayed live channel, same messages in Noise frames |
| Auth | token in the first frame | Noise handshake as the first frames |

#### The frontend

- **No framework, no build step.** Hand-written ES modules embedded as
  they are: `index.html`, `app.css`, `app.js`, `api.js`, `list.js`,
  `pane.js`. Testable logic lives in Rust behind `snapshot`.
- **Live updates.** `changed` pushed; page debounces and calls `snapshot`
  with its view and `rev`. `hello` on connect carries the build id; a page
  from an older build reloads.
- **Reconnect** as in the spike; `term.reset()` on each pane connect.
- **Views** (hash routes; wide screens list beside pane, narrow one at a
  time):
  1. **List** (`#/`): overlord row on top, then sessions (status, name,
     agent, tags, description, last update). Sessions/Groups tabs, filter,
     sort, live/archived. View state is the browser's own.
  2. **Overlord** (`#/overlord`): active overlord's pane, status, agent
     switcher.
  3. **Session** (`#/s/<id>`): header (name, status, agent, model, branch,
     description, tags), the pane, or Resume when stopped.
  Nothing in the page says "tmux".
- **Actions, in order:** (1) open a session's / the overlord's pane
  (`overlord.ensure` when asked); (2) filter, sort, tabs, archived;
  (3) resume, restart, stop; (4) "tell the overlord" box (`overlord.ask`);
  (5) archive, unarchive; (6) rename, describe, notes, tags, pin;
  (7) switch the active overlord's agent. Not in this slice: new-session,
  fork and setup forms, key screen, usage, sub-agents, history search,
  Remote Control, limit resume.

#### The overlord's instructions

Nothing new for the overlord: no MCP tool, meta-context or list concept.
`overlord.md` and `mcp.rs` unchanged; web clients are filtered from
`list_clients`, so `get_ui_state` answers as today. For a web-only user,
`goto`, `show_list`, `set_view` and `propose_session` fail with "no
terminal is attached to tmux", as they do today with no terminal. Making
the web UI a target for them is the next slice (changes `ui.state` and
`overlord.md`).

#### Tests

Never `tmux -L rlord` or the default server, no real agent, no model.

- **Unit:** token file (0600, reused, rotate), constant-time compare,
  Host/Origin checks; op allow-list reaches real methods and forbidden
  families don't; `snapshot` shape and `rev` stability; assets (every file
  `index.html` names exists, no outside URLs, CSP names no outside host,
  vendored sums); pane frame bounds; `list_clients` drops view clients;
  `user_active` counts web input.
- **Integration, no tmux** (`crates/daemon/tests/web_http.rs`,
  `127.0.0.1:0`, in-memory `Daemon`): wrong Host 404; bad/no Origin 403;
  wrong token 4401; right token `hello`; `snapshot` answers; a change gives
  `changed`; off-list op errors.
- **Gated tmux** (`RLORD_TMUX_TESTS=1`, own test file, socket
  `rlord-test-web`): step 1's behaviours; end to end against `cat`: bytes
  echo, resize changes width, no status row, a root binding doesn't fire
  and reaches the pane, `list_clients` hides the web client, killing the
  target closes the socket and leaves no view session.
- **By hand** against `scripts/demo/demo.sh` in a real browser: drawing,
  fit, focus, Option as Meta, browser-kept keys, wheel, selection/copy, two
  tabs, TUI overlay open at once, daemon restart, overlord switch, phone
  width.

### Implementation steps

One commit each. The PR bumps the version to the next `-dev.N` and
regenerates `Cargo.lock`.

1. **Prove the view session** on a scratch socket (gated test, small
   `tmux.rs` helpers): grouped session shows the target's pane; `status
   off` leaves the target's on; empty key table blocks root bindings and
   passes the key; unbound mouse reaches a mouse app; `destroy-unattached`
   cleans up; killing the target alone leaves the window alive;
   `new-session -t` on a missing name makes a shell. If any fails on tmux
   3.2 or CI's tmux, stop and report. Fallback: attach to the target
   directly, refuse `rlord nav` from web clients, accept the status row.
2. **tmux helpers and side effects** (`tmux.rs`, `send.rs`): view prefix,
   view-session command, `rlord-web` table with mouse bindings, window-id
   check, leftover sweep; `list_clients`, `kill_session`, `user_active`
   changes. Check every place a tmux session ends (`methods.rs` stop and
   archive, `overlord.rs`, `admin.reset`).
3. **Config and token** (`config.rs`, `web/mod.rs`): `[web]`, token file,
   `Web` state on `Daemon`, `Daemon::subscribe`.
4. **API layer** (`web/api.rs`): types, allow-list, `snapshot` + `rev`,
   coalesced events. Move `group_list` from `crates/tui/src/groups.rs` into
   `rlord_core`. Extract "make sure this session runs" from `ui::goto` for
   Resume.
5. **HTTP and control socket** (`web/http.rs`, `web/assets.rs`; `axum`
   with `ws`, `futures-util`): static files, headers, checks, first-frame
   auth, limits, `hello`, requests and events, graceful shutdown;
   placeholder page; no-tmux integration tests.
6. **Lifecycle and CLI** (`lib.rs`, `crates/rlord/src/main.rs`): `web.*`
   methods, start when enabled, `rlord web` and flags, browser opener.
7. **Pane bridge** (`web/pane.rs`; `portable-pty`): resolution, PTY and
   view session, registry (open panes, last input per session), pane
   websocket, cleanup; gated end-to-end tests.
8. **Vendor xterm.js** (`xtask/src/vendor_web.rs`, sums in
   `xtask/src/tools.rs`) with the checksum test.
9. **Page: list** (`index.html`, `app.css`, `app.js`, `api.js`,
   `list.js`).
10. **Page: panes** (`pane.js`, overlord and session views, visible-only
    connections, reconnect, Resume, follow overlord switch), then the
    by-hand browser pass.
11. **Actions 3 to 7**, each op on the allow-list with a test.
12. **Docs and cleanup**: `docs/architecture.md` "Web UI" section (fix "no
    TCP listener at all in v1"), `docs/guide.md`, README, config template.
    Delete `spikes/web-ui/`, fold `web-ui-spike.md` in. Optional `:web`.

### Risks

- The view session rests on tmux details not yet run (step 1 has the
  fallback).
- A missed kill path leaves an agent running under a view session.
- A first TCP listener: Host, Origin, first-frame auth and CSP stand
  between a hostile page and the user's agents; most tests go there.
- Browser behaviour unverified (Cmd-W/Cmd-T, Option as Meta, IME).
- New dependencies (`axum`, `hyper`, `tokio-tungstenite`, `portable-pty`)
  grow the daemon and `deny.toml` surface; `openpty` via `libc` can replace
  `portable-pty`.
- Size mismatches between overlay and web views look odd; accepted.
- Daemon restart drops every pane; the start-up sweep cleans leftovers.

### Answers (user, 2026-11-17)

1. Off by default; `rlord web` or `[web] enabled = true` starts it.
2. Port 47811 (default taken, not asked).
3. Web as a UI-command target: next slice.
4. Resume button, never auto-resume.
5. Latest active view sets the size.
6. Delete `spikes/web-ui/` and fold the spike doc in at step 12 (default).
7. `:web` last and optional (default).

### Changes from the user after planning (2026-11-17); slice 1 plan needs revising

- **Web server is an opt-in plugin, not part of the daemon.** Main `rlord`
  keeps only a thin `rlord web` command section with no HTTP, websocket or
  network code. Opting in downloads a separate `rlord-web` executable;
  the point is that a user who doesn't opt in has nothing that could send
  sessions over the network. The plugin talks to the daemon over its Unix
  socket (needs `snapshot`, a streaming subscribe, web-input reporting,
  a protocol version handshake). tmux-side helpers (view sessions,
  `list_clients` filter, `kill_session`) stay in core/daemon.
- **Server side all Rust.** No JavaScript server.
- **Client in TypeScript**, not plain JavaScript. Build compiles it to
  the embedded assets; tooling choice (and how type-checking runs in CI)
  to be planned.
- **Most modern web stack available.** The re-plan must research the
  current (late 2026) state rather than assume: TypeScript's native
  compiler (TS 7 / `tsgo`), Rolldown/Vite or oxc for building, oxlint or
  Biome for lint/format, the current xterm.js major, and whether a light
  UI framework (signals-based) beats hand-written DOM. Pick the newest
  stable choices, pinned, with reasons.
