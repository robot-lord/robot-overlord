// The form open over the page (store.form), and the banner for one the
// overlord opened while the user was typing in another.

import type { Store } from "../store";
import { formOf } from "../ui";
import { Modal } from "./Modal";

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
  const close = () => {
    store.form.value = null;
  };
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
      {form && (
        <Modal title={NAMES[form.kind] ?? form.kind} onClose={close}>
          <p class="hint">This form isn't in the web UI yet.</p>
        </Modal>
      )}
    </>
  );
}
