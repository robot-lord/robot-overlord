// A session (`#/s/<id>`): its header and its buttons, then what the entry
// puts below them: the local page's pane (views/panes.tsx). The phone's
// view is compact (views/phonePanes.tsx): a short header, then tabs, its
// details and buttons in one of them.

import type { ComponentChildren } from "preact";
import { useEffect, useState } from "preact/hooks";
import { type Runner, SessionActions, useRunner } from "../components/Actions";
import { Status, Work } from "../components/Status";
import type { Row, SessionBase } from "../proto";
import type { Store } from "../store";

/** Open the fork form for `s` (not over a form the user is typing in). */
export function openFork(store: Store, s: SessionBase): void {
  if (store.form.value?.touched || s.role === "overlord") return;
  store.form.value = { kind: "fork", cmd: null, touched: false, session: s };
}

/** Back to the list (narrow screens only show one at a time). */
export function Back() {
  return (
    <a class="back" href="#/" aria-label="back to the list">
      ‹
    </a>
  );
}

export function Session({
  store,
  id,
  body,
  compact = false,
}: {
  store: Store;
  id: string;
  /**
   * Below the buttons: the pane. `details` is the session's facts and
   * buttons, for a compact view to place (else they are above).
   */
  body: (s: SessionBase, act: Runner, details: ComponentChildren) => ComponentChildren;
  /** Only the name and status on top. */
  compact?: boolean;
}) {
  const listed: SessionBase | undefined = store.row(id);
  // Not in the list's view (archived, filtered out): asked for once.
  const [fetched, setFetched] = useState<SessionBase | null>(null);
  const [missing, setMissing] = useState<string | null>(null);
  // Bumped after each action, to fetch again a session the list doesn't
  // show (the list's own rows come with the next snapshot).
  const [gen, setGen] = useState(0);
  const act = useRunner(store, () => setGen((g) => g + 1));
  useEffect(() => {
    setFetched(null);
    setMissing(null);
    if (listed) return;
    let live = true;
    store
      .request<Row>("session.get", { ref: id })
      .then((r) => live && setFetched(r))
      .catch((e: unknown) => live && setMissing(e instanceof Error ? e.message : String(e)));
    return () => {
      live = false;
    };
    // (Once per session and action: a listed row needs no fetch.)
  }, [id, listed === undefined, gen]);
  const s = listed ?? fetched;
  if (!s) {
    return (
      <section class="view">
        <header class="view-head">
          <Back />
        </header>
        <p class="hint">{missing ?? "loading…"}</p>
      </section>
    );
  }
  const facts = (
    <>
      <span class="dim">{store.agentLabel(s.agent)}</span>
      {s.model && <span class="dim">{s.model}</span>}
      {s.branch && <span class="dim">⎇ {s.branch}</span>}
      {s.tags.map((t) => (
        <span class="tag">#{t}</span>
      ))}
      {s.description && <p class="desc-line">{s.description}</p>}
      <Work s={s} />
    </>
  );
  const actions = <SessionActions s={s} r={act} onFork={() => openFork(store, s)} />;
  if (compact) {
    const details = (
      <div class="details">
        <div class="view-head">{facts}</div>
        {s.notes && <p class="notes">{s.notes}</p>}
        {actions}
      </div>
    );
    return (
      <section class="view">
        <header class="view-head">
          <Back />
          <h1>{s.name}</h1>
          <Status s={s} full />
        </header>
        {body(s, act, details)}
      </section>
    );
  }
  return (
    <section class="view">
      <header class="view-head">
        <Back />
        <h1>{s.name}</h1>
        <Status s={s} full />
        {facts}
      </header>
      {actions}
      {body(s, act, null)}
    </section>
  );
}
