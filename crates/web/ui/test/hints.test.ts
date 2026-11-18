import { describe, expect, it } from "vitest";
import {
  agentBlocker,
  described,
  fresh,
  launch,
  owner,
  picked,
  prefill,
  prefillConfirm,
  request,
  setAgent,
  setLocation,
} from "../src/forms/hints";
import type { ConfirmPrefill, FormDefaults, FormRepos, NewPrefill } from "../src/proto";
import defaultsFx from "./fixtures/form_defaults.json";
import reposFx from "./fixtures/form_repos.json";
import cmds from "./fixtures/ui_cmds.json";

const d = defaultsFx as unknown as FormDefaults;
const repos = reposFx as unknown as FormRepos;
const confirm = cmds.confirm_new_session.prefill as unknown as ConfirmPrefill;
const proposal = cmds.new_session.prefill as unknown as NewPrefill;

describe("the describe form", () => {
  it("opens on the default repo and agent, every hint a default", () => {
    const f = fresh("describe", d, repos);
    expect(f.location).toEqual({ kind: "repo", path: "/scratch/home/src/demo" });
    expect([f.agent, f.model, f.defaultBase]).toEqual(["claude", "opus", "latest"]);
    expect(owner(f, "repo")).toBe("default");
    // Opened on a group: the session joins it, the user's choice.
    expect(fresh("describe", d, repos, "web")).toMatchObject({ tags: "web", chosen: ["tags"] });
  });

  it("any pick on a fresh form is the user's, and the request says so", () => {
    let f = fresh("describe", d, repos);
    f = { ...f, prompt: "fix the footer" };
    f = picked(setAgent(f, "codex", d.agents), "agent", true);
    f = picked({ ...f, workspace: true }, "worktree", false);
    expect(owner(f, "agent")).toBe("yours");
    const r = request(f);
    if ("error" in r) throw new Error(r.error);
    expect(r.op).toBe("overlord.new_session");
    expect(r.args).toMatchObject({
      description: "fix the footer",
      repo: "/scratch/home/src/demo",
      agent: "codex",
      model: "",
      workspace: true,
      base: "latest",
      tags: [],
      chosen: ["agent", "worktree"],
      resolved: [],
      confirmed: false,
    });
    expect(request({ ...f, prompt: " " })).toEqual({
      error: "describe what the session should do first",
    });
  });

  it("reopened by the overlord: its hints, its labels, then confirmed", () => {
    const f = prefillConfirm(fresh("describe", d, repos), confirm, d.agents);
    // The fixture's: a plain folder, codex, the agent default model.
    expect(f.location).toEqual({ kind: "folder", path: "/tmp" });
    expect([f.agent, f.model, f.workspace, f.base, f.prefer]).toEqual([
      "codex",
      "",
      false,
      "main",
      "local",
    ]);
    expect(f.tags).toBe("web");
    expect(owner(f, "agent")).toBe("overlord's");
    expect(f.changed).toEqual(["agent"]);
    expect(f.note).toBe("n");
    expect(f.confirmed).toBe(true);
    const r = request(f);
    if ("error" in r) throw new Error(r.error);
    expect(r.args).toMatchObject({
      project: "/tmp",
      workspace: false,
      confirmed: true,
      resolved: confirm.resolved,
    });
    // Re-picking the same value keeps the overlord's label; a change is
    // the user's and drops the confirmation.
    expect(owner(picked(f, "agent", false), "agent")).toBe("overlord's");
    const g = picked(setAgent(f, "claude", d.agents), "agent", true);
    expect([owner(g, "agent"), g.confirmed, g.note]).toEqual(["yours", false, null]);
    expect(described({ ...f, prompt: "something else" }).confirmed).toBe(false);
  });

  it("the configured default base shows as the default", () => {
    const f = prefillConfirm(fresh("describe", d, repos), { ...confirm, base: "LATEST" }, d.agents);
    expect(f.base).toBe("");
  });
});

describe("the manual form", () => {
  it("takes the overlord's proposal and launches it by its id", () => {
    const f = prefill(fresh("manual", d, repos), proposal, d.agents);
    expect(f.location).toEqual({ kind: "folder", path: "/tmp" });
    expect([f.name, f.branch, f.model, f.workspace]).toEqual(["n", "b", "opus", false]);
    expect(f.context).toBe("c");
    expect(f.relatedNames).toEqual(["alpha"]);
    const l = launch(f);
    if ("error" in l) throw new Error(l.error);
    expect(l.op).toBe("session.new");
    // The background is the daemon's to add: only the id goes.
    expect(l.args).toEqual({
      agent: "claude",
      model: "opus",
      workspace: false,
      name: "n",
      prompt: "p",
      branch: "b",
      proposal: "p1",
      project: "/tmp",
    });
    expect(launch(setLocation(f, { kind: "auto" }))).toEqual({ error: "pick a repo" });
  });

  it("a base counts only with a worktree in a repo", () => {
    let f = setLocation(fresh("manual", d, repos), { kind: "repo", path: "/r" });
    f = { ...f, workspace: true, base: "main", prefer: "remote" };
    const l = launch(f);
    if ("error" in l) throw new Error(l.error);
    expect(l.args).toMatchObject({ base: "main", prefer: "remote", repo: "/r" });
    const off = launch({ ...f, workspace: false });
    if ("error" in off) throw new Error(off.error);
    expect(off.args.base).toBeUndefined();
  });

  it("leaving a plain folder brings the worktree default back", () => {
    const f = setLocation(fresh("manual", d, repos), { kind: "folder", path: "/tmp" });
    expect(f.workspace).toBe(false);
    expect(setLocation(f, { kind: "repo", path: "/r" }).workspace).toBe(true);
  });
});

describe("an agent that can't launch", () => {
  it("says why and what to run", () => {
    const b = agentBlocker(d.agents, "claude");
    expect(b?.why).toContain("isn't installed");
    expect(b?.why).toContain("rlord install-hooks");
    expect(b?.command).toContain("install.sh");
    expect(agentBlocker(d.agents, "opencode")?.why).not.toContain("install-hooks");
    const ok = d.agents.map((a) => ({ ...a, installed: true }));
    expect(agentBlocker(ok, "claude")).toBeNull();
  });
});
