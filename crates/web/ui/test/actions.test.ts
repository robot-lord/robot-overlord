import { describe, expect, it } from "vitest";
import { editOf, editRequests, isRunning, parseTags } from "../src/actions";
import type { SessionBase } from "../src/proto";
import fixture from "./fixtures/snapshot.json";

const row = fixture.rows[0] as unknown as SessionBase;

function session(patch: Partial<SessionBase> = {}): SessionBase {
  return { ...row, id: "s1", name: "api", description: "", notes: "", tags: ["web"], ...patch };
}

describe("parseTags", () => {
  it("reads tags as the list's tag editor does", () => {
    expect(parseTags(" #Web, ui  ui ##API,,")).toEqual(["web", "ui", "api"]);
    expect(parseTags("  ")).toEqual([]);
  });
});

describe("editRequests", () => {
  it("asks for nothing when nothing changed", () => {
    const s = session({ description: "d", notes: "n" });
    expect(editRequests(s, editOf(s))).toEqual([]);
    // Whitespace the daemon trims anyway isn't a change.
    expect(editRequests(s, { ...editOf(s), description: " d ", notes: "n\n" })).toEqual([]);
  });

  it("asks only for the fields that changed", () => {
    const s = session();
    expect(
      editRequests(s, {
        name: " api-two ",
        description: "new",
        tags: "ui #web",
        notes: "a note\n",
      }),
    ).toEqual([
      { op: "session.rename", args: { ref: "s1", name: "api-two" } },
      { op: "session.describe", args: { ref: "s1", description: "new" } },
      { op: "session.notes", args: { ref: "s1", notes: "a note" } },
      { op: "session.tag", args: { ref: "s1", add: ["ui"], remove: [] } },
    ]);
    expect(editRequests(s, { ...editOf(s), tags: "" })).toEqual([
      { op: "session.tag", args: { ref: "s1", add: [], remove: ["web"] } },
    ]);
  });

  it("never renames to nothing", () => {
    const s = session();
    expect(editRequests(s, { ...editOf(s), name: "  " })).toEqual([]);
  });
});

describe("isRunning", () => {
  it("needs a status other than stopped and a tmux session", () => {
    expect(isRunning({ status: "idle", tmux_session: "x" })).toBe(true);
    expect(isRunning({ status: "stopped", tmux_session: "x" })).toBe(false);
    expect(isRunning({ status: "idle", tmux_session: null })).toBe(false);
  });
});
