// The phone page end to end, without a phone: its own store, `Phone` and
// `RemoteTransport` (wired as the app's entry wires them), over the dev
// bridge to `rlord-device bridge` (the app's engine, native), through the
// real relay to a loopback host (an in-memory daemon with fixture
// sessions, the real `rlord-web`, a fake overlord). `cargo xtask app
// test-e2e` starts the host and the bridge, which was handed the host's
// pairing code as if scanned; the host confirms a pairing by itself.
//
// The steps run in order on one pairing: pair, list, type into the
// overlord's pane (a stream over the bridge, `RemotePaneLink`, drawn into
// a stand-in screen), the relay restarted, the plugin stopped and started
// (the pane reopens; a described new session waits in the outbox), unpair.
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { type DevSocket, installDevBridge } from "../../src/devBridge";
import { Phone } from "../../src/phone";
import { wirePhone } from "../../src/phoneApp";
import type { PaneState, Screen } from "../../src/paneLink";
import type { SessionMessages, Snapshot } from "../../src/proto";
import { type Bridge, type BridgeHost, findBridge, type RemoteTransport } from "../../src/remote";
import { RemotePaneLink } from "../../src/remotePane";
import { Store } from "../../src/store";
import { ApiError } from "../../src/transport";

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

/**
 * A pane's output as text to search: escape sequences dropped, and all
 * white space (tmux may move the cursor over blanks instead of writing
 * them). Not a terminal: `app test-term` reads a real screen.
 */
function plain(text: string): string {
  // (Escape sequences are control characters, on purpose.)
  /* oxlint-disable no-control-regex */
  return text
    .replaceAll(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, "")
    .replaceAll(/\x1b\[[0-9;?<>=]*[ -/]*[@-~]/g, "")
    .replaceAll(/\x1b[()][0-9A-Za-z]/g, "")
    .replaceAll(/\x1b[=>78DEHMNOZc]/g, "")
    .replaceAll(/\s+/g, "");
  /* oxlint-enable no-control-regex */
}

/** A screen that keeps what was written since the last reset, as text. */
class TextScreen implements Screen {
  cols = 120;
  rows = 30;
  private dec = new TextDecoder();
  text = "";
  resets = 0;
  reset(): void {
    this.resets += 1;
    this.text = "";
    this.dec = new TextDecoder();
  }
  write(bytes: Uint8Array, parsed?: () => void): void {
    this.text += this.dec.decode(bytes, { stream: true });
    parsed?.();
  }
  has(s: string): boolean {
    return plain(this.text).includes(plain(s));
  }
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

  /** The overlord's transcript's messages' text (`session.messages` with its id). */
  const transcript = async () => {
    const id = store.snapshot.value?.overlord?.id;
    if (!id) throw new Error("no overlord in the snapshot");
    const m = await t.request<SessionMessages>("session.messages", { ref: id });
    return m.messages.map((x) => x.text);
  };

  /** The overlord's pane, opened once paired (kept open through the steps). */
  const screen = new TextScreen();
  const paneStates: PaneState["kind"][] = [];
  let pane: RemotePaneLink | null = null;
  const paneOpen = () => pane?.state.kind === "open";

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
    pane?.stop();
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
    // The stack confirms at once, so the code may be on screen for less
    // than one poll: every screen the signal shows is watched.
    let code: string | null = null;
    const unwatch = phone.pairing.subscribe((p) => {
      if (p.kind === "waiting") code = p.code;
    });
    t.send({ app: "pair_continue", name: "e2e phone" });
    try {
      await until("the code to compare", () => {
        pairing(); // a pairing that ended without success fails at once
        return code;
      });
    } finally {
      unwatch();
    }
    expect(code).toMatch(/^\d{6}$/);
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

  it("types into the overlord's pane and sees its answer", async () => {
    pane = new RemotePaneLink({
      bridge: t,
      target: "overlord",
      screen,
      onState: (s) => paneStates.push(s.kind),
    });
    pane.connect();
    await until("the overlord's pane open", paneOpen);
    const opened = pane.state;
    if (opened.kind !== "open") throw new Error("not open");
    expect(opened.id).toBe(store.snapshot.value?.overlord?.id);
    expect(opened.input).toBe(true);
    // A free pane goes to its opener, at the size it asked for.
    expect(opened.control).toBe(true);
    expect([opened.cols, opened.rows]).toEqual([120, 30]);
    // tmux draws the whole screen for a new view: the prompt is there.
    await until("the fake overlord's prompt", () => screen.has("fake overlord"));
    pane.input("hello from the phone\r");
    const answer = 'Fake overlord here: you said "hello from the phone".';
    await until("the overlord's answer on screen", () => screen.has(answer));
    // And in its transcript, which the Transcript tab reads.
    await until("the answer in the transcript", async () => (await transcript()).includes(answer));
    // The chat's way in is gone from the remote API.
    const asked = await t.request("overlord.ask", { text: "not any more" }).then(
      () => null,
      (e: unknown) => e,
    );
    expect(asked).toBeInstanceOf(ApiError);
    expect((asked as ApiError).code).toBe("not_allowed");
  });

  it("recovers from the relay killed and restarted", async () => {
    const seen = hellos;
    const resets = screen.resets;
    await stack("restart-relay");
    await helloAfter(seen);
    await until("live", () => phone.conn.value?.state === "live");
    const snap = await t.request<Snapshot>("snapshot");
    expect(snap.rows.length).toBeGreaterThanOrEqual(3);
    // The pane reopened in the new session, from a fresh screen.
    await until("the pane open again", () => paneOpen() && screen.resets > resets);
    await until("the screen redrawn", () => screen.has("Fake overlord here"));
  });

  it("recovers from the plugin killed and restarted, and runs what was asked meanwhile", async () => {
    const seen = hellos;
    await stack("stop-plugin");
    await until("the host offline", () => phone.conn.value?.state === "host_offline");
    // The pane closed with the session (`link`) and waits for the next hello.
    await until("the pane closed", () => !paneOpen());
    // A described new session while the host is away: it waits in the outbox.
    const described = t.request("overlord.new_session", {
      description: "while you were away",
      chosen: [],
      resolved: [],
    });
    await until("the request in the outbox", () => phone.outbox.value.length > 0);
    expect(phone.outbox.value[0]?.op).toBe("overlord.new_session");
    // (The supervisor settles after a stop before the next start.)
    await new Promise((r) => setTimeout(r, 1000));
    await stack("start-plugin");
    await helloAfter(seen);
    await described;
    // The overlord got it as typing: the fake answers the description's line.
    await until("the overlord's answer to what was asked meanwhile", async () =>
      (await transcript()).some(
        (m) => m.startsWith("Fake overlord here") && m.includes("while you were away"),
      ),
    );
    await until("the outbox emptied", () => phone.outbox.value.length === 0);
    // The pane reopened by itself, and takes typing again.
    await until("the pane open again", paneOpen);
    pane?.input("back again\r");
    await until("the answer after the restart", () =>
      screen.has('Fake overlord here: you said "back again".'),
    );
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
