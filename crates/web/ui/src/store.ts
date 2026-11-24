// The page's state, as signals: the connection, the latest snapshot, and
// the view the user picked (filter, sort, tabs: the browser's own, kept in
// localStorage). The daemon does the grouping, sorting and labels; this
// only asks for snapshots at the right times.

import { batch, computed, type Signal, signal } from "@preact/signals";
import type {
  Control,
  ListView,
  PageOp,
  Query,
  Row,
  Snapshot,
  Sort,
  UiCmd,
  Unchanged,
} from "./proto";
import type { ConnState } from "./transport";
import type { OpenForm } from "./ui";

/** What the store needs of its transport (a fake in tests). */
export interface Requester {
  request<T = unknown>(op: PageOp, args?: object): Promise<T>;
}

export type Tab = "sessions" | "groups";

export interface View {
  archived: "live" | "only" | "all";
  sort: Sort;
  reverse: boolean;
  /** The filter box's text (the TUI's syntax, read by the daemon). */
  search: string;
  tab: Tab;
  /** The group picked in the Groups tab ("" is untagged; null: the first). */
  group: string | null;
  /** The usage measure; null: the user's own (or the configured one). */
  usage: string | null;
}

export const DEFAULT_VIEW: View = {
  archived: "live",
  sort: "activity",
  reverse: false,
  search: "",
  tab: "sessions",
  group: null,
  usage: null,
};

/** A sort as the daemon labels it, as the page's `Sort`. */
export function sortOf(label: string): Sort | null {
  const m: Record<string, Sort> = {
    updated: "activity",
    activity: "activity",
    user: "user",
    state: "state",
    created: "created",
    name: "name",
    usage: "usage",
  };
  return m[label] ?? null;
}

/** A page's view in the daemon's shape (what `page.report` sends). */
export function listView(v: View): ListView {
  return {
    query: v.search.trim(),
    sort: v.sort === "activity" ? "updated" : v.sort,
    reverse: v.reverse,
    usage: v.usage,
    archived: v.archived,
    tab: v.tab,
    group: v.group,
  };
}

/** A view in the daemon's shape (the overlord's `set_view`, the default) as the page's. */
export function viewOf(l: ListView): View {
  const archived = l.archived === "only" || l.archived === "all" ? l.archived : "live";
  return {
    archived,
    sort: sortOf(l.sort) ?? "activity",
    reverse: l.reverse,
    search: l.query,
    tab: l.tab === "groups" ? "groups" : "sessions",
    group: l.group,
    usage: l.usage,
  };
}

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
  if (v.search.trim()) q.query = v.search.trim();
  if (v.usage) q.usage = v.usage;
  return q;
}

/** Whether a row is in a group ("" is the untagged rows). */
export function inGroup(r: Row, group: string): boolean {
  return group === "" ? r.tags.length === 0 : r.tags.includes(group);
}

type ViewStorage = Pick<Storage, "getItem" | "setItem">;

function savedView(storage: ViewStorage | null): boolean {
  try {
    return Boolean(storage?.getItem(VIEW_KEY));
  } catch {
    return false;
  }
}

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
    if (typeof v.usage === "string") out.usage = v.usage;
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
  /** The row the overlord asked to highlight (`set_view`'s `select`). */
  readonly selected = signal<string | null>(null);
  /** A one-line notice for the user (the overlord did something elsewhere). */
  readonly toast = signal<string | null>(null);
  /** The form open over the page, if any. */
  readonly form = signal<OpenForm | null>(null);
  /** A form the overlord opened while the user was typing in another. */
  readonly parked = signal<UiCmd | null>(null);
  /** This screen as a pane's holder is named (`page:<id>`, `device:<id>`; control.ts). */
  readonly surface = signal<string | null>(null);
  /** This screen may type into panes (false: a view-only device). */
  readonly input = signal(true);

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
  /** The browser has a view of its own (else it adopts the daemon's default). */
  private ownView: boolean;

  /**
   * `defaultView`: a browser with no saved view starts from the daemon's
   * default view (the page does; tests mostly don't).
   */
  constructor(storage: ViewStorage | null = null, opts: { defaultView?: boolean } = {}) {
    this.storage = storage;
    this.ownView = !opts.defaultView || savedView(storage);
    this.view = signal(loadView(storage));
  }

  /**
   * The control socket said hello (again): fetch, passing the rev we have.
   * A browser with no view of its own first takes the daemon's default.
   */
  connected(api: Requester): void {
    this.api = api;
    this.error.value = null;
    if (this.ownView) {
      void this.refresh();
      return;
    }
    void this.adoptDefault(api);
  }

  /** Take the daemon's default view (unless the user picked one meanwhile), then fetch. */
  private async adoptDefault(api: Requester): Promise<void> {
    try {
      const d = await api.request<ListView>("view.default");
      if (!this.ownView) this.view.value = viewOf(d);
    } catch {
      // Not answered: the built-in view.
    }
    await this.refresh();
  }

  /** The overlord's view for this page: shown, and kept as the browser's own. */
  applyView(l: ListView, select: string | null = null): void {
    this.setView(viewOf(l));
    this.selected.value = select;
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
    this.ownView = true;
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
  request<T = unknown>(op: PageOp, args: object = {}): Promise<T> {
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

  /**
   * Who holds a session's pane, from the snapshot (the overlord's too):
   * null when nobody does, undefined when the snapshot doesn't say.
   */
  controlOf(id: string | null): Control | null | undefined {
    if (!id) return undefined;
    const s = this.snapshot.value;
    const o = s?.overlord;
    const found: { control?: Control | null } | undefined =
      o && o.id === id ? o : s?.rows.find((r) => r.id === id);
    return found?.control;
  }
}
