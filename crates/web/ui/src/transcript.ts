// A session's conversation as text (`session.messages`; the overlord's by
// its id): the latest page, and earlier ones on request. Pure logic and
// signals: views/Transcript.tsx draws it.

import { batch, signal } from "@preact/signals";
import type { Status, TranscriptMessage } from "./proto";
import type { Requester } from "./store";

/** While the agent is working and the transcript is on screen, ask this often. */
export const POLL_MS = 3000;

/** How often to look again, or null (only on `changed`). */
export function pollEvery(status: Status | null | undefined, visible: boolean): number | null {
  return status === "working" && visible ? POLL_MS : null;
}

/** `latest` (the newest page) after the earlier messages already shown. */
export function mergeLatest(
  have: TranscriptMessage[],
  latest: TranscriptMessage[],
): TranscriptMessage[] {
  const first = latest[0]?.line;
  if (first === undefined) return have;
  return [...have.filter((m) => m.line < first), ...latest];
}

/** `older` (a page from before the first shown) before what is shown. */
export function mergeOlder(
  have: TranscriptMessage[],
  older: TranscriptMessage[],
): TranscriptMessage[] {
  const first = have[0]?.line ?? Number.POSITIVE_INFINITY;
  return [...older.filter((m) => m.line < first), ...have];
}

/** The answer's own name for its conversation (a new one replaces what is shown). */
function convKey(r: object): string {
  return JSON.stringify((r as { session?: unknown }).session ?? null);
}

interface Page {
  messages: TranscriptMessage[];
  more: boolean;
}

/** One conversation's messages, fetched by plain look-ups. */
export class MessageLog {
  readonly messages = signal<TranscriptMessage[]>([]);
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
  private readonly args: Record<string, unknown>;

  /** `ref`: the session's id (the overlord's for its own). */
  constructor(api: Requester, ref: string) {
    this.api = api;
    this.args = { ref };
  }

  /** The newest page. One at a time: one asked for meanwhile runs after. */
  async refresh(): Promise<void> {
    if (this.inFlight) {
      this.again = true;
      return;
    }
    this.inFlight = true;
    try {
      const r = await this.api.request<Page>("session.messages", this.args);
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
      const r = await this.api.request<Page>("session.messages", { ...this.args, before: first });
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
