// The phone's bodies for the overlord and session views: a live pane over
// the bridge (remotePane.ts) with the key row, the read-only transcript,
// and a session's details, in tabs. Only app.tsx imports this.

import type { ComponentChildren } from "preact";
import { useMemo, useState } from "preact/hooks";
import { isRunning } from "../actions";
import type { Runner } from "../components/Actions";
import { Pane, type PhoneExtras } from "../components/Pane";
import type { MakeLink } from "../paneLink";
import type { Overlord, SessionBase } from "../proto";
import type { RemoteTransport } from "../remote";
import { RemotePaneLink } from "../remotePane";
import type { Store } from "../store";
import { StartOverlord } from "./Overlord";
import { SessionText } from "./Transcript";

/** A bridge stream for `target`. */
function useStream(t: RemoteTransport, target: string): MakeLink {
  return useMemo<MakeLink>(
    () => (screen, onState) => new RemotePaneLink({ bridge: t, target, screen, onState }),
    [t, target],
  );
}

export type Tab = "terminal" | "transcript" | "details";

const LABELS: Record<Tab, string> = {
  terminal: "Terminal",
  transcript: "Transcript",
  details: "Details",
};

/** The tabs a session shows: the terminal first while it runs. */
export function sessionTabs(running: boolean): Tab[] {
  return running ? ["terminal", "transcript", "details"] : ["transcript", "details"];
}

function Tabs({ tabs, on, pick }: { tabs: Tab[]; on: Tab; pick: (t: Tab) => void }) {
  return (
    <div class="tabs view-tabs" role="tablist">
      {tabs.map((t) => (
        <button
          type="button"
          role="tab"
          aria-selected={t === on}
          class={t === on ? "on" : ""}
          onClick={() => pick(t)}
        >
          {LABELS[t]}
        </button>
      ))}
    </div>
  );
}

/** The tab shown: the one picked if it is offered, else the first. */
function useTab(tabs: Tab[]): [Tab, (t: Tab) => void] {
  const [picked, pick] = useState<Tab | null>(null);
  const first = tabs[0] ?? "transcript";
  return [picked && tabs.includes(picked) ? picked : first, pick];
}

/** The overlord: its terminal (Start when it isn't running) and its transcript. */
export function OverlordBody({
  store,
  t,
  o,
  extras,
}: {
  store: Store;
  t: RemoteTransport;
  o: Overlord;
  extras: PhoneExtras;
}) {
  const makeLink = useStream(t, "overlord");
  const [tab, pick] = useTab(["terminal", "transcript"]);
  return (
    <>
      <Tabs tabs={["terminal", "transcript"]} on={tab} pick={pick} />
      {tab === "terminal" ? (
        <Pane
          store={store}
          makeLink={makeLink}
          watch={`${o.id}:${o.status}`}
          expectId={o.id}
          phone={extras}
          notRunning={
            <>
              <p>The overlord isn't running.</p>
              <StartOverlord store={store} />
            </>
          }
        />
      ) : (
        <SessionText store={store} api={t} s={o} />
      )}
    </>
  );
}

/** A session: Terminal (while it runs), Transcript, Details. */
export function SessionBody({
  store,
  t,
  s,
  act,
  details,
  extras,
}: {
  store: Store;
  t: RemoteTransport;
  s: SessionBase;
  act: Runner;
  details: ComponentChildren;
  extras: PhoneExtras;
}) {
  const running = isRunning(s);
  const tabs = sessionTabs(running);
  const [tab, pick] = useTab(tabs);
  return (
    <>
      <Tabs tabs={tabs} on={tab} pick={pick} />
      {tab === "terminal" ? (
        <SessionTerm store={store} t={t} s={s} extras={extras} />
      ) : tab === "transcript" ? (
        <SessionText store={store} api={t} s={s} act={act} />
      ) : (
        details
      )}
    </>
  );
}

function SessionTerm({
  store,
  t,
  s,
  extras,
}: {
  store: Store;
  t: RemoteTransport;
  s: SessionBase;
  extras: PhoneExtras;
}) {
  const makeLink = useStream(t, s.id);
  return (
    <Pane
      key={s.id}
      store={store}
      makeLink={makeLink}
      watch={`${s.status}:${s.tmux_session ?? ""}`}
      phone={extras}
      notRunning={<p>This session isn't running.</p>}
    />
  );
}
