// The dev bridge (development only): it plays the iOS shell over a
// websocket, so the phone entry finds it and RemoteTransport runs on it
// unchanged.

import { describe, expect, it } from "vitest";
import { type DevSocket, installDevBridge } from "../src/devBridge";
import { type BridgeHost, findBridge, RECV } from "../src/remote";

class Sock implements DevSocket {
  static last: Sock | null = null;
  readyState = 0;
  sent: string[] = [];
  closed = false;
  onopen: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  readonly url: string;
  constructor(url: string) {
    this.url = url;
    Sock.last = this;
  }
  send(t: string): void {
    this.sent.push(t);
  }
  close(): void {
    this.closed = true;
  }
  open(): void {
    this.readyState = 1;
    this.onopen?.(new Event("open"));
  }
  push(t: string): void {
    this.onmessage?.({ data: t } as MessageEvent);
  }
}

describe("the dev bridge", () => {
  it("is the shell's bridge to the page, over its socket", async () => {
    const w: BridgeHost = {};
    let gone = 0;
    installDevBridge(w, "ws://127.0.0.1:1/", Sock, () => gone++);
    const s = Sock.last as Sock;
    expect(s.url).toBe("ws://127.0.0.1:1/");
    const b = findBridge(w);
    expect(b).not.toBeNull();
    const got: string[] = [];
    // A message before the page listens waits for it.
    s.push('{"event":"conn","state":"unpaired"}');
    b?.listen((t) => got.push(t));
    expect(typeof (w as Record<string, unknown>)[RECV]).toBe("function");
    // Sent before the socket opens: sent once it does, in order.
    b?.send('{"app":"ready"}');
    b?.send('{"app":"status"}');
    expect(s.sent).toEqual([]);
    s.open();
    expect(s.sent).toEqual(['{"app":"ready"}', '{"app":"status"}']);
    // (Flushed once the page sends.)
    await Promise.resolve();
    expect(got).toEqual(['{"event":"conn","state":"unpaired"}']);
    s.push('{"event":"settings"}');
    expect(got.at(-1)).toBe('{"event":"settings"}');
    s.onclose?.({} as CloseEvent);
    expect(gone).toBe(1);
  });
});
