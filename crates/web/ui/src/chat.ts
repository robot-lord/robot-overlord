// A conversation as text: the overlord's (`overlord.messages`) or a
// session's (`session.messages`), the latest page and earlier ones on
// request, and the bubbles for asks still in the phone's outbox. Pure
// logic and signals: views/Chat.tsx draws it.

import { batch, signal } from "@preact/signals";
import type { OutboxItem, OverlordMessage, Status } from "./proto";
import type { Requester } from "./store";
import type { Caps } from "./transport";

/** While the agent is working and the chat is on screen, ask this often. */
export const POLL_MS = 3000;

/** How often to look again, or null (only on `changed`). */
export function pollEvery(status: Status | null | undefined, visible: boolean): number | null {
  return status === "working" && visible ? POLL_MS : null;
}

/** `latest` (the newest page) after the earlier messages already shown. */
export function mergeLatest(have: OverlordMessage[], latest: OverlordMessage[]): OverlordMessage[] {
  const first = latest[0]?.line;
  if (first === undefined) return have;
  return [...have.filter((m) => m.line < first), ...latest];
}

/** `older` (a page from before the first shown) before what is shown. */
export function mergeOlder(have: OverlordMessage[], older: OverlordMessage[]): OverlordMessage[] {
  const first = have[0]?.line ?? Number.POSITIVE_INFINITY;
  return [...older.filter((m) => m.line < first), ...have];
}

/** The answer's own name for its conversation (a new one replaces what is shown). */
function convKey(r: object): string {
  const head =
    (r as { overlord?: unknown; session?: unknown }).overlord ??
    (r as { session?: unknown }).session ??
    null;
  return JSON.stringify(head);
}

interface Page {
  messages: OverlordMessage[];
  more: boolean;
}

/** One conversation's messages, fetched by plain look-ups. */
export class MessageLog {
  readonly messages = signal<OverlordMessage[]>([]);
  /** Earlier ones are left. */
  readonly more = signal(false);
  /** Nothing asked back yet. */
  readonly loaded = signal(false);
  readonly error = signal<string | null>(null);
  readonly loadingOlder = signal(false);

  private key: string | null = null;
  private inFlight = false;
  private again = false;
  private readonly api: Requester;
  private readonly op: "overlord.messages" | "session.messages";
  private readonly args: Record<string, unknown>;

  constructor(
    api: Requester,
    op: "overlord.messages" | "session.messages",
    args: Record<string, unknown> = {},
  ) {
    this.api = api;
    this.op = op;
    this.args = args;
  }

  /** The newest page. One at a time: one asked for meanwhile runs after. */
  async refresh(): Promise<void> {
    if (this.inFlight) {
      this.again = true;
      return;
    }
    this.inFlight = true;
    try {
      const r = await this.api.request<Page>(this.op, this.args);
      const key = convKey(r);
      batch(() => {
        if (key !== this.key) {
          this.key = key;
          this.messages.value = r.messages;
          this.more.value = r.more;
        } else {
          const merged = mergeLatest(this.messages.value, r.messages);
          // More earlier ones only if what was shown before had them.
          if (merged.length === r.messages.length) this.more.value = r.more;
          this.messages.value = merged;
        }
        this.error.value = null;
        this.loaded.value = true;
      });
    } catch (e) {
      this.error.value = e instanceof Error ? e.message : String(e);
    } finally {
      this.inFlight = false;
    }
    if (this.again) {
      this.again = false;
      await this.refresh();
    }
  }

  /** The page before the first shown. */
  async older(): Promise<void> {
    const first = this.messages.value[0]?.line;
    if (first === undefined || !this.more.value || this.loadingOlder.value) return;
    this.loadingOlder.value = true;
    const key = this.key;
    try {
      const r = await this.api.request<Page>(this.op, { ...this.args, before: first });
      // The conversation changed meanwhile: this page is another's.
      if (convKey(r) !== key || this.key !== key) return;
      batch(() => {
        this.messages.value = mergeOlder(this.messages.value, r.messages);
        this.more.value = r.more;
      });
    } catch (e) {
      this.error.value = e instanceof Error ? e.message : String(e);
    } finally {
      this.loadingOlder.value = false;
    }
  }
}

/** An ask still in the outbox, as the chat shows it. */
export interface Bubble {
  id: string;
  text: string;
  at: number;
  kind: "pending" | "failed" | "interrupted";
  note: string;
}

/** The outbox's asks as bubbles, oldest first. */
export function bubbles(items: OutboxItem[]): Bubble[] {
  return items
    .filter((i) => i.op === "overlord.ask")
    .toSorted((a, b) => a.at - b.at)
    .map((i): Bubble => {
      const base = { id: i.id, text: i.text ?? "", at: i.at };
      switch (i.state) {
        case "waiting":
          return { ...base, kind: "pending", note: "waiting to be sent" };
        case "sent":
          return { ...base, kind: "pending", note: "sent, waiting for your computer" };
        case "running":
          return { ...base, kind: "pending", note: "your computer has it" };
        case "interrupted":
          return {
            ...base,
            kind: "interrupted",
            note: "may not have reached the overlord: check the chat, then send it again",
          };
        case "failed":
          return {
            ...base,
            kind: "failed",
            note:
              i.error?.code === "expired"
                ? "not answered in 24 hours: not sent"
                : (i.error?.message ?? "failed"),
          };
      }
    });
}

/**
 * Whether a failed ask goes back into the composer (with the error under
 * it). With an outbox, only when it never entered it; once in, its
 * bubble shows the failure.
 */
export function backToComposer(code: string | undefined, caps: Caps): boolean {
  if (!caps.pending) return true;
  return code !== "interrupted" && code !== "expired" && code !== "cancelled" && code !== "failed";
}
