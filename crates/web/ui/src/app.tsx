// The phone app's page (app.html, built into dist-app/ by `cargo xtask
// web build --app`). The same store, list, forms and session buttons as
// the local page, over `RemoteTransport` to the app's engine: live panes
// as streams over the bridge (with the key row), the read-only
// transcript, no local-only screens, and the app's own pairing and
// status screens. It has no network of its own.

import { effect, signal } from "@preact/signals";
import { render } from "preact";
import { FormHost } from "./components/FormHost";
import { Toast } from "./components/Toast";
import { tick } from "./components/time";
import type { PhoneExtras } from "./components/Pane";
import { debugBuild, Phone, screen, viewportVars, waitingWords } from "./phone";
import { wirePhone } from "./phoneApp";
import { type BridgeHost, findBridge, type RemoteTransport } from "./remote";
import { followHash, parse, route } from "./route";
import { listView, Store } from "./store";
import { Reporter } from "./ui";
import { List } from "./views/List";
import { Overlord } from "./views/Overlord";
import { ConnLine, Pairing, StatusScreen } from "./views/Phone";
import { OverlordBody, SessionBody } from "./views/phonePanes";
import { Back, Session } from "./views/Session";
import "./app.css";
import "./phone.css";

function safeStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

const store = new Store(safeStorage(), { defaultView: true });
const phone = new Phone();
/** The status screen (`#/status`): the phone's own, not a route the daemon knows. */
const statusOpen = signal(location.hash === "#/status");

route.value = parse(location.hash);
followHash();
window.addEventListener("hashchange", () => {
  statusOpen.value = location.hash === "#/status";
});
tick();

// The app over the part of the screen the keyboard leaves.
const vv = window.visualViewport;
if (vv) {
  const fit = () => {
    for (const [k, v] of Object.entries(viewportVars(vv))) {
      document.documentElement.style.setProperty(k, v);
    }
  };
  vv.addEventListener("resize", fit);
  vv.addEventListener("scroll", fit);
  fit();
}

const reporter = new Reporter((args) => {
  transport?.request("page.report", args).catch(() => {});
});

/** What this page shows, to the daemon (only what changed). */
function report(): void {
  const shown = {
    route: route.value,
    view: listView(store.view.value),
    visible: document.visibilityState === "visible",
    form: store.form.value?.kind ?? null,
  };
  if (store.conn.peek() === "open") reporter.shown(shown);
}

const bridge = findBridge(window as unknown as BridgeHost);
const transport: RemoteTransport | null = bridge
  ? wirePhone({
      store,
      phone,
      bridge,
      go: (h) => {
        location.hash = h;
      },
      onHello: () => {
        reporter.reset();
        report();
      },
    })
  : null;
if (!transport) store.conn.value = "closed";
// The shell's settings (a debug build mirrors panes' text for UI tests).
transport?.send({ app: "settings" });

effect(report);
document.addEventListener("visibilitychange", report);
for (const ev of ["pointerdown", "keydown"] as const) {
  document.addEventListener(ev, () => reporter.input(), { capture: true, passive: true });
}

/** The overlord's button in the header, marked when it waits for you. */
function OverlordLink() {
  const o = store.snapshot.value?.overlord;
  const waits = o?.status === "waiting_input";
  return (
    <a
      class={`overlord-link${waits ? " attention" : ""}`}
      href="#/overlord"
      title={waits ? "the overlord waits for you" : "the overlord"}
    >
      overlord{waits ? " •" : ""}
    </a>
  );
}

/** What waits in the outbox (a described new session): one line each, with Cancel. */
function Waiting({ cancel }: { cancel: (id: string) => void }) {
  const items = phone.outbox.value;
  if (items.length === 0) return null;
  return (
    <ul class="waiting">
      {items.map((i) => (
        <li key={i.id} class={`waiting-line${i.state === "failed" ? " failed" : ""}`}>
          <span class="waiting-text">{waitingWords(i)}</span>
          <button type="button" class="dim" onClick={() => cancel(i.id)}>
            {i.state === "failed" || i.state === "interrupted" ? "Dismiss" : "Cancel"}
          </button>
        </li>
      ))}
    </ul>
  );
}

function App({ t }: { t: RemoteTransport }) {
  const send = (m: Parameters<RemoteTransport["send"]>[0]) => t.send(m);
  const extras: PhoneExtras = {
    paste: () => t.paste(),
    copy: (text) => t.copy(text),
    debug: debugBuild(phone.settings.value),
  };
  const sc = screen(phone.conn.value, phone.pairing.value);
  if (sc === "starting") {
    return (
      <div class="app">
        <p class="hint">starting…</p>
      </div>
    );
  }
  if (sc === "pair") {
    return (
      <div class="app">
        <Pairing phone={phone} send={send} />
      </div>
    );
  }
  const r = route.value;
  const status = statusOpen.value;
  const snap = store.snapshot.value;
  const cancel = (id: string) => send({ app: "cancel", request: id });
  return (
    <div class={`app at-${status ? "status" : r.kind}`}>
      <header class="bar">
        <a class="brand" href="#/">
          rlord
        </a>
        <ConnLine phone={phone} />
        {store.error.value && store.conn.value === "open" && (
          <span class="err">{store.error.value}</span>
        )}
        <OverlordLink />
        <a class="menu-link" href="#/status" aria-label="this phone">
          ⋯
        </a>
      </header>
      <FormHost store={store} manualInside />
      <Toast store={store} />
      <div class="body">
        <aside class="side">
          {!snap && (
            <a class="row overlord" href="#/overlord">
              <span class="name">overlord</span>
              <span class="dim">waiting for your computer</span>
            </a>
          )}
          <List
            store={store}
            route={r}
            caps={t.caps}
            oneNew
            waiting={<Waiting cancel={cancel} />}
          />
        </aside>
        <main class="main">
          {status ? (
            <StatusScreen phone={phone} send={send} />
          ) : r.kind === "list" ? (
            <p class="hint">Pick the overlord or a session.</p>
          ) : r.kind === "overlord" ? (
            snap ? (
              <Overlord
                store={store}
                body={(o) => <OverlordBody store={store} t={t} o={o} extras={extras} />}
              />
            ) : (
              <section class="view">
                <header class="view-head">
                  <Back />
                  <h1>overlord</h1>
                </header>
                <p class="hint">Waiting for your computer.</p>
              </section>
            )
          ) : (
            <Session
              key={r.id}
              store={store}
              id={r.id}
              compact
              body={(s, act, details) => (
                <SessionBody
                  store={store}
                  t={t}
                  s={s}
                  act={act}
                  details={details}
                  extras={extras}
                />
              )}
            />
          )}
        </main>
      </div>
    </div>
  );
}

const root = document.getElementById("app");
if (root) {
  render(
    transport ? <App t={transport} /> : <p class="hint">This page runs inside the rlord app.</p>,
    root,
  );
}
