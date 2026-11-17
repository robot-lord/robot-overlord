import { describe, expect, it } from "vitest";
import { ago, lastUpdate } from "../src/components/time";
import { href, parse } from "../src/route";

describe("routes", () => {
  it("parse and print", () => {
    expect(parse("")).toEqual({ kind: "list" });
    expect(parse("#/")).toEqual({ kind: "list" });
    expect(parse("#/overlord")).toEqual({ kind: "overlord" });
    expect(parse("#/s/01ABC")).toEqual({ kind: "session", id: "01ABC" });
    expect(parse("#/s/a%20b")).toEqual({ kind: "session", id: "a b" });
    expect(parse("#/s/%E0%A4%A")).toEqual({ kind: "list" });
    expect(parse("#/s/")).toEqual({ kind: "list" });
    expect(parse("#/nope")).toEqual({ kind: "list" });
    for (const r of [
      { kind: "list" } as const,
      { kind: "overlord" } as const,
      { kind: "session", id: "x/y z" } as const,
    ]) {
      expect(parse(href(r))).toEqual(r);
    }
  });
});

describe("times", () => {
  it("reads as a list shows them", () => {
    const m = 60_000;
    expect(ago(0, 59_000)).toBe("now");
    expect(ago(0, 5 * m)).toBe("5m");
    expect(ago(0, 3 * 60 * m)).toBe("3h");
    expect(ago(0, 3 * 24 * 60 * m)).toBe("3d");
    expect(ago(0, 30 * 24 * 60 * m)).toBe("4w");
    expect(ago(10, 0)).toBe("now");
  });

  it("last update is the latest message, else creation", () => {
    expect(lastUpdate({ last_user_msg_at: 5, last_agent_msg_at: 9, created_at: 1 })).toBe(9);
    expect(lastUpdate({ last_user_msg_at: null, last_agent_msg_at: null, created_at: 1 })).toBe(1);
  });
});
