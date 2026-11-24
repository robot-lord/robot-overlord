// The page: the token, the control socket, the store, and the routes.
// Wide screens show the list beside the pane; narrow ones one at a time.

import { effect } from "@preact/signals";
import { render } from "preact";
import { LocalTransport, takeToken, wsUrl } from "./api";
import { runUi } from "./commands";
import { FormHost } from "./components/FormHost";
import { LocalForms } from "./components/LocalForms";
import { Toast } from "./components/Toast";
import { tick } from "./components/time";
import type { UiCmd } from "./proto";
import { followHash, parse, route } from "./route";
import { listView, Store } from "./store";
import { LOCAL_CAPS, menu } from "./transport";
import { Reporter } from "./ui";
import { List, open as openForm } from "./views/List";
import { Overlord } from "./views/Overlord";
import { OverlordPane, SessionPane } from "./views/panes";
import { openFork, Session } from "./views/Session";
import "./app.css";

function safeStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

const storage = safeStorage();
const store = new Store(storage, { defaultView: true });
let build: string | null = null;
let version = "";

const token = takeToken(location, storage, (u) => history.replaceState(null, "", u));
// (After the token: the fragment was `#t=…` until then.)
route.value = parse(location.hash);
followHash();
tick();

/** Carry out an overlord's UI command (`ui.ts` decides). */
function onUi(cmd: UiCmd): void {
  runUi(cmd, store, LOCAL_CAPS, (h) => {
    location.hash = h;
  });
}

const reporter = new Reporter((args) => {
  if (control) control.request("page.report", args).catch(() => {});
});

const control = token
  ? new LocalTransport(wsUrl("/ws"), token, {
      state: (s) => {
        store.conn.value = s;
      },
      hello: (h) => {
        // A page from another build reloads (the plugin was updated).
        if (build !== null && build !== h.build) location.reload();
        build = h.build ?? null;
        version = h.version;
        if (!control) return;
        store.connected(control);
        // A new page id: tell the daemon everything again, and take any
        // command that was meant for this page before it reconnected.
        reporter.reset();
        report();
        control
          .request<UiCmd | null>("page.pending")
          .then((c) => c && onUi(c))
          .catch(() => {});
      },
      changed: (rev) => store.changed(rev),
      ui: (cmd) => {
        onUi(cmd);
        // "Got it": the daemon won't hand it to another page.
        control?.request("page.report", {}).catch(() => {});
      },
    })
  : null;
if (!token) store.conn.value = "no_token";

/** What this page shows, to the daemon (only what changed). */
function report(): void {
  // (Read first: the effect follows what it read. The connection isn't
  // followed: on connecting, hello reports everything.)
  const shown = {
    route: route.value,
    view: listView(store.view.value),
    visible: document.visibilityState === "visible",
    form: store.form.value?.kind ?? null,
  };
  if (store.conn.peek() === "open") reporter.shown(shown);
}
effect(report);
document.addEventListener("visibilitychange", report);
// The list's keys: n (describe a new session), N (manual), f (fork the
// session shown). Never while
// typing in a field or a pane, or with a form open.
document.addEventListener("keydown", (e) => {
  const t = e.target instanceof HTMLElement ? e.target : null;
  if (e.metaKey || e.ctrlKey || e.altKey || store.form.value) return;
  if (t?.closest("input, textarea, select, [contenteditable], .pane")) return;
  const r = route.value;
  if (e.key === "n") openForm(store, "describe");
  else if (e.key === "N") openForm(store, "manual");
  else if (e.key === "f" && r.kind === "session") {
    const s = store.row(r.id);
    if (!s) return;
    openFork(store, s);
  } else return;
  e.preventDefault();
});
for (const ev of ["pointerdown", "keydown", "wheel"] as const) {
  document.addEventListener(ev, () => reporter.input(), { capture: true, passive: true });
}

const words: Record<string, string> = {
  connecting: "connecting…",
  open: "connected",
  closed: "disconnected, retrying…",
  unauthorized: "this link has expired: run `rlord web` again",
  no_token: "no token: open the link `rlord web` prints",
};

const caps = control?.caps ?? LOCAL_CAPS;

function App() {
  const r = route.value;
  const conn = store.conn.value;
  return (
    <div class={`app at-${r.kind}`}>
      <header class="bar">
        <a class="brand" href="#/">
          rlord
        </a>
        <span class={`conn conn-${conn}`} title={version ? `rlord-web ${version}` : undefined}>
          {words[conn]}
        </span>
        {store.error.value && conn === "open" && <span class="err">{store.error.value}</span>}
        {conn === "open" && (
          <span class="menu">
            {menu(caps).map((m) =>
              m === "devices" ? (
                <DevicesButton />
              ) : (
                <button type="button" class="dim" onClick={() => openForm(store, m)}>
                  {m === "setup" ? "Code folder" : "Keys"}
                </button>
              ),
            )}
          </span>
        )}
      </header>
      <FormHost store={store} local={(f) => <LocalForms store={store} form={f} />} />
      <Toast store={store} />
      <div class="body">
        <aside class="side">
          <List store={store} route={r} />
        </aside>
        <main class="main">
          {r.kind === "list" || !token ? (
            <p class="hint">Pick the overlord or a session.</p>
          ) : r.kind === "overlord" ? (
            <Overlord
              store={store}
              body={(o) => <OverlordPane store={store} o={o} token={token} />}
            />
          ) : (
            <Session
              key={r.id}
              store={store}
              id={r.id}
              body={(s, act) => <SessionPane s={s} act={act} token={token} />}
            />
          )}
        </main>
      </div>
    </div>
  );
}

/** "Devices" (remote access), marked while a device waits for the user. */
function DevicesButton() {
  const relay = store.snapshot.value?.relay;
  const waiting = relay?.pending ?? null;
  return (
    <button
      type="button"
      class={waiting ? "attention" : "dim"}
      title={waiting ? `"${waiting.name}" wants to pair` : "remote access: paired devices"}
      onClick={() => openForm(store, "devices")}
    >
      Devices{waiting ? " •" : ""}
    </button>
  );
}

const root = document.getElementById("app");
if (root) render(<App />, root);
