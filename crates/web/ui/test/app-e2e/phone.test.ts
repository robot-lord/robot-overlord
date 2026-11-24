// The phone page end to end, without a phone: its own store, `Phone` and
// `RemoteTransport` (wired as the app's entry wires them), over the dev
// bridge to `rlord-device bridge` (the app's engine, native), through the
// real relay to a loopback host (an in-memory daemon with fixture
// sessions, the real `rlord-web`, a fake overlord). `cargo xtask app
// test-e2e` starts the host and the bridge, which was handed the host's
// pairing code as if scanned; the host confirms a pairing by itself.
//
// The steps run in order on one pairing: pair, list, ask, the relay
// restarted, the plugin stopped and started, unpair.
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { type DevSocket, installDevBridge } from "../../src/devBridge";
import { Phone } from "../../src/phone";
import { wirePhone } from "../../src/phoneApp";
import type { OverlordMessages, Snapshot } from "../../src/proto";
import { type Bridge, type BridgeHost, findBridge, type RemoteTransport } from "../../src/remote";
import { Store } from "../../src/store";

const BRIDGE = process.env.RLORD_BRIDGE_URL;
const CONTROL = process.env.RLORD_STACK_CONTROL;
const WAIT = 60_000;

type Maybe<T> = T | null | undefined | false;

/** The last messages from the bridge, for a failure's message. */
const heard: string[] = [];

/** Polls `f` until it gives something truthy. */
async function until<T>(what: string, f: () => Maybe<T> | Promise<Maybe<T>>, ms = WAIT) {
  const end = Date.now() + ms;
  for (;;) {
    const v = await f();
    if (v) return v;
    if (Date.now() > end) {
      throw new Error(`timed out waiting for ${what}; the bridge said:\n  ${heard.join("\n  ")}`);
    }
    await new Promise((r) => setTimeout(r, 50));
  }
}

let n = 0;
/** One command to the host stack (by its control folder); the event it said. */
async function stack(cmd: string): Promise<Record<string, unknown>> {
  if (!CONTROL) throw new Error("RLORD_STACK_CONTROL: run `cargo xtask app test-e2e`");
  n += 1;
  const name = `${String(n).padStart(3, "0")}-${cmd}`;
  const done = join(CONTROL, `${name}.done`);
  writeFileSync(join(CONTROL, `${name}.cmd`), cmd);
  await until(`the host's ${cmd}`, () => existsSync(done));
  const text = readFileSync(done, "utf8");
  const ev = (text ? JSON.parse(text) : {}) as Record<string, unknown>;
  if (ev.event === "error") throw new Error(`host stack: ${String(ev.message)}`);
  return ev;
}

/** The host's paired devices' ids (from `relay.status`). */
async function devices(): Promise<string[]> {
  const ev = await stack("status");
  const st = ev.status as { devices?: { id?: string }[] };
  return (st.devices ?? []).map((d) => d.id ?? "");
}

describe("the phone page over the bridge", () => {
  const store = new Store();
  const phone = new Phone();
  let hellos = 0;
  let t: RemoteTransport;
  let ws: DevSocket;

  /** Waits for a hello after `n` of them (a new session with the host). */
  const helloAfter = (seen: number) => until(`hello #${seen + 1}`, () => hellos > seen);

  /** The pairing screen; a pairing that ended without success fails the test at once. */
  const pairing = () => {
    const p = phone.pairing.value;
    if (p.kind === "start" && p.reason) throw new Error(`the pairing ended: ${p.reason}`);
    return p;
  };

  /** The overlord's messages' text. */
  const chat = async () => {
    const m = await t.request<OverlordMessages>("overlord.messages", {});
    return m.messages.map((x) => x.text);
  };

  beforeAll(() => {
    if (!BRIDGE) throw new Error("RLORD_BRIDGE_URL: run `cargo xtask app test-e2e`");
    const w: BridgeHost = {};
    ws = installDevBridge(w, BRIDGE, WebSocket);
    const shell = findBridge(w);
    if (!shell) throw new Error("the dev bridge isn't the shell's");
    const bridge: Bridge = {
      send: (text) => shell.send(text),
      listen: (recv) =>
        shell.listen((text) => {
          heard.push(text.slice(0, 300));
          if (heard.length > 12) heard.shift();
          recv(text);
        }),
    };
    t = wirePhone({
      store,
      phone,
      bridge,
      go: () => {},
      onHello: () => {
        hellos += 1;
      },
    });
  });

  // Each step builds on the last: after a failure the rest are skipped.
  let failed = false;
  beforeEach((ctx) => {
    if (failed) ctx.skip();
  });
  afterEach((ctx) => {
    if (ctx.task.result?.state === "fail") failed = true;
  });

  afterAll(() => {
    t?.stop();
    ws?.close();
  });

  it("pairs by the host's code, confirmed on the host", async () => {
    // The bridge scanned the code at start; the page's `ready` replays it.
    const scanned = await until("the scanned code", () => {
      const p = phone.pairing.value;
      return p.kind === "scanned" && p;
    });
    expect(scanned.relay).toMatch(/^ws:\/\/127\.0\.0\.1:\d+$|^http:\/\/127\.0\.0\.1:\d+$/);
    t.send({ app: "pair_continue", name: "e2e phone" });
    const waiting = await until("the code to compare", () => {
      const p = pairing();
      return p.kind === "waiting" && p;
    });
    expect(waiting.code).toMatch(/^\d{6}$/);
    await until("paired", () => pairing().kind === "done");
    await until("live", () => phone.conn.value?.state === "live");
    await until("the store connected", () => store.conn.value === "open" && store.snapshot.value);
    t.send({ app: "status" });
    const st = await until(
      "status",
      () => phone.status.value?.state === "live" && phone.status.value,
    );
    expect(st.device_name).toBe("e2e phone");
    expect(st.keys).toBe("software");
    expect(await devices()).toContain(st.device_id);
  });

  it("lists the host's sessions", async () => {
    const snap = store.snapshot.value as Snapshot;
    const names = snap.rows.map((r) => r.name);
    for (const i of [0, 1, 2]) expect(names).toContain(`fixture session ${i}`);
    // A device's snapshot carries no relay section.
    expect((snap as unknown as Record<string, unknown>).relay).toBeUndefined();
    const id = snap.rows.find((r) => r.name === "fixture session 0")?.id;
    const m = await t.request<{ session: { id: string }; messages: unknown[] }>(
      "session.messages",
      { ref: id },
    );
    expect(m.session.id).toBe(id);
    // The shell's own message, served by the bridge.
    t.send({ app: "settings" });
    await until("settings", () => phone.settings.value?.platform === "bridge");
  });

  it("asks the overlord and reads its answer", async () => {
    const r = await t.request<{ sent?: boolean }>("overlord.ask", { text: "hello from the phone" });
    expect(r.sent).toBe(true);
    const answer = 'Fake overlord here: you said "hello from the phone".';
    const seen = await until("the overlord's answer", async () => {
      const m = await chat();
      return m.includes(answer) && m;
    });
    expect(seen).toContain("hello from the phone");
    expect(phone.outbox.value).toEqual([]);
  });

  it("recovers from the relay killed and restarted", async () => {
    const seen = hellos;
    await stack("restart-relay");
    await helloAfter(seen);
    await until("live", () => phone.conn.value?.state === "live");
    const snap = await t.request<Snapshot>("snapshot");
    expect(snap.rows.length).toBeGreaterThanOrEqual(3);
  });

  it("recovers from the plugin killed and restarted, and runs what was asked meanwhile", async () => {
    const seen = hellos;
    await stack("stop-plugin");
    await until("the host offline", () => phone.conn.value?.state === "host_offline");
    // Asked while the host is away: it waits in the outbox.
    const ask = t.request<{ sent?: boolean }>("overlord.ask", { text: "while you were away" });
    await until("the ask in the outbox", () => phone.outbox.value.length > 0);
    // (The supervisor settles after a stop before the next start.)
    await new Promise((r) => setTimeout(r, 1000));
    await stack("start-plugin");
    await helloAfter(seen);
    expect((await ask).sent).toBe(true);
    const answer = 'Fake overlord here: you said "while you were away".';
    await until("the answer to what was asked meanwhile", async () =>
      (await chat()).includes(answer),
    );
    await until("the outbox emptied", () => phone.outbox.value.length === 0);
  });

  it("unpairs, on both sides", async () => {
    const id = phone.status.value?.device_id;
    expect(id).toBeTruthy();
    t.send({ app: "unpair" });
    await until("unpaired", () => phone.conn.value?.state === "unpaired");
    expect(phone.pairing.value.kind).toBe("start");
    expect(phone.hello.value).toBeNull();
    const end = Date.now() + WAIT;
    while ((await devices()).includes(id as string)) {
      if (Date.now() > end) throw new Error("the host still lists the device");
      await new Promise((r) => setTimeout(r, 200));
    }
  });
});
