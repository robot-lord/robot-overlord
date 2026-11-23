// The form open over the page (store.form), and the banner for one the
// overlord opened while the user was typing in another. The forms every
// page has are here; the local page's own (code folder, keys, devices)
// come from its entry (`local`), so the phone's bundle doesn't carry them.

import type { ComponentChildren } from "preact";
import { Fork } from "../forms/Fork";
import { NewSession } from "../forms/NewSession";
import type { Store } from "../store";
import { formOf, type OpenForm } from "../ui";

const NAMES: Record<string, string> = {
  describe: "new session",
  confirm: "your new session",
  manual: "a proposed session",
  fork: "fork",
  setup: "code folder",
  keys: "keys",
  devices: "devices",
};

export function FormHost({
  store,
  local,
  manualInside = false,
}: {
  store: Store;
  /** The local page's own forms. */
  local?: (form: OpenForm) => ComponentChildren;
  /** The describe form offers the manual one (a page with one New button). */
  manualInside?: boolean;
}) {
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
        <NewSession store={store} kind="describe" cmd={form.cmd} manualInside={manualInside} />
      )}
      {form?.kind === "manual" && <NewSession store={store} kind="manual" cmd={form.cmd} />}
      {form?.kind === "fork" && form.session && <Fork store={store} s={form.session} />}
      {form && local?.(form)}
    </>
  );
}
