// Conversations as text, for a page without panes (the phone): the
// overlord's chat (`overlord.messages`, a composer sending `overlord.ask`,
// bubbles for asks still in the outbox) and a session's recent messages,
// read-only (`session.messages`). Everything is drawn as text nodes: no
// markup from a message reaches the page, and a link in one is only text.

import { signal } from "@preact/signals";
import type { ComponentChildren } from "preact";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "preact/hooks";
import { isRunning } from "../actions";
import { backToComposer, type Bubble, bubbles, MessageLog, pollEvery } from "../chat";
import { ResumeButton, type Runner, RunnerNote } from "../components/Actions";
import { ago, now } from "../components/time";
import type { OutboxItem, Overlord, OverlordMessage, SessionBase, Status } from "../proto";
import type { Requester, Store } from "../store";
import { ApiError, type Caps } from "../transport";
import { StartOverlord } from "./Overlord";

/** Whether the page is on screen (the app in the foreground). */
const visible = signal(typeof document === "undefined" || document.visibilityState === "visible");
if (typeof document !== "undefined") {
  document.addEventListener("visibilitychange", () => {
    visible.value = document.visibilityState === "visible";
  });
}

/** Ask again on every new snapshot, and every few seconds while `status` is working. */
function useRefresh(log: MessageLog, store: Store, status: Status | null | undefined): void {
  const rev = store.snapshot.value?.rev;
  useEffect(() => {
    void log.refresh();
  }, [log, rev, store.conn.value]);
  const every = pollEvery(status, visible.value);
  useEffect(() => {
    if (every === null) return;
    const id = setInterval(() => void log.refresh(), every);
    return () => clearInterval(id);
  }, [log, every]);
}

/** "14:05" for a message's time. */
function clock(at: string | null): string {
  if (!at) return "";
  const d = new Date(at);
  return Number.isNaN(d.getTime())
    ? ""
    : d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function Message({ m }: { m: OverlordMessage }) {
  return (
    <div class={`msg msg-${m.who}`}>
      <p class="msg-text">{m.text}</p>
      {m.at && <span class="msg-at">{clock(m.at)}</span>}
    </div>
  );
}

/**
 * A log's messages, oldest first, scrolled to the newest. Earlier ones
 * load when scrolled to the top (or by the button there).
 */
export function MessageList({
  log,
  empty,
  children,
}: {
  log: MessageLog;
  empty: string;
  /** After the messages (the outbox's bubbles). */
  children?: ComponentChildren;
}) {
  const box = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);
  /** Distance from the bottom to keep while earlier messages come in above. */
  const keep = useRef<number | null>(null);
  const msgs = log.messages.value;
  const older = () => {
    const el = box.current;
    if (el) keep.current = el.scrollHeight - el.scrollTop;
    void log.older();
  };
  useLayoutEffect(() => {
    const el = box.current;
    if (!el) return;
    if (keep.current !== null) {
      el.scrollTop = el.scrollHeight - keep.current;
      keep.current = null;
    } else if (atBottom.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [msgs, children]);
  return (
    <div
      class="msgs"
      ref={box}
      onScroll={(e) => {
        const el = e.currentTarget;
        atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
        if (el.scrollTop < 40 && log.more.value && !log.loadingOlder.value) older();
      }}
    >
      {log.more.value && (
        <button type="button" class="dim older" disabled={log.loadingOlder.value} onClick={older}>
          {log.loadingOlder.value ? "loading…" : "Earlier messages"}
        </button>
      )}
      {msgs.length === 0 && (
        <p class="hint">{log.error.value ?? (log.loaded.value ? empty : "loading…")}</p>
      )}
      {msgs.map((m) => (
        <Message key={m.line} m={m} />
      ))}
      {children}
      {msgs.length > 0 && log.error.value && <p class="err msg-err">{log.error.value}</p>}
    </div>
  );
}

function BubbleView({
  b,
  cancel,
  resend,
}: {
  b: Bubble;
  cancel: (id: string) => void;
  resend: (b: Bubble) => void;
}) {
  return (
    <div class={`msg msg-you bubble bubble-${b.kind}`}>
      <p class="msg-text">{b.text}</p>
      <span class="msg-note">
        {b.note} · {ago(b.at, now.value)}
      </span>
      <span class="bubble-buttons">
        {b.kind === "pending" ? (
          <button type="button" onClick={() => cancel(b.id)}>
            Cancel
          </button>
        ) : (
          <>
            <button type="button" onClick={() => resend(b)}>
              Send again
            </button>
            <button type="button" class="dim" onClick={() => cancel(b.id)}>
              Dismiss
            </button>
          </>
        )}
      </span>
    </div>
  );
}

/**
 * The overlord's chat: its messages, the outbox's asks, and a composer.
 * `api` sends (the transport itself: an ask may wait in the outbox before
 * any hello); `cancel` drops an outbox entry.
 */
export function Chat({
  store,
  api,
  caps,
  outbox = [],
  cancel = () => {},
  status,
}: {
  store: Store;
  api: Requester;
  caps: Caps;
  outbox?: OutboxItem[];
  cancel?: (id: string) => void;
  status: Status | null | undefined;
}) {
  const log = useMemo(() => new MessageLog(api, "overlord.messages"), [api]);
  useRefresh(log, store, status);
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [inFlight, setInFlight] = useState(0);
  const ask = async (t: string): Promise<void> => {
    setError(null);
    setInFlight((n) => n + 1);
    try {
      await api.request("overlord.ask", { text: t });
      void log.refresh();
    } catch (e) {
      const code = e instanceof ApiError ? e.code : undefined;
      if (backToComposer(code, caps)) {
        setText((cur) => cur || t);
        setError(e instanceof Error ? e.message : String(e));
      }
    } finally {
      setInFlight((n) => n - 1);
    }
  };
  const send = () => {
    const t = text.trim();
    if (!t) return;
    setText("");
    void ask(t);
  };
  // Without an outbox an ask is answered at once: one at a time.
  const blocked = !caps.pending && inFlight > 0;
  return (
    <div class="chat">
      <MessageList log={log} empty="Nothing said yet.">
        {bubbles(outbox).map((b) => (
          <BubbleView
            key={b.id}
            b={b}
            cancel={cancel}
            resend={(x) => {
              cancel(x.id);
              void ask(x.text);
            }}
          />
        ))}
      </MessageList>
      <form
        class="composer"
        onSubmit={(ev) => {
          ev.preventDefault();
          send();
        }}
      >
        <textarea
          rows={1}
          placeholder="tell the overlord…"
          aria-label="tell the overlord"
          value={text}
          onInput={(ev) => {
            setText(ev.currentTarget.value);
            setError(null);
          }}
          onKeyDown={(ev) => {
            // Cmd- or Ctrl-Enter sends; Enter is a new line (a phone's return key).
            if (ev.key === "Enter" && (ev.metaKey || ev.ctrlKey) && !ev.isComposing) {
              ev.preventDefault();
              send();
            }
          }}
        />
        <button type="submit" class="primary" disabled={blocked || !text.trim()}>
          {blocked ? "sending…" : "Send"}
        </button>
        {error && <p class="err composer-err">{error}</p>}
      </form>
    </div>
  );
}

/** The phone's overlord body: Start when it isn't running, then the chat. */
export function OverlordChat({
  store,
  api,
  caps,
  o,
  outbox,
  cancel,
}: {
  store: Store;
  api: Requester;
  caps: Caps;
  o: Overlord;
  outbox: OutboxItem[];
  cancel: (id: string) => void;
}) {
  return (
    <>
      {!isRunning(o) && (
        <div class="not-running inline">
          <p>The overlord isn't running.</p>
          <StartOverlord store={store} />
        </div>
      )}
      <Chat store={store} api={api} caps={caps} outbox={outbox} cancel={cancel} status={o.status} />
    </>
  );
}

/** A session's recent messages, read-only. */
export function SessionMessages({
  store,
  api,
  s,
}: {
  store: Store;
  api: Requester;
  s: SessionBase;
}) {
  const log = useMemo(() => new MessageLog(api, "session.messages", { ref: s.id }), [api, s.id]);
  useRefresh(log, store, s.status);
  return <MessageList log={log} empty="No messages yet." />;
}

/**
 * The phone's session body: Resume when it isn't running, its notes, its
 * recent messages, and why there is no terminal.
 */
export function SessionText({
  store,
  api,
  s,
  act,
}: {
  store: Store;
  api: Requester;
  s: SessionBase;
  act: Runner;
}) {
  return (
    <div class="session-text">
      {!isRunning(s) && (
        <div class="not-running inline">
          <p>{s.archived_at ? "This session is archived." : "This session isn't running."}</p>
          <ResumeButton s={s} r={act} />
          <RunnerNote r={act} />
        </div>
      )}
      {s.notes && <p class="notes">{s.notes}</p>}
      <SessionMessages store={store} api={api} s={s} />
      <p class="dim small no-pane">The live terminal isn't available remotely yet.</p>
    </div>
  );
}
