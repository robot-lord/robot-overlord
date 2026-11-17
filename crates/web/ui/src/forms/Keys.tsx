// The key screen, read-only: every key rlord binds and what stands in its
// way. Keys change only in a terminal (rlord tests there that a key
// reaches it), so this only shows them.

import { useEffect, useState } from "preact/hooks";
import { Modal } from "../components/Modal";
import type { KeyRow } from "../proto";
import type { Store } from "../store";

export function Keys({ store, row }: { store: Store; row: string }) {
  const [keys, setKeys] = useState<KeyRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    store
      .request<{ keys: KeyRow[] }>("keys.list")
      .then((r) => setKeys(r.keys))
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  }, []);
  useEffect(() => {
    if (row) document.getElementById(`key-${row}`)?.scrollIntoView({ block: "center" });
  }, [keys, row]);
  return (
    <Modal title="Keys" onClose={() => (store.form.value = null)}>
      <p class="note">
        Read-only here. To change a key, run rlord in a terminal and open the key screen there
        (:keys): it checks that the new key reaches rlord.
      </p>
      {!keys && <p class="hint">{error ?? "loading…"}</p>}
      {keys && (
        <table class="keys">
          <tbody>
            {keys.map((k) => (
              <tr id={`key-${k.id}`} class={k.id === row ? "on" : ""}>
                <td>
                  <kbd>{k.key}</kbd>
                </td>
                <td>
                  {k.does}
                  <span class="dim small"> · {k.where}</span>
                  {k.findings.map((f) => (
                    <div class={`small finding-${f.level}`}>{f.text}</div>
                  ))}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Modal>
  );
}
