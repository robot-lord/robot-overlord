import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "../src/api";
import type { Snapshot } from "../src/proto";
import {
  DEBOUNCE_MS,
  inGroup,
  listView,
  type Requester,
  Store,
  sortOf,
  viewOf,
} from "../src/store";
import fixture from "./fixtures/snapshot.json";

const base = fixture as Snapshot;

function snap(rev: string, names: string[] = ["alpha"]): Snapshot {
  const row = base.rows[0];
  if (!row) throw new Error("fixture has no rows");
  return { ...base, rev, rows: names.map((n) => ({ ...row, id: n, name: n })) };
}

interface Call {
  op: string;
  args: { view: Record<string, unknown>; rev?: string };
  resolve(v: unknown): void;
  reject(e: unknown): void;
}

/** A control socket whose answers the test gives by hand. */
function fakeApi() {
  const calls: Call[] = [];
  const api: Requester = {
    request<T>(op: string, args: object = {}) {
      return new Promise<T>((resolve, reject) => {
        calls.push({
          op,
          args: args as Call["args"],
          resolve: (v) => resolve(v as T),
          reject,
        });
      });
    },
  };
  return { api, calls };
}

/** Let awaited promises run. */
const settle = () => vi.advanceTimersByTimeAsync(0);

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("store", () => {
  it("fetches at hello, then passes its rev and keeps the snapshot when unchanged", async () => {
    const s = new Store();
    const { api, calls } = fakeApi();
    s.connected(api);
    expect(calls).toHaveLength(1);
    expect(calls[0]?.op).toBe("snapshot");
    expect(calls[0]?.args.rev).toBeUndefined();
    calls[0]?.resolve(snap("r-1"));
    await settle();
    expect(s.snapshot.value?.rev).toBe("r-1");

    s.connected(api);
    expect(calls[1]?.args.rev).toBe("r-1");
    const before = s.snapshot.value;
    calls[1]?.resolve({ unchanged: true, rev: "r-1" });
    await settle();
    expect(s.snapshot.value).toBe(before);
  });

  it("debounces changed events and ignores the rev it has", async () => {
    const s = new Store();
    const { api, calls } = fakeApi();
    s.connected(api);
    calls[0]?.resolve(snap("r-1"));
    await settle();

    s.changed("r-1");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS * 2);
    expect(calls).toHaveLength(1);

    s.changed("r-2");
    s.changed("r-3");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS - 1);
    expect(calls).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(calls).toHaveLength(2);
    expect(calls[1]?.args.rev).toBe("r-1");
    calls[1]?.resolve(snap("r-3", ["alpha", "beta"]));
    await settle();
    expect(s.rows.value.map((r) => r.id)).toEqual(["alpha", "beta"]);
  });

  it("asks one at a time, and once more for what came in meanwhile", async () => {
    const s = new Store();
    const { api, calls } = fakeApi();
    s.connected(api);
    s.changed("r-2");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    s.changed("r-3");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    expect(calls).toHaveLength(1);
    calls[0]?.resolve(snap("r-2"));
    await settle();
    expect(calls).toHaveLength(2);
    expect(calls[1]?.args.rev).toBe("r-2");
  });

  it("a new view asks without a rev and drops an answer for the old view", async () => {
    const s = new Store();
    const { api, calls } = fakeApi();
    s.connected(api);
    calls[0]?.resolve(snap("r-1"));
    await settle();

    s.setView({ sort: "name" });
    expect(calls[1]?.args).toEqual({ view: { archived: "live", sort: "name", reverse: false } });
    s.setView({ archived: "all" });
    calls[1]?.resolve(snap("r-1", ["by-name"]));
    await settle();
    expect(s.snapshot.value?.rows[0]?.id).toBe("alpha");
    expect(calls[2]?.args).toEqual({ view: { archived: "all", sort: "name", reverse: false } });
    calls[2]?.resolve(snap("r-1", ["all"]));
    await settle();
    expect(s.snapshot.value?.rows[0]?.id).toBe("all");

    // Same query, other tab: drawn here, nothing asked.
    s.setView({ tab: "groups" });
    expect(calls).toHaveLength(3);
  });

  it("keeps the snapshot and says why when a request fails", async () => {
    const s = new Store();
    const { api, calls } = fakeApi();
    s.connected(api);
    calls[0]?.resolve(snap("r-1"));
    await settle();
    s.changed("r-2");
    await vi.advanceTimersByTimeAsync(DEBOUNCE_MS);
    calls[1]?.reject(new ApiError("disconnected", "connection lost"));
    await settle();
    expect(s.snapshot.value?.rev).toBe("r-1");
    expect(s.error.value).toBe("connection lost");
  });

  it("remembers the view in storage, ignoring junk", () => {
    const m = new Map<string, string>();
    const storage = {
      getItem: (k: string) => m.get(k) ?? null,
      setItem: (k: string, v: string) => void m.set(k, v),
    };
    const a = new Store(storage);
    a.setView({ sort: "created", search: "web" });
    expect(new Store(storage).view.value).toMatchObject({ sort: "created", search: "web" });
    m.set("rlord.view", JSON.stringify({ sort: "bogus", archived: 3, tab: "groups" }));
    expect(new Store(storage).view.value).toMatchObject({
      sort: "activity",
      archived: "live",
      tab: "groups",
    });
    m.set("rlord.view", "{not json");
    expect(new Store(storage).view.value.sort).toBe("activity");
  });

  it("draws the picked group's rows, or the first group's", async () => {
    const s = new Store();
    const { api, calls } = fakeApi();
    s.connected(api);
    calls[0]?.resolve(base);
    await settle();
    s.setView({ tab: "groups" });
    // The fixture's groups: #wip (a favourite) first, then #web, untagged.
    expect(s.group.value).toBe("wip");
    expect(s.rows.value.map((r) => r.id)).toEqual(["beta"]);
    s.setView({ group: "" });
    expect(s.rows.value.map((r) => r.id)).toEqual(["gamma"]);
    s.setView({ group: "gone" });
    expect(s.group.value).toBe("wip");
  });

  it("puts untagged rows in the untagged group only", () => {
    const row = base.rows[0];
    if (!row) throw new Error("no row");
    expect(inGroup({ ...row, tags: [] }, "")).toBe(true);
    expect(inGroup({ ...row, tags: ["a"] }, "")).toBe(false);
    expect(inGroup({ ...row, tags: ["a", "b"] }, "b")).toBe(true);
  });
});

describe("views from the daemon", () => {
  it("maps the daemon's view shape both ways", () => {
    const l = {
      query: "tag:web status:needs",
      sort: "updated",
      reverse: true,
      usage: "tokens",
      archived: "only",
      tab: "groups",
      group: "web",
    };
    const v = viewOf(l);
    expect(v).toEqual({
      archived: "only",
      sort: "activity",
      reverse: true,
      search: "tag:web status:needs",
      tab: "groups",
      group: "web",
      usage: "tokens",
    });
    expect(listView(v)).toEqual(l);
    expect(sortOf("status")).toBeNull();
    expect(viewOf({ ...l, sort: "bogus", tab: "x", archived: "x" })).toMatchObject({
      sort: "activity",
      tab: "sessions",
      archived: "live",
    });
  });

  it("sends the filter as query, with the usage measure", async () => {
    const s = new Store();
    const { api, calls } = fakeApi();
    s.connected(api);
    calls[0]?.resolve(snap("r-1"));
    await settle();
    s.applyView(
      {
        query: "tag:wip",
        sort: "name",
        reverse: false,
        usage: "context",
        archived: "live",
        tab: "sessions",
        group: null,
      },
      "beta",
    );
    expect(s.selected.value).toBe("beta");
    expect(calls.at(-1)?.args.view).toMatchObject({
      query: "tag:wip",
      sort: "name",
      usage: "context",
    });
  });

  it("a browser with no view of its own starts from the daemon's default", async () => {
    const s = new Store(null, { defaultView: true });
    const { api, calls } = fakeApi();
    s.connected(api);
    expect(calls[0]?.op).toBe("view.default");
    calls[0]?.resolve({
      query: "status:needs",
      sort: "state",
      reverse: false,
      usage: null,
      archived: "live",
      tab: "sessions",
      group: null,
    });
    await settle();
    expect(calls[1]?.op).toBe("snapshot");
    expect(calls[1]?.args.view).toMatchObject({ query: "status:needs", sort: "state" });
    // One that has its own keeps it.
    const saved = new Map<string, string>();
    const storage = {
      getItem: (k: string) => saved.get(k) ?? null,
      setItem: (k: string, v: string) => void saved.set(k, v),
    };
    new Store(storage).setView({ sort: "name" });
    const own = new Store(storage, { defaultView: true });
    const b = fakeApi();
    own.connected(b.api);
    expect(b.calls[0]?.op).toBe("snapshot");
  });
});
