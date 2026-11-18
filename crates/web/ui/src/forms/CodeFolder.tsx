// The code-folder form (`:setup` in the TUI): the folder the user keeps
// repos in (the repo finder scans it; new repos go there) and what else
// the finder scans. An optional description goes to the overlord, which
// reopens the form with its proposal (`confirm_setup`); otherwise rlord
// saves it, asking first before creating a folder or using the home
// folder.

import { useEffect, useState } from "preact/hooks";
import { Modal } from "../components/Modal";
import type { FormSetup, UiCmd } from "../proto";
import { href } from "../route";
import type { Store } from "../store";
import {
  askFor,
  described,
  describing,
  fromSetup,
  prefill,
  rootsLabel,
  rootsOptions,
  type SetupForm,
  saveCall,
  setFolder,
} from "./setup";

export function CodeFolder({ store, cmd }: { store: Store; cmd: UiCmd | null }) {
  const [f, setF] = useState<SetupForm | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ask, setAsk] = useState<"create" | "home" | null>(null);
  const [busy, setBusy] = useState(false);
  const close = () => {
    store.form.value = null;
  };
  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const a = await store.request<FormSetup>("form.setup");
        if (!live) return;
        let form = fromSetup(a, Boolean(store.snapshot.value?.overlord));
        if (cmd?.cmd === "setup") form = prefill(form, cmd.prefill);
        setF(form);
      } catch (e) {
        if (live) setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      live = false;
    };
  }, [cmd]);
  const title = f?.proposal ? "Code folder, as the overlord would save it" : "Code folder";
  if (!f) {
    return (
      <Modal title={title} onClose={close}>
        <p class="hint">{error ?? "finding where your projects are…"}</p>
      </Modal>
    );
  }
  const edit = (next: SetupForm) => {
    setF(next);
    setError(null);
    setAsk(null);
    const open = store.form.value;
    if (open && !open.touched) store.form.value = { ...open, touched: true };
  };
  const save = async (form: SetupForm) => {
    const c = saveCall(form);
    if ("error" in c) {
      setError(c.error);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await store.request(c.op, c.args);
      close();
      if (c.op === "overlord.setup_request") location.hash = href({ kind: "overlord" });
      else store.toast.value = `code folder saved: ${form.codeDir.trim()}`;
    } catch (e) {
      const m = e instanceof Error ? e.message : String(e);
      setError(m);
      setAsk(askFor(m));
    } finally {
      setBusy(false);
    }
  };
  const options = rootsOptions(f);
  const dir = f.codeDir.trim();
  return (
    <Modal title={title} onClose={close} busy={busy}>
      {f.note && <p class="note">{f.note}</p>}
      <form
        class="form"
        onSubmit={(e) => {
          e.preventDefault();
          void save(f);
        }}
      >
        {f.describe !== null && (
          <label class="field wide">
            <span class="field-name">Where is your code? (optional, for the overlord)</span>
            <textarea
              rows={2}
              value={f.describe}
              placeholder="e.g. my repos are in ~/src, also scan ~/work"
              onInput={(e) => edit(described(f, e.currentTarget.value))}
            />
          </label>
        )}
        <label class="field">
          <span class="field-name">
            Code folder
            {f.changed.includes("code_dir") && <span class="owner owner-overlords">changed</span>}
          </span>
          <input
            type="text"
            value={f.codeDir}
            placeholder="the folder you keep your repos in"
            onInput={(e) => edit(setFolder(f, e.currentTarget.value))}
          />
        </label>
        {f.choices.length > 0 && (
          <div class="field">
            <span class="field-name dim small">rlord suggests</span>
            <span class="field-value">
              {f.choices.map((c) => (
                <button
                  type="button"
                  class={c === dir ? "on" : ""}
                  onClick={() => edit(setFolder(f, c))}
                >
                  {c}
                </button>
              ))}
            </span>
          </div>
        )}
        {f.current && <p class="dim small hints-note">now: {f.current}</p>}
        <label class="field">
          <span class="field-name">
            The repo finder scans
            {f.changed.includes("repo_roots") && <span class="owner owner-overlords">changed</span>}
          </span>
          <select
            value={JSON.stringify(f.roots)}
            onChange={(e) => {
              const v = options.find((o) => JSON.stringify(o) === e.currentTarget.value);
              if (v) edit({ ...f, roots: v, rootsFromDefault: false });
            }}
          >
            {options.map((o) => (
              <option value={JSON.stringify(o)}>{rootsLabel(o)}</option>
            ))}
          </select>
        </label>
        {error && <p class="err">{error}</p>}
        {ask && (
          <div class="buttons">
            <button
              type="button"
              class="primary"
              onClick={() => {
                const next =
                  ask === "create" ? { ...f, confirmCreate: dir } : { ...f, confirmHome: dir };
                setF(next);
                setAsk(null);
                void save(next);
              }}
            >
              {ask === "create" ? `Create ${dir}` : "Use the home folder anyway"}
            </button>
          </div>
        )}
        <div class="buttons">
          <button type="submit" class="primary" disabled={busy}>
            {busy ? "saving…" : describing(f) ? "Ask the overlord" : "Save"}
          </button>
          <button type="button" onClick={close} disabled={busy}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}
