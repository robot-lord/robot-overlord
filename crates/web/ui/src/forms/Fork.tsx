// The fork form (`f` in the TUI's list): a new session that goes on from
// a copy of this one's conversation. What it should do first (optional),
// its name, and a new worktree when the agent and the folder allow one.

import { useState } from "preact/hooks";
import { Modal } from "../components/Modal";
import type { Row, SessionBase } from "../proto";
import { href } from "../route";
import type { Store } from "../store";

/** A new worktree is possible: an agent that can fork elsewhere, in a git repo. */
export function canWorktree(s: Pick<SessionBase, "agent" | "repo_path">): boolean {
  return (s.agent === "claude" || s.agent === "codex") && Boolean(s.repo_path);
}

/** `session.fork`'s arguments, as the form stands. */
export function forkArgs(
  s: Pick<SessionBase, "id" | "agent" | "repo_path">,
  prompt: string,
  name: string,
  worktree: boolean,
): Record<string, unknown> {
  const a: Record<string, unknown> = { ref: s.id };
  if (prompt.trim()) a.prompt = prompt.trim();
  if (name.trim()) a.name = name.trim();
  if (worktree && canWorktree(s)) a.workspace = true;
  return a;
}

export function Fork({ store, s }: { store: Store; s: SessionBase }) {
  const [prompt, setPrompt] = useState("");
  const [name, setName] = useState("");
  const [worktree, setWorktree] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const close = () => {
    store.form.value = null;
  };
  const touch = () => {
    const open = store.form.value;
    if (open && !open.touched) store.form.value = { ...open, touched: true };
  };
  const send = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await store.request<Row & { warnings?: string[] }>(
        "session.fork",
        forkArgs(s, prompt, name, worktree),
      );
      close();
      if (r.warnings && r.warnings.length > 0) store.toast.value = r.warnings.join(" · ");
      location.hash = href({ kind: "session", id: r.id });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal title={`Fork ${s.name}`} onClose={close} busy={busy}>
      <form
        class="form"
        onSubmit={(e) => {
          e.preventDefault();
          void send();
        }}
      >
        <label class="field wide">
          <span class="field-name">What should the fork do first?</span>
          <textarea
            rows={3}
            value={prompt}
            placeholder="(optional: it opens idle at its input)"
            onInput={(e) => {
              setPrompt(e.currentTarget.value);
              touch();
            }}
          />
        </label>
        <label class="field">
          <span class="field-name">Name</span>
          <input
            type="text"
            value={name}
            placeholder={`${s.name}-fork, or the next free one`}
            onInput={(e) => {
              setName(e.currentTarget.value);
              touch();
            }}
          />
        </label>
        <label class="field">
          <span class="field-name">Where</span>
          <span class="field-value">
            {canWorktree(s) ? (
              <>
                <input
                  type="checkbox"
                  checked={worktree}
                  onChange={(e) => {
                    setWorktree(e.currentTarget.checked);
                    touch();
                  }}
                />
                <span class="small">
                  {worktree
                    ? "a new worktree from its last commit (uncommitted changes stay behind)"
                    : `its folder, ${s.project_path}`}
                </span>
              </>
            ) : (
              <span class="small">its folder, {s.project_path}</span>
            )}
          </span>
        </label>
        {error && <p class="err">{error}</p>}
        <div class="buttons">
          <button type="submit" class="primary" disabled={busy}>
            {busy ? "forking…" : "Fork"}
          </button>
          <button type="button" onClick={close} disabled={busy}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}
