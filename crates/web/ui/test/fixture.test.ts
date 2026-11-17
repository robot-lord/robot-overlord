// proto.ts against the daemon's real `snapshot` answer: the fixture is
// written and checked by crates/daemon/tests/web_api.rs, so a field the
// daemon adds, drops or retypes fails here (by `tsc`, in `web check`)
// until proto.ts follows.

import { describe, expect, it } from "vitest";
import type { Group, Overlord, RosterEntry, Row, Snapshot, Status } from "../src/proto";
import fixture from "./fixtures/snapshot.json";

// --- Type-level checks (`tsc --noEmit` fails on a mismatch) ---

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

// (The standard exact-type test: T is meant to appear once.)
/* oxlint-disable typescript/no-unnecessary-type-parameters */
type Equal<X, Y> =
  (<T>() => T extends X ? 1 : 2) extends <T>() => T extends Y ? 1 : 2 ? true : false;
/* oxlint-enable typescript/no-unnecessary-type-parameters */

type RequiredKeys<T> = {
  [K in keyof T]-?: object extends Pick<T, K> ? never : K;
}[keyof T];

type Fx = typeof fixture;
type FxRow = Fx["rows"][number];
type FxOverlord = NonNullable<Fx["overlord"]>;
type FxRoster = FxOverlord["overlords"][number];
type FxGroup = Fx["groups"][number];

// The fixture fits the types (every field present, each of its type)...
const fits: Widen<Snapshot> = fixture;
// ...and has no field the types don't name (optional ones aside).
const keys: [
  Equal<keyof Fx, RequiredKeys<Snapshot>>,
  Equal<keyof FxRow, RequiredKeys<Row>>,
  Equal<keyof FxOverlord, RequiredKeys<Overlord>>,
  Equal<keyof FxRoster, RequiredKeys<RosterEntry>>,
  Equal<keyof FxGroup, RequiredKeys<Group>>,
] = [true, true, true, true, true];

// --- Values the types can't see ---

const STATUSES: Status[] = ["starting", "working", "waiting_input", "idle", "stopped", "error"];

describe("snapshot fixture", () => {
  it("fits proto.ts", () => {
    expect(fits.rev).toBe(fixture.rev);
    expect(keys).toEqual([true, true, true, true, true]);
  });

  it("uses only known statuses, all of them listed in order", () => {
    expect(fixture.statuses.map((s) => s.status)).toEqual(STATUSES);
    for (const r of fixture.rows) expect(STATUSES).toContain(r.status);
    expect(STATUSES).toContain(fixture.overlord.status);
    const roster = fixture.overlord.overlords.flatMap((r) => (r.status === null ? [] : [r.status]));
    for (const st of roster) expect(STATUSES).toContain(st);
  });
});
