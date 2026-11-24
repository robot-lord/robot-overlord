// The control bar's states (control.ts), from snapshots: this screen
// holds the pane, another one does (away or not), nobody does, a view-only
// device; and the surface names from hello.

import { describe, expect, it } from "vitest";
import {
  type ControlFacts,
  controlKey,
  controlView,
  controlWords,
  surfaceOf,
} from "../src/control";
import type { Control, Snapshot } from "../src/proto";
import { Store } from "../src/store";
import fixture from "./fixtures/snapshot.json";

const PHONE = "device:ab12cd34ef560789";

const held = (holder: string, more: Partial<Control> = {}): Control => ({
  holder,
  by: holder === "terminal" ? "terminal" : holder.startsWith("page:") ? "web" : "phone",
  name: holder === "terminal" ? "laptop's terminal" : "Ann's iPhone",
  away: false,
  cols: 120,
  rows: 40,
  ...more,
});

/** The fixture snapshot with `control` on its first row and the overlord. */
function snapshot(row: Control | null, overlord: Control | null = null): Snapshot {
  const s = structuredClone(fixture) as unknown as Snapshot;
  const first = s.rows[0];
  if (!first || !s.overlord) throw new Error("the fixture has a row and an overlord");
  first.control = row;
  s.overlord.control = overlord;
  return s;
}

const facts = (more: Partial<ControlFacts>): ControlFacts => ({
  control: undefined,
  me: PHONE,
  opened: true,
  input: true,
  window: { cols: 52, rows: 40 },
  took: null,
  ...more,
});

describe("surfaces", () => {
  it("a phone by its device id, a page by its page id", () => {
    expect(surfaceOf({ page: "ab12cd34ef560789", device: "ab12cd34ef560789" })).toBe(PHONE);
    expect(surfaceOf({ page: "p1" })).toBe("page:p1");
  });
});

describe("the control bar", () => {
  it("holder: this screen holds the pane (from the snapshot)", () => {
    const store = new Store();
    store.snapshot.value = snapshot(held(PHONE));
    const id = store.snapshot.value.rows[0]?.id ?? "";
    const v = controlView(facts({ control: store.controlOf(id) }));
    expect(v).toEqual({ kind: "holder" });
  });

  it("viewer: another screen holds it, at its size, with Take over", () => {
    const store = new Store();
    store.snapshot.value = snapshot(held("terminal"));
    const id = store.snapshot.value.rows[0]?.id ?? "";
    const v = controlView(facts({ control: store.controlOf(id), opened: false }));
    expect(v).toEqual({
      kind: "viewer",
      name: "laptop's terminal",
      away: false,
      cols: 120,
      rows: 40,
      canTake: true,
    });
    if (v.kind !== "viewer") throw new Error("a viewer");
    expect(controlWords(v)).toBe("Controlled by laptop's terminal");
  });

  it("away: the holder left and keeps control a while", () => {
    const store = new Store();
    store.snapshot.value = snapshot(
      null,
      held("page:p1", { name: "a browser on laptop", away: true }),
    );
    const o = store.snapshot.value.overlord?.id ?? null;
    const v = controlView(facts({ control: store.controlOf(o) }));
    if (v.kind !== "viewer") throw new Error("a viewer");
    expect(controlWords(v)).toBe("Controlled by a browser on laptop (away)");
    expect(v.canTake).toBe(true);
  });

  it("nobody holds it: the open's answer decides (a free pane went to its opener)", () => {
    const store = new Store();
    store.snapshot.value = snapshot(null);
    const id = store.snapshot.value.rows[0]?.id ?? "";
    expect(store.controlOf(id)).toBeNull();
    expect(controlView(facts({ control: null, opened: true }))).toEqual({ kind: "holder" });
    // A host that doesn't say (before control existed): this screen holds it.
    expect(controlView(facts({ control: undefined, opened: null }))).toEqual({ kind: "holder" });
    // Opened as a viewer while the snapshot is behind: the window's size.
    expect(controlView(facts({ control: null, opened: false }))).toMatchObject({
      kind: "viewer",
      cols: 52,
      rows: 40,
      canTake: true,
    });
    // A session the snapshot doesn't list: not known.
    expect(store.controlOf("nope")).toBeUndefined();
  });

  it("a take counts until the snapshot moves on", () => {
    const before = held("terminal");
    const took = controlKey(before);
    expect(controlView(facts({ control: before, took }))).toEqual({ kind: "holder" });
    // The snapshot says so now.
    expect(controlView(facts({ control: held(PHONE), took }))).toEqual({ kind: "holder" });
    // Someone took it back since: a viewer again.
    const back = held("terminal", { cols: 100 });
    expect(controlView(facts({ control: back, took }))).toMatchObject({ kind: "viewer" });
  });

  it("view only: never the holder, no Take over, whoever holds it", () => {
    for (const control of [held(PHONE), held("terminal"), null]) {
      const v = controlView(facts({ control, input: false, took: controlKey(control) }));
      expect(v).toMatchObject({ kind: "viewer", canTake: false });
      if (v.kind !== "viewer") throw new Error("a viewer");
      expect(controlWords(v)).toMatch(/view only|View only/);
    }
  });
});
