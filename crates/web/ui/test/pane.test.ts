import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { BACKOFF_FIRST } from "../src/api";
import { HIDE_CLOSE_MS, PaneSocket, type PaneState, type Screen } from "../src/pane";
import { CLOSE_UNAUTHORIZED } from "../src/proto";
import { FakeSock } from "./fake";

class FakeScreen implements Screen {
  cols = 100;
  rows = 30;
  resets = 0;
  out: number[] = [];
  reset(): void {
    this.resets++;
    this.out = [];
  }
  write(bytes: Uint8Array): void {
    this.out.push(...bytes);
  }
}

function harness(target = "overlord") {
  const socks: FakeSock[] = [];
  const states: PaneState["kind"][] = [];
  const screen = new FakeScreen();
  const p = new PaneSocket({
    url: "ws://127.0.0.1:47811/pane",
    token: "t".repeat(64),
    target,
    screen,
    onState: (s) => states.push(s.kind),
    open: (url) => {
      const s = new FakeSock(url);
      socks.push(s);
      return s;
    },
  });
  const last = () => {
    const s = socks.at(-1);
    if (!s) throw new Error("no socket");
    return s;
  };
  const opened = (id = "o1") => {
    last().accept();
    last().push({ type: "opened", target, id, name: "overlord-1" });
  };
  return { p, socks, states, screen, last, opened };
}

const bytes = (s: string) => new TextEncoder().encode(s).buffer;

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("pane socket", () => {
  it("sends the token, then what to open at the screen's size", () => {
    const h = harness("01SESSION");
    h.p.connect();
    h.last().accept();
    expect(h.last().binaryType).toBe("arraybuffer");
    expect(h.last().json(0)).toEqual({ auth: "t".repeat(64) });
    expect(h.last().json(1)).toEqual({ type: "open", target: "01SESSION", cols: 100, rows: 30 });
  });

  it("resets the screen at opened, then writes output and sends input", () => {
    const h = harness();
    h.p.connect();
    h.p.input("ignored before opened");
    h.opened();
    expect(h.screen.resets).toBe(1);
    expect(h.p.state).toEqual({ kind: "open", id: "o1", name: "overlord-1" });
    h.last().push(bytes("hi"));
    expect(new TextDecoder().decode(new Uint8Array(h.screen.out))).toBe("hi");
    h.p.input("ls\r");
    h.p.input(new Uint8Array([0x1b, 0x80]));
    h.p.resize(120, 40);
    const sent = h.last().sent.slice(2);
    expect(new TextDecoder().decode(sent[0] as Uint8Array)).toBe("ls\r");
    expect(sent[1]).toEqual(new Uint8Array([0x1b, 0x80]));
    expect(JSON.parse(sent[2] as string)).toEqual({ type: "resize", cols: 120, rows: 40 });
    expect(sent).toHaveLength(3);
  });

  it("stops at not_running and not_found; connect tries again", () => {
    const h = harness();
    h.p.connect();
    h.last().accept();
    h.last().push({ type: "closed", reason: "not_running", message: "it isn't running" });
    h.last().serverClose(1000);
    expect(h.p.state.kind).toBe("not_running");
    vi.advanceTimersByTime(60_000);
    expect(h.socks).toHaveLength(1);

    h.p.connect();
    h.last().accept();
    h.last().push({ type: "closed", reason: "not_found", message: "no such session" });
    h.last().serverClose(1000);
    expect(h.p.state).toEqual({ kind: "not_found", message: "no such session" });
    vi.advanceTimersByTime(60_000);
    expect(h.socks).toHaveLength(2);
  });

  it("retries with backoff when lost or ended, and resets the screen again", () => {
    const h = harness();
    h.p.connect();
    h.opened();
    h.last().push({ type: "closed", reason: "ended", message: "" });
    h.last().serverClose(1000);
    expect(h.p.state.kind).toBe("retrying");
    vi.advanceTimersByTime(BACKOFF_FIRST);
    expect(h.socks).toHaveLength(2);
    h.last().serverClose(1006);
    vi.advanceTimersByTime(BACKOFF_FIRST);
    expect(h.socks).toHaveLength(2);
    vi.advanceTimersByTime(BACKOFF_FIRST);
    expect(h.socks).toHaveLength(3);
    h.opened();
    expect(h.screen.resets).toBe(2);
  });

  it("stops for good when the token is refused", () => {
    const h = harness();
    h.p.connect();
    h.last().accept();
    h.last().serverClose(CLOSE_UNAUTHORIZED);
    expect(h.p.state.kind).toBe("unauthorized");
    vi.advanceTimersByTime(60_000);
    expect(h.socks).toHaveLength(1);
  });

  it("closes shortly after the tab is hidden, and reopens when shown", () => {
    const h = harness();
    h.p.connect();
    h.opened();
    h.p.visible(false);
    vi.advanceTimersByTime(HIDE_CLOSE_MS - 1);
    // Shown again in time: nothing happens.
    h.p.visible(true);
    vi.advanceTimersByTime(HIDE_CLOSE_MS);
    expect(h.p.state.kind).toBe("open");

    h.p.visible(false);
    vi.advanceTimersByTime(HIDE_CLOSE_MS);
    expect(h.p.state.kind).toBe("hidden");
    // It said close before going.
    expect(h.socks[0]?.sent.map((s) => (typeof s === "string" ? JSON.parse(s) : s)).at(-1)).toEqual(
      { type: "close" },
    );
    expect(h.socks[0]?.readyState).toBe(3);
    vi.advanceTimersByTime(60_000);
    expect(h.socks).toHaveLength(1);
    h.p.visible(true);
    expect(h.socks).toHaveLength(2);
    expect(h.p.state.kind).toBe("connecting");
  });

  it("a new connect replaces the old socket; stop ends it", () => {
    const h = harness();
    h.p.connect();
    h.opened("o1");
    h.p.connect();
    expect(h.socks[0]?.readyState).toBe(3);
    h.opened("o2");
    expect(h.p.state).toMatchObject({ kind: "open", id: "o2" });
    h.p.stop();
    expect(h.last().readyState).toBe(3);
    h.p.connect();
    expect(h.socks).toHaveLength(2);
  });
});
