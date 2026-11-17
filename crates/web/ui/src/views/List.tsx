// The list (`#/`): the overlord on top with a box to tell it something,
// then the sessions. Sessions and Groups tabs, a filter, the sort, and
// live/archived. The daemon sorts and groups; this draws.

import { useSignal, useSignalEffect } from "@preact/signals";
import { useEffect } from "preact/hooks";
import { Tell } from "../components/Actions";
import { now, ago, lastUpdate } from "../components/time";
import { Status } from "../components/Status";
import { usageFigure } from "../components/usage";
import type { Overlord, Row, Snapshot } from "../proto";
import { href, type Route } from "../route";
import { SORTS, type Store, type View } from "../store";

/** "Claude Code" for "claude", from the overlord's roster. */
function agentLabels(s: Snapshot | null): Map<string, string> {
  return new Map((s?.overlord?.overlords ?? []).map((o) => [o.agent, o.label]));
}

export function List({ store, route }: { store: Store; route: Route }) {
  const snap = store.snapshot.value;
  const view = store.view.value;
  const labels = agentLabels(snap);
  // The open session, else the row the overlord asked to highlight.
  const selected = route.kind === "session" ? route.id : store.selected.value;
  return (
    <div class="list">
      <Controls store={store} view={view} />
      {snap && <OverlordRow o={snap.overlord} labels={labels} active={route.kind === "overlord"} />}
      {snap?.overlord && <Tell store={store} />}
      {view.tab === "groups" && snap && <Groups store={store} snap={snap} />}
      {snap ? (
        <Rows rows={store.rows.value} labels={labels} selected={selected} view={view} />
      ) : (
        <p class="empty">{store.conn.value === "open" ? "loading…" : "waiting for rlord…"}</p>
      )}
    </div>
  );
}

function Controls({ store, view }: { store: Store; view: View }) {
  // Typing waits a moment before asking the daemon.
  const text = useSignal(view.search);
  useSignalEffect(() => {
    const t = text.value;
    if (t === store.view.peek().search) return;
    const id = setTimeout(() => store.setView({ search: t }), 200);
    return () => clearTimeout(id);
  });
  // A view set from elsewhere (the overlord, the default) shows its filter.
  useEffect(() => {
    if (text.peek() !== view.search) text.value = view.search;
  }, [view.search]);
  return (
    <div class="controls">
      <div class="tabs" role="tablist">
        {(["sessions", "groups"] as const).map((t) => (
          <button
            type="button"
            role="tab"
            aria-selected={view.tab === t}
            class={view.tab === t ? "on" : ""}
            onClick={() => store.setView({ tab: t })}
          >
            {t === "sessions" ? "Sessions" : "Groups"}
          </button>
        ))}
      </div>
      <input
        class="filter"
        type="search"
        placeholder="filter"
        title="words match name, project, tags, description · status:needs|turn|bg|working|limited · tag:x · proj:x · agent:x"
        aria-label="filter sessions"
        value={text.value}
        onInput={(e) => {
          text.value = e.currentTarget.value;
        }}
      />
      <label class="sort">
        <span class="dim">sort</span>
        <select
          value={view.sort}
          onChange={(e) => {
            const v = SORTS.find((s) => s.sort === e.currentTarget.value);
            if (v) store.setView({ sort: v.sort });
          }}
        >
          {SORTS.map((s) => (
            <option value={s.sort}>{s.label}</option>
          ))}
        </select>
      </label>
      <button
        type="button"
        class="reverse"
        title="reverse the order"
        aria-pressed={view.reverse}
        onClick={() => store.setView({ reverse: !view.reverse })}
      >
        {view.reverse ? "↑" : "↓"}
      </button>
      <select
        class="archived"
        aria-label="live or archived"
        value={view.archived}
        onChange={(e) => {
          const v = e.currentTarget.value;
          if (v === "live" || v === "only" || v === "all") store.setView({ archived: v });
        }}
      >
        <option value="live">live</option>
        <option value="only">archived</option>
        <option value="all">live and archived</option>
      </select>
    </div>
  );
}

function OverlordRow({
  o,
  labels,
  active,
}: {
  o: Overlord | null;
  labels: Map<string, string>;
  active: boolean;
}) {
  if (!o) {
    return (
      <div class="row overlord">
        <span class="name">overlord</span>
        <span class="dim">not set up yet: run rlord in a terminal</span>
      </div>
    );
  }
  return (
    <a class={`row overlord${active ? " selected" : ""}`} href={href({ kind: "overlord" })}>
      <Status s={o} at={now.value} />
      <span class="name">overlord</span>
      <span class="agent">{labels.get(o.agent) ?? o.agent}</span>
    </a>
  );
}

function Groups({ store, snap }: { store: Store; snap: Snapshot }) {
  const picked = store.group.value;
  if (snap.groups.length === 0) return <p class="empty">no groups</p>;
  return (
    <div class="groups" role="listbox" aria-label="groups">
      {snap.groups.map((g) => (
        <button
          type="button"
          role="option"
          aria-selected={g.tag === picked}
          class={`group${g.tag === picked ? " on" : ""}`}
          onClick={() => store.setView({ group: g.tag })}
        >
          {g.favourite && (
            <span class="fav" title="favourite">
              ★
            </span>
          )}
          <span class={g.tag === "" ? "dim" : ""}>{g.label}</span>
          <span class="count">{g.count}</span>
        </button>
      ))}
    </div>
  );
}

function UsageFigure({ r }: { r: Row }) {
  const f = usageFigure(r);
  if (!f) return null;
  return (
    <span class="usage" title={f[1]}>
      {" · "}
      {f[0]}
    </span>
  );
}

function Rows({
  rows,
  labels,
  selected,
  view,
}: {
  rows: Row[];
  labels: Map<string, string>;
  selected: string | null;
  view: View;
}) {
  if (rows.length === 0) {
    return <p class="empty">{view.search ? "nothing matches" : "no sessions"}</p>;
  }
  const at = now.value;
  return (
    <ul class="rows">
      {rows.map((r) => (
        <li key={r.id}>
          <a
            class={`row${r.id === selected ? " selected" : ""}${r.archived_at ? " archived" : ""}`}
            href={href({ kind: "session", id: r.id })}
          >
            <Status s={r} at={at} />
            <span class="name">
              {r.pinned && (
                <span class="pin" title="pinned">
                  ▴
                </span>
              )}
              {r.name}
            </span>
            <span class="agent">
              {labels.get(r.agent) ?? r.agent}
              <UsageFigure r={r} />
            </span>
            <span class="when" title={new Date(lastUpdate(r)).toLocaleString()}>
              {ago(lastUpdate(r), at)}
            </span>
            {(r.tags.length > 0 || r.description) && (
              <span class="more">
                {r.tags.map((t) => (
                  <span class="tag">#{t}</span>
                ))}
                {r.description && <span class="desc">{r.description}</span>}
              </span>
            )}
          </a>
        </li>
      ))}
    </ul>
  );
}
