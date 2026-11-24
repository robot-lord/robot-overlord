// The transcript's logic (`session.messages`; the overlord's by its id):
// the latest page, earlier pages, a new conversation, one look-up at a
// time, and polling while the session works.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { SessionMessages, TranscriptMessage } from "../src/proto";
import type { Requester } from "../src/store";
import { MessageLog, mergeLatest, mergeOlder, POLL_MS, pollEvery } from "../src/transcript";

const msg = (line: number, text = `m${line}`): TranscriptMessage => ({
  line,
  who: line % 2 ? "you" : "agent",
  at: null,
  text,
});

interface Call {
  op: string;
  args: Record<string, unknown>;
  resolve(v: unknown): void;
  reject(e: unknown): void;
}

function fakeApi() {
  const calls: Call[] = [];
  const api: Requester = {
    request<T>(op: string, args: object = {}) {
      return new Promise<T>((resolve, reject) => {
        calls.push({
          op,
          args: args as Record<string, unknown>,
          resolve: (v) => resolve(v as T),
          reject,
        });
      });
    },
  };
  return { api, calls };
}

const settle = () => vi.advanceTimersByTimeAsync(0);

/** A `session.messages` answer: the newest page of five, earlier ones left. */
const answer: SessionMessages = {
  session: { id: "a1", name: "fixture session 0", agent: "claude", conversation: "c1" },
  messages: [66, 67, 68, 69, 70].map((l) => msg(l, l % 2 ? `ask ${l}` : `answer ${l}`)),
  more: true,
};

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("the transcript", () => {
  it("reads a session's messages by its id", async () => {
    const { api, calls } = fakeApi();
    const log = new MessageLog(api, "a1");
    expect(log.loaded.value).toBe(false);
    const done = log.refresh();
    expect(calls[0]).toMatchObject({ op: "session.messages", args: { ref: "a1" } });
    calls[0]?.resolve(answer);
    await done;
    expect(log.messages.value.map((m) => m.text)).toEqual(answer.messages.map((m) => m.text));
    expect(log.messages.value[0]?.who).toBe("agent");
    expect(log.messages.value[1]?.text).toBe("ask 67");
    expect(log.more.value).toBe(true);
    expect(log.loaded.value).toBe(true);
  });

  it("pages back from the first shown, and merges a newer page after them", async () => {
    const { api, calls } = fakeApi();
    const log = new MessageLog(api, "a1");
    const first = log.refresh();
    calls[0]?.resolve(answer);
    await first;
    const first_line = answer.messages[0]?.line ?? 0;
    const older = log.older();
    expect(calls[1]?.args).toEqual({ ref: "a1", before: first_line });
    calls[1]?.resolve({ ...answer, messages: [msg(60), msg(61)], more: false });
    await older;
    expect(log.messages.value.map((m) => m.line)).toEqual([60, 61, 66, 67, 68, 69, 70]);
    expect(log.more.value).toBe(false);
    // Nothing earlier left: no more asking.
    await log.older();
    expect(calls).toHaveLength(2);
    // The newest page moved on: what was shown before it stays.
    const again = log.refresh();
    calls[2]?.resolve({ ...answer, messages: [msg(68), msg(69), msg(70), msg(71)], more: true });
    await again;
    expect(log.messages.value.map((m) => m.line)).toEqual([60, 61, 66, 67, 68, 69, 70, 71]);
    expect(log.more.value).toBe(false);
  });

  it("a new conversation (a fresh start, another overlord) replaces what is shown", async () => {
    const { api, calls } = fakeApi();
    const log = new MessageLog(api, "a1");
    const one = log.refresh();
    calls[0]?.resolve(answer);
    await one;
    const two = log.refresh();
    calls[1]?.resolve({
      session: { id: "a1", name: "fixture session 0", agent: "claude", conversation: "c2" },
      messages: [msg(1, "hello")],
      more: false,
    });
    await two;
    expect(log.messages.value.map((m) => m.text)).toEqual(["hello"]);
    expect(log.more.value).toBe(false);
  });

  it("asks one at a time, and once more after if asked meanwhile", async () => {
    const { api, calls } = fakeApi();
    const log = new MessageLog(api, "a1");
    const a = log.refresh();
    void log.refresh();
    void log.refresh();
    expect(calls).toHaveLength(1);
    calls[0]?.resolve(answer);
    await settle();
    expect(calls).toHaveLength(2);
    calls[1]?.resolve(answer);
    await a;
  });

  it("a failed look-up says why and keeps what is shown", async () => {
    const { api, calls } = fakeApi();
    const log = new MessageLog(api, "a1");
    const one = log.refresh();
    expect(calls[0]).toMatchObject({ op: "session.messages", args: { ref: "a1" } });
    calls[0]?.resolve({ messages: [msg(3)], more: false });
    await one;
    const two = log.refresh();
    calls[1]?.reject(new Error("disconnected"));
    await two;
    expect(log.error.value).toBe("disconnected");
    expect(log.messages.value).toHaveLength(1);
  });

  it("merges by line", () => {
    expect(mergeLatest([msg(1), msg(2), msg(3)], [msg(3), msg(4)]).map((m) => m.line)).toEqual([
      1, 2, 3, 4,
    ]);
    expect(mergeLatest([msg(1)], [])).toEqual([msg(1)]);
    expect(mergeOlder([msg(5), msg(6)], [msg(4), msg(5)]).map((m) => m.line)).toEqual([4, 5, 6]);
  });

  it("polls every 3 s only while the session works and the transcript is on screen", () => {
    expect(pollEvery("working", true)).toBe(POLL_MS);
    expect(POLL_MS).toBe(3000);
    expect(pollEvery("working", false)).toBeNull();
    expect(pollEvery("idle", true)).toBeNull();
    expect(pollEvery(null, true)).toBeNull();
  });
});
