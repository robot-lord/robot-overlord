// The phone entry's wiring, by bridge events: hello hands the store its
// transport and pulls a parked command; the overlord's UI commands run
// with a device's caps (a code-folder or key command is only a notice).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Phone } from "../src/phone";
import { wirePhone } from "../src/phoneApp";
import type { UiCmd } from "../src/proto";
import { Store } from "../src/store";
import { decide } from "../src/ui";
import { REMOTE_CAPS } from "../src/transport";
import cmds from "./fixtures/ui_cmds.json";
import { FakeBridge } from "./fake";

const fx = cmds as unknown as Record<string, UiCmd>;
const cmd = (name: string): UiCmd => {
  const c = fx[name];
  if (!c) throw new Error(`no ${name} in the fixture`);
  return c;
};

const HELLO = { event: "hello", version: "1", build: "b", rev: "r-0", page: "dev1", api: 1 };

function harness() {
  const bridge = new FakeBridge();
  const store = new Store();
  const phone = new Phone();
  const went: string[] = [];
  let hellos = 0;
  const t = wirePhone({
    store,
    phone,
    bridge,
    go: (h) => went.push(h),
    onHello: () => hellos++,
    firstId: 1,
  });
  /** The requests sent, by op. */
  const ops = () => bridge.all().flatMap((m) => (typeof m.op === "string" ? [m.op] : []));
  /** Answer the last request with `op`. */
  const answer = (op: string, ok: unknown) => {
    const m = bridge
      .all()
      .toReversed()
      .find((x) => x.op === op);
    if (!m) throw new Error(`no ${op} sent`);
    bridge.push({ id: m.id, ok });
  };
  return { bridge, store, phone, went, t, ops, answer, hellos: () => hellos };
}

const settle = () => vi.advanceTimersByTimeAsync(0);

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("the phone entry", () => {
  it("at hello: the store connects, and a parked command is pulled", async () => {
    const h = harness();
    expect(h.store.conn.value).toBe("connecting");
    h.bridge.push({ event: "conn", state: "live", host: "laptop" });
    h.bridge.push(HELLO);
    expect(h.store.conn.value).toBe("open");
    expect(h.phone.hello.value?.page).toBe("dev1");
    expect(h.hellos()).toBe(1);
    expect(h.ops()).toContain("page.pending");
    expect(h.ops()).toContain("snapshot");
    h.answer("page.pending", { cmd: "goto", id: "a1", overlord: false });
    await settle();
    expect(h.went).toEqual(["#/s/a1"]);
  });

  it("runs the commands a device handles", async () => {
    const h = harness();
    h.bridge.push(HELLO);
    const push = (c: UiCmd) => h.bridge.push({ event: "ui", cmd: c });
    push(cmd("goto"));
    push({ cmd: "goto", id: "o", overlord: true });
    push(cmd("show_list"));
    expect(h.went.slice(-2)).toEqual(["#/overlord", "#/"]);
    push(cmd("set_view"));
    expect(h.store.view.value.search).toBe("tag:web");
    expect(h.store.selected.value).toBe("alpha");
    push(cmd("new_session"));
    expect(h.store.form.value?.kind).toBe("manual");
    push(cmd("confirm_new_session"));
    expect(h.store.form.value?.kind).toBe("confirm");
    push(cmd("notice"));
    expect(h.store.toast.value).toContain("new-session form");
    // Each one is acknowledged ("got it": no other screen gets it).
    await settle();
    const acks = h.bridge
      .all()
      .filter((m) => m.op === "page.report" && JSON.stringify(m.args) === "{}");
    expect(acks).toHaveLength(7);
  });

  it("a command for a screen the phone hasn't opens nothing", () => {
    const h = harness();
    h.bridge.push(HELLO);
    h.bridge.push({ event: "ui", cmd: cmd("setup") });
    expect(h.store.form.value).toBeNull();
    expect(h.store.toast.value).toContain("code-folder form");
    h.bridge.push({ event: "ui", cmd: cmd("keys") });
    expect(h.store.form.value).toBeNull();
    expect(h.store.toast.value).toContain("key screen");
    // The local page still opens them.
    expect(decide(cmd("setup"), null)[0]?.do).toBe("open");
    expect(decide(cmd("keys"), null, REMOTE_CAPS)[0]?.do).toBe("toast");
  });

  it("the engine's events reach the phone's state", () => {
    const h = harness();
    h.bridge.push({ event: "conn", state: "unpaired" });
    h.bridge.push({ event: "pairing", state: "waiting", code: "654321" });
    expect(h.phone.conn.value?.state).toBe("unpaired");
    expect(h.phone.pairing.value).toEqual({ kind: "waiting", code: "654321" });
    expect(h.store.conn.value).toBe("closed");
  });
});
