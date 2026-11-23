// The local page's own forms: the code folder, the key screen and remote
// access (devices). Their ops are local-only; only main.tsx imports this.

import { CodeFolder } from "../forms/CodeFolder";
import { Keys } from "../forms/Keys";
import type { Store } from "../store";
import type { OpenForm } from "../ui";
import { Devices } from "../views/Devices";

export function LocalForms({ store, form }: { store: Store; form: OpenForm }) {
  switch (form.kind) {
    case "setup":
      return <CodeFolder store={store} cmd={form.cmd} />;
    case "keys":
      return <Keys store={store} row={form.cmd?.cmd === "keys" ? form.cmd.row : ""} />;
    case "devices":
      return <Devices store={store} />;
    case "describe":
    case "confirm":
    case "manual":
    case "fork":
      return null;
  }
}
