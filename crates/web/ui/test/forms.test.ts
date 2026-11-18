import { describe, expect, it } from "vitest";
import { canWorktree, forkArgs } from "../src/forms/Fork";
import {
  askFor,
  described,
  fromSetup,
  prefill,
  rootsOptions,
  saveCall,
  setFolder,
} from "../src/forms/setup";
import type { FormSetup, SetupPrefill } from "../src/proto";
import setupFx from "./fixtures/form_setup.json";
import cmds from "./fixtures/ui_cmds.json";

const answer = setupFx as unknown as FormSetup;
const proposal = cmds.setup.prefill as unknown as SetupPrefill;

describe("the code-folder form", () => {
  it("with nothing set and nothing to suggest: asks for a folder", () => {
    const f = fromSetup(answer, true);
    expect(f.codeDir).toBe("");
    expect(f.describe).toBe("");
    expect(saveCall(f)).toEqual({ error: "type the folder you keep your repos in" });
    expect(saveCall(f, true)).toMatchObject({ error: expect.stringContaining("still finding") });
    expect(fromSetup(answer, false).describe).toBeNull();
  });

  it("saves a folder by plain code, asking before creating it or using home", () => {
    const f = setFolder(fromSetup(answer, false), "/h/code");
    expect(saveCall(f)).toEqual({
      op: "form.setup_save",
      args: { code_dir: "/h/code", roots: "follow" },
    });
    expect(askFor("/h/code doesn't exist (create it?)")).toBe("create");
    expect(saveCall({ ...f, confirmCreate: "/h/code" })).toMatchObject({
      args: { create: true },
    });
    expect(askFor("~ is your whole home folder: the repo finder would scan all of it.")).toBe(
      "home",
    );
    expect(saveCall({ ...f, confirmHome: "/h/code" })).toMatchObject({
      args: { allow_home: true },
    });
    // A new folder drops what was agreed for the old one.
    expect(setFolder({ ...f, confirmCreate: "/h/code" }, "/h/src").confirmCreate).toBeNull();
    expect(askFor("anything else")).toBeNull();
  });

  it("with a description: asks the overlord, saying whose each hint is", () => {
    const f = described(setFolder(fromSetup(answer, true), "/h/src"), "my code is in ~/src");
    expect(saveCall(f)).toEqual({
      op: "overlord.setup_request",
      args: {
        description: "my code is in ~/src",
        code_dir: "/h/src",
        code_dir_source: "picked by the user",
        roots: "follow",
        roots_source: "user",
      },
    });
  });

  it("reopened by the overlord: its proposal, saved as shown", () => {
    const f = prefill(fromSetup(answer, true), proposal);
    expect([f.codeDir, f.note, f.confirmed]).toEqual(["/tmp", "n", true]);
    expect(f.changed).toEqual(["code_dir"]);
    expect(f.roots).toEqual({ kind: "set", list: ["/tmp"] });
    expect(rootsOptions(f)[0]).toEqual({ kind: "set", list: ["/tmp"] });
    // The description is still there, but confirmed: saved by plain code.
    expect(saveCall(f)).toEqual({
      op: "form.setup_save",
      args: { code_dir: "/tmp", roots: ["/tmp"] },
    });
    // Editing the description asks the overlord again.
    expect(saveCall(described(f, "something else"))).toMatchObject({
      op: "overlord.setup_request",
    });
  });
});

describe("the fork form", () => {
  const s = { id: "a1", agent: "claude", repo_path: "/r" };
  it("offers a worktree only for an agent that forks elsewhere, in a repo", () => {
    expect(canWorktree(s)).toBe(true);
    expect(canWorktree({ ...s, agent: "opencode" })).toBe(false);
    expect(canWorktree({ ...s, repo_path: null })).toBe(false);
  });
  it("sends only what was filled in", () => {
    expect(forkArgs(s, " ", "", false)).toEqual({ ref: "a1" });
    expect(forkArgs(s, "try X", "a1-x", true)).toEqual({
      ref: "a1",
      prompt: "try X",
      name: "a1-x",
      workspace: true,
    });
    expect(forkArgs({ ...s, agent: "opencode" }, "", "", true)).toEqual({ ref: "a1" });
  });
});
