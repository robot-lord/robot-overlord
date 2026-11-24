// The overlord (`#/overlord`): its status, its agents and its buttons,
// then what the entry puts below them: the local page's pane
// (views/panes.tsx on the local page, views/phonePanes.tsx on the phone).

import type { ComponentChildren } from "preact";
import { useState } from "preact/hooks";
import { OverlordActions, useRunner } from "../components/Actions";
import { Status, Work } from "../components/Status";
import type { Overlord as OverlordRow } from "../proto";
import type { Store } from "../store";
import { Back } from "./Session";

/** Start the overlord (`overlord.ensure`), saying how it went. */
export function StartOverlord({ store }: { store: Store }) {
  const [starting, setStarting] = useState<string | null>(null);
  const start = () => {
    setStarting("starting…");
    store
      .request("overlord.ensure")
      .then(() => setStarting(null))
      .catch((e: unknown) => setStarting(e instanceof Error ? e.message : String(e)));
  };
  return (
    <>
      <button type="button" onClick={start} disabled={starting === "starting…"}>
        Start it
      </button>
      {starting && <p class="dim">{starting}</p>}
    </>
  );
}

export function Overlord({
  store,
  body,
}: {
  store: Store;
  /** Below the buttons: the pane (and on the phone the transcript). */
  body: (o: OverlordRow) => ComponentChildren;
}) {
  const o = store.snapshot.value?.overlord ?? null;
  const r = useRunner(store);
  if (!store.snapshot.value) return <p class="hint">loading…</p>;
  if (!o) {
    return (
      <section class="view">
        <header class="view-head">
          <Back />
          <h1>overlord</h1>
        </header>
        <p class="hint">No overlord yet: run rlord in a terminal to set one up.</p>
      </section>
    );
  }
  const active = o.overlords.find((a) => a.active);
  return (
    <section class="view">
      <header class="view-head">
        <Back />
        <h1>overlord</h1>
        <Status s={o} full />
        <span class="dim">{active?.label ?? o.agent}</span>
        {o.model && <span class="dim">{o.model}</span>}
        <Work s={o} />
      </header>
      <OverlordActions o={o} r={r} />
      {body(o)}
    </section>
  );
}
