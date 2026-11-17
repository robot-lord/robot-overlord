// The status chip's words, as the TUI draws them (crates/tui/src/lib.rs
// `state_chip`, statustext.rs).

import { describe, expect, it } from "vitest";
import { type Shown, shown, workLines } from "../src/components/Status";

const base: Shown = { status: "idle", status_label: "your turn", shown_status: "idle" };
const limit = { resets_at: 0, until: "14:05", reset: false, kind: null, resume: false };

describe("shown", () => {
  it("is the stored status when nothing is laid over it", () => {
    expect(shown(base, false)).toEqual({ cls: "idle", label: "your turn" });
    expect(shown({ status: "working", status_label: "working" }, true)).toEqual({
      cls: "working",
      label: "working",
    });
  });

  it("says background work, short in the list and in words in a header", () => {
    const s: Shown = { ...base, shown_status: "background", async_tasks: 2 };
    expect(shown(s, false)).toEqual({ cls: "background", label: "bg 2" });
    expect(shown(s, true).label).toBe("waiting on 2 background tasks");
    expect(shown({ ...s, async_tasks: 1 }, true).label).toBe("waiting on 1 background task");
  });

  it("says a usage limit, its reset and a scheduled resume", () => {
    const s: Shown = { ...base, status: "stopped", shown_status: "limited", limit };
    expect(shown(s, false)).toEqual({ cls: "limited", label: "till 14:05" });
    expect(shown(s, true).label).toBe("limited until 14:05");
    const resume = { ...s, limit: { ...limit, resume: true } };
    expect(shown(resume, false).label).toBe("till 14:05 ↻");
    expect(shown(resume, true).label).toBe("limited until 14:05 · resumes then");
    const reset = { ...s, limit: { ...limit, reset: true } };
    expect(shown(reset, false).label).toBe("reset");
    expect(shown(reset, true).label).toBe("limit reset just now");
    expect(shown({ ...resume, limit: { ...limit, reset: true, resume: true } }, true).label).toBe(
      "limit reset · resuming",
    );
  });

  it("falls back to the stored status for a limit it wasn't given", () => {
    expect(shown({ ...base, shown_status: "limited" }, false)).toEqual({
      cls: "idle",
      label: "your turn",
    });
  });
});

describe("workLines", () => {
  const at = 10 * 60_000;
  it("names the background work and the shells", () => {
    const s: Shown = {
      ...base,
      shown_status: "background",
      async_tasks: 3,
      async_work: [
        { label: "tests", started_at: at - 5 * 60_000 },
        { label: "lint", started_at: at - 60_000 },
      ],
      async_shells: { count: 1, tasks: [{ label: "npm run dev", started_at: at - 2 * 60_000 }] },
    };
    expect(workLines(s, at)).toEqual([
      "Waiting on 3 background tasks: tests, lint, +1 more, started 5m ago. Its turn is over; it goes on when they finish.",
      "1 background shell: npm run dev (2m ago).",
    ]);
  });

  it("says nothing without background work", () => {
    expect(workLines(base, at)).toEqual([]);
    expect(workLines({ ...base, async_shells: { count: 0, tasks: [] } }, at)).toEqual([]);
  });
});
