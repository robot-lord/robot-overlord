// A pane's link to rlord, and the local page's: one pane socket
// (crates/web/src/pane.rs). The terminal (term.ts) draws whichever link it
// is given: this socket on the local page, the bridge's streams on the
// phone (remotePane.ts). Only main.tsx's views build a `PaneSocket`, so
// the phone's bundle has no websocket.
//
// After the token the page sends `{"type":"open","target","cols","rows",
// "page"}`; then binary frames are bytes both ways and text frames are
// control (`resize` and `close` in; `opened {id, name, control, cols,
// rows, window}` and `closed {reason}` out). Opening never starts
// anything: a target that isn't running is closed with `not_running`, and
// the view offers Resume.
//
// A socket is open only while the pane is on screen: the view mounts one
// pane, and a hidden tab closes it shortly after (`HIDE_CLOSE_MS`).

import { BACKOFF_FIRST, BACKOFF_LAST, type OpenSock, openWebSocket, type Sock } from "./api";
import { HIDE_CLOSE_MS, type PaneLink, type PaneState, type Screen } from "./paneLink";
import { CLOSE_UNAUTHORIZED } from "./proto";

export { HIDE_CLOSE_MS, type PaneState, type Screen } from "./paneLink";

const enc = new TextEncoder();

/**
 * One pane socket, reconnecting as the control socket does. Knows nothing
 * of xterm.js: it writes to a `Screen` and is fed input by its owner.
 */
export class PaneSocket implements PaneLink {
  private sock: Sock | null = null;
  private backoff = BACKOFF_FIRST;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private hideTimer: ReturnType<typeof setTimeout> | null = null;
  private stopped = false;
  private current: PaneState = { kind: "connecting" };

  private readonly url: string;
  private readonly token: string;
  private readonly target: string;
  private readonly page: () => string | null;
  private readonly screen: Screen;
  private readonly onState: (s: PaneState) => void;
  private readonly open: OpenSock;

  constructor(opts: {
    url: string;
    token: string;
    target: string;
    /** This page's id (the control socket's hello): the plugin's surface `page:<id>`. */
    page?: () => string | null;
    screen: Screen;
    onState: (s: PaneState) => void;
    open?: OpenSock;
  }) {
    this.url = opts.url;
    this.token = opts.token;
    this.target = opts.target;
    this.page = opts.page ?? (() => null);
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
      const open: Record<string, unknown> = {
        type: "open",
        target: this.target,
        cols: this.screen.cols,
        rows: this.screen.rows,
      };
      const page = this.page();
      if (page) open.page = page;
      s.send(JSON.stringify(open));
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
      let m: {
        type?: string;
        id?: string;
        name?: string;
        reason?: string;
        message?: string;
        control?: boolean;
        cols?: number;
        rows?: number;
        window?: { cols: number; rows: number };
      };
      try {
        m = JSON.parse(data) as typeof m;
      } catch {
        return;
      }
      if (m.type === "opened") {
        // A fresh screen for each connection: tmux redraws it whole.
        this.screen.reset();
        this.backoff = BACKOFF_FIRST;
        this.set({
          kind: "open",
          id: m.id ?? null,
          name: m.name ?? null,
          control: typeof m.control === "boolean" ? m.control : null,
          input: true,
          cols: m.cols ?? null,
          rows: m.rows ?? null,
          window: m.window ?? null,
        });
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
