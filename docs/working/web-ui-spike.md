**Status:** parked (feasibility spike; local branch `spike/web-ui`, never pushed)

# Web UI spike: drive the overlord from a browser (PTY bridge)

Question: can a local browser cleanly drive the rlord overlord session?
Scope was option A only: a localhost websocket server spawns a PTY running
`tmux -L <socket> attach -t =<overlord>` and streams it to xterm.js. The web
view targets only the overlord, as an overlay; no general tmux splits.

Short answer: **yes, cleanly.** Typing, resize, mouse, colours and reconnect
all work with a few hundred lines and no rlord changes. The rough edges are
tmux-client semantics (shared window size, full tmux client), not the
bridge.

## What was built

`spikes/web-ui/` (its own Cargo workspace, outside the main build and lockfile):

- `src/main.rs`: axum server on `127.0.0.1` only. Routes `/t/<token>/`
  (the page) and `/t/<token>/ws`. Each websocket gets its own PTY
  (`portable-pty`) running `tmux -L $RLORD_TMUX_SOCKET attach-session -t
  =<name>` with `TERM=xterm-256color COLORTERM=truecolor`, `TMUX` unset.
  Binary frames carry raw bytes both ways; text frames carry control
  (`{"resize":[c,r]}` in; `{"target":…}` / `{"error":…}` out). Socket
  close kills the tmux client (the session lives on).
- `src/page.html`: xterm.js 5.5 + fit addon (from jsdelivr), a header
  bar, a status chip, reconnect with backoff (250 ms doubling to 5 s),
  `term.reset()` on each connect so tmux's full redraw lands clean.
- Refuses an empty or `rlord` `RLORD_TMUX_SOCKET`: scratch servers only.
- `--target <session>` serves another session (used for a probe pane).

Run against the demo:

```sh
scripts/demo/demo.sh /tmp/rwd/d
source /tmp/rwd/d/env.sh
cd spikes/web-ui && cargo run -- --port 47811   # prints the URL
scripts/demo/demo.sh --stop /tmp/rwd/d
```

## Finding the pane

Clean. One daemon call, `overlord.get`, returns the active overlord with
`tmux_session` (`overlord-rlord` in the demo); the bridge confirms it with
`has-session -t =name` and attaches by exact name. Resolving on every
connect means a reconnect follows a restarted or switched overlord for
free. `overlord.get` still returns the name while the tmux session is
dead, so the `has-session` check matters; the page shows "tmux session
… is not running" and keeps retrying.

A real version should call `overlord.ensure` (as the TUI does when the
user opens the overlord) so opening the web view starts it, and live
inside the daemon (it already knows the active overlord and when it
switches) rather than as a separate process.

## What was tested and how

Against `scripts/demo/demo.sh` (own tmux socket `rlord-demo`, stand-in
agents, `RLORD_NO_LOGIN_SHELL=1`, stand-in `SHELL`). No real agents, no
model calls, `tmux -L rlord` and the default server untouched. A raw
Python websocket client drove the bridge; tmux state was read with
`display -p` / `capture-pane` / `list-clients`. A probe pane
(`webspike-probe`, a Python script in raw mode with SGR mouse on, logging
its input) checked what reaches an app.

| Check | Result |
| --- | --- |
| Typing | Works. Text, Ctrl-C (`\x03`), arrows (`\e[A`) reach the app byte-exact. |
| Resize | Works. Resize frame → `TIOCSWINSZ` → tmux resizes the window (90x25 browser → 90x24 window; tmux's status row takes one). |
| Mouse | Works. tmux turns on `?1000h ?1002h ?1006h` on the browser terminal (rlord's `mouse on`); xterm.js reports SGR; a click arrives at the app as `\e[<0;5;5M` with correct coordinates. Wheel follows rlord's `WheelUpPane` binding (copy mode, or passthrough for mouse/alt-screen apps). |
| Colours | Works. Client reports `RGB` in `client_termfeatures`; truecolor (`38;2;…`), 256 (`38;5;…`), bold, UTF-8 incl. CJK pass through. |
| Reconnect: socket drop | New attach, full redraw (~2–3 KB), page resets first. |
| Reconnect: overlord killed | tmux client prints `[exited]`, socket closes; retries get a clear error until it is back; back within one retry after `overlord.ensure`. |
| Reconnect: bridge restart | Token is kept in `$XDG_RUNTIME_DIR/robot-overlord/web-spike.token` (0600), so an open page reconnects. A fresh token per start would strand it. |
| Bind / token / Origin / Host | Listens on `127.0.0.1` only. Bad token 404; foreign `Host` 404 (DNS rebinding); websocket without Origin, or with `http://evil.example` or even `http://localhost:<port>`, 403; exact `http://127.0.0.1:<port>` 101. Page sent with a tight CSP, `no-referrer`, `no-store`. |

**Not verified:** rendering in a real browser. The Chrome extension wasn't
connected, so the page itself (xterm.js drawing, fit addon, focus, key
handling such as Option/Meta and browser-reserved shortcuts) was only
checked at the protocol level. First thing to do on picking this up.

## What was rough

- **Shared window size.** The web client is an ordinary tmux client; the
  overlord's window uses `window-size latest`, so it takes the size of
  whichever client was active last. Two tabs, or a tab plus the TUI
  overlay (which sizes the window to its popup), fight: a 140x45 tab and
  an 80x24 tab flip the window each time one types. Detaching leaves the
  window at the last web size until another client acts. Options: one web
  client at a time (kick older ones), or let the daemon own sizing as the
  overlay does (`auto_size_window`).
- **It's a full tmux client.** The user's tmux keys work in it: prefix
  commands, `switch-client`, rlord's nav bindings (Option-D / Option-O).
  So the "overlord only" view can be navigated to other sessions or the
  list. Fine for a local power user; wrong for a locked-down overlay.
  Fixes: a dedicated key table for web clients is not available per
  client, so either attach a grouped/linked session with no other
  windows and bindings stripped, or switch to option B (control mode
  `tmux -C`, or `pipe-pane` + `send-keys`, rendering just the pane).
- **tmux chrome.** The overlord's status bar (session option, shared with
  the TUI) shows in the browser and eats a row. Can't be hidden per
  client.
- **Browser-eaten keys.** Cmd-W/Cmd-T/Cmd-L etc. never reach the page;
  Option as Meta needs xterm.js `macOptionIsMeta`. Not tested (see above).
- **Typing into the stand-in.** The demo's stand-in agent ignores input,
  so typed text only shows via tty echo; end-to-end "send a message, see
  the overlord answer" needs a real agent and wasn't in scope.
- **CDN assets.** The page loads xterm.js from jsdelivr. A real version
  should embed them (`include_bytes!`) so it works offline and the CSP
  can drop the CDN.

## What a remote version would need

- **Transport security:** TLS (or only over an SSH tunnel / Tailscale),
  never plain ws over a network. Origin + Host checks stay.
- **Real auth:** the URL token is a bearer secret fit for localhost only
  (it leaks via history, screen shares). Remote needs login (device
  pairing or passkey), a session cookie (`HttpOnly`, `SameSite=Strict`,
  `Secure`), expiry and revocation, and rate limiting.
- **Least privilege:** the overlord can launch agents and send to any
  session, so the web view is as powerful as the user's shell. Remote
  use argues for the locked-down view (no tmux keys, overlord pane
  only; option B) and an audit log of input.
- **Latency/bandwidth:** raw tmux output is chatty; fine on localhost.
  Remote may want output coalescing and compression (permessage-deflate),
  and a mobile-friendly input box rather than raw keys.
- **Lifecycle:** run in the daemon, start/stop with it, follow
  `overlord.switch` and push a "switched" message, one attached web
  client policy, and `overlord.ensure` on open.
- **Phone layout:** xterm.js at phone width is cramped; the overlord's
  output would want reflow or a chat-style view fed from the transcript
  rather than the screen.
