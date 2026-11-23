// What the page needs of its connection to rlord, whichever it has: the
// local page's control socket (`LocalTransport`, api.ts) or the phone
// app's bridge to its native engine (`RemoteTransport`, remote.ts). The
// requests and answers are the control socket's shapes either way, so
// nothing above a `Transport` knows which one it has; views ask `caps`,
// never the user agent.
//
// This file names no implementation: each entry (main.tsx, app.tsx)
// imports only its own, so the local page's bundle carries no remote code
// and the phone's no websocket.

import type { ErrorCode, Hello, PageOp, RemoteErrorCode, UiCmd } from "./proto";

/** What this connection can do: views hide what it can't. */
export interface Caps {
  /** Live terminal panes (xterm.js over `/pane`). */
  panes: boolean;
  /** The code-folder form (`form.setup*`, `overlord.setup_request`). */
  setup: boolean;
  /** The key screen (`keys.list`). */
  keys: boolean;
  /** Remote access: paired devices, pairing (`device.*`). */
  devices: boolean;
  /** Requests can wait in an outbox (pending and failed bubbles). */
  pending: boolean;
}

/** The local page: everything but an outbox. */
export const LOCAL_CAPS: Caps = {
  panes: true,
  setup: true,
  keys: true,
  devices: true,
  pending: false,
};

/** A paired device: chat and state; no panes and no local-only ops. */
export const REMOTE_CAPS: Caps = {
  panes: false,
  setup: false,
  keys: false,
  devices: false,
  pending: true,
};

/** The engine's own error codes (a device's requests only). */
export type EngineErrorCode =
  /** No live session, and the op can't wait. */
  | "disconnected"
  | "host_offline"
  /** A 17th waiting ask. */
  | "outbox_full"
  | "cancelled"
  /** 24 hours without an answer. */
  | "expired";

/** A failed request: the daemon's or plugin's code, or `disconnected`. */
export class ApiError extends Error {
  readonly code: ErrorCode | RemoteErrorCode | EngineErrorCode;
  constructor(code: ErrorCode | RemoteErrorCode | EngineErrorCode, message: string) {
    super(message);
    this.code = code;
  }
}

/** The connection as the store shows it. */
export type ConnState = "connecting" | "open" | "closed" | "unauthorized";

export interface TransportEvents {
  state(s: ConnState): void;
  hello(h: Hello): void;
  changed(rev: string): void;
  /** A UI command from the overlord, for this page. */
  ui?(cmd: UiCmd): void;
}

export interface Transport {
  readonly caps: Caps;
  /** Ask the daemon (an op on its allow-list). */
  request<T = unknown>(op: PageOp, args?: object): Promise<T>;
  stop(): void;
}

/**
 * The header's menu: the buttons for local-only screens, in order,
 * for what `caps` allows.
 */
export function menu(caps: Caps): ("setup" | "keys" | "devices")[] {
  return (["setup", "keys", "devices"] as const).filter((m) => caps[m]);
}
