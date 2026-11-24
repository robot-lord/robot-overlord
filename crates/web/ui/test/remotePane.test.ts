// A pane over the bridge (RemotePaneLink) against RemoteTransport on a
// fake bridge: the open and its answer, bytes to the screen, acks at 16
// KiB and at 50 ms of what was parsed, `closed`, reopening at a new
// session's hello, and a stream id used again only after its `closed`.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { PaneState, Screen } from "../src/paneLink";
import { RemoteTransport } from "../src/remote";
import {
  ACK_BYTES,
  ACK_MS,
  fromBase64,
  RemotePaneLink,
  RETRY_FIRST,
  StreamIds,
  toBase64,
} from "../src/remotePane";
import { FakeBridge } from "./fake";

const HELLO = { event: "hello", version: "1", rev: "r-0", page: "dev1", device: "dev1", api: 2 };

/** A screen that parses when told to (xterm.js's write callback). */
class FakeScreen implements Screen {
  cols = 52;
  rows = 40;
  resets = 0;
  out: number[] = [];
  waiting: (() => void)[] = [];
  /** Parse at once (else `parse()` runs what waits). */
  eager = true;
  reset(): void {
    this.resets++;
    this.out = [];
  }
  write(bytes: Uint8Array, parsed?: () => void): void {
    this.out.push(...bytes);
    if (!parsed) return;
    if (this.eager) parsed();
    else this.waiting.push(parsed);
  }
  parse(): void {
    const w = this.waiting;
    this.waiting = [];
    for (const f of w) f();
  }
  text(): string {
    return new TextDecoder().decode(new Uint8Array(this.out));
  }
}

const OPENED = {
  id: "fake-overlord",
  name: "overlord",
  overlord: true,
  input: true,
  control: true,
  cols: 52,
  rows: 40,
  window: { cols: 52, rows: 40 },
};

function harness(target = "overlord") {
  const bridge = new FakeBridge();
  const t = new RemoteTransport(
    bridge,
    { state: () => {}, hello: () => {}, changed: () => {}, app: () => {} },
    1,
  );
  const screen = new FakeScreen();
  const states: PaneState[] = [];
  const make = () =>
    new RemotePaneLink({ bridge: t, target, screen, onState: (s) => states.push(s) });
  /** Every message the page sent, parsed. */
  const sent = () => bridge.all();
  /** The pane messages sent (`pane` key), and the pane.open requests. */
  const panes = () => sent().filter((m) => typeof m.pane === "string");
  const opens = () => sent().filter((m) => m.op === "pane.open");
  /** Answer the last pane.open. */
  const answer = (ok: object = OPENED) => {
    const o = opens().at(-1);
    if (!o) throw new Error("no pane.open sent");
    const args = o.args as { stream: number };
    bridge.push({ id: o.id, ok: { stream: args.stream, ...ok } });
  };
  const refuse = (code: string, message = code) => {
    const o = opens().at(-1);
    if (!o) throw new Error("no pane.open sent");
    bridge.push({ id: o.id, error: { code, message } });
  };
  const data = (stream: number, bytes: Uint8Array | string) =>
    bridge.push({
      event: "pane",
      stream,
      type: "data",
      b: toBase64(typeof bytes === "string" ? new TextEncoder().encode(bytes) : bytes),
    });
  const closed = (stream: number, reason: string, message = "") =>
    bridge.push({ event: "pane", stream, type: "closed", reason, message });
  return { bridge, t, screen, states, make, sent, panes, opens, answer, refuse, data, closed };
}

const settle = () => vi.advanceTimersByTimeAsync(0);

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("base64 and stream ids", () => {
  it("round-trips bytes, any length", () => {
    const all = Uint8Array.from({ length: 70_000 }, (_, i) => i % 256);
    expect(fromBase64(toBase64(all))).toEqual(all);
    expect(toBase64(new TextEncoder().encode("hi"))).toBe("aGk=");
  });

  it("takes the lowest free id, 1 to 250", () => {
    const ids = new StreamIds();
    expect([ids.take(), ids.take(), ids.take()]).toEqual([1, 2, 3]);
    ids.free(2);
    expect(ids.take()).toBe(2);
    for (let i = 4; i <= 250; i++) expect(ids.take()).toBe(i);
    expect(ids.take()).toBeNull();
    ids.clear();
    expect(ids.take()).toBe(1);
  });
});

describe("a pane over the bridge", () => {
  it("opens with its size, resets at the answer, then draws the stream's bytes", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    expect(h.opens()[0]).toMatchObject({
      op: "pane.open",
      args: { stream: 1, target: "overlord", cols: 52, rows: 40 },
    });
    expect(p.state.kind).toBe("connecting");
    h.answer();
    await settle();
    expect(h.screen.resets).toBe(1);
    expect(p.state).toEqual({
      kind: "open",
      id: "fake-overlord",
      name: "overlord",
      control: true,
      input: true,
      cols: 52,
      rows: 40,
      window: { cols: 52, rows: 40 },
    });
    h.data(1, "> ");
    h.data(2, "not ours");
    expect(h.screen.text()).toBe("> ");
  });

  it("sends input, resize and close as pane messages", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    // Nothing goes before the open is answered.
    p.input("early");
    h.answer();
    await settle();
    p.input("hello\r");
    p.input(new Uint8Array([0x1b, 0x80]));
    p.resize(60, 30);
    p.stop();
    expect(h.panes()).toEqual([
      { pane: "input", stream: 1, b: toBase64(new TextEncoder().encode("hello\r")) },
      { pane: "input", stream: 1, b: toBase64(new Uint8Array([0x1b, 0x80])) },
      { pane: "resize", stream: 1, cols: 60, rows: 30 },
      { pane: "close", stream: 1 },
    ]);
  });

  it("acknowledges what was parsed at 16 KiB at once", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    h.answer();
    await settle();
    h.screen.eager = false;
    const chunk = new Uint8Array(8 * 1024);
    h.data(1, chunk);
    h.data(1, chunk);
    // Received, not parsed yet: no ack.
    expect(h.panes()).toEqual([]);
    h.screen.parse();
    expect(h.panes()).toEqual([{ pane: "ack", stream: 1, bytes: ACK_BYTES, frames: 2 }]);
    // Cumulative.
    h.data(1, chunk);
    h.data(1, chunk);
    h.screen.parse();
    expect(h.panes().at(-1)).toEqual({ pane: "ack", stream: 1, bytes: 2 * ACK_BYTES, frames: 4 });
    expect(p.state.kind).toBe("open");
  });

  it("acknowledges a little within 50 ms of parsing it", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    h.answer();
    await settle();
    h.data(1, "abc");
    h.data(1, "de");
    await vi.advanceTimersByTimeAsync(ACK_MS - 1);
    expect(h.panes()).toEqual([]);
    await vi.advanceTimersByTimeAsync(1);
    expect(h.panes()).toEqual([{ pane: "ack", stream: 1, bytes: 5, frames: 2 }]);
    // Nothing new: nothing more.
    await vi.advanceTimersByTimeAsync(500);
    expect(h.panes()).toHaveLength(1);
    expect(p.state.kind).toBe("open");
  });

  it("says not_running and not_found as the local pane does, and frees the id", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    h.refuse("not_running", "it isn't running");
    await settle();
    expect(p.state.kind).toBe("not_running");
    await vi.advanceTimersByTimeAsync(60_000);
    expect(h.opens()).toHaveLength(1);
    // The refused open's id is free: the next open uses it.
    p.connect();
    expect(h.opens()[1]).toMatchObject({ args: { stream: 1 } });
    h.refuse("not_found", "no such session");
    await settle();
    expect(p.state).toEqual({ kind: "not_found", message: "no such session" });
  });

  it("waits for the computer when the engine can't send the open", async () => {
    const h = harness();
    const p = h.make();
    // No hello yet: nothing is asked.
    p.connect();
    expect(h.opens()).toEqual([]);
    expect(p.state).toEqual({ kind: "retrying", message: "waiting for your computer" });
    // The hello opens it.
    h.bridge.push(HELLO);
    expect(h.opens()).toHaveLength(1);
    h.refuse("host_offline");
    await settle();
    expect(p.state.kind).toBe("retrying");
    await vi.advanceTimersByTimeAsync(60_000);
    expect(h.opens()).toHaveLength(1);
  });

  it("a stream that ended opens again after a while; a failed open backs off", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    h.answer();
    await settle();
    h.closed(1, "ended");
    expect(p.state).toEqual({ kind: "retrying", message: "it ended" });
    await vi.advanceTimersByTimeAsync(RETRY_FIRST);
    expect(h.opens()).toHaveLength(2);
    // The ended stream's id is free again: used.
    expect(h.opens()[1]).toMatchObject({ args: { stream: 1 } });
    h.refuse("too_many", "two panes are open");
    await settle();
    await vi.advanceTimersByTimeAsync(RETRY_FIRST);
    expect(h.opens()).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(RETRY_FIRST);
    expect(h.opens()).toHaveLength(3);
  });

  it("a lost session closes it (`link`); the next hello reopens it, from a fresh screen", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    h.answer();
    await settle();
    h.data(1, "before");
    h.bridge.push({ event: "conn", state: "connecting" });
    h.closed(1, "link");
    expect(p.state.kind).toBe("retrying");
    await vi.advanceTimersByTimeAsync(60_000);
    expect(h.opens()).toHaveLength(1);
    h.bridge.push(HELLO);
    expect(h.opens()).toHaveLength(2);
    expect(h.opens()[1]).toMatchObject({ args: { stream: 1, target: "overlord" } });
    h.answer();
    await settle();
    expect(h.screen.resets).toBe(2);
    expect(h.screen.text()).toBe("");
    // No close was sent for the old session's stream.
    expect(h.panes().filter((m) => m.pane === "close")).toEqual([]);
  });

  it("a hello with no `link` closed before it still reopens (nothing crosses sessions)", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    h.answer();
    await settle();
    h.bridge.push(HELLO);
    expect(h.opens()).toHaveLength(2);
    expect(h.opens()[1]).toMatchObject({ args: { stream: 1 } });
    expect(p.state.kind).toBe("connecting");
  });

  it("reuses a stream id only after that id's closed", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    h.answer();
    await settle();
    // Opened again (a switch of overlord): the old stream is closing.
    p.connect();
    expect(h.panes()).toEqual([{ pane: "close", stream: 1 }]);
    expect(h.opens()[1]).toMatchObject({ args: { stream: 2 } });
    h.answer();
    await settle();
    // The old stream's late bytes are not drawn.
    h.data(1, "old");
    h.data(2, "new");
    expect(h.screen.text()).toBe("new");
    // Its closed frees id 1: the next open takes it.
    h.closed(1, "closed");
    expect(p.state.kind).toBe("open");
    p.connect();
    expect(h.opens()[2]).toMatchObject({ args: { stream: 1 } });
  });

  it("an answer for an open given up on is dropped, and that stream closed", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    p.stop();
    expect(h.panes()).toEqual([{ pane: "close", stream: 1 }]);
    h.answer();
    await settle();
    expect(h.screen.resets).toBe(0);
    expect(p.state.kind).toBe("connecting");
    // After stop a hello opens nothing.
    h.bridge.push(HELLO);
    expect(h.opens()).toHaveLength(1);
  });

  it("closes shortly after the app is hidden, and reopens when shown", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    h.answer();
    await settle();
    p.visible(false);
    await vi.advanceTimersByTimeAsync(3000);
    expect(p.state.kind).toBe("hidden");
    expect(h.panes()).toEqual([{ pane: "close", stream: 1 }]);
    // A hello while hidden opens nothing.
    h.bridge.push(HELLO);
    expect(h.opens()).toHaveLength(1);
    p.visible(true);
    expect(h.opens()).toHaveLength(2);
  });

  it("a revoked device's pane stops", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    h.answer();
    await settle();
    h.closed(1, "revoked", "this phone was revoked");
    expect(p.state).toEqual({ kind: "not_found", message: "this phone was revoked" });
    await vi.advanceTimersByTimeAsync(60_000);
    expect(h.opens()).toHaveLength(1);
  });

  it("a view-only answer says so", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const p = h.make();
    p.connect();
    h.answer({ ...OPENED, input: false, control: false, cols: 100, rows: 30 });
    await settle();
    expect(p.state).toMatchObject({ kind: "open", input: false, control: false, cols: 100 });
  });
});
