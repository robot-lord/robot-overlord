// The page's state, as signals: the connection, the latest snapshot, and
// the view the user picked (filter, sort, tabs: the browser's own, kept in
// localStorage). The daemon does the grouping, sorting and labels; this
// only asks for snapshots at the right times.

import { batch, computed, type Signal, signal } from "@preact/signals";
import type { ConnState } from "./api";
import type { Query, Row, Snapshot, Sort, Unchanged } from "./proto";

/** What the store needs of the control socket. */
export interface Requester {
  request<T = unknown>(op: string, args?: object): Promise<T>;
}

export type Tab = "sessions" | "groups";

export interface View {
  archived: "live" | "only" | "all";
  sort: Sort;
  reverse: boolean;
  search: string;
  tab: Tab;
  /** The group picked in the Groups tab ("" is untagged; null: the first). */
  group: string | null;
}

export const DEFAULT_VIEW: View = {
  archived: "live",
  sort: "activity",
  reverse: false,
  search: "",
  tab: "sessions",
  group: null,
};

export const SORTS: { sort: Sort; label: string }[] = [
  { sort: "activity", label: "last update" },
  { sort: "user", label: "your last prompt" },
  { sort: "state", label: "status change" },
  { sort: "created", label: "created" },
  { sort: "name", label: "name" },
  { sort: "usage", label: "usage" },
];

/** Wait this long after a `changed` before asking: changes come in bursts. */
export const DEBOUNCE_MS = 100;

const VIEW_KEY = "rlord.view";

/** The daemon query for a view (the tab and group are drawn here). */
export function query(v: View): Query {
  const q: Query = { archived: v.archived, sort: v.sort, reverse: v.reverse };
  if (v.search.trim()) q.search = v.search.trim();
  return q;
}

/** Whether a row is in a group ("" is the untagged rows). */
export function inGroup(r: Row, group: string): boolean {
  return group === "" ? r.tags.length === 0 : r.tags.includes(group);
}

type ViewStorage = Pick<Storage, "getItem" | "setItem">;

function loadView(storage: ViewStorage | null): View {
  try {
    const saved = storage?.getItem(VIEW_KEY);
    if (!saved) return { ...DEFAULT_VIEW };
    const v = JSON.parse(saved) as Partial<View>;
    const out = { ...DEFAULT_VIEW };
    if (v.archived === "live" || v.archived === "only" || v.archived === "all") {
      out.archived = v.archived;
    }
    if (SORTS.some((s) => s.sort === v.sort) && v.sort) out.sort = v.sort;
    if (typeof v.reverse === "boolean") out.reverse = v.reverse;
    if (typeof v.search === "string") out.search = v.search;
    if (v.tab === "sessions" || v.tab === "groups") out.tab = v.tab;
    if (typeof v.group === "string") out.group = v.group;
    return out;
  } catch {
    return { ...DEFAULT_VIEW };
  }
}

export class Store {
  readonly conn = signal<ConnState | "no_token">("connecting");
  readonly snapshot = signal<Snapshot | null>(null);
  /** The last failed request, for the user. */
  readonly error = signal<string | null>(null);
  readonly view: Signal<View>;

  /** The group the Groups tab shows: the picked one if it still exists. */
  readonly group = computed<string | null>(() => {
    const s = this.snapshot.value;
    const want = this.view.value.group;
    if (!s || s.groups.length === 0) return null;
    return s.groups.some((g) => g.tag === want) ? want : (s.groups[0]?.tag ?? null);
  });

  /** The rows to draw: all of them, or the picked group's. */
  readonly rows = computed<Row[]>(() => {
    const s = this.snapshot.value;
    if (!s) return [];
    if (this.view.value.tab !== "groups") return s.rows;
    const g = this.group.value;
    return g === null ? [] : s.rows.filter((r) => inGroup(r, g));
  });

  private api: Requester | null = null;
  /** The query the shown snapshot answers (a `rev` only means "unchanged" for it). */
  private shownQuery = "";
  private inFlight = false;
  private again = false;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private readonly storage: ViewStorage | null;

  constructor(storage: ViewStorage | null = null) {
    this.storage = storage;
    this.view = signal(loadView(storage));
  }

  /** The control socket said hello (again): fetch, passing the rev we have. */
  connected(api: Requester): void {
    this.api = api;
    this.error.value = null;
    void this.refresh();
  }

  /** The daemon changed: ask again soon, unless we already have that rev. */
  changed(rev: string): void {
    if (this.snapshot.value?.rev === rev) return;
    if (this.timer) clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      this.timer = null;
      void this.refresh();
    }, DEBOUNCE_MS);
  }

  setView(patch: Partial<View>): void {
    const before = JSON.stringify(query(this.view.value));
    this.view.value = { ...this.view.value, ...patch };
    try {
      this.storage?.setItem(VIEW_KEY, JSON.stringify(this.view.value));
    } catch {
      // Storage off: the view lasts as long as the page.
    }
    if (JSON.stringify(query(this.view.value)) !== before) void this.refresh();
  }

  /**
   * Fetch a snapshot for the current view. One request at a time: a
   * refresh asked for meanwhile runs once the current one is back.
   */
  async refresh(): Promise<void> {
    const api = this.api;
    if (!api) return;
    if (this.inFlight) {
      this.again = true;
      return;
    }
    this.inFlight = true;
    const q = query(this.view.value);
    const key = JSON.stringify(q);
    const have = this.snapshot.value;
    const args: { view: Query; rev?: string } = { view: q };
    if (have && key === this.shownQuery) args.rev = have.rev;
    try {
      const r = await api.request<Snapshot | Unchanged>("snapshot", args);
      // The view moved while this was out: its answer is for the old one.
      if (JSON.stringify(query(this.view.value)) !== key) {
        this.again = true;
      } else if (!("unchanged" in r)) {
        batch(() => {
          this.snapshot.value = r;
          this.error.value = null;
        });
        this.shownQuery = key;
      }
    } catch (e) {
      // Disconnected: the next hello fetches again.
      this.error.value = e instanceof Error ? e.message : String(e);
    } finally {
      this.inFlight = false;
    }
    if (this.again) {
      this.again = false;
      await this.refresh();
    }
  }

  /** Ask the daemon (an op on its allow-list); fails when not connected. */
  request<T = unknown>(op: string, args: object = {}): Promise<T> {
    if (!this.api) return Promise.reject(new Error("not connected"));
    return this.api.request<T>(op, args);
  }

  /** "Claude Code" for "claude", from the overlord's roster. */
  agentLabel(agent: string): string {
    return this.snapshot.value?.overlord?.overlords.find((o) => o.agent === agent)?.label ?? agent;
  }

  /** A row by id, from the current snapshot. */
  row(id: string): Row | undefined {
    return this.snapshot.value?.rows.find((r) => r.id === id);
  }
}
