// A pane over the phone's bridge: one stream of the device's session with
// the host (docs/working/web-ui-remote.md, "Slice 5 build", "Engine and
// page (the bridge)"). The terminal (term.ts) draws it as it draws the
// local page's pane socket.
//
// - `pane.open {stream, target, cols, rows}` is an ordinary request; its
//   answer comes before the stream's first byte, so the screen is reset
//   there (tmux draws the whole screen for a new view client).
// - Bytes come as `{"event":"pane","stream","type":"data","b"}` (one per
//   host frame), base64. The page acknowledges what xterm.js has parsed
//   (its write callback), cumulative bytes and frames, every 16 KiB or
//   50 ms: the host sends only under that credit.
// - A stream ends with `closed`: the host's reasons, or `link` when the
//   session ended. A stream id is used again only after its `closed` (or
//   in a new session, where nothing of the old one lives).
// - Reconnecting is reopening: a new session's `hello` opens the pane
//   anew, never resumes it.

import { HIDE_CLOSE_MS, type PaneLink, type PaneState, type Screen } from "./paneLink";
import type { PageOp, PaneCloseReason, PaneEvent, PaneMessage, PaneOpened } from "./proto";
import { ApiError } from "./transport";

/** Acknowledge after this much parsed... */
export const ACK_BYTES = 16 * 1024;
/** ...or this long after the first byte not yet acknowledged was parsed. */
export const ACK_MS = 50;
/** Stream ids a pane may use (251 to 255 are reserved). */
export const STREAM_FIRST = 1;
export const STREAM_LAST = 250;
/** A failed open tries again after this, doubling up to the last. */
export const RETRY_FIRST = 1000;
export const RETRY_LAST = 15_000;

const enc = new TextEncoder();

/** Bytes as standard base64 with padding (what the engine reads). */
export function toBase64(bytes: Uint8Array): string {
  let bin = "";
  // (In chunks: a spread of 64 KiB of arguments is too many for some engines.)
  for (let i = 0; i < bytes.length; i += 0x2000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x2000));
  }
  return btoa(bin);
}

export function fromBase64(b: string): Uint8Array {
  const bin = atob(b);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** The stream ids in use in this session: one is free again only at its `closed`. */
export class StreamIds {
  private used = new Set<number>();

  /** The lowest free id, now in use; null when all are. */
  take(): number | null {
    for (let id = STREAM_FIRST; id <= STREAM_LAST; id++) {
      if (!this.used.has(id)) {
        this.used.add(id);
        return id;
      }
    }
    return null;
  }

  free(id: number): void {
    this.used.delete(id);
  }

  has(id: number): boolean {
    return this.used.has(id);
  }

  /** A new session: nothing of the old one is open. */
  clear(): void {
    this.used.clear();
  }
}

/** What a pane needs of the phone's transport (RemoteTransport; a fake in tests). */
export interface PaneBridge {
  /** A live session is up (a hello came). */
  readonly connected: boolean;
  request<T = unknown>(op: PageOp, args?: object): Promise<T>;
  sendPane(m: PaneMessage): void;
  /**
   * A free stream id with `on` for its events, or null when all are in
   * use. The id stays taken until its `closed` (then `on` hears no more).
   */
  claim(on: (ev: PaneEvent) => void): number | null;
  /** The id was never opened (its open was refused): free it now. */
  unclaim(stream: number): void;
  /** Stop hearing a stream that is closing (its id stays taken until `closed`). */
  mute(stream: number): void;
  /** Every new session's hello; returns a function that stops listening. */
  onHello(f: () => void): () => void;
}

/** What each `closed` reason means for the view. */
function afterClose(reason: PaneCloseReason): "wait" | "retry" | "ended" | "gone" {
  switch (reason) {
    case "link":
      return "wait";
    case "ended":
      return "ended";
    case "revoked":
      return "gone";
    case "closed":
    case "overflow":
    case "failed":
      return "retry";
  }
}

/** One pane over the bridge. */
export class RemotePaneLink implements PaneLink {
  private current: PaneState = { kind: "connecting" };
  private stopped = false;
  /** The stream being opened or open, and whether its open was answered. */
  private stream: number | null = null;
  private opened = false;
  /** Bumped by each open: an answer for an older one is dropped. */
  private gen = 0;
  /** What xterm.js has parsed of this stream, and what was acknowledged. */
  private parsedBytes = 0;
  private parsedFrames = 0;
  private ackedBytes = 0;
  private ackTimer: ReturnType<typeof setTimeout> | null = null;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private hideTimer: ReturnType<typeof setTimeout> | null = null;
  private backoff = RETRY_FIRST;
  private readonly unHello: () => void;

  private readonly bridge: PaneBridge;
  private readonly target: string;
  private readonly screen: Screen;
  private readonly onState: (s: PaneState) => void;

  constructor(opts: {
    bridge: PaneBridge;
    target: string;
    screen: Screen;
    onState: (s: PaneState) => void;
  }) {
    this.bridge = opts.bridge;
    this.target = opts.target;
    this.screen = opts.screen;
    this.onState = opts.onState;
    // A new session: whatever was open lived in the old one. Open anew.
    this.unHello = this.bridge.onHello(() => {
      this.stream = null;
      this.opened = false;
      if (this.stopped || this.current.kind === "hidden") return;
      if (this.current.kind === "not_found") return;
      this.connect();
    });
  }

  get state(): PaneState {
    return this.current;
  }

  /** The stream in use (tests). */
  get streamId(): number | null {
    return this.stream;
  }

  connect(): void {
    if (this.stopped) return;
    this.clearRetry();
    this.drop();
    if (!this.bridge.connected) {
      // Opened at the next hello.
      this.set({ kind: "retrying", message: "waiting for your computer" });
      return;
    }
    const gen = ++this.gen;
    const stream = this.bridge.claim((ev) => this.event(gen, ev));
    if (stream === null) {
      this.later("too many panes open");
      return;
    }
    this.stream = stream;
    this.opened = false;
    this.set({ kind: "connecting" });
    this.bridge
      .request<PaneOpened>("pane.open", {
        stream,
        target: this.target,
        cols: this.screen.cols,
        rows: this.screen.rows,
      })
      .then((ok) => this.answered(gen, stream, ok))
      .catch((e: unknown) => this.refused(gen, stream, e));
  }

  input(data: string | Uint8Array<ArrayBuffer>): void {
    if (this.current.kind !== "open" || this.stream === null) return;
    const bytes = typeof data === "string" ? enc.encode(data) : data;
    if (bytes.length === 0) return;
    this.bridge.sendPane({ pane: "input", stream: this.stream, b: toBase64(bytes) });
  }

  resize(cols: number, rows: number): void {
    if (this.current.kind !== "open" || this.stream === null) return;
    this.bridge.sendPane({ pane: "resize", stream: this.stream, cols, rows });
  }

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
      this.clearRetry();
      this.drop();
      this.set({ kind: "hidden" });
    }, HIDE_CLOSE_MS);
  }

  stop(): void {
    this.stopped = true;
    this.clearRetry();
    if (this.hideTimer) clearTimeout(this.hideTimer);
    this.drop();
    this.unHello();
  }

  private answered(gen: number, stream: number, ok: PaneOpened): void {
    if (gen !== this.gen || this.stream !== stream) return;
    this.opened = true;
    this.backoff = RETRY_FIRST;
    this.parsedBytes = 0;
    this.parsedFrames = 0;
    this.ackedBytes = 0;
    // A fresh screen for each open: tmux draws it whole.
    this.screen.reset();
    this.set({
      kind: "open",
      id: ok.id ?? null,
      name: ok.name ?? null,
      control: typeof ok.control === "boolean" ? ok.control : null,
      input: ok.input,
      cols: ok.cols ?? null,
      rows: ok.rows ?? null,
      window: ok.window ?? null,
    });
  }

  private refused(gen: number, stream: number, e: unknown): void {
    // Never opened: nothing will close it.
    this.bridge.unclaim(stream);
    if (gen !== this.gen || this.stream !== stream) return;
    this.stream = null;
    const code = e instanceof ApiError ? e.code : "failed";
    const message = e instanceof Error ? e.message : String(e);
    if (code === "not_running") {
      this.set({ kind: "not_running" });
    } else if (code === "not_found") {
      this.set({ kind: "not_found", message });
    } else if (code === "disconnected" || code === "host_offline") {
      // Opened at the next hello.
      this.set({ kind: "retrying", message: "waiting for your computer" });
    } else {
      this.later(message || code);
    }
  }

  private event(gen: number, ev: PaneEvent): void {
    if (gen !== this.gen || ev.stream !== this.stream) return;
    if (ev.type === "data") {
      if (!this.opened) return;
      const bytes = fromBase64(ev.b);
      this.screen.write(bytes, () => this.parsed(gen, bytes.length));
      return;
    }
    // closed: the id is free again (the transport frees it).
    this.stream = null;
    this.opened = false;
    this.stopAck();
    if (this.stopped) return;
    switch (afterClose(ev.reason)) {
      case "wait":
        this.set({ kind: "retrying", message: "connection lost" });
        // A hello may have come already (the engine says `link` first).
        if (this.bridge.connected) this.later("connection lost");
        return;
      case "ended":
        // The session stopped or the overlord restarted: opening again
        // says not_running, or shows the new one.
        this.later("it ended");
        return;
      case "gone":
        this.set({ kind: "not_found", message: ev.message || "this phone was revoked" });
        return;
      case "retry":
        if (ev.reason === "closed") return;
        this.later(ev.message || ev.reason);
    }
  }

  /** xterm.js took in a frame: acknowledge at 16 KiB, else within 50 ms. */
  private parsed(gen: number, n: number): void {
    if (gen !== this.gen || !this.opened) return;
    this.parsedBytes += n;
    this.parsedFrames += 1;
    if (this.parsedBytes - this.ackedBytes >= ACK_BYTES) {
      this.ack();
    } else if (!this.ackTimer) {
      this.ackTimer = setTimeout(() => {
        this.ackTimer = null;
        this.ack();
      }, ACK_MS);
    }
  }

  private ack(): void {
    if (this.ackTimer) clearTimeout(this.ackTimer);
    this.ackTimer = null;
    if (this.stream === null || !this.opened) return;
    this.ackedBytes = this.parsedBytes;
    this.bridge.sendPane({
      pane: "ack",
      stream: this.stream,
      bytes: this.parsedBytes,
      frames: this.parsedFrames,
    });
  }

  private stopAck(): void {
    if (this.ackTimer) clearTimeout(this.ackTimer);
    this.ackTimer = null;
  }

  /** Close the stream, if any: the id stays taken until its `closed`. */
  private drop(): void {
    this.stopAck();
    const s = this.stream;
    this.stream = null;
    this.opened = false;
    // An answer still to come for it is dropped (`gen`).
    this.gen++;
    if (s === null) return;
    this.bridge.mute(s);
    this.bridge.sendPane({ pane: "close", stream: s });
  }

  private later(message: string): void {
    this.set({ kind: "retrying", message });
    this.clearRetry();
    this.retryTimer = setTimeout(() => this.connect(), this.backoff);
    this.backoff = Math.min(this.backoff * 2, RETRY_LAST);
  }

  private clearRetry(): void {
    if (this.retryTimer) clearTimeout(this.retryTimer);
    this.retryTimer = null;
  }

  private set(s: PaneState): void {
    this.current = s;
    this.onState(s);
  }
}
