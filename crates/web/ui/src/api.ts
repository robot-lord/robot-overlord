// The control socket: the token, its first-frame auth, reconnecting, and
// requests matched to answers by id.
//
// The token comes in the URL fragment (`#t=<token>`, never sent to the
// server), is kept in localStorage (per origin, so per port) and taken out
// of the address bar.

import {
  CLOSE_UNAUTHORIZED,
  type ControlMessage,
  type ErrorCode,
  type Hello,
  type Op,
} from "./proto";

/** What the page needs of a WebSocket (a fake in tests). */
export interface Sock {
  readonly readyState: number;
  binaryType: BinaryType;
  send(data: string | Uint8Array<ArrayBuffer>): void;
  close(code?: number, reason?: string): void;
  onopen: ((ev: Event) => void) | null;
  onmessage: ((ev: MessageEvent) => void) | null;
  onclose: ((ev: CloseEvent) => void) | null;
  onerror: ((ev: Event) => void) | null;
}

export type OpenSock = (url: string) => Sock;

/** WebSocket.OPEN (not read from the global: tests run without one). */
const OPEN = 1;

export const openWebSocket: OpenSock = (url) => new WebSocket(url);

/** A websocket URL on this page's own origin. */
export function wsUrl(path: string, base: string = location.href): string {
  const u = new URL(path, base);
  u.protocol = "ws:";
  u.hash = "";
  u.search = "";
  return u.toString();
}

const TOKEN_KEY = "rlord.token";

/**
 * The token: from the fragment (`#t=<token>`, then saved and removed from
 * the address bar), else the saved one. Null when there is neither.
 */
export function takeToken(
  loc: { hash: string; pathname: string; search: string },
  storage: Pick<Storage, "getItem" | "setItem"> | null,
  replace: (url: string) => void,
): string | null {
  const m = /^#t=([0-9a-f]{16,})$/i.exec(loc.hash);
  if (m?.[1]) {
    try {
      storage?.setItem(TOKEN_KEY, m[1]);
    } catch {
      // Storage off: the token lives as long as the page.
    }
    replace(`${loc.pathname}${loc.search}#/`);
    return m[1];
  }
  try {
    return storage?.getItem(TOKEN_KEY) ?? null;
  } catch {
    return null;
  }
}

/** A failed request. `code` is the plugin's, or `disconnected`. */
export class ApiError extends Error {
  readonly code: ErrorCode | "disconnected";
  constructor(code: ErrorCode | "disconnected", message: string) {
    super(message);
    this.code = code;
  }
}

export type ConnState = "connecting" | "open" | "closed" | "unauthorized";

export interface ControlEvents {
  state(s: ConnState): void;
  hello(h: Hello): void;
  changed(rev: string): void;
}

/** Retry waits: from FIRST, doubling up to LAST. */
export const BACKOFF_FIRST = 250;
export const BACKOFF_LAST = 5000;

interface Pending {
  resolve(v: unknown): void;
  reject(e: ApiError): void;
}

/**
 * The control socket. Connects at once and keeps reconnecting (backoff
 * reset by each hello) until `stop`, or until the token is refused.
 */
export class Control {
  private sock: Sock | null = null;
  private authed = false;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private backoff = BACKOFF_FIRST;
  private retry: ReturnType<typeof setTimeout> | null = null;
  private stopped = false;

  private readonly url: string;
  private readonly token: string;
  private readonly on: ControlEvents;
  private readonly open: OpenSock;

  constructor(url: string, token: string, on: ControlEvents, open: OpenSock = openWebSocket) {
    this.url = url;
    this.token = token;
    this.on = on;
    this.open = open;
    this.connect();
  }

  /** Ask the daemon (an op on its allow-list). */
  request<T = unknown>(op: Op, args: object = {}): Promise<T> {
    const s = this.sock;
    if (!s || !this.authed || s.readyState !== OPEN) {
      return Promise.reject(new ApiError("disconnected", "not connected"));
    }
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: (v) => resolve(v as T), reject });
      s.send(JSON.stringify({ id, op, args }));
    });
  }

  stop(): void {
    this.stopped = true;
    if (this.retry) clearTimeout(this.retry);
    this.sock?.close();
  }

  private connect(): void {
    this.retry = null;
    this.on.state("connecting");
    const s = this.open(this.url);
    this.sock = s;
    this.authed = false;
    s.onopen = () => s.send(JSON.stringify({ auth: this.token }));
    s.onmessage = (ev) => {
      if (typeof ev.data !== "string") return;
      let m: ControlMessage;
      try {
        m = JSON.parse(ev.data) as ControlMessage;
      } catch {
        return;
      }
      this.message(m);
    };
    s.onclose = (ev) => this.closed(s, ev.code);
    s.onerror = () => {};
  }

  private message(m: ControlMessage): void {
    if ("event" in m) {
      if (m.event === "hello") {
        this.authed = true;
        this.backoff = BACKOFF_FIRST;
        this.on.state("open");
        this.on.hello(m);
      } else if (m.event === "changed") {
        this.on.changed(m.rev);
      }
      return;
    }
    const p = this.pending.get(m.id);
    if (!p) return;
    this.pending.delete(m.id);
    if (m.error) p.reject(new ApiError(m.error.code, m.error.message));
    else p.resolve(m.ok);
  }

  private closed(s: Sock, code: number): void {
    if (this.sock !== s) return;
    this.sock = null;
    this.authed = false;
    for (const p of this.pending.values()) {
      p.reject(new ApiError("disconnected", "connection lost"));
    }
    this.pending.clear();
    if (code === CLOSE_UNAUTHORIZED) {
      this.on.state("unauthorized");
      return;
    }
    this.on.state("closed");
    if (this.stopped) return;
    this.retry = setTimeout(() => this.connect(), this.backoff);
    this.backoff = Math.min(this.backoff * 2, BACKOFF_LAST);
  }
}
