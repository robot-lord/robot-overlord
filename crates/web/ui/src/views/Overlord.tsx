// The overlord (`#/overlord`): the active overlord's pane, its status and
// its agents. The pane follows a switch to another agent's overlord.

import { useState } from "preact/hooks";
import { OverlordActions, useRunner } from "../components/Actions";
import { Pane } from "../components/Pane";
import { Status } from "../components/Status";
import type { Store } from "../store";
import { Back } from "./Session";

export function Overlord({ store, token }: { store: Store; token: string }) {
  const o = store.snapshot.value?.overlord ?? null;
  const [starting, setStarting] = useState<string | null>(null);
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
  const start = () => {
    setStarting("starting…");
    store
      .request("overlord.ensure")
      .then(() => setStarting(null))
      .catch((e: unknown) => setStarting(e instanceof Error ? e.message : String(e)));
  };
  return (
    <section class="view">
      <header class="view-head">
        <Back />
        <h1>overlord</h1>
        <Status status={o.status} label={o.status_label} />
        <span class="dim">{active?.label ?? o.agent}</span>
        {o.model && <span class="dim">{o.model}</span>}
      </header>
      <OverlordActions o={o} r={r} />
      <Pane
        target="overlord"
        token={token}
        watch={`${o.id}:${o.status}`}
        expectId={o.id}
        notRunning={
          <>
            <p>The overlord isn't running.</p>
            <button type="button" onClick={start} disabled={starting === "starting…"}>
              Start it
            </button>
            {starting && <p class="dim">{starting}</p>}
          </>
        }
      />
    </section>
  );
}
