**Status:** in progress (slices 1 to 3, the local web UI, the web as a UI target, and the e2e core and the relay, are built and merged into `feature/web`; slice 4, the phone app, is built for iOS on `feat/web-ui-4` (local commits) with Android deferred; slice 5, live terminals on the phone, is planned on `feat/web-ui-5` ("Slice 5 plan" at the end) and being built ("Slice 5 build"). The feature branch stays dark: no releases, TestFlight or store builds until the final merge to `main`; the app is built and installed locally only)

# rlord web UI and end-to-end encrypted relay

Two components: the **web UI** and the **transit** (a relay). Built in
sections. Slice 1 (the local web UI, an opt-in plugin) is built and
documented in `docs/architecture.md` ("Web UI") and `docs/guide.md`; what
follows the slice 1 plan is history kept for the relay and phone work.

## From the spike (folded in from `web-ui-spike.md`)

A throwaway spike (`spikes/web-ui/`, since deleted; local branch
`spike/web-ui`, never pushed) answered "can a local browser drive the
overlord?": yes, with one tmux client on a PTY per websocket. Slice 1 kept
that bridge and fixed what the spike found rough: the status row, tmux's
navigation keys and the CDN assets (view sessions and an embedded page).
What it found that still matters:

- **Shared window size.** A web view is an ordinary tmux client, so the
  window takes the latest active client's size; two views of one session
  (or a view and the TUI overlay) take turns. Accepted in slice 1; a
  relayed phone view at its own size would make it worse. Options: one
  view per session at a time, or the daemon sizing the window as the
  overlay does (`auto_size_window`).
- **What a remote version needs beyond slice 1** (the relay plan above and
  `e2e-crypto.md` cover transport, pairing and keys):
  - Real auth. The URL token is a bearer secret fit for localhost only (it
    leaks through history and screen shares); remote access needs
    per-device keys, expiry and revocation.
  - Least privilege. The web view is as powerful as the user's shell (the
    overlord can launch agents and type into any session). Remote use
    argues for a narrower view by default and an audit log of input.
  - Bandwidth. Raw tmux output is chatty; remote may want output
    coalescing and compression.
  - Phone layout. xterm.js at phone width is cramped: the overlord wants a
    chat view fed from its transcript rather than its screen, and an input
    box rather than raw keys.

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

### Production relay (user, 2026-11-17)

Needed before the relay runs as a production service. Not all of it is
slice 3's; "Production relay: requirements and where they land" in the
slice 3 plan says what slice 3 leaves room for and what waits for a later
production-relay slice. The user decides the shape of the richer relay;
the design choices are open questions there, not settled.

1. **Modular deployment.** The relay should ship on different backends.
   Proposed: durable storage behind a simple key-value abstraction, so a
   backend can be DynamoDB, Redis, S3 or anything else. Durable state is
   probably just session information. Where it is deployed is undecided.
2. **Identity monitoring.** The relay can't read payloads, but it must
   know which client and which account each connection belongs to, so it
   can meter throughput (at least per account), ban or allow users, and
   offer tiers of functionality. Content stays zero-trust; what metadata
   the relay learns as a result is a privacy trade-off to document.
3. **Load shedding.** The relay is a network proxy: per-account limits,
   backpressure, global shedding.
4. **Account registration and login.** Something more identifiable than
   the tokens rlord instances present, for account management. It need
   not be in the relay; it can be a separate service. The hosted public
   service and an on-prem or self-hosted setup are to be told apart
   (does self-host need accounts at all; one code path or a pluggable
   one). Once logged in, a user registers their rlord hosts almost
   entirely from their rlord instance (CLI, TUI or web flow). The
   network invariants hold: no network code in `rlord` (the flow lives
   in `rlord-web` or another plugin), the host only dials out, and the
   device-to-host E2E trust never depends on the account service
   (accounts govern access to the relay, not the keys).

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

- Spike: local PTY bridge works ("From the spike" above).
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

- Step 13 done (`xtask/src/{version,build,release,fresh_install}.rs`,
  `smoke_in_container.sh`, `fresh_install.sh`, `release.yml`,
  `ui/vite.config.ts`): `Version::web_tarball` (`rlord-web-v<ver>-<target>
  .tar.gz`: the binary, both licenses, `THIRD-PARTY-LICENSES.md`), in
  `tarballs()` and so in `workflow_assets()`, preflight's asset list,
  `SHA256SUMS`, the attestations and `verify_and_publish`; a dev build
  checks `dev_assets()` (both macOS tarballs). `build-release`, per target:
  the plugin (`-p robot-overlord-web`, `RLORD_WEB_REQUIRE_ASSETS=1`),
  `codesign` on macOS, SHA-256, then `rlord` with `RLORD_WEB_SHA256`, both
  tarballs. The page is built once first (`cargo xtask web build`) or
  copied from `RLORD_WEB_DIST`. `release.yml`: `build-macos` and
  `build-linux` need `web-assets`, download `web-page` and set
  `RLORD_WEB_DIST`. Smoke (`smoke-linux`) and `fresh-install` install the
  plugin with `rlord web install --yes --from /dist`, check `(verified)`
  and start it (`rlord web url`). Choices within the plan:
  - **The build checks the coupling itself**: after packing a target it can
    run (macOS on the Mac, x86_64 Linux on CI's runner), the new `rlord`
    installs the new plugin with `web install --from` in a scratch home
    (`check_install`); a mismatch stops the build before anything ships.
  - The licenses come from Vite's own `build.license`
    (`THIRD-PARTY-LICENSES.md` in `dist/`, so the page's artifact carries
    it to the build jobs, and the plugin also serves it). `.md` is
    `text/markdown`.
  - `sha256` in xtask runs `shasum` (as `write_sums` does): xtask has no
    hashing crate.
  - Checked here: `cargo xtask build-release macos` (plugin signed, its
    hash found in the built `rlord`, `check_install` passed) and
    `build-release linux` (both targets, debs and rpms), then
    `smoke-linux` on ubuntu:24.04 and 22.04 with the web steps. The
    `fresh-install --offline --only tarball,deb ubuntu:24.04` (the deb's
    `rlord` also takes the plugin: same binary, same hash). The
    smoke step first piped `rlord web status` into `grep -q`, which made
    rlord panic on the closed pipe; it reads from a file now.
  - Owed: a dry run of `release.yml` (`workflow_dispatch`, `dry_run`)
    before the next stable release; it needs the branch pushed. No
    release was cut or tagged.

- Step 14 done: `docs/architecture.md` "Web UI" (under Control surfaces;
  the plugin in the crate list); the socket paragraph no longer says "no
  TCP listener": `rlord` listens on nothing but its socket, and has no
  HTTP server, websocket, TLS or network-client crate (the OpenCode
  adapter's local HTTP client is the one exempt file), per step 3's
  finding. `docs/guide.md` "The web UI", README (a feature line, config),
  `SECURITY.md` (what is in scope for the plugin), `CONTRIBUTING.md` (the
  page's tooling, the gated tmux tests), `CLAUDE.md` (a standing rule:
  no network code in `rlord`, how to add a browser op), and `[web]` in the
  config template. `spikes/web-ui/` deleted; what still matters from
  `web-ui-spike.md` is "From the spike" above, and the spike doc is
  removed. This doc and `e2e-crypto.md` stay in progress (the relay and
  the phone app are the rest of the effort). Not done: the optional `:web`
  list command.
- Merged main (1.0.0 signing, 1.1.0 fresh repo code). feat/web-ui is now
  a long-running feature branch at `1.2.0-dev.1`. The plugin goes through
  main's `sign()` (Developer ID, identifier `com.robot-lord.rlord-web`)
  before it is hashed; `build-macos` in `release.yml` runs in the
  `release` environment with the keychain import and `RLORD_RELEASE`,
  takes `web-page` through `RLORD_WEB_DIST`, and "Verify signature"
  checks both binaries.
- Owed from slice 1 (by hand, in a real browser): Option as Meta and the
  browser-kept keys on a real keyboard, the wheel and copy, IME, two tabs
  at once, the TUI overlay open alongside, WebGL context loss
  mid-session; a dry run of `release.yml` before the next stable release.
- Slice 1 follow-ups (`feat/web-ui-2`, into `feature/web`, 1.3.0-dev.2):
  - **Release dry run** (`release.yml`, `workflow_dispatch` on
    `feature/web`, `dry_run` on, run 44185921608): preflight (warnings
    only: not on main, not stable, ref vs version), `web-assets`,
    `build-linux` and `smoke-linux` (both arches, five distros, each with
    `rlord web install --from` and `(verified)`) passed. `build-macos` was
    refused before it started: the `release` environment's deployment
    branch policy allows only `main` and `v*` tags, so `feature/web` can't
    use it, and `release` was skipped. Not worked around. Owed: the macOS
    half (signing, `Verify signature` on both binaries) and the `release`
    job's asset check, from `main` (or with `feature/*` let into the
    environment, the user's call).
  - The page draws `shown_status` as the TUI does (`components/Status.tsx`
    `shown`, after `state_chip`): "till 14:05" / "limited until 14:05"
    (↻, "· resumes then", the reset), "bg N" / "waiting on N background
    tasks"; pane headers name the work and background shells
    (`statustext.rs`'s sentences), list rows carry them in the status's
    tooltip. The row's separate limit tag is gone. `session.get` through
    `web.call` now gets `status_label` (a session the list doesn't show
    read "not started").
  - The filter box takes the TUI's tokens: `snapshot` runs the view's
    `search` through `SessionQuery::apply_search_text` (`search_tokens`),
    so `tag:`, `status:` (`bg`, `limited`, `turn` included), `agent:`,
    `proj:`, `group:` and `sort:` work; the page parses nothing.
  - `:web` in the list (`launcher.rs`): starts the plugin if needed and
    opens the page; not installed: says to run `rlord web`. `page_url` and
    `open_in_browser` moved to `core/src/plugin.rs` for both callers.
  - Checked in headless Chromium (Playwright 1.65.1, outside the repo)
    against a second demo (socket `rlord-demo-f2`, port 47831) with
    background work written into a transcript: "bg 1" and its tooltip,
    the header lines, every filter token; `:web` driven in the demo's TUI
    with a stand-in `open`. A usage limit is checked by unit tests only
    (the demo can't make one).
- Slice 2 (the web as a UI target; `feat/web-ui-2`, 1.3.0-dev.3), per
  the plan below and the user's answers:
  - Step 1 done, the gate passed (`web_tmux.rs`
    `client_activity_is_input_not_output`, tmux 3.8): a client whose pane
    prints for 3 s keeps its `client_activity`; a resize doesn't move it
    either; a key press moves it to now. Resolution: one second (epoch
    seconds). No fallback needed.
  - Step 2 done (`ui.rs` `Target`, `pick`, `target`; `web.rs` `WebUi`,
    `web.pages`, op `page.report`; `config.rs` `[web] ui_target`):
    `pick(terminal_s, pages, pref)` is pure and unit-tested; `target`
    feeds it the user's terminal (`user_client`'s activity) and each page's
    last use (a user-driven op, a `page.report` with `input`, or the last
    key typed into the pane it shows, from `view_clients`). `ui.state`
    has `target`, `web` and `current_view` from the target. Choices within
    the plan:
    - `ui_target = "terminal" | "web"` is a preference, not a lock: with
      only the other surface present, that one gets the command (so a
      web-only user isn't refused because of a config line).
    - Pages never used rank by arrival (the newest wins among them).
    - The pages are dropped when the last `web.subscribe` stream ends
      (counted, since a restarted plugin's stream can overlap the old
      one's); the plugin is to send `web.pages {pages: []}` at its start
      so a crashed plugin's pages go at once.
    - Look-ups that don't count as use: `snapshot`, `session.get`,
      `page.report` (unless `input`); later steps add their own.
    - `page.report`'s `form` is one of `describe`, `confirm`, `manual`,
      `fork`, `setup`, `keys` or null.
  - Step 3 done (`lib.rs` `ui_cmd_to`, `notice_others`, `pending_web`,
    `push_loop`, `web_ui_line`; `web.rs` op `page.pending`; TUI
    `push_of`, `notice`; `WEB_PROTOCOL` 2). Lines are `{"notify":"ui",
    "to":"terminal"|"web", "page"?, "cmd"}`; `web.subscribe` passes the
    web's on as `{"notify":"ui","page","cmd"}`, at once when they come
    alone, and after the folded `changed` when they come in a burst
    (kept, in order). Choices within the plan:
    - The web's slot holds the last web command with its page id, and
      `page.pending` hands it over only when that page has closed since
      (a reload, a reconnect): a page that got it never sees it twice.
    - `notice_others` sends to the terminal's TUI (when one runs) or the
      most recently used page; notices are never parked.
  - Step 4 done (`ui.rs`, `setup.rs`, `mcp.rs`, `web.rs`): `show_list`,
    `goto`, `set_view`, `propose_session`, `confirm_new_session`,
    `setup::confirm` and `keys_open` pick the target once and pass it
    down (`show_list_on`, `open_form`, `show_form`, `switch_to`); every
    answer has `on`, and `create_session` / `fork_session` with `go` pass
    it on. No screen: they fail naming `rlord` in a terminal and `rlord
    web`. `snapshot`'s view takes `query` (before `search`) and `usage`;
    ops `view.default` and `keys.list`. The proposal store
    (`Daemon::proposals`, last 8, ids `p<n>`): `propose_session` keeps
    `context` and `related` there and the prefill carries `proposal`.
    Tests: `web_api.rs` `the_overlords_ui_tools_land_on_a_page`,
    `snapshot_takes_query_and_the_default_view`; gated `web_tmux.rs`
    `the_surface_used_last_gets_the_command` (terminal and page both
    present, each in turn the later; `ui_target = "web"`). Choices within
    the plan and the user's answers:
    - **Key screen (answer 2): read-only in the page.** `open_keys` goes
      to the terminal whenever one is attached, whatever the target
      (keys change only there); with only a page, the page is sent `keys
      {row}` and shows them read-only (`keys.list`, now a passive op), and
      the answer's note says changing one needs a terminal.
    - `goto` checks for a screen before resuming the session (no point
      starting what can't be shown).
    - Notices go out for `goto` and the forms (new session, confirm,
      code folder, keys), not for `set_list_view` or `show_list`.
    - `set_list_view` on the web sends `set_view`, then `show_list` unless
      `show: false`, and updates the page's view in the daemon at once.
  - Step 5 done (`web.rs`): ops `form.defaults`, `form.repos`,
    `form.setup` (status and suggestion in one answer), `form.setup_save`,
    `overlord.new_session` (a `project` becomes the form's wire shape,
    `folder_request`), `overlord.setup_request`, `session.new` (`launch`:
    a `proposal` id brings the stored `context` and `related`, and
    `by_overlord`), `session.fork`. Argument filtering moved into
    `filtered` and is unit-tested per op (the "never" column dropped).
    Fixtures written by `web_api.rs` `the_forms_ops_and_fixtures`
    (`RLORD_UPDATE_FIXTURES=1`): `form_defaults.json`, `form_repos.json`,
    `form_setup.json` and `ui_cmds.json` (one of each command), with the
    scratch folder written as `/scratch`. Choices within the plan:
    - `form.setup_save` also passes `allow_home`: the plan's table left it
      out, but without it the form could never save a confirmed home
      folder (`save_call`'s home rule).
    - The look-ups (`form.defaults`, `form.repos`, `form.setup`) don't
      count as use of the page; the saves and launches do.
    - `web_api.rs` now points `HOME` at its scratch folder (the repo
      finder and the suggestion look there).
  - Step 6 done (`crates/web/src/{daemon,http,main}.rs`): page ids
    `p<n>-<8 hex>`, the `hello` carries `page`, every `web.call` carries
    the socket's own id (a `page` in the browser's frame is ignored),
    `web.pages` on every open (before `hello`) and close, and once at
    start with none; `Subscription::next` yields `Event::Changed` or
    `Event::Ui {page, cmd}`, and a UI command goes to that page's socket
    only as `{"event":"ui","cmd"}`. `web.pages` calls are serialized so
    the daemon gets the lists in order. Test: `http.rs`
    `pages_and_their_ui_commands`; unit test for the stream's lines.
  - Step 7 done (`ui/src/{proto,api,store,ui,main}.ts(x)`,
    `components/{Toast,Modal,FormHost,usage}`, `views/List.tsx`):
    `UiCmd` and the forms' answer types in `proto.ts`, held to the
    daemon's fixtures by `fixture.test.ts`; `ui.ts` `decide` (pure: goto,
    show_list, set_view, notice, and a form command that opens, or is
    parked behind a banner when the open form was typed into) and
    `Reporter` (`page.report`: only what changed, everything again after
    a reconnect, input at most every 5 s from pointer, key and wheel
    events); `page.pending` on every hello. The store maps between the
    page's view and the daemon's (`viewOf`, `listView`), sends the filter
    as `query` with the usage measure, highlights `set_view`'s `select`,
    and a browser with no saved view starts from `view.default`. Rows
    show the usage figure in the list's measure. The form host shows a
    placeholder until steps 8 and 9.
  - Step 8 done (`ui/src/forms/{hints.ts,NewSession.tsx}`,
    `components/Picker.tsx`, the list's "+ New session" and "Manual",
    keys `n` / `N`): `hints.ts` is the TUI's rules (`fresh`, `setAgent`,
    `picked`, `described`, `owner`, `setLocation`, `typedBase`, `request`,
    `launch`, `prefill`, `prefillConfirm`, `agentBlocker`), Vitest-tested
    against the daemon's fixtures (labels, the confirm round trip, a plain
    folder, a proposal launched by its id). The describe form sends
    `overlord.new_session` and goes to the overlord; the manual form sends
    `session.new` and goes to the new session; a failure keeps the form.
    Each hint shows whose it is (yours, the overlord's, default) and
    "changed" when the overlord changed it; a reopened form's note sits on
    top and Send reads "Confirm". The repo picker lists the finder's
    repos (asking again while the first scan runs), and takes a typed path
    as a repo or a plain folder (the daemon checks it at launch).
    Found on the way, fixed in its own commit: `page.pending` handed a
    command to a reloaded page even when the old page had acted on it;
    any call from the addressed page now clears it, and the page answers
    each command with an empty `page.report`. Also fixed: the page's
    report effect returned before reading its signals while connecting,
    so it never ran again.
    Checked headless (Chromium and WebKit, demo `rlord-demo-s2`, port
    47841): `propose_session` from the stand-in opens the proposal with
    the overlord's note read-only; Launch starts the session, the page
    goes to it, and its background holds the note (from the daemon's
    proposal store).
  - Step 9 done (`ui/src/forms/{Fork.tsx,setup.ts,CodeFolder.tsx,
    Keys.tsx}`): the fork form (Fork among a session's buttons, key `f`
    on a session; a worktree offered for Claude Code and Codex in a repo,
    as `ForkForm::for_session`; warnings shown as a notice); the
    code-folder form ("Code folder" in the header; the overlord's
    `confirm_setup` opens it with its note and marks what it changed;
    with a description it asks the overlord, else `form.setup_save`); the
    key screen, read-only ("Keys" in the header, and `open_keys` with
    only a page), with the note that keys change in a terminal. Vitest:
    `forms.test.ts`. Choices within the plan:
    - The code-folder form is `CodeFolder.tsx`, not `Setup.tsx`: on
      macOS's case-insensitive disk `forms/Setup` resolves to
      `forms/setup.ts`.
    - The browser can't look at the disk, so it doesn't decide "create it"
      or "home" itself: the daemon's save refuses, the form reads which
      question its message asks (`askFor`), and the user's answer goes
      back with `create` or `allow_home`.
    Checked headless (Chromium and WebKit): `open_keys` from the stand-in
    shows the keys with the asked one highlighted; `confirm_setup` opens
    the form with its note, saving a new folder asks first, then writes
    `code_dir` to the demo's config; a fork from a session's view starts
    and the page goes to it.
  - Step 10 done (`overlord.md`, `mcp.rs`): the intro's summon line
    names the web UI's overlord row; a short "The user's screen"
    paragraph (terminal or web UI, whichever used last, `on` in each
    answer, no overlay to hide in the web UI); `get_ui_state` (`target`,
    what the screen shows), `goto_session` and `show_list` say "the
    user's screen"; `open_keys` "terminal only; the web UI shows them
    read-only". `mcp.rs`: `get_ui_state` names `target` and `web`;
    `goto_session`, `show_list`, `set_list_view`, `open_keys`,
    `confirm_setup`, `propose_session`, `confirm_new_session` and `go` on
    `create_session` / `fork_session` say "the user's screen (their
    terminal, or the web UI if they used it last)" and "(in a terminal,
    hides your overlay)". No tool added or removed; `READ_ONLY_TOOLS`
    and `MUTATING` unchanged.
  - Step 11 done: headless pass (Playwright 1.65.1, outside the repo)
    in Chromium and WebKit against a second demo (`RLORD_DEMO_SOCKET=
    rlord-demo-s2`, `/tmp/rwd/s2`, `[web] port = 47841`), the overlord
    stood in for by raw `mcp.call` requests on the demo's socket, and a
    terminal stood in for by a tmux client on a PTY on the demo's socket.
    17 checks, all passing in both browsers: with only the page,
    `get_ui_state` says `web` and what the page shows; `goto_session`
    moves the page; `set_list_view` fills the filter box, filters and
    highlights the row; `propose_session` opens the proposal with the
    overlord's notes, Launch starts it with its background; the describe
    form's request reaches the overlord's pane, `confirm_new_session`
    (with that request's id) reopens it with the agent marked "overlord's
    · changed" and the note, and Confirm sends it back `confirmed: yes`;
    a form the user typed into stays while the overlord's proposal waits
    behind the banner; with two tabs only the one used last moves; with a
    terminal too, the one typed in last gets `goto_session` (the other
    keeps its place; the page shows the notice), and `ui_target =
    "terminal"` (config reloaded) sends `show_list` to the terminal though
    the page was used last; at 390 px wide the form fits with no sideways
    scroll; no console errors. Found and fixed: the parked-form banner sat
    under the dialog's backdrop, so "open it now" couldn't be clicked.
    Test-harness notes: the demo's stand-in overlord never reads its
    input, so its terminal's input queue fills after a few requests (the
    pass restarts it before each); what it was typed is read with
    `pipe-pane` (its screen redraws).
  - Step 12 done: `docs/architecture.md` "Web UI" ("The web as a UI
    target": surfaces and the target rule, pages and `page.report`,
    addressed `ui` lines and the parked slots, the hand-answered ops and
    the proposal store; `snapshot`'s `query` and `usage`), `docs/guide.md`
    (the forms, keys read-only, "the screen you used last", `[web]
    ui_target`), `SECURITY.md` (the page launches and forks directly and
    saves the code folder), the config template (`ui_target`). CLAUDE.md's
    browser-op rule still holds as written.
- Owed after slice 2 (carried over from slice 1, plus slice 2's own):
  - By hand in a real browser: Option as Meta and the browser-kept keys
    on a real keyboard, the wheel and copy, IME, the TUI overlay open
    alongside, WebGL context loss mid-session; slice 2's forms with a real
    keyboard (Tab order, Enter in the pickers), and the target rule with
    a person moving between a real terminal and the browser.
  - The release dry run's macOS half (signing, `Verify signature` on both
    binaries) and the `release` job's asset check, from `main` (or with
    `feature/*` let into the `release` environment: the user's call).
  - Not headless-checked: the code-folder form's "Use the home folder
    anyway" path (unit-tested in `forms.test.ts`; the demo can't point at
    a real home safely), and a usage limit's display (unit tests only).
  - The overlord's "no overlord yet" error from `overlord.new_session`
    still names `Alt-O` and the manual form's `N` (`ask::send`); in the
    page the form shows it as is. Worth a web-neutral wording later.
  - The relay and the phone app (the rest of this effort).
- Slice 3 planned ("Slice 3 plan: e2e core and the relay" at the end;
  Fable planning agent, 2026-11-17, branch `feat/web-ui-3`): `crates/e2e`,
  `e2e-wasm`, `relay-proto`, `relay` (`rlord-relay`) and a test client;
  `rlord-web` dials out to one pinned relay; pairing with the host's
  confirmation; remote ops through `web::OPS` with a remote column.
  Nothing built. Also recorded: the user's production-relay requirements
  (storage backends, identity and metering, load shedding, accounts) in
  the spec, and in the plan what slice 3 leaves room for and what waits
  for a production-relay slice. Eighteen open questions wait for the
  user.
- Slice 3 build (Opus implementer, 2026-11-17, `feat/web-ui-3`):
  - Step 1 gates all pass. (a) `snow` 0.10.0's raw split (feature
    `risky-raw-split`) plus our own ChaCha20-Poly1305 cipher state gives
    the same frames as its `TransportState` with `rekey_outgoing` after
    every message: checked inside the vectors run itself
    (`vectors::run` fails on any difference), so it stays checked. (b)
    `rustls` 0.23 (ring), `tokio-rustls` 0.26, `p256` 0.14 (ecdsa),
    `rusqlite` 0.42 (bundled) and `webpki-roots` 1 cross-build with
    `cargo zigbuild` to static binaries for both musl targets (a scratch
    crate). (c) The spike's core builds natively and for
    `wasm32-unknown-unknown` inside the main workspace (edition 2021,
    resolver 2, the spike's exact pins next to the workspace's own
    `sha2` 0.10.9 and `getrandom` 0.3.4: no duplicates).
  - **Naming collision found** (the plan breaks here, by name only):
    rlord already has a `remote.*` RPC family, `crates/daemon/src/remote.rs`,
    `rlord remote <session> on|off` and per-session `remote` fields, all
    for Claude Code's Remote Control, and the MCP server calls
    `remote.status` and `remote.set`. The plan's `remote.*` user-only
    commands, `daemon/src/remote.rs` and `rlord remote setup|pair|…`
    would collide. Taken instead (rename is cheap if the user prefers
    another word): RPC family `relay.*` (`daemon/src/relay.rs`), CLI
    `rlord relay setup|pair|confirm|reject|devices|revoke|status|disable`,
    TUI `:relay` (Devices screen `tui/src/relay.rs`), snapshot key
    `relay`. Config stays `[remote]` and the plugin's module stays
    `crates/web/src/remote/` (no collision there).
  - Steps 2 and 3 done (`crates/e2e`, lib `rlord_e2e`): the spike's core
    ported to edition 2021, libcrux only (the RustCrypto backend, the
    classical baseline and the `pq-*` features dropped); `Kind` (1
    pairing, 2 reconnect, 3 mailbox keying); `Established::into_session`
    (kinds 1 and 2) and `into_mailbox` (kind 3 only, over the raw
    split); a `Session` poisons itself on any decrypt failure
    (malformed frames included) and every later call returns
    `Error::Decrypt`; `stream.rs` (stream id and `MORE` flag,
    `fragment`, `Reassembler` per stream with the 1 MiB limit, `single`
    for one-frame messages); `mailbox.rs` (`MailboxSession`, Noise's
    cipher state and `REKEY` by hand over `chacha20poly1305` =0.10.1,
    Padmé padding, `export`/`import` of 81 bytes, poisoned on any
    failure); `device_id`. Vectors in `crates/e2e/vectors/v1.json` (88 KB,
    was 232 KB) by `cargo xtask e2e vectors [--check]`; the protocol test
    fails when the file is stale. New vector cases: the kind-3
    handshake, a mailbox sequence both ways (equal to snow's transport,
    and with export and import between every message), an export
    mid-sequence, fragment headers, one 16 KiB frame by hash, and the
    failures (wrong kind both ways, mailbox as live and live as mailbox,
    tampered message 1 and 2, a frame after poisoning, oversized frame,
    mailbox gap, replay, tamper, export after poison, bad stream flags).
  - Added to the core while building: a **mailbox id** (8 bytes from the
    keying handshake's hash, part of the export, 89 bytes now), sent in
    clear before each lane-2 frame, so a frame queued under a replaced
    mailbox is dropped by id instead of poisoning the new one.
  - Step 4 done: `crates/e2e-wasm` (the spike's wrapper without the
    benchmark-only calls, plus `E2eMailbox`, mailbox handshakes,
    `fragment`, `Reassembler`, `device_id`, `run_vectors`); `cargo xtask
    e2e wasm|test-wasm` (O3, `+simd128`, bindgen in process into
    `crates/e2e-wasm/pkg/`, 562 KB raw with the vector runner); a
    separate Vitest project (`vitest.e2e.config.ts`, `test/e2e/`,
    excluded from `web check` and from `tsc`, since the page has no Node
    types): the vectors byte for byte, fragmenting, and an interop run.
    `rust-toolchain.toml` gains `wasm32-unknown-unknown`; CI's `web` job
    runs `cargo xtask e2e test-wasm`. Deviation: the interop run is the
    WASM core as a device against the native core as host over stdio
    (`rlord-device interop-host`), not through the relay: a WASM host
    would need the relay link's P-256 signing in JS, which no WASM build
    has yet. The relay path is covered natively (below).
  - Step 5 done: `crates/relay-proto` (`msg`, `frame`, `auth`,
    `address`, `check_challenge`). Transcript fields are length-prefixed
    (the plan wrote plain concatenation). Binary frames put the lane
    first (the plan put the address first) so a frame parses without
    knowing its lane. p256 0.14 needs getrandom 0.4's `wasm_js` on wasm32
    (a target-only dependency). `docs/working/e2e-protocol.md` written as
    the shared spec (to be reviewed, then promoted).
  - Steps 6, 7 and the relay half of 17: built in parallel by another
    Opus agent on `feat/web-ui-3-relay`, merged here (see its entry
    below).
  - Step 8 done: `check-deps` gains the longer `rlord` deny list, the
    relay tree check, the e2e sans-IO check, the plugin-listener source
    check and the bind-to-every-interface source check, each with a
    planted-name test.
  - Step 9, 14, 15, 16: built in parallel by another Opus agent on
    `feat/web-ui-3-daemon`, merged here. As built: `web::Op.remote`
    (`op()`/`local()`), `via` required, `relay.*` dispatched from the
    socket only (never `Daemon::rpc`, so never MCP or `web.call`),
    `WEB_PROTOCOL` 3, `overlord.messages`, the `device.*` ops, the
    mirror with `notice_all`, `rlord_core::qr` (`qrcode` 0.14.1, no
    default features), `rlord relay …`, `:relay` (TUI Devices screen),
    the page's Devices modal. Its deviations: the page's Devices is a
    modal (form kind `devices`), not a route (`page.report` only takes
    route kinds); `snapshot.relay.devices` is a count; every forwarded
    command starts the plugin if needed (not only setup);
    `relay.pair` checks setup first; `setup --reset --yes`; `revoke`
    takes an id, id prefix or name; `rlord web remove` also turns
    `[remote] enabled` off; a fresh QR survives a state report up to 3 s
    older than it. The overlord: nothing new (it can see
    `web.form = "devices"` in `get_ui_state` while the modal is open,
    and no device data), so `overlord.md` and `mcp.rs` are unchanged.
  - Steps 10 to 12 done (`crates/web/src/remote/`): one actor task owns
    keys, devices, the relay connection, links, sessions and the
    pairing; daemon calls run in their own tasks and come back to it.
    `keys.rs`, `link.rs` (TLS with ring and webpki roots plus `ca_file`;
    no redirects, no extensions; close codes mapped to final or retry),
    `pairing.rs`, `devices.rs`, `session.rs` (20 ops/s, burst 40),
    `mailbox.rs`, `audit.rs`. Off unless enabled, pinned, and a device
    or a pairing exists; "wrong relay" and final close codes stop it
    until a command or restart; `bye`'s `retry_after` honoured with up to
    20% jitter. Deviation found by the crash tests: the mailbox's state,
    request ids and the last handled `(boot, seq)` are one atomic file
    (`mailbox-<id>.json`), saved before acting and acknowledging;
    otherwise a crash between saving and acknowledging made the
    redelivered frame fail to decrypt and discard the mailbox.
  - Step 13 done: `crates/e2e-device` (lib `rlord_e2e_device`, bin
    `rlord-device`: pair, check, snapshot, ask, interop-host) with hostile
    relays in `testing.rs` (a relay claiming the pinned key, a faithful
    forwarder under its own identity, a redirect). Tests:
    `e2e-device/tests/pin.rs` (claimed key, other key, other origin,
    redirect: nothing sent after the failed check, no other address
    tried) and `web/tests/remote.rs` against the real plugin, daemon and
    relay: pair and confirm, the cheap check, snapshot, `changed`,
    `overlord.ask` reaching the daemon, the local-only ops refused, a
    signal waking a device, a snapshot of several frames, a mailbox
    request answered after the plugin returns, `repeat`, ops refused by
    mailbox, revocation (link closed, 4403 after), reject, a spent QR,
    three wrong secrets closing the rendezvous, a forwarder failing on
    both ends, replayed and tampered frames, no dialling without setup
    or after `disable` across a restart, both mailbox crash points
    (`RLORD_WEB_TEST_CRASH`, debug builds only), and a mailbox frame
    whose save fails (`RLORD_WEB_TEST_FAIL`, debug builds only: memory
    is put back, so the frame the relay delivers again is handled).
  - Step 18 (docs): `docs/architecture.md` "Remote access (through a
    relay)", `docs/guide.md` "Remote access through a relay",
    `SECURITY.md`, `CLAUDE.md`'s rules and Releases, `docs/relay.md`
    (the relay agent).
  - Relay (steps 6, 7, the relay half of 17), as built: `crates/relay`
    (lib `rlord_relay` with `start`/`Running`/`Admin` for in-process
    tests, bin `rlord-relay`), `Store` trait with SQLite and its
    conformance suite, `StaticAuthorizer`, the `Meter`, backpressure by
    64 frame permits per connection (the sender isn't read while a
    frame waits; `bye idle` and 4408 after `stall`), files and proxy TLS
    (SIGHUP reload in files mode; ignored in proxy mode), the release
    tarballs, `deploy/relay/`, `cargo xtask relay-image` and a
    `relay-image` job in `release.yml`. Its deviations: a refused binary
    frame is answered `err` with `n: 0` (a binary frame has no number;
    clients number requests from 1); a pairing attempt counts when it
    connects (the relay can't tell success from failure; the host
    closes the rendezvous on success); a second pairing device gets 4429;
    `drop` closes with 1000; shutdown sends `bye overloaded` (retry 5 s)
    then 4503; the per-address limit closes after the upgrade (`bye
    limit`, 4429); a peer leaving over 1024 control messages unread is
    closed (4408); the relay waits up to 1 s for a close reply; every
    admitted host gets a store record; mailbox deliveries are bounded by
    the queue limits, not the frame permits; the binary caps
    tungstenite/axum/hyper/rustls logs at `info` (at trace they print
    payloads); `ready.limits` is sent to hosts; the image is pushed by
    the publishing run just before the release is published (the
    workflow only makes a draft), `--local` releases get no image.
  - `check-deps` deviation: `wasm-bindgen` is not on `rlord`'s deny list:
    `ulid` (through `web-time`) depends on it for wasm32 targets only,
    which `cargo tree --target all` shows; rlord is never built for
    wasm32. `robot-overlord-e2e-wasm` stays denied by name.
- Slice 3 reliability pass (Opus implementer, 2026-11-17, `feat/web-ui-3`;
  the relay and e2e-core parts by two forks of it). The rule, now stated
  in `e2e-protocol.md` §4 and `docs/architecture.md`: one websocket link
  is one Noise session per device link with implicit nonces; delivery in
  a connection is lossless and in order; any reconnect is a fresh
  `KKpsk2`; recovery is resync by snapshot `rev` plus resend by request
  id. No explicit nonces. What changed:
  - Host never drops a frame quietly (`remote/outq.rs`): bounded queues
    per kind and per link, a writer that sends control first and takes
    turns between links; a link that overflows is dropped (only that
    device's session ends), a control message that can't be queued closes
    the connection, a binary frame the host can't parse closes it (it used
    to be skipped). A link's `drop` waits behind its frames (a pairing's
    `done` arrives before the close).
  - One request store per device for both paths (`remote/requests.rs`,
    `mailbox.rs`): an op that changes something needs an id and runs at
    most once per id; a resend gets the stored answer or `in_progress`
    and then the answer (by the live link it came on and the mailbox if a
    resend came that way); a request running when the plugin stopped is
    `interrupted` (was `repeat`). `web::Op.idempotent` marks the look-ups;
    `web.hello`'s answer lists the remote ones. `proto.ts` gains
    `RemoteErrorCode` and `RemoteReply`.
  - Mailbox gaps: the receiver skips up to `MAX_SKIP` = 64 lost messages
    (vectors: `mailbox_gap_over_max`, `mailbox_reordered`, `mb_gaps`
    replace `mailbox_gap`). The relay answers each lane-2 frame `queued`
    or `refused` (was `err n:0` on refusal only); the host sends one
    mailbox answer per device at a time and resends one refused `full`
    (same bytes) when the device links and every 30 s. `rlord-device`
    drops mailbox frames by `(boot, seq)` as the host does.
  - Relay: flow control per link; device to host waits only that device
    and a stall closes the device (4205), never the host; host to device
    never blocks the host (128-frame device buffer; overflow closes the
    device with 4205 and tells the host `undeliverable`); `drop` closes
    with 4205 (was 1000); a host's read bucket scales with 1 + its links.
    New close code 4205 "reset: reconnect now" in `relay-proto`.
  - Detection: the host pings every 15 s and gives up after 45 s of
    silence (was 70 s, relay pings only). Rust's `Instant` on macOS is
    `CLOCK_UPTIME_RAW`, checked on this machine: it stood 13.6 h behind
    `CLOCK_MONOTONIC` (time asleep), so it doesn't advance in sleep, and
    Linux's `CLOCK_MONOTONIC` doesn't count suspend either. The host
    compares wall and monotonic clocks each second and reconnects at once
    when either jumped 10 s (covers both kinds of clock).
  - Tests: `outq` unit tests (order, overflow, close), the request store
    and outbox bounds, sleep detection, `only_look_ups_are_idempotent`;
    in `web/tests/remote.rs` a resend by mailbox after the live link died
    runs once, a relay restart losing the queue then a resend works, a
    flooding device never breaks another's session, an answer refused by
    a full queue is sent again; relay tests for per-link stalls, a
    non-reading device closed alone, `drop` 4205, `queued`/`refused`.
- Linux CI fixes on #77 (2026-11-18). Two `web/tests/remote.rs` failures
  on ubuntu runners, both real:
  - A flooding device held another's snapshot up 12 s (2 s on macOS).
    The relay reads a host at its rate times (1 + links); the host wrote
    as fast as the socket took it, so the backlog moved out of the
    per-link queues into the kernel's socket buffers (megabytes on Linux
    loopback), first in, first out, and turn-taking no longer applied.
    The writer now paces itself at 90% of the relay's rate and burst
    (`ready.limits` gains `burst_bytes`). Reproduced and checked in a
    Linux container: 12.9 s before, 1.2 s after.
  - Pairing signalled the new device: the mirror's change (`web.remote_state`)
    moved `rev`, and the host signals every device without a live session
    on any move, so a pairing, or a device's own live session ending,
    woke it (and used up its 30 s signal window). The daemon now marks a
    relay-status-only change (`changed_relay`, `"relay": true` on
    `web.subscribe`) and the host signals no one for it.
  - `advisories`: `proc-macro-error2` (RUSTSEC-2026-0173, unmaintained)
    comes with `libcrux-ml-kem` through `hax-lib-macros` 0.3.7; no newer
    `core-models` moves off it. Ignored in `deny.toml` with a reason.
  - Trailing signal: a change inside a device's 30 s signal window was
    dropped, so the device missed it until a later change. The host now
    owes it one signal when the window ends (`remote/signals.rs`), still
    at most one per window, and forgets it if the device went live or was
    revoked meanwhile.
- Owed after slice 3:
  - **The phone slice must** (the device side of the rule above;
    `e2e-protocol.md` §4): reconnect in a loop with backoff, at once on
    4205 (backing off if it repeats), on a network change and on coming
    to the foreground; run `KKpsk2` each time its host comes online
    (`host online`, `ready.host`); compare the snapshot's `rev` and fetch
    when it differs; give every request an id unique for the device and
    resend unanswered ones by id after any reconnect; when the relay's
    `boot` changes, resend every mailbox request it has no answer to (the
    queue is gone); answer `queued`/`refused` (resend a refused frame
    later, before newer ones); drop mailbox frames by `(boot, seq)` and
    answers by request id it already has; treat `in_progress` as "wait",
    `interrupted` as "check, then ask anew".
  - **The protocol review** (`docs/working/e2e-protocol.md`), before
    anyone but the user runs a relay. Then promote that doc.
  - By hand: `rlord-relay` in Docker compose on a real host with a real
    certificate, a host behind NAT, `rlord-device` on another network;
    pair, confirm, ask the overlord, revoke. The multi-platform `docker
    buildx` build, push and attestation and the release workflow's
    `relay-image` job have not run (no buildx here); the new ghcr.io
    package starts private and must be made public once.
  - Adversarial cases not automated end to end: a relay that lets a
    revoked device through (the core's vectors and the host's
    allow-list check cover it: `kk_revoked_device_host`,
    `device_link`), a relay that invents `link` for a device not on the
    list (covered by `device_link`'s own check, untested through a
    hostile relay), a relay that swaps two devices' frames, a mailbox
    frame replayed or skipped through the relay (the core's vectors
    cover replay, gap and tamper), a recorded pairing replayed after the
    rendezvous closed. Client-side `bye` `retry_after` and `enroll`
    handling in the plugin are built but tested only on the relay's side.
  - Not built: TLS channel binding (open question 9, no), mTLS (5,
    later), ACME (2, later), compression (provisional in `e2e-crypto.md`;
    real snapshot sizes still to record), the phone app, panes over the
    relay (streams 1 and up close the link).
  - Owed from the spike: phone benchmarks, Firefox.
  - The CLI's pairing flow against a live daemon has no end-to-end test
    (the CLI autostarts a real daemon); its logic is unit-tested and the
    same daemon commands are exercised by `web/tests/remote.rs`.
- Slice 3 merged into `feature/web` (#77).
- **The feature branch stays dark** (user, 2026-11-18): no releases, no
  `release.yml` runs and no app-store submission from `feature/web` or
  its branches; build locally only until the final merge to `main`.
- Slice 4 planned ("Slice 4 plan: the phone app" at the end; Fable
  planning agent, 2026-11-23, branch `feat/web-ui-4`): a native Rust
  device engine (`crates/e2e-client`, UniFFI in `crates/e2e-ffi`) inside
  thin Swift and Kotlin shells around a WebView that runs a phone build
  of the page with no network access; `Transport` with `LocalTransport`
  and `RemoteTransport`; QR pairing; the reconnect loop with every
  requirement owed above; no push yet (background refresh and local
  notifications); the phone as a third surface; local installs only.
  Nothing built. Xcode 27.2 is installed (`xcode-select` points at the
  command line tools, so tasks set `DEVELOPER_DIR`); no Android SDK yet.
  Sixteen open questions wait for the user; the first
  (engine in the shell, not WASM in the page) changes an earlier answer.
- Slice 4 build (Opus implementer, 2026-11-23, `feat/web-ui-4`,
  1.3.0-dev.5). Answers: native engine, iOS first, team `PL2X8AJK8A`,
  bundle id `com.robot-lord.rlord`, defaults for the rest. iOS is built
  in this pass; the Android shell and SDK wait for a later PR.
  - Step 1 gates (scratch code, not kept). All iOS gates pass; Android's
    wait for the SDK and NDK.
    - (a) `rlord_e2e`, `relay-proto` and `rlord_e2e_device` (tokio,
      tokio-tungstenite, rustls with ring, webpki-roots) build for
      `aarch64-apple-ios` and `aarch64-apple-ios-sim`.
      `aarch64-linux-android` is added to the toolchain but not built:
      ring needs the NDK's C compiler.
    - (b) UniFFI 0.32.2: bindgen in process (`uniffi_bindgen::bindings::
      generate`, a library call, Swift and Kotlin from a debug dylib);
      a Rust object made from Swift, and a foreign trait (`with_foreign`)
      called back from a tokio worker thread, run on the iOS 27.2
      simulator (a Swift binary linked to the static library, started by
      `simctl spawn`). Kotlin generated, not run.
    - (c) Signatures from `SecKeyCreateSignature` with
      `ecdsaSignatureMessageX962SHA256` (software key) and from the
      Mac's own Secure Enclave (CryptoKit `SecureEnclave.P256`), DER
      converted to `r || s`, all pass `relay_proto::auth::verify`,
      including the high-S ones (20 and 23 of 40): p256's verifier
      takes high S, so the engine converts DER and does not normalise.
      The phone's own enclave stays in the by-hand list.
    - (d) A built Preact page (Vite, module script and stylesheet) loads
      in `WKWebView` from a `WKURLSchemeHandler` on `rlord-app://page/`
      under the planned policy as a response header and a `<meta>`:
      `'self'` matches the custom scheme, no `'unsafe-inline'` is needed
      (Preact's style objects go through CSSOM), Trusted Types are
      enforced (`innerHTML` throws `TypeError`), `fetch` and `WebSocket`
      are blocked by `connect-src 'none'`. The bridge round-trips: page
      to shell by `messageHandlers.rlord.postMessage` (main frame,
      `rlord-app` origin checked), shell to page by
      `callAsyncJavaScript` with the text as an argument. Found: the iOS
      27 SDK refuses a UIKit app without the scene life cycle; the shell
      is a SwiftUI `App`.
    - (e) A binary in the iOS simulator reaches a listener on the Mac's
      `127.0.0.1`.
  - The lead fixed one contract for the engine, the page and the shell
    before splitting the work (it refines "The bridge and
    RemoteTransport"): page messages `{"app":"ready"}` (the engine replays
    `conn`, `pairing`, `outbox` and the last `hello`, so a reload
    restores everything) and `{"app":"pair_continue","name"}` (after the
    engine's `pairing` `scanned` event, which carries the relay origin and
    the default name), beside the planned `status`, `pair_cancel`,
    `unpair`, `cancel`; the shell serves `scan` and `settings` itself and
    answers `{"event":"settings","app_lock","notifications","platform"}`;
    the engine answers `status` with `{"event":"status","host","relay",
    "state","device_id","device_name","keys","version","api":{min,max}}`;
    engine error codes `disconnected`, `host_offline`, `outbox_full`,
    `cancelled`, `expired`, `interrupted`. Steps 2 and 3, 4, and 5 and 6
    were built in parallel by three Opus agents on their own branches,
    merged here.
  - Step 4 done (daemon and plugin; agent branch `slice4-daemon`):
    `ui::Target::Device(id)`; `pick` takes devices as it takes pages
    (latest use, then newest; `ui_target = "web"` covers devices);
    `confirm_setup` and `open_keys` use `target_local` (never a phone;
    with only a phone they fail naming the phone, the terminal and `rlord
    web`); one `WebUi` record per device (at most 16, dropped when the
    plugin no longer lists the device as paired); one parked command per
    device for 10 minutes; `page.report` (idempotent) and `page.pending`
    remote, the device id stamped by the plugin; `device.leave`
    (`{"left":id}`; `bad_request` locally, new RPC code
    `RPC_ERR_BAD_REQUEST` -32012); `session.messages {ref, before,
    limit}` (`{"session":{id,name,agent,conversation},"messages",
    "more"}`, 50 messages of 4 KiB); `web::API` = 1 in `web.hello` and
    the device's hello (`{"event":"hello","version","api":1,"rev",
    "page":id,"device":id}`, no `build`); `WEB_PROTOCOL` 4;
    `get_ui_state` `target: "phone"` and a `phone` object (`open`,
    `devices`, `name`, `showing`, `visible`, `form`); answers say
    `"on":"phone"`; `overlord.md` and `mcp.rs` updated; the TUI ignores
    `to:"device"` lines. Fixture `crates/daemon/tests/fixtures/
    remote_api.json` pins the remote ops and snapshot keys within a
    level. Deviations: a plugin-only method `web.ui_delivered {device,
    n}` clears a parked command once the plugin sent it live (clearing on
    an empty `page.report` would race: the device id is stable across
    sessions, so its post-hello report would wipe a command it never
    got); a leaving device is removed when its link closes or after a 3 s
    grace (closing at once lost the answer at the relay); notices are
    never sent to a phone (when the phone is the target they go to the
    TUI, else the newest page); slice 3's `remote.rs` refused-op list
    swaps `page.report` for `device.revoke`. Official docs corrected in
    the same change (`docs/architecture.md`, `e2e-protocol.md` §3 and
    §5). Tests: `web_api.rs` `session_messages_are_bounded`,
    `a_paired_phone_is_a_surface`,
    `screens_on_this_computer_never_go_to_a_phone`,
    `device_leave_is_the_callers_own`,
    `the_remote_api_only_grows_within_a_level`; `web/tests/
    remote_surface.rs` (leave, a `ui` command live, parked then pulled
    with the relay's signal, the phone the target after it sent a form;
    later moved onto the engine as `engine_surface.rs`, below);
    unit tests in `ui`, `lib`, the TUI and the plugin.
  - Steps 5 and 6 done (the page; agent branch `slice4-page`):
    `transport.ts` (`Transport`, `Caps`, `LOCAL_CAPS`, `REMOTE_CAPS`,
    `ApiError`), `Control` became `LocalTransport`; `remote.ts`
    `RemoteTransport` and the bridge (`window.__rlord_recv(text)`,
    defined once, not writable; iOS `messageHandlers.rlord` first, then
    Android's `rlord` listener object; request ids count up from
    `Date.now()` so a reloaded page never reuses one); `phone.ts`,
    `chat.ts`, `commands.ts` (`runUi` shared by both entries; on a phone
    `setup` and `keys` become a notice), `phoneApp.ts`; the second entry
    `app.html` + `src/app.tsx` + `phone.css`; `views/Chat.tsx`
    (overlord chat with pending and failed bubbles, a session's notes and
    `session.messages`, "The live terminal isn't available remotely
    yet."), `views/Phone.tsx` (pairing screens, `#/status`, the
    connection line). Views take their body and local-only forms from
    their entry, so the bundles stay apart. `cargo xtask web build --app`
    writes `dist-app/` (`index.html`, `assets/`, the licenses) and
    `manifest.json` (`{"index","csp","files":[{path,type,bytes}]}`,
    paths with a leading `/`), then checks the bundle (`xtask/src/
    web_app.rs`, also `web check-app`): every file local, no network
    scheme or API, the CSP exact, no inline script, no xterm, no
    `LocalTransport` (found by a `static marker` string, since class
    names are minified). `web build` checks the local bundle has no bridge
    code. Lint: oxlint's `react/no-danger`. CI's `web` job builds the app
    entry too. Tests: `transport`, `remote` (a fake bridge, a reload
    restoring bubbles from `outbox`), `chat` (fixture
    `overlord_messages.json`, written by the daemon test), `phone`,
    `phoneApp`. No DOM tests (no jsdom dependency added).
  - Step 8 (`crates/e2e-ffi`, `app bindings`, `check-deps`) and most of
    step 9 (the iOS shell) built by the lead while the engine was built:
    - `crates/e2e-ffi`: proc-macro UniFFI, foreign traits `Signers`,
      `Vault`, `Listener`, object `Engine`, error `FfiError`, record
      `BackgroundResult`. Found: UniFFI 0.32.2 escapes Swift's `open`
      keyword with backticks into the C header, which then fails to
      compile, so the FFI's vault method is `unseal` (the engine's
      `open`).
    - `cargo xtask app bindings | core-ios | ios [--sim [NAME] |
      --device ID] | test-ios [--unit]` (`xtask/src/app.rs`, unit-tested
      logic: developer dir, versions, simulator choice, xcframework
      arguments). Bindgen in process (`uniffi_bindgen` =0.32.2 in xtask).
      Swift into `apps/ios/Generated/Swift`, the header and
      `module.modulemap` into `Generated/Headers`, the xcframework
      `apps/ios/Frameworks/RlordCore.xcframework`, the page into
      `Generated/page` (all git-ignored). Apple wants a numbers-only
      `CFBundleShortVersionString`: `1.3.0-dev.5` builds as marketing
      version `1.3.0`, build number `5`. Kotlin is generated only once
      `apps/android` exists.
    - `check-deps`: `rlord` denies `robot-overlord-e2e-ffi` and
      `uniffi`; a source check fails if `apps/` names a network API
      (`URLSession`, `NWConnection`, `CFNetwork`, `socket(`, `java.net`,
      `okhttp`, `HttpURLConnection`, …; generated and build folders
      skipped; `NWPathMonitor` allowed), with planted-name tests.
    - `apps/ios`: a committed Xcode project (objectVersion 77, folders
      synchronised, so new Swift files need no project edit), a shared
      scheme with the unit and UI test targets, `Config/Base.xcconfig`
      (team `PL2X8AJK8A`, `com.robot-lord.rlord`, iOS 26, arm64 only,
      `#include? "../Local.xcconfig"`), `Local.xcconfig.example`,
      `Config/Info.plist` (background refresh id, `fetch` mode). Sources:
      `RlordApp.swift` (SwiftUI `App`, `.backgroundTask(.appRefresh)`),
      `Shell.swift` (engine, bridge routing, settings, lifecycle with
      the OS's finishing time, `NWPathMonitor` keyed by interfaces so a
      path change redials, app lock after 5 minutes away with
      `LocalAuthentication` and a cover in the app switcher, local
      notifications, the debug-only `-rlordQR` launch argument),
      `WebView.swift` (scheme handler from the manifest, CSP and
      `nosniff` on every response, content rule list, navigation limited
      to the first load and same-document changes, no windows, link
      previews, data detectors, media or fraud-warning lookups,
      non-persistent data store, inspectable in debug only),
      `Keys.swift` (Secure Enclave signer and ECIES vault,
      `AfterFirstUnlockThisDeviceOnly`; software keys only without an
      enclave, in debug builds), `Scanner.swift` (AVFoundation, `RLORD1:`
      only; a paste field in debug builds). No App Transport Security
      exception: the app makes no URL loads, and ATS doesn't govern the
      engine's own sockets.
    - Found: the Apple Silicon simulator passes the Mac's Secure
      Enclave through (`SecureEnclave.isAvailable` is true; enclave
      signing and ECIES work there), so the simulator runs the hardware
      key path, not the software fallback the plan expected.
    - `crates/web/examples/remote_stack.rs`: a loopback host for the
      app's runs (an in-memory daemon with fixture sessions, the real
      `rlord-web`, the real relay in process; commands `pair`,
      `restart-relay`, `stop-plugin`, `start-plugin`, `status`, by stdin
      or `<dir>/control/*.cmd` files so the simulator's test runner can
      drive it; `--auto-confirm`). `xtask` drives it (`app::Stack`).
  - Gap closed (agent branch `s4-mailbox`): a device never learned that
    its host had discarded its mailbox (a failure there, or the saved
    state losing it), so its mailbox frames were dropped by id and
    answers never came. The host's device `hello` now carries `mailbox`
    (the id it holds for the device, or null); the engine discards and
    keys a new one when it differs. Additive, `api` stays 1
    (`e2e-protocol.md` §3, step 4). The engine also no longer offers a
    refused frame, or marks one `queued`, that was sealed under a
    mailbox it has since discarded: the request goes again under the
    new one.
  - Step 7 done (agent branch `s4-bridge`): the phone page without a
    phone.
    - `rlord-device bridge --state DIR [--port N] [--ca FILE] [--qr
      RLORD1:…] [--name NAME] [--origin ORIGIN]…`
      (`crates/e2e-device/src/bridge.rs`): the engine with software keys
      behind a websocket on 127.0.0.1 that plays the shell. Each text
      message from the page goes to `Engine::page_message`, but `scan`
      and `settings`, which the bridge serves as the shell does
      (`settings` answers `{"event":"settings","app_lock","notifications",
      "platform":"bridge"}`, kept in memory; `scan` says `{"event":"scan"}`
      on stdout for whoever runs it to answer with `qr …` on stdin); each
      `to_page` goes back as a text message. Foreground while a page is
      connected; one page at a time (a new one replaces the last). A
      connection naming an `Origin` is refused unless given with
      `--origin` (another site open in the same browser can't reach the
      engine); Node sends none. Stdout: `ready` (URL, port), `page`,
      `scan`, `notify`; stdin `qr …`, `network up|down`, `quit` (and its
      end).
    - The dev page: `app-dev.html` + `src/appDev.ts` + `src/devBridge.ts`
      (`installDevBridge`: `webkit.messageHandlers.rlord.postMessage` is
      `ws.send`, each message calls `__rlord_recv`, so `findBridge` and
      `RemoteTransport` run unchanged), built by `vite build --mode
      app-dev` into `dist-app-dev/` (git-ignored) with
      `VITE_RLORD_BRIDGE_PORT`. Its CSP is the phone page's but
      `connect-src ws://127.0.0.1:<port>` (`web_app::check_dev`). The
      phone bundle can't carry it: `app.tsx` never imports it, and
      `check-app` fails on its marker (`rlord-dev-bridge`), `WebSocket`
      or any `ws://`; the local page's check fails on the marker too.
    - `remote_stack` gains a fake overlord (tmux needed): the overlord row
      is the example itself in a pane of the scratch tmux server, so
      `overlord.ask` types into it as into a real one, and it writes each
      line and an answer into a Claude transcript `overlord.messages`
      reads. `claude`, `codex` and `opencode` on its PATH are the
      example too (with `RLORD_NO_LOGIN_SHELL`), so a restart from the
      page can't start a real agent. `pair` hands out the code only once
      the plugin is linked to the relay (earlier, the device's pairing
      found the rendezvous closed). A `.cmd` file's `.done` holds the
      event the command said. The scratch tmux server and its socket file
      go at quit.
    - `cargo xtask app dev [--port N]` (`xtask/src/app_dev.rs`): the
      stack, the bridge paired by the stack's code, the dev page built and
      checked, `vite build --watch` and `vite preview` on 127.0.0.1;
      prints the URL; stdin takes `pair`, `restart-relay`, `stop-plugin`,
      `start-plugin`, `status`, `network up|down`, `quit`; the page's Scan
      gets a fresh code. Checked in a headless Chromium: pairs, lists,
      the overlord's chat, and a `fetch` is blocked by the CSP.
    - `cargo xtask app test-e2e`: the stack (fails without its fake
      overlord), the bridge with the stack's code, then
      `vitest.app-e2e.config.ts` (`test/app-e2e/phone.test.ts`, not in
      `npm test`) with `RLORD_BRIDGE_URL` and `RLORD_STACK_CONTROL`: the
      page's own `Store`, `Phone` and `wirePhone` in Node over the dev
      bridge and Node's WebSocket. Steps: pair (scanned, continue, the
      code, confirmed by the stack, live, the host lists the device);
      list (fixture rows, no `relay` section, `session.messages`, the
      bridge's `settings`); ask (`sent`, then the fake overlord's answer
      in `overlord.messages`); the relay restarted (a new hello, a
      request); the plugin stopped (`host_offline`, an ask waits in the
      outbox) and started (a new hello, the ask answered once, the outbox
      empty); unpair (`unpaired`, the host drops the device). After a
      failed step the rest are skipped. No Xcode, no `DEVELOPER_DIR`.
      CI's `web` job installs tmux and runs it. Run on macOS about 25
      times and on Linux (Debian, Node 26, without xtask, whose build
      needs more than Docker's 2 GB here) twice.
    - 2 of those macOS runs timed out at pairing. Found in the finishing
      pass: the stack confirms at once, so the six-digit screen could
      last less than the test's 50 ms poll; the test now watches every
      screen the pairing signal shows.
  - Steps 2 and 3 done (the engine; agent branch `slice4-engine`, merged
    here): `crates/e2e-client` (`machine`, `driver`, `outbox`, `store`,
    `link`, `soft`, `Engine`), `rlord-device` on it
    (`crates/e2e-device/src/phone.rs` wraps it for tests),
    `testing::Tamper` (a relay that holds the pinned key and forwards to
    the real one) and `Flaky`, `crates/web/tests/engine.rs` and
    `hostile.rs` (the owed hostile cases end to end). `check-deps`: the
    engine's tree names no rlord and no relay, and rlord never names the
    engine.
  - Finishing pass (lead, 2026-11-23; `feature/web` with main 1.15.3
    merged in, version 1.16.0-dev.2, local commits only, nothing pushed
    or released):
    - `crates/e2e-ffi` wraps the real engine (the stand-in is gone):
      adapters for `Signers`, `Vault` (`unseal` is the engine's `open`)
      and `Listener`; `BackgroundResult` names its outcome
      (`not_paired`, `nothing_new`, `host_offline`, `refreshed`,
      `failed`). The shell's scene-phase handler runs for the initial
      phase too, so the engine (which starts as if in the background)
      hears `lifecycle(true)` at launch.
    - The phone-surface tests through the engine (agent branch
      `s4-enginetests`): `crates/web/tests/engine_surface.rs` (a phone
      leaves by itself, a `ui` command live, parked then pulled with the
      relay's signal, the phone the target after it sent a form, the
      engine's look-ups are the daemon's idempotent ops); the low-level
      `remote_surface.rs` is gone. Found: the engine kept `page.report`
      in its outbox (saved, resent for 2 minutes, which could resend a
      stale `input: true`); it is a look-up now.
    - Supervisor bug (agent branch `s4-supervisor`): after `web().stop()`
      the dead plugin's `web.subscribe` stream still counted, because the
      daemon only noticed a closed stream on its next write, so a relay
      command right after a stop or restart went to the dead stream
      ("rlord-web didn't answer within 10 s", from `relay.rs`). The push
      loop now reads the client's end and ends at EOF; the stream count
      lives in the supervisor; `stop()` returns once the plugin is reaped
      and its stream closed (at most 1 s more), and its SIGKILL path
      loops until reaped. Tests in `web_supervise.rs` and `remote.rs`;
      the 500 ms sleep in `engine.rs` is gone.
    - `hostile.rs` `two_devices_frames_swapped` was flaky (about half the
      runs): passed on as they came, the host could drop the second link
      for the first swapped frame before the relay forwarded the second.
      The filter now holds the first until the other's arrives.
    - iOS tests (`cargo xtask app test-ios`, simulator): XCTest
      `LivePageTests` loads the real phone page through the scheme
      handler and content rules and checks from inside that `fetch` (off
      and on the page's scheme), a websocket and `innerHTML` are refused;
      `BackgroundTests` (an unpaired engine's check through the FFI, the
      notification's text); XCUITest `PairAndAskUITests` against the
      loopback stack (debug launch arguments `-rlordReset YES -rlordQR
      … -appLock NO`): pair, the fixture list, home and back, the relay
      restarted through the stack's control folder, then an ask answered
      by the fake overlord in the chat. The simulator's test runner
      writes the Mac's control folder directly.
    - Step 11: the shell is one shared instance (`Shell.shared`), so the
      OS's background launch runs the same engine as the window; the
      `.appRefresh` task calls `background_check` (20 s budget), logs the
      outcome and asks for the next refresh, as does every trip to the
      background; `notify` posts a count, never names, only when the
      user turned notifications on. A real refresh can only be checked
      on a phone (by hand).
    - Docs: `docs/architecture.md` "The phone app" (and the hello's
      mailbox), `docs/guide.md` "The phone app" (with the by-hand
      install), `SECURITY.md`, `CONTRIBUTING.md` "The phone app",
      `CLAUDE.md` (the app's network rule, how a page feature reaches the
      phone, the `api` level), `docs/relay.md` (public certificates
      only), `e2e-protocol.md` §4's device side as built,
      `e2e-crypto.md`'s phone row and answer 3.
- Owed after slice 4 (the branch stays dark: local builds only):
  - **Android** (steps 12 and 13): the SDK and NDK, `install-tools
    --app`, `app core-android`, the Gradle project, the asset loader and
    WebView settings, the message listener, Keystore `Signer` and
    `Vault`, CameraX with ZXing, lifecycle and the network callback, app
    lock, WorkManager, `app android`, `app test-android`, the Android
    gates of step 1 (a)–(e). Kotlin bindings are generated only once
    `apps/android` exists.
  - **No answer watchdog on a live session** (engine-agent finding): a
    request whose frame the host never got (a link dropped under it,
    say) waits until the next reconnect; the engine doesn't time it out
    or resend on the same session. Recorded, not built.
  - **By hand on the user's iPhone** (the install is in the guide):
    enclave key creation and signing accepted by a real relay; wifi to
    cellular and back; airplane mode; a long lock; the app killed
    mid-ask; a background refresh producing a notification (and a
    background launch with the app not running); app lock; revoke from
    the host while the app is open and while it is closed; the
    handshake's and first snapshot's time on cellular, with real
    snapshot sizes (owed to the compression question); the phone CSS in
    WKWebView (keyboard, viewport, scrolling).
  - `app test-e2e`'s first run under xtask on Linux is CI's first `web`
    job on this branch (it ran on Linux by hand, without xtask).
  - The page: no DOM tests (no jsdom dependency was added); the
    pairing, status and chat views are tested through their state and
    event sequences only.
  - Relays behind a private CA can't be reached from the phone (open
    question 15, deferred).
  - The protocol review before the final merge to `main` (slice 3
    answer 8), and the push slice with the hosted relay.
- Owed from merging main 1.15.3 into `feature/web` (1.16.0-dev.1):
  - The attention queue (main's `attention.rs`) counts only terminals as
    "looking": `tmux::list_clients` leaves web views out, so a session
    shown in a browser isn't stamped `last_viewed_at` and a focused
    browser doesn't count as presence. Decide whether a web view (and a
    device) should count, and how its focus is known.
  - Main's new daemon methods aren't on `web::OPS`: `attention.list`,
    `attention.report`, `attention.poke`, `overlord.answer_limit_offer`,
    `setup.import_offer` / `setup.import_answer`, `sctx.tools` /
    `sctx.call`. The page doesn't use `attention_reason` or
    `last_viewed_at` yet (in `proto.ts`, not drawn). Add any the page
    needs, each with a test.
- Slice 5 planned ("Slice 5 plan: live terminals on the phone" at the
  end; Fable planning agent, 2026-11-24, branch `feat/web-ui-5`): pane
  streams inside the device's Noise session (opened by a new remote op
  `pane.open`, credit flow control, tmux coalescing for a slow phone,
  reopen and redraw on reconnect), xterm.js with WebGL in the phone
  page, a phone-sized view under explicit control, a key
  row and a compose line, a per-device view-only switch, `web::API` 2
  (`overlord.ask` and `overlord.messages` leave the remote API; the
  transcript stays), and a real-Claude-Code test through the relay
  (`cargo xtask app test-term`). Nothing built. The user answered the
  nine questions the same day ("Slice 5 answers"): the size rule became
  an explicit control handoff (one screen controls a pane; the others
  show "Controlled by <location>" and Take over), which raises five
  new questions.
- Slice 5 build (Opus implementer, from 2026-11-24, `feat/web-ui-5`):
  gates and the contract in "Slice 5 build: gates and the contract
  between the parts" at the end. Gate 1a failed as stated (an unread
  tmux client's server memory grows with a flood); the plan's fallback
  is taken (the host always reads, holds at most 256 KiB, and past that
  reattaches a fresh view client behind a reset). 1b passes with
  `assume-paste-time 0` on a held session; a bracketed paste from the
  held terminal still reaches the pane (accepted). 1c to 1f pass in the
  simulator. Step 2 done: `stream.rs` pane frames (1 to 250, standing
  alone, 251 to 255 refused), the vectors (and the WASM runner) and
  `e2e-protocol.md` §3.

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
  release build makes `rlord-web` first, signs it (macOS: the same
  Developer ID as `rlord` since the merge of main's signing, identifier
  `com.robot-lord.rlord-web`), takes the SHA-256 of the signed executable and builds `rlord` with
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

## Slice 2 plan: the web as a UI target

Planned 2026-11-17 (Fable planning agent). Built on `feat/web-ui-2`
(steps 1 to 12; see "Progress" for what each did and the choices made).
Goal: a
user with only the web UI open can use everything the overlord does that
targets "the user's terminal" today: `goto_session`, `show_list`,
`set_list_view`, `get_ui_state`, `propose_session`, `confirm_new_session`,
`confirm_setup`, `go: true` on `create_session` / `fork_session`, and the
forms (new session `n`, manual `N`, fork `f`, the code folder), with the
describe-then-confirm loop running in the browser. No new MCP tool. No
network code in `rlord`; the plugin stays opt-in.

What exists today (read from the code, for the implementer):

- `ui.rs` does terminal navigation with tmux (`switch_user_to`,
  `ensure_home`, `hide_overlord`) and pushes typed commands to the TUI with
  `Daemon::ui_cmd` (`{"notify":"ui","cmd":…}` on the broadcast). Commands:
  `set_view`, `new_session`, `confirm_new_session`, `setup`,
  `overlord_setup`, `keys` (`tui/src/overlord.rs` `handle_ui_cmd`).
- One command is parked when no TUI is subscribed (`pending_ui`, 30 s,
  taken by `ui.pending`). The TUI reports its view with `ui.view.report`.
- `web.subscribe` drops every `ui` line (`push_loop`), and the drain after
  a `changed` throws away whatever came meanwhile.
- `web.call` filters arguments per op (`web::OPS`); `ui.*` and `setup.*`
  are forbidden method families.
- The forms call `session.defaults`, `repo.find`, `overlord.status`,
  `overlord.new_session` (`ask::new_session`), `session.new`,
  `session.fork`, `setup.status`, `setup.suggest`, `setup.apply`,
  `overlord.setup_request`.
- The page keeps its own view in `localStorage`; its filter is free text
  only (`snapshot` passes `search`, not the `tag:` / `status:` syntax).

### Decisions

#### Which screen a command lands on: the one the user used last

A **surface** is somewhere the user looks at rlord: the terminal (a tmux
client, with or without the TUI) or a web page (one browser tab's control
socket). Later the phone is a third; the code should hold a list of
surfaces with a last-used time, not a terminal/web boolean.

- **One target per command, chosen by plain code: the surface the user
  last did something on.** With one surface present it is the target.
  With none, the tools fail as today, with a message naming both ways in
  (`rlord` in a terminal, or `rlord web`).
- **Not both.** Rejected: a form open on two screens can be sent twice
  (two launches), and a terminal the user left on a session would be
  pulled to the list while they work in the browser.
- **Last-used time, per surface:**
  - Terminal: tmux's `client_activity` of `tmux::user_client` (as today's
    choice of client).
  - Web page: the latest of (a) the daemon's own clock when a
    user-driven op arrives from that page (every op except `snapshot`,
    `session.get`, `page.report` without `input`, `page.pending`,
    `form.*` look-ups), (b) a `page.report` with `input: true` (the page
    sends one on pointer or key input, at most every 5 s), and (c)
    `tmux::view_clients` activity for the pane that page shows (typing
    into the overlord's pane in the browser). The daemon stamps the time;
    the page never sends one.
  - Sending a form counts: `overlord.new_session` from a page makes that
    page the target for the `confirm_new_session` that answers it; the
    same call from the TUI's socket leaves the terminal ahead through its
    key press.
  - A tie or an unknown goes to the terminal (today's behaviour).
- **Among pages**, the target is the one with the latest time. A hidden
  tab still counts (the user who asked from it and switched tabs finds
  the form when they come back); `visible` is reported for
  `get_ui_state` only.
- **The other surface gets a one-line notice** when there is one:
  `{"cmd":"notice","text":"the overlord opened a new-session form in the
  web UI"}` (the TUI's status line, the page's toast). Choosing wrongly is
  the main way this can annoy; the notice makes it visible.
- **Every targeted tool's answer says where it landed** (`"on":
  "terminal" | "web"`), so the overlord can say "it's open in your
  browser" when that matters.
- **Terminal only, unchanged:** the overlord overlay (`ui.overlord`,
  `ui.overlay.*`), the overlord's first-run dialog (`overlord_setup`) and
  the key screen (`open_keys`: it tests that a key reaches rlord through
  the terminal, which a browser can't). `open_keys` with no terminal fails
  saying the key screen needs a terminal. When the target is the web,
  nothing touches the terminal: no `hide_overlord`, no `ensure_home`.

`ui.rs` gets one function that decides, `ui::target(d) -> Target`
(`Terminal(Client)`, `Web(page id)`, `None`), built on a pure
`pick(terminal: Option<ms>, pages: &[(id, ms)])` that is unit-tested.
Each tool calls it once and passes the result down, so the command and
the "bring it to the front" step can't pick different surfaces.

#### What the daemon knows about pages

- The plugin gives each control socket a **page id** (`p<n>-<8 hex>`),
  tells the page in `hello` (`"page"`), and stamps it on every
  `web.call` (`{op, args, page}`). The browser can't choose it.
- The plugin reports the open pages with **`web.pages {pages: [ids]}`**
  on every control-socket open and close (the whole list each time, so a
  lost call heals on the next). Plugin-only, like `web.hello`: never a
  browser op.
- The daemon keeps `web_ui: Mutex<WebUi>`: per page `{route, view,
  visible, form, last_input}`. A page missing from `web.pages` is
  dropped; all are dropped when the `web.subscribe` stream ends (the
  plugin is gone). At most 16 (the plugin's control-socket limit).
- **Op `page.report {route, view, visible, form, input}`** (answered in
  `web.rs`, no daemon method): what that page shows. `route` is `{kind:
  "list" | "overlord" | "session", id?}` (kind checked, id clipped);
  `view` goes through `ui::normalize_view` (the TUI's view shape, so both
  surfaces report the same object); `form` is the open form's kind or
  null. It does not call `Daemon::changed()`.
- **`ui.state` (`get_ui_state`)** gains `target` (`terminal`, `web`,
  `none`) and `web`: `{open, pages, showing: {kind, id, name}, visible,
  form}` for the most recent page (null when no page is open).
  `current_view` becomes the target surface's view. `terminal`,
  `tui_running`, `overlord_overlay_open` and `default_view` stay as they
  are.

#### How `ui` commands reach the page

Over the stream the plugin already holds: a new line on `web.subscribe`.

- `Daemon::ui_cmd` becomes `ui_cmd_to(target, cmd)`: the line is
  `{"notify":"ui","to":"terminal","cmd":…}` or `{"notify":"ui",
  "to":"web","page":"<id>","cmd":…}`. The TUI ignores `to: "web"` lines (one
  condition where it parses `notify: ui`, `tui/src/lib.rs`); a line with
  no `to` means the terminal, so nothing else changes for it.
- `push_loop`, `Push::Web`: `ui` lines addressed to the web are written
  through as they come, in order, never folded into a `changed`. The
  drain after a `changed` must keep them (collect, then write), not
  discard them as it does now. A lagged receiver may still lose one; the
  parked copy below covers the common case.
- **Parked per surface.** `pending_ui` becomes one slot for the terminal
  and one for the web (30 s each). A command for the web with no page
  open is parked; op `page.pending` hands it to the next page that says
  hello. The terminal's `ui.pending` never takes the web's, and the
  other way round.
- Plugin: `daemon.rs`'s `Subscription::next` yields `Changed(rev)` or
  `Ui {page, cmd}`; `http.rs` sends `{"event":"ui","cmd":…}` only to the
  control socket with that page id.
- `plugin::WEB_PROTOCOL` goes up by one (`web.call` gains `page`,
  `web.pages` is new, the stream carries `ui`).
- Relay later: `ui` commands and `page.report` are mailbox messages, the
  same shapes.

Commands the page handles (typed in `proto.ts` as `UiCmd`):

| Command | From | The page |
| --- | --- | --- |
| `goto {id, overlord}` | `goto_session`, `go: true` | routes to `#/s/<id>` or `#/overlord` |
| `show_list` | `show_list`, `set_list_view` with `show` | routes to `#/` and closes an untouched form |
| `set_view {view, select}` | `set_list_view` | applies the view to its store, highlights `select` |
| `new_session {prefill}` | `propose_session` | opens the manual form pre-filled |
| `confirm_new_session {prefill}` | `confirm_new_session` | reopens the describe form with the overlord's hints, note and labels |
| `setup {prefill, status}` | `confirm_setup` | opens the code-folder form |
| `notice {text}` | the other surface was targeted | a toast |

As in the TUI, a form command never replaces a form the user has typed
into: it is kept, and a banner offers to open it.

#### The tools, retargeted (`ui.rs`, `setup.rs`)

- `show_list`: terminal as today; web sends `show_list`.
- `goto`: `ensure_running` first (unchanged), then terminal as today, or
  `goto` to the page. Answer: `switched_to` plus `on`.
- `set_view`: starts from the target's current view (the TUI's
  `ui_view`, or the page's reported view), applies the changes, sends
  `set_view` to the target, and shows the list there unless `show:
  false`. `save_as_default` and the sticky usage measure are daemon
  state, shared by both surfaces.
- `propose_session`, `confirm_new_session`, `setup::confirm`: build the
  prefill as today, send the command to the target; on the terminal
  `show_list` follows as today, on the web the command itself opens the
  form.
- `keys_open`: terminal only.
- **`snapshot` takes the list's filter syntax.** Its `view` gains
  `query` (the text the TUI's filter takes); the daemon runs
  `SessionQuery::apply_search_text` on it, as `mcp::call` does for
  `list_sessions`, and passes `usage`. The page's filter box then means
  what the TUI's does, so `set_list_view`'s `status:needs tag:wip` works
  in the browser. `search` stays accepted.
- **Default view.** Op `view.default` (answered in `web.rs` by
  `ui::default_view`): a page with no saved view of its own starts from
  the daemon's default; after that its view is the browser's own, as in
  slice 1.

#### Ops added to the allow-list, and what each may pass

`ui.*` and `setup.*` stay forbidden as pass-through families. The ops
below that need them are answered by hand in `web.rs` (method `""`, as
`session.resume` is), calling the Rust function with arguments picked one
by one; no op name starts with a forbidden family either. Each gets a
test in `web_api.rs`.

| Op | Reaches | Arguments passed on | Never from a browser |
| --- | --- | --- | --- |
| `page.report` | `web.rs` | `route`, `view`, `visible`, `form`, `input` | a time, a page id |
| `page.pending` | `web.rs` (the web's parked command) | none | |
| `view.default` | `ui::default_view` | none | |
| `form.defaults` | `session.defaults` | none | `no_listing` |
| `form.repos` | `repo.find` | none | |
| `form.setup` | `setup::status` + `setup::suggest` | none | `launch_dir` |
| `form.setup_save` | `setup::apply` | `code_dir`, `roots`, `create` | `skip` |
| `overlord.new_session` | `ask::new_session` | `description`, `repo`, `project`, `agent`, `model`, `workspace`, `base`, `prefer`, `tags`, `chosen`, `resolved`, `confirmed` | `request_id` |
| `overlord.setup_request` | `ask::setup_request` | `description`, `code_dir`, `code_dir_source`, `roots`, `roots_source` | |
| `session.new` | `session.new` | `repo`, `project`, `name`, `branch`, `agent`, `model`, `workspace`, `prompt`, `base`, `prefer`, `proposal` | `extra_args`, `role`, `tmux_name`, `hooks_work`, `base_resolved`, `location_resolved`, `remote_control`, `context`, `related`, `by_overlord` |
| `session.fork` | `session.fork` | `ref`, `prompt`, `name`, `workspace` | `context`, `related`, `description`, `model`, `branch` |

- **The overlord's background stays on the daemon's side.**
  `propose_session` carries `context`, `related` and "launched by the
  overlord". The TUI sends those back itself; a browser must not be able
  to write text the session is told is the overlord's. So
  `propose_session` keeps each proposal in memory under a short id (the
  last 8), the prefill carries the id, and the web's `session.new` passes
  `proposal`: `web.rs` fills `context`, `related` and `by_overlord` from
  the stored proposal. The form shows them read-only, as the TUI does.
- `overlord.new_session` with `project` (a plain folder): `web.rs` turns
  it into the form's wire shape (`repo: "<path> (plain folder, not a
  repo)"`, `workspace: false`), so that quirk lives in one place in Rust.
- Both launch ops go through `Daemon::call`, so the agent warm-up, the
  Codex-server start and the hooks check run as for the TUI.
- `form.setup_save` writes `config.toml` and may create the folder: the
  first browser op that writes config. It is what the user confirms in
  the form, limited to those three arguments. `SECURITY.md` should say
  so, and that the browser can now launch a session directly (it could
  already ask the overlord to).

#### The page (`crates/web/ui/src/`)

- `ui.ts`: takes a `UiCmd` and the current state (route, store, open
  form) and returns what to do. Pure, Vitest-tested. Also the
  `page.report` sender (on hello, route, view, visibility, form open or
  close, and throttled input) and `page.pending` on hello.
- `components/Modal.tsx`, `components/Picker.tsx` (a filtered list: repos
  with `scanning` and `skipped`; agents; an agent's models),
  `components/Toast.tsx`.
- `forms/hints.ts`: pure: which hints the user picked (`chosen`), which
  the overlord set (`resolved`), the request the form sends, and the
  prefill applied from `confirm_new_session` and `propose_session`.
  Mirrors `newform.rs` `submit_request`, `prefill_confirm` and
  `prefill`; tested against fixtures the daemon writes.
- `forms/NewSession.tsx`, two kinds as in the TUI:
  - **Describe** (`n`): a description box first, then the hints (repo,
    agent, model, worktree, base, tags), each shown with its default and
    a label (yours, the overlord's, default), plainly optional. Send:
    `overlord.new_session`, then route to `#/overlord` so the user sees
    any question it asks. Reopened by `confirm_new_session`: the note on
    top, changed hints marked, Send sets `confirmed: true`. No overlord:
    the error names the manual form.
  - **Manual** (`N`): repo first, name, agent, model, worktree, branch,
    base, prompt; the overlord's background and related sessions
    read-only when it proposed the session. Launch: `session.new`, then
    route to the new session. A failure keeps the form and its text.
  - An agent that isn't installed blocks Send with the reason, as
    `agent_blocker` does.
- `forms/Fork.tsx` (`f`): what the fork should do first, its name, a new
  worktree when the agent and repo allow it (the same test as
  `ForkForm::for_session`). `session.fork`, then route to the fork;
  warnings shown.
- `forms/Setup.tsx`: an optional description for the overlord, the
  folder (rlord's suggestion and its alternatives), what the finder
  scans. With a description and not yet confirmed: `overlord.setup_request`;
  otherwise `form.setup_save`, asking first before creating a folder or
  using the home folder (`save_call`'s rules, in `forms/setup.ts`, pure
  and tested).
- Entry points: "New session" (describe) and "Manual" in the list's
  header; Fork among a session's actions; "Code folder" in the header's
  menu. Keys `n`, `N`, `f` work when focus is on the list (never in a
  field or a pane).
- `List.tsx`: the filter box sends `query`; a usage figure on each row
  (the measure in `usage_metric`).
- Nothing in the page says "tmux" or "terminal" for its own actions.
- No overlord set up yet: unchanged, the page says to run `rlord` in a
  terminal. The first-run sequence stays in the TUI.

#### The overlord's instructions and tool descriptions

CLAUDE.md requires both in the same change. Agent-neutral, and shorter
where possible: `overlord.md` is loaded every turn.

`overlord.md`:

- Intro: "they drive the real rlord UI the user is looking at" stays;
  add one short paragraph, "The user's screen": it is a terminal or the
  web UI in a browser; rlord sends what you show to whichever they used
  last, and each answer's `on` says which. In the web UI you are a pane
  beside the list, with no overlay to hide. The key screen exists only
  in a terminal.
- "The user summons you with Alt-O" gains "(in the web UI: the overlord
  row)".
- `get_ui_state`: "which session their terminal shows" becomes "which
  screen they're on (`target`) and what it shows".
- `goto_session`, `show_list`, `set_list_view`: "the user's terminal" /
  "hide yourself" become "the user's screen"; keep the Alt-O hint as the
  terminal case.
- `propose_session`, `confirm_new_session`, `confirm_setup`: no change
  in when to use them; "on their screen" already fits.
- `open_keys`: add "terminal only".
- Nothing about tmux, page ids or routes; no parameter details.

`mcp.rs` descriptions: `get_ui_state` (names `target` and `web`),
`goto_session`, `show_list`, `set_list_view`, `open_keys`,
`confirm_setup`, `propose_session`, `confirm_new_session`, and `go` on
`create_session` and `fork_session`: "the user's terminal" becomes "the
user's screen (their terminal, or the web UI if they used it last)";
"(hides your overlay)" becomes "(in a terminal, hides your overlay)".
`READ_ONLY_TOOLS` and `MUTATING` are unchanged. `tidy.md` and
`import.md` use none of these tools.

#### Tests

Scratch tmux sockets only (`rlord-test-web`, `rlord-test-web-api`), never
`tmux -L rlord` or the default server; no real agent, no model call. The
overlord is stood in for by raw `mcp.call` requests on the test daemon's
socket.

- **Unit, daemon:** `pick` (one surface, both, tie, none; several
  pages); `normalize_view` on a page's report; the proposal store (kept,
  bounded, unknown id refused).
- **In-memory daemon, no tmux** (`web_api.rs`; with no tmux server there
  is no terminal, so a registered page is the target):
  - `web.pages` and `page.report` register and drop pages; pages are
    dropped when the subscription ends; a `page.report` can't set a
    time or another page's state.
  - `set_list_view`, `show_list`, `propose_session`,
    `confirm_new_session` and `confirm_setup` through `mcp.call` each
    put the right `ui` line, addressed to the page, on a `web.subscribe`
    stream, and answer `on: "web"`; with no page and no terminal they
    fail naming both ways in.
  - `ui` lines survive coalescing: a burst of changes with a command in
    the middle delivers the command exactly once.
  - Parking: a command with no page open comes back from `page.pending`
    once, within 30 s; the terminal's `ui.pending` doesn't take it.
  - `get_ui_state`: `target`, `web.showing`, `current_view` from the
    page.
  - Every new op: reaches its function with only the listed arguments;
    the "never" column is dropped (`session.new` with `extra_args`,
    `role`, `by_overlord`, `context`; `form.setup_save` with `skip`);
    `session.new` with a `proposal` gets the stored background.
    `setup::apply` writes to a temp config path, as its own tests do.
  - `snapshot` with `query: "tag:x status:turn"` filters as
    `list_sessions` does.
  - `the_pages_ops_are_the_daemons` keeps holding `proto.ts` `OPS` to
    `web::OPS`.
  - Fixtures written by a daemon test (`RLORD_UPDATE_FIXTURES=1`) and
    type-checked by `fixture.test.ts`: one of each `UiCmd`, and the
    answers of `form.defaults`, `form.repos`, `form.setup`.
- **Gated tmux** (`web_tmux.rs`, `RLORD_TMUX_TESTS=1`):
  - `client_activity` moves on input and not on output (step 1).
  - A terminal client on a PTY and a page both present: the later one is
    the target; `goto_session` to a session running `cat` switches the
    terminal client when it is the target and sends `goto` when the page
    is, leaving the terminal where it was.
  - `open_keys` with only a page fails with the terminal-only message.
- **TUI:** a `ui` line with `to: "web"` is ignored; `notice` sets the
  status line.
- **Plugin** (`crates/web/tests/http.rs`, in-memory daemon): `hello`
  carries a page id; `web.pages` follows opens and closes; a `ui` event
  reaches only the addressed socket; `page` can't be set from the
  browser's request.
- **Page** (Vitest): `ui.ts` for each command, including a parked form
  command; `forms/hints.ts` and `forms/setup.ts` against the fixtures
  (chosen and resolved labels, the confirm round trip, a plain folder,
  create and home prompts); the view set from a command; `page.report`
  throttling.
- **`overlord.rs`'s test** that every tool is mentioned in `overlord.md`
  still passes (no tool added or removed).
- **By hand and headless** against `demo.sh --web`, the overlord stood in
  for by `mcp.call` from a script: each tool with only a browser, only a
  terminal, and both (typing in one, then the other); the confirm loop
  end to end (describe, reopened form, confirmed send); manual launch,
  fork and code folder; two tabs; phone width.

### Implementation steps

For an Opus implementer at high effort. One commit each; the PR bumps
the version to the next `-dev.N` and regenerates `Cargo.lock`. Record
each step in "Progress" as slice 1 did.

1. **Prove the activity signal** (`web_tmux.rs`, gated): on a scratch
   socket, `client_activity` moves when a client types and not when its
   pane prints; note its resolution (seconds). If output moves it, stop
   and use the fallback under "Risks" before going on.
2. **Surfaces and the target** (`daemon/src/ui.rs`, `web.rs`, `lib.rs`):
   `WebUi` on `Daemon`, `web.pages`, `page` on `web.call`, op
   `page.report`, `ui::target` and `pick`, `ui.state`'s `target` and
   `web`. Tests.
3. **Addressed commands** (`lib.rs`, `ui.rs`, `tui/src/lib.rs`,
   `tui/src/overlord.rs`): `ui_cmd_to`, the two parked slots, op
   `page.pending`, `push_loop` writing `ui` lines to the web stream
   without loss, the TUI ignoring `to: "web"`, `notice` on both sides,
   `WEB_PROTOCOL` + 1. Tests.
4. **Retarget the tools** (`ui.rs`, `setup.rs`): `show_list`, `goto`,
   `set_view`, `propose_session` (with the proposal store),
   `confirm_new_session`, `setup::confirm`, `keys_open`; `on` in each
   answer; `snapshot`'s `query` and `usage`; op `view.default`. Tests.
5. **Form ops** (`web.rs`): the table above, each with its test, and the
   fixtures.
6. **Plugin** (`crates/web/src/daemon.rs`, `http.rs`): page ids, `hello`
   with `page`, `web.pages`, `page` on calls, `ui` events to the
   addressed socket. Tests.
7. **Page: commands** (`proto.ts`, `ui.ts`, `store.ts`, `main.tsx`,
   `List.tsx`, `Toast.tsx`): `UiCmd` types and fixtures, `goto`,
   `show_list`, `set_view`, `notice`, `page.report`, `page.pending`, the
   filter syntax, the default view, the usage figure.
8. **Page: new-session forms** (`Modal.tsx`, `Picker.tsx`,
   `forms/hints.ts`, `forms/NewSession.tsx`): describe and manual, the
   reopened confirm form, the proposed session.
9. **Page: fork and code-folder forms** (`forms/Fork.tsx`,
   `forms/setup.ts`, `forms/Setup.tsx`).
10. **The overlord's words** (`overlord.md`, `mcp.rs`), as listed above.
11. **Browser pass** on the demo, by hand and headless, with the
    scripted `mcp.call` stand-in; fix what it finds.
12. **Docs**: `docs/architecture.md` "Web UI" (surfaces and the target
    rule, addressed `ui` lines, the hand-answered ops, the proposal
    store), `docs/guide.md`, `SECURITY.md`, the browser-op rule in
    `CLAUDE.md` if its wording no longer holds; "Owed from slice 1"
    carried over.

Steps 2 to 5 are daemon-only and testable without the plugin or the
page; 6 needs 3; 7 needs 6; 8 and 9 need 5 and 7.

### Risks

- **The activity signal.** The rule rests on tmux's `client_activity`
  meaning input. If the TUI's own redraws move it, the terminal would
  always look newest. Step 1 checks before anything is built on it.
  Fallback: the TUI reports its key presses (a throttled call beside
  `ui.view.report`), and a terminal showing a session directly counts
  through `client_activity` only when no TUI is subscribed.
- **Wrong target with both open.** Seconds-level resolution and a user
  moving between screens while the overlord thinks can send a form to
  the screen they just left. Bounded by the notice on the other surface,
  `on` in the answer, and the form staying open until used.
- **Form logic in two languages.** `newform.rs` and `setupform.rs` hold
  rules (what counts as chosen, the base default, create and home
  prompts) that `forms/*.ts` repeats. The labels on a reopened form are
  already worked out in Rust (`apply_provenance`); the rest is held by
  shared fixtures. Drift shows up as a wrong label, not a wrong launch:
  the daemon checks every launch argument again.
- **A wider browser surface.** The page can now launch a session with
  any prompt in any folder, and write the code folder to `config.toml`.
  Still behind Host, Origin, the token and the allow-list, and no more
  than `overlord.ask` could already cause, but direct. Argument
  filtering and its tests are the guard.
- **A lost command.** A lagged stream or a page reconnecting at the
  wrong moment drops a `ui` line; only the last one is parked. The
  overlord's answer still says a form was opened. Same weakness as the
  TUI's today.
- **`ui_view` is one slot for every TUI.** Slice 2 keeps that and adds
  per-page views beside it; two TUIs still overwrite each other, as now.
- **Slow ops.** `overlord.new_session` waits up to 90 s for an overlord
  to start and `session.new` may fetch its base; the plugin's 120 s call
  limit covers both, and the forms must show "sending" rather than
  freeze.
- **`overlord.md` grows.** The added paragraph must be paid for by
  trimming the lines it makes redundant.

### Open questions for the user

1. **Target rule.** Planned: the surface you used last, one target per
   command, a one-line notice on the other. Alternatives: always both
   (rejected above for double sends), or a fixed preference in config
   (`[web] ui_target = "auto" | "terminal" | "web"`). Is "used last"
   right, and do you want the config override as well?
2. **Key screen.** Planned: terminal only; `open_keys` says so when only
   a browser is open. Agreed, or should the page at least show the
   current keys read-only?
3. **Default view.** Planned: one default view in the daemon
   (`save_as_default`), which a browser adopts only when it has no saved
   view; after that each browser keeps its own. Or should the browser
   always open on the daemon's default, as the TUI does?
4. **First run in the browser.** Planned: out of this slice. With no
   overlord set up, the page still says to run `rlord` in a terminal;
   only the code-folder form comes to the web. Should the overlord's own
   setup dialog (agent, model) come too?
5. **The notice on the other screen.** Planned: yes, for form and
   navigation commands. Too noisy for `set_list_view` and `show_list`?
6. **Launching straight from the browser.** The manual form and fork
   call `session.new` and `session.fork` directly, with filtered
   arguments. Fine for the local page; for the relayed phone later,
   should direct launches be off by default (describe-to-the-overlord
   only)?
7. **Filter syntax.** The browser's filter box takes the TUI's syntax
   (`tag:`, `status:`, `agent:`) from this slice on, so the overlord's
   `set_list_view` means the same on both. Any objection to changing the
   box's meaning from plain free text?

### Slice 2 answers (user, 2026-11-17)

1. UI target: the surface used last, plus a config override
   `[web] ui_target = auto | terminal | web`.
2. Key screen: shown read-only in the page; editing stays in the terminal.
3. The overlord's first-run dialog stays terminal-only this slice.
4. Direct launches (`session.new`, `session.fork`): the relayed phone
   gets the same as local (not off by default).
5. Defaults taken, not asked: a browser adopts the daemon's default view
   only when it has no saved one; the one-line notice on the other surface
   stays; the filter syntax change already landed (`1a2d5b6`).

## Slice 3 plan: e2e core and the relay

Planned 2026-11-17 (Fable planning agent), on `feat/web-ui-3` from
`feature/web`. Nothing below is built. Goal: a paired device reaches the
overlord and the session list of a host behind NAT, through a relay that
sees only ciphertext, with both network invariants at the top of this doc
holding and tested. The remote client in this slice is a Rust test client;
the phone app is the next slice. No remote browser client. No live panes
yet (the stream ids leave room). No network code in `rlord`.

The user's decisions this plan takes as fixed: `e2e-crypto.md`'s table,
answers and "Decision after the spike"; the scope given for this slice
(sans-IO `crates/e2e` from the spike, a separate `rlord-relay`,
`rlord-web` dialling out to one pinned relay, per-device keys by QR with
host confirmation, chat and state first). **Compression stays provisional
and is not built**: nothing below compresses anything, no frame has a
compression flag, and the relay link refuses transport compression.

Crate versions below were read from crates.io on 2026-11-17: `snow`
0.10.0, `libcrux-ml-kem` 0.0.11, `wasm-bindgen` 0.2.130, `rustls` 0.23.46,
`tokio-rustls` 0.26.6, `tokio-tungstenite` 0.30.0 (the plugin's lockfile
has 0.29.0 as a dev-dependency), `axum` 0.8.9, `webpki-roots` 1.0.12,
`p256` 0.14.0, `rcgen` 0.14.10, `qrcode` 0.14.1, `rustls-acme` 0.15.4,
`uniffi` 0.32.2.

What exists today (read from the code, for the implementer):

- The spike (`spikes/e2e/` on its own branch, never merged; read-only at
  `.claude/worktrees/agent-affb15205011d80a4/spikes/e2e/`): `core/src/`
  `lib.rs` (`StaticKey`, `RelayId`, `QrPayload`, `channel_id`,
  `short_code`, `Error`), `handshake.rs` (`Initiator`, `Responder`,
  `Established`, `prologue`, `pair_psk`, `kk_psk`, `FixedRandom`),
  `kem.rs`, `session.rs` (`Mode`, `Session`, `padme_len`,
  `live_padded_len`), `vectors.rs`; `wasm/src/lib.rs` (`E2eSession`,
  `DeviceHandshake`, `HostAnswer`); `peer/` and `relay/` (a forwarder that
  only announces its identity); `xtask/` (`vectors`, `wasm`, `sizes`). It
  is edition 2024 in its own workspace; the main workspace is 2021.
- `snow` 0.10.0 has `HandshakeState::dangerously_get_raw_split`
  (`handshakestate.rs`), and no way to save or restore a `TransportState`.
- `crates/web/src/daemon.rs`: the plugin calls the daemon
  (`Daemon::call`), and the daemon reaches the plugin only by lines on
  the `web.subscribe` stream (`Subscription::next` yields `Changed` or
  `Ui`).
- `crates/daemon/src/web.rs`: `OPS` (`Op {op, method, args}`), `PASSIVE`,
  `FORBIDDEN`, `call`, `filtered`; `web.call` takes `{op, args, page}`.
  `plugin::WEB_PROTOCOL` is 2.
- `xtask/src/check_deps.rs`: `DENIED`, `denied_in_tree`, `MAIN_CRATES`.
  `rlord`'s tree already has `sha2`, `getrandom` and `rand`; it has no
  `ring`, `p256` or `base64`.
- `xtask/src/version.rs`: `Version::web_tarball`, `tarballs`,
  `workflow_assets`, `dev_assets`; `LINUX` and `MAC` targets.

### Decisions

#### Crates

| Crate | Package, output | Depends on | Notes |
| --- | --- | --- | --- |
| `crates/e2e` | `robot-overlord-e2e`, lib `rlord_e2e` | `snow`, `libcrux-ml-kem`, `chacha20poly1305`, `sha2`, `curve25519-dalek`, `getrandom`, `zeroize`, `base64` (the spike's exact pins) | Sans-IO: no tokio, sockets, files or clock. `forbid(unsafe_code)`. Builds native and `wasm32-unknown-unknown`. |
| `crates/e2e-wasm` | `robot-overlord-e2e-wasm`, `cdylib` | `rlord_e2e`, `wasm-bindgen` =0.2.130, `getrandom` with `wasm_js` | The spike's wrapper minus the benchmark-only calls (`encrypt_copy`, `LoopbackPair`). WASM only. |
| `crates/relay-proto` | `robot-overlord-relay-proto`, lib `rlord_relay_proto` | `p256` (ECDSA), `sha2`, `serde`, `serde_json`, `base64` | The relay link's messages, frame headers, the two signed transcripts, the relay address. No Noise, no KEM. Sans-IO; builds to WASM too. |
| `crates/relay` | `robot-overlord-relay`, bin `rlord-relay` | `rlord_relay_proto`, `axum` (ws), `tokio`, `tokio-rustls`, `rustls` (ring provider), `rusqlite`, `toml`, `serde`, `anyhow`, `tracing` | **Not** `rlord_e2e`, not `rlord-core`, not `rlord-daemon`. |
| `crates/web` (existing) | `rlord-web` | adds `rlord_e2e`, `rlord_relay_proto`, `tokio-tungstenite` (rustls, ring, `webpki-roots`) | New module tree `src/remote/`. |
| `crates/e2e-device` | `robot-overlord-e2e-device`, lib + bin `rlord-device` | `rlord_e2e`, `rlord_relay_proto`, `tokio-tungstenite` | The test client: a library the integration tests drive, and a small CLI for pairing by hand. Never released or packaged. |

- All `publish = false`, in `members`, not in `default-members`, not in
  `version::CRATES`.
- `crates/e2e-ffi` (UniFFI) is not created. **UniFFI-ready** means the
  public surface of `rlord_e2e` keeps to what UniFFI's macros can export
  later without a rewrite: owned arguments and returns (`Vec<u8>`,
  `String`, fixed arrays wrapped in structs), no lifetimes or generics in
  public signatures, constructors returning `Result`, one flat `Error`
  enum, state machines as owned objects consumed or mutated by methods.
  A doc comment at the top of `lib.rs` states the rule; the WASM wrapper
  is the first consumer held to it.
- `crates/e2e/src/`: `lib.rs` (constants, `Error`, `StaticKey`,
  `DeviceId`), `qr.rs` (`QrPayload`), `handshake.rs`, `kem.rs` (libcrux
  only; the RustCrypto backend and the classical baseline are dropped,
  the `pq-*` features with them), `session.rs` (live `Session`),
  `mailbox.rs` (new, below), `stream.rs` (new: stream ids and
  fragmenting), `vectors.rs` behind the `vectors` feature. Ported to
  edition 2021.
- **The relay needs no crypto core** is kept as a checked property: the
  P-256 signing it does need lives in `relay-proto`, not in `rlord_e2e`
  (this replaces `e2e-crypto.md`'s "relay-identity signing behind a
  feature" of `crates/e2e`).
- `rust-toolchain.toml` gains the `wasm32-unknown-unknown` target.

#### The e2e core beyond the spike

- **Handshake kinds** in the prologue: 1 pairing (`IKpsk2`), 2 live
  reconnect (`KKpsk2`), 3 mailbox keying (`KKpsk2`, new). The prologue
  stays `label | VERSION | kind | origin | relay key fingerprint | id`,
  with the relay identity **this connection proved** (below), and the id
  the rendezvous id (pairing) or `channel_id` (the others).
- **A session exists only after the device's first transport frame
  decrypts**, for all three kinds (the spike's rule for pairing). A
  replayed message 1 gets a message 2 and nothing else.
- **Any decrypt failure ends the session** (`Session` poisons itself and
  every later call returns `Error::Decrypt`); the owner closes the link.
- **Streams** (`stream.rs`): the first plaintext byte of a live frame is
  a stream id. `0` is control; `1..` are reserved for panes and unused.
  The second byte is a flags byte whose bit 0 means "more follows", so a
  control message larger than one 16 KiB frame (a snapshot) is sent as
  fragments and reassembled (`Reassembler`, 1 MiB limit, the local
  control socket's). Other flag bits must be zero and are rejected.
- **Mailbox** (`mailbox.rs`, `MailboxSession`): keyed by a kind-3
  handshake run over a live link; the two raw keys come from
  `dangerously_get_raw_split`, and the mailbox runs its own cipher state
  over them (`chacha20poly1305`, the version `snow` resolves; counter
  nonce; Noise's `REKEY` after every message; Padmé padding). That is
  what makes it saveable: `export() -> Vec<u8>` and `import(&[u8])`
  (version byte, both keys, both counters), which the owner stores.
  Vectors prove its frames equal `snow`'s own transport frames for the
  same keys, rekeys included. A decrypt failure or a gap discards the
  state on both ends; the next live link keys a new one.
  - Advance-then-save order is fixed: the sender saves the advanced
    state before handing the frame out; the receiver saves after it has
    acted. A crash between loses at most that message, which the sender
    resends by id.
- **Vectors**: `crates/e2e/vectors/v1.json`, written by `cargo xtask e2e
  vectors` and checked by `--check` (a test fails when the committed file
  differs). Added to the spike's set: the kind-3 handshake, a mailbox
  sequence with rekeys, export and import mid-sequence, fragmenting, and
  the new failures (wrong kind, a poisoned session, a mailbox gap). The
  two 16 KiB frames go (the spike's file was 232 KB); one 16 KiB frame's
  hash stays.

#### WASM build

`cargo xtask e2e wasm` (`xtask/src/e2e.rs`): `cargo build --release
--target wasm32-unknown-unknown -p robot-overlord-e2e-wasm` with
`--cfg getrandom_backend="wasm_js"` and `-C target-feature=+simd128` at
O3, then bindgen in process (`wasm-bindgen-cli-support` =0.2.130, `--target
web`), into `crates/e2e-wasm/pkg/` (not committed). No `wasm-pack`, no
`wasm-opt`.

Nothing loads it in the page in this slice (no remote browser client; the
local page needs no E2E). It is kept alive by tests: `cargo xtask e2e
test-wasm` runs Vitest (`crates/web/ui/test/e2e/`, a separate Vitest
project so `web check` doesn't need the WASM) over the vectors in Node and
one interop run against the native test client. The worker, Trusted Types
policy and CSP change of `e2e-crypto.md` belong to the slice that ships a
WASM client (the phone's WebView).

#### The relay link (relay ⇄ host, relay ⇄ device)

One websocket per peer, `wss://<host>/v1`, opened by the peer. Control
messages are JSON text frames (at most 4 KiB); data is binary frames (at
most 20 KiB). All of it is defined in `relay-proto` (`msg.rs`, `frame.rs`,
`auth.rs`, `address.rs`) and used by all three parties.

**Relay identity.** A P-256 key made on the relay's first start
(`<data dir>/relay.key`, 0600). Its fingerprint is SHA-256 of the SEC1
uncompressed public key (32 bytes, the QR's field). The **relay address**
is one string, `https://relay.example.com#k=<base64url fingerprint>`,
printed by `rlord-relay address` and pasted into `rlord remote setup`: the
pin travels with the origin. The origin is normalised (lower-case host,
no path, no default port) and at most 255 bytes.

**Connecting** (5 s limit from upgrade to `ready`):

1. Peer: `{"t":"hello","v":1,"role":"host"|"device"|"pair","key":<P-256
   public>,"nonce":<32 bytes>,"host"?,"rendezvous"?,"enroll"?}`.
   A device names its host's fingerprint; a pairing device names the
   rendezvous id; a host enrolling for the first time gives `enroll`,
   an object (`{"kind":"token","token"}`; other kinds are reserved for
   account login, "Production relay" below).
2. Relay: `{"t":"challenge","origin","key","nonce","sig"}`. `sig` is
   ECDSA P-256 over `"rlord-relay/1 relay" | origin | relay key | peer
   key | peer nonce | relay nonce`.
3. The peer checks, in this order and before sending anything else: the
   origin is byte-for-byte its pin; the key's fingerprint is its pin; the
   signature verifies. Any miss closes the socket and is reported as
   "wrong relay" (never retried against anything else). Then:
   `{"t":"auth","sig"}` over `"rlord-relay/1 peer" | role | origin |
   relay key | peer key | relay nonce | peer nonce | host or rendezvous`.
4. Relay: `{"t":"ready","boot","host":"online"|"offline","signal":n,
   "queued":n}` (the last three for a device), or a close code: 4401 bad
   signature, 4403 not registered or not on the host's list, 4408 too
   slow, 4409 replaced by a newer connection of the same identity, 4426
   unsupported `v`, 4429 over a limit, 4402 suspended, 4503 overloaded
   (the last two, `bye` and `enroll`: "Production relay" below).

- The proved origin and key are what go into every Noise prologue.
- **No channel binding to TLS** in v1 (the signed transcript does not
  include a TLS exporter), so that a self-hoster can put a proxy in
  front. A middlebox that passes TLS validation for the pinned origin and
  forwards the challenge faithfully learns what the relay learns
  (metadata) and nothing more; it cannot become a different relay. Open
  question 9.
- **No redirects, no fallbacks**: a 3xx on the upgrade is a failed
  connection; the client has exactly one URL, built from the pin.
  `permessage-deflate` is never offered or accepted.
- **TLS**: the certificate must validate for the origin's host against
  `webpki-roots`, plus the PEM in `[remote] ca_file` when set (a private
  CA on premises). Plain `http://127.0.0.1:<port>` origins are allowed,
  loopback only, so the tests run the real binaries.

**After `ready`**, control messages carry a number `n` and are answered
`{"t":"ok","n"}` or `{"t":"err","n","code"}`:

| From | Message | Meaning |
| --- | --- | --- |
| host | `devices {allow: [fingerprints]}` | The whole allow-list, on every connect and every change (heals drift, as `web.pages` does). A device not on it is refused and its queue dropped. |
| host | `rendezvous {id, ttl}` / `rendezvous_close {id}` | Open a pairing rendezvous (at most 300 s, 4 at once). |
| host | `signal {device}` | "Something for you": sets that device's flag. No content. |
| host | `drop {link}` | Close a device's link (decrypt failure, revocation). |
| relay → host | `link {id, device, kind}` / `unlink {id}` | A device (or a pairing device, with its rendezvous id) connected or went. |
| device | `ack_signal` | Clears its flag after it has re-synced. |
| either | `ack {seq, device?}` | Deletes queued mailbox messages up to `seq`. |
| relay → device | `host {state}` | Its host came or went. |

Binary frames: device side `lane | payload`; host side `link (u32) | lane
| payload`. Lane 1 is live: forwarded at once to the other end of the
link, dropped with `{"t":"undeliverable"}` when there is none. Lane 2 is
the mailbox: host frames carry the device's fingerprint instead of a link
(the device may be away), and frames from the relay carry a sequence
number. The payload is `rlord_e2e`'s bytes, untouched and unparsed.

**"Anything for me?"** is the `ready` message: a device that connects,
authenticates and reads `signal: 0`, `queued: 0` and an unchanged `boot`
has nothing to fetch and has run no Noise handshake. A new `boot` (the
relay restarted and forgot its flags) counts as "yes". The host signals a
device when the daemon's `rev` moves while that device has no live link,
at most once per 30 s per device. The phone's platform push hangs off
this same flag in the next slice.

#### What the relay keeps, and its limits

- **On disk**, all through the `Store` trait ("Production relay" below;
  slice 3's one backend is `<data dir>/relay.db`, SQLite, 0600,
  `rusqlite` bundled, which this repo already cross-builds static):
  hosts (fingerprint, account, status, first seen, last seen), devices
  (host, fingerprint), enrolment tokens (hashed, with their account).
  And `relay.key`. Nothing else.
- **In memory only**: connections, links, rendezvous, signal flags,
  mailbox queues. A restart forgets them; senders resend by id. No
  ciphertext is ever written to disk.
- **Never**: content keys, names, plaintext, logs of payloads. The log
  has connection events with fingerprints shortened to 8 hex characters.
- **Registration** (`[hosts] registration`): `"token"` (default: a host's
  first connection must carry a token from `rlord-relay token new`),
  `"open"` (any host key, up to `max_hosts`), `"closed"` (only
  `rlord-relay host add <fingerprint>`). A relay refuses a host it has
  not registered (the spec's relay-side enforcement).
- A host makes its relay-identity key for one relay and never reuses it
  (below), so "registered with two relays" cannot happen by accident.
- **Defaults**, all in `relay.toml` under `[limits]`:

  | Limit | Default |
  | --- | --- |
  | Hosts (`max_hosts`) | 1000 |
  | Devices per host | 16 |
  | Connections per identity | 1 (a newer one replaces the older, 4409) |
  | Unauthenticated connections per address | 8 |
  | Bytes per connection | 256 KiB/s, burst 1 MiB (live panes will raise it) |
  | Mailbox queue, per device and direction | 32 messages, 128 KiB, 600 s; `mailbox_ttl = 0` turns queues off |
  | Rendezvous | 300 s, 4 per host, 3 failed attempts closes it |
  | Idle | ping every 25 s, closed after 60 s of silence |
  | Unused host (`host_expiry_days`) | 90 (0 = never) |

  A full queue refuses the new message (`err`, code `full`); it never
  drops an older one, since the mailbox cipher needs order.
- **Push-only holds for the relay too**: it listens and never dials a
  host or a device. In this slice it makes no outbound connection at all.

#### `rlord-relay`: commands and config

`rlord-relay serve [--config PATH]`, `address`, `token new [--uses N]
[--ttl DURATION]`, `host list|add|remove`, `check` (loads the config,
opens the store, proves the key signs and verifies, exits), `--version`.

```toml
origin = "https://relay.example.com"   # what peers pin; must be how they reach it
listen = "0.0.0.0:443"
data_dir = "/var/lib/rlord-relay"

[tls]
mode = "files"          # "files" | "proxy"
cert = "/etc/rlord-relay/fullchain.pem"
key = "/etc/rlord-relay/privkey.pem"

[hosts]
registration = "token"  # "token" | "open" | "closed"

[limits]                # the table above
```

- `mode = "files"`: the relay terminates TLS (`rustls`, ring provider,
  TLS 1.3 and 1.2); the files are re-read on SIGHUP and every 12 hours,
  so a renewal needs no restart.
- `mode = "proxy"`: plain HTTP for a reverse proxy that terminates TLS
  (Caddy in the shipped compose file). It binds loopback unless `listen`
  says otherwise; the client address for the per-address limit then comes
  from `X-Forwarded-For`.
- Built-in ACME and mTLS are not in this slice (open questions 2 and 5).

#### The host side: `rlord-web` dials out

`crates/web/src/remote/`: `mod.rs` (the task: start, backoff, state),
`keys.rs`, `link.rs` (the relay connection and its auth), `pairing.rs`,
`devices.rs`, `session.rs` (a device's live session and its ops),
`mailbox.rs`, `audit.rs`.

- **Off unless asked.** The plugin opens a relay connection only when
  `[remote] enabled = true`, a pin is set, and there is at least one
  paired device or an open pairing. A user of the local web UI who never
  runs `rlord remote setup` has a plugin that never dials out. The
  listener stays `127.0.0.1` only: nothing remote is ever accepted.
- **Keys** in `<data dir>/remote/` (0700; files 0600 through
  `rlord_core::private::write`): `host.key` (X25519 static), `relay.key`
  (the host's P-256 relay identity), `pin.json` (the origin and
  fingerprint these keys were made for), `devices.json`, and
  `mailbox-<device id>.bin`. The `rlord` binary and the daemon never read
  them.
- **The pin is part of the keys.** `rlord remote setup <relay address>`
  writes `[remote] relay` and `relay_key` and makes the keys. If the
  config's relay later differs from `pin.json`, the plugin refuses to
  connect and says so (`rlord remote status`). Moving to another relay
  is `rlord remote setup --reset <address>`: it deletes `remote/` (host
  keys and every device), so every device pairs again. There is no other
  way to change the relay.
- **Connection**: one socket, reconnect with the supervisor's backoff
  shape (1 s doubling to 60 s, never given up while enabled). A "wrong
  relay" result (origin, key or signature) is not retried until the
  config or the plugin restarts, and is shown in `rlord remote status`.
- **Audit** (`<data dir>/remote.log`, 0600, rotated at 1 MiB): one line
  per pairing, confirmation, revocation, link and remote op (time, device
  id, op name; never arguments).
- A device may send 20 ops per second (burst 40); over that the op fails
  with `rate_limited`.

#### Pairing and the host's confirmation

Pairing, confirming and revoking are user-only: never an MCP tool, never
reachable by a remote device.

1. **Start**: `rlord remote pair`, `:remote` in the list, or "Devices" in
   the local page. The plugin makes a rendezvous id and a pairing secret,
   opens the rendezvous at the relay (connecting first if this is the
   first device), and returns the QR string (`RLORD1:…`, the spike's
   payload). It is valid for 300 s and for one device.
2. **Show**: the terminal draws the QR from the string
   (`rlord_core::qr`, new, on the `qrcode` crate, half-block characters;
   not network code); the page draws the same matrix, which the daemon's
   answer carries (`qr_rows`). `rlord remote pair --text` prints the
   string for the test client.
3. **Handshake**: the device connects with role `pair`, runs `IKpsk2`
   with the ML-KEM step, and its first transport frame carries its name
   (clipped to 40 characters, control characters removed) and its relay
   public key. A wrong secret counts as a failed attempt; three close
   the rendezvous.
4. **Confirm**: the host shows the device's name and the six-digit code
   (`short_code`); the device shows the same code. The pairing link
   stays open while it waits; the host tells the device `waiting`. Until
   the user confirms, the device is not stored, not on the relay's
   allow-list and can call nothing.
   - CLI: `rlord remote pair` waits, prints name and code, asks `[y/N]`.
     `rlord remote confirm` / `reject` do the same for a pairing started
     elsewhere.
   - TUI: a pairing waiting for confirmation puts a notice on the status
     line of every surface; `:remote` opens the Devices screen
     (`crates/tui/src/remote.rs`), where the waiting device shows its
     name and code with confirm and reject keys.
   - Local page: "Devices" in the header (`ui/src/views/Devices.tsx`):
     the same list, QR, code and buttons. Local pages only.
5. **Done**: on confirm the plugin stores the device (`devices.json`: id,
   name, static public key, relay key fingerprint, paired at, last
   seen), sends the relay the new allow-list, tells the device `done`
   with the host's name, and destroys the secret and the rendezvous. On
   reject, on 120 s without an answer, or if the device's link drops
   first: `rejected`, nothing stored, start again.

The device id is the first 8 bytes (hex) of a labelled hash of its
static key; it names the device in the CLI, the log and `web.call`.

#### Revocation and expiry

- `rlord remote revoke <device>` (and the Devices screens): the plugin
  deletes the record and its mailbox file, drops its live link, and
  sends the relay the shorter allow-list. `KKpsk2` with a deleted key
  cannot complete; the relay refuses the device before that.
- `rlord remote devices` lists them (name, id, paired, last seen, linked
  now).
- A device unused for `[remote] device_expiry_days` (default 90, 0 =
  never) is revoked by the plugin at start and once a day.
- `rlord remote disable` sets `enabled = false` and closes the relay
  connection; devices stay paired. `rlord remote setup --reset` is the
  only thing that removes the host's keys.
- `rlord web remove` also deletes `remote/` after asking, since the
  plugin that could use it is gone.

#### How remote ops reach the daemon

A live session's control stream carries the local control socket's
messages unchanged: requests `{"id","op","args"}`, answers `{"id","ok"}`
or `{"id","error":{"code","message"}}`, events
`{"event":"hello","version","rev","device"}` and
`{"event":"changed","rev"}`. The plugin passes each request to the daemon
as `web.call {op, args, via: "remote", device: "<id>"}`.

- **One allow-list.** `web::Op` gains `remote: bool`. `web::call` reads
  `via` (`"local"` or `"remote"`; missing or anything else is refused)
  and refuses an op whose `remote` is false with `NotAllowed` when it
  came from a device. `"remote."` joins `FORBIDDEN`.
  `plugin::WEB_PROTOCOL` becomes 3.

  | Remote: yes | Remote: no (local pages only) |
  | --- | --- |
  | `snapshot`, `session.get`, `view.default`, `overlord.ensure`, `overlord.ask`, `overlord.switch`, `overlord.new_session`, `overlord.messages` (new), `session.resume`, `session.restart`, `session.stop`, `session.archive`, `session.unarchive`, `session.rename`, `session.describe`, `session.notes`, `session.tag`, `session.pin`, `session.new`, `session.fork`, `form.defaults`, `form.repos` | `page.report`, `page.pending`, `keys.list`, `form.setup`, `form.setup_save`, `overlord.setup_request`, and the new `device.*` ops |

  Direct launches are remote too (slice 2 answer 4). What writes
  `config.toml` or manages devices is not.
- **New op `overlord.messages {before, limit}`** (both): the active
  overlord's recent conversation as text, read by plain code from its
  transcript (`rlord_core::agents::transcript`, as `recall.rs` does), at
  most 50 messages, each clipped to 4 KiB. It is what a chat view needs
  beside `overlord.ask`; `proto.ts` `OPS` gains it with its test.
- **New local-only ops**, answered in `web.rs` by asking the plugin
  (below): `device.list`, `device.pair`, `device.pair_cancel`,
  `device.confirm`, `device.reject`, `device.revoke`. They are what the
  page's Devices view calls.
- **Not a surface yet.** A remote call carries no `page`, never counts
  as use of a screen, and gets no `ui` commands: `ui::target` is
  unchanged. A describe form sent from a device is answered on the
  host's own screen. Making the phone a third surface is the phone
  slice's.
- **Panes**: the plugin answers anything on streams 1 and up by closing
  the link. `web.pane_target` is never called for a device.
- **Mailbox**: only `overlord.ask` and `overlord.new_session` may travel
  by the mailbox, each in one frame, when the device finds its host
  offline or the live link down. The host answers by the mailbox with
  the usual answer, keeps the last 64 request ids per device and drops a
  repeat. Everything else needs a live link. Open question 1.

**User-only commands** (`crates/daemon/src/remote.rs`): `remote.status`,
`remote.setup`, `remote.pair`, `remote.pair_cancel`, `remote.confirm`,
`remote.reject`, `remote.devices`, `remote.revoke`, `remote.disable`,
called by the CLI and the TUI over the socket, never on `OPS`, never in
`mcp.rs`.

- The daemon holds no keys and does no crypto. It forwards each command
  to the plugin as a line on the `web.subscribe` stream,
  `{"notify":"remote","id","req"}` (never coalesced, like `ui` lines),
  and waits up to 10 s for the plugin's `web.remote_reply {id, ok |
  error}`. `crates/web/src/daemon.rs`'s `Subscription::next` gains
  `Event::Remote {id, req}`.
- The plugin reports changes with `web.remote_state {relay, connected,
  error, devices: [{id, name, paired_at, last_seen, linked}], pending:
  {name, code, expires} | null}`. The daemon keeps that mirror
  (`Daemon::remote_state`), answers `remote.status` and `remote.devices`
  from it, calls `Daemon::changed()`, and sends the confirmation notice
  (`notice_others`' path, to every surface). `snapshot` for a local page
  gains `remote: {enabled, connected, devices, pending}`; a device's
  snapshot does not.
- `web::start_if_enabled` also starts the plugin when `[remote] enabled`
  is true; `rlord remote …` without the plugin names `rlord web install`.

#### Config

```toml
[remote]
enabled = false        # true: rlord-web keeps a connection to the relay below
relay = ""             # https://relay.example.com (set by `rlord remote setup`)
relay_key = ""         # the relay's key fingerprint, base64url (same command)
ca_file = ""           # extra root certificate (PEM) for a private relay
device_expiry_days = 90
```

`crates/core/src/config.rs` `RemoteConfig`; the template gets the section
with the same comments. There is no default relay. `rlord remote setup`
writes the first three with `Config::set_*` (the code `rlord web remove`
uses for `[web] enabled`).

#### The overlord

Nothing new for it: no MCP tool, no meta-context, no list concept, and
devices are not visible to it. `overlord.md` and `mcp.rs` are unchanged.
A daemon test asserts no MCP tool reaches a `remote.*` method.

#### `check-deps`

`xtask/src/check_deps.rs` gets three checks, each with a planted-name
test like today's:

1. **`rlord`** (existing): `DENIED` gains `robot-overlord-e2e`,
   `robot-overlord-e2e-wasm`, `robot-overlord-relay`,
   `robot-overlord-relay-proto`, `robot-overlord-e2e-device`, `snow`,
   `libcrux-ml-kem`, `chacha20poly1305`, `curve25519-dalek`, `p256`,
   `ecdsa`, `ring`, `aws-lc-rs`, `tokio-rustls`, `webpki-roots`,
   `wasm-bindgen`. `qrcode` is allowed (it draws a string).
2. **`rlord-relay` holds no content crypto and none of rlord**: its tree
   must not name `robot-overlord-e2e`, `snow`, `libcrux-ml-kem`,
   `ml-kem`, `chacha20poly1305`, `curve25519-dalek`, `x25519-dalek`,
   `robot-overlord-core`, `robot-overlord-daemon`, `robot-overlord-web`.
3. **`rlord_e2e` is sans-IO**: its tree must not name `tokio`, `mio`,
   `axum`, `hyper`, `tungstenite`, `rustls`, `socket2`.

A fourth, source-level: no file under `crates/web/src/` other than
`main.rs` names `TcpListener` (the plugin's one listener stays the
loopback one), and `crates/relay` is the only crate with a non-loopback
bind.

#### Release and packaging of `rlord-relay`

- **Tarballs**: `rlord-relay-v<ver>-<target>.tar.gz` for the two Linux
  targets (static musl): the binary, both licences, `relay.toml.example`,
  `rlord-relay.service`. `Version::relay_tarball(target)` in
  `xtask/src/version.rs`, added to `tarballs()` and `workflow_assets()`
  for `LINUX` only, so `SHA256SUMS`, the attestations, preflight and
  `verify_and_publish` cover it. No macOS tarball (a relay is a server; a
  Mac runs it from a checkout), so a dev build's `dev_assets()` has none.
- **Build** (`xtask/src/build.rs`): `cargo zigbuild -p
  robot-overlord-relay` per Linux target in `build-release linux`; no
  compiled-in hash (it is not a plugin of `rlord`; its users verify with
  `SHA256SUMS` and `gh attestation verify`).
- **Image**: `ghcr.io/robot-lord/rlord-relay:<version>` and `:latest`
  (stable releases only), `linux/amd64` and `linux/arm64`, `FROM scratch`
  with the static binary and a non-root user id, built from the release's
  own tarballs by `cargo xtask relay-image` (plain `docker buildx`, no
  marketplace action) in a new `relay-image` job of `release.yml` after
  `smoke-linux`; pushed and attested only when the release is published.
  The job needs `packages: write`.
- **Self-hosting files** in `deploy/relay/`: `Dockerfile`, `compose.yml`
  (the relay in `proxy` mode behind Caddy, which gets the certificate),
  `Caddyfile`, `relay.toml.example`, `rlord-relay.service` (systemd, for
  the tarball with `files` mode).
- **Smoke** (`smoke-linux`, each distro): `rlord-relay --version`,
  `rlord-relay check` against the example config, and `rlord-relay
  address` printing a parseable address.
- **Docs**: a new official doc `docs/relay.md` (indexed from
  `docs/README.md`): what the relay sees and never sees, running it
  (Docker compose, or tarball and systemd), DNS and TLS, registration
  modes and tokens, limits, backup (the key and the database), upgrades,
  and that losing `relay.key` means every host sets up again.
- Not on crates.io, not in Homebrew, not in the deb or rpm.
- **Versions**: the relay shares the workspace version. The link's `v`
  is separate (1); a relay refuses another `v` with 4426, and the host
  shows "the relay speaks another protocol version" rather than
  retrying.

#### Production relay: requirements and where they land

The user's four requirements ("Production relay" in the spec above),
checked one by one. Each says what slice 3 builds or reserves now, what
waits for a later **production-relay slice**, and where this plan
disagrees with the proposal as given. The design choices are open
questions 12 to 18; nothing here settles them. This subsection amends
the relay decisions above where it says so.

**The rule that bounds all four: accounts govern access to the relay,
never the keys.** The account, the tier and the store never appear in
the QR, a Noise prologue or a device record. A device is authorised by
its host (the allow-list the host sends, and the host's own
`devices.json` and static keys), never by an account login. So a
compromised or hostile account service can refuse service, or register
hosts of its own under an account; it cannot add a device to an existing
host, read anything, or move a host. The adversarial tests already hold
the relay to that ("a relay that invents `link` for a device not on the
allow-list").

**1. Modular deployment, storage behind an abstraction.**

- *Agreed in part.* All durable access in `crates/relay` goes through
  one trait from the start (`src/store.rs`, `trait Store`), with SQLite
  as slice 3's only backend. Other backends come later behind cargo
  features, so a self-hoster's binary never links a cloud SDK.
- *Pushback: plain get/put/delete is not enough, and "durable state is
  session information" is the wrong way round.*
  - Sessions are the one thing that must **not** be durable: connections,
    links, rendezvous, signal flags and mailbox queues live in a relay
    process's memory. What is durable is registration: host key →
    account and status, a host's device allow-list, enrolment tokens,
    and later account records and usage totals.
  - The trait needs four things a bare KV lacks: a **conditional write**
    (a version per record: token use-once, replacing an allow-list,
    two relays writing the same host), a **prefix scan** (a host's
    devices, an account's hosts), **expiry** (as an `expires_at` field
    the reader checks; a backend's own TTL is only cleanup), and
    nothing else. Proposed shape: `get(ns, key) -> (bytes, version)`,
    `put(ns, key, bytes, expect: Option<version>)`, `delete(ns, key,
    expect)`, `scan(ns, prefix)`.
  - **No durable counters.** Metering a proxy with a store write per
    frame, or per second per connection, is the wrong cost. Counters
    live in memory per account and are flushed as periodic usage records
    (approximate by one flush interval after a crash). Quota checks read
    memory.
  - **S3 is a poor fit** for anything but rarely-changing records:
    conditional writes exist, but there is no scan-by-prefix that is
    cheap or consistent enough for an allow-list on every connect, and
    no usable expiry. DynamoDB, Redis and Postgres all fit the four
    operations.
- *Pushback: the store does not make the relay multi-node.* Two relay
  processes sharing a store still can't splice a host on one to a device
  on the other. That needs routing (a load balancer that sends a host
  and its devices to the same process, or relay-to-relay forwarding),
  and it is where queues and signal flags would have to move too. Moving
  mailbox queues into a shared store would also end "no ciphertext on
  disk" and put it with a third party. Recommended: one process first
  (a proxy for kilobytes per second per link goes a long way on one
  machine), routing designed when it is needed. Open questions 12 and
  13.
- *Slice 3:* the `Store` trait and its SQLite backend; a conformance
  test suite any backend must pass (`crates/relay/tests/store.rs`);
  records as versioned JSON values under namespaces `host`, `device`,
  `token`. The pairing `done` message also gives the device the host's
  relay key fingerprint (it needs it for `hello.host`; a later routing
  key would be derived from it).
- *Later:* the chosen backend(s); routing between processes; a
  migration command between stores.

**2. Identity monitoring: which client, which account.**

- *No new identity on the wire is needed.* Every connection already
  proves a P-256 key. The relay maps host key → account at registration;
  a device belongs to its host's account through the allow-list. "Which
  client" is the fingerprint; "which account" is a lookup.
- *Slice 3 (amends "What the relay keeps"):* a host record has `account`
  and `status` (`active`, `suspended`); an enrolment token is made for
  an account (`rlord-relay token new --account NAME`; a self-hosted relay
  with no accounts uses the one account `local`). Every connection's
  state carries its account and the limits that apply; an in-memory
  `Meter` counts bytes each way, messages and connections per account
  and per identity. `rlord-relay host suspend|resume` sets the status; a
  suspended host and its devices are refused with close code 4402, which
  clients show and do not retry.
- *Reserved in link v1 now:* `ready` may carry `limits` (what this
  account's tier allows: devices, bytes per second, live panes), so a
  client can say why something is refused; `err` and the new `bye`
  message (below) carry a machine-readable `code`.
- *Later:* durable usage records, tiers as named limit sets, an admin
  interface for ban and allow, usage shown to the user.
- *The privacy trade-off, to state in `docs/relay.md` and
  `SECURITY.md`.* A relay with accounts learns, beyond the list in
  `e2e-crypto.md` ("What the relay sees"): which person or organisation
  (as far as the login identifies one) owns which hosts; how many hosts
  and devices they have; each one's addresses, times online and volume;
  and with billing, who pays. It still learns no content, names of
  sessions or devices, or content keys. A self-hosted relay without
  accounts sees only key fingerprints. Hosted is therefore less private
  than self-hosted in metadata, equal in content.
- *Pushback:* devices should not log in to an account. The phone is
  paired by QR and authorised by its host; an account login on the phone
  would tie E2E reachability to the account service and add a second
  identity to protect, for nothing the allow-list doesn't already give.

**3. Load shedding.**

- *A constraint the crypto sets:* a live lane frame can never be
  dropped. The Noise session counts frames; one missing frame poisons
  it. So shedding is done by **backpressure** (stop reading from the
  sender) and by **closing connections**, never by dropping frames. The
  mailbox lane refuses a new message when full (already planned).
- *Slice 3 (amends the limits):* every per-link buffer is bounded (64
  frames; the spike's forwarder used unbounded channels). A full buffer
  stops the relay reading from the sender; a link stalled for 10 s is
  closed. Limits are looked up per account (`Limits` from the
  `Authorizer`, below), not read from one global table. A global
  `max_connections`: at the cap an upgrade is refused with HTTP 503 and
  `Retry-After` before any signature work. Before closing for load or
  rate, the relay sends `{"t":"bye","code","retry_after"}`; close code
  4503 means overloaded. The host and the test client honour
  `retry_after` with jitter and never reconnect faster than it says.
- *Later:* shedding order under pressure (unauthenticated first, then
  pairing, then by tier); per-account aggregate throughput and
  connection caps tied to tiers; a health endpoint and metrics on a
  separate loopback listener; draining for a deploy (`bye` to everyone
  with a spread of `retry_after`).

**4. Accounts and login.**

- *The key stays the identity; an account is a binding of keys to a
  person.* "More identifiable than tokens" is met by binding a host's
  relay key to an account once, not by giving the host a password or a
  long-lived bearer token. After binding, the host authenticates as in
  slice 3: its key, signed challenge. Nothing account-related is stored
  on the host besides the key it already has.
- *Pluggable, one code path.* The relay asks an `Authorizer`
  (`crates/relay/src/policy.rs`) one question when an unknown or known
  key connects: admit (account, limits), refuse (code), or pending
  (below). Slice 3 ships `StaticAuthorizer`: the `token`, `open` and
  `closed` modes, accounts as plain labels. A hosted service plugs in an
  authorizer that asks the account service. Self-hosting needs no
  account service and no login (open question 16).
- *Registering a host from rlord, keeping the invariants.* The host may
  only ever dial its one pinned relay, so the login must not introduce a
  second server the plugin talks to. Proposed: a **device-authorisation
  flow over the relay link itself**. `rlord remote login` (CLI, TUI or
  local page, through the daemon to `rlord-web`, as every `remote.*`
  command) connects to the pinned relay with the not-yet-registered key;
  the relay proves its identity as always, then answers `enroll {url,
  code, expires}` instead of `ready`; rlord shows the address and code;
  the user approves in any browser, signed in to the account service;
  the relay then sends `ready`. The plugin makes no HTTP request, holds
  no account credential, and the `rlord` binary only draws a string.
- *Reserved in link v1 now:* `hello.enroll` is an object (`{"kind":
  "token","token"}` in slice 3; other kinds later), and the relay
  message `enroll {url, code, expires}` is defined and handled by the
  client (shown to the user, then wait) even though `StaticAuthorizer`
  never sends it.
- *Later:* the account service (sign-up, login, the approval page,
  listing and removing hosts, tiers, billing if any), the authorizer
  that talks to it, `rlord remote login` and `logout` (a signed
  de-registration), and the hosted relay's terms and abuse handling.
- *Pushback:* keep passwords out of this entirely (delegate login to an
  identity provider or passkeys; open question 15), and keep the account
  service off the E2E path: it is never in the QR, never dialled by a
  device, and never able to confirm a pairing.

| | Slice 3 | Production-relay slice |
| --- | --- | --- |
| Storage | `Store` trait (versioned get/put/delete, prefix scan, `expires_at`), SQLite, conformance tests | other backends behind features, migration, multi-process routing |
| Identity | key → account and status on the host record, per-account and per-identity `Meter` in memory, suspend and resume, close code 4402, `ready.limits` reserved | durable usage, tiers, admin ban and allow, usage shown to users |
| Load | bounded buffers and backpressure, per-account `Limits`, `max_connections` with 503, `bye` with `retry_after`, 4503, clients honour it | shedding order, tier caps, metrics and health, drain |
| Accounts | `Authorizer` trait, `StaticAuthorizer`, `hello.enroll` as an object, `enroll` message handled by clients | account service, login over the link, `rlord remote login`, hosted terms |

#### Tests

Never `tmux -L rlord` or the default server; no real agent; no network
beyond loopback. Every relay in a test is the real `rlord-relay` on
`127.0.0.1:0`.

- **`rlord_e2e` unit and vectors**: the spike's `tests/protocol.rs` and
  vectors, plus the kind-3 handshake, mailbox export and import, the
  mailbox against `snow`'s transport for the same keys, fragment
  reassembly (limit, bad flags), a poisoned session, QR round trip and
  rejects. `cargo xtask e2e vectors --check`.
- **WASM** (`cargo xtask e2e test-wasm`): the vectors in Node from the
  built module, byte-identical; one interop pairing and reconnect
  against the native test client through the real relay.
- **`relay-proto`**: transcripts sign and verify; a signature over
  another origin, key, nonce or role fails; address parse and
  normalisation; frame headers.
- **Relay integration** (`crates/relay/tests/`): auth both ways; each
  close code; registration modes (token used once, expired, `closed`);
  allow-list replace; link and unlink; splice both ways; a newer
  connection replaces the older; every limit in the table; queue order,
  ack, expiry, `full`; signal flags and `boot`; no
  `Sec-WebSocket-Extensions` in the upgrade answer; `files` mode with an
  `rcgen` certificate and a SIGHUP reload; the database holds no payload
  bytes after a run (a marker string sent as payload is not found in
  `relay.db` or the log).
- **Relay, room for production**: the `Store` conformance suite
  (versions, a lost conditional write, scan, expiry) against SQLite; a
  suspended host and its devices get 4402 and the clients don't retry;
  a slow receiver stops the sender being read and the link closes after
  the stall limit, with no frame dropped before that; at
  `max_connections` an upgrade gets 503 with `Retry-After`; a client
  told `bye` with `retry_after` waits at least that long; a client sent
  `enroll {url, code}` reports it and waits for `ready`; per-account
  counters add up across a host and its devices.
- **Daemon** (`web_api.rs`): `via` required; each `remote: false` op
  refused from a device and each `remote: true` op reaching its method
  with only its arguments; `remote.*` unreachable through `web.call` and
  through `mcp.call`; the forwarded command and its reply; the mirror
  and the confirmation notice; `overlord.messages` bounded;
  `the_pages_ops_are_the_daemons` still holds.
- **End to end** (`crates/web/tests/remote.rs`: the real `rlord-web`
  under an in-memory daemon, the real relay, `rlord-e2e-device` as the
  device): setup, pair, confirm, `snapshot`, a change gives `changed`,
  `overlord.ask` reaches the daemon, reconnect with a fresh session,
  the cheap check (no handshake when nothing is waiting, a handshake
  after a signal), a mailbox `overlord.ask` sent while the plugin is
  stopped and answered when it returns, a snapshot larger than one
  frame.
- **Adversarial** (same harness; a "hostile relay" is a small test
  forwarder in `crates/e2e-device/src/testing.rs`, built on
  `relay-proto`):
  - *Wrong relay*: a relay with another key at the pinned origin; the
    pinned key claimed without the private key; a faithful forwarder to
    the real relay under its own identity (handshake fails on both
    ends); a 3xx redirect; a device given a QR whose relay differs from
    the one it reaches. In each, nothing is sent after the failed check
    and no other address is tried.
  - *Replay*: a recorded message 1 (pairing and reconnect) replayed
    gives no session and no op; a recorded transport frame replayed,
    reordered or dropped poisons the session; a recorded mailbox frame
    replayed or skipped discards the mailbox and the next link heals it;
    a recorded pairing replayed after the rendezvous closed is refused
    by the relay and by the host.
  - *Revoked device*: its relay connection is refused (4403); with a
    relay that lets it through anyway, `KKpsk2` fails at the host; its
    queued mailbox frames are dropped; its live link closes at
    revocation.
  - *Tampering*: a flipped bit in message 1, in the KEM ciphertext, in
    message 2, in a transport frame and in a mailbox frame each fail
    closed; a truncated or oversized frame; a relay that swaps two
    devices' frames; a relay that invents `link` for a device not on the
    allow-list (the host's own check refuses it).
  - *Pairing*: a wrong secret three times closes the rendezvous; a
    second device using a used QR is refused; an unconfirmed device can
    call nothing and is gone after a reject or the timeout; a remote
    device calling `device.confirm` or `device.pair` is refused.
  - *Crash points*: the plugin killed between advancing and saving the
    mailbox, and between acting and saving, each heals without a
    repeated op (the request id check).
- **`check-deps`**: the three trees and a planted name in each.
- **CLI and TUI**: `rlord remote` without the plugin names the install;
  `rlord_core::qr` renders a known string to a known matrix; the Devices
  screen's keys; the notice.
- **Page** (Vitest): the Devices view against a fixture the daemon
  writes (`device_list.json`).
- **By hand**: `rlord-relay` in Docker compose on a real host with a real
  certificate, a host behind NAT, `rlord-device` on another network;
  pair, confirm, ask the overlord, revoke.

### Implementation steps

For an Opus implementer at high effort. One commit each; the PR bumps
the version to the next `-dev.N` and regenerates `Cargo.lock`. Record
each step in "Progress".

1. **Gates, before anything is built on them.** (a) `snow` 0.10.0's raw
   split plus our cipher state gives the same frames as its
   `TransportState`, rekeys included (a scratch test). (b) `rustls` with
   the ring provider, `p256` and `rusqlite` cross-build for both musl
   targets with `cargo zigbuild`. (c) The spike's core builds for native
   and WASM inside the main workspace (edition 2021, resolver 2, the
   exact pins beside the workspace's own `sha2` and `getrandom`). If (a)
   fails, use the fallback under "Risks" and tell the user.
2. **`crates/e2e`**: the port, `stream.rs`, kind 3, the poisoning rule,
   vectors and `cargo xtask e2e vectors`.
3. **Mailbox** (`mailbox.rs`): cipher state, export and import, vectors.
4. **`crates/e2e-wasm`**, `cargo xtask e2e wasm|test-wasm`, the toolchain
   target, CI's `web` job.
5. **`crates/relay-proto`**: messages, frames, transcripts, address.
6. **Relay, part 1** (`crates/relay`): config, key, `store.rs` (the
   `Store` trait, SQLite, the conformance suite), `policy.rs`
   (`Authorizer`, `StaticAuthorizer`, `Limits`), `serve` with `proxy`
   and `files` TLS, the connect sequence with `enroll`, `bye` and the
   close codes, registration, accounts as labels, the commands.
7. **Relay, part 2**: allow-lists, links and splicing over bounded
   buffers with backpressure, rendezvous, queues, signals, the `Meter`,
   per-account limits, `max_connections`. Integration tests.
8. **`check-deps`**: the new lists and checks.
9. **Daemon** (`web.rs`, `remote.rs`, `lib.rs`, `config.rs`): `via` and
   `Op.remote`, `WEB_PROTOCOL` 3, `[remote]`, the forwarded commands,
   the mirror, `overlord.messages`, the `device.*` ops, `snapshot`'s
   `remote`. Tests.
10. **Plugin: keys and link** (`remote/{mod,keys,link}.rs`): setup,
    the pin check, connect, auth, backoff, state reports.
11. **Plugin: pairing, devices, revocation** (`pairing.rs`,
    `devices.rs`, `audit.rs`).
12. **Plugin: sessions** (`session.rs`, `mailbox.rs`): `KKpsk2`, the
    control stream to `web.call`, `changed`, signals, the mailbox path.
13. **Test client** (`crates/e2e-device`) and the end-to-end and
    adversarial tests.
14. **CLI** (`crates/rlord/src/remote_cmd.rs`, `core/src/qr.rs`):
    `rlord remote setup|pair|confirm|reject|devices|revoke|status|
    disable`.
15. **TUI** (`tui/src/remote.rs`, `launcher.rs`): `:remote`, the Devices
    screen, the notice.
16. **Local page** (`views/Devices.tsx`, `proto.ts`, fixtures).
17. **Release** (`version.rs`, `build.rs`, `release.rs`,
    `smoke_in_container.sh`, `release.yml`, `deploy/relay/`): tarballs,
    the image job, smoke. A dry run of the workflow.
18. **Docs**: `docs/relay.md`; `docs/architecture.md` "Web UI" gains
    "Remote access" (the invariants, the three parties, the link, the
    pin, pairing, `via` and the remote column) and its crate list;
    `docs/guide.md`; `SECURITY.md` (what the relay sees, what a paired
    device can do, reporting scope for the relay); `CLAUDE.md`'s network
    rule (the relay and e2e crates, the three `check-deps` checks, how to
    make an op remote); the config template; `e2e-crypto.md` and this doc
    updated. The protocol written out in one place for the reviewer
    (`docs/working/e2e-protocol.md`, promoted when reviewed).

Steps 2 to 5 need nothing else; 6 and 7 need 5; 9 needs nothing before
it; 10 to 12 need 2, 3, 5, 7 and 9; 13 needs 12; 14 to 16 need 9 and 11;
17 needs 7.

### Risks

- **A protocol of our own, now carrying real sessions.** The hybrid
  step, the mailbox cipher over raw split keys and the relay link's
  transcripts are ours. Bounded by vectors, the adversarial tests, a
  written protocol, and no use beyond the user's own relay before a
  review (open question 8).
- **The mailbox cipher leaves `snow`'s transport.** It is the one place
  the core handles raw keys and nonces itself. Gate 1(a) checks it
  against `snow`. Fallback if the raw split is unusable: the mailbox
  session lives in memory only and is keyed again on every live link;
  messages queued across a restart of either end are lost and resent by
  id. Same wire shape, weaker delivery.
- **Two paths for two ops** (live and mailbox). A request can arrive
  twice or out of order with its live twin; the request id check is the
  only guard. Kept to `overlord.ask` and `overlord.new_session`.
- **Pairing grants a shell.** A paired device can type into agents and
  launch sessions. Guards: the QR's one use and short life, the code and
  the host's confirmation, the allow-list with its remote column, the
  audit log, expiry, revocation.
- **The relay is a public server.** Unauthenticated connections, token
  guessing, queue filling and slow peers are what its limits and tests
  are for; `open` registration invites abuse and is not the default.
- **Metadata.** The relay sees addresses, fingerprints, who reaches
  whom, when and how much (to the padding bucket). Stated in
  `docs/relay.md` and `SECURITY.md`, not fixed.
- **No TLS channel binding.** A party that can pass TLS validation for
  the pinned origin can sit in front of the relay and see its metadata.
  It cannot read or redirect content.
- **`rlord-web` gets a TLS client and outbound connections.** The opt-in
  promise now rests on `[remote] enabled`, the pin and at least one
  device; a bug there dials out for a user who only wanted the local
  page. Tested: with no `[remote]` the plugin opens no outbound socket.
- **A wrong origin setting on a self-hosted relay** (the relay's
  `origin` not being how peers reach it) fails every connection with
  "wrong relay". `rlord-relay check` and the docs say so first.
- **Three processes and two version axes** (`WEB_PROTOCOL`, the link's
  `v`). A relay is upgraded on its own schedule; 4426 and a clear
  message, not a parse error.
- **Build weight**: `ring` and `libcrux` in the plugin, `ring` and
  SQLite in the relay, a WASM target and `wasm-bindgen-cli-support` in
  xtask. Gate 1(b) and 1(c) check the cross-builds before the rest.
- **Compression stays open.** A snapshot is JSON and can be tens of
  kilobytes per fetch; uncompressed is the v1 cost. `rev` keeps repeats
  off the wire. Owed to the exploration in `e2e-crypto.md`: record real
  snapshot sizes, before and after padding, from the end-to-end test.
- **Owed from the spike, still**: phone benchmarks, Firefox.

### Open questions for the user

1. **How much of the mailbox in this slice?** Planned: the core's
   saveable mailbox session and the relay's short queue (32 messages,
   128 KiB, 10 minutes) are built and tested, and `rlord-web` uses them
   for two ops only, `overlord.ask` and `overlord.new_session`, when the
   host is briefly unreachable; everything else needs a live link.
   Alternative: live link plus the contentless signal only, and the
   mailbox waits for the phone slice (less code of our own handling
   keys now, but a message typed while the laptop reconnects fails
   instead of arriving). Recommended: as planned.
2. **TLS on the relay.** Planned: certificate files (re-read on renewal)
   or plain HTTP behind a proxy, with a Docker compose file that puts
   Caddy in front to get the certificate. Alternative: ACME built into
   `rlord-relay` (`rustls-acme`), one binary and nothing else to run,
   at the cost of an outbound connection from the relay and more
   dependencies. Recommended: files and proxy now, built-in ACME later
   if self-hosters ask.
3. **Who may register a host on a relay.** Planned: `token` by default
   (the relay's owner makes a token, `rlord remote setup` uses it once),
   with `open` and `closed` as settings. Is `token` right for your
   hosted relay too, or should that one be `open` with a host cap?
   Recommended: `token` everywhere until the protocol is reviewed.
4. **The hosted relay.** Planned: not stood up in this slice; it ships
   the binary, the image and the self-host docs, and you run one
   yourself for the by-hand pass. A project-run relay (domain, machine,
   who pays, abuse handling) comes with the phone slice. Recommended:
   as planned; there is no default relay in the config either way.
5. **mTLS.** The spec says "TLS or mTLS, configurable"; the earlier
   answer made mTLS optional. Planned: signed challenge only in this
   slice, mTLS later for native clients. Recommended: defer.
6. **Where the host confirms a device.** Planned: the CLI (`rlord remote
   pair` waits and asks), the TUI's Devices screen, and the local page's
   Devices view, all three; never a remote device, never the overlord.
   Should the local page be allowed to start a pairing and show the QR
   (the pairing secret then passes through the browser), or only to
   confirm and revoke? Recommended: allow all of it on the local page.
7. **What a remote device may do.** Planned: everything a local page can
   except what writes `config.toml` (`form.setup_save`,
   `overlord.setup_request`), the surface reports, the key screen and
   device management; no panes yet. Recommended: as planned.
8. **Review before use.** Planned: you may use it on your own relay
   before a review; nobody else is offered a relay, and the docs say the
   protocol is unreviewed, until someone has read
   `docs/working/e2e-protocol.md`. Who reviews (paid, informal, none for
   now) is still open from `e2e-crypto.md`. Recommended: informal review
   before the phone slice ships, paid before a public relay.
9. **Binding the relay's proof to the TLS session.** Planned: no, so a
   relay can sit behind a proxy or CDN. With it, a middlebox holding a
   valid certificate for the relay's name could not even forward; without
   it, such a middlebox sees the relay's metadata and nothing else.
   Recommended: no binding in v1; add it as an option for `files` mode
   if a reviewer asks.
10. **Unused devices.** Planned: a device not seen for 90 days is
    revoked (`[remote] device_expiry_days`, 0 = never). Recommended: 90.
11. **Where the relay ships.** Planned: Linux tarballs (both
    architectures) in the same GitHub release, and a `ghcr.io` image;
    no macOS build, no crates.io, no Homebrew, no deb or rpm.
    Recommended: as planned.
12. **The relay's storage abstraction.** You proposed a plain
    key-value interface (DynamoDB, Redis, S3, anything). Planned: a
    slightly richer trait (versioned conditional writes, prefix scan,
    an expiry field), SQLite now, other backends behind features later;
    counters and all session state stay in memory. Options: (a) that;
    (b) strictly get/put/delete, with allow-lists and tokens packed into
    single values and races accepted; (c) no abstraction until a second
    backend is real. S3 fits only (b) and poorly. Recommended: (a), and
    pick the first hosted backend when you pick where it runs.
13. **One relay process or several.** A shared store does not let two
    processes splice each other's connections. Options: (a) one process
    per relay, scaled up, restarts handled by reconnects; (b) a load
    balancer that keeps a host and its devices on one process (needs a
    routing key in the URL, a link v2); (c) relay-to-relay forwarding
    with shared presence (Redis or similar), which also moves queues
    and flags out of memory. Recommended: (a) first, (b) when one
    machine is not enough, never queues in a third-party store.
14. **Where accounts live.** Options: (a) a separate account service
    the hosted relay asks through the `Authorizer` interface, self-host
    using the static one; (b) accounts built into `rlord-relay` behind a
    setting; (c) an off-the-shelf identity product in front. Recommended:
    (a): the relay stays small and self-hostable, and the account
    service can change without touching the protocol.
15. **How a person logs in.** Options: an identity provider (GitHub or
    another OAuth/OIDC login), email links, passkeys, or passwords we
    store. Recommended: an identity provider first (rlord's users have
    GitHub accounts), no passwords of our own, passkeys later if wanted.
16. **Accounts on a self-hosted relay.** Options: (a) none: tokens,
    `open` or `closed`, every host in the one account `local`; (b)
    account labels on tokens for per-team metering and suspension, no
    login; (c) the full account service, self-hosted too. Recommended:
    (a) as the default with (b) available (slice 3 builds both, they are
    the same code); (c) only if a company asks.
17. **Registering a host from rlord.** Planned for the later slice:
    `rlord remote login` shows an address and a code that arrive over
    the pinned relay link, you approve in a browser, and the host's key
    is bound to your account; rlord keeps no account credential.
    Alternative: `rlord-web` talks to the account service directly over
    HTTPS (a second pinned server, a stored token, more to protect).
    Recommended: over the relay link, so "one pinned relay" stays
    literally true.
18. **What tiers differ in, and what is metered.** Candidates: number of
    hosts and devices, bytes per second and per month, live panes,
    mailbox size. Recommended: tiers are named sets of the limits the
    relay already enforces, metered per account as in-memory counters
    flushed to periodic usage records; per-device figures stay in
    memory only (less kept about each device). The actual tiers and
    whether anything is paid are yours to set.

### Slice 3 answers (user, 2026-11-17)

All 18 open questions: the recommended defaults, "for now". Questions 12–18
(the production relay) stay revisitable when the hosted relay is planned.

## Slice 4 plan: the phone app

Planned 2026-11-23 (Fable planning agent), on `feat/web-ui-4` from
`feature/web` (slice 3 merged there as #77). Nothing below is built. Goal:
the user pairs their own phone by QR, sees the session list and talks to
the overlord of a host behind NAT through their own relay, and the app
recovers by itself from every kind of lost connection. Chat and state
only: no live panes (the stream ids still leave room), no remote browser
client, no network code in `rlord`.

The user's decisions this plan takes as fixed: a native shell around a
WebView running the same Preact page; crypto in Rust; chat and state
first, panes later; per-device keys, pairing by QR with the host's
confirmation; one pinned relay per host, no redirects or fallbacks; the
host never accepts an incoming connection; the phone gets the same direct
launches as a local page; post-quantum hybrid; **compression stays
provisional and is not built**.

**The feature branch stays dark (user, 2026-11-18).** No release of any
kind is cut from `feature/web` or a branch off it: no stable release, no
dev build, no `release.yml` run, no TestFlight, no Play track, no
app-store submission. Everything in this slice is built and installed
locally until the final merge to `main`. A PR into `feature/web` still
bumps the `-dev.N` version (CI's `version-bump` checks it); nothing is
published for it. `release.yml`, `version::CRATES` and the release asset
lists are not touched by this slice.

Versions read from crates.io on 2026-11-23: `uniffi` 0.32.2, `tauri`
2.12.7, `rustls-platform-verifier` 0.7.1, `tokio-tungstenite` 0.30.0,
`cargo-ndk` 4.1.2 (not used, below).

What exists today (read from the code and this machine, for the
implementer):

- `crates/e2e-device` (`rlord_e2e_device`, bin `rlord-device`): `Device`
  (`start_pairing`, `connect`, `live`, `key_mailbox`, `mailbox_request`,
  `mailbox_receive`, `mailbox_open`), `Live` (`request`, `answer`,
  `event`), `client::connect` (tokio-tungstenite 0.29, rustls with ring,
  `webpki-roots`, `ca_file`; the pin check; `ConnectError`), and the
  hostile relays in `testing.rs`. It does the protocol steps a test asks
  for; it has **no reconnect loop, no outbox, nothing saved to disk**.
- `crates/e2e-wasm`: the `wasm-bindgen` surface, kept alive by `cargo
  xtask e2e test-wasm`. Nothing loads it in a page.
- The page (`crates/web/ui/src/`): there is no `Transport` type yet.
  `api.ts` has the concrete `Control` (token, websocket, backoff 250 ms
  to 5 s, requests by number); `store.ts` depends only on `Requester`
  (`request(op, args)`); `main.tsx` wires `Control` to the store, the
  `Reporter` and the `ui` commands. `views/Overlord.tsx` and
  `views/Session.tsx` are a header, buttons and a `Pane` (tmux bytes): a
  device has no pane. `app.css` already shows one column under 900 px.
- The plugin's CSP (`http.rs`): `default-src 'none'; script-src 'self';
  style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src
  'self'; connect-src 'self' ws://127.0.0.1:<port>`. No Trusted Types.
- Remote ops: `web::OPS` with `remote` and `idempotent`; a remote call
  has no `page`, is not a surface and gets no `ui` commands (`ui::target`
  knows the terminal and web pages only). `overlord.messages` exists for
  a chat view.
- **Xcode 27.2 is at `/Applications/Xcode.app`, but `xcode-select`
  points at the command line tools.** `cargo xtask app …` sets
  `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer` itself (or
  the user runs `sudo xcode-select -s` once). iOS 27.2 simulator runtimes
  are installed. No Android SDK or NDK, and no iOS or Android Rust
  targets yet (`rustup target add` in step 1).

### Decisions

#### Who owns the keys, the relay link and the sessions: a native Rust engine

The device's whole protocol side runs **natively, in Rust, inside the
app**, reached from Swift and Kotlin through UniFFI. The WebView holds no
key, opens no socket and runs no crypto; it draws the page and exchanges
plain JSON with the engine over the shell's message bridge. The WASM
build is not loaded by the phone page.

This changes answer 3 of `e2e-crypto.md` ("crypto runs as WASM there too;
UniFFI shrinks to what the shell needs"), so it is open question 1. Why:

| Concern | Engine in the shell (UniFFI) | WASM in the page's worker |
| --- | --- | --- |
| Work while the app is in the background (a refresh task, later a push wake-up) | runs without a WebView | the WebView and its sockets are suspended or not loaded at all; a second native relay client would be needed anyway |
| Relay identity key (P-256) | made and kept in the Secure Enclave / Android Keystore, never extractable: the engine asks the shell to sign | must be plain bytes in JS memory to sign in WASM |
| Content key (X25519) and mailbox state | never enter the JS realm; script injected in the page can send ops (as the page can) but can't copy a key | plain bytes in the worker |
| The relay link's rules (exactly one URL, no redirect, no `permessage-deflate` offered, an extra CA) | the same rustls and tungstenite code the host and the tests use | a WebView's `WebSocket` always offers `permessage-deflate`, takes no extra CA and reports little about a failed upgrade |
| The reconnect loop and every recovery rule of `e2e-protocol.md` §4 | one Rust state machine, tested natively against the real relay, plugin and hostile relays | TypeScript in a worker, tested in Node against stand-ins |
| Saving mailbox state before sending (the crash rule) | atomic file writes | IndexedDB transactions, from a worker the OS may kill |
| The page's CSP | `connect-src 'none'`: the page has no network path at all | needs `connect-src wss://<relay>`, `'wasm-unsafe-eval'`, a worker and a Trusted Types policy |
| Cost | UniFFI bindings, an xcframework and an Android library; tokio and rustls in the app | no native library; the WASM path gets real use |

The WASM core stays built and tested (`cargo xtask e2e test-wasm`) for a
later remote browser client; this slice adds nothing to it. The owed
"phone benchmarks" of the spike measured WASM in a phone browser and no
longer gate this slice; they stay owed to a remote browser.

#### Crates

| Crate | Package, output | Depends on | Notes |
| --- | --- | --- | --- |
| `crates/e2e-client` (new) | `robot-overlord-e2e-client`, lib `rlord_e2e_client` | `rlord_e2e`, `rlord_relay_proto`, `tokio`, `tokio-tungstenite`, `rustls` (ring), `webpki-roots`, `serde_json` | The device engine: the state machine, the link driver, the outbox, storage. Not `rlord-core`, not `rlord-daemon`. |
| `crates/e2e-ffi` (new; named in `e2e-crypto.md`) | `robot-overlord-e2e-ffi`, `staticlib` + `cdylib`, lib `rlord_e2e_ffi` | `rlord_e2e_client`, `uniffi` =0.32.2 | The UniFFI surface. Proc-macro mode, no UDL file. |
| `crates/e2e-device` (existing) | lib + bin `rlord-device` | `rlord_e2e_client` | Becomes a thin test client and CLI on top of the engine, so `web/tests/remote.rs` exercises the code the phone runs. Gains `rlord-device bridge` (below). Keeps `testing.rs`. |

- All `publish = false`, in `members`, not in `default-members`, not in
  `version::CRATES`.
- `e2e-client` splits in two so the hard part is testable without
  sockets or clocks:
  - `machine.rs`: a **sans-IO state machine**. Inputs: the page's
    messages, relay text and binary frames, a link opening or closing
    (with its close code), timers firing, `foreground`, `background`,
    `network(up|down)`, a clock reading. Outputs: dial, send, close, set
    a timer, save state, a message for the page, a notification request.
    No tokio, no time source of its own.
  - `driver.rs`: tokio, the websocket, TLS and timers, feeding the
    machine. The pin check stays `relay-proto`'s `check_challenge`.
- Three traits the platform implements (UniFFI foreign traits):
  `Signer` (`public_key()`, `sign(transcript) -> 64 bytes`: the relay
  identity), `Vault` (`seal(bytes)`, `open(bytes)`: wrapping at rest),
  `Listener` (`to_page(json)`, `notify(kind, count)`). Tests and
  `rlord-device` use software ones.
- The FFI surface is small on purpose: `Engine::new(dir, vault, signers,
  listener)`, `page_message(json)`, `scanned(qr, name)`,
  `lifecycle(foreground | background)`, `network(online)`,
  `background_check(budget_ms)`, `wipe()`. Everything else is JSON
  through `page_message` and `to_page`, so a new page feature needs no
  binding change.
- `rust-toolchain.toml` gains `aarch64-apple-ios`,
  `aarch64-apple-ios-sim` and `aarch64-linux-android` (and their mention
  in its comment). Apple Silicon only: no x86_64 simulator or emulator
  target.
- `check-deps` gains: `rlord`'s deny list names the two new crates and
  `uniffi`; the `e2e-client` tree names no `rlord-core`, `rlord-daemon`
  or `robot-overlord-relay`; and the app source check below.

#### Platforms, toolchain, minimum OS

- **iOS first, Android second, both in this slice** (open question 2).
  The engine, the bridge and the page are platform-neutral and built
  first; the iOS shell is built and used before the Android one starts,
  and the Android steps can be cut into a later PR without touching the
  rest. iOS first because it is the stricter platform (background work,
  the enclave's P-256 limit, WKWebView) and the build machine is a Mac.
- **iOS:** Swift, SwiftUI app shell, `WKWebView`. Full Xcode (the
  current release, with the iOS SDK it ships). Minimum **iOS 26**: the
  first with Trusted Types in WebKit, and everything else used
  (`WKURLSchemeHandler`, `WKScriptMessageHandlerWithReply`,
  `BGAppRefreshTask`, `NWPathMonitor`, the Secure Enclave) is older.
- **Android:** Kotlin, one activity, `android.webkit.WebView`
  (`androidx.webkit`). Android SDK and NDK installed by `cargo xtask
  install-tools --app` (versions and checksums of the command line
  tools in `xtask/src/tools.rs`), the Gradle wrapper committed with
  `distributionSha256Sum`, dependency verification on
  (`gradle/verification-metadata.xml`). `minSdk` 31 (Android 12),
  `targetSdk` the current level when built. The app refuses to start the
  page on a System WebView older than a floor checked at launch (the
  first with Trusted Types and `WebMessageListener`: 83; take 120) and
  says to update it.
- **Layout:** `apps/ios/` (a committed Xcode project, sources under
  `apps/ios/rlord/`, signing settings in a git-ignored
  `Local.xcconfig`), `apps/android/` (Gradle project). Neither is in
  the Cargo workspace. Generated files (bindings, the xcframework, the
  `.so`, the page) are not committed.
- **No third-party SDK** in either app: no analytics, crash reporter,
  ad or push library, no Firebase, no Play Services dependency.
  Android: ZXing `core` (pure Java) for QR decoding, CameraX,
  `androidx.webkit`, JNA (UniFFI's Kotlin needs it). iOS: Apple
  frameworks only.

#### The shell: hand-written Swift and Kotlin, not Tauri mobile

| | Thin native shells + UniFFI (chosen) | Tauri 2 mobile | Capacitor / React Native |
| --- | --- | --- | --- |
| Rust in the app | through UniFFI bindings | in process, no bindings | none; a native module would still wrap the core |
| WebView control (scheme handler, CSP headers, content rules, navigation, Safe Browsing off) | all of it, in a few hundred lines per platform | through Tauri's own asset protocol and IPC, which inject scripts and rewrite the CSP; Trusted Types with it is unverified | their own bridge and runtime |
| Enclave / Keystore signing and wrapping | native code, called directly | a Swift and a Kotlin plugin anyway | a native plugin anyway |
| Dependencies in a security-critical app | UniFFI, tokio, rustls | plus tauri, wry, tao and their trees; `tauri-cli` downloaded as a tool | an npm tree in the app |
| Project files | ours | generated and owned by `tauri ios/android init` | generated |

The shell's whole job is: load the bundled page, pass JSON between the
page and the engine, scan a QR, sign and wrap with the platform
keystore, report lifecycle and network changes, and post a local
notification. That is small enough to own, and owning it is what lets
the WebView be locked down exactly. Tauri would save the UniFFI layer
and cost the control.

#### Key storage

Per paired host the device has its own key set (a second host never
sees the same device keys; the UI shows one host in this slice, open
question 10).

| Thing | iOS | Android |
| --- | --- | --- |
| Relay identity (P-256 ECDSA) | Secure Enclave key (`kSecAttrTokenIDSecureEnclave`, private, non-extractable), `AfterFirstUnlockThisDeviceOnly`; signs the peer transcript with `ecdsaSignatureMessageX962SHA256` | Android Keystore EC key (`secp256r1`, `SHA256withECDSA`), StrongBox when the phone has one, else the TEE |
| Vault key (wraps everything below) | a second Secure Enclave P-256 key used for ECIES (`eciesEncryptionCofactorVariableIVX963SHA256AESGCM`) | Keystore AES-256-GCM key, StrongBox when present |
| Content key (X25519 static), the host's static key and pin, the mailbox state (89 bytes), `(boot, seq)` last opened, the outbox | one sealed file per host in the app's support directory, excluded from backup, written atomically (temp file, fsync, rename) by the engine | the same, in `noBackupFilesDir`; `allowBackup="false"` |

- X25519 can't live in the enclave (P-256 only) and StrongBox has no
  Curve25519, as `e2e-crypto.md` found: the content key is software,
  wrapped by a hardware key, in memory only while the engine runs. P-256
  as the Noise curve stays rejected.
- The enclave and Keystore return ASN.1 DER signatures with a random
  nonce; the link wants 64 bytes `r || s`. The engine converts (and
  normalises `s` if the relay's verifier wants low-S). Verification
  doesn't depend on RFC 6979, which stays what software signers do.
  Checked in step 1.
- `AfterFirstUnlock`, not `WhenUnlocked`: a background check runs while
  the phone is locked. The keys never leave the device and are not in
  any backup; a restored or new phone pairs again.
- Simulators and emulators have no enclave: debug builds fall back to
  software keys in the Keychain / Keystore, and say so on the status
  screen. A release build refuses to run without hardware keys.
- **Nothing of the host's content is stored on the phone.** The last
  snapshot and the chat live in the engine's memory. The only user
  content at rest is the outbox (the user's own unsent or unanswered
  requests), sealed with the rest.
- **App lock** (open question 9): on by default, the device's
  biometrics or passcode (`LocalAuthentication`, `BiometricPrompt`)
  when the app opens after more than 5 minutes away; the app requires a
  passcode to be set on the phone. While locked the WebView is covered
  (also in the app switcher; Android `FLAG_SECURE`). Pairing a phone is
  granting it a shell: an unlocked stolen phone must not be one tap from
  it.

#### The bridge and `RemoteTransport`

- **`Transport`** (new, `crates/web/ui/src/transport.ts`): what the page
  needs of its connection: `request(op, args)`, the events (`state`,
  `hello`, `changed`, `ui`), `stop()`, and `caps` (`panes`, `setup`,
  `keys`, `devices`, `pending`). `Control` becomes `LocalTransport` (same
  file, same tests, all caps true but `pending`). `Store` keeps its
  `Requester`.
- **`RemoteTransport`** (`remote.ts`): the same interface over the
  shell's bridge. Requests and answers are the control socket's shapes
  unchanged (`{"id","op","args"}`, `{"id","ok"}`, `{"id","error"}`), so
  nothing above `Transport` knows which one it has. Views ask `caps`,
  never the user agent: with `panes` false a view shows no pane; with
  `setup`, `keys` and `devices` false those buttons are absent (their
  ops are local-only and would be refused).
- **The page picks nothing at run time.** A second Vite entry
  (`app.html`, `src/app.tsx`) builds the phone page into
  `crates/web/ui/dist-app/` with `RemoteTransport`, without xterm.js and
  without `LocalTransport`; the local page's bundle carries no remote
  code. `cargo xtask web build --app`.
- **The bridge is three hops of the same JSON string.** Page to shell:
  iOS `window.webkit.messageHandlers.rlord.postMessage(text)` (a
  `WKScriptMessageHandler` registered for the main frame of the app's
  own origin only); Android `WebViewCompat.addWebMessageListener` with
  the app origin as the only allowed origin (never
  `addJavascriptInterface`). Shell to page: iOS `callAsyncJavaScript`
  of one fixed function with the text as an argument (never a built
  script string); Android the listener's `JavaScriptReplyProxy`. The
  shell parses nothing but the few `app` messages it serves itself.
- **Messages beyond the control socket's** (typed in `proto.ts` as
  `AppMessage` / `AppEvent`, with fixture tests):
  - page to engine: `{"app":"status"}`, `{"app":"pair_cancel"}`,
    `{"app":"unpair"}`, `{"app":"cancel","request":"<id>"}`,
    `{"app":"settings",…}`; page to shell: `{"app":"scan"}` (opens the
    native scanner).
  - engine to page: `{"event":"conn","state":…,"host":"<name>",
    "relay":"<origin>","detail"?}`, `{"event":"pairing","state":
    "connecting"|"waiting"|"done"|"rejected"|"failed","code"?,
    "reason"?}`, `{"event":"outbox","items":[{id, op, text?, at,
    state}]}`, plus `hello`, `changed` and `ui` as a local page gets
    them.
  - `conn.state` is one of `unpaired`, `offline` (no network),
    `connecting`, `relay` (linked to the relay, no session yet),
    `host_offline`, `live`, `wrong_relay`, `revoked`, `update_app`,
    `update_host`, `suspended`. The header shows it in words with the
    host's name.
- **The QR never passes through the WebView.** The scanner is a native
  screen; its string goes from the shell straight to
  `Engine::scanned`. The page only sees `pairing` events.
- **Request ids.** The page numbers its requests as today. The engine
  gives every request that needs one a device-unique id (16 random
  bytes, base64url) and maps answers back; the id is saved with the
  outbox entry, so it survives a restart of the app. The page never
  chooses one.

#### The WebView: CSP, Trusted Types, no network

- The page is served from the app bundle by the shell: iOS a
  `WKURLSchemeHandler` on `rlord-app://page/`; Android a
  `WebViewAssetLoader` on `https://appassets.androidplatform.net/`. Only
  the files of `dist-app/` (an asset table generated at build time, with
  types); anything else is 404.
- Response header on every file (and the same policy as a `<meta>` in
  `app.html`, for the directives a meta may carry): `default-src 'none';
  script-src 'self'; style-src 'self'; img-src 'self' data:; font-src
  'self'; connect-src 'none'; base-uri 'none'; form-action 'none';
  frame-ancestors 'none'; require-trusted-types-for 'script';
  trusted-types 'none'`. No worker and no WASM, so no
  `'wasm-unsafe-eval'`, no `worker-src` and no Trusted Types policy at
  all. `style-src` drops `'unsafe-inline'` if the phone page works
  without it (the local page needs it for xterm.js, which the phone page
  doesn't have); kept only if step 1 shows Preact needs it.
- The WebView can load nothing from the network, in depth: iOS a
  `WKContentRuleList` that blocks every URL not on the app scheme, and
  the navigation delegate cancels every navigation but the first load
  and same-document (hash) changes; Android `blockNetworkLoads = true`,
  `shouldInterceptRequest` answering everything itself,
  `allowFileAccess` and `allowContentAccess` off, Safe Browsing off and
  the WebView metrics opt-out set (both would talk to a third party
  about a page that is only local files). A link in chat text opens
  nothing in the WebView; it is copied or handed to the system browser
  after a confirmation.
- JavaScript is on; nothing else is: no file pickers, no geolocation, no
  media capture in the WebView (the camera is the native scanner's), no
  `window.open`. Web inspection (`isInspectable`,
  `setWebContentsDebuggingEnabled`) in debug builds only.
- Chat and session text is rendered as text nodes. No markdown-to-HTML,
  no `dangerouslySetInnerHTML` (a lint rule on the UI package), so
  `trusted-types 'none'` holds.
- **One network path in the whole app: the engine's relay link.**
  `cargo xtask check-deps` fails if anything under `apps/` names
  `URLSession`, `NWConnection`, `CFNetwork`, `java.net`, `okhttp`,
  `HttpURLConnection` or a socket API (`NWPathMonitor` and
  `ConnectivityManager` are allowed: they only observe). App Transport
  Security and Android's network security config allow cleartext to
  `127.0.0.1` in debug builds only (a loopback relay in tests).

#### Pairing on the phone

1. First launch (or after unpairing): one screen. "On your computer run
   `rlord relay pair` (or open Devices in rlord), then scan the code."
   One button: Scan.
2. The native scanner (iOS `AVCaptureMetadataOutput`; Android CameraX
   with ZXing) accepts only a string starting `RLORD1:`; the engine
   parses it (`QrPayload`). The camera permission is asked here, with
   the reason in words. Debug builds also take the string by paste or a
   launch argument (simulators have no camera).
3. Before anything is sent the phone shows what it read: the relay's
   origin ("this computer is reached through relay.example.com") and a
   name field (the phone's model name as the default, 40 characters).
   Continue.
4. The engine makes the key set, dials the QR's relay as role `pair`,
   checks it against the QR's pin, runs `IKpsk2`, and sends its name and
   relay public key. The page shows the six digits: "Check that your
   computer shows 123456, and confirm there." The host waits 120 s.
5. `done`: the engine saves the host (static key, pin, relay key
   fingerprint, name) and the keys, opens its first live session, keys
   the mailbox, and the page goes to the list. `rejected`, a timeout, a
   spent or expired QR, or "wrong relay": back to step 1 with the reason
   in one line; the keys made in step 4 are deleted.
- **Unpair** (the status screen): the engine sends the new op
  `device.leave` when it has a live session (the host removes this
  device and updates the relay's allow-list), then deletes the host
  record, both keys and the sealed file. Without a session it deletes
  locally and says to revoke the phone on the computer too.
- **Revoked or expired** (close code 4403 on a `device` link): final.
  The page says the phone is no longer paired with that computer and
  offers to pair again; the old keys are deleted.

#### The reconnect loop (`machine.rs`)

The engine wants a relay link whenever the app is in the foreground and
the phone has a network, and a live session whenever the host is online.
Every requirement recorded for this slice ("Owed after slice 3";
`e2e-protocol.md` §4, Device) and where it lands:

| Requirement | In the machine | Named test |
| --- | --- | --- |
| Reconnect in a loop with backoff | 250 ms doubling to 30 s, each with up to 20% jitter; reset by a link that stayed `ready` 10 s | `backoff_grows_and_resets` |
| At once on 4205, backing off if it repeats | the first 4205 redials with no wait; another within 60 s takes the backoff | `reset_reconnects_at_once`, `repeated_reset_backs_off` |
| On a network change | `network(up)` or a changed path closes the link and dials at once; `network(down)` stops dialling | `network_change_redials`, `offline_stops_dialling` |
| On coming to the foreground | `foreground` dials at once and resets the backoff; a link found silent is replaced | `foreground_dials_now` |
| `KKpsk2` each time the host comes online | on `ready.host = online` and on `host online`; never reuses a session | `kk_on_host_online`, `no_session_reuse_after_reconnect` |
| Compare the snapshot's `rev`, fetch when it differs | the engine keeps the last `rev`; `hello.rev` and `changed` go to the page, whose store asks `snapshot {rev}` as it does locally | `rev_differs_fetches`, `rev_same_asks_nothing` |
| A device-unique id per request | 16 random bytes, saved with the entry before it is sent | `ids_survive_restart` |
| Resend unanswered requests by id after any reconnect | every outbox entry without an answer goes out again, same id, oldest first, on the next live session | `resend_by_id_after_reconnect` |
| When the relay's `boot` changes, resend every unanswered mailbox request | compared on every `ready`; each is encrypted afresh under the same request id | `boot_change_resends_mailbox` |
| Answer `queued` / `refused`; a refused frame again later, before newer ones | one mailbox frame out at a time; the next leaves after `queued`; `refused full` is sent again (the same bytes) on the next link and every 30 s; `no_queue` fails the request as `host_offline`; `unknown` or `bad` discards the mailbox | `mailbox_one_at_a_time`, `refused_full_resent_first`, `no_queue_fails_request` |
| Drop mailbox frames by `(boot, seq)` | saved with the state; a frame already opened is acknowledged and ignored | `redelivered_frame_ignored` |
| Drop answers by request id it already has | an answer for an id not in the outbox is ignored | `duplicate_answer_dropped` |
| `in_progress` means wait | the entry stays, marked running; no resend on this link; the answer follows | `in_progress_waits` |
| `interrupted` means check, then ask anew | the entry ends with code `interrupted`; the page says it may not have happened and offers to send again (a new id) after the user has looked | `interrupted_surfaces_once` |

The rest of the device's side, decided here:

- **Final close codes** (4401, 4402, 4403, 4426) and "wrong relay" stop
  the loop until the user acts: `revoked` (4403), `suspended` (4402),
  `update_app` (4426), `wrong_relay`. No other address is ever tried;
  the engine builds exactly one URL, from the pin.
- **`bye {retry_after}`** is honoured with jitter; 4429 and 4503 take the
  backoff; 4409 (replaced by a newer connection of this identity) waits
  the backoff and does not fight; `enroll` is shown and waited on.
- **Silence.** The engine pings every 15 s and gives a link up after 45
  s without any message (the host's numbers). Like the host it compares
  the wall and monotonic clocks every second and redials at once after a
  10 s jump: a phone sleeps more than a laptop.
- **A session that won't start.** No message 2 within 10 s, or
  `undeliverable`, retries the handshake on the same link after 1 s
  doubling to 30 s; it doesn't redial the relay.
- **Any decrypt failure** poisons the session (the core does it); the
  engine closes the socket and redials. An unknown text message is
  skipped; a binary frame it can't parse closes the link.
- **The mailbox.** Keyed on the first live session and again whenever
  there is none or the host's `hello` names another. Advance, seal and save before a frame leaves; on
  receipt: check `(boot, seq)`, the mailbox id, decrypt (a failure
  discards the mailbox), record the answer, save, then `ack`. A
  discarded mailbox is keyed again on the next live session and the
  unanswered requests go out again by id.
- **What may wait in the outbox.** Only `overlord.ask` and
  `overlord.new_session` are accepted without a live session (they
  travel by the mailbox, or live once the host is back). Every other op
  needs a live session when it is sent, else it fails at once
  (`disconnected` or `host_offline`). Once sent, an op that changes
  something stays in the outbox until answered and is resent by id for
  2 minutes; after that it ends as `interrupted` (the same "check, then
  ask anew"). A look-up in flight when the link dies fails with
  `disconnected`, as on the local page; the store asks again on `hello`.
- **Pending messages** (open question 11): an unanswered ask is shown in
  the chat as pending, with its age and Cancel (which removes it from
  the outbox; what already reached the host can't be recalled). The
  engine stops resending one after 24 hours and marks it failed. At most
  16 entries; a 17th ask is refused with a clear message.
- **Bounds.** At most one resend pass per link; `boot` changes and
  signals from the relay can't make the device send faster than its
  backoff; 20 ops per second is the host's limit and the page stays far
  under it.

#### Foreground, background and push

- **Foreground:** a relay link and a live session, `changed` events,
  the page as on a desk. On `ready` with `signal` set the engine clears
  it (`ack_signal`) once the page has fetched.
- **Going to the background:** the shell asks the OS for the short
  finishing time it gives, the engine sends what is queued, then closes
  the link cleanly (1000). No socket is held in the background and no
  foreground service runs on Android. The host sees the session end and
  goes back to signalling the relay.
- **Push is not built in this slice** (open question 5). Evaluated
  against the invariants:

  | | Poll in the foreground only | OS background refresh + local notification (chosen for this slice) | APNs / FCM wake-up (a later slice) |
  | --- | --- | --- | --- |
  | "Anything for me?" when the app is closed | no | sometimes: when the OS grants a refresh (its choice; minutes to hours apart) | yes, promptly |
  | Third parties | none | none | Apple or Google see a wake-up for this install each time the host's state moves; the relay holds the push token |
  | Everyone dials out | holds | holds | holds for host and device; **the relay gains outbound connections** (to `api.push.apple.com`, FCM) and credentials |
  | One pinned relay | holds | holds | holds only if the wake-up carries nothing (no address, no content) and the app answers it by dialling its pinned relay; the token is registered over the pinned link |
  | Self-hosted relay | works | works | a store app's push credentials are the publisher's: a self-hosted relay needs a project-run push gateway (a fourth party), or the self-hoster builds the app themselves |
  | Needs | nothing | nothing | a paid Apple developer account and the push entitlement; a Firebase project and Play Services on Android; a relay-link message for the token; relay code, config and docs; all of it before the protocol review |

  Push wake-ups don't break either invariant if built contentless, but
  they add a third party that learns when the user's agents are active,
  make the relay an HTTP client with secrets, and can't be finished for
  self-hosters without a gateway. None of that can be released from a
  dark branch, and Android's needs Google's libraries in the app. So:
  not now. The earlier decision ("APNs/FCM wake-ups", contentless)
  stands as the direction; it gets its own slice with the hosted relay,
  designed once the app is in daily use. This slice leaves room: the
  relay's `signal` flag is already the thing a push would announce, and
  the engine's `background_check` is what a push would call.
- **Background refresh** (iOS `BGAppRefreshTask`; Android `WorkManager`,
  periodic, network required): the OS calls
  `Engine::background_check(budget)`. One attempt, no loop: dial, the
  cheap check (`ready`'s `signal`, `queued`, `boot`); nothing waiting
  ends there with no handshake. Otherwise `KKpsk2`, `snapshot`, deliver
  any mailbox answers, clear the flag, close. The engine counts sessions
  whose `shown_status` is `waiting_input` that were not in the count it
  last notified, and asks the shell for a **local** notification:
  "rlord: 2 sessions need you". A count only by default (open question
  6); it is made on the phone, so no third party sees it. The
  notification permission is asked the first time the user turns
  notifications on in the status screen, not at launch.

#### What the phone shows, and how the page adapts

- **Ops**: the remote column as it stands (`snapshot`, `session.get`,
  `view.default`, `overlord.*`, the `session.*` actions, `session.new`,
  `session.fork`, `form.defaults`, `form.repos`), plus the additions
  under "Daemon and plugin changes". Never `form.setup*`,
  `overlord.setup_request`, `keys.list` or `device.*` management.
- **Views** (the one-column layout the page already has under 900 px):
  - *List*: sessions and groups tabs, the filter box, the usage column
    dropped at phone width, a New button (the `n` / `N` keys have no
    keyboard to live on): describe by default, manual from inside it.
  - *Overlord*: a **chat view** (`views/Chat.tsx`): `overlord.messages`
    (paged backwards on scroll), a composer that sends `overlord.ask`,
    pending and failed bubbles from the `outbox` event, the status and
    agent in the header, Start when it isn't running. Refreshed on
    `changed`, and every 3 s while the overlord is `working` and the
    chat is on screen (a look-up; `rev` doesn't move for every line).
  - *Session*: header, description, notes, tags, the action buttons
    (resume, restart, stop, archive, rename, describe, notes, tag, pin,
    fork), and the session's recent messages read-only (open question
    8). Where the pane would be: "The live terminal isn't available
    remotely yet."
  - *Forms*: New session (describe, then confirm; and manual) and Fork,
    as full-screen sheets. Code folder, Keys and Devices are absent.
  - *Status* (new, phone only): the host's name, the relay's origin, the
    connection state in words, the device's id and name, key storage
    (hardware or software), notifications, app lock, Unpair, versions.
- **Phone details** (CSS and a little page code, no native UI):
  `viewport-fit=cover` and the safe-area insets; `100dvh`; the composer
  kept above the keyboard with `visualViewport`; inputs at 16 px so iOS
  doesn't zoom; touch targets at least 44 px; nothing that needs hover;
  light and dark from the system. The WebView fills the screen; the
  system back gesture and Android's back button walk the page's hash
  history.
- **The local page gains nothing visible.** `Chat.tsx` is reachable
  there only in tests (the same component, a fake `Requester`).

#### Daemon and plugin changes

- **The phone becomes a surface** (open question 7; slice 2 left room,
  slice 3 left it to this slice). Without it a describe form sent from
  the phone is answered on the host's screen, where the user isn't.
  - `ui::target` gains `Target::Device(id)`; `pick` takes devices beside
    pages, with the same rule: the surface the user last did something
    on. A device's last-used time is the daemon's clock when a
    user-driven remote op arrives from it (the same list of ops that
    don't count) or a `page.report` with `input`.
  - `page.report` and `page.pending` become remote; for a device the
    "page" is the device id the plugin stamps (the device can't choose
    it). The daemon keeps the same record per device (at most 16).
  - `ui` lines gain `"to":"device","device":"<id>"`; the plugin sends
    `{"event":"ui","cmd"}` on that device's live session. With no live
    session the command is parked for that device (10 minutes, one slot)
    and the plugin sends the relay a `signal` for it; `page.pending`
    hands it over at the next `hello`. Not by the mailbox (slice 2
    thought it would be): parked and pulled is less to get wrong.
  - On a device the page handles `goto`, `show_list`, `set_view`,
    `new_session`, `confirm_new_session` and `notice`. A tool that needs
    a terminal or a local page (`confirm_setup`, `open_keys`, the
    overlay) never targets a device: it takes the next surface or fails
    naming them. Tool answers say `"on": "phone"`; `get_ui_state` gains
    `target: "phone"` and a `phone` object shaped like `web`.
- **`hello` gains `"api": 1`**, the remote API level (ops, snapshot and
  `ui` shapes as a device sees them). The app is built on its own
  schedule and meets a plugin of another version: the engine supports a
  range and says `update_app` or `update_host` outside it. Within a
  level, changes are additive only (a daemon test pins the remote
  snapshot's keys against a fixture).
- **`device.leave`** (remote, needs an id): the calling device removes
  itself. The plugin does what `revoke` does for that one device. From a
  local page it is `bad_request`.
- **`session.messages {ref, before, limit}`** (both, idempotent; open
  question 8): `overlord.messages` for any session, the same bounded
  read of its transcript by plain code (50 messages, 4 KiB each).
- `plugin::WEB_PROTOCOL` becomes 4. `proto.ts` `OPS` gains the new ops
  with their tests.

#### The overlord

It gains a surface, so `overlord.md` changes in the same change: the
phone is a third screen a targeted tool may land on (`"on": "phone"`),
what can't open there (the code-folder form, the key screen, the
overlay), and that a request from the phone is answered on the phone.
Agent-neutral wording, a few lines. `mcp.rs`: the `get_ui_state` and
targeted tools' descriptions name `phone`. No new MCP tool; devices and
pairing stay invisible to it. The existing tests (every tool mentioned,
none stale) cover it.

#### Build tasks (`cargo xtask app …`, `xtask/src/app.rs`)

| Task | Does |
| --- | --- |
| `web build --app` | the phone page into `crates/web/ui/dist-app/`, and its asset table |
| `app bindings` | UniFFI's bindgen **in process** (`uniffi_bindgen` as a library, pinned beside `uniffi` in `Cargo.lock`; no tool download), Swift into `apps/ios/Generated/`, Kotlin into `apps/android/app/build/generated/uniffi/` |
| `app core-ios` | `cargo build --release -p robot-overlord-e2e-ffi` for `aarch64-apple-ios` and `aarch64-apple-ios-sim`, then `xcodebuild -create-xcframework` into `apps/ios/Frameworks/` |
| `app core-android` | the same for `aarch64-linux-android`, the linker set from `ANDROID_NDK_HOME` by xtask (no `cargo-ndk`), into `apps/android/app/src/main/jniLibs/arm64-v8a/` |
| `app ios [--sim \| --device <id>]` | page, bindings, core, `xcodebuild` (Debug), then install and launch (`simctl` or `devicectl`) |
| `app android [--emulator \| --device <serial>]` | page, bindings, core, `./gradlew assembleDebug`, `adb install -r`, launch |
| `app dev` | a local relay, a dev daemon with the plugin, and `rlord-device bridge`, for working on the phone page in a desktop browser |
| `app test-e2e`, `app test-ios`, `app test-android` | under "Tests" |
| `install-tools --app` | the Android command line tools, SDK platform and NDK, by version and checksum |

The Xcode and Gradle builds never call these tasks; they consume what
the tasks left. Pure logic in the tasks (target names, paths, the
asset table) gets unit tests.

#### Installing on the user's own phone, without a release

- **iOS:** Xcode installed and signed in; the phone in Developer Mode
  and trusted. `apps/ios/Local.xcconfig` (git-ignored; a committed
  `.example`) holds the team id and the bundle id. `cargo xtask app ios
  --device <id>` builds, signs with automatic provisioning and installs
  with `devicectl`. A free Apple ID's profile lasts 7 days (run the
  task again; Keychain items outlive a reinstall, so the pairing should
  too, checked in step 10); a paid account's lasts a year (open question 3).
- **Android:** USB debugging on; `cargo xtask app android --device
  <serial>` installs a debug build signed by the local debug keystore.
  Reinstalling over it keeps the pairing.
- The app's version is the workspace's. Nothing is uploaded anywhere.
- The relay for this is the user's own (slice 3's by-hand pass, still
  owed, comes first); there is still no default or project-run relay.

### Tests

Never `tmux -L rlord` or the default server; no real agent; no network
beyond loopback; every relay in an automated test is the real
`rlord-relay` on `127.0.0.1:0`.

- **The machine** (`crates/e2e-client`, unit, a scripted link and a fake
  clock): every row of the reconnect table by its name; the final close
  codes; `bye`; 4409; `enroll`; silence and a clock jump; a handshake
  that gets no answer; the mailbox's save-before-send (a "crash" between
  each pair of steps, then a restart from what was saved, never repeats
  or loses a request); the outbox bounds and the 24-hour stop; ids that
  survive a restart; the background check doing no handshake when
  nothing waits.
- **Against the real plugin, daemon and relay** (`crates/web/tests/
  remote.rs`, now through the engine; slice 3's tests pass unchanged
  first): the relay restarted (`boot` changes, queued requests resent
  and run once); the plugin restarted mid-request (`in_progress`, then
  the answer; `interrupted`); the host's `drop` (4205) and a stalled
  link; a full queue refusing a frame, then taking it; a connection cut
  at each step by a test proxy (`testing::Flaky`), with the request
  still answered once; a network that stays away, then returns; two
  engines with one identity (4409, no fight); `device.leave`; a `ui`
  command reaching the device live, and parked then pulled; the phone
  chosen as the target after it sent a form.
- **Hostile relays** (`crates/e2e-device/src/testing.rs` gains
  `Tamper`: a relay that holds the pinned relay's own key and forwards
  to the real one, so it passes the pin and can then misbehave). The
  cases still owed from slice 3, end to end: a revoked device let
  through (`KKpsk2` fails at the host); a `link` invented for a device
  not on the list; two devices' frames swapped; a mailbox frame
  replayed, and one skipped; a recorded pairing replayed after the
  rendezvous closed; `bye` `retry_after` and `enroll` on the client
  side (the plugin's and the engine's). New, aimed at the device: a
  relay that lies that the host is online (no spin: the handshake
  backoff); one that changes `boot` on every connect and one that
  signals constantly (the device sends no faster than its bounds); one
  that never answers `queued` or `refused` (a timeout, a redial); a
  replayed message 2; garbage binary frames; old mailbox frames
  replayed to the device (the mailbox is discarded and keyed again, no
  answer accepted twice).
- **Daemon** (`web_api.rs`): `page.report` and `page.pending` from a
  device; `pick` with devices; a device never targeted by a
  terminal-only or local-only tool; `device.leave` only removes the
  caller and is refused locally; `session.messages` bounded; `api` in
  the plugin's hello path; the remote snapshot's keys against the
  fixture; `the_pages_ops_are_the_daemons` still holds.
- **Page** (Vitest): `RemoteTransport` against a fake bridge (requests,
  events, a reload restoring pending bubbles from `outbox`);
  `LocalTransport` keeps `api.test.ts`; `caps` hide what a device can't
  call; the chat view against a daemon-written fixture
  (`overlord_messages.json`); the pairing and status screens by event
  sequence; the `ui` commands on the phone entry.
- **The phone page's bundle** (a Rust test beside the asset table):
  every file local, no URL with a network scheme in the built output,
  the CSP's `connect-src 'none'` and `trusted-types 'none'` present,
  no xterm.js and no `LocalTransport` in it.
- **Scripted end to end, no simulator** (`cargo xtask app test-e2e`): a
  real relay, a dev daemon with the plugin, and `rlord-device bridge`
  (the engine running natively, offering the bridge's JSON on a loopback
  websocket: a debug-only stand-in for the shell). A Vitest project runs
  the phone page's own store and `RemoteTransport` over it: pair (the
  task confirms on the host with `rlord relay confirm`), list, ask the
  overlord, the relay killed and restarted, the plugin killed and
  restarted, unpair. Runs in CI's `web` job.
- **Simulators** (local only; no macOS CI minutes on a dark branch):
  - `app test-ios`: XCTest for the vault and signer wrappers (the
    software fallback), the scheme handler's asset table and headers,
    and the bridge; one XCUITest that launches with a debug QR argument
    against the same local relay and host (the simulator shares the
    Mac's loopback): pair, see a fixture session, ask, send the app to
    the background and bring it back (a fresh session), stop the relay
    and start it (recovers), a web console check that a `fetch` from
    the page is blocked.
  - `app test-android`: the same on an emulator (`adb reverse` for the
    loopback relay), instrumented tests for Keystore wrapping and
    signing, the asset loader and the message listener.
- **`check-deps`**: the new deny-list names, the engine's tree, and the
  app source check, each with a planted-name test.
- **By hand, on real phones** (hardware keys exist only there): enclave
  and StrongBox key creation and signing accepted by a real relay;
  wifi to cellular and back; airplane mode; a long lock; the app killed
  mid-ask; a background refresh producing a notification; app lock;
  revoke from the host while the app is open and while it is closed;
  the handshake and first snapshot's time on cellular, recorded here
  (with real snapshot sizes, still owed to the compression question).

### Implementation steps

For an Opus implementer at high effort. One commit each; the PR bumps
the version to the next `-dev.N` and regenerates `Cargo.lock`; nothing
is released. Record each step in "Progress". Steps 1 and 9 onward need
Xcode; 12 onward the Android SDK.

1. **Gates, before anything is built on them** (scratch code, not
   kept). (a) `rlord_e2e`, `relay-proto` and the link's TLS stack
   (rustls with ring, tokio-tungstenite) build for the three mobile
   targets. (b) UniFFI 0.32.2: bindgen in process from xtask; a foreign
   trait called from Rust and a Rust object called from Swift (simulator)
   and Kotlin. (c) A Secure Enclave signature (on the user's phone) and
   an Android Keystore one, converted to `r || s`, pass
   `relay-proto`'s check; if the verifier wants low-S, normalise in the
   engine. (d) `WKWebView` loads a built Preact page with module scripts
   from the custom scheme under the policy above (`'self'` on a custom
   scheme, `connect-src 'none'`, `trusted-types 'none'`, no
   `'unsafe-inline'` styles) and the bridge round-trips; the same in
   Android's WebView. (e) A simulator and an emulator reach a relay on
   the Mac's loopback. A failed gate is reported to the user with the
   fallback under "Risks" before going on.
2. **`crates/e2e-client`**: `machine.rs` with the table's tests, the
   outbox, storage with the `Vault` trait, the `Signer` trait
   (`relay-proto`'s transcript signing takes a signer, not only a seed),
   `driver.rs`.
3. **`e2e-device` on the engine**: the CLI and the test helpers call
   it; slice 3's `remote.rs` and `pin.rs` pass unchanged; then the new
   integration tests, `testing::Flaky` and `testing::Tamper` with the
   owed hostile cases.
4. **Daemon and plugin**: `api` in `hello`, `device.leave`,
   `session.messages`, the device as a surface (`ui.rs`, `web.rs`, the
   plugin's session and parked slot), `WEB_PROTOCOL` 4, `overlord.md`,
   `mcp.rs`, tests.
5. **Page, part 1**: `Transport`, `LocalTransport`, `caps`; no visible
   change to the local page; tests.
6. **Page, part 2**: `RemoteTransport`, the `app` entry and build
   (`web build --app`), the chat view, the session messages, pairing
   and status screens, the phone CSS, the bundle test.
7. **`rlord-device bridge`, `cargo xtask app dev` and `app test-e2e`**;
   CI's `web` job runs the last.
8. **`crates/e2e-ffi`** and `app bindings`; `check-deps`.
9. **iOS shell**: `app core-ios`, the Xcode project, the scheme
   handler and content rules, the bridge, the enclave `Signer` and
   `Vault`, the scanner, lifecycle and `NWPathMonitor`, app lock.
   `app ios`.
10. **iOS tests** (`app test-ios`) and the install on the user's phone;
    the by-hand list on iOS.
11. **Background refresh and local notifications** (engine
    `background_check`, iOS task).
12. **Android shell**: `install-tools --app`, `app core-android`, the
    Gradle project, the asset loader and WebView settings, the message
    listener, Keystore `Signer` and `Vault`, CameraX with ZXing,
    lifecycle and the network callback, app lock, WorkManager. `app
    android`.
13. **Android tests** (`app test-android`), the install, the by-hand
    list on Android.
14. **Docs**: `docs/architecture.md` "Remote access" gains "The phone
    app" (the engine, the bridge, the one network path, what is stored
    on the phone, the phone as a surface); `docs/guide.md` (pairing a
    phone, what it can do, unpairing, revoking a lost one);
    `SECURITY.md` (what a stolen phone can do, what the app keeps, no
    third parties, no push yet); `CLAUDE.md` (the app's network rule,
    how a page feature reaches the phone, the `api` level);
    `CONTRIBUTING.md` (the app toolchain and tasks);
    `e2e-protocol.md` §4's device part as built (still to be reviewed,
    then promoted); `e2e-crypto.md`'s phone row and answer 3; this doc.

Steps 2 and 3 need 1(a); 4, 5 and 6 need nothing native and can run
beside 2 and 3; 7 needs 3 and 6; 8 needs 2; 9 needs 6 and 8; 12 needs 6
and 8. Steps 12 and 13 can move to a later PR.

### Risks

- **Two native apps are a new kind of thing in this repo.** Swift,
  Kotlin, Xcode and Gradle projects, signing, simulators. Bounded by
  keeping the shells thin (no logic the engine or the page can hold) and
  the tasks in xtask; still the largest slice so far.
- **Nothing native can be built today.** No Xcode, no Android SDK on
  the build machine; hardware keys can only be tested on the user's
  phone, by hand.
- **UniFFI in a first use.** Foreign traits called from a tokio thread,
  bindgen in process, a static library per target. Gate 1(b). Fallback
  if in-process bindgen is unusable: the `uniffi-bindgen` binary built
  from the workspace (`cargo run`), still pinned by the lockfile.
- **`'self'` and module scripts on a custom scheme in WKWebView.** Gate
  1(d). Fallback: name the scheme in the policy instead of `'self'`;
  last resort, a loopback HTTP server inside the app is **not** taken
  (a listener on the phone).
- **Hardware signatures.** DER, random nonces, possibly high-S. Gate
  1(c). Fallback: normalise; if the enclave can't be used at all, a
  software P-256 key wrapped by the vault key (weaker, stated).
- **The engine is one more implementation of the device's side** beside
  what slice 3's tests drove. Bounded by making `rlord-device` the same
  code, so there is one.
- **App and host on different versions.** The page in the app was built
  against one daemon and meets another. The `api` level and
  additive-only snapshots; a clear `update_app` / `update_host`, not a
  broken list.
- **A phone is easier to lose than a laptop, and it holds a shell.**
  App lock, hardware keys, nothing of the host's stored, revocation and
  the 90-day expiry. An unlocked phone in the wrong hands, inside the
  app-lock window, can still type to the overlord.
- **No push means "needs you" arrives late.** A background refresh is
  the OS's favour, not a schedule. The app is honest about it on the
  status screen. If that proves the feature that matters most, the push
  slice moves up.
- **The phone as a surface can target wrongly** (a form opening on a
  phone in a pocket). The same guard as slice 2: the notice on the other
  surfaces, and the answer's `"on"`.
- **Chat without a pane is thinner than the terminal.** Text only, 50
  messages of 4 KiB, no tool output. It is the chat-first scope; panes
  over the relay are the slice after.
- **Uncompressed snapshots on cellular.** Tens of kilobytes per fetch.
  `rev` keeps repeats off the wire; sizes are recorded in the by-hand
  pass for the compression question.
- **WebView differences.** WKWebView and Android's Chromium differ in
  keyboard, viewport and scrolling behaviour; the phone CSS is checked
  on both, by hand.
- **Private-CA relays.** The engine trusts `webpki-roots` only; the QR
  carries no CA. A phone can't reach a relay behind a private CA until
  that is designed (owed).
- **The protocol is still unreviewed** and now runs on a phone. Only
  the user's own relay and phone, until the review (slice 3 answer 8).

### Open questions for the user

1. **Where the phone's crypto and relay link live.** Planned: a native
   Rust engine in the shell (UniFFI) owns the keys, the relay
   connection, the Noise sessions, the mailbox and the reconnect loop;
   the WebView only draws and has no network access. This changes your
   earlier answer ("crypto runs as WASM there too"). Alternative: WASM
   in a worker of the page, with the shell keeping only keys at rest:
   less native code, but key bytes in the page's memory, no hardware
   relay key, no work in the background without a second client, and
   the recovery logic in TypeScript. Recommended: the native engine.
2. **Which phone first.** Planned: iOS first, then Android, both in
   this slice, Android's steps separable. Which phone do you carry?
   Recommended: iOS first; if you carry an Android phone, swap the
   order (the engine and the page don't care).
3. **Apple developer account.** A free Apple ID installs builds that
   last 7 days and can never have push; the paid program gives a year
   and is needed for push and any later distribution. Are you in the
   program, and under which team should dev builds be signed?
   Recommended: the paid team if you have one; the free one is enough
   for this slice.
4. **The shell.** Planned: hand-written Swift and Kotlin shells around
   the WebView. Alternative: Tauri 2 mobile (Rust in process, no UniFFI,
   one project), at the cost of its IPC and asset layer in the middle
   of a locked-down WebView and plugins still needed for the keystores.
   Recommended: hand-written.
5. **Push notifications.** Planned: none in this slice. The app checks
   when opened and when the OS grants a background refresh, and posts
   local notifications; APNs/FCM wake-ups (contentless, as you decided)
   become their own slice with the hosted relay, because they add Apple
   or Google as a party that sees when your agents are active, make the
   relay hold push credentials and dial out, need a paid Apple account
   and Google's libraries, and need a gateway for self-hosters.
   Alternative: build APNs now for your own relay and your own build
   only. Recommended: as planned.
6. **What a notification says.** Planned: a count ("2 sessions need
   you"), made on the phone. Alternative: session names on the lock
   screen, as a setting. Recommended: count only by default, names as
   an opt-in.
7. **The phone as a screen the overlord can target.** Planned: yes, by
   the same rule as web pages (the screen you used last), so a describe
   form sent from the phone is confirmed on the phone; the code-folder
   form, the key screen and the overlay never go there. Alternative:
   the phone offers only what needs no overlord UI command (chat,
   actions, the manual form). Recommended: yes.
8. **A session's view without its pane.** Planned: add a read-only
   `session.messages` (the session's recent conversation as text, like
   `overlord.messages`), so you can see what a waiting session asked;
   answering it goes through the overlord until panes arrive.
   Alternative: also a direct "send text to this session" op now.
   Recommended: reading yes, direct sending no (a second way to type
   into agents, before panes make it moot).
9. **App lock.** Planned: on by default, biometrics or passcode when
   the app opens after more than 5 minutes away, and the phone must
   have a passcode. Recommended: on by default, with a setting to turn
   it off.
10. **More than one computer per phone.** Planned: storage and keys are
    per host from the start, the app shows one host in this slice, a
    switcher later. Recommended: as planned.
11. **Messages typed while the computer is unreachable.** Planned: an
    ask stays pending (visible, cancellable) and is resent until
    answered; the app stops trying after 24 hours. Alternative: fail
    after the relay's 10-minute queue. Recommended: as planned.
12. **Unpairing from the phone.** Planned: a new remote op
    `device.leave` that lets a device remove only itself, so "Unpair"
    on the phone also clears it from the computer. Recommended: yes.
13. **Minimum OS.** Planned: iOS 26 and Android 12. Recommended: as
    planned, unless your phone is older.
14. **The app's name and identifier.** Planned: "rlord", with a bundle
    and application id under the project's domain; dev builds use
    whatever `Local.xcconfig` says. Which identifier do you want
    reserved? Recommended: decide before the first device install; it
    is awkward to change once keys are stored under it.
15. **Relays behind a private CA.** Planned: not reachable from the
    phone in this slice (public certificates only). Alternative: trust
    the phone's own certificate store (`rustls-platform-verifier`), so
    a CA profile installed on the phone works. Recommended: defer
    until someone runs such a relay.
16. **The review and the relay.** Slice 3's answers put an informal
    protocol review "before the phone slice ships" and a project-run
    relay "with the phone slice". Planned: with the branch dark, neither
    happens in this slice; you use your own relay and phone, the review
    happens before the final merge to `main`, and the hosted relay is
    planned with the production-relay and push work. Recommended: as
    planned.

### Slice 4 answers (user, 2026-11-23)

1. Crypto and the relay link: the native Rust engine (UniFFI) in the
   shell. This replaces the earlier "WASM in the page" answer for the phone.
2. iOS first.
3. The paid Apple developer team (`PL2X8AJK8A`, the one that signs the
   macOS Developer ID builds). No Apple Development certificate is on this
   Mac yet; Xcode automatic signing creates one on the first device build.
4. App identifier `com.robot-lord.rlord`.
5–16. The recommended defaults.

`xcode-select` now points at Xcode (the user ran `sudo xcode-select -s`).

## Slice 5 plan: live terminals on the phone

Planned 2026-11-24 (Fable planning agent), on `feat/web-ui-5`. Nothing
below is built. Goal: on the phone, every session and the overlord is
reached through its live tmux pane, over the relay, in a terminal view
with the phone's keyboard and a row of special keys; the phone's chat
path to the overlord is removed.

### What the user decided (2026-11-24, binding)

User, 2026-11-24, after the first real-iPhone run (paired through the
Tailscale Serve dev relay, CONTRIBUTING.md): the phone's core job is
good communication with the terminals, the overlord's included.

- The phone talks to every session through its tmux pane, as the
  terminal app and the desktop page do: live panes over the relay (today
  streams 1 and up close the link), a terminal view with the keyboard and
  a row of special keys.
- The overlord stays special on the phone and the desktop (its own
  place, its prominence), but on the phone it is reached through its pane
  too. The phone's separate message path to it (the chat view over
  `overlord.ask` / `overlord.messages`) is removed; a removal from the
  remote API bumps `web::API`. The desktop page's overlord is unchanged.
- The read-only transcript view (`session.messages`) is what the phone
  shows today instead of a pane; whether it stays (as an offline or
  slow-link fallback) is a planning question.
- Slice 4 merges first, as built; slice 5 is its own PR.
- Test setup built for it (`feat/web-ui-5`): `scripts/demo/demo.sh
  --real-claude [--web] DIR` runs the real Claude Code (pinned) on a fake
  local model (`cargo xtask fake-model`, `cargo xtask demo-claude`), so
  live panes can be tried against a real Claude terminal with no model
  calls. CONTRIBUTING.md "Demo and screenshots".
- Testing uses real terminals: automated tests around the real-Claude
  demo mode where feasible, plus by-hand phone checks through the
  Tailscale Serve dev relay.

Found on the real phone, fixed on `feat/web-ui-4`: the first device
install needs `-allowProvisioningDeviceRegistration`; the pairing screen
restyled (robot, centred); the web view's own scroll view rubber-banded
the whole app past a list's end. The demo gotcha: a `rlord-demo` tmux
server left by an older demo keeps that demo's environment, so a new
demo's stand-ins write transcripts into the old folder (stop old demos
with `scripts/demo/demo.sh --stop DIR` first).

Invariants this plan takes as fixed: the host never accepts an incoming
connection; one pinned relay per host, no redirects or fallbacks; no
network code in `rlord`; post-quantum hybrid handshakes; one websocket
link is one Noise session with implicit nonces, so delivery in a link is
lossless and in order and a reconnect is a fresh handshake; pairing,
confirming and revoking are user-only; the feature branch stays dark (no
release of any kind; the PR bumps the `-dev.N` version only).

No push-back on the decisions. Two consequences worth knowing, both
accepted by this plan: with `overlord.ask` gone from the remote API the
phone can no longer leave the overlord a message while the computer is
unreachable (only a described new session still waits in the outbox);
and the spike's worry that a terminal is cramped at phone width is now
ours to solve (font size, landscape, a compose line, under "Input").

What exists today (read from the code, for the implementer):

- **The local pane** (`crates/web/src/pane.rs`): one tmux client on a PTY
  per `/pane` websocket, attached to a throwaway view session grouped
  with the target (`tmux::view_command`: `status off`, key table
  `rlord-web`, `destroy-unattached on`). Binary frames are bytes both
  ways; text frames are `open`, `resize`, `close` in and `opened`,
  `closed {reason}` out. `Bridge` and `Pty` are private to that file.
- **The desktop terminal is xterm.js** (`@xterm/xterm` 6.0.0 with the fit
  and WebGL addons; `crates/web/ui/src/pane.ts`). `PaneSocket` already
  knows nothing of xterm.js: it writes to a `Screen` and is fed input.
  `mountPane` builds the terminal. Only `main.tsx` (through
  `views/panes.tsx`) imports it, and a bundle test checks the phone's
  bundle has no xterm.js.
- **Streams** (`crates/e2e/src/stream.rs`): a frame's plaintext is
  `stream id | flags | bytes`; stream 0 is control, flag bit 0 is "more
  follows", at most 16 KiB of plaintext per frame. The host
  (`remote/session.rs`) drops a link that uses another stream ("panes are
  not served yet"); the engine (`e2e-client/src/machine`, `open`) treats
  one as an error.
- **The host's queues** (`remote/outq.rs`): control first, then mailbox,
  then one frame per link in turn; 4 MiB per link, overflow drops that
  link; the writer paces at 90% of the relay's rate and burst (256 KiB/s
  and 1 MiB by default, times one plus the host's links). **Frames are
  encrypted when queued**, so a link's queue is strictly first in, first
  out: nothing can overtake inside it.
- **The relay** buffers 128 frames per device (`hub.rs`,
  `DEVICE_FRAMES`); a device that falls further behind is closed (4205)
  and the host told `undeliverable`. A binary frame is at most 20 KiB.
- **The bridge to the page** is JSON text both ways
  (`Engine::page_message`, `Listener::to_page`; iOS
  `callAsyncJavaScript` of one fixed function). The phone page's CSP:
  `script-src 'self'; style-src 'self'; connect-src 'none';
  require-trusted-types-for 'script'; trusted-types 'none'`.
- **Remote ops**: `web::OPS` with `remote` and `idempotent`; `web::API`
  is 1, pinned by `tests/fixtures/remote_api.json`; the engine supports
  `API_MIN..=API_MAX` (1..=1). Only `overlord.ask` and
  `overlord.new_session` wait in the outbox and travel by mailbox
  (`outbox::may_wait`, `mailbox::MAILBOX_OPS`).
- **Window size**: tmux's `window-size latest`. The TUI's overlord
  overlay pins the overlord's window to its popup (`tmux::resize_window`,
  which sets `window-size manual`) and hands it back on close
  (`auto_size_window`). The overlay is not a tmux client (it draws
  `capture_frame` and sends keys).

### Decisions

#### 1. Framing: pane streams inside the device's Noise session

A pane is a stream in the live session the device already has. No second
websocket, no second handshake, no relay change.

| | Streams in the one session (chosen) | A second relay link per pane | A relay-level "pane lane" |
| --- | --- | --- | --- |
| Relay | unchanged: it can't tell a keystroke from a snapshot | a device identity holds one link today (4409 replaces the older); needs a new role | new lane, new relay code and limits |
| Handshakes | none extra | one hybrid `KKpsk2` per pane open | none extra |
| Order between a pane's bytes and its `opened` / `closed` | given by the session: one ordered sequence | two sequences to reconcile | two |
| Fairness between devices | `outq`'s turn-taking and pacing as they are | more links per device, the host's rate scales with links | new rules |
| Cost | flow control and priority inside the link are ours | a busy pane can't delay control at all | the relay learns which traffic is a terminal |

- **Data frames.** Plaintext `stream | 0 | bytes`, stream 1 to 250 (251
  to 255 stay reserved), at most 16 382 bytes. Each frame stands alone:
  the "more" flag on a pane stream is a protocol error (the link is
  closed). Host to device the bytes are the view client's output; device
  to host they are input. A data frame for a stream that isn't open is
  decrypted and discarded (input in flight when the pane ended), never an
  error.
- **Opening is an op**, so the daemon's one allow-list decides it:
  `{"id","op":"pane.open","args":{"stream","target","cols","rows"}}`
  (`target`: `overlord` or a session reference; sizes 2 to 1000, the
  local socket's bounds). Answer `{"id","ok":{"stream","id","name",
  "overlord","input","control","cols","rows","window"}}` (`control`: this
  device holds the pane; `cols`, `rows`: the size its view is attached
  at, the window's when it only views; decision 3) or an error: `not_running`,
  `not_found`, `too_many`, `bad_request` (a stream already open, a bad
  size). Opening never starts anything, as locally.
- **The rest are events on the control stream** (they need no answer and
  must not spend the 20 ops per second):
  - device to host: `{"event":"pane","stream","type":"resize","cols",
    "rows"}`, `{…"type":"ack","bytes","frames"}` (cumulative totals the
    page has consumed), `{…"type":"close"}`.
  - host to device: `{"event":"pane","stream","type":"closed","reason",
    "message"}` with reason `ended` (the client exited: the session
    stopped, the overlord restarted), `closed` (the device asked),
    `revoked`, `overflow` (input the PTY didn't take) or `failed`.
- **Order does the hard part.** Control and data share the session's one
  sequence, so `opened` (the op's answer) is before the stream's first
  byte and `closed` is after its last. A device reuses a stream id only
  after it has seen that id's `closed`.
- **Flow control, end to end, by credit.** The host sends on a stream
  only while what it has sent and the device hasn't acknowledged is under
  **64 KiB and 48 frames** (`hello` says so: `"panes":{"max":2,
  "window":65536,"frames":48}`, additive). The page acknowledges what
  xterm.js has parsed (its write callback), every 16 KiB or 50 ms. Both
  limits matter: the relay closes a device that is 128 frames behind, and
  a stalled phone must never get there because of a pane. An `ack` past
  what was sent closes the stream.
- **Backpressure reaches tmux, and tmux coalesces.** A pane's PTY is read
  only when all three allow it: the device's credit, the link's queue
  being short (below), and the pane's own rate (decision 5). While they
  don't, the bytes stay in tmux. A tmux client that isn't being read
  stops being sent incremental output and gets one redraw of the current
  screen when it is readable again (`tty_block_maybe` in tmux's `tty.c`;
  checked in gate 1a). So a slow phone sees fewer, whole screens, the
  host buffers almost nothing, and nothing the host has read is ever
  dropped: the in-link rule holds without a queue that can overflow.
- **Priority: a busy pane never delays control by more than two frames.**
  Because frames are encrypted when queued, priority can't be done inside
  the link's queue. It is done before it: the actor encrypts a pane frame
  only while that link's queue holds under **32 KiB**
  (`OutQ::link_bytes`, and a wake when a link's queue falls below the
  mark). An answer, an `ack`-driven grant or a `changed` therefore waits
  behind at most 32 KiB, well under a tenth of a second at the paced
  rate. Between links the writer's turn-taking and pacing are unchanged,
  so one device's pane can't stall another device.
- **Input** needs no credit: it is small. The host keeps at most 64
  frames of input per pane for the PTY; more than that closes the stream
  (`overflow`), not the link. The page refuses a paste over 64 KiB (the
  local socket's frame limit).
- **Limits**: 2 open panes per device (one on screen, one overlapping a
  navigation), 8 remote panes in all, counted apart from the 32 local
  pane sockets.

**Reconnect: reopen, never resume.** A stream lives in one session and a
session in one link. When the link goes, the host ends that link's view
clients (their view sessions vanish, `destroy-unattached`); when the
device is live again the page opens the pane anew, with a new view
client, and tmux draws the whole screen for it before anything else. The
page resets its terminal at `opened`, as the local page does.

- No gaps and no duplicates are possible: nothing of a stream crosses
  sessions, and tmux serialises "the full screen, then what follows" for
  its own client.
- A `capture-pane -e` snapshot followed by a separately started byte
  stream was considered and rejected: the two are taken at different
  moments, so the seam needs sequence numbers we would have to invent,
  and the stream is a client's drawing, not the pane's output, so there
  is no offset to resume from. Attaching is the snapshot.
- To save a round trip after `hello`, the engine remembers the page's
  wanted pane and sends its `pane.open` right behind its own `hello`
  frame (the host handles them in order). Built last, measured in step 9.
- **A link that died silently** is the case a terminal makes painful
  (keys go nowhere for up to 45 s). New rule in the machine: when the
  page sent input and the link heard nothing for 5 s, ping at once; no
  message within 5 s more, redial. This also settles the "no answer
  watchdog" owed from slice 4 for the case that matters.

**Compression: none on pane streams in this slice.** tmux's output for a
client is already a diff of the screen; credit and tmux's coalescing
bound the volume; a keystroke's frame is dominated by the 32-byte
padding and the tag, not its content. Should measurements (step 9: bytes
per second of a real Claude Code pane idle, streaming and scrolling, and
what lz4 and zstd would save on a recorded stream) argue for it later,
it would be a flag bit on the frame (bit 1, rejected today), offered in
`hello`, with a fresh context per frame. A context that lives across
frames would work on a lossless link but is exactly the arrangement
where a size side channel is strongest, and it adds state to reset at
every open. A pane mixes secrets with text others wrote more than any
other content rlord carries, so if anything is compressed first it
should be snapshots, not panes. Stays provisional; the numbers are
recorded for that decision.

#### 2. The terminal on the phone: xterm.js, the desktop's

| | xterm.js 6 in the page (chosen) | Our own renderer | An emulator in the engine (Rust), a grid drawn by the page | A native terminal view (SwiftTerm) |
| --- | --- | --- | --- | --- |
| Fidelity for Claude Code's redraws | the emulator the desktop page already runs against the same view clients | a new emulator to get right | good emulators exist, but input encoding, mouse and modes are ours again | good |
| Shared with the desktop page | all of it | none | none | none; Android needs another |
| Third-party code in the app | `@xterm/xterm`, `addon-fit`, `addon-webgl`: already dependencies, bundled, no network | none | a Rust crate in the engine | a Swift package |
| CSP and Trusted Types | no `innerHTML` or other HTML sink in the 6.0.0 builds (searched); three `<style>` elements and one `style` attribute, all in the DOM renderer and the scrollbar, are refused by `style-src 'self'`; the WebGL renderer draws to a canvas and needs none | ours | fine | outside the page |
| Size | about 475 KB of script before compression, in the app bundle | small | bridge traffic per cell | none in the page |

- The phone page gains xterm.js with the **WebGL renderer only**. A lost
  WebGL context (iOS takes them from a backgrounded app) is answered by
  making a new addon, not by the DOM renderer. The CSP is unchanged.
- If gate 1c shows WebGL can't be relied on in WKWebView, the fallback is
  the DOM renderer with `style-src 'self' 'unsafe-inline'` (the local
  page's policy). The user agreed to that, for the phone page and only if
  WebGL proves unreliable (answer 4); the page still has no network and
  no HTML sink.
- Not loaded: the clipboard addon (a session must not write the phone's
  clipboard through OSC 52), link handling (the web view cancels every
  navigation anyway).
- Code shape: `pane.ts` splits. `term.ts` holds the xterm.js mounting
  (`mountTerm(el, link, opts)`, the theme, fit, WebGL with re-creation)
  against a small `PaneLink` interface (`connect`, `input`, `resize`,
  `visible`, `stop`, `state`); `pane.ts` keeps `PaneSocket` (the
  websocket, local only); `remotePane.ts` is the bridge's `PaneLink`.
  The local bundle still has no remote code and the phone's no websocket.
- Font: `ui-monospace, Menlo, monospace` at 12 px by default (about 54
  columns on a 390-point phone), with smaller and larger steps (10 to 16
  px) in the pane's menu, kept per device. If Claude Code's glyphs are
  missing in WKWebView (gate 1c), a monospace font is bundled
  (`font-src 'self'` already allows it).

#### 3. Control: one screen holds a pane, and the handoff is explicit

Revised after the user's answer 1 ("a 'take over from <location>' and a
corresponding 'currently controlled by <location>' view that basically
makes the bind explicit on both sides"). tmux's silent `window-size
latest` is replaced, for every surface, by a holder rlord keeps and
shows.

**The model.** Each running session's pane (the overlord's included) has
at most one **controller**: the screen it is laid out for and the only
one whose keys reach it. Every other screen showing it is a **viewer**:
it sees the pane as the controller sees it, with "Controlled by
<location>" and a **Take over** action. Taking over is one tap or key,
no confirmation; the pane is resized to the taker, and the previous
holder's screen turns into the viewer's, naming the taker.

| Surface | Its name on other screens | As controller | As viewer | Take over |
| --- | --- | --- | --- | --- |
| The terminal (rlord's TUI and its tmux client on the computer; the overlord overlay) | "<host name>'s terminal" | the window follows the terminal's size, as today | tmux shows the window padded or cropped; rlord's status row says "Controlled by <name> · <key> to take over"; keys don't reach the pane | a key (rlord's status row, and the overlay's own line) |
| A local page | "a browser on <host name>" | the window is pinned to the page's terminal size | the terminal is drawn at the window's size, scaled to fit; a bar above it | a button in the bar |
| A paired phone | the name given at pairing | pinned to the phone's size | as a page: the whole screen, scaled down, with the bar | a button in the bar |

**Where it lives: the daemon** (`crates/daemon/src/control.rs`, new). In
memory, per session id: the holder (`terminal`, `page:<id>` or
`device:<id>`), the holder's size, since when, and whether the holder is
away. A daemon restart forgets it (every pane is free again). Plain
code: no agent decides anything.

- Device names come from the plugin (the daemon holds no device list):
  the mirror the plugin already sends (`web.remote_state`) gains each
  paired device's id and name. Names only; no keys.
- **How each surface learns it.** Every session in the snapshot (and the
  overlord) gains `control`: `null`, or `{holder, by, name, away, cols,
  rows}` (`by`: `terminal`, `web` or `phone`). A change moves `rev`, so
  pages and phones hear `changed` as for anything else and compare
  `holder` with their own id from `hello`. The TUI reads the same field
  from the daemon as it reads the rest of a session: the list marks a
  session held elsewhere, the overlay and the status row show the line.
  The plugin is told on its `web.subscribe` stream (`{"control":{…}}`),
  because it enforces typing and sizes the viewers' clients.
- **Ops**: `pane.take {target, cols, rows}` (local and remote; running it
  twice is the same as once). The terminal's key runs `rlord control
  take <session>` (a daemon method over the socket, `control.take`).
  A view-only device (decision 6) is refused (`not_allowed`).
- Plugin-only daemon methods, replacing `web.pane_target`:
  `web.pane_open {target, surface, cols, rows}` (resolves the target,
  says whether this surface controls, and the size to attach at; **a
  free pane is taken by the surface that opens it**, since there is
  nobody to take it from), `web.pane_size {target, surface, cols, rows}`
  (the controller's terminal changed size: pin again) and
  `web.pane_closed {target, surface}`.

**How it is enforced in tmux.**

- **Every view client attaches with `ignore-size`** (local pages' too),
  so no browser or phone ever sizes a window by itself. Size comes only
  from the daemon: holder a page or a phone, the window is pinned to its
  size (`tmux::resize_window`, as the overlay does today); holder the
  terminal or nobody, it is unpinned (`auto_size_window`) and follows
  the terminal's own client, the only kind left that tmux counts.
- **Typing.** The plugin passes a pane's input on only for the
  controller's stream or socket; a viewer's input is discarded and its
  page doesn't send any (a key press there points at Take over). The
  terminal isn't in rlord's path (the user's tmux client types straight
  into the pane), and tmux can't make a client read-only and back. So
  while another surface holds a session, the daemon sets that session's
  `key-table` option to a table of rlord's (`rlord-held`): rlord's
  navigation keys, the take-over key, and `Any` bound to a message. View
  sessions have their own table, so only real terminals are affected.
  Gate 1b; the fallback is under "Risks".
- **The overlay** is the terminal surface. Holding the overlord, it pins
  the window to its popup as today. Not holding it, it pins nothing,
  draws the frame it captured as it is, shows the line, and sends no
  keys but its own (close, take over). `web.pane_claim`, the `fit` event
  and the bound no-op key of the first plan are gone.
- **The keyboard never resizes the terminal.** Rows are computed for the
  full view with the keyboard down. When the keyboard comes up the
  terminal slides up, its bottom rows above the key row (Claude Code's
  prompt is at the bottom); a drag shows the top. A new size is sent
  (`resize`, which pins again) only for a rotation or a font-size
  change, 200 ms after it settles.

**Opening, leaving, coming back.**

- Opening a pane another live surface holds opens it as a viewer. It
  never takes by itself.
- **When the holder leaves** (its stream or socket closes, the link
  drops, the phone goes to the background, the terminal moves to another
  session or closes the overlay) it is marked **away** and keeps control
  for 30 s: a network blip, or a look at the list and back, changes
  nothing and redraws nothing. Viewers see "Controlled by <name>
  (away)". After that it is released: if other surfaces are showing the
  pane, control passes to the terminal if it is one of them, else to the
  one used last (`ui::target`'s order); if none is, the pane is free and
  keeps its size until someone opens it. Explicit where two screens
  compete; automatic where nobody is left to ask. Open question 2.
- What rlord types by itself (the overlord's tools sending to a session,
  the desktop page's "tell the overlord" box) isn't a surface and is
  never held back.

**The overlord and the input rule.** The overlord sees and does nothing
here: no MCP tool, nothing in `get_ui_state`, so `overlord.md` is
unchanged (if a later change shows control to it, that change updates
it). "Describe, then confirm" doesn't apply: this is a one-tap action on
a pane, not config-style input.

#### 4. Input

- **Direct keys.** xterm.js's own hidden textarea takes the phone's
  keyboard (it sets `autocorrect`, `autocapitalize` and `spellcheck`
  off); a tap on the terminal focuses it. Every key goes to the pane at
  once.
- **The key row** (in the page, above the keyboard, one scrolling row,
  44-point targets): `Esc`, `Tab`, `⇧Tab` (`ESC [ Z`: Claude Code's mode
  switch), `Ctrl` and `Alt` (sticky for the next key), `←` `↓` `↑` `→`
  (application cursor mode respected), `⏎`, `^C`, Paste, the compose
  toggle, hide keyboard. The byte sequences are pure functions
  (`term/keys.ts`: `keyBytes(key, {ctrl, alt, appCursor})`,
  `withCtrl(data)`), unit-tested.
- **A compose line** (the toggle): an ordinary text field with the
  phone's autocorrect, dictation and emoji, sent as one paste
  (`term.paste`, bracketed when the pane asked for it) followed by Enter;
  a second button pastes without Enter. Direct keys suit `y`, arrows and
  `Esc`; a paragraph for an agent is typed far better in a real text
  field. Default: direct keys, the compose line one tap away.
- **Paste and copy.** The Paste key asks the shell (`{"app":"paste"}`,
  answered `{"event":"paste","text"}` from `UIPasteboard`, which shows
  iOS's own paste notice); the page then pastes it. "Copy screen" sends
  the visible buffer's text to the shell (`{"app":"copy","text"}`). The
  shell reads the pasteboard only for that message. Selecting text by
  touch inside a canvas terminal is poor: the transcript (decision 8) is
  the place to select and copy. If `navigator.clipboard` works on the
  page's scheme (gate 1d), the shell messages aren't built.
- **Scrolling back.** A vertical drag on the terminal becomes wheel
  reports (SGR `64` / `65`, one per row of drag) when the view client
  asked for the mouse, which rlord's tmux does by default (`[tmux]
  mouse`); the `rlord-web` table's copy of root's wheel binding scrolls
  the pane's history, as the desktop's wheel does. With the mouse off
  there is no scroll-back in the terminal; the transcript covers it.
- **The shell** (iOS): the web view's own accessory bar (previous, next,
  Done) is removed and smart quotes, dashes and autocorrection are turned
  off for the page's text input, both by overriding the content view's
  input traits (gate 1d decides how much is needed). Nothing else native.
- **Latency.** Target, key to echo on screen: half of presses under 100
  ms and nearly all under 250 ms on wifi with a nearby relay; half under
  250 ms on a mobile network. It is two round trips through the relay
  plus at most 15 ms of the host's coalescing, and an echo after a pause
  isn't delayed at all (below). No local echo or prediction in this
  slice. A debug read-out in the pane's menu shows the last key-to-echo
  time; step 9 records the numbers.

#### 5. Bandwidth and battery

- **Coalescing on the host.** The pane's task reads up to one frame
  (16 382 bytes). A read after 15 ms of quiet is sent at once; otherwise
  bytes gather until the frame is full or 15 ms have passed. A spinner
  becomes one small frame per tick, not one per write.
- **A rate per pane**: 64 KiB/s sustained, 256 KiB burst (constants in
  `remote/panes.rs`). A spinner is a few hundred bytes a tick and a
  streaming answer a few KiB/s; only a flood meets the limit, and what
  waits stays in tmux and arrives as one redraw.
- **Only what is on screen streams.** One pane per view; the page closes
  the stream when the view goes or the app is hidden. In the background
  the engine closes the link (as today), the host ends that link's view
  clients, and nothing runs. Back in front: dial, handshake, `hello`,
  open, one redraw.
- **Budget**: at most 2 streams of 64 KiB/s per device, against the
  relay's default 256 KiB/s per device. Control traffic keeps the rest.

#### 6. Security: typing in a pane is running code on the computer

That is already what a paired phone has (slice 4: "pairing a phone is
granting it a shell"; `overlord.ask` and `session.new` with a prompt both
run code). A pane makes it direct, so:

- **Any paired device may type, by default.** The gate is the pairing
  (the six digits confirmed on the computer), the app lock, hardware keys
  and revocation. No confirmation on the computer per pane or per key:
  the user isn't there.
- **View only, per device**: a flag on the device's record (`input:
  false`), set by the user on the computer: `rlord relay input <device>
  on|off`, the TUI's Devices screen, and the local page's Devices modal
  (a local-only op `device.input`). Never an MCP tool, never a remote op.
  Such a device is only ever a viewer: it can't take a pane (decision
  3), the host attaches its view client read-only, discards any input
  frame, answers `"input": false` in `pane.open`, and the page shows no
  keyboard and no Take over. New devices may type (answer 2).
- **`remote` flags**: `pane.open` is remote (and refused from a local
  page, which has its socket); `pane.take` is remote and local. `overlord.ask` becomes local-only;
  `overlord.messages` is removed. The local-only list is otherwise
  unchanged.
- **A device can reach only what the daemon resolves.** The target is
  `overlord` or a session reference, resolved by the daemon on every
  open; no tmux name comes from a device. The view session's key table
  has no keyboard bindings, so tmux's prefix, other windows and the
  user's profile are out of reach, as for a local page.
- **Audit** (`remote.log`): `pane open <session id> <cols>x<rows>
  rw|ro`, `pane take <session id>`, `pane input <session id>` (the first input of a stream), `pane
  close <session id> in=<bytes> out=<bytes>`. Never the bytes, as today's
  rule says.
- **Revoking a device ends its panes at once** (the link is dropped; its
  view clients are killed). So does the 90-day expiry.
- **What the phone keeps**: nothing. Screen contents live in the page's
  memory; the app switcher cover and app lock are slice 4's.
- Terminal output is untrusted text drawn to a canvas: no markup reaches
  the DOM, titles and OSC 52 are ignored, links open nothing.

#### 7. Remote API and protocol changes

- **`web::API` becomes 2** (a removal): `overlord.ask` and
  `overlord.messages` leave the remote API; `pane.open` and `pane.take` join it, and the
  snapshot's sessions gain `control`.
  `tests/fixtures/remote_api.json` is regenerated on purpose. The engine
  supports 2 only (`API_MIN` = `API_MAX` = 2): an older host gets
  `update_host`, an older app `update_app`. The app and the host are the
  user's own two builds; no compatibility code.
- `hello` gains `panes` (limits) and `input` (this device may type).
  Additive.
- **Mailbox**: `MAILBOX_OPS` and `outbox::may_wait` shrink to
  `overlord.new_session`. The outbox's 24-hour ask rule goes with the
  asks.
- `plugin::WEB_PROTOCOL` becomes 5 (`web.pane_open`, `web.pane_size`
  and `web.pane_closed` in place of `web.pane_target`, the `control`
  line on `web.subscribe`, device names in `web.remote_state`).
- **`e2e-protocol.md`**: §3 "Frames" (streams 1 to 250, no "more" on a
  pane stream, frames for a closed stream discarded) and a new "Panes"
  part (the ops, the events, credit, the limits, order, reopen on
  reconnect, controller and viewer); §3 "Requests by mailbox" (one op); §4 (the host reads a PTY
  only under credit, queue mark and rate; the input-stall rule on the
  device; panes end with their session); §5 (the pane methods and control).
- **Vectors** (`cargo xtask e2e vectors`): pane frame headers; a pane
  frame between two fragments of a control message; "more" on a pane
  stream refused; a reserved stream id refused. The crypto is untouched.
- `relay-proto` and the relay: no change.

#### 8. What the phone shows; what goes

- **The overlord keeps its place**: the first row of the list, and a
  button in the header from every screen, marked when it waits for
  input. It opens `#/overlord`: status and agent in the header, Start
  when it isn't running (`overlord.ensure`), and its terminal. The pane
  follows a switch to another agent's overlord, as the desktop's does.
- **A session** (`#/s/<id>`): three tabs. *Terminal* (first when it is
  running; with the control bar when another screen holds it), *Transcript*, *Details* (description, notes, tags, the
  action buttons). Not running: Resume, and the transcript.
- **The transcript stays: `session.messages`, read-only.** Recommended,
  for what a terminal on a phone does badly rather than as an offline
  fallback (it needs the same live session; the phone stores nothing):
  a stopped session has no pane to show; long history reads better as
  scrolling text than as tmux's copy mode under a thumb; text can be
  selected and copied; and it costs one bounded look-up, no stream. The
  overlord's transcript uses the same op with the overlord's id.
- **Removed**: the chat view (`OverlordChat`, the composer, the pending
  and failed bubbles), `overlord.ask` and `overlord.messages` as remote
  ops, and the daemon's `overlord_messages`. `views/Chat.tsx` shrinks to
  `views/Transcript.tsx` (the message list and `SessionText`); `chat.ts`
  to `transcript.ts` (`MessageLog`). A described new session still waits
  in the outbox when the computer is away: shown as one line on the list
  with Cancel, not as a chat bubble.
- **The desktop page's overlord is unchanged**: the pane and the "tell
  the overlord" box (`overlord.ask`, local). Its panes do gain the
  control bar of decision 3, as every surface does.
- **`overlord.md`**: the overlord sees nothing new (no tool, no state);
  what the phone types arrives as typing. Its lines about the phone as a
  surface stay true. Checked in step 4; the tests on its tool list cover
  the rest.

### Tests

Never `tmux -L rlord` or the default server; every relay is the real
`rlord-relay` on loopback; no model is ever called.

| Layer | Needs | Runs in CI |
| --- | --- | --- |
| Core, engine, page units | nothing | yes |
| Host, daemon, relay, engine with a stand-in terminal | tmux | yes (`RLORD_TMUX_TESTS=1`, as `web/tests/pane.rs`) |
| The real Claude Code through the relay | tmux, Node, the pinned CLI from npm (2.1.331, cached) | yes (answer 3) |
| iOS simulator | Xcode | no (local, as slice 4) |
| A real iPhone | the phone, the Tailscale Serve dev relay | by hand |

- **`crates/e2e`** (`stream.rs`): `pane_frames_stand_alone`,
  `pane_more_is_refused`, `reserved_streams_refused`,
  `control_fragments_and_pane_frames_interleave`; the vectors.
- **`remote/outq.rs`**: `a_links_queue_reports_its_bytes`,
  `the_low_mark_wakes_once`; **`remote/panes.rs`** (units, a fake PTY):
  `credit_stops_reading`, `ack_past_sent_closes_the_stream`,
  `a_quiet_pane_sends_at_once_and_a_busy_one_gathers`,
  `the_rate_bounds_a_flood`, `input_overflow_closes_the_stream_only`.
- **`crates/web/tests/remote_pane.rs`** (new; the real plugin, daemon and
  relay, the engine through `e2e-device`'s `phone.rs`, a scratch tmux
  server, the "agent" a small script): `a_pane_opens_and_carries_bytes`,
  `opening_starts_nothing`, `resize_reaches_the_agent`,
  `close_leaves_no_view`, `a_dropped_link_leaves_no_view`,
  `reopen_after_reconnect_draws_the_screen` (the screen parsed after a
  reconnect equals a fresh open's), `a_flood_never_drops_the_link` (a
  device that stops acknowledging: the host's queue stays under the mark,
  the link lives, the pane catches up with one redraw),
  `a_flood_never_delays_control` (a snapshot is answered promptly while a
  pane floods), `a_flooding_pane_never_stalls_another_device`,
  `a_view_only_device_cannot_type`, `a_view_only_device_cannot_take`,
  `a_second_surface_opens_as_a_viewer`,
  `take_over_resizes_and_swaps_typing`, `a_viewers_input_goes_nowhere`,
  `a_blip_keeps_control`, `control_passes_on_after_the_holder_leaves`,
  `revoking_ends_its_panes`,
  `two_devices_one_pane`, `too_many_panes`, `a_relay_restart_reopens`,
  `the_audit_log_names_no_bytes`.
- **`crates/web/tests/hostile.rs`**: a relay that replays, drops or
  reorders a pane frame ends the session (no bytes reach the PTY out of
  order); a relay that withholds a device's acks only slows that pane.
- **Daemon** (`web_api.rs`): `pane_open_from_a_device`,
  `pane_open_is_refused_locally`, `pane_take_from_a_device_and_a_page`,
  `overlord_ask_is_local_only`, `overlord_messages_is_gone`,
  `device_input_is_local_only`, the snapshot's `control` keys and the
  fixture at `api` 2. `control.rs` units (a fake clock, no tmux):
  `a_free_pane_goes_to_its_opener`, `an_opener_never_takes`,
  `take_swaps_the_holder`, `away_keeps_control_for_the_grace`,
  `release_prefers_the_terminal_then_the_last_used`,
  `nobody_left_means_free`, `a_restart_forgets`. `web_tmux.rs`:
  `a_held_window_is_pinned_and_views_never_size_it`,
  `the_terminal_holding_unpins`, `a_held_sessions_keys_reach_no_pane`,
  `the_take_key_takes`. `crates/web/tests/pane.rs`: a local page as
  viewer and as taker. **TUI**: the overlay as viewer (no pin, the line,
  the take key), the list's mark.
- **Engine** (`machine/tests.rs`): `pane_frames_go_to_the_page`,
  `page_input_becomes_stream_frames`, `long_input_is_split`,
  `pane_open_is_never_kept`, `a_stalled_link_under_input_redials`,
  `the_wanted_pane_follows_hello`, `api_1_host_says_update_host`,
  `only_a_new_session_waits`.
- **Page** (Vitest): `keys.test.ts`; `remotePane.test.ts` (a fake
  bridge: open, bytes, acknowledging at 16 KiB and 50 ms, `closed`,
  reopen on `hello`, a stream id reused only after its `closed`);
  `pane.test.ts` unchanged; the transcript's tests in place of the
  chat's; the bundle test turned round (xterm.js present, still no
  websocket and no `LocalTransport`); the control bar's states (holder,
  viewer, away) from snapshots.
- **The real terminal, automated** (`cargo xtask app test-term`, new,
  beside `test-e2e`): a demo with `demo-claude`, the loopback relay, a
  dev daemon with the plugin, `rlord-device bridge`, and a Vitest project
  that feeds a headless xterm.js (`@xterm/headless` 6.0.0, a dev
  dependency) through `RemotePaneLink` and reads the screen as text:
  - Claude Code's prompt is drawn at 54 columns;
  - a typed line and Enter bring the fake model's echo;
  - a prompt with "tool" brings the permission dialog; arrows and Enter
    through the key row's sequences accept it; `fake-model-hello.txt`
    exists;
  - `⇧Tab` changes the mode line;
  - `Esc` stops a slow answer (`--delay-ms`);
  - a resize, then a second fresh open, show the same screen;
  - the link cut mid-answer (`testing::Flaky`), then reopened, shows the
    same screen as a fresh open once the answer ended;
  - the compose line's paste arrives as one bracketed paste;
  - opened while the terminal holds the session, it is a viewer at the
    window's size; after `pane.take` Claude Code is laid out at 54
    columns and typing works.
  The fake model logs every request; the test fails on a `BLOCKED` line.
- **iOS** (`app test-ios`): `LivePageTests` gains the terminal mounting
  under the page's CSP with WebGL (and that the only policy violations
  are xterm.js's refused style elements); the XCUITest opens the
  overlord's terminal on the loopback stack, types through the key row,
  sees the echo (the page mirrors the screen's text into a hidden element
  in debug builds), goes to the background and back.
- **By hand, on the iPhone through the Tailscale Serve relay**, against
  `demo.sh --real-claude --web DIR`: a working session and the overlord;
  the keyboard (autocorrect, smart punctuation, dictation, a hardware
  keyboard); every key of the row; the compose line; paste and copy;
  scroll-back; rotation; font sizes; the desktop TUI and page on the same
  session (controlled-by and take over, each way, the overlay included;
  the phone leaving and coming back inside and after the 30 s); wifi to mobile and
  back, a tunnel, a long lock, background and foreground mid-answer; a
  flood (`cat` of a large file) on a slow network; view-only; revoke
  while a pane is open; key-to-echo times and bytes per second, recorded
  here.

### Implementation steps

For an Opus implementer at high effort. One commit each; the PR bumps
the version to the next `-dev.N`; nothing is released. Record each step
in "Progress".

1. **Gates** (scratch code, not kept; a failed gate goes to the user with
   its fallback before anything is built on it).
   - (a) tmux and an unread client: a view client whose PTY isn't read
     while the pane floods; tmux's memory stays bounded and the client
     gets one redraw when read again. On this Mac's tmux and CI's.
   - (b) control in tmux 3.2: view clients attached with `ignore-size`
     never size the window; `resize-window` pins it with views attached
     and unpinning hands it to the terminal's client; a session's
     `key-table` set to a table with `Any` bound keeps a real client's
     keys from the pane, keeps rlord's navigation, and the take key
     works; `read-only` for a view-only device's client.
   - (c) xterm.js 6 with WebGL in WKWebView under the phone's CSP, in the
     simulator and on the phone: draws, survives a lost context, Claude
     Code's glyphs.
   - (d) the iOS keyboard on xterm.js's textarea: letters, Enter,
     backspace, smart punctuation, the accessory bar, dictation; the
     clipboard path.
   - (e) the real Claude Code in a pane at 45 to 60 columns: usable, and
     what a change of size does to its screen.
   - (f) the bridge: 64 KiB/s of base64 through `callAsyncJavaScript`
     without dropped frames in the page.
2. **Core**: `stream.rs` (pane frames, the reserved ids), the vectors,
   the WASM vector runner, `e2e-protocol.md` §3. Proves the framing.
3. **Host**: `crates/web/src/bridge.rs` (`Bridge` and `Pty` moved out of
   `pane.rs`, `ignore-size` on every view, the read-only attach, input
   only for the controller; `pane.rs` on it); `remote/panes.rs` (a task per
   stream, credit, the queue mark, the rate, coalescing, input);
   `outq.rs` (`link_bytes`, the low mark); `session.rs` (data frames,
   the `pane` events, `pane.open` passed to the daemon and answered);
   `devices.rs` (`input`); `audit.rs` lines; `rlord-device pane` (a
   by-hand client: raw mode in a terminal). `remote_pane.rs` passes with
   the low-level device. Proves panes over the relay, flow control and
   priority, with no phone.
4. **Daemon and TUI**: `control.rs` and its tmux side (`rlord-held`,
   pin and unpin), `web.pane_open` / `pane_size` / `pane_closed`, the
   `control` line to the plugin and the snapshot field, `rlord control
   take`, the status row's line and key, the overlay as holder and as
   viewer, the list's mark; `web::OPS` (`pane.open`, `pane.take`,
   `overlord.ask` local, `overlord.messages` gone, `device.input`),
   `API` 2 and the fixture, `relay.rs` and the CLI's `rlord relay
   input`, the Devices screens, `MAILBOX_OPS`, `WEB_PROTOCOL` 5,
   `proto.ts` `OPS`. `overlord.md` checked (expected: no change). Proves
   the allow-list and the handoff with no phone. Steps 3 and 4 meet at
   the plugin's calls, so 3 starts against `web.pane_open` answering
   "you control".
5. **Engine**: the machine's pane passthrough, the input-stall rule, API
   2, `may_wait`; `rlord-device bridge` carries the new messages.
   `crates/e2e-ffi` and the Swift bindings don't change. Proves the
   device's side natively.
6. **Page**: `term.ts`, `remotePane.ts`, `term/keys.ts`, the key row and
   the compose line, the overlord and session terminal views, the control bar and viewer
   scaling (the local page's panes too), the tabs,
   `Transcript.tsx`, the chat's removal, the outbox line on the list,
   `REMOTE_CAPS.panes`, the bundle test. `app test-e2e` updated (its ask
   step becomes typing in the fake overlord's pane).
7. **The real terminal test**: `cargo xtask app test-term`, the headless
   xterm.js project, and CI's `web` job with the pinned CLI cached
   (answer 3).
8. **iOS shell**: the input traits and accessory bar, `paste` and `copy`
   if gate 1d needs them, `app test-ios` additions. Install on the phone.
9. **By hand and measured**: the list under "Tests"; key-to-echo times,
   bytes per second, the compression figures; the `pane.open`-behind-
   `hello` shortcut if the reopen time wants it.
10. **Docs**: `docs/architecture.md` ("Web UI" and "Remote access": panes, flow
    control, control and take over; "The phone app": the terminal, what went),
    `docs/guide.md` (the phone's terminal, the key row, "controlled by"
    and Take over on each screen, view-only devices), `SECURITY.md` (a paired phone types into any session;
    view-only; the audit lines), `CLAUDE.md` (the phone page has a
    terminal; `pane.open`), `CONTRIBUTING.md` (`app test-term`),
    `e2e-protocol.md` §4 and §5 as built, `e2e-crypto.md`'s compression
    note with the measurements, this doc.

Step 2 needs gate 1a; 3 needs 2; 4 can run beside 3; 5 needs 2; 6 needs
gates 1c and 1d and can start beside 3 against a fake bridge; 7 needs 3,
5 and 6; 8 needs 6. Android stays owed from slice 4; nothing here is
iOS-only but step 8.

### Risks

- **tmux's coalescing is the design's keystone.** If an unread client
  makes tmux buffer without bound, or close the client, on a tmux rlord
  supports (3.2 and up), the host must read and coalesce itself, which
  means a terminal emulator on the host or dropping bytes it has read.
  Gate 1a, first. Fallback: the host keeps reading and, past a mark,
  discards and asks tmux for a redraw (`refresh-client`) when the device
  catches up: lossless on the wire still, since only unsent bytes go.
- **WebGL in WKWebView** (lost contexts, memory on old phones). Gate 1c;
  the fallback loosens the phone page's CSP, as the user allowed.
- **The iOS keyboard on a hidden textarea**: smart punctuation,
  dictation and composition can send what the terminal doesn't expect.
  Gate 1d, the shell's input traits, and the compose line as the place
  for prose.
- **Claude Code at phone width.** It works narrow but wraps hard; its
  dialogs assume more columns. Gate 1e; font steps and landscape.
- **Control touches every surface.** The TUI, its overlay, rlord's
  status row, the local page and the phone all change, and local web
  views stop sizing windows by themselves (a visible change to slice 1's
  behaviour: a page opened beside a terminal is a viewer until it takes
  over). The state machine is plain code with unit tests; the tmux side
  is gate 1b.
- **Holding back a terminal's keys** rests on a session `key-table` with
  `Any`. If tmux 3.2 can't do that cleanly (mouse events, the prefix,
  rlord's own bindings), the fallback is honest rather than strict: the
  terminal always can type, the status row still says who controls, and
  the take key still takes. Pages and phones are held back by the plugin
  either way.
- **A terminal can't be scaled.** A viewer in a terminal sees a
  phone-sized pane padded, or a larger one cropped; only pages and
  phones get the whole screen scaled.
- **A take-over redraws** the agent at the taker's size. Once per
  handoff, on purpose.
- **Latency on mobile networks** is two relay round trips; no prediction
  softens it. Measured in step 9; local echo is a later slice if needed.
- **A raw terminal is a bigger grant than a chat box.** It was a shell
  already; now it is plainly one. View-only devices, the audit lines and
  revocation.
- **The relay's 128-frame buffer** is a cliff for a device: the frame
  credit keeps panes away from it, but a 1 MiB snapshot is 64 frames by
  itself. Unchanged from today; noted in case panes make it visible.
- **The pinned Claude Code in CI** pulls an npm package on every cache
  miss, and a new release can add a first-run dialog (the pin exists for
  that). The fake model refuses and logs anything sent elsewhere.
- **`pane.open` as an "idempotent" op** stretches the word (it opens
  something in the plugin, though it changes nothing in rlord). It keeps
  the op out of the request store and the outbox, which is what is
  wanted; said in its comment.
- **The protocol is still unreviewed** and now carries keystrokes. Only
  the user's own relay and phone until the review (slice 3 answer 8).

### Open questions for the user

The first nine are answered below. New, from the explicit control model:

1. **Viewers can't type.** Planned: only the controller's keys reach a
   pane; on any other screen a key press points at Take over (one tap or
   key, no confirmation). Alternative: every screen may always type and
   control only decides the size. Recommended: as planned; it is what
   makes the bind explicit on both sides, and typing into a layout made
   for another screen is the confusion this removes.
2. **When the controller leaves.** Planned: it keeps control for 30 s
   (a blip or a look at the list changes nothing), then control passes
   to the terminal if it shows the pane, else to the screen used last,
   else the pane is free. Alternative: control stays with the phone
   until someone takes it, however long it has been gone. Recommended:
   as planned; otherwise every return to the desk starts with a take
   over from a phone in a pocket.
3. **A free pane goes to whoever opens it**, with no tap (there is
   nobody to take it from); a pane held by a screen that is present is
   never taken without one. Agreed?
4. **The desktop page follows the same rule**: opened beside a terminal
   that holds the session, its pane is a viewer until Take over (today
   two views silently take turns). Planned: yes, one rule everywhere.
5. **Names**: "<host name>'s terminal", "a browser on <host name>", and
   the phone's pairing name. Two browser tabs are both "a browser".
   Good enough?

### Slice 5 answers (user, 2026-11-24)

To the first plan's nine questions:

1. Size: "last typed wins" in spirit, but explicit. The user's words: "A
   'take over from <location>' and a corresponding 'currently controlled
   by <location>' view that basically makes the bind explicit on both
   sides would be the best user experience." Decision 3 is rewritten as
   an explicit control handoff on every surface (the TUI, the desktop
   page, a named phone).
2. Typing: every paired device may type by default, with a host-only
   per-device view-only switch, as planned.
3. CI: yes, install the pinned Claude Code in CI (cached) for `app
   test-term`. (Was question 5.)
4. CSP: yes, allow `style-src 'unsafe-inline'` on the phone page, only
   if WebGL proves unreliable. (Was question 6.)
5. to 9. No objection, so the recommendations stand: the compose line
   beside direct keys; the Transcript tab stays (`session.messages`);
   losing notes for the overlord while the computer is unreachable is
   accepted; scroll-back by touch needs `[tmux] mouse`; no local echo
   for now.

Control follow-ups (user, 2026-11-24), all as recommended: only the
controller types (viewers are pointed at Take over); a holder that leaves
keeps control 30 s, then it returns to the terminal if it shows the pane,
else the last-used screen, else free; a free pane goes to whoever opens it
without a tap; the desktop page follows the same rule. Two browser tabs
both named "a browser on <host>" was not raised; kept as planned.

### Slice 5 build: gates and the contract between the parts

Implementer's record (Opus, 2026-11-24, `feat/web-ui-5`). The gates ran
first (scratch code, not kept); then the contract below was fixed so
the host, the daemon, the engine and the page could be built side by
side. Where this differs from the plan, the gate result is why.

#### Gate results

- **1a, tmux and an unread client: fails as stated; the plan's fallback
  is taken.** A view client whose PTY is never read gets one redraw when
  read again (3.5 KB after a 200 MB flood on 3.8; about 24 KB on 3.2a and
  3.4), but tmux's memory grows with the flood while it isn't read: 880
  MB for an 800 MB `yes` flood on tmux 3.8 (macOS), 290 MB on 3.2a and
  3.4 (Ubuntu 22.04 and 24.04). The reason is in tmux (`tty.c`,
  `server-client.c`): after a redraw tmux stops discarding for a client
  until that redraw's bytes are written (`c->redraw > 0`), and an unread
  client never writes them. A client that is read, even slowly, stays
  bounded and coalesced: read at 64 KiB/s (or 16 KiB/s) through a 200 MB
  flood, the server stayed at 4 to 10 MB and the client got 54 to 400 KB
  in all. So the host never leaves a view client unread: when a pane
  can't send (no credit, the link's queue over its mark, or the pane's
  rate), its task keeps reading into a hold of at most 256 KiB, sent
  first when it can send again; if the hold would overflow, the task
  drops it and ends that view client, and when the pane can send again
  attaches a fresh one at the same size, its first frame starting with
  `CAN ESC c` (abort any sequence, full reset) ahead of tmux's full
  redraw. A fresh client rather than `refresh-client`: after discarded
  bytes tmux's idea of the terminal's state (modes, attributes) is wrong,
  and a new client starts from none. Lossless on the wire still (only
  bytes never sent are dropped); a slow phone sees fewer, whole screens,
  as planned. Measured by hand only in the gate; `a_flood_never_drops_the_link`
  covers it.
- **1b, control in tmux: passes on 3.2a, 3.4 and 3.8, with one gap.**
  `new-session -f ignore-size` (and `refresh-client -f ignore-size`)
  works on all three: views never size the window; `resize-window` pins
  it with a terminal and views attached, and a terminal's resize doesn't
  unpin it; unsetting `window-size` hands it back to the terminal's size
  (with only ignore-size views left it takes a view's size, so a free
  pane is never unpinned). A `read-only` view client can't type. A
  session `key-table` of rlord's with `Any` bound holds a real terminal's
  keys (letters, Enter, arrows, C-c, mouse wheel) while rlord's own
  bindings in that table (navigation, take) still run, **only with
  `assume-paste-time 0` on the held session**: otherwise keys typed
  faster than 1 ms apart count as a paste and skip the table (3.2a and
  3.4 leaked a fast burst and a C-c). The gap: a **bracketed paste** from
  the held terminal reaches the pane on 3.4 and 3.8 (tmux forwards
  pastes past key tables; not on 3.2a). Taken as the plan's "honest
  rather than strict" for pastes only: typed keys are held, a paste from
  the computer's own terminal still lands. `Any` runs `display-message`
  (not `-N`: with `-N` a held message swallows the take key too). The
  table's name: `rlord-held`.
- **1c, xterm.js with WebGL in WKWebView under the phone's CSP: passes
  (simulator).** The WebGL renderer loads and draws; the only policy
  violations are xterm.js's refused `<style>` elements
  (`style-src-elem`); a lost context (`WEBGL_lose_context`, no restore)
  fires `onContextLoss` and a new addon draws again. A context that is
  restored within xterm.js's own 3 s window is handled by the addon
  itself. Claude Code's glyphs are all present in `ui-monospace`;
  `⏺` and `⏸` draw as colour emoji (Apple's emoji presentation), a
  cosmetic difference, so no font is bundled. 12 px fits 52 columns on
  an iPhone 18 Pro. On the real phone: owed to the by-hand pass.
- **1d, the iOS keyboard on xterm.js's textarea: passes (simulator).**
  Letters, quotes, `--`, `...`, Enter (`\r`) and delete (`\x7f`) arrive
  as typed (no smart punctuation through XCUITest's typing; the real
  keyboard's smart punctuation and dictation are owed to the by-hand
  pass, and the shell turns them off in step 8). The web view's
  accessory bar (up, down, Done) is there, so step 8 removes it. The
  page is a secure context (`isSecureContext` true) and
  `navigator.clipboard.readText`/`writeText` exist on `rlord-app://`;
  but whether `readText` works was
  inconclusive (the test couldn't find iOS's paste callout and hung on
  it), so the shell's `paste` and `copy` messages (`UIPasteboard`) are
  built as planned.
- **1e, the real Claude Code at 45 to 60 columns: passes.** At 54, 45 and
  60 columns its prompt, answers, the mode line and the permission
  dialog (four options, wrapped) are all usable; a resize redraws its
  whole screen at the new width at once (rows already in tmux's history
  keep their old wrapping).
- **1f, the bridge: passes (simulator).** 16 KiB frames base64 in JSON
  through `callAsyncJavaScript`: 64 KiB/s paced for 5 s with nothing
  dropped; a burst of 200 frames (3.2 MB) queued in 10 ms and written
  into xterm.js in 0.15 s (about 21 MB/s), in order.

#### The contract

Names and shapes the parts build against. The plan's decisions hold;
this fills in what they left open.

**Device and host (control stream, JSON).**

- `pane.open` is a remote op: `{"id","op":"pane.open","args":{"stream",
  "target","cols","rows"}}`. The host checks `stream` (1 to 250, not
  open on this link), the size (2 to 1000) and its limits (2 per device,
  8 remote in all: `too_many`), then asks the daemon (`web.call` with the
  op, `via: "remote"`, `device`), which resolves the target and applies
  the control rules, and attaches the view client. Answer `{"id","ok":
  {"stream","id","name","overlord","input","control","cols","rows",
  "window":{"cols","rows"}}}`: `cols`, `rows` the size the view client is
  attached at (the device's own when it controls, else the window's),
  `window` the window's size now. Errors: `not_running`, `not_found`,
  `too_many`, `bad_request`, `not_allowed`, `failed`.
- `pane.take` is a remote and local op: `{"target","cols","rows"}`,
  answered `{"control":true,"cols","rows"}`; refused `not_allowed` for a
  view-only device.
- Events, device to host: `{"event":"pane","stream","type":"resize",
  "cols","rows"}`, `{"event":"pane","stream","type":"ack","bytes",
  "frames"}` (cumulative), `{"event":"pane","stream","type":"close"}`.
  Host to device: `{"event":"pane","stream","type":"closed","reason",
  "message"}`, reason `ended`, `closed`, `revoked`, `overflow` or
  `failed`. Data frames: `stream | 0 | bytes`.
- `hello` gains `"panes":{"max":2,"window":65536,"frames":48}` and
  `"input": true|false`.

**Engine and page (the bridge).**

- Page to engine: `pane.open` and `pane.take` as ordinary requests
  (`{"id","op","args"}`; never kept in the outbox, refused
  `disconnected`/`host_offline` with no live session);
  `{"pane":"input","stream","b":"<base64>"}`,
  `{"pane":"resize","stream","cols","rows"}`,
  `{"pane":"ack","stream","bytes","frames"}`,
  `{"pane":"close","stream"}`. The engine turns them into the events
  above and data frames (input split into frames of at most 16 382
  bytes); `ack` and `resize` are events, not ops (no rate limit).
- Engine to page: `{"event":"pane","stream","type":"data","b":
  "<base64>"}` (one per host frame, so the page counts frames by
  messages), `{"event":"pane","stream","type":"closed","reason",
  "message"}` (the host's, and `reason: "link"` for every stream the
  engine saw opened when the session ends). Base64 is standard with
  padding (`atob`).

**Plugin and daemon.**

- `web.call {op: "pane.open", args, via: "remote", device}`: answered
  `{"id","name","overlord","tmux_session","running","control","cols",
  "rows","window":{"cols","rows"}}` (the plugin keeps `tmux_session`
  and drops it from the device's answer). `web.pane_open {target,
  surface, cols, rows}` (local pages; `surface` `page:<id>`) answers the
  same. Both: a free pane is taken by the opener, unless the opener is
  a view-only device; `running: false` with no control change for a
  target that isn't running.
- `web.pane_size {target, surface, cols, rows}`: the surface's terminal
  changed size; if it holds the pane, pin again. Answer `{"control"}`.
- `web.pane_closed {target, surface}`: the surface stopped showing the
  pane (holder: away for 30 s).
- `target` after an open is the session id the open returned.
- `web.subscribe` gains `{"notify":"control","sessions":{"<session id>":
  {"holder","by","name","away","cols","rows","tmux_session"}}}`, the
  whole map of held panes, on every change (the plugin decides typing
  and viewers' sizes from it).
- `web.remote_state` gains `devices: [{"id","name","input"}]` (names
  and the view-only flag; no keys).
- Snapshot sessions and the overlord gain `control`: `null` or
  `{"holder","by","name","away","cols","rows"}`; `holder` is `terminal`,
  `page:<id>` or `device:<id>`, `by` is `terminal`, `web` or `phone`.
- `device.input {device, input}` is a local-only op; the daemon passes
  it to the plugin as a relay command (`{"cmd":"input","device",
  "input"}`), as it does a revoke. `rlord relay input <device> on|off`
  is the same.
- `control.take {target}` (daemon method, `rlord control take
  <session>`): the terminal takes.
- The local pane socket's `open` gains `"page"` (the page id from the
  control socket's `hello`); `opened` gains `control`, `cols`, `rows`,
  `window`.
- `tmux::view_command(target, view, read_only)` attaches with `-f
  ignore-size` (and `read-only` for a view-only device).
- `plugin::WEB_PROTOCOL` 5; `web::API` 2.

#### Step 6 (page)

Built on `s5/page` (Opus, 2026-11-24), against a fake bridge; not run
against a host or the engine yet.

- **Links.** `paneLink.ts` holds `PaneLink` (`connect`, `input`,
  `resize`, `visible`, `stop`, `state`), `PaneState` and `Screen`
  (`write(bytes, parsed)`: xterm.js's write callback). `pane.ts` keeps
  `PaneSocket` (local only): `open` gains `page`, `opened`'s `control`,
  `cols`, `rows`, `window` land in the open state (absent: `control:
  null`, read as "this page holds it", the old behaviour).
  `remotePane.ts` is `RemotePaneLink`: `pane.open {stream, target, cols,
  rows}` as a request; the stream id is the lowest free of 1 to 250,
  freed at its `closed` (or when the open is refused, or at a new
  session's `hello`, where nothing of the old one lives); the screen is
  reset at the answer; `data` is base64-decoded into xterm.js and
  acknowledged (cumulative `bytes` and `frames`) once parsed, at 16 KiB
  at once, else 50 ms after; `closed` `link` waits for the next `hello`,
  `ended` and `failed`/`overflow` retry with backoff, `revoked` stops;
  every `hello` reopens. `RemoteTransport` routes `{"event":"pane"}` by
  stream and owns the ids (`claim`, `unclaim`, `mute`, `onHello`), and
  `paste()`/`copy()` for the shell's messages.
- **Terminal.** `term.ts` mounts xterm.js on any link: WebGL only, a new
  `WebglAddon` per lost context (at most 5 in a row); holder fit to the
  box, viewer resized to the window's size and CSS-scaled down; on the
  phone the rows are laid out for the box plus what the keyboard takes
  (`innerHeight - visualViewport.height`), the terminal anchored at the
  bottom and slid by a drag, the rest of the drag one SGR wheel report
  per row when the pane asked for the mouse; a new size only after a
  width change or a font step, 200 ms after. `term/keys.ts` and
  `term/layout.ts` are the pure parts. 12 px `ui-monospace` by default,
  10 to 16 kept in `localStorage`.
- **Control.** `control.ts`: `surfaceOf(hello)`, `controlView` (the
  snapshot's `control` against this surface; the open's `control` while
  the snapshot is behind or says nobody; a take counts until the
  snapshot's `control` changes; view-only is always a viewer with no
  Take over). The bar is in `components/Pane.tsx`, so the local page's
  panes have it too; a key typed as a viewer marks Take over.
- **Phone views.** `#/overlord`: Terminal and Transcript tabs; a session:
  Terminal (while running) / Transcript / Details (a compact `Session`
  hands its facts and buttons to the tab); not running: Transcript (with
  Resume) and Details. The header has an `overlord` link marked while it
  waits for input. The key row (and compose line, Paste, Copy screen,
  font steps, the key-to-echo read-out) is there from the first render
  so the terminal is laid out above it. `views/Chat.tsx` became
  `views/Transcript.tsx`, `chat.ts` `transcript.ts`; the outbox shows as
  lines under the overlord's row with Cancel. `caps.tell` keeps the
  "tell the overlord" box to the local page.
- **Proved** (Vitest, 149 tests, no jsdom): the key bytes, the sizing
  maths, the control bar's states from the snapshot fixture, the pane
  link against `RemoteTransport` on a fake bridge (open, bytes, acks at
  16 KiB and 50 ms, `closed`, reopen at `hello`, id reuse only after
  `closed`, hidden, revoked, view only), the transport's stream routing
  and paste. `web build --app` passes with xterm.js in the bundle.
- **Deviations.** (1) The phone bundle's checks: xterm.js is now
  required (`xterm-helper-textarea` in a script); the licenses notice is
  exempt from the URL check (xterm.js's copyright lines name its
  authors' sites; it is never loaded as code); `importScripts` is matched
  as a call (`importScripts(`), since xterm.js feature-tests it with
  `typeof`. (2) `SettingsEvent` gains `debug` (the shell's debug build),
  which turns on the screen mirror; `platform: "bridge"` does too.
  (3) `SessionBase.control` is optional until the snapshot fixture
  carries it. (4) The overlord view has a Transcript tab too
  (`session.messages` with its id, as decision 8 says it can).
- **Owed.** Make `control` required in `proto.ts` once the daemon's
  fixture has it (`fixture.test.ts` fails on the key check until then).
  The shell sends `"debug": true` in `settings` in debug builds, and
  answers `paste` and `copy` (step 8). The engine could name a new
  session's description as an outbox item's `text` (the line shows only
  "New session" without it). `apps/ios`'s UI test still asks through the
  chat (step 8). Not seen in a browser or on a phone yet: the layout,
  the slide, the scale and the key row are by-hand items for step 9.
  `app test-e2e` updated, not run (needs the host's panes).
