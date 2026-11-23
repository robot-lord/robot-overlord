import type { Sock } from "../src/api";
import type { Bridge } from "../src/remote";

/** The app shell's message bridge, driven by hand: `push` is the native side. */
export class FakeBridge implements Bridge {
  sent: string[] = [];
  private recv: ((text: string) => void) | null = null;
  send(text: string): void {
    this.sent.push(text);
  }
  listen(recv: (text: string) => void): void {
    this.recv = recv;
  }
  push(m: unknown): void {
    this.recv?.(typeof m === "string" ? m : JSON.stringify(m));
  }
  json(i: number): unknown {
    const s = this.sent.at(i);
    return s === undefined ? undefined : JSON.parse(s);
  }
  /** Every message sent, parsed. */
  all(): Record<string, unknown>[] {
    return this.sent.map((s) => JSON.parse(s) as Record<string, unknown>);
  }
}

/** A websocket the test drives by hand. */
export class FakeSock implements Sock {
  readyState = 0;
  binaryType: BinaryType = "blob";
  sent: (string | Uint8Array<ArrayBuffer>)[] = [];
  closedWith: number | undefined;
  onopen: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  readonly url: string;
  constructor(url: string) {
    this.url = url;
  }
  send(data: string | Uint8Array<ArrayBuffer>): void {
    this.sent.push(data);
  }
  close(code?: number): void {
    this.closedWith = code;
    this.serverClose(code ?? 1000);
  }
  // The server's side.
  accept(): void {
    this.readyState = 1;
    this.onopen?.({} as Event);
  }
  push(data: unknown): void {
    const d = typeof data === "string" || data instanceof ArrayBuffer ? data : JSON.stringify(data);
    this.onmessage?.({ data: d } as MessageEvent);
  }
  serverClose(code = 1006): void {
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.onclose?.({ code } as CloseEvent);
  }
  json(i: number): unknown {
    const s = this.sent[i];
    return typeof s === "string" ? JSON.parse(s) : s;
  }
}
