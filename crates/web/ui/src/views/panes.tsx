// The local page's bodies for the overlord and session views: their live
// panes on the local pane socket (pane.ts), or Start / Resume when not
// running. Only main.tsx imports this, so the phone's bundle has no
// websocket.

import { useMemo } from "preact/hooks";
import { wsUrl } from "../api";
import { ResumeButton, type Runner, RunnerNote } from "../components/Actions";
import { Pane } from "../components/Pane";
import { PaneSocket } from "../pane";
import type { MakeLink } from "../paneLink";
import type { Overlord, SessionBase } from "../proto";
import type { Store } from "../store";
import { StartOverlord } from "./Overlord";

/** A pane socket for `target`, naming this page to the plugin (control). */
function useSocket(store: Store, target: string, token: string): MakeLink {
  return useMemo<MakeLink>(
    () => (screen, onState) =>
      new PaneSocket({
        url: wsUrl("/pane"),
        token,
        target,
        page: () => {
          const s = store.surface.peek();
          return s?.startsWith("page:") ? s.slice("page:".length) : null;
        },
        screen,
        onState,
      }),
    [store, target, token],
  );
}

/** The overlord's pane; it follows a switch to another agent's overlord. */
export function OverlordPane({ store, o, token }: { store: Store; o: Overlord; token: string }) {
  const makeLink = useSocket(store, "overlord", token);
  return (
    <Pane
      store={store}
      makeLink={makeLink}
      watch={`${o.id}:${o.status}`}
      expectId={o.id}
      notRunning={
        <>
          <p>The overlord isn't running.</p>
          <StartOverlord store={store} />
        </>
      }
    />
  );
}

/** A session's pane, or Resume. */
export function SessionPane({
  store,
  s,
  act,
  token,
}: {
  store: Store;
  s: SessionBase;
  act: Runner;
  token: string;
}) {
  const makeLink = useSocket(store, s.id, token);
  return (
    <Pane
      key={s.id}
      store={store}
      makeLink={makeLink}
      watch={`${s.status}:${s.tmux_session ?? ""}`}
      notRunning={
        <>
          <p>{s.archived_at ? "This session is archived." : "This session isn't running."}</p>
          <ResumeButton s={s} r={act} />
          <p>
            <RunnerNote r={act} />
          </p>
        </>
      }
    />
  );
}
