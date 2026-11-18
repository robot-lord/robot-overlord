import { describe, expect, it } from "vitest";
import type { UiCmd } from "../src/proto";
import { decide, INPUT_EVERY_MS, type OpenForm, Reporter } from "../src/ui";
import cmds from "./fixtures/ui_cmds.json";

const fx = cmds as unknown as Record<string, UiCmd>;
const cmd = (name: string): UiCmd => {
  const c = fx[name];
  if (!c) throw new Error(`no ${name} in the fixture`);
  return c;
};

describe("the overlord's commands", () => {
  it("goto routes to the session or the overlord", () => {
    expect(decide({ cmd: "goto", id: "a1", overlord: false }, null)).toEqual([
      { do: "route", to: { kind: "session", id: "a1" }, closeForm: false },
    ]);
    expect(decide({ cmd: "goto", id: "o1", overlord: true }, null)).toEqual([
      { do: "route", to: { kind: "overlord" }, closeForm: false },
    ]);
  });

  it("show_list routes to the list and closes only an untouched form", () => {
    const open: OpenForm = { kind: "manual", cmd: null, touched: false };
    expect(decide(cmd("show_list"), open)).toEqual([
      { do: "route", to: { kind: "list" }, closeForm: true },
    ]);
    expect(decide(cmd("show_list"), { ...open, touched: true })).toEqual([
      { do: "route", to: { kind: "list" }, closeForm: false },
    ]);
  });

  it("set_view applies the view and the row to highlight", () => {
    const [e] = decide(cmd("set_view"), null);
    expect(e).toMatchObject({ do: "view", select: "alpha" });
    expect(e?.do === "view" && e.view.query).toBe("tag:web");
  });

  it("a notice is a toast", () => {
    expect(decide(cmd("notice"), null)).toEqual([
      { do: "toast", text: "the overlord opened a new-session form in the terminal" },
    ]);
  });

  it("a form opens, unless the user is typing in one: then it is parked", () => {
    for (const [name, kind] of [
      ["new_session", "manual"],
      ["confirm_new_session", "confirm"],
      ["setup", "setup"],
      ["keys", "keys"],
    ] as const) {
      const c = cmd(name);
      expect(decide(c, null)).toEqual([{ do: "open", form: { kind, cmd: c, touched: false } }]);
      // An untouched form is replaced.
      expect(decide(c, { kind: "fork", cmd: null, touched: false })[0]?.do).toBe("open");
      expect(decide(c, { kind: "fork", cmd: null, touched: true })).toEqual([
        { do: "park", cmd: c },
      ]);
    }
  });
});

describe("page.report", () => {
  const view = {
    query: "",
    sort: "updated",
    reverse: false,
    usage: null,
    archived: "live",
    tab: "sessions",
    group: null,
  };

  it("sends everything first, then only what changed", () => {
    const sent: Record<string, unknown>[] = [];
    const r = new Reporter((a) => sent.push(a));
    r.shown({ route: { kind: "list" }, view, visible: true, form: null });
    expect(sent[0]).toEqual({ route: { kind: "list" }, view, visible: true, form: null });
    r.shown({ route: { kind: "list" }, view, visible: true, form: null });
    expect(sent).toHaveLength(1);
    r.shown({ route: { kind: "session", id: "a1" }, view, visible: true, form: "fork" });
    expect(sent[1]).toEqual({ route: { kind: "session", id: "a1" }, form: "fork" });
    // After a reconnect (a new page id), everything again.
    r.reset();
    r.shown({ route: { kind: "session", id: "a1" }, view, visible: false, form: "fork" });
    expect(Object.keys(sent[2] ?? {}).toSorted()).toEqual(["form", "route", "view", "visible"]);
  });

  it("reports input at most every 5 s", () => {
    const sent: Record<string, unknown>[] = [];
    let t = 1000;
    const r = new Reporter(
      (a) => sent.push(a),
      () => t,
    );
    r.input();
    r.input();
    t += INPUT_EVERY_MS - 1;
    r.input();
    expect(sent).toEqual([{ input: true }]);
    t += 1;
    r.input();
    expect(sent).toHaveLength(2);
  });
});
