// A session's conversation as text, read-only (`session.messages`; the
// overlord's by its id): the phone's Transcript tab. Better than a
// terminal for long history, selecting and copying, and a stopped
// session. Everything is drawn as text nodes: no markup from a message
// reaches the page, and a link in one is only text.

import { signal } from "@preact/signals";
import { useEffect, useLayoutEffect, useMemo, useRef } from "preact/hooks";
import { isRunning } from "../actions";
import { ResumeButton, type Runner, RunnerNote } from "../components/Actions";
import type { SessionBase, Status, TranscriptMessage } from "../proto";
import type { Requester, Store } from "../store";
import { MessageLog, pollEvery } from "../transcript";

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

function Message({ m }: { m: TranscriptMessage }) {
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
export function MessageList({ log, empty }: { log: MessageLog; empty: string }) {
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
  }, [msgs]);
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
      {msgs.length > 0 && log.error.value && <p class="err msg-err">{log.error.value}</p>}
    </div>
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
  const log = useMemo(() => new MessageLog(api, s.id), [api, s.id]);
  useRefresh(log, store, s.status);
  return <MessageList log={log} empty="No messages yet." />;
}

/** The Transcript tab: Resume when it isn't running, then its messages. */
export function SessionText({
  store,
  api,
  s,
  act,
}: {
  store: Store;
  api: Requester;
  s: SessionBase;
  /** Resume's runner; none for the overlord (Start is above). */
  act?: Runner;
}) {
  return (
    <div class="session-text">
      {act && !isRunning(s) && (
        <div class="not-running inline">
          <p>{s.archived_at ? "This session is archived." : "This session isn't running."}</p>
          <ResumeButton s={s} r={act} />
          <RunnerNote r={act} />
        </div>
      )}
      <SessionMessages store={store} api={api} s={s} />
    </div>
  );
}
