// Development only: the phone shell's native side, played over the
// loopback websocket of `rlord-device bridge` (the app's engine run on
// the desktop). It gives the page what the iOS shell gives it:
// `webkit.messageHandlers.rlord.postMessage(text)` sends to the bridge,
// and each message from the bridge is a call of `__rlord_recv(text)`. So
// `findBridge` and `RemoteTransport` run unchanged.
//
// Used by the dev entry (app-dev.html, `cargo xtask app dev`) and the
// scripted end-to-end tests (`cargo xtask app test-e2e`), never by the
// phone app's page: `cargo xtask web build --app` fails if this file's
// marker, a WebSocket or a `ws://` URL is in that bundle.

import { type BridgeHost, RECV } from "./remote";

/** (The phone page's bundle check fails if this string is in it.) */
export const DEV_MARKER = "rlord-dev-bridge";

/** What the shim needs of a WebSocket (the browser's, or Node's global one). */
export interface DevSocket {
  readonly readyState: number;
  send(text: string): void;
  close(): void;
  onopen: ((ev: Event) => void) | null;
  onmessage: ((ev: MessageEvent) => void) | null;
  onclose: ((ev: CloseEvent) => void) | null;
}

export type DevSocketCtor = new (url: string) => DevSocket;

const OPEN = 1;

/**
 * Installs the iOS shell's message handler on `w`, backed by a websocket
 * to `url`. What the page sends before the socket opens waits for it; a
 * message from the bridge before the page listens waits for that.
 * `onClose`: the bridge went away (the dev page reloads).
 */
export function installDevBridge(
  w: BridgeHost,
  url: string,
  Ws: DevSocketCtor,
  onClose?: () => void,
): DevSocket {
  const ws = new Ws(url);
  const out: string[] = [];
  const early: string[] = [];
  const host = w as BridgeHost & Record<string, unknown>;
  const deliver = (text: string): boolean => {
    const recv = host[RECV];
    if (typeof recv !== "function") return false;
    (recv as (t: string) => void)(text);
    return true;
  };
  ws.onopen = () => {
    for (const t of out.splice(0)) ws.send(t);
  };
  const flush = () => {
    while (early.length > 0 && deliver(early[0] as string)) early.shift();
  };
  ws.onmessage = (ev) => {
    early.push(String(ev.data));
    flush();
  };
  ws.onclose = () => onClose?.();
  w.webkit = {
    messageHandlers: {
      rlord: {
        postMessage(text: string) {
          // (The page listens before its first send.)
          queueMicrotask(flush);
          if (ws.readyState === OPEN) ws.send(text);
          else out.push(text);
        },
      },
    },
  };
  return ws;
}
