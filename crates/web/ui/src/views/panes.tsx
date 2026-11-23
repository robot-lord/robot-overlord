// The local page's bodies for the overlord and session views: their live
// panes (xterm.js), or Start / Resume when not running. Only main.tsx
// imports this, so the phone's bundle has no terminal.

import { ResumeButton, type Runner, RunnerNote } from "../components/Actions";
import { Pane } from "../components/Pane";
import type { Overlord, SessionBase } from "../proto";
import type { Store } from "../store";
import { StartOverlord } from "./Overlord";

/** The overlord's pane; it follows a switch to another agent's overlord. */
export function OverlordPane({ store, o, token }: { store: Store; o: Overlord; token: string }) {
  return (
    <Pane
      target="overlord"
      token={token}
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
export function SessionPane({ s, act, token }: { s: SessionBase; act: Runner; token: string }) {
  return (
    <Pane
      key={s.id}
      target={s.id}
      token={token}
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
