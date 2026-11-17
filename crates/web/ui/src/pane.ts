// A pane in the page: xterm.js on one pane socket (crates/web/src/pane.rs).
//
// After the token the page sends `{"type":"open","target","cols","rows"}`;
// then binary frames are bytes both ways and text frames are control
// (`resize` and `close` in; `opened` and `closed {reason}` out). Opening
// never starts anything: a target that isn't running is closed with
// `not_running`, and the view offers Resume.
//
// A socket is open only while the pane is on screen: the view mounts one
// pane, and a hidden tab closes it shortly after (`HIDE_CLOSE_MS`), so
// background tabs never take the window's size (tmux's `window-size
// latest`).

import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { BACKOFF_FIRST, BACKOFF_LAST, type OpenSock, openWebSocket, type Sock } from "./api";
import { CLOSE_UNAUTHORIZED } from "./proto";

export type PaneState =
  | { kind: "connecting" }
  | { kind: "open"; id: string | null; name: string | null }
  /** It isn't running: the view offers Resume. */
  | { kind: "not_running" }
  /** No such session (or no overlord yet). */
  | { kind: "not_found"; message: string }
  /** Closed on purpose while the tab is hidden. */
  | { kind: "hidden" }
  | { kind: "unauthorized" }
  /** Lost or refused: retrying. */
  | { kind: "retrying"; message: string };

/** What the socket side needs of the terminal (a fake in tests). */
export interface Screen {
  readonly cols: number;
  readonly rows: number;
  reset(): void;
  write(bytes: Uint8Array): void;
}

/** How long a hidden tab keeps its pane open. */
export const HIDE_CLOSE_MS = 3000;

const enc = new TextEncoder();

/**
 * One pane socket, reconnecting as the control socket does. Knows nothing
 * of xterm.js: it writes to a `Screen` and is fed input by its owner.
 */
export class PaneSocket {
  private sock: Sock | null = null;
  private backoff = BACKOFF_FIRST;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private hideTimer: ReturnType<typeof setTimeout> | null = null;
  private stopped = false;
  private current: PaneState = { kind: "connecting" };

  private readonly url: string;
  private readonly token: string;
  private readonly target: string;
  private readonly screen: Screen;
  private readonly onState: (s: PaneState) => void;
  private readonly open: OpenSock;

  constructor(opts: {
    url: string;
    token: string;
    target: string;
    screen: Screen;
    onState: (s: PaneState) => void;
    open?: OpenSock;
  }) {
    this.url = opts.url;
    this.token = opts.token;
    this.target = opts.target;
    this.screen = opts.screen;
    this.onState = opts.onState;
    this.open = opts.open ?? openWebSocket;
  }

  get state(): PaneState {
    return this.current;
  }

  /** Connect now (again), whatever the state: after Resume, or a change. */
  connect(): void {
    if (this.stopped) return;
    if (this.retryTimer) clearTimeout(this.retryTimer);
    this.retryTimer = null;
    this.drop();
    this.set({ kind: "connecting" });
    const s = this.open(this.url);
    s.binaryType = "arraybuffer";
    this.sock = s;
    s.onopen = () => {
      s.send(JSON.stringify({ auth: this.token }));
      s.send(
        JSON.stringify({
          type: "open",
          target: this.target,
          cols: this.screen.cols,
          rows: this.screen.rows,
        }),
      );
    };
    // Why the plugin closed it, from its `closed` message.
    let reason: string | null = null;
    let message = "";
    s.onmessage = (ev) => {
      const data: unknown = ev.data;
      if (data instanceof ArrayBuffer) {
        this.screen.write(new Uint8Array(data));
        return;
      }
      if (typeof data !== "string") return;
      let m: { type?: string; id?: string; name?: string; reason?: string; message?: string };
      try {
        m = JSON.parse(data) as typeof m;
      } catch {
        return;
      }
      if (m.type === "opened") {
        // A fresh screen for each connection: tmux redraws it whole.
        this.screen.reset();
        this.backoff = BACKOFF_FIRST;
        this.set({ kind: "open", id: m.id ?? null, name: m.name ?? null });
      } else if (m.type === "closed") {
        reason = m.reason ?? "";
        message = m.message ?? "";
      }
    };
    s.onclose = (ev) => {
      if (this.sock !== s) return;
      this.sock = null;
      if (this.stopped) return;
      if (ev.code === CLOSE_UNAUTHORIZED) {
        this.set({ kind: "unauthorized" });
      } else if (reason === "not_running") {
        this.set({ kind: "not_running" });
      } else if (reason === "not_found") {
        this.set({ kind: "not_found", message });
      } else {
        // Lost, or the pane's client ended (the session stopped, the
        // overlord restarted): try again; opening never starts anything.
        this.later(reason === "ended" ? "it ended" : message || (reason ?? "connection lost"));
      }
    };
    s.onerror = () => {};
  }

  /** Bytes typed into the terminal. */
  input(data: string | Uint8Array<ArrayBuffer>): void {
    if (this.current.kind !== "open" || !this.sock) return;
    this.sock.send(typeof data === "string" ? enc.encode(data) : data);
  }

  resize(cols: number, rows: number): void {
    if (this.current.kind !== "open" || !this.sock) return;
    this.sock.send(JSON.stringify({ type: "resize", cols, rows }));
  }

  /** The tab was hidden or shown. */
  visible(v: boolean): void {
    if (v) {
      if (this.hideTimer) clearTimeout(this.hideTimer);
      this.hideTimer = null;
      if (this.current.kind === "hidden") this.connect();
      return;
    }
    if (this.hideTimer || this.current.kind === "hidden") return;
    this.hideTimer = setTimeout(() => {
      this.hideTimer = null;
      if (this.retryTimer) clearTimeout(this.retryTimer);
      this.retryTimer = null;
      this.drop();
      this.set({ kind: "hidden" });
    }, HIDE_CLOSE_MS);
  }

  /** For good (the view went away). */
  stop(): void {
    this.stopped = true;
    if (this.retryTimer) clearTimeout(this.retryTimer);
    if (this.hideTimer) clearTimeout(this.hideTimer);
    this.drop();
  }

  /** Close the socket, telling the plugin first. */
  private drop(): void {
    const s = this.sock;
    this.sock = null;
    if (!s) return;
    if (s.readyState === 1) s.send(JSON.stringify({ type: "close" }));
    s.close();
  }

  private later(message: string): void {
    this.set({ kind: "retrying", message });
    this.retryTimer = setTimeout(() => this.connect(), this.backoff);
    this.backoff = Math.min(this.backoff * 2, BACKOFF_LAST);
  }

  private set(s: PaneState): void {
    this.current = s;
    this.onState(s);
  }
}

/** The terminal's look. */
const THEME = {
  background: "#0b0b0d",
  foreground: "#d8d8dc",
  cursor: "#c79af2",
  selectionBackground: "#4a3d5c",
};

/**
 * A terminal in `el` on its own pane socket, resized with `el`, its
 * socket closed while the tab is hidden. `dispose` ends both.
 */
export function mountPane(
  el: HTMLElement,
  opts: { url: string; token: string; target: string; onState: (s: PaneState) => void },
): { socket: PaneSocket; focus(): void; dispose(): void } {
  const term = new Terminal({
    cursorBlink: true,
    fontFamily: 'Menlo, "SF Mono", "DejaVu Sans Mono", monospace',
    fontSize: 13,
    // tmux keeps the history (copy mode, by the wheel); the page draws
    // only the screen.
    scrollback: 0,
    // Option is Meta (Option-D and friends reach the agent); Option-click
    // selects text past tmux's mouse mode.
    macOptionIsMeta: true,
    macOptionClickForcesSelection: true,
    theme: THEME,
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(el);
  // WebGL when there is a context, else xterm.js's DOM renderer; a lost
  // context falls back to it too.
  try {
    const gl = new WebglAddon();
    gl.onContextLoss(() => gl.dispose());
    term.loadAddon(gl);
  } catch {
    // The DOM renderer it is.
  }
  fit.fit();

  const socket = new PaneSocket({
    url: opts.url,
    token: opts.token,
    target: opts.target,
    onState: (s) => {
      if (s.kind === "open") term.focus();
      opts.onState(s);
    },
    screen: {
      get cols() {
        return term.cols;
      },
      get rows() {
        return term.rows;
      },
      reset: () => term.reset(),
      write: (b) => term.write(b),
    },
  });
  const subs = [
    term.onData((d) => socket.input(d)),
    // Mouse reports in X10 encoding can carry bytes over 127: sent raw.
    term.onBinary((d) => socket.input(Uint8Array.from(d, (c) => c.charCodeAt(0)))),
    term.onResize(({ cols, rows }) => socket.resize(cols, rows)),
  ];
  let frame = 0;
  const ro = new ResizeObserver(() => {
    cancelAnimationFrame(frame);
    frame = requestAnimationFrame(() => fit.fit());
  });
  ro.observe(el);
  const onVisibility = () => socket.visible(document.visibilityState === "visible");
  document.addEventListener("visibilitychange", onVisibility);
  socket.connect();
  onVisibility();

  return {
    socket,
    focus: () => term.focus(),
    dispose: () => {
      socket.stop();
      document.removeEventListener("visibilitychange", onVisibility);
      ro.disconnect();
      cancelAnimationFrame(frame);
      for (const d of subs) d.dispose();
      term.dispose();
    },
  };
}
