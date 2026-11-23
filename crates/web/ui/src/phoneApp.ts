// The phone page's wiring, apart from drawing (app.tsx) so tests drive it
// by events: the bridge's transport feeds the store (the same as the
// local page's control socket does) and the phone's own state, and the
// overlord's UI commands run with a device's caps.

import { runUi } from "./commands";
import type { Phone } from "./phone";
import type { UiCmd } from "./proto";
import { type Bridge, RemoteTransport } from "./remote";
import type { Store } from "./store";

export interface Wiring {
  store: Store;
  phone: Phone;
  bridge: Bridge;
  /** Move to a hash route (in history). */
  go: (hash: string) => void;
  /** After each hello, once the store has the transport (report what is shown). */
  onHello?: () => void;
  firstId?: number;
}

export function wirePhone(w: Wiring): RemoteTransport {
  const { store, phone } = w;
  const ui = (cmd: UiCmd) => runUi(cmd, store, t.caps, w.go);
  const t: RemoteTransport = new RemoteTransport(
    w.bridge,
    {
      state: (s) => {
        store.conn.value = s;
      },
      hello: (h) => {
        phone.hello.value = h;
        store.connected(t);
        w.onHello?.();
        // A command meant for this phone while it was away.
        t.request<UiCmd | null>("page.pending")
          .then((c) => c && ui(c))
          .catch(() => {});
      },
      changed: (rev) => store.changed(rev),
      ui: (cmd) => {
        ui(cmd);
        // "Got it": the daemon won't hand it to another screen.
        t.request("page.report", {}).catch(() => {});
      },
      app: (ev) => phone.apply(ev),
    },
    w.firstId,
  );
  return t;
}
