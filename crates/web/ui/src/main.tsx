// The page: mount and connect. (Views come with the list and pane steps.)
import { signal } from "@preact/signals";
import { render } from "preact";
import { type ConnState, Control, takeToken, wsUrl } from "./api";
import "./app.css";

const state = signal<ConnState | "no_token">("connecting");
const version = signal("");
let build: string | null = null;

const token = takeToken(location, safeStorage(), (u) => history.replaceState(null, "", u));
if (token) {
  const control = new Control(wsUrl("/ws"), token, {
    state: (s) => {
      state.value = s;
    },
    hello: (h) => {
      // A page from another build reloads (the plugin was updated).
      if (build !== null && build !== h.build) location.reload();
      build = h.build;
      version.value = h.version;
    },
    changed: () => {},
  });
  window.addEventListener("pagehide", () => control.stop());
} else {
  state.value = "no_token";
}

function safeStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

const words: Record<ConnState | "no_token", string> = {
  connecting: "connecting…",
  open: "connected",
  closed: "disconnected, retrying…",
  unauthorized: "this link has expired: run `rlord web` again",
  no_token: "no token: open the link `rlord web` prints",
};

function App() {
  return (
    <header class="bar">
      <b class="brand">rlord</b>
      <span class={`conn conn-${state.value}`}>{words[state.value]}</span>
      {version.value && <span class="dim">{version.value}</span>}
    </header>
  );
}

const root = document.getElementById("app");
if (root) render(<App />, root);
