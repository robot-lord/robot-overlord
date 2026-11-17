**Status:** in progress (slice 1: steps 1–12 built; steps 13–14 not started)

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

## Network invariant: everyone dials out to the relay

This is the premise of the remote design, not an option:

- **The rlord host never accepts an incoming connection from the network.**
  Assume it sits behind NAT, a corporate firewall, or a laptop on hotel
  wifi. Its only listener is `rlord-web` on `127.0.0.1` for the local
  browser. Every remote path starts with the host opening an outbound TCP
  (TLS) connection to the relay and keeping it open.
- **The relay is the only publicly reachable piece.** A server with a
  domain and a TLS certificate: either the hosted one (run by the project
  author) or one a user or company runs themselves. It is a separate
  binary (`rlord-relay`), never part of `rlord`.
- **Devices also dial out to the relay.** The phone never connects to the
  host directly. No port forwarding, no UPnP, no direct peer-to-peer in v1
  (a hole-punched direct path is a possible later optimisation, never a
  requirement).
- **The relay splices outbound connections.** Host and device each hold a
  connection to the relay; the relay matches them by their relay identity
  and forwards opaque Noise frames between them. "Anything for me?"
  signals, mailbox delivery and live pane streams all ride on connections
  the host and device opened themselves.
- The relay sees who is connected and traffic metadata, never content
  (see `e2e-crypto.md`).

## Network invariant: one pinned relay per host

An rlord host set up for one relay can never be reached through another,
and neither the host nor its devices will ever talk to another. This
matters most for self-hosted, on-prem relays.

- **The relay is part of the host's identity.** The host's config names
  one relay: its origin (`https://relay.example.com`) and the relay's own
  identity public key. The relay proves it holds that key on every
  connection (a signed challenge inside TLS). Pin the relay identity key,
  not the TLS certificate, so ACME renewals don't break the pin; the TLS
  certificate must still validate for the configured domain.
- **Pairing carries the pin.** The QR holds the relay origin and the relay
  key fingerprint alongside the host's keys. A device stores the pin per
  paired host and only ever dials that relay for that host.
- **Bound into the handshake.** The relay origin and relay key are in the
  Noise prologue of pairing and reconnect. A handshake routed through any
  other relay fails cryptographically on both ends, even if that relay
  forwards bytes faithfully.
- **No way to be moved.** No relay-supplied redirects, relay lists,
  fallbacks or discovery; HTTP redirects are refused. Moving a host to a
  different relay is a user action on the host that re-pairs every
  device; old pairings stop working.
- **The relay enforces it too:** a relay accepts only host identities
  registered with it, so a host can't be registered with two relays at
  once by accident.

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
- **Self-hosting:** relay as one static binary (`rlord-relay`, separate from `rlord`) plus a
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
- **First slice:** the local web UI on localhost. No relay yet. (First
  planned inside the daemon; now an opt-in plugin, see the slice 1 plan.)

## Progress

- Spike: local PTY bridge works (`web-ui-spike.md`, `spikes/web-ui/`).
- Slice 1 planned (see below). Plan by a Fable planning agent.
- 2026-11-17, user: the web server will not live in the daemon. It becomes
  a separate opt-in executable (a plugin, downloaded only on opt-in); the
  main `rlord` binary keeps a thin `rlord web` command and no HTTP,
  websocket or network code. Steps 1 and 2 (tmux and `send.rs`, in
  core/daemon) stand.
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
- Slice 1 replanned for the plugin (Fable planning agent, 2026-11-17):
  `rlord-web` in `crates/web`, supervised by the daemon, verified by a
  hash compiled into `rlord`; TypeScript 7, Vite 8, Preact 11 and
  xterm.js 6. Steps 3 to 14 below replace the old 3 to 12; eight open
  questions wait for the user.
- Step 3 done: `crates/web` (`rlord-web --version`; `serve` says
  `web.hello` and waits for the daemon to go), in `members` only;
  `core/src/plugin.rs` (`WEB_PROTOCOL`, exit codes 3 and 4, the
  incompatible-protocol RPC code, paths, `verify`, `locate`,
  `random_hex`; `sha2`); `[web] enabled/port` in `config.rs` (the config
  template waits for step 14); `cargo xtask check-deps`, run by CI's
  `check` job and at the start of `build-release`. Deviation: the source
  check found a TCP client already in `rlord`:
  `core/src/agents/opencode_service.rs`, the OpenCode adapter's HTTP
  client for OpenCode's own background server (the local address its
  `service.json` names). It is exempt by file in `ALLOWED_FILES`, with
  that reason; any other file naming a TCP or UDP socket type fails. So
  "no network code in `rlord`" is true of HTTP servers, websockets, TLS
  and network-client crates, not of TCP itself; step 14's docs should
  say so.
- Step 4 done (`daemon/src/web.rs`, `lib.rs`): `web.hello`, `web.call`
  (ops `snapshot`, `session.get`, `overlord.ensure`; anything else gets
  RPC code `plugin::RPC_ERR_NOT_ALLOWED`, and no op may name a forbidden
  family), `web.subscribe` (one push loop with `events.subscribe`,
  `Daemon::push_loop`), `web.pane_target`; `rev` is
  `<boot id>-<changes>` from `Daemon::changed()`. `group_list` and
  friends moved to `rlord_core::groups`; `ui::ensure_running` came out of
  `ui::goto` (not yet an op: Resume is step 11's). Fixture
  `crates/web/ui/test/fixtures/snapshot.json`, checked by
  `crates/daemon/tests/web_api.rs` (regenerate with
  `RLORD_UPDATE_FIXTURES=1`). Choices within the plan:
  - Snapshot: `{rev, rows, groups: [{tag, label, count, favourite}],
    overlord, statuses: [{status, label}]}`; every row (and the overlord
    and its roster entries) gets `status_label`. `overlord` is
    `overlord.get` as is (its `overlords` is the roster), null before
    setup. The view is forced to `include_overlord: false`.
  - Typed errors: `web::Incompatible` and `web::NotAllowed`, mapped to
    their RPC codes in `dispatch` by downcast.
  - The `web.subscribe` ack carries the current `rev`.
- Step 5 done (`daemon/src/web.rs` `Web`, `lib.rs`, `rlord/src/main.rs`):
  the daemon spawns `<plugin> serve` (output appended to `web.log`), waits
  up to 5 s for its hello, restarts it with backoff (1 s doubling to
  60 s, given up after five failures in a row), doesn't retry exit codes
  3 and 4, and stops it with SIGTERM then SIGKILL after 2 s (`web.stop`,
  and at daemon exit in `run_daemon`). `web.status`, `web.start`,
  `web.stop`, `web.rotate`; `[web] enabled` starts it right after the
  socket is bound. `rlord web` (start and open the browser, else print
  the URL), `url`, `status`, `start`, `stop`, `new-token`. Tests:
  `crates/daemon/tests/web_supervise.rs`, whose stand-in is a shell
  script running that test binary's own `standin` test in a mode.
  Choices within the plan:
  - A hello counts only from the process the daemon started (by pid); a
    hello from any other process is answered but changes nothing.
  - The failure count resets on an explicit start and after a run of
    60 s or more, not on hello (a plugin that says hello and then dies
    would otherwise retry every second forever).
  - With a compiled-in hash, `RLORD_WEB_BIN` and a plugin beside the
    executable are ignored (only the installed, matching plugin runs).
    Tests swap the expected hash with `Web::set_expected_hash`
    (in-process only, not reachable over the socket).
  - Until step 12, `rlord web` with no plugin fails naming
    `rlord web install` (which doesn't exist yet); the opt-in prompt is
    step 12's.
- Step 6 done (`crates/web/src/{main,daemon,token,http,assets}.rs`,
  `build.rs`): the plugin reads `[web] port`, makes or reuses the token,
  binds 127.0.0.1 (exit 4 if taken), says hello with its port (exit 3 on
  another protocol), subscribes, serves, and exits 0 when the
  subscription ends. Host check and headers on every response, Origin
  check on upgrades, first-frame auth (4401), 16 control sockets, 1 MiB
  control frames, the control socket's `hello`, requests and `changed`
  events. Assets embedded by `build.rs` from `ui/dist/`, else the
  placeholder page; `RLORD_WEB_REQUIRE_ASSETS=1` and debug-only
  `RLORD_WEB_ASSETS` as planned. Tests: unit tests in each module, and
  `crates/web/tests/http.rs` (the real binary started by an in-memory
  daemon's supervisor, port 0, no tmux). Choices within the plan:
  - Browser messages: the hello is `{"event":"hello","version","build",
    "rev"}`; error codes are strings: `not_allowed`, `failed`,
    `bad_request`, `daemon_unavailable`.
  - The build id is the version plus an FNV-1a hash of the asset table
    (no hashing crate in the build script).
  - Without `dist/`, `build.rs` watches `ui/` (a missing path would
    rerun it on every build).
  - A frame over the limit ends the socket (tungstenite's error); there
    is no 1009 close frame to rely on.
  - `web.start` returns at hello, which comes just before the plugin
    subscribes and serves; the listener is already bound then, so an
    early browser connection waits in the backlog rather than failing.
- Step 7 done (`crates/web/src/pane.rs`): the pane socket at `/pane`
  (Origin check, token first frame, 32 sockets, 64 KiB frames). The page
  sends `{"type":"open","target","cols","rows"}`; the plugin asks
  `web.pane_target`, refuses a target that isn't running (`closed`,
  reason `not_running`; an unknown one `not_found`), calls
  `bind_web_table`, runs `view_command` on an `openpty` PTY (tokio
  `AsyncFd`, no PTY crate), waits for `view_shows`, and answers
  `{"type":"opened","target","id","name"}`. Then binary frames are bytes
  both ways; `{"type":"resize"}` sets the PTY size; `{"type":"close"}`,
  the socket going, or the client exiting (`closed`, reason `ended`) ends
  it: the tmux client gets SIGTERM (SIGKILL after 1 s) and a view
  session still there is killed. `sweep_views` runs at plugin start.
  View names are `webview-<pid>-<n>`. Gated test
  `crates/web/tests/pane.rs` (socket `rlord-test-web`): sweep at start,
  not running, open, no status row, typing in and output back, a root
  binding doesn't fire, `list_clients` hides the view, resize, two views
  at once, close, and killing the target closes the socket with no view
  left. The plugin's integration tests share `tests/common/mod.rs`.
  Run on macOS tmux 3.8, and later (during step 11) on Linux tmux 3.2a
  (Ubuntu 22.04) and 3.4 (Ubuntu 24.04): `pane.rs` and `http.rs` pass on
  both. The test binaries were cross-built with `cargo zigbuild --target
  aarch64-unknown-linux-musl -p robot-overlord-web --tests --bins` (it
  takes no `--no-run`) and copied into the containers with `docker cp` at
  the same absolute paths (`CARGO_BIN_EXE_rlord-web` and
  `CARGO_TARGET_TMPDIR` are compiled in; colima doesn't share
  `/private/tmp`, so a bind mount shows empty folders).
- End-to-end check of steps 3 to 7 against `scripts/demo/demo.sh`
  (2026-11-17, release `rlord-web` copied beside the demo's `rlord`,
  unverified; started by `rlord web url`): a scripted websocket client
  got the control `hello` and a snapshot with the overlord; opened the
  overlord's pane at 100x30 and got its screen; the view session had
  `status off` (the overlord's own stays on) and a 100x30 window, and no
  status-row text was streamed; typed bytes reached the overlord's pane
  and came back in the stream; closing left no view session. Stopping
  the demo stopped the plugin with its daemon. A real browser is still
  step 10's by-hand pass.

- Step 8 done (`crates/web/ui/`, `xtask/src/web.rs`, `ci.yml`,
  `release.yml`, `scripts/demo/demo.sh`): every version in the stack
  table re-read from the npm registry on the day and all unchanged
  (`typescript` 7.0.2, `vite` 8.3.8, `oxlint` 1.94.0, `oxlint-tsgolint`
  7.0.2007, `@biomejs/biome` 2.6.8, `preact` 11.2.0, `@preact/signals`
  2.11.4, `@xterm/xterm` 6.0.0, `@xterm/addon-fit` 0.11.0,
  `@xterm/addon-webgl` 0.19.0, `vitest` 5.1.0); Node 26.13.1 in
  `.node-version` (current LTS). `npm ci --ignore-scripts` leaves every
  tool working: their native binaries (tsc, rolldown, oxlint, tsgolint,
  biome) come as optional platform packages, with no install script. The
  lockfile lists every platform's binary, so the Mac lockfile serves
  Linux CI. `cargo xtask web install|check|build|watch` run the tools from
  `node_modules/.bin` (never `npx`, which could fetch); `check` and `build`
  install first when `node_modules` is older than the lockfile. CI's `web`
  job: install, check, build, then `cargo test -p robot-overlord-web`
  with `RLORD_WEB_REQUIRE_ASSETS=1`. Release's `web-assets` job builds and
  uploads the page as artifact `web-page` (not `dist-*`, which the release
  job merges into the release files); `release` needs it. The build jobs
  don't take it yet: that wiring (`RLORD_WEB_DIST`) is step 13's.
  `actions/setup-node` v7.1.0 pinned by commit, `package-manager-cache:
  false`. `demo.sh --web` builds the page and the release `rlord-web` and
  puts it beside the demo's `rlord`. Choices within the plan:
  - The Node check wants `.node-version`'s major; another minor or patch
    only gets a note (this Mac has 26.13.0; CI and the release job use the
    exact one through `node-version-file`).
  - Lint: oxlint's correctness rules as errors, suspicious as warnings
    (`--deny-warnings` fails both), plus the type-aware promise rules
    (`no-floating-promises`, `no-misused-promises`, `await-thenable`) and
    `switch-exhaustiveness-check`. Off: `prefer-add-event-listener` (a
    socket has one handler per event, set on purpose) and
    `no-unsafe-type-assertion` (the plugin's JSON is typed by `proto.ts`).
  - Biome formats with 2 spaces and 100 columns; its linter is off (oxlint
    lints). The fixture is left as the Rust test writes it.
  - `vite.config.ts`: `assetsInlineLimit: 0` (no `data:` assets), no
    module-preload polyfill, no source maps. Vite reads `jsxImportSource`
    from `tsconfig.json`: no JSX plugin.
- Step 9 done (`ui/src/{proto,api,store,route,main}.ts(x)`,
  `views/List.tsx`, `components/`): the list with the overlord on top,
  Sessions and Groups tabs, filter (the query's free-text `search`,
  200 ms after typing), sort (the six `SortMode`s) with reverse,
  live/archived/both; status, name, agent, tags, description, last update,
  pin and usage-limit marks. The view is kept in `localStorage`. A
  `changed` is debounced 100 ms; `snapshot` passes the shown `rev` only
  when the query is the one that snapshot answers; one request at a time,
  with one more for whatever came in meanwhile; an answer for a view that
  has since changed is dropped. `test/fixture.test.ts` holds `proto.ts`
  to the fixture by `tsc`: the fixture must fit the types (strings
  widened, as JSON imports type them) and have exactly their required
  keys; it found the overlord's `limit`. Vitest: `api`, `store`, routes,
  times. Choices within the plan:
  - The Groups tab draws the picked group's rows from the snapshot's rows
    (as the TUI does, `in_group`); the daemon still makes the groups and
    their order. Untagged is the group with tag `""`.
  - The filter is free text only: `session.list`'s `search`. The TUI's
    `tag:` / `status:` tokens are parsed in Rust (`apply_search_text`) and
    not reachable through `snapshot`; worth adding to `snapshot` later if
    wanted.
  - Agent names ("Claude Code") come from the overlord's roster labels.
  - Hash routes `#/`, `#/overlord`, `#/s/<id>`; wide screens (900 px and
    up) show the list beside the main pane, narrow ones one at a time.
  - The build id: the first `hello` sets it; a later `hello` with another
    reloads the page.
  - Checked in headless Chromium and WebKit (Playwright 1.65.1, outside
    the repo) against `demo.sh --web`: nine demo rows and the overlord
    row, no console errors or CSP reports.
- Step 10 done (`ui/src/pane.ts`, `components/Pane.tsx`,
  `views/{Overlord,Session}.tsx`): the overlord's and a session's views,
  each a header and one pane. `PaneSocket` (no xterm.js; Vitest-tested
  with a fake socket and screen) sends the token and `open` at the
  terminal's size, resets the screen at `opened`, sends input as binary
  and `resize` on fit, and closes with `close` first. `not_running` and
  `not_found` stop it; a lost socket or `ended` retries with the control
  socket's backoff; 4401 stops it for good. A hidden tab closes the pane
  3 s after (`HIDE_CLOSE_MS`) and reopens it when shown. The view retries
  a stopped pane when its row's status changes, and reopens the overlord's
  when the snapshot's overlord id differs from the one `opened` named
  (following a switch). xterm.js 6: `macOptionIsMeta`,
  `macOptionClickForcesSelection`, `scrollback: 0` (tmux keeps history),
  WebGL with the DOM renderer when there is no context or it is lost.
  Choices within the plan:
  - A stopped overlord offers "Start it" (`overlord.ensure`, already on
    the allow-list); no overlord yet says to run rlord in a terminal (the
    web doesn't set one up). A session's Resume is shown but disabled until
    step 11 puts its op on the allow-list.
  - The roster is shown (agent chips with status); switching is action 7,
    step 11.
  - A session not in the list's view (archived, filtered) is fetched with
    `session.get`.
- Step 10's browser pass (2026-11-17, Playwright 1.65.1 headless, outside
  the repo, against `demo.sh --web`): Chromium (WebGL), Chromium with
  WebGL off (DOM renderer) and WebKit (WebGL) all draw the overlord's
  pane; typing reaches its tmux pane (checked with `capture-pane` on
  `rlord-demo`); a session's pane opens; no console errors, no CSP
  reports. Chromium also: following `rlord overlord switch codex` and back
  (the pane reopens on the other overlord, one view session each time);
  a stopped session shows Resume and its pane comes back on `rlord
  restart`; a stopped overlord's Start works; a hidden tab leaves no view
  session and reopens when shown; `rlord web stop` then `start`
  reconnects the control socket and the pane; at 390 px wide the list and
  the pane show one at a time.
  - **`style-src 'unsafe-inline'` stays.** Served with `style-src 'self'`
    (the header rewritten in the test), every browser reports
    `style-src-elem` violations: xterm.js 6 still makes `<style>` elements
    (one with WebGL, three with the DOM renderer) and has no nonce option.
  - Found and fixed on the way: the list drew a "not set up" overlord row
    before the first snapshot; the pane's note sat under xterm.js's
    canvases, so its buttons couldn't be clicked.
  - A page served through Playwright's `route.fulfill` trips Chromium's
    Local Network Access check on its websockets
    (`ERR_BLOCKED_BY_LOCAL_NETWORK_ACCESS_CHECKS`); the real page, served
    by the plugin, doesn't. Only the strict-CSP test needed the flag that
    turns the check off.
  - Not covered headless (still owed by hand): Option as Meta and the
    browser-kept keys on a real keyboard, the wheel and copy, IME, two
    tabs at once, the TUI overlay open alongside, WebGL context loss
    mid-session.
  - The page is 486 kB of script (128 kB gzipped), nearly all xterm.js
    and its WebGL addon.

- Step 11 done (`daemon/src/web.rs`, `ui/src/actions.ts`,
  `components/Actions.tsx`, the views): ops `session.resume`,
  `session.restart`, `session.stop`, `overlord.ask`, `session.archive`,
  `session.unarchive`, `session.rename`, `session.describe`,
  `session.notes`, `session.tag`, `session.pin`, `overlord.switch`, each
  tested in `web_api.rs` (`actions_reach_their_methods`; the ones that
  start an agent reach their method and fail there, with no tmux or agent).
  The page: a session's Restart and Stop (running) or Resume (stopped),
  Archive, Pin, and an Edit form (name, description, tags, notes) that
  sends only the fields that changed; Unarchive for an archived one;
  Resume on the pane's note. The overlord's Restart, Stop and its agents as
  switch buttons. "Tell the overlord" is a box under the overlord row in
  the list (Enter sends, Shift-Enter a new line). Nothing new for the
  overlord itself: `overlord.md` and `mcp.rs` unchanged. Choices within
  the plan:
  - **Each op names its arguments** (`web::Op`): only those reach the
    method, so a browser can't set what trusted callers set (`session.stop`'s
    `interrupt`, `session.restart`'s `hooks_work`). Tested.
  - `session.resume` is answered in `web.rs` (`ui::ensure_running`, with
    the agent warm-up and Codex-server start that `call` does before
    `ui.goto`); there is no daemon method of that name.
  - `proto.ts` lists the ops (`OPS`, typing `request`); a daemon test holds
    that list to the daemon's.
  - Restart, Stop and Archive ask once more inline (Yes / No).
  - The plugin's call limit went from 60 s to 120 s: `overlord.ask` waits
    up to 90 s for a just-started overlord.
  - `demo.sh` takes `RLORD_DEMO_SOCKET`, so a second demo runs beside
    another without sharing its tmux server.
  - Browser pass (Chromium headless, Playwright 1.65.1, a second demo on
    its own socket and port 47821): edit (all four fields), pin and unpin,
    stop then Resume (pane comes back), restart, archive then unarchive
    then Resume, tell the overlord (text reached its pane, checked with
    `capture-pane`), switch to Codex and back, the narrow layout. Checked
    against the database: name, description, notes, tags. No console
    errors beyond Chromium's WebGL performance notes.

- Step 12 done (`crates/rlord/src/web_install.rs`, `web-install.sh`,
  `update.sh`, `core/src/plugin.rs`, `config.rs`): `rlord web install
  [--yes] [--if-installed] [--from DIR]` and `rlord web remove`; `rlord
  web` with no plugin asks `[y/N]` on a terminal, else fails naming
  `rlord web install --yes`, before it asks the daemon anything.
  `update.sh` runs `rlord web install --if-installed` with the new rlord
  after installing it and before the reset. Tests:
  `crates/rlord/tests/web_install.rs` (PATH holds only `tar`, `gzip` and
  `bash`, so nothing can download). Choices within the plan:
  - **The script only downloads** (`web-install.sh VERSION TARGET DIR`:
    the tarball and `SHA256SUMS` by `gh` or `curl`, and `gh attestation
    verify` for a stable release when `gh` is logged in; a failed
    attestation stops the install). Checking the tarball against
    `SHA256SUMS`, unpacking (`tar`) and `plugin::verify` are in Rust.
    `--from` copies the two files from DIR and runs no script.
  - Unpacked in `<plugins>/.install-<random>` (removed afterwards), so the
    last step is one rename into `<plugins>/rlord-web` (0755; folder
    0700).
  - **Tests need an expected hash a checkout's rlord doesn't have**: a
    debug build takes `RLORD_TEST_WEB_SHA256` (`plugin::install_hash`; a
    release build never reads it). Only the CLI reads it; the daemon still
    uses the compiled-in hash only.
  - A build with no hash (checkout, crates.io) refuses `install` and says
    how a checkout finds its plugin; with `--if-installed` it does nothing.
  - `install` of a plugin that already matches says so and stops;
    `--if-installed` replaces a non-matching one without asking (the user
    opted in already) and is silent when there is none.
  - `remove` asks only a running daemon to stop the plugin (it doesn't
    start one), deletes the plugin and `web.token`, and writes
    `[web] enabled = false` only when it was true (`Config::set_bool`).
  - Found on the way: a test running a no-hash `rlord web` found the
    `target/debug/rlord-web` beside it and started a daemon; the tests now
    point `RLORD_WEB_BIN` at a missing file.
  - Not tested: the download itself (no network in tests); first real
    check is the release dry run.

## Slice 1 plan: local web UI, as an opt-in plugin

Planned 2026-11-17, revised the same day for the plugin shape (the user's
changes are recorded under "Answers and changes"). Steps 1 and 2 are built;
nothing from step 3 on is. Version numbers below were read from the npm
registry, crates.io and nodejs.org on 2026-11-17, not from memory.

### Decisions

#### Shape: two executables

- **`rlord` (core, daemon, tui, CLI) has no HTTP, websocket or TCP code and
  no such dependency.** It gains: a `[web]` config section, a daemon module
  that answers the plugin over the Unix socket and supervises it as a child
  process, a `rlord web` command, and a compiled-in install script.
- **`rlord-web` is the plugin**: a separate executable holding the HTTP
  server, websockets, PTY bridge and embedded page. Later it also carries
  the E2E relay client. It is never in the `rlord` tarball, the deb, the
  rpm, the Homebrew formula or crates.io. A user who has not opted in has
  no file on disk that can send a session anywhere.
- **Split of work.** Whatever needs the daemon's state stays in the daemon
  and is reached over the socket. Whatever touches a network socket is in
  the plugin. The tmux view helpers stay in `rlord_core::tmux` (built); the
  plugin calls them directly, on the same tmux server.

| Concern | Where |
| --- | --- |
| Op allow-list, `snapshot`, `rev`, change stream, pane target lookup | daemon, `crates/daemon/src/web.rs` |
| Start, stop, restart of the plugin; `web.status` | daemon, same module |
| Plugin path, hash check, protocol version, `[web]` config | `crates/core/src/plugin.rs`, `config.rs` |
| Opt-in prompt, download, open browser | `crates/rlord/src/main.rs` + `crates/rlord/web-install.sh` |
| Token, HTTP, websockets, headers, limits, PTY bridge, assets | plugin, `crates/web/` |

#### Crate layout and keeping the main binary clean

- New workspace member `crates/web`: package `robot-overlord-web`,
  `publish = false`, `[[bin]] name = "rlord-web"`. In `members`, not in
  `default-members`, so a plain `cargo build` or `cargo test` at the root
  is unchanged; `--workspace` (CI) includes it.
- It depends on `rlord-core` only (paths, config, `private::write`, tmux
  helpers, `plugin.rs`). It does not depend on `rlord-daemon` (that would
  pull SQLite into the plugin); it has its own small async client for the
  socket's newline-delimited JSON. `rlord-daemon` is a dev-dependency, for
  integration tests against an in-memory `Daemon`.
- Its own dependencies: `axum` 0.8 with `ws` (hyper 1, tokio-tungstenite),
  `tokio`, `serde`, `serde_json`, `libc` (`openpty`, as the step 1 test
  does; no PTY crate), `subtle` (constant-time compare). `sha2` goes in
  `rlord-core` (hash check; not network code).
- No crate under `crates/{core,daemon,tui,rlord}` depends on
  `robot-overlord-web`. Release builds use `cargo build -p <package>`, one
  invocation per binary, so the plugin's dependency features never unify
  into `rlord`.
- **Guard: `cargo xtask check-deps`** (run by CI's `check` job and by
  `build-release`). It fails when `cargo tree -p robot-overlord -e
  normal,build` names a crate on a deny list (`axum`, `hyper`, `h2`,
  `http`, `tungstenite`, `tokio-tungstenite`, `rustls`, `native-tls`,
  `reqwest`, `ureq`, `curl`, `robot-overlord-web`), or when
  `TcpListener`, `TcpStream` or `UdpSocket` appears in the source of the
  four main crates.
- Stated plainly: tokio's `net` feature is already on in the main binary
  for Unix sockets, and it compiles tokio's TCP types too. They are unused
  and the guard keeps them unused; tokio has no feature that gives Unix
  sockets alone. `rlord update` also already downloads, by running `curl`
  or `gh` from a compiled-in script. The plugin download works the same
  way, so the main binary still links no network client.
- `crates/web` is not added to `version::CRATES` or to
  `xtask/src/publish.rs`.

#### Opt-in, install and verification

- **Opt-in is an explicit act.** `rlord web` with no plugin installed says
  what will happen (the file and version it downloads and from where, that
  it opens a listener on `127.0.0.1` only, that `rlord web remove` undoes
  it) and asks `[y/N]`. Without a terminal it fails and names
  `rlord web install --yes`. Nothing else installs the plugin: not the
  daemon, not `rlord update`, not a config key.
- **Where it comes from.** The same GitHub release as the running `rlord`
  (tag `v<own version>`), a second tarball per target:
  `rlord-web-v<version>-<target>.tar.gz` (the binary, both licenses, and
  `THIRD-PARTY-LICENSES` for the bundled npm packages).
- **Install location.** `<data dir>/plugins/rlord-web` (0700 directory,
  0755 file), written with copy-then-rename as `install.sh` does. Not on
  `PATH`: nobody runs it by hand. The same place for every install method
  (get.sh, Homebrew, deb, rpm), since the plugin is per user.
- **Verification: the hash of the plugin is compiled into `rlord`.** The
  release build makes `rlord-web` first, signs it (macOS ad-hoc), takes
  the SHA-256 of the executable and builds `rlord` with
  `RLORD_WEB_SHA256=<hex>` (`option_env!` in `core/src/plugin.rs`; `rlord`
  is built per target, so one hash each). The trust root is the `rlord`
  the user already runs: a swapped release asset, a tampered
  `SHA256SUMS` or a compromised download path cannot produce a plugin this
  `rlord` will start. No signing key to hold or lose.
  - The script downloads the tarball and `SHA256SUMS` (with `gh` when
    logged in, else `curl`, as `update.sh`), checks the tarball against
    `SHA256SUMS` (catches a broken download early), and unpacks to a temp
    directory. `rlord` then hashes the unpacked executable in Rust
    (`plugin::verify`) and only then moves it into place.
  - The daemon runs `plugin::verify` again before every spawn. A plugin
    left over from another version therefore never starts; that check is
    also the version match.
  - When `gh` is present and the release is stable, the script also runs
    `gh attestation verify` on the tarball (stable releases carry build
    provenance). Extra evidence; not required, since dev builds have none.
- **A build with no compiled-in hash** (a checkout's `cargo build`, or
  `cargo install robot-overlord` from crates.io) never downloads. It uses
  `RLORD_WEB_BIN`, or an `rlord-web` next to its own executable
  (`target/debug/`), unverified, and says so. For crates.io installs
  `rlord web` says the plugin comes with release installs (open
  question 2).
- **Updates.** `update.sh` ends with `rlord web install --if-installed`:
  silent when the user never opted in, otherwise it fetches the plugin for
  the new version. If that fails (offline), the old plugin fails
  `plugin::verify`, the daemon does not start it, and `rlord web` and
  `rlord web status` say to run `rlord web install`.
- **Removal.** `rlord web remove` stops the plugin, deletes it and
  `web.token`, and sets `enabled = false`.

#### Compatibility handshake

- `rlord_core::plugin::WEB_PROTOCOL: u32`, bumped whenever a `web.*` method
  the plugin uses changes shape.
- The plugin's first call is `web.hello {protocol, version, pid, port}`.
  The daemon answers `{protocol, version}` and refuses a different
  protocol with a typed error; the plugin then exits with code 3, which the
  daemon does not retry.
- Release installs are held to the exact build by the hash. The protocol
  number is what lets a checkout's plugin run against a checkout's daemon
  a few commits apart, and gives a clear message instead of a parse error.
- The page gets a build id (version plus a hash of the asset table) in
  `hello` on the control websocket; a page from another build reloads.

#### How it starts, stops and is supervised

- **The daemon runs the plugin as a child process.** No launchd or systemd
  unit of its own, no detached process to track.
- Config, read by both sides:

  ```toml
  [web]
  enabled = false   # true: the daemon starts the plugin when it starts
  port = 47811      # 127.0.0.1 only; 0 = any free port
  ```

  A fixed port matters: the browser stores the token per origin, and the
  origin includes the port. Installing does not set `enabled`.
- **Start.** With `enabled` and a verified plugin, at daemon start.
  Otherwise the first `rlord web` starts it for this daemon run
  (`web.start`). The daemon spawns `rlord-web serve` with the daemon's
  environment (so the socket, data dir and `RLORD_TMUX_SOCKET` match),
  output appended to `<data dir>/web.log`. `web.start` returns when the
  plugin's `web.hello` arrives (5 second limit) with the port it bound.
- **Stop.** `web.stop`, and daemon exit: SIGTERM, then SIGKILL after 2
  seconds. The plugin also exits when its subscription to the daemon ends,
  so a daemon that crashed leaves no listener behind.
- **Restart.** An unexpected exit is retried with backoff (1 s doubling to
  60 s; stop after five failures in a row). Exit code 3 (incompatible) and
  4 (port in use) are not retried. `web.status` reports state, pid, port,
  the last error line and why it isn't running (not installed, hash
  mismatch, port in use). A failure never stops the daemon.
- **A daemon restart restarts the plugin**, so open panes drop and the page
  reconnects (the token file persists). Accepted, as in the first plan
  (open question 5).
- **On its own start** the plugin calls `tmux::sweep_views()`, and before
  attaching each view `tmux::bind_web_table()` (both owed from step 2).

#### The `rlord web` command

Subcommands replace the first plan's flags; there are more verbs now.

- `rlord web`: opt in if needed, start if needed, open the browser at the
  token URL (`open` on macOS, `xdg-open` elsewhere), print the URL if that
  fails.
- `rlord web url`: start if needed, print the URL only.
- `rlord web status`: installed or not, running or not, port, last error.
- `rlord web start` / `stop`.
- `rlord web new-token`: delete the token and restart the plugin, which
  closes every open web connection.
- `rlord web install [--yes] [--if-installed] [--from DIR]` (`--from`: a
  local folder of release files, for the smoke tests; still hash-checked)
  and `rlord web remove`.

The CLI composes the URL from `web.status` (port) and the token file. It
calls `web.status`, `web.start`, `web.stop`, `web.rotate` over the Unix
socket (`Daemon::call_ui`). Those four and `web.hello` are never on the
allow-list below, and never reachable from MCP. A `:web` command in the
list is the last, optional step.

#### Daemon RPC additions (`crates/daemon/src/web.rs`)

No HTTP types; this is the old plan's `web/api.rs`, now on the daemon side
of the socket. The relay client will use the same four calls.

- **`web.hello`**: above.
- **`web.call {op, args}`**: the only way a browser request reaches the
  daemon. `op` comes from a fixed allow-list mapped to daemon methods, not
  a pass-through; `admin.*`, `mcp.*`, `hook.*`, `web.*`, `config.*`,
  `ui.*`, `tmux.*` and `setup.*` are off it. The list lives in the daemon,
  the always-installed and smaller side, so a parsing bug in the plugin
  can't reach a method the web was never meant to have. (It is not a
  boundary against a hostile local process, which can call the socket
  itself, as today.)
- **Op `snapshot {view, rev}`**: everything the list needs in one answer
  (rows for the asked view, the same query object as `session.list`;
  status labels; groups with counts and favourites; active overlord and
  roster) and a `rev`. A client passing the current `rev` gets
  `{"unchanged": true}`. Grouping, sorting and labels are computed in
  Rust; the page only draws.
- **`rev`** is `"<boot id>-<n>"`: a random id per daemon start plus a
  counter that `Daemon::changed()` increments. Cheap, and never repeats
  across a restart.
- **`web.subscribe`**: takes over the connection as `events.subscribe`
  does (shared loop), then sends `{"notify":"changed","rev":"…"}` at most
  once per 100 ms; a lagged receiver counts as a change. It is not counted
  as a TUI subscriber and gets no `ui` commands in this slice.
- **`web.pane_target {target}`**: `overlord` resolves to
  `overlord::session`, anything else through `resolve_ref`. Returns the
  tmux session name and whether it is running. It starts nothing.
- **No web-input reporting**: `send::user_active` already counts a
  browser through tmux's own client activity (step 2).

| Layer | Slice 1 | Relay later |
| --- | --- | --- |
| Requests + events | browser ⇄ plugin: one control websocket, JSON text frames; plugin ⇄ daemon: `web.call`, `web.subscribe` | the mailbox; `changed` + `rev` is the "anything for me?" signal |
| Pane messages | one websocket per pane; binary = bytes, text = control | the relayed live channel, same messages in Noise frames |
| Auth | token in the first frame | Noise handshake as the first frames |

Browser-side messages are unchanged from the first plan: requests
`{"id","op","args"}` answered by `{"id","ok"}` or `{"id","error":{"code",
"message"}}`; events `{"event":"changed","rev"}`; pane messages
`Open {target, cols, rows}`, `Input(bytes)`, `Resize`, `Close` in and
`Opened`, `Output(bytes)`, `Closed {reason}` out.

#### The plugin (`crates/web/src/`)

- `main.rs`: `serve` and `--version`; exit codes.
- `daemon.rs`: the async socket client (`hello`, `call`, `subscribe`).
- `token.rs`: the token file.
- `http.rs`: axum routes, Host and Origin checks, headers, limits, the
  control websocket.
- `pane.rs`: the PTY bridge (`openpty`, tokio `AsyncFd`), view sessions,
  the pane websocket.
- `assets.rs` + `build.rs`: the embedded page.
- `ui/`: the TypeScript client (below).

#### Auth, headers, assets

The reviewed design stands. Only its owner changed: all of it is in the
plugin.

- **Bind** `127.0.0.1` only (not `::1`, not `0.0.0.0`).
- **Token.** 32 random bytes from `/dev/urandom`, hex. The plugin creates
  `<data dir>/web.token` with `rlord_core::private::write` (0600) when it
  is missing and reuses it across restarts so open pages reconnect. The
  CLI only reads it.
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
  elements at run time (to re-check against xterm.js 6 in step 10; drop it
  if 6 no longer needs it). Scripts stay strict: the build must emit no
  inline script, and a test checks the built `index.html`.
- **Limits**: 16 control sockets, 32 pane sockets, 64 KiB per pane input
  frame, 1 MiB per control frame.
- **Assets embedded.** `build.rs` walks `crates/web/ui/dist/` and writes a
  table (path, content type, `include_bytes!`); file names carry content
  hashes, so no hand-kept list. No CDN, no outside URL. With no `dist/`
  (a Rust-only build or CI's `check` job) it embeds a one-line page naming
  `cargo xtask web build`; with `RLORD_WEB_REQUIRE_ASSETS=1` (set by
  `build-release`) a missing `dist/` is an error. Debug builds read
  `RLORD_WEB_ASSETS=<dir>` from disk instead, for development.

#### Pane streaming: a tmux client per view, through a view session

Unchanged, and proven in steps 1 and 2. The bridge now runs in the plugin.

**Chosen: the spike's bridge (one tmux client on a PTY per websocket),
attached to a throwaway grouped session instead of the target itself.**

Rejected:

- **Control mode (`tmux -C`).** Bare pane, no status row, no key tables.
  But the bridge would rebuild terminal state for a view joining
  mid-stream (alternate screen, cursor-key mode, mouse modes, bracketed
  paste), decode `%output`, re-encode input via `send-keys -H`, and add
  flow control.
- **capture-pane frames plus `send-keys`** (the TUI overlay,
  `tmux::capture_frame`). A browser terminal sends raw bytes, not named
  keys; scrollback and mouse need hand-written handling again; whole-screen
  frames are the wrong shape for a relayed byte channel.

The view session (`tmux::view_command`, `VIEW_PREFIX`, `WEB_TABLE`):

- Per pane websocket, one tmux command in the PTY creates a session
  grouped with the target and attaches, with `status off`, `key-table
  rlord-web` and `destroy-unattached on`.
- **Status row gone**: `status` is a session option.
- **Navigation gone**: the `rlord-web` key table has no keyboard bindings,
  and tmux doesn't fall back to root from a session's default table, so
  nav keys, the user's profile and the prefix are unreachable. The table
  holds copies of root's pane mouse bindings (`bind_web_table`).
- **Cleanup**: `destroy-unattached` removes it when the client exits;
  `sweep_views` kills leftovers with no client.
- Already handled in core and daemon (step 2): `list_clients` drops view
  clients, `kill_session` kills the target's views first, `user_active`
  and the overlord's idle check count a browser, `view_command` refuses a
  missing target and `view_shows` confirms the window id after attaching.

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
- Target resolved on every open with `web.pane_target`. A `changed` event
  showing a new active overlord makes the page reopen the pane.
- Copy: xterm.js selection with the modifier held
  (`macOptionClickForcesSelection`). OSC 52 (`@xterm/addon-clipboard`) is a
  follow-up.

#### The frontend stack

TypeScript, built to static files that are embedded in `rlord-web`. Node is
needed to develop and build the page, never to install or run rlord.

| Piece | Choice, pinned | Why |
| --- | --- | --- |
| Type checker | `typescript` 7.0.2 | The native (Go) compiler has been the stable `typescript` package and `tsc` since 7.0, 2026-07-08; `tsgo` was only the preview's name. 7.1 is in beta. |
| Build | `vite` 8.3.8 | Stable since 2026-03; bundles with Rolldown (1.0 in 2026-05) and transforms TS and JSX with Oxc, no esbuild or Rollup. Vite pins its own Rolldown (`~1.2.18`), so Rolldown is not pinned separately. |
| Lint | `oxlint` 1.94.0 + `oxlint-tsgolint` 7.0.2007 | Type-aware linting went stable 2026-07 and uses TypeScript 7's own checker; `no-floating-promises` and friends matter in websocket code. |
| Format | `@biomejs/biome` 2.6.8, formatter only | Oxfmt is still 0.x (0.79, beta since 2026-02), so it fails "stable". Switch to it at its 1.0 and drop Biome. |
| UI | `preact` 11.2.0 + `@preact/signals` 2.11.4 | See below. |
| Terminal | `@xterm/xterm` 6.0.0, `@xterm/addon-fit` 0.11.0, `@xterm/addon-webgl` 0.19.0 | 6.0.0 is the current stable major (6.1 is beta only). 6 removed the canvas renderer: WebGL for speed, falling back to the DOM renderer when the context is lost or missing. 6 also dropped the Alt-as-Ctrl-arrow hack and reworked the viewport, so the spike's key and scroll behaviour is re-checked by hand. |
| Unit tests | `vitest` 5.1.0 | Runs on the same Vite config. |
| Runtime for the tools | Node 26.13.1 (current LTS), `npm ci --ignore-scripts` with the committed `package-lock.json` | npm ships with Node, so no second tool to pin. |

All versions exact in `package.json` (no `^`), integrity hashes in the
lockfile, Node in `crates/web/ui/.node-version`.

**A signals framework over hand-written DOM: yes.** The page is a live
list that redraws on every `changed`, with filters, tabs, groups and three
routes. By hand that means writing keyed list diffing and update
bookkeeping ourselves. Signals give fine-grained updates for about 6 kB.

- **Preact 11 + signals, chosen.** Plain TSX that `tsc` checks with no
  framework tooling; Vite 8 compiles it natively (`jsx: "react-jsx"`,
  `jsxImportSource: "preact"`), so no compiler plugin; the smallest
  runtime; 11 has been stable since 2026-09.
- **Solid, not yet.** `solid-js` 2.0.0 was published on 2026-11-17, hours
  before this plan, and the stable `vite-plugin-solid` (2.11.16) still
  requires Solid 1. Solid 1.9 would mean adopting a major the day it was
  superseded. Worth another look once 2.x and its plugin settle.
- **Svelte 5, no.** Checking `.svelte` files needs `svelte-check`, which
  supports TypeScript 5 and 6 only: TypeScript 7.0 has no stable
  programmatic API (due in 7.1). It would hold the type checker a major
  back.
- **Vite+ (`vite-plus` 1.7.0, stable since 2026-09), no.** It bundles
  these same tools behind one CLI but also wants to manage the runtime and
  package manager, trails the tools' own releases, and includes the
  pre-1.0 Oxfmt. Separate pins are plainer.

**Node against a Rust-only pipeline.** A Node-free build is possible
(the standalone esbuild binary, plus the native `tsc` and `oxlint`
binaries fetched from their npm tarballs), but it gives up Vite, Rolldown
and Vitest, the stack asked for, and is a path nobody else walks. Chosen:
Node for developers of the page, CI and the release build only, always
behind `cargo xtask`:

- `cargo xtask web install`: `npm ci --ignore-scripts` in `crates/web/ui`
  (checks the Node version against `.node-version` first).
- `cargo xtask web check`: `tsc --noEmit`, `oxlint --type-aware
  --deny-warnings`, `biome format` (check only), `vitest run`.
- `cargo xtask web build`: `vite build` into `crates/web/ui/dist/` (not
  committed).
- `cargo xtask web watch`: `vite build --watch`, to pair with a debug
  plugin run with `RLORD_WEB_ASSETS`. No Vite dev server: its origin would
  fail the Origin check, and the checks get no development exceptions.

Someone working only on Rust needs none of this: the plugin builds with
the placeholder page.

#### Frontend layout and behaviour (`crates/web/ui/`)

- `package.json`, `package-lock.json`, `.node-version`, `tsconfig.json`,
  `vite.config.ts`, `.oxlintrc.json`, `biome.json`, `index.html`.
- `tsconfig.json`: `strict`, `noUncheckedIndexedAccess`,
  `exactOptionalPropertyTypes`, `verbatimModuleSyntax`,
  `erasableSyntaxOnly`, `moduleResolution: "bundler"`, `noEmit`.
- `src/main.tsx` (mount, routes), `src/proto.ts` (message and snapshot
  types), `src/api.ts` (control socket: token, auth frame, reconnect,
  requests by id), `src/store.ts` (signals: snapshot, connection, view
  state), `src/pane.ts` (xterm.js and the pane socket),
  `src/views/{List,Overlord,Session}.tsx`, `src/components/`,
  `src/app.css`.
- `test/`: Vitest for `api.ts` and `store.ts` (reconnect, `rev`, request
  matching), and a type check of `test/fixtures/snapshot.json` against
  `proto.ts`. A Rust test in the daemon fails when a real `snapshot`
  differs in shape from that fixture, so the hand-written types can't
  drift from the daemon unnoticed.
- **Live updates.** `changed` pushed; the page debounces and calls
  `snapshot` with its view and `rev`.
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

#### Release and packaging

- `version.rs`: `Version::web_tarball(target)`; the three plugin tarballs
  join `tarballs()` and `workflow_assets()`, so preflight, the workflow's
  asset check, `SHA256SUMS`, the provenance attestation and
  `verify_and_publish` cover them with no further change.
- `build.rs` (`cargo xtask build-release`), per target: build the page
  once (`cargo xtask web build`, unless `RLORD_WEB_DIST` points at a
  prebuilt one), build `rlord-web` with `RLORD_WEB_REQUIRE_ASSETS=1`, sign
  it on macOS, hash it, build `rlord` with `RLORD_WEB_SHA256`, run
  `check-deps`, pack both tarballs. The deb and rpm are made from that same
  `rlord`, so they carry the hash too.
- `release.yml`: a new `web-assets` job (Linux, Node from `.node-version`
  through `actions/setup-node` pinned by commit) runs `cargo xtask web
  check` and `web build` and uploads `dist/`; `build-macos` and
  `build-linux` download it and set `RLORD_WEB_DIST`. One build of the
  page, identical bytes on every target, and Node stays off the two jobs
  that compile the binaries. Dev builds (`cargo xtask release` on the Mac)
  build the page locally.
- `ci.yml`: a new `web` job: `cargo xtask web check`, `web build`, then
  `cargo test -p robot-overlord-web` against the real page. The existing
  `check` job gains `cargo xtask check-deps` and needs no Node.
- `smoke-linux` and `fresh-install`: after installing, `rlord web install
  --yes --from DIST` and `rlord web status`, proving the compiled-in hash
  matches the shipped plugin on each distro.
- Homebrew formula, deb and rpm: unchanged, `rlord` only.

#### The overlord's instructions

Nothing new for the overlord: no MCP tool, meta-context or list concept.
`overlord.md` and `mcp.rs` unchanged; web clients are filtered from
`list_clients`, so `get_ui_state` answers as today. For a web-only user,
`goto`, `show_list`, `set_view` and `propose_session` fail with "no
terminal is attached to tmux", as they do today with no terminal. Making
the web UI a target for them is the next slice (changes `ui.state` and
`overlord.md`).

#### Tests

Never `tmux -L rlord` or the default server, no real agent, no model, no
download from the network.

- **Unit, core:** `plugin::verify` (right hash, wrong hash, missing file),
  plugin path, `[web]` config.
- **Unit and in-memory, daemon** (`crates/daemon/tests/web_api.rs`):
  `web.call` reaches real methods and refuses every forbidden family and
  the lifecycle methods; `snapshot` shape (against the shared fixture) and
  `unchanged`; `rev` moves on a change and differs across a restart;
  `web.subscribe` coalesces and treats lag as a change; `web.hello`
  refuses another protocol; `web.pane_target` starts nothing.
- **Supervision, daemon** (a stand-in `rlord-web` shell script through
  `RLORD_WEB_BIN`, no network): start waits for hello; stop kills; backoff
  on a crash; codes 3 and 4 not retried; a hash mismatch is reported and
  nothing is spawned.
- **Unit, plugin:** token file (0600, reused, recreated), constant-time
  compare, Host and Origin checks, pane frame bounds, asset table (every
  file `index.html` names exists, no outside URL, no inline script, CSP
  names no outside host).
- **Integration, plugin, no tmux** (`crates/web/tests/http.rs`,
  `127.0.0.1:0`, an in-memory `Daemon` on a temp socket): wrong Host 404;
  bad or no Origin 403; wrong token 4401; right token `hello`; `snapshot`
  answers; a change gives `changed`; an off-list op errors; the plugin
  exits when the daemon goes away.
- **Gated tmux** (`RLORD_TMUX_TESTS=1`, `RLORD_TMUX_SOCKET=rlord-test-web`):
  steps 1 and 2's tests (built); end to end against `cat` through the
  plugin's bridge: bytes echo, resize changes width, no status row, a root
  binding doesn't fire and reaches the pane, `list_clients` hides the web
  client, killing the target closes the socket and leaves no view
  session, `sweep_views` clears a leftover at start.
- **Install** (`crates/rlord/tests/web_install.rs`): `web-install.sh` with
  `--from` a temp folder: installs a matching file, refuses a mismatching
  one, `--if-installed` does nothing when absent, `remove` cleans up.
- **CLI:** `rlord web` without a terminal and without the plugin fails and
  names `install --yes`; `check-deps` has its own test with a planted
  forbidden dependency name.
- **Frontend:** `cargo xtask web check` (types, lint, format, Vitest).
- **By hand** against `scripts/demo/demo.sh --web` (builds the page and
  `rlord-web`, copies it beside the demo's `rlord`; own data and tmux
  server `rlord-demo`) in a real browser: drawing, fit, focus, Option as
  Meta, browser-kept keys, wheel, selection/copy, two tabs, TUI overlay
  open at once, daemon restart, overlord switch, phone width, WebGL
  fallback.

### Implementation steps

One commit each. The PR bumps the version to the next `-dev.N` and
regenerates `Cargo.lock`. Steps 1 and 2 are done; 3 onward replace the
first plan's steps 3 to 12.

1. **Done.** Prove the view session on a scratch socket
   (`crates/daemon/tests/web_tmux.rs`).
2. **Done.** tmux helpers and side effects (`tmux.rs`, `send.rs`,
   `overlord.rs`).
3. **Skeleton and guard.** `crates/web` (`rlord-web --version`, a `serve`
   that only says hello and waits), workspace entry;
   `core/src/plugin.rs` (path, `WEB_PROTOCOL`, `verify`, `sha2`); `[web]`
   in `config.rs`; `cargo xtask check-deps` and its CI line.
4. **Daemon web API** (`daemon/src/web.rs`): `web.hello`, `web.call` with
   the allow-list, `snapshot` and `rev`, `web.subscribe`,
   `web.pane_target`. Move `group_list` from `crates/tui/src/groups.rs`
   into `rlord_core`. Extract "make sure this session runs" from
   `ui::goto` for Resume. Snapshot fixture.
5. **Supervision and the command** (`daemon/src/web.rs`, `lib.rs`,
   `crates/rlord/src/main.rs`): spawn, hello wait, backoff, stop;
   `web.status`, `web.start`, `web.stop`, `web.rotate`; start when
   enabled; `rlord web` with `url`, `status`, `start`, `stop`,
   `new-token` and the browser opener, using a plugin beside the
   executable or `RLORD_WEB_BIN`. Stand-in plugin tests.
6. **Plugin: HTTP and control socket** (`daemon.rs`, `token.rs`,
   `http.rs`, `assets.rs`, `build.rs`): socket client, token, static
   files, headers, checks, first-frame auth, limits, `hello`, requests and
   events, exit with the daemon; placeholder page; no-tmux integration
   tests.
7. **Plugin: pane bridge** (`pane.rs`): `web.pane_target`, `openpty`,
   view session, `bind_web_table` before each attach, `sweep_views` at
   start, pane websocket, cleanup; gated end-to-end tests.
8. **Frontend toolchain** (`crates/web/ui/`, `xtask/src/web.rs`): pinned
   `package.json` and lockfile, configs, a page that connects and shows
   the connection state; `cargo xtask web install|check|build|watch`;
   `build.rs` embedding; CI's `web` job; `demo.sh --web`. Confirm here
   that `npm ci --ignore-scripts` leaves every tool working.
9. **Page: list** (`proto.ts`, `api.ts`, `store.ts`, `views/List.tsx`).
10. **Page: panes** (`pane.ts`, overlord and session views, visible-only
    connections, reconnect, Resume, follow overlord switch, WebGL with
    DOM fallback), then the by-hand browser pass, including the xterm.js 6
    checks and whether `style-src 'unsafe-inline'` can go.
11. **Actions 3 to 7**, each op on the allow-list with a test.
12. **Opt-in install** (`crates/rlord/web-install.sh`, `main.rs`): the
    prompt, `install` (`--yes`, `--if-installed`, `--from`), `remove`,
    the hash check before the move into place, the line in `update.sh`.
13. **Release packaging** (`xtask/src/version.rs`, `build.rs`,
    `release.rs`, `preflight.rs`, `smoke.rs`, `fresh_install.rs`,
    `release.yml`): the second tarball per target, build order with the
    compiled-in hash, the `web-assets` job, smoke checks. A dry run of the
    release workflow before the first real one.
14. **Docs and cleanup**: `docs/architecture.md` "Web UI" section (the
    daemon still has no TCP listener; the plugin does, and say how it is
    installed and verified), `docs/guide.md`, README, `SECURITY.md`,
    config template. Delete `spikes/web-ui/`, fold `web-ui-spike.md` in.
    Optional `:web`.

### Risks

- **The compiled-in hash couples two builds.** A wrong build order, a
  re-sign after hashing or a rebuilt plugin gives an `rlord` that refuses
  its own plugin. The smoke step installs the shipped plugin with the
  shipped `rlord` on every platform before a release is published.
- **The release build now runs npm packages** (the bundler and compiler,
  at build time). Bounded by exact pins, lockfile integrity,
  `--ignore-scripts`, no cache, and a separate job that only produces
  static files; the output is embedded in the plugin, never in `rlord`.
- **A young toolchain.** TypeScript 7.0, Vite 8, Preact 11 and Vitest 5
  are all 2026 majors; type-aware Oxlint has been stable four months.
  Expect rough edges and patch bumps. Each is replaceable without touching
  the Rust side.
- **xterm.js 6 is new to this project** (the spike used 5.5): renderer,
  viewport and Alt-key changes, and its CSP needs, are unverified until
  step 10.
- **A first TCP listener**: Host, Origin, first-frame auth and CSP stand
  between a hostile page and the user's agents; most plugin tests go
  there.
- **Two processes can disagree**: plugin and daemon versions, a stale
  token, a plugin that outlives its daemon. Covered by the hash check
  before spawn, the protocol handshake and exit-with-the-daemon, each with
  a test.
- **Opt-in must stay honest.** `check-deps` is the only thing stopping a
  later change from adding a network crate to `rlord`; it has to stay in
  CI and in `build-release`.
- A missed kill path leaves an agent running under a view session
  (step 2 covered the known ones).
- Browser behaviour unverified (Cmd-W/Cmd-T, Option as Meta, IME).
- Size mismatches between overlay and web views look odd; accepted.
- A daemon restart drops every pane; the plugin's start-up sweep cleans
  leftovers.

### Open questions for the user

1. **Verification.** The plugin's SHA-256 compiled into `rlord`, plus
   `SHA256SUMS`, plus `gh attestation verify` when `gh` is there. No
   separate signature or signing key. Enough, or do you want a detached
   signature (minisign or cosign) as well?
2. **`cargo install robot-overlord` users** get no plugin download (their
   build has no compiled-in hash). Acceptable, or should those builds fall
   back to trusting the release's `SHA256SUMS`?
3. **Framework.** Preact 11 with signals now. Solid 2.0.0 came out today
   with no stable Vite plugin yet; Svelte 5 can't be type-checked by
   TypeScript 7. Go with Preact, wait for Solid 2, or stay with
   hand-written DOM?
4. **Node in the release build** (one job, pinned, scripts off, page built
   once). The alternative is committing the built `dist/` and having CI
   check it matches a rebuild: no Node at release time, but built files in
   every page change's diff.
5. **Plugin lifetime.** Planned: a child of the daemon that restarts with
   it, so `rlord reset` or an update drops open panes for a moment. The
   alternative is a plugin that outlives the daemon and reconnects, which
   keeps panes open but needs its own start-up and version handling.
6. **Formatter.** Biome's formatter until Oxfmt reaches 1.0, or take
   Oxfmt 0.79 now for one tool family?
7. **A headless-browser smoke test** (Playwright 1.65) would catch a CSP
   or bundling break that unit tests can't see, at the cost of a browser
   download in CI. Planned: by hand only in this slice.
8. **Command shape.** `rlord web` now has subcommands (`install`,
   `remove`, `start`, `stop`, `status`, `url`, `new-token`) in place of
   the first plan's flags. Fine?

### Answers and changes (user, 2026-11-17)

To the first plan's questions:

1. Off by default; `rlord web` or `[web] enabled = true` starts it.
2. Port 47811 (default taken, not asked).
3. Web as a UI-command target: next slice.
4. Resume button, never auto-resume.
5. Latest active view sets the size.
6. Delete `spikes/web-ui/` and fold the spike doc in at the last step
   (default).
7. `:web` last and optional (default).

Changes after the first plan, which this revision implements:

- The web server is an opt-in plugin, a separate `rlord-web` executable
  downloaded only on opt-in. Main `rlord` keeps a thin `rlord web` command
  and no HTTP, websocket or network code, so a user who doesn't opt in has
  nothing that could send sessions over the network.
- Server side all Rust; no JavaScript server.
- Browser client in TypeScript.
- The most modern stable web stack, researched as of late 2026, pinned,
  with reasons.

What the revision replaced from the first plan: the server as a module in
the daemon (`crates/daemon/src/web/`), the op allow-list in the server
process, vendored xterm.js 5.5 with `cargo xtask vendor-web`, hand-written
ES modules with no build step, `portable-pty`, the `rlord web --flags`
form, and web-input reporting over IPC.

### Answers to the plugin re-plan (user, 2026-11-17)

1. Verification as planned (compiled-in SHA-256, `SHA256SUMS`, optional
   `gh attestation verify`); no extra signature.
2. crates.io builds: no plugin download (default taken, not asked).
3. Preact 11 + signals.
4. Page built in one pinned Node job; `dist/` not committed.
5. Plugin is a child of the daemon.
6. Biome for formatting until Oxfmt 1.0 (default).
7. Browser pass by hand this slice; Playwright later (default).
8. Subcommands for `rlord web` (default).
