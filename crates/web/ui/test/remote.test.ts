// RemoteTransport against a fake bridge: the control socket's request and
// answer shapes, the engine's events, pane streams' events and ids, the
// shell's paste, and a reload that gets its waiting requests back from
// the engine's `outbox` after `{"app":"ready"}`.

import { afterEach, describe, expect, it, vi } from "vitest";
import { Phone, waitingWords } from "../src/phone";
import type { AppEvent, Hello, UiCmd } from "../src/proto";
import { type BridgeHost, connState, findBridge, RECV, RemoteTransport } from "../src/remote";
import { ApiError, type ConnState } from "../src/transport";
import { FakeBridge } from "./fake";

const HELLO = {
  event: "hello",
  version: "1.3.0",
  build: "b1",
  rev: "r-0",
  page: "ab12cd34ef560789",
  api: 1,
};

function harness(firstId = 1) {
  const bridge = new FakeBridge();
  const states: ConnState[] = [];
  const hellos: Hello[] = [];
  const changed: string[] = [];
  const uis: UiCmd[] = [];
  const app: AppEvent[] = [];
  const t = new RemoteTransport(
    bridge,
    {
      state: (s) => states.push(s),
      hello: (h) => hellos.push(h),
      changed: (r) => changed.push(r),
      ui: (c) => uis.push(c),
      app: (e) => app.push(e),
    },
    firstId,
  );
  return { bridge, t, states, hellos, changed, uis, app };
}

describe("RemoteTransport", () => {
  it("says ready first, and is connecting until a hello", () => {
    const h = harness();
    expect(h.bridge.all()).toEqual([{ app: "ready" }]);
    expect(h.states).toEqual(["connecting"]);
    expect(h.t.caps.panes).toBe(true);
    expect(h.t.caps.tell).toBe(false);
    h.bridge.push(HELLO);
    expect(h.states).toEqual(["connecting", "open"]);
    expect(h.hellos[0]?.page).toBe("ab12cd34ef560789");
    expect(h.t.connected).toBe(true);
  });

  it("sends requests in the control socket's shape and matches answers by id", async () => {
    const h = harness(7);
    h.bridge.push(HELLO);
    const a = h.t.request("snapshot", { view: {} });
    const b = h.t.request("overlord.new_session", { description: "hi" });
    expect(h.bridge.json(1)).toEqual({ id: 7, op: "snapshot", args: { view: {} } });
    expect(h.bridge.json(2)).toEqual({
      id: 8,
      op: "overlord.new_session",
      args: { description: "hi" },
    });
    h.bridge.push({ id: 8, error: { code: "outbox_full", message: "16 wait already" } });
    h.bridge.push({ id: 7, ok: { rev: "r-1" } });
    // An answer for no request of this page (one from before a reload): dropped.
    h.bridge.push({ id: 99, ok: {} });
    await expect(a).resolves.toEqual({ rev: "r-1" });
    const e = await b.catch((x: unknown) => x);
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ code: "outbox_full", message: "16 wait already" });
  });

  it("takes requests before any hello (a new session may wait in the outbox)", async () => {
    const h = harness();
    const p = h.t.request("overlord.new_session", { description: "later" });
    expect(h.bridge.json(1)).toMatchObject({ op: "overlord.new_session" });
    h.bridge.push({ id: 1, ok: null });
    await expect(p).resolves.toBeNull();
  });

  it("passes changed, ui and the app's events on, and skips what it doesn't know", () => {
    const h = harness();
    h.bridge.push(HELLO);
    h.bridge.push({ event: "changed", rev: "r-5" });
    h.bridge.push({ event: "ui", cmd: { cmd: "show_list" } });
    const conn = { event: "conn", state: "live", host: "laptop", relay: "https://r.example" };
    h.bridge.push(conn);
    h.bridge.push({ event: "pairing", state: "waiting", code: "123456" });
    h.bridge.push({ event: "outbox", items: [] });
    h.bridge.push({ event: "newer_thing" });
    h.bridge.push("not json");
    h.bridge.push("null");
    expect(h.changed).toEqual(["r-5"]);
    expect(h.uis).toEqual([{ cmd: "show_list" }]);
    expect(h.app.map((e) => e.event)).toEqual(["conn", "pairing", "outbox"]);
    // `live` doesn't move the state: the hello does.
    expect(h.states).toEqual(["connecting", "open"]);
  });

  it("follows the engine's connection state", () => {
    const h = harness();
    h.bridge.push(HELLO);
    h.bridge.push({ event: "conn", state: "host_offline", host: "laptop" });
    expect(h.states.at(-1)).toBe("closed");
    expect(h.t.connected).toBe(false);
    h.bridge.push({ event: "conn", state: "connecting" });
    h.bridge.push({ event: "conn", state: "relay" });
    expect(h.states).toEqual(["connecting", "open", "closed", "connecting"]);
    h.bridge.push(HELLO);
    expect(h.states.at(-1)).toBe("open");
    expect(connState("revoked")).toBe("closed");
    expect(connState("live")).toBeNull();
  });

  it("fails what is still out on stop, and sends nothing after", async () => {
    const h = harness();
    const p = h.t.request("snapshot");
    h.t.stop();
    await expect(p).rejects.toMatchObject({ code: "disconnected" });
    await expect(h.t.request("snapshot")).rejects.toBeInstanceOf(ApiError);
    h.t.send({ app: "status" });
    expect(h.bridge.sent).toHaveLength(2);
  });

  it("a reload gets what waits in the outbox back", () => {
    const outbox = {
      event: "outbox",
      items: [
        { id: "AAAA", op: "overlord.new_session", text: "deploy it", at: 1000, state: "sent" },
        {
          id: "BBBB",
          op: "overlord.new_session",
          at: 2000,
          state: "failed",
          error: { code: "expired", message: "not answered in 24 hours" },
        },
      ],
    };
    // Reloaded: a new page, new transport, nothing of its own.
    const after = new Phone();
    const two = harness();
    expect(two.bridge.all()).toEqual([{ app: "ready" }]);
    expect(after.outbox.value).toEqual([]);
    // The engine replays on ready.
    two.bridge.push({ event: "conn", state: "host_offline", host: "laptop" });
    two.bridge.push(outbox);
    for (const e of two.app) after.apply(e);
    expect(after.outbox.value.map(waitingWords)).toEqual([
      "New session: deploy it · sent, waiting for your computer",
      "New session · not answered in 24 hours",
    ]);
    expect(after.conn.value?.state).toBe("host_offline");
  });
});

describe("pane streams on the transport", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("routes a stream's events to its pane, and frees the id at its closed", () => {
    const h = harness();
    h.bridge.push(HELLO);
    const got: unknown[] = [];
    const a = h.t.claim((ev) => got.push(["a", ev.type]));
    const b = h.t.claim((ev) => got.push(["b", ev.type]));
    expect([a, b]).toEqual([1, 2]);
    h.bridge.push({ event: "pane", stream: 2, type: "data", b: "aGk=" });
    h.bridge.push({ event: "pane", stream: 1, type: "closed", reason: "ended" });
    // An event for a stream nobody has: dropped.
    h.bridge.push({ event: "pane", stream: 9, type: "data", b: "aGk=" });
    expect(got).toEqual([
      ["b", "data"],
      ["a", "closed"],
    ]);
    // 1 is free again; 2 isn't.
    expect(h.t.claim(() => {})).toBe(1);
    expect(h.t.claim(() => {})).toBe(3);
  });

  it("a muted stream is heard no more, but its id waits for its closed", () => {
    const h = harness();
    h.bridge.push(HELLO);
    const got: string[] = [];
    const a = h.t.claim((ev) => got.push(ev.type));
    expect(a).toBe(1);
    h.t.mute(1);
    h.bridge.push({ event: "pane", stream: 1, type: "data", b: "aGk=" });
    expect(h.t.claim(() => {})).toBe(2);
    h.bridge.push({ event: "pane", stream: 1, type: "closed", reason: "closed" });
    expect(got).toEqual([]);
    expect(h.t.claim(() => {})).toBe(1);
    // An open refused: the id is free at once.
    h.t.unclaim(1);
    expect(h.t.claim(() => {})).toBe(1);
  });

  it("a new session's hello frees every id, then tells the panes", () => {
    const h = harness();
    h.bridge.push(HELLO);
    h.t.claim(() => {});
    h.t.claim(() => {});
    const order: string[] = [];
    const off = h.t.onHello(() => order.push(`hello, next id ${String(h.t.claim(() => {}))}`));
    h.bridge.push(HELLO);
    expect(order).toEqual(["hello, next id 1"]);
    off();
    h.bridge.push(HELLO);
    expect(order).toHaveLength(1);
  });

  it("sends pane messages as they are", () => {
    const h = harness();
    h.t.sendPane({ pane: "ack", stream: 3, bytes: 10, frames: 1 });
    expect(h.bridge.json(1)).toEqual({ pane: "ack", stream: 3, bytes: 10, frames: 1 });
  });

  it("asks the shell to paste and takes its answer; copy is one message", async () => {
    vi.useFakeTimers();
    const h = harness();
    const p = h.t.paste();
    expect(h.bridge.json(1)).toEqual({ app: "paste" });
    h.bridge.push({ event: "paste", text: "from the pasteboard" });
    await expect(p).resolves.toBe("from the pasteboard");
    h.t.copy("the screen");
    expect(h.bridge.json(2)).toEqual({ app: "copy", text: "the screen" });
    // A shell that never answers: null, in the end.
    const q = h.t.paste();
    await vi.advanceTimersByTimeAsync(30_000);
    await expect(q).resolves.toBeNull();
  });
});

describe("the bridge", () => {
  it("on iOS: posts to the message handler and takes messages by one fixed function", () => {
    const posted: string[] = [];
    const w: BridgeHost & Record<string, unknown> = {
      webkit: { messageHandlers: { rlord: { postMessage: (t: string) => posted.push(t) } } },
    };
    const b = findBridge(w);
    if (!b) throw new Error("no bridge");
    const got: string[] = [];
    b.listen((t) => got.push(t));
    b.send("{}");
    expect(posted).toEqual(["{}"]);
    const recv = w[RECV] as (t: string) => void;
    recv('{"event":"changed","rev":"x"}');
    expect(got).toEqual(['{"event":"changed","rev":"x"}']);
    // Fixed: a script can't swap the receiver.
    expect(() => {
      w[RECV] = () => {};
    }).toThrow(TypeError);
    expect(RECV).toBe("__rlord_recv");
  });

  it("on Android: the listener object's postMessage and onmessage", () => {
    const posted: string[] = [];
    const rlord = {
      postMessage: (t: string) => posted.push(t),
      onmessage: null as ((ev: { data: unknown }) => void) | null,
    };
    const b = findBridge({ rlord });
    if (!b) throw new Error("no bridge");
    const got: string[] = [];
    b.listen((t) => got.push(t));
    b.send("x");
    rlord.onmessage?.({ data: "y" });
    expect([posted, got]).toEqual([["x"], ["y"]]);
  });

  it("in a plain browser: none", () => {
    expect(findBridge({})).toBeNull();
    expect(findBridge({ webkit: { messageHandlers: {} } })).toBeNull();
  });
});
