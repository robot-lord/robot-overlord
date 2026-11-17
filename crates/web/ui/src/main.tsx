// The page: the token, the control socket, the store, and the routes.
// Wide screens show the list beside the pane; narrow ones one at a time.

import { render } from "preact";
import { Control, takeToken, wsUrl } from "./api";
import { tick } from "./components/time";
import { followHash, parse, route } from "./route";
import { Store } from "./store";
import { List } from "./views/List";
import { Overlord } from "./views/Overlord";
import { Session } from "./views/Session";
import "./app.css";

function safeStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

const storage = safeStorage();
const store = new Store(storage);
let build: string | null = null;
let version = "";

const token = takeToken(location, storage, (u) => history.replaceState(null, "", u));
// (After the token: the fragment was `#t=…` until then.)
route.value = parse(location.hash);
followHash();
tick();

const control = token
  ? new Control(wsUrl("/ws"), token, {
      state: (s) => {
        store.conn.value = s;
      },
      hello: (h) => {
        // A page from another build reloads (the plugin was updated).
        if (build !== null && build !== h.build) location.reload();
        build = h.build;
        version = h.version;
        if (control) store.connected(control);
      },
      changed: (rev) => store.changed(rev),
    })
  : null;
if (!token) store.conn.value = "no_token";

const words: Record<string, string> = {
  connecting: "connecting…",
  open: "connected",
  closed: "disconnected, retrying…",
  unauthorized: "this link has expired: run `rlord web` again",
  no_token: "no token: open the link `rlord web` prints",
};

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
      </header>
      <div class="body">
        <aside class="side">
          <List store={store} route={r} />
        </aside>
        <main class="main">
          {r.kind === "list" || !token ? (
            <p class="hint">Pick the overlord or a session.</p>
          ) : r.kind === "overlord" ? (
            <Overlord store={store} token={token} />
          ) : (
            <Session key={r.id} store={store} id={r.id} token={token} />
          )}
        </main>
      </div>
    </div>
  );
}

const root = document.getElementById("app");
if (root) render(<App />, root);
