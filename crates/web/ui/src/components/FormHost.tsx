// The form open over the page (store.form), and the banner for one the
// overlord opened while the user was typing in another.

import { Fork } from "../forms/Fork";
import { Keys } from "../forms/Keys";
import { NewSession } from "../forms/NewSession";
import { CodeFolder } from "../forms/CodeFolder";
import type { Store } from "../store";
import { formOf } from "../ui";

const NAMES: Record<string, string> = {
  describe: "new session",
  confirm: "your new session",
  manual: "a proposed session",
  fork: "fork",
  setup: "code folder",
  keys: "keys",
};

export function FormHost({ store }: { store: Store }) {
  const form = store.form.value;
  const parked = store.parked.value;
  return (
    <>
      {parked && (
        <div class="banner" role="status">
          <span>
            The overlord opened {NAMES[formOf(parked) ?? ""] ?? "a form"}: finish this one first, or
          </span>
          <button
            type="button"
            onClick={() => {
              const kind = formOf(parked);
              store.parked.value = null;
              if (kind) store.form.value = { kind, cmd: parked, touched: false };
            }}
          >
            open it now
          </button>
          <button type="button" class="dim" onClick={() => (store.parked.value = null)}>
            dismiss
          </button>
        </div>
      )}
      {form && (form.kind === "describe" || form.kind === "confirm") && (
        <NewSession store={store} kind="describe" cmd={form.cmd} />
      )}
      {form?.kind === "manual" && <NewSession store={store} kind="manual" cmd={form.cmd} />}
      {form?.kind === "setup" && <CodeFolder store={store} cmd={form.cmd} />}
      {form?.kind === "keys" && (
        <Keys store={store} row={form.cmd?.cmd === "keys" ? form.cmd.row : ""} />
      )}
      {form?.kind === "fork" && form.session && <Fork store={store} s={form.session} />}
    </>
  );
}
