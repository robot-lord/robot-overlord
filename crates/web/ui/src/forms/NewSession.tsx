// The new-session forms, as in the TUI's list (`n` and `N`):
//
// - Describe: the user says what they want; the hints below (repo, agent,
//   model, worktree, base, tags) are optional, each shown with whose it
//   is. Sent to the overlord, which may reopen the form with its own
//   hints marked (`confirm_new_session`); sending that back confirms them.
// - Manual: repo first, then the rest; launched at once. A session the
//   overlord proposed shows its background, read-only.

import { useEffect, useState } from "preact/hooks";
import { Modal } from "../components/Modal";
import { type Choice, Picker } from "../components/Picker";
import type { FormDefaults, FormRepos, Row, UiCmd } from "../proto";
import { href } from "../route";
import type { Store } from "../store";
import {
  agentBlocker,
  described,
  fresh,
  type Hint,
  launch,
  type Location,
  modelsOf,
  type NewForm,
  owner,
  picked,
  prefill,
  prefillConfirm,
  request,
  setAgent,
  setLocation,
} from "./hints";

/** The repo finder's choices, and choices for a typed path. */
export function repoChoices(repos: FormRepos | null): Choice[] {
  return (repos?.repos ?? []).map((r) => ({
    value: r.path,
    label: r.path,
    ...(r.default ? { note: "★ default" } : {}),
  }));
}

function locLabel(l: Location): string {
  if (l.kind === "repo") return l.path;
  if (l.kind === "folder") return `${l.path} (plain folder, no repo)`;
  return "the overlord picks";
}

function typedPath(text: string): Choice[] {
  const t = text.trim();
  if (!t.startsWith("/") && !t.startsWith("~")) return [];
  return [
    { value: `repo:${t}`, label: t, note: "(typed: a repo)" },
    { value: `folder:${t}`, label: t, note: "(typed: a plain folder, no repo)" },
  ];
}

export function NewSession({
  store,
  kind,
  cmd,
  manualInside = false,
}: {
  store: Store;
  kind: NewForm["kind"];
  cmd: UiCmd | null;
  /** The describe form offers the manual one (a page with one New button). */
  manualInside?: boolean;
}) {
  const [defaults, setDefaults] = useState<FormDefaults | null>(null);
  const [repos, setRepos] = useState<FormRepos | null>(null);
  const [f, setF] = useState<NewForm | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [finding, setFinding] = useState(false);
  const close = () => {
    store.form.value = null;
  };
  // The defaults and the repos, then the form (filled from the command).
  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const [d, r] = await Promise.all([
          store.request<FormDefaults>("form.defaults"),
          store.request<FormRepos>("form.repos"),
        ]);
        if (!live) return;
        setDefaults(d);
        setRepos(r);
        const group =
          store.view.value.tab === "groups" && store.group.value ? store.group.value : null;
        let form = fresh(kind, d, r, kind === "describe" && !cmd ? group : null);
        if (cmd?.cmd === "new_session") form = prefill(form, cmd.prefill, d.agents);
        if (cmd?.cmd === "confirm_new_session") form = prefillConfirm(form, cmd.prefill, d.agents);
        setF(form);
      } catch (e) {
        if (live) setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      live = false;
    };
  }, [kind, cmd]);
  // The finder's first disk scan: ask again until it is done.
  useEffect(() => {
    if (!repos?.scanning) return;
    const id = setTimeout(() => {
      store
        .request<FormRepos>("form.repos")
        .then((r) => setRepos(r))
        .catch(() => {});
    }, 1000);
    return () => clearTimeout(id);
  }, [repos]);

  const title =
    kind === "manual"
      ? f?.proposal
        ? "The overlord's proposal"
        : "New session (manual)"
      : f?.reopened
        ? "Your new session, as the overlord would launch it"
        : "New session";
  if (!f || !defaults) {
    return (
      <Modal title={title} onClose={close}>
        <p class="hint">{error ?? "loading…"}</p>
      </Modal>
    );
  }
  const agents = defaults.agents;
  /** A change the user made: the form is theirs now (a command won't replace it). */
  const edit = (next: NewForm) => {
    setF(next);
    setError(null);
    const open = store.form.value;
    if (open && !open.touched) store.form.value = { ...open, touched: true };
  };
  const hint = (h: Hint, next: NewForm, changed: boolean) =>
    edit(f.kind === "describe" ? picked(next, h, changed) : next);
  const blocker = agentBlocker(agents, f.agent);
  const send = async () => {
    const s = f.kind === "describe" ? request(f) : launch(f);
    if ("error" in s) {
      setError(s.error);
      return;
    }
    if (f.kind === "describe" && !store.snapshot.value?.overlord) {
      setError("no overlord yet: use the manual form (or run rlord in a terminal to set one up)");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const r = await store.request<Row>(s.op, s.args);
      close();
      location.hash =
        f.kind === "describe" ? href({ kind: "overlord" }) : href({ kind: "session", id: r.id });
    } catch (e) {
      // The form stays, with what the user typed.
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const label = (h: Hint) =>
    f.kind === "describe" && (
      <span class={`owner owner-${owner(f, h).replace("'", "")}`}>
        {owner(f, h)}
        {f.changed.includes(h) ? " · changed" : ""}
      </span>
    );
  const models = modelsOf(agents, f.agent);
  const repoField = (
    <div class="field">
      <span class="field-name">
        {f.kind === "describe" ? "Repo" : "Repo or folder"} {label("repo")}
      </span>
      <div class="field-value">
        <button type="button" class="pick" onClick={() => setFinding(!finding)}>
          {locLabel(f.location)}
        </button>
        {f.kind === "describe" && f.location.kind !== "auto" && (
          <button
            type="button"
            class="dim link"
            onClick={() => hint("repo", setLocation(f, { kind: "auto" }), true)}
          >
            let the overlord pick
          </button>
        )}
        {finding && (
          <Picker
            items={repoChoices(repos)}
            placeholder="find a repo, or type a path"
            note={
              repos?.scanning
                ? "still scanning your code folder…"
                : repos && repos.skipped.length > 0
                  ? `skipped: ${repos.skipped.join(", ")}`
                  : null
            }
            typed={typedPath}
            onPick={(c) => {
              setFinding(false);
              const loc: Location = c.value.startsWith("folder:")
                ? { kind: "folder", path: c.value.slice(7) }
                : { kind: "repo", path: c.value.replace(/^repo:/, "") };
              const same = JSON.stringify(loc) === JSON.stringify(f.location);
              hint("repo", setLocation(f, loc), !same);
            }}
          />
        )}
      </div>
    </div>
  );
  const agentField = (
    <label class="field">
      <span class="field-name">Agent {label("agent")}</span>
      <select
        value={f.agent}
        onChange={(e) => {
          const v = e.currentTarget.value;
          hint("agent", setAgent(f, v, agents), v !== f.agent);
        }}
      >
        {agents.map((a) => (
          <option value={a.name}>
            {a.label}
            {a.installed ? "" : " (not installed)"}
          </option>
        ))}
      </select>
    </label>
  );
  const modelField = (
    <label class="field">
      <span class="field-name">Model {label("model")}</span>
      <select
        value={f.model}
        onChange={(e) => {
          const v = e.currentTarget.value;
          hint("model", { ...f, model: v }, v !== f.model);
        }}
      >
        <option value="">agent default</option>
        {[...new Set([...(f.model ? [f.model] : []), ...models])].map((m) => (
          <option value={m}>{m}</option>
        ))}
      </select>
    </label>
  );
  const worktreeField = (
    <label class="field">
      <span class="field-name">Worktree {label("worktree")}</span>
      <span class="field-value">
        <input
          type="checkbox"
          checked={f.workspace}
          disabled={f.location.kind === "folder"}
          onChange={(e) => {
            const v = e.currentTarget.checked;
            hint("worktree", { ...f, workspace: v }, v !== f.workspace);
          }}
        />
        <span class="dim small">
          {f.location.kind === "folder" ? "a plain folder has none" : "its own git worktree"}
        </span>
      </span>
    </label>
  );
  const baseField = (
    <label class="field">
      <span class="field-name">Base {label("base")}</span>
      <input
        type="text"
        value={f.base}
        placeholder={`${f.defaultBase} (default)`}
        disabled={!f.workspace || f.location.kind === "folder"}
        onInput={(e) => {
          const v = e.currentTarget.value;
          // A side chosen for the old base isn't for the new one.
          hint("base", { ...f, base: v, prefer: "" }, v !== f.base);
        }}
      />
    </label>
  );
  return (
    <Modal title={title} onClose={close} busy={busy}>
      {f.note && <p class="note">{f.note}</p>}
      <form
        class="form"
        onSubmit={(e) => {
          e.preventDefault();
          void send();
        }}
      >
        {f.kind === "describe" ? (
          <>
            <label class="field wide">
              <span class="field-name">What should it do?</span>
              <textarea
                rows={4}
                value={f.prompt}
                placeholder="Describe the session in your own words"
                onInput={(e) => edit(described({ ...f, prompt: e.currentTarget.value }))}
              />
            </label>
            <p class="dim small hints-note">
              Optional hints: leave them as they are and the overlord works them out from your
              description. Your description wins where they disagree.
            </p>
            {manualInside && !cmd && (
              <button
                type="button"
                class="dim link"
                onClick={() => {
                  store.form.value = { kind: "manual", cmd: null, touched: false };
                }}
              >
                Fill in every field yourself instead (no overlord)
              </button>
            )}
            <div class="hints">
              {repoField}
              {agentField}
              {modelField}
              {worktreeField}
              {baseField}
              <label class="field">
                <span class="field-name">Tags {label("tags")}</span>
                <input
                  type="text"
                  value={f.tags}
                  placeholder="none"
                  onInput={(e) => {
                    const v = e.currentTarget.value;
                    hint("tags", { ...f, tags: v }, v !== f.tags);
                  }}
                />
              </label>
            </div>
          </>
        ) : (
          <>
            {repoField}
            <label class="field">
              <span class="field-name">Name</span>
              <input
                type="text"
                value={f.name}
                placeholder="(from the prompt)"
                onInput={(e) => edit({ ...f, name: e.currentTarget.value })}
              />
            </label>
            {agentField}
            {modelField}
            {worktreeField}
            <label class="field">
              <span class="field-name">Branch</span>
              <input
                type="text"
                value={f.branch}
                placeholder="(from the name)"
                disabled={!f.workspace || f.location.kind === "folder"}
                onInput={(e) => edit({ ...f, branch: e.currentTarget.value })}
              />
            </label>
            {baseField}
            <label class="field wide">
              <span class="field-name">First prompt</span>
              <textarea
                rows={4}
                value={f.prompt}
                placeholder="(optional: it opens idle at its input)"
                onInput={(e) => edit({ ...f, prompt: e.currentTarget.value })}
              />
            </label>
            {(f.context || f.relatedNames.length > 0) && (
              <div class="background">
                <span class="field-name">The overlord's notes for it</span>
                {f.context && <pre class="readonly">{f.context}</pre>}
                {f.relatedNames.length > 0 && (
                  <p class="dim small">related: {f.relatedNames.join(", ")}</p>
                )}
              </div>
            )}
          </>
        )}
        {blocker && (
          <div class="blocker">
            <p>{blocker.why}</p>
            {blocker.command && <code>{blocker.command}</code>}
          </div>
        )}
        {error && <p class="err">{error}</p>}
        <div class="buttons">
          <button type="submit" class="primary" disabled={busy || blocker !== null}>
            {busy
              ? "sending…"
              : f.kind === "manual"
                ? "Launch"
                : f.confirmed
                  ? "Confirm"
                  : "Send to the overlord"}
          </button>
          <button type="button" onClick={close} disabled={busy}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}
