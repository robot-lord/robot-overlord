import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  ApiError,
  BACKOFF_FIRST,
  BACKOFF_LAST,
  type ConnState,
  LocalTransport,
  takeToken,
  wsUrl,
} from "../src/api";
import { CLOSE_UNAUTHORIZED } from "../src/proto";
import { FakeSock } from "./fake";

function harness() {
  const socks: FakeSock[] = [];
  const states: ConnState[] = [];
  const changed: string[] = [];
  const hellos: string[] = [];
  const c = new LocalTransport(
    "ws://127.0.0.1:47811/ws",
    "a".repeat(64),
    {
      state: (s) => states.push(s),
      hello: (h) => hellos.push(h.build ?? ""),
      changed: (r) => changed.push(r),
    },
    (url) => {
      const s = new FakeSock(url);
      socks.push(s);
      return s;
    },
  );
  const last = () => {
    const s = socks.at(-1);
    if (!s) throw new Error("no socket");
    return s;
  };
  const up = () => {
    last().accept();
    last().push({ event: "hello", version: "1", build: "b1", rev: "r-0" });
  };
  return { c, socks, states, changed, hellos, last, up };
}

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("control socket", () => {
  it("sends the token first and is open at hello", () => {
    const h = harness();
    expect(h.states).toEqual(["connecting"]);
    h.last().accept();
    expect(h.last().json(0)).toEqual({ auth: "a".repeat(64) });
    expect(h.states).toEqual(["connecting"]);
    h.last().push({ event: "hello", version: "1", build: "b1", rev: "r-0" });
    expect(h.states).toEqual(["connecting", "open"]);
    expect(h.hellos).toEqual(["b1"]);
  });

  it("matches answers to requests by id, in any order", async () => {
    const h = harness();
    h.up();
    const a = h.c.request("snapshot", { view: {} });
    const b = h.c.request("session.get", { ref: "x" });
    const [ra, rb] = [h.last().json(1), h.last().json(2)] as { id: number; op: string }[];
    expect(ra?.op).toBe("snapshot");
    expect(rb?.op).toBe("session.get");
    h.last().push({ id: rb?.id, error: { code: "not_allowed", message: "no" } });
    h.last().push({ id: ra?.id, ok: { rev: "r-1" } });
    await expect(a).resolves.toEqual({ rev: "r-1" });
    await expect(b).rejects.toMatchObject({ code: "not_allowed", message: "no" });
  });

  it("refuses requests before hello and fails pending ones on close", async () => {
    const h = harness();
    await expect(h.c.request("snapshot")).rejects.toBeInstanceOf(ApiError);
    h.up();
    const p = h.c.request("snapshot");
    h.last().serverClose();
    await expect(p).rejects.toMatchObject({ code: "disconnected" });
  });

  it("passes changed events on", () => {
    const h = harness();
    h.up();
    h.last().push({ event: "changed", rev: "r-5" });
    expect(h.changed).toEqual(["r-5"]);
  });

  it("reconnects with backoff, reset by hello", () => {
    const h = harness();
    h.last().serverClose();
    expect(h.states.at(-1)).toBe("closed");
    let wait = BACKOFF_FIRST;
    for (let i = 0; i < 8; i++) {
      const n = h.socks.length;
      vi.advanceTimersByTime(wait - 1);
      expect(h.socks.length).toBe(n);
      vi.advanceTimersByTime(1);
      expect(h.socks.length).toBe(n + 1);
      h.last().serverClose();
      wait = Math.min(wait * 2, BACKOFF_LAST);
    }
    expect(wait).toBe(BACKOFF_LAST);
    vi.advanceTimersByTime(BACKOFF_LAST);
    h.up();
    h.last().serverClose();
    const n = h.socks.length;
    vi.advanceTimersByTime(BACKOFF_FIRST);
    expect(h.socks.length).toBe(n + 1);
  });

  it("stops for good when the token is refused", () => {
    const h = harness();
    h.last().accept();
    h.last().serverClose(CLOSE_UNAUTHORIZED);
    expect(h.states.at(-1)).toBe("unauthorized");
    vi.advanceTimersByTime(60_000);
    expect(h.socks.length).toBe(1);
  });

  it("stays closed after stop", () => {
    const h = harness();
    h.up();
    h.c.stop();
    vi.advanceTimersByTime(60_000);
    expect(h.socks.length).toBe(1);
  });
});

function store() {
  const m = new Map<string, string>();
  return {
    m,
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
  };
}

describe("token", () => {
  it("takes it from the fragment, saves it and clears the address bar", () => {
    const s = store();
    const replaced: string[] = [];
    const t = "0123456789abcdef".repeat(4);
    expect(
      takeToken({ hash: `#t=${t}`, pathname: "/", search: "" }, s, (u) => replaced.push(u)),
    ).toBe(t);
    expect(replaced).toEqual(["/#/"]);
    expect(takeToken({ hash: "#/", pathname: "/", search: "" }, s, () => {})).toBe(t);
  });

  it("is null with neither, and survives storage that throws", () => {
    expect(takeToken({ hash: "#/", pathname: "/", search: "" }, store(), () => {})).toBeNull();
    const broken = {
      getItem: (): string | null => {
        throw new Error("denied");
      },
      setItem: () => {
        throw new Error("denied");
      },
    };
    const t = "f".repeat(64);
    expect(takeToken({ hash: `#t=${t}`, pathname: "/", search: "" }, broken, () => {})).toBe(t);
    expect(takeToken({ hash: "#/", pathname: "/", search: "" }, broken, () => {})).toBeNull();
  });

  it("builds websocket URLs on the page's origin", () => {
    expect(wsUrl("/ws", "http://127.0.0.1:47811/#/s/x")).toBe("ws://127.0.0.1:47811/ws");
    expect(wsUrl("/pane", "http://127.0.0.1:9/?a=1#t=z")).toBe("ws://127.0.0.1:9/pane");
  });
});
