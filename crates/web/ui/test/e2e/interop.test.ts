// The WASM core as a device against the native core as the host
// (`rlord-device interop-host`, on stdin/stdout): pairing, reconnect, and
// frames both ways, with random keys.
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { describe, expect, it } from "vitest";
import { type E2e, load, type Session } from "./wasm";

const b64 = (b: Uint8Array) => Buffer.from(b).toString("base64url");
const unb64 = (s: string) => new Uint8Array(Buffer.from(s, "base64url"));

function host() {
  const bin = process.env.RLORD_DEVICE_BIN;
  if (!bin) throw new Error("RLORD_DEVICE_BIN: run `cargo xtask e2e test-wasm`");
  const p = spawn(bin, ["interop-host"], { stdio: ["pipe", "pipe", "inherit"] });
  const lines = createInterface({ input: p.stdout })[Symbol.asyncIterator]();
  return {
    async next(): Promise<Record<string, unknown>> {
      const r = await lines.next();
      if (r.done) throw new Error("the native host ended");
      return JSON.parse(r.value) as Record<string, unknown>;
    },
    send(v: unknown) {
      p.stdin.write(`${JSON.stringify(v)}\n`);
    },
    end() {
      p.stdin.end();
    },
  };
}

function frame(e2e: E2e, s: Session, plain: Uint8Array): Uint8Array {
  const view = new Uint8Array(e2e.memory.buffer, s.buffer_ptr(), s.region_size());
  view.set(plain);
  const n = s.encrypt(plain.length);
  return new Uint8Array(e2e.memory.buffer, s.buffer_ptr(), n).slice();
}

function open(e2e: E2e, s: Session, wire: Uint8Array): Uint8Array {
  const view = new Uint8Array(e2e.memory.buffer, s.buffer_ptr(), s.region_size());
  view.set(wire);
  const n = s.decrypt(wire.length);
  return new Uint8Array(e2e.memory.buffer, s.buffer_ptr(), n).slice();
}

async function echo(e2e: E2e, h: ReturnType<typeof host>, s: Session, count: number) {
  for (let i = 0; i < count; i++) {
    const plain = new Uint8Array((i * 37) % 3000).map((_, j) => (i + j) & 0xff);
    h.send({ frame: b64(frame(e2e, s, plain)) });
    const back = await h.next();
    expect(open(e2e, s, unb64(back.frame as string))).toEqual(plain);
  }
}

describe("WASM device, native host", () => {
  it("pairs, reconnects and exchanges frames", async () => {
    const e2e = await load();
    const h = host();
    const offer = await h.next();
    const fp = unb64(offer.fingerprint as string);
    const key = e2e.generate_static_key();
    const priv = key.slice(0, 32);
    const pub = key.slice(32);

    const pair = e2e.DeviceHandshake.pair(offer.qr as string, priv, offer.origin as string, fp);
    h.send({ pair: b64(pair.message()) });
    const a = await h.next();
    const est = pair.finish(unb64(a.msg2 as string));
    expect(est.short_code()).toBe(a.code);
    await echo(e2e, h, est.into_session(), 200);

    const remote = est.remote_static();
    const kk = e2e.DeviceHandshake.reconnect(priv, remote, offer.origin as string, fp);
    h.send({ reconnect: b64(kk.message()), device: b64(pub) });
    const b = await h.next();
    await echo(e2e, h, kk.finish(unb64(b.msg2 as string)).into_session(), 200);
    h.end();
  });
});
