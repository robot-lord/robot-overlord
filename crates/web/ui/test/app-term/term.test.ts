// The real Claude Code (the pinned CLI, on rlord's fake model: no model
// is ever called) in a demo rlord's pane, reached the way the phone
// reaches it: the page's `RemotePaneLink` over `rlord-device bridge` (the
// phone's engine) and the real relay, drawn into a headless xterm.js and
// read back as the screen's text. `cargo xtask app test-term` sets it up.

import { execFileSync, spawn } from "node:child_process";
import { existsSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { Terminal } from "@xterm/headless";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { installDevBridge } from "../../src/devBridge";
import type { PaneState, Screen } from "../../src/paneLink";
import { Phone } from "../../src/phone";
import { wirePhone } from "../../src/phoneApp";
import type { Snapshot } from "../../src/proto";
import { type Bridge, type BridgeHost, findBridge, type RemoteTransport } from "../../src/remote";
import { RemotePaneLink } from "../../src/remotePane";
import { Store } from "../../src/store";
import { keyBytes } from "../../src/term/keys";

const BRIDGE = process.env.RLORD_BRIDGE_URL;
const DIR = process.env.RLORD_TERM_DIR ?? "";
const TMUX = process.env.RLORD_TERM_TMUX ?? "tmux";
const SOCKET = process.env.RLORD_TERM_SOCKET ?? "";
const RLORD = process.env.RLORD_TERM_RLORD ?? "";
const ENV = JSON.parse(process.env.RLORD_TERM_ENV ?? "{}") as Record<string, string>;
const CONTROL = process.env.RLORD_TERM_CONTROL ?? "";
const WAIT = 60_000;
/** The phone's width at 12 px (gate 1c: 52 to 54 columns). */
const COLS = 54;
const ROWS = 30;
/** Two of the demo's Claude sessions. */
const MAIN = "api-rate-limits";
const HELD = "docs-navigation";

type Maybe<T> = T | null | undefined | false;
const heard: string[] = [];

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

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** A headless xterm.js as the pane link's screen; its text as the user sees it. */
class HeadlessScreen implements Screen {
  readonly term: Terminal;
  resets = 0;
  constructor(cols: number, rows: number) {
    this.term = new Terminal({ cols, rows, allowProposedApi: true, scrollback: 2000 });
  }
  get cols() {
    return this.term.cols;
  }
  get rows() {
    return this.term.rows;
  }
  reset(): void {
    this.resets += 1;
    this.term.reset();
  }
  write(bytes: Uint8Array, parsed?: () => void): void {
    this.term.write(bytes, parsed);
  }
  /** The visible rows, trailing blanks trimmed. */
  text(): string {
    const b = this.term.buffer.active;
    const out: string[] = [];
    for (let i = 0; i < this.term.rows; i++) {
      out.push(b.getLine(b.viewportY + i)?.translateToString(true) ?? "");
    }
    return out.join("\n");
  }
  /** Whether the screen shows `s` (white space ignored: wrapping). */
  has(s: string): boolean {
    return this.text().replaceAll(/\s+/g, "").includes(s.replaceAll(/\s+/g, ""));
  }
  /** The longest row's length. */
  widest(): number {
    return Math.max(
      ...this.text()
        .split("\n")
        .map((l) => l.length),
    );
  }
  /** Settled: written and parsed. */
  async flush(): Promise<void> {
    await new Promise<void>((r) => this.term.write("", r));
  }
}

function rlord(args: string[]): string {
  return execFileSync(RLORD, args, { env: { ...process.env, ...ENV }, encoding: "utf8" });
}

function tmux(args: string[]): string {
  return execFileSync(TMUX, ["-L", SOCKET, ...args], { encoding: "utf8" }).trim();
}

let n = 0;
/** A line for the bridge (`network down|up`), through the harness. */
async function bridgeCmd(line: string): Promise<void> {
  n += 1;
  const name = String(n).padStart(3, "0");
  writeFileSync(join(CONTROL, `${name}.cmd`), line);
  await until(`the bridge's ${line}`, () => existsSync(join(CONTROL, `${name}.done`)));
}

/** The demo's tmux session for a session name. */
function tmuxSession(name: string): string {
  return `${name}-rlord`;
}

function windowSize(name: string): string {
  return tmux([
    "display-message",
    "-p",
    "-t",
    `=${tmuxSession(name)}:`,
    "#{window_width}x#{window_height}",
  ]);
}

const keys = { ctrl: false, alt: false, appCursor: false };

describe("Claude Code's terminal on the phone's pane link", () => {
  const store = new Store();
  const phone = new Phone();
  let hellos = 0;
  let t: RemotePaneLink | null = null;
  let tr: RemoteTransport;
  let terminal: { kill(): void } | null = null;

  /** Opens `target` on a fresh screen; the link and the screen. */
  async function open(target: string, cols = COLS, rows = ROWS) {
    const screen = new HeadlessScreen(cols, rows);
    const states: PaneState[] = [];
    const link = new RemotePaneLink({
      bridge: tr,
      target,
      screen,
      onState: (s) => states.push(s),
    });
    link.connect();
    await until(`${target}'s pane open`, () => link.state.kind === "open");
    return { link, screen, states };
  }

  beforeAll(() => {
    if (!BRIDGE) throw new Error("RLORD_BRIDGE_URL: run `cargo xtask app test-term`");
    const w: BridgeHost = {};
    installDevBridge(w, BRIDGE, WebSocket);
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
    tr = wirePhone({
      store,
      phone,
      bridge,
      go: () => {},
      onHello: () => {
        hellos += 1;
      },
    });
  });

  let failed = false;
  beforeEach((ctx) => {
    if (failed) ctx.skip();
  });
  afterEach((ctx) => {
    if (ctx.task.result?.state === "fail") failed = true;
    t?.stop();
    t = null;
  });
  afterAll(() => {
    terminal?.kill();
    tr?.stop();
  });

  it("pairs with the demo host", async () => {
    await until("the scanned code", () => phone.pairing.value.kind === "scanned");
    tr.send({ app: "pair_continue", name: "term test" });
    await until("paired", () => phone.pairing.value.kind === "done");
    await until("live", () => phone.conn.value?.state === "live");
    await until("a snapshot", () => store.snapshot.value);
    const names = (store.snapshot.value as Snapshot).rows.map((r) => r.name);
    expect(names).toContain(MAIN);
  });

  it("draws Claude Code's prompt at the phone's width", async () => {
    const { link, screen } = await open(MAIN);
    t = link;
    const st = link.state;
    if (st.kind !== "open") throw new Error("not open");
    // Nobody else showed it: the phone holds it, and the window is laid
    // out for the phone.
    expect(st.control).toBe(true);
    await until("the window at the phone's size", () => windowSize(MAIN) === `${COLS}x${ROWS}`);
    await until("Claude Code's prompt", () => screen.has("? for shortcuts"));
    await screen.flush();
    expect(screen.widest()).toBeLessThanOrEqual(COLS);
    // Its rule spans the phone's width.
    expect(screen.text()).toContain("─".repeat(COLS));
  });

  it("types a line and sees the fake model's answer", async () => {
    const { link, screen } = await open(MAIN);
    t = link;
    await until("the prompt", () => screen.has("? for shortcuts"));
    link.input("hello from the phone");
    await until("the typed line", () => screen.has("hello from the phone"));
    link.input(keyBytes("enter", keys));
    await until("the fake model's echo", () => screen.has("rlord's fake model"));
    await until("the answer's end", () => screen.has("done"), WAIT);
    expect(screen.has("hello from the phone")).toBe(true);
  });

  it("accepts a tool's permission dialog with the key row's keys", async () => {
    const { link, screen } = await open(MAIN);
    t = link;
    await until("the prompt", () => screen.has("? for shortcuts"));
    const file = join(DIR, "src", MAIN, "fake-model-hello.txt");
    expect(existsSync(file)).toBe(false);
    link.input("please run a tool");
    link.input(keyBytes("enter", keys));
    await until("the permission dialog", () => screen.has("Do you want to proceed?"));
    // Down to "2." and back up to "1. Yes", then Enter.
    link.input(keyBytes("down", keys));
    await sleep(300);
    link.input(keyBytes("up", keys));
    await sleep(300);
    link.input(keyBytes("enter", keys));
    await until("the tool ran", () => existsSync(file));
  });

  it("switches the mode with ⇧Tab", async () => {
    const { link, screen } = await open(MAIN);
    t = link;
    await until("the prompt", () => screen.has("? for shortcuts"));
    const before = screen
      .text()
      .split("\n")
      .filter((l) => l.includes("mode"))
      .join("|");
    link.input(keyBytes("backtab", keys));
    const after = await until("the mode line changed", () => {
      const now = screen
        .text()
        .split("\n")
        .filter((l) => l.includes("mode") || l.includes("plan"))
        .join("|");
      return now !== before && now;
    });
    expect(after).not.toBe(before);
  });

  it("stops a slow answer with Esc", async () => {
    const { link, screen } = await open(MAIN);
    t = link;
    await until("the prompt", () => screen.has("? for shortcuts"));
    link.input("a long question so the answer streams for a while");
    link.input(keyBytes("enter", keys));
    await until("the answer streaming", () => screen.has("esc to interrupt"));
    link.input(keyBytes("esc", keys));
    await until("interrupted", () => screen.has("Interrupted"));
    expect(screen.has("esc to interrupt")).toBe(false);
  });

  it("shows the same screen after a resize and on a fresh open", async () => {
    const a = await open(MAIN);
    t = a.link;
    await until("the prompt", () => a.screen.has("? for shortcuts"));
    a.screen.term.resize(48, 26);
    a.link.resize(48, 26);
    await until("relaid at 48", () => windowSize(MAIN) === "48x26");
    await sleep(1500);
    await a.screen.flush();
    const b = await open(MAIN, 48, 26);
    await sleep(1500);
    await b.screen.flush();
    expect(b.screen.text()).toBe(a.screen.text());
    b.link.stop();
    // Back to the phone's size for what follows.
    a.screen.term.resize(COLS, ROWS);
    a.link.resize(COLS, ROWS);
    await until("relaid at the phone's size", () => windowSize(MAIN) === `${COLS}x${ROWS}`);
  });

  it("reopens after the link is cut mid-answer, to the same screen as a fresh open", async () => {
    const a = await open(MAIN);
    t = a.link;
    await until("the prompt", () => a.screen.has("? for shortcuts"));
    a.link.input("another question to cut in the middle of");
    a.link.input(keyBytes("enter", keys));
    await until("the answer streaming", () => a.screen.has("esc to interrupt"));
    const seen = hellos;
    await bridgeCmd("network down");
    await sleep(1500);
    await bridgeCmd("network up");
    await until("a new session", () => hellos > seen);
    await until("reopened", () => a.link.state.kind === "open");
    await until(
      "the answer ended",
      () => !a.screen.has("esc to interrupt") && a.screen.has("? for shortcuts"),
    );
    await sleep(1000);
    await a.screen.flush();
    const b = await open(MAIN);
    await sleep(1500);
    await b.screen.flush();
    expect(a.screen.text()).toBe(b.screen.text());
    b.link.stop();
  });

  it("sends the compose line as one bracketed paste", async () => {
    const { link, screen } = await open(MAIN);
    t = link;
    await until("the prompt", () => screen.has("? for shortcuts"));
    await until("bracketed paste asked for", () => screen.term.modes.bracketedPasteMode);
    // What the compose line does: one paste (two lines), then Enter. As
    // xterm.js's own paste sends it: newlines as CR, in brackets when the
    // pane asked for them.
    const text = "first pasted line\nsecond pasted line".replaceAll(/\r?\n/g, "\r");
    link.input(`\x1b[200~${text}\x1b[201~`);
    await sleep(500);
    link.input(keyBytes("enter", keys));
    // One turn with both lines: the paste didn't submit the first alone.
    await until("the echo", () => screen.has("first pasted line"));
    const log = execFileSync("grep", ["-c", "second pasted line", join(DIR, "fake-model.log")], {
      encoding: "utf8",
    });
    expect(Number(log.trim())).toBeGreaterThan(0);
  });

  it("opens as a viewer while the terminal holds it, and takes over", async () => {
    // A terminal on the computer: a real tmux client at 100x30, holding
    // the session.
    terminal = spawn(
      "python3",
      [
        "-c",
        "import os,pty,sys,fcntl,termios,struct\n" +
          "pid,fd=pty.fork()\n" +
          "if pid==0:\n os.execvp(sys.argv[1],[sys.argv[1],'-L',sys.argv[2],'attach','-t','='+sys.argv[3]])\n" +
          "fcntl.ioctl(fd,termios.TIOCSWINSZ,struct.pack('HHHH',30,100,0,0))\n" +
          "while True:\n try: os.read(fd,65536)\n except OSError: break\n",
        TMUX,
        SOCKET,
        tmuxSession(HELD),
      ],
      { stdio: "ignore" },
    );
    await until("the terminal attached", () =>
      tmux(["list-clients", "-F", "#{client_session}"]).includes(tmuxSession(HELD)),
    );
    rlord(["control", "take", HELD]);
    await until("the window at the terminal's size", () => windowSize(HELD) === "100x30");
    const { link, screen } = await open(HELD);
    t = link;
    const st = link.state;
    if (st.kind !== "open") throw new Error("not open");
    expect(st.control).toBe(false);
    expect([st.cols, st.rows]).toEqual([100, 30]);
    await until("Claude Code drawn for the terminal", () => screen.has("? for shortcuts"));
    // Taking over lays Claude Code out for the phone, and typing works.
    await tr.request("pane.take", { target: st.id, cols: COLS, rows: ROWS });
    await until("the window at the phone's size", () => windowSize(HELD) === `${COLS}x${ROWS}`);
    screen.term.resize(COLS, ROWS);
    await until("the snapshot says the phone holds it", () => {
      const row = store.snapshot.value?.rows.find((r) => r.name === HELD);
      return row?.control?.holder.startsWith("device:");
    });
    link.input("typed after the take");
    await until("typing reaches it", () => screen.has("typed after the take"));
  });
});
