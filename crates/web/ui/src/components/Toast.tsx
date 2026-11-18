// A one-line notice: the overlord did something on another screen.

import { useEffect } from "preact/hooks";
import type { Store } from "../store";

/** How long a notice stays. */
export const TOAST_MS = 6000;

export function Toast({ store }: { store: Store }) {
  const text = store.toast.value;
  useEffect(() => {
    if (!text) return;
    const id = setTimeout(() => {
      if (store.toast.value === text) store.toast.value = null;
    }, TOAST_MS);
    return () => clearTimeout(id);
  }, [text]);
  if (!text) return null;
  return (
    <div class="toast" role="status">
      <span>{text}</span>
      <button type="button" aria-label="dismiss" onClick={() => (store.toast.value = null)}>
        ×
      </button>
    </div>
  );
}
