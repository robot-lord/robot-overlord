// Carrying out the overlord's UI commands on a page (`ui.ts` decides
// what each does): shared by the local page and the phone's.

import type { UiCmd } from "./proto";
import { href } from "./route";
import type { Store } from "./store";
import type { Caps } from "./transport";
import { decide } from "./ui";

/** Do what `cmd` asks; `go` moves to a hash route (in history, so back returns). */
export function runUi(cmd: UiCmd, store: Store, caps: Caps, go: (hash: string) => void): void {
  for (const e of decide(cmd, store.form.value, caps)) {
    switch (e.do) {
      case "route":
        if (e.closeForm) store.form.value = null;
        go(href(e.to));
        break;
      case "view":
        store.applyView(e.view, e.select);
        break;
      case "open":
        store.parked.value = null;
        store.form.value = e.form;
        break;
      case "park":
        store.parked.value = e.cmd;
        break;
      case "toast":
        store.toast.value = e.text;
        break;
    }
  }
}
