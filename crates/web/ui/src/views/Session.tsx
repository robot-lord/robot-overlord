// A session (`#/s/<id>`): its header and its pane, or Resume when it
// isn't running.

import { useEffect, useState } from "preact/hooks";
import { Pane } from "../components/Pane";
import { Status } from "../components/Status";
import type { Row, SessionBase } from "../proto";
import type { Store } from "../store";

/** Back to the list (narrow screens only show one at a time). */
export function Back() {
  return (
    <a class="back" href="#/" aria-label="back to the list">
      ‹
    </a>
  );
}

export function Session({ store, id, token }: { store: Store; id: string; token: string }) {
  const listed: SessionBase | undefined = store.row(id);
  // Not in the list's view (archived, filtered out): asked for once.
  const [fetched, setFetched] = useState<SessionBase | null>(null);
  const [missing, setMissing] = useState<string | null>(null);
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
    // (Once per session: a listed row needs no fetch.)
  }, [id, listed === undefined]);
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
  return (
    <section class="view">
      <header class="view-head">
        <Back />
        <h1>{s.name}</h1>
        <Status status={s.status} label={s.status_label} />
        <span class="dim">{store.agentLabel(s.agent)}</span>
        {s.model && <span class="dim">{s.model}</span>}
        {s.branch && <span class="dim">⎇ {s.branch}</span>}
        {s.tags.map((t) => (
          <span class="tag">#{t}</span>
        ))}
        {s.description && <p class="desc-line">{s.description}</p>}
      </header>
      <Pane
        key={s.id}
        target={s.id}
        token={token}
        watch={`${s.status}:${s.tmux_session ?? ""}`}
        notRunning={
          <>
            <p>{s.archived_at ? "This session is archived." : "This session isn't running."}</p>
            <button type="button" disabled title="Resuming from the web comes in a later build">
              Resume
            </button>
          </>
        }
      />
    </section>
  );
}
