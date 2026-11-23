// The phone's own screens by event sequence: pairing from the first
// screen to the list (and its failures), the connection in words, and
// the status screen's facts.

import { describe, expect, it } from "vitest";
import {
  connBad,
  connWords,
  deviceName,
  keysWords,
  Phone,
  relayName,
  screen,
  spaced,
  viewportVars,
} from "../src/phone";
import type { AppEvent, ConnKind } from "../src/proto";

function run(events: AppEvent[]): Phone {
  const p = new Phone();
  for (const e of events) p.apply(e);
  return p;
}

const shown = (p: Phone) => screen(p.conn.value, p.pairing.value);

describe("pairing", () => {
  it("goes from Scan to the list", () => {
    const p = new Phone();
    expect(shown(p)).toBe("starting");
    p.apply({ event: "conn", state: "unpaired" });
    expect([shown(p), p.pairing.value]).toEqual(["pair", { kind: "start", reason: null }]);
    p.apply({
      event: "pairing",
      state: "scanned",
      relay: "https://relay.example.com",
      name: "Brandon's iPhone with a name much longer than forty characters",
    });
    expect(p.pairing.value).toEqual({
      kind: "scanned",
      relay: "https://relay.example.com",
      name: "Brandon's iPhone with a name much longer",
    });
    expect(relayName("https://relay.example.com/")).toBe("relay.example.com");
    p.apply({ event: "pairing", state: "connecting" });
    expect([shown(p), p.pairing.value.kind]).toEqual(["pair", "connecting"]);
    p.apply({ event: "pairing", state: "waiting", code: "123456" });
    expect(p.pairing.value).toEqual({ kind: "waiting", code: "123456" });
    expect(spaced("123456")).toBe("123 456");
    // Still unpaired until the engine says otherwise: the code stays up.
    p.apply({ event: "conn", state: "unpaired" });
    expect(shown(p)).toBe("pair");
    p.apply({ event: "pairing", state: "done" });
    p.apply({ event: "conn", state: "connecting", host: "laptop" });
    expect(shown(p)).toBe("main");
    p.apply({ event: "conn", state: "live", host: "laptop" });
    expect(shown(p)).toBe("main");
  });

  it("back to the first screen with the reason when it fails", () => {
    const rejected = run([
      { event: "conn", state: "unpaired" },
      { event: "pairing", state: "scanned", relay: "https://r.example", name: "phone" },
      { event: "pairing", state: "waiting", code: "123456" },
      { event: "pairing", state: "rejected" },
    ]);
    expect(shown(rejected)).toBe("pair");
    expect(rejected.pairing.value).toEqual({
      kind: "start",
      reason: "Your computer turned this phone down.",
    });
    const failed = run([
      { event: "conn", state: "unpaired" },
      { event: "pairing", state: "connecting" },
      { event: "pairing", state: "failed", reason: "the code has expired: make a new one" },
    ]);
    expect(failed.pairing.value).toEqual({
      kind: "start",
      reason: "the code has expired: make a new one",
    });
  });

  it("revoked: says so, and offers to pair again", () => {
    const p = run([
      { event: "conn", state: "live", host: "laptop" },
      { event: "conn", state: "revoked" },
    ]);
    expect(shown(p)).toBe("pair");
    expect(p.pairing.value).toEqual({
      kind: "start",
      reason: "This phone is no longer paired with laptop. Pair it again to use it.",
    });
  });

  it("unpaired after being paired: the first screen, no old reason", () => {
    const p = run([
      { event: "conn", state: "unpaired" },
      { event: "pairing", state: "done" },
      { event: "conn", state: "live", host: "laptop" },
      { event: "conn", state: "unpaired" },
    ]);
    expect(p.pairing.value).toEqual({ kind: "start", reason: null });
    expect(shown(p)).toBe("pair");
  });

  it("names are trimmed and at most 40 characters", () => {
    expect(deviceName("  pixel  ")).toBe("pixel");
    expect(deviceName("é".repeat(50))).toBe("é".repeat(40));
  });
});

const words = (state: ConnKind, detail?: string) =>
  connWords({ event: "conn", state, host: "laptop", ...(detail ? { detail } : {}) });

describe("the connection in words", () => {
  it("names the host in each state", () => {
    expect(connWords(null)).toBe("starting…");
    expect(words("live")).toBe("laptop: connected");
    expect(words("connecting")).toBe("connecting to laptop…");
    expect(words("relay")).toBe("reached the relay, waiting for laptop…");
    expect(words("host_offline")).toBe("laptop is offline");
    expect(words("offline")).toBe("no network");
    expect(words("update_app")).toBe("update this app to talk to laptop");
    expect(words("update_host")).toBe("update rlord on laptop");
    expect(words("wrong_relay", "pin mismatch")).toBe(
      "the relay isn't the one laptop uses: pair again (pin mismatch)",
    );
    expect(connWords({ event: "conn", state: "connecting" })).toBe("connecting to your computer…");
    expect(connBad("suspended")).toBe(true);
    expect(connBad("host_offline")).toBe(false);
  });
});

describe("the status screen", () => {
  it("keeps the engine's status and the shell's settings", () => {
    const p = run([
      { event: "conn", state: "live", host: "laptop", relay: "https://relay.example.com" },
      {
        event: "status",
        host: "laptop",
        relay: "https://relay.example.com",
        state: "live",
        device_id: "ab12cd34ef560789",
        device_name: "iPhone",
        keys: "software",
        version: "1.3.0",
        api: { min: 1, max: 1 },
      },
      { event: "settings", app_lock: true, notifications: false, platform: "ios" },
    ]);
    expect(p.status.value?.device_id).toBe("ab12cd34ef560789");
    expect(keysWords(p.status.value?.keys ?? "hardware")).toContain("software");
    expect(keysWords("hardware")).toBe("in the phone's secure hardware");
    expect(p.settings.value).toMatchObject({ app_lock: true, notifications: false });
  });

  it("lays the app over what the keyboard leaves", () => {
    expect(viewportVars({ height: 512.4, offsetTop: 0 })).toEqual({
      "--vvh": "512px",
      "--vvtop": "0px",
    });
    expect(viewportVars({ height: 300, offsetTop: -3 })["--vvtop"]).toBe("0px");
  });
});
