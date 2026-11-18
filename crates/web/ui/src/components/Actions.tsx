// The buttons that change things: a session's (resume, restart, stop,
// archive, pin, edit), the overlord's, and the "tell the overlord" box.
// Each sends ops on the daemon's allow-list; the list redraws from the
// `changed` that follows.

import type { ComponentChildren } from "preact";
import { useState } from "preact/hooks";
import { type Edit, editOf, editRequests, isRunning, type Request } from "../actions";
import type { Overlord, SessionBase } from "../proto";
import type { Store } from "../store";

export interface Runner {
  /** What is being done ("stopping…"), while a request is out. */
  busy: string | null;
  /** The last failure, for the user. */
  error: string | null;
  /** Send `reqs` in order; stop at the first failure. True when all went. */
  run: (label: string, reqs: Request[]) => Promise<boolean>;
}

/** Send requests one at a time, saying what is going on. */
export function useRunner(store: Store, done?: () => void): Runner {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const run = async (label: string, reqs: Request[]): Promise<boolean> => {
    setBusy(label);
    setError(null);
    try {
      for (const r of reqs) await store.request(r.op, r.args);
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setBusy(null);
      done?.();
    }
  };
  return { busy, error, run };
}

/** A button that asks once more before doing something hard to undo. */
export function ConfirmButton({
  label,
  ask,
  disabled,
  onConfirm,
}: {
  label: string;
  ask: string;
  disabled?: boolean;
  onConfirm: () => void;
}) {
  const [asking, setAsking] = useState(false);
  if (!asking) {
    return (
      <button type="button" disabled={disabled} onClick={() => setAsking(true)}>
        {label}
      </button>
    );
  }
  return (
    <span class="confirm">
      <span>{ask}</span>
      <button
        type="button"
        class="danger"
        onClick={() => {
          setAsking(false);
          onConfirm();
        }}
      >
        Yes
      </button>
      <button type="button" onClick={() => setAsking(false)}>
        No
      </button>
    </span>
  );
}

/** Busy or failed, under a row of buttons. */
export function RunnerNote({ r }: { r: Runner }) {
  if (r.busy) return <span class="dim">{r.busy}</span>;
  if (r.error) return <span class="err">{r.error}</span>;
  return null;
}

/** Resume a stopped session (or unarchive an archived one). */
export function ResumeButton({ s, r }: { s: SessionBase; r: Runner }) {
  if (s.archived_at) {
    return (
      <button
        type="button"
        disabled={r.busy !== null}
        onClick={() =>
          void r.run("unarchiving…", [{ op: "session.unarchive", args: { ref: s.id } }])
        }
      >
        Unarchive
      </button>
    );
  }
  return (
    <button
      type="button"
      disabled={r.busy !== null}
      onClick={() => void r.run("resuming…", [{ op: "session.resume", args: { ref: s.id } }])}
    >
      Resume
    </button>
  );
}

/** A session's buttons, and its edit form when open. */
export function SessionActions({
  s,
  r,
  onFork,
}: {
  s: SessionBase;
  r: Runner;
  onFork?: () => void;
}) {
  const [editing, setEditing] = useState(false);
  const ref = s.id;
  const busy = r.busy !== null;
  const one = (label: string, op: Request["op"], args: Record<string, unknown> = {}) =>
    void r.run(label, [{ op, args: { ref, ...args } }]);
  let buttons: ComponentChildren;
  if (s.archived_at) {
    buttons = <ResumeButton s={s} r={r} />;
  } else {
    buttons = (
      <>
        {isRunning(s) ? (
          <>
            <ConfirmButton
              label="Restart"
              ask="Restart its agent?"
              disabled={busy}
              onConfirm={() => one("restarting…", "session.restart")}
            />
            <ConfirmButton
              label="Stop"
              ask="Stop it?"
              disabled={busy}
              onConfirm={() => one("stopping…", "session.stop")}
            />
          </>
        ) : (
          <ResumeButton s={s} r={r} />
        )}
        <ConfirmButton
          label="Archive"
          ask="Archive it (its agent stops)?"
          disabled={busy}
          onConfirm={() => one("archiving…", "session.archive")}
        />
        <button
          type="button"
          disabled={busy}
          aria-pressed={s.pinned}
          onClick={() =>
            one(s.pinned ? "unpinning…" : "pinning…", "session.pin", { pinned: !s.pinned })
          }
        >
          {s.pinned ? "Unpin" : "Pin"}
        </button>
        <button type="button" aria-expanded={editing} onClick={() => setEditing(!editing)}>
          Edit
        </button>
        {onFork && (
          <button
            type="button"
            disabled={busy}
            title="a new session from a copy of its conversation (f)"
            onClick={onFork}
          >
            Fork
          </button>
        )}
      </>
    );
  }
  return (
    <>
      <div class="actions">
        {buttons}
        <RunnerNote r={r} />
      </div>
      {editing && !s.archived_at && (
        <EditForm
          key={s.id}
          s={s}
          busy={busy}
          onCancel={() => setEditing(false)}
          onSave={(e) => {
            const reqs = editRequests(s, e);
            if (reqs.length === 0) {
              setEditing(false);
              return;
            }
            void (async () => {
              if (await r.run("saving…", reqs)) setEditing(false);
            })();
          }}
        />
      )}
    </>
  );
}

/** Name, description, tags and notes. */
function EditForm({
  s,
  busy,
  onSave,
  onCancel,
}: {
  s: SessionBase;
  busy: boolean;
  onSave: (e: Edit) => void;
  onCancel: () => void;
}) {
  // (From the session as it was when the form opened.)
  const [e, setE] = useState<Edit>(() => editOf(s));
  const field = (k: keyof Edit) => ({
    value: e[k],
    onInput: (ev: { currentTarget: HTMLInputElement | HTMLTextAreaElement }) =>
      setE({ ...e, [k]: ev.currentTarget.value }),
  });
  return (
    <form
      class="edit"
      onSubmit={(ev) => {
        ev.preventDefault();
        onSave(e);
      }}
    >
      <label>
        <span>name</span>
        <input type="text" required {...field("name")} />
      </label>
      <label>
        <span>description</span>
        <input type="text" {...field("description")} />
      </label>
      <label>
        <span>tags</span>
        <input type="text" placeholder="space-separated" {...field("tags")} />
      </label>
      <label>
        <span>notes</span>
        <textarea rows={5} {...field("notes")} />
      </label>
      <div class="edit-buttons">
        <button type="submit" disabled={busy}>
          Save
        </button>
        <button type="button" onClick={onCancel}>
          Cancel
        </button>
      </div>
    </form>
  );
}

/** The overlord's buttons: restart, stop, and its agents to switch to. */
export function OverlordActions({ o, r }: { o: Overlord; r: Runner }) {
  const busy = r.busy !== null;
  const ref = o.id;
  return (
    <div class="actions">
      {isRunning(o) && (
        <>
          <ConfirmButton
            label="Restart"
            ask="Restart the overlord?"
            disabled={busy}
            onConfirm={() => void r.run("restarting…", [{ op: "session.restart", args: { ref } }])}
          />
          <ConfirmButton
            label="Stop"
            ask="Stop the overlord?"
            disabled={busy}
            onConfirm={() => void r.run("stopping…", [{ op: "session.stop", args: { ref } }])}
          />
        </>
      )}
      <span class="roster" title="the overlord's agents: pick one to make it the overlord">
        {o.overlords.map((a) =>
          a.active ? (
            <span class="agent-chip on">{a.label}</span>
          ) : (
            <button
              type="button"
              class="agent-chip"
              disabled={busy || !a.installed}
              title={
                a.installed ? `switch the overlord to ${a.label}` : `${a.label} isn't installed`
              }
              onClick={() =>
                void r.run(`switching to ${a.label}…`, [
                  { op: "overlord.switch", args: { agent: a.agent } },
                ])
              }
            >
              {a.label}
              {a.exists && a.status_label && <span class="dim"> · {a.status_label}</span>}
            </button>
          ),
        )}
      </span>
      <RunnerNote r={r} />
    </div>
  );
}

/** "Tell the overlord": typed into its pane, as if at its prompt. */
export function Tell({ store }: { store: Store }) {
  const [text, setText] = useState("");
  const [sent, setSent] = useState(false);
  const r = useRunner(store);
  const send = async () => {
    const t = text.trim();
    if (!t) return;
    setSent(false);
    if (await r.run("sending…", [{ op: "overlord.ask", args: { text: t } }])) {
      setText("");
      setSent(true);
    }
  };
  return (
    <form
      class="tell"
      onSubmit={(ev) => {
        ev.preventDefault();
        void send();
      }}
    >
      <textarea
        rows={1}
        placeholder="tell the overlord…"
        aria-label="tell the overlord"
        value={text}
        onInput={(ev) => {
          setText(ev.currentTarget.value);
          setSent(false);
        }}
        onKeyDown={(ev) => {
          // Enter sends; Shift-Enter is a new line.
          if (ev.key === "Enter" && !ev.shiftKey && !ev.isComposing) {
            ev.preventDefault();
            void send();
          }
        }}
      />
      <button type="submit" disabled={r.busy !== null || !text.trim()}>
        Send
      </button>
      <div class="tell-note">
        <RunnerNote r={r} />
        {sent && !r.busy && !r.error && <span class="dim">sent</span>}
      </div>
    </form>
  );
}
