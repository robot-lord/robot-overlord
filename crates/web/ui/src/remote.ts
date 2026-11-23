// The phone app's transport: the same requests and answers as the control
// socket, over the shell's message bridge to the app's native engine,
// which owns the keys, the relay link and the outbox. The page has no
// network at all (its CSP says `connect-src 'none'`).
//
// Bridge (docs/working/web-ui-remote.md, "The bridge and RemoteTransport"):
// every message is one JSON text. iOS: page to shell
// `window.webkit.messageHandlers.rlord.postMessage(text)`; shell to page a
// call of the one global function `window.__rlord_recv(text)`. Android: a
// `WebMessageListener` named `rlord` (`rlord.postMessage(text)`,
// `rlord.onmessage`).

import type {
  AppEvent,
  AppMessage,
  BridgeMessage,
  ConnKind,
  PageOp,
  RemoteErrorCode,
} from "./proto";
import {
  ApiError,
  type Caps,
  type ConnState,
  type EngineErrorCode,
  REMOTE_CAPS,
  type Transport,
  type TransportEvents,
} from "./transport";

/** The shell's message bridge (a fake in tests). */
export interface Bridge {
  send(text: string): void;
  /** Take every message the native side sends. */
  listen(recv: (text: string) => void): void;
}

/** The global function the iOS shell calls with each message. */
export const RECV = "__rlord_recv";

/** What the page looks for on `window`. */
export interface BridgeHost {
  webkit?: { messageHandlers?: { rlord?: { postMessage(text: string): void } } };
  rlord?: {
    postMessage(text: string): void;
    onmessage: ((ev: { data: unknown }) => void) | null;
  };
}

/** The bridge the shell set up: iOS's message handler, else Android's listener. */
export function findBridge(w: BridgeHost): Bridge | null {
  const ios = w.webkit?.messageHandlers?.rlord;
  if (ios && typeof ios.postMessage === "function") {
    return {
      // (A WebKit message handler, not window.postMessage: no target origin.)
      // oxlint-disable-next-line unicorn/require-post-message-target-origin
      send: (t) => ios.postMessage(t),
      listen: (recv) => {
        // Fixed once: nothing later in the page can swap the receiver.
        Object.defineProperty(w, RECV, {
          value: (t: unknown) => recv(String(t)),
          writable: false,
          configurable: false,
          enumerable: false,
        });
      },
    };
  }
  const android = w.rlord;
  if (android && typeof android.postMessage === "function") {
    return {
      // (Android's WebMessageListener object: the shell fixed its origin.)
      // oxlint-disable-next-line unicorn/require-post-message-target-origin
      send: (t) => android.postMessage(t),
      listen: (recv) => {
        android.onmessage = (ev) => recv(String(ev.data));
      },
    };
  }
  return null;
}

/** The store's connection state for an engine's `conn` state (null: wait for hello). */
export function connState(k: ConnKind): ConnState | null {
  switch (k) {
    case "live":
      return null;
    case "connecting":
    case "relay":
      return "connecting";
    case "unpaired":
    case "offline":
    case "host_offline":
    case "wrong_relay":
    case "revoked":
    case "update_app":
    case "update_host":
    case "suspended":
      return "closed";
  }
}

export interface RemoteEvents extends TransportEvents {
  /** The engine's and shell's own events (`conn`, `pairing`, `outbox`, `status`, `settings`). */
  app(ev: AppEvent): void;
}

interface Pending {
  resolve(v: unknown): void;
  reject(e: ApiError): void;
}

/**
 * The bridge as a `Transport`. Says `{"app":"ready"}` at once, so the
 * engine replays what the page needs after a (re)load: the connection,
 * a pairing in progress, the outbox and the last hello.
 */
export class RemoteTransport implements Transport {
  /** (The local page's bundle check fails if this string is in it.) */
  static readonly marker = "rlord-remote-transport";
  readonly caps: Caps = REMOTE_CAPS;
  private nextId: number;
  private pending = new Map<number, Pending>();
  private stopped = false;
  private live = false;
  private shown: ConnState | null = null;
  private readonly bridge: Bridge;
  private readonly on: RemoteEvents;

  /**
   * `firstId`: the first request's number. By default the clock, so a
   * reloaded page never reuses a number the engine may still answer.
   */
  constructor(bridge: Bridge, on: RemoteEvents, firstId: number = Date.now()) {
    this.bridge = bridge;
    this.on = on;
    this.nextId = firstId;
    bridge.listen((t) => this.recv(t));
    this.state("connecting");
    this.send({ app: "ready" });
  }

  /**
   * Ask the daemon. The engine answers every request: at once with
   * `disconnected` or `host_offline` when it can't be sent, or later (an
   * ask waits in the outbox).
   */
  request<T = unknown>(op: PageOp, args: object = {}): Promise<T> {
    if (this.stopped) return Promise.reject(new ApiError("disconnected", "stopped"));
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: (v) => resolve(v as T), reject });
      this.bridge.send(JSON.stringify({ id, op, args }));
    });
  }

  /** A message for the engine or the shell. */
  send(m: AppMessage): void {
    if (!this.stopped) this.bridge.send(JSON.stringify(m));
  }

  stop(): void {
    this.stopped = true;
    for (const p of this.pending.values()) p.reject(new ApiError("disconnected", "stopped"));
    this.pending.clear();
  }

  private state(s: ConnState): void {
    if (this.shown === s) return;
    this.shown = s;
    this.on.state(s);
  }

  private recv(text: string): void {
    if (this.stopped) return;
    let m: BridgeMessage;
    try {
      m = JSON.parse(text) as BridgeMessage;
    } catch {
      return;
    }
    if (typeof m !== "object" || m === null) return;
    if ("event" in m) {
      switch (m.event) {
        case "hello":
          this.live = true;
          this.state("open");
          this.on.hello(m);
          return;
        case "changed":
          this.on.changed(m.rev);
          return;
        case "ui":
          this.on.ui?.(m.cmd);
          return;
        case "conn": {
          const s = connState(m.state);
          if (s) {
            this.live = false;
            this.state(s);
          }
          this.on.app(m);
          return;
        }
        case "pairing":
        case "outbox":
        case "status":
        case "settings":
          this.on.app(m);
          return;
        default:
          // An event this page doesn't know (a newer engine): skipped.
          return;
      }
    }
    if (typeof m.id !== "number") return;
    const p = this.pending.get(m.id);
    if (!p) return;
    this.pending.delete(m.id);
    if (m.error) {
      const code = m.error.code as RemoteErrorCode | EngineErrorCode;
      p.reject(new ApiError(code, m.error.message));
    } else p.resolve(m.ok);
  }

  /** Whether a live session is up (a hello came since the last `conn` away from live). */
  get connected(): boolean {
    return this.live;
  }
}
