// The chat's logic against the daemon's real `overlord.messages` answer
// (test/fixtures/overlord_messages.json, written and checked by
// crates/daemon/tests/web_api.rs): the latest page, earlier pages, a new
// conversation, polling while the overlord works, and the outbox's bubbles.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  backToComposer,
  bubbles,
  MessageLog,
  mergeLatest,
  mergeOlder,
  POLL_MS,
  pollEvery,
} from "../src/chat";
import type { OverlordMessage, OverlordMessages } from "../src/proto";
import type { Requester } from "../src/store";
import { LOCAL_CAPS, REMOTE_CAPS } from "../src/transport";
import fixture from "./fixtures/overlord_messages.json";

/** JSON imports type strings as `string`: compare against that. */
type Widen<T> = T extends string
  ? string
  : T extends number
    ? number
    : T extends boolean
      ? boolean
      : T extends readonly (infer U)[]
        ? Widen<U>[]
        : T extends object
          ? { [K in keyof T]: Widen<T[K]> }
          : T;

// The fixture fits the type (`tsc --noEmit` fails otherwise).
const fits: Widen<OverlordMessages> = fixture;
const answer = fixture as OverlordMessages;

const msg = (line: number, text = `m${line}`): OverlordMessage => ({
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

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("the chat's messages", () => {
  it("reads the daemon's answer", async () => {
    expect(fits.messages.length).toBe(5);
    const { api, calls } = fakeApi();
    const log = new MessageLog(api, "overlord.messages");
    expect(log.loaded.value).toBe(false);
    const done = log.refresh();
    expect(calls[0]).toMatchObject({ op: "overlord.messages", args: {} });
    calls[0]?.resolve(answer);
    await done;
    expect(log.messages.value.map((m) => m.text)).toEqual(answer.messages.map((m) => m.text));
    expect(log.messages.value[0]?.who).toBe("agent");
    expect(log.more.value).toBe(true);
    expect(log.loaded.value).toBe(true);
  });

  it("pages back from the first shown, and merges a newer page after them", async () => {
    const { api, calls } = fakeApi();
    const log = new MessageLog(api, "overlord.messages");
    const first = log.refresh();
    calls[0]?.resolve(answer);
    await first;
    const first_line = answer.messages[0]?.line ?? 0;
    const older = log.older();
    expect(calls[1]?.args).toEqual({ before: first_line });
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

  it("a new conversation (another overlord, a fresh start) replaces what is shown", async () => {
    const { api, calls } = fakeApi();
    const log = new MessageLog(api, "overlord.messages");
    const one = log.refresh();
    calls[0]?.resolve(answer);
    await one;
    const two = log.refresh();
    calls[1]?.resolve({
      overlord: { id: "overlord-2", agent: "codex", conversation: "c2" },
      messages: [msg(1, "hello")],
      more: false,
    });
    await two;
    expect(log.messages.value.map((m) => m.text)).toEqual(["hello"]);
    expect(log.more.value).toBe(false);
  });

  it("asks one at a time, and once more after if asked meanwhile", async () => {
    const { api, calls } = fakeApi();
    const log = new MessageLog(api, "overlord.messages");
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
    const log = new MessageLog(api, "session.messages", { ref: "a1" });
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

  it("polls every 3 s only while the overlord works and the chat is on screen", () => {
    expect(pollEvery("working", true)).toBe(POLL_MS);
    expect(POLL_MS).toBe(3000);
    expect(pollEvery("working", false)).toBeNull();
    expect(pollEvery("idle", true)).toBeNull();
    expect(pollEvery(null, true)).toBeNull();
  });
});

describe("pending and failed bubbles", () => {
  it("are the outbox's asks, oldest first, each with what can be done", () => {
    const got = bubbles([
      { id: "c", op: "overlord.ask", text: "third", at: 30, state: "interrupted" },
      { id: "a", op: "overlord.ask", text: "first", at: 10, state: "waiting" },
      { id: "n", op: "overlord.new_session", at: 5, state: "waiting" },
      { id: "b", op: "overlord.ask", text: "second", at: 20, state: "running" },
      {
        id: "d",
        op: "overlord.ask",
        text: "fourth",
        at: 40,
        state: "failed",
        error: { code: "failed", message: "nothing to send" },
      },
    ]);
    expect(got.map((b) => [b.id, b.kind])).toEqual([
      ["a", "pending"],
      ["b", "pending"],
      ["c", "interrupted"],
      ["d", "failed"],
    ]);
    expect(got[2]?.note).toContain("check the chat");
    expect(got[3]?.note).toBe("nothing to send");
  });

  it("a refused ask goes back into the composer; one the outbox holds doesn't", () => {
    expect(backToComposer("failed", LOCAL_CAPS)).toBe(true);
    expect(backToComposer(undefined, LOCAL_CAPS)).toBe(true);
    for (const code of ["outbox_full", "disconnected", "host_offline", "rate_limited"]) {
      expect(backToComposer(code, REMOTE_CAPS)).toBe(true);
    }
    for (const code of ["interrupted", "expired", "cancelled", "failed"]) {
      expect(backToComposer(code, REMOTE_CAPS)).toBe(false);
    }
  });
});
