// The Devices view against the daemon's real answers: the fixtures are
// written and checked by crates/daemon/tests/web_api.rs (`device.list`
// with a paired device and one waiting, `device.pair`'s offer).

import { describe, expect, it } from "vitest";
import type { DeviceList, PairOffer } from "../src/proto";
import {
  leftText,
  linkText,
  offerStands,
  qrPath,
  seenText,
  spacedCode,
} from "../src/views/Devices";
import listFx from "./fixtures/device_list.json";
import pairFx from "./fixtures/device_pair.json";

/** JSON imports type strings as `string`: compare against that. */
type Widen<T> = T extends string
  ? string
  : T extends number
    ? number
    : T extends boolean
      ? boolean
      : T extends readonly (infer U)[]
        ? Widen<U>[]
        : T extends object
          ? { [K in keyof T]: Widen<T[K]> }
          : T;

// The fixtures fit the types (`tsc --noEmit` fails otherwise).
const listFits: Widen<DeviceList> = listFx;
const pairFits: Widen<PairOffer> = pairFx;
const list = listFx as unknown as DeviceList;
const offer: PairOffer = pairFx;

describe("the Devices view", () => {
  it("reads the daemon's device list", () => {
    expect(listFits.devices[0]?.id).toBe("ab12cd34ef560789");
    expect(list.pending?.name).toBe("laptop");
    expect(spacedCode(list.pending?.code ?? "")).toBe("654 321");
    expect(spacedCode("12345")).toBe("12345");
    // No relay in the fixture's config: says how to set one up.
    expect(linkText(list)).toContain("rlord relay setup");
    const on = { ...list, relay: "https://relay.example.com", enabled: true };
    expect(linkText({ ...on, plugin: { ...on.plugin, installed: true } })).toBe(
      "https://relay.example.com: connected.",
    );
    expect(
      linkText({
        ...on,
        plugin: { ...on.plugin, installed: true },
        connected: false,
        error: "wrong relay",
      }),
    ).toBe("https://relay.example.com: not connected (wrong relay).");
    expect(linkText({ ...on, enabled: false })).toContain("off");
    const d = list.devices[0];
    if (!d) throw new Error("a device");
    expect(seenText(d, (d.last_seen ?? 0) * 1000 + 3 * 60_000)).toBe("seen 3m ago");
    expect(seenText({ ...d, last_seen: null }, 0)).toBe("never seen");
  });

  it("draws the offer's code from its modules", () => {
    expect(pairFits.qr).toMatch(/^RLORD1:/);
    const n = offer.qr_rows.length;
    expect(offer.qr_rows.every((r) => r.length === n)).toBe(true);
    const { size, d } = qrPath(offer.qr_rows);
    expect(size).toBe(n + 8);
    // The top-left finder: a run of seven dark modules, in the quiet zone's corner.
    expect(d.startsWith("M4 4h7v1h-7z")).toBe(true);
    // Every dark module is in exactly one run.
    const dark = offer.qr_rows
      .join("")
      .split("")
      .filter((c) => c === "1").length;
    const runs = [...d.matchAll(/h(\d+)v1/g)].reduce((a, m) => a + Number(m[1]), 0);
    expect(runs).toBe(dark);
    expect(qrPath(["0110"], 0).d).toBe("M1 0h2v1h-2z");
  });

  it("counts the offer down, and drops it once taken or gone", () => {
    expect(leftText(offer.expires, (offer.expires - 125) * 1000)).toBe("2:05");
    expect(leftText(offer.expires, offer.expires * 1000)).toBeNull();
    expect(leftText(null, 0)).toBeNull();
    // A device took it.
    expect(offerStands(list, 0, 0)).toBe(false);
    const open = { ...list, pending: null, pairing: { expires: offer.expires } };
    expect(offerStands(open, 0, 60_000)).toBe(true);
    // The plugin no longer has it, once it has had a moment to say so.
    const gone = { ...open, pairing: null };
    expect(offerStands(gone, 10_000, 11_000)).toBe(true);
    expect(offerStands(gone, 10_000, 20_000)).toBe(false);
  });
});
