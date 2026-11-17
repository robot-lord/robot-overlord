// A pane on screen: the terminal, and what to say when it isn't showing.
// One socket while mounted (`mountPane`); the view remounts it (by `key`)
// for another target.

import type { ComponentChildren } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import { wsUrl } from "../api";
import { mountPane, type PaneSocket, type PaneState } from "../pane";

export function Pane({
  target,
  token,
  watch,
  expectId,
  notRunning,
}: {
  target: string;
  token: string;
  /** When this changes and the pane isn't showing, try again (the target may be running now). */
  watch: string;
  /** The session the pane should show: reopen when it shows another (the overlord was switched). */
  expectId?: string | null;
  /** What to offer when the target isn't running. */
  notRunning: ComponentChildren;
}) {
  const el = useRef<HTMLDivElement>(null);
  const sock = useRef<PaneSocket | null>(null);
  const [state, setState] = useState<PaneState>({ kind: "connecting" });

  useEffect(() => {
    if (!el.current) return;
    const p = mountPane(el.current, { url: wsUrl("/pane"), token, target, onState: setState });
    sock.current = p.socket;
    return () => {
      sock.current = null;
      p.dispose();
    };
  }, [target, token]);

  useEffect(() => {
    const s = sock.current;
    if (!s) return;
    const st = s.state;
    if (st.kind === "not_running" || st.kind === "not_found" || st.kind === "retrying") {
      s.connect();
    } else if (st.kind === "open" && expectId && st.id && st.id !== expectId) {
      s.connect();
    }
  }, [watch, expectId]);

  return (
    <div class="pane">
      <div class="term" ref={el} />
      {state.kind !== "open" && (
        <div class="pane-note">
          <Note state={state} notRunning={notRunning} />
        </div>
      )}
    </div>
  );
}

function Note({ state, notRunning }: { state: PaneState; notRunning: ComponentChildren }) {
  switch (state.kind) {
    case "open":
      return null;
    case "connecting":
      return <p>connecting…</p>;
    case "not_running":
      return <div class="not-running">{notRunning}</div>;
    case "not_found":
      return <p>{state.message || "not found"}</p>;
    case "hidden":
      return <p>closed while the tab was hidden</p>;
    case "unauthorized":
      return <p>this link has expired: run `rlord web` again</p>;
    case "retrying":
      return <p>{state.message}: trying again…</p>;
  }
}
