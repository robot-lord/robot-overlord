// The new-session forms' rules, as the TUI has them (crates/tui/src/
// newform.rs): which hints are the user's own picks (`chosen`), which the
// overlord set (`resolved`), the rest defaults; the request the describe
// form sends; the launch the manual form sends; and how a proposal or a
// reopened form fills them in. Pure: forms/NewSession.tsx draws them.
//
// Drift from the TUI shows up as a wrong label, not a wrong launch: the
// daemon checks every launch argument again.

import type {
  AgentChoice,
  ConfirmPrefill,
  FormDefaults,
  FormRepos,
  NewPrefill,
  Op,
} from "../proto";

/** The hints the describe form sends, by the names the daemon uses. */
export type Hint = "repo" | "agent" | "model" | "worktree" | "base" | "tags";
export const HINTS: Hint[] = ["repo", "agent", "model", "worktree", "base", "tags"];

/** Where the session runs: a repo, a plain folder, or (describe) the overlord's pick. */
export type Location =
  | { kind: "repo"; path: string }
  | { kind: "folder"; path: string }
  | { kind: "auto" };

export interface NewForm {
  /** `describe` goes to the overlord; `manual` launches. */
  kind: "describe" | "manual";
  location: Location;
  name: string;
  agent: string;
  model: string;
  workspace: boolean;
  branch: string;
  /** Where a new branch starts: "" is the configured default (`defaultBase`). */
  base: string;
  /** With a base that had diverged: the side chosen (`local`, `remote`). */
  prefer: string;
  /** `[session] base`: "latest" or "head". */
  defaultBase: string;
  defaultWorkspace: boolean;
  /** The description (describe) or the first prompt (manual). */
  prompt: string;
  /** Space-separated tags (describe). */
  tags: string;
  chosen: Hint[];
  resolved: Hint[];
  /** Opened by the overlord (`confirm_new_session`). */
  reopened: boolean;
  /** Sending confirms the overlord's hints as they are. */
  confirmed: boolean;
  changed: string[];
  note: string | null;
  /** A session the overlord proposed: its id, and its background to show. */
  proposal: string | null;
  context: string;
  relatedNames: string[];
}

/** A fresh form, from the forms' defaults and the repo finder. */
export function fresh(
  kind: NewForm["kind"],
  d: Pick<FormDefaults, "agent" | "agents" | "base" | "workspace">,
  repos: Pick<FormRepos, "repos"> | null,
  group: string | null = null,
): NewForm {
  const list = repos?.repos ?? [];
  const def = list.find((r) => r.default) ?? list[0];
  const f: NewForm = {
    kind,
    location: def ? { kind: "repo", path: def.path } : { kind: "auto" },
    name: "",
    agent: "",
    model: "",
    workspace: d.workspace,
    branch: "",
    base: "",
    prefer: "",
    defaultBase: d.base.trim().toLowerCase() === "head" ? "head" : "latest",
    defaultWorkspace: d.workspace,
    prompt: "",
    // Opened on a group: the new session joins it, the user's choice.
    tags: group ?? "",
    chosen: group ? ["tags"] : [],
    resolved: [],
    reopened: false,
    confirmed: false,
    changed: [],
    note: null,
    proposal: null,
    context: "",
    relatedNames: [],
  };
  return setAgent(f, d.agent, d.agents);
}

/** An agent's choices: its default model first, then the rest. */
export function modelsOf(agents: AgentChoice[], name: string): string[] {
  const a = agents.find((x) => x.name === name);
  if (!a) return [];
  return a.model && !a.models.includes(a.model) ? [a.model, ...a.models] : a.models;
}

/**
 * Another agent: the model becomes its default (one agent's model means
 * nothing to another). The same agent again changes nothing.
 */
export function setAgent(f: NewForm, name: string, agents: AgentChoice[]): NewForm {
  if (f.agent === name) return f;
  const a = agents.find((x) => x.name === name);
  return {
    ...f,
    agent: name,
    model: a?.model ?? "",
    chosen: f.chosen.filter((h) => h !== "model"),
    resolved: f.resolved.filter((h) => h !== "model"),
  };
}

/**
 * The user picked hint `h` (`changed`: its value is new). On a fresh form
 * any pick is theirs (pinning the default counts); on one the overlord
 * reopened only a real change is, and that drops the overlord's label and
 * its confirmation.
 */
export function picked(f: NewForm, h: Hint, changed: boolean): NewForm {
  let out = f;
  if ((changed || !f.reopened) && !f.chosen.includes(h)) {
    out = { ...out, chosen: [...out.chosen, h] };
  }
  if (changed) {
    out = { ...out, resolved: out.resolved.filter((x) => x !== h) };
    out = described(out);
  }
  return out;
}

/** The description or a hint changed: the overlord's confirmation no longer holds. */
export function described(f: NewForm): NewForm {
  return f.confirmed ? { ...f, confirmed: false, changed: [], note: null } : f;
}

/** Whose a hint's value is, as the form labels it. */
export function owner(f: NewForm, h: Hint): "yours" | "overlord's" | "default" {
  if (f.chosen.includes(h)) return "yours";
  if (f.resolved.includes(h)) return "overlord's";
  return "default";
}

/** A new location; a plain folder has no worktree. */
export function setLocation(f: NewForm, loc: Location): NewForm {
  const wasFolder = f.location.kind === "folder";
  let workspace = f.workspace;
  if (loc.kind === "folder") workspace = false;
  else if (wasFolder && !f.chosen.includes("worktree")) workspace = f.defaultWorkspace;
  return { ...f, location: loc, workspace };
}

/** The base typed, when it counts: a worktree on, not a plain folder. */
export function typedBase(f: NewForm): string | null {
  const b = f.base.trim();
  return b && f.workspace && f.location.kind !== "folder" ? b : null;
}

/** Tags as typed: words, a leading `#` dropped. */
export function tagList(text: string): string[] {
  return text
    .split(/\s+/)
    .map((t) => t.replace(/^#+/, ""))
    .filter((t) => t);
}

/** One request to send. */
export interface Send {
  op: Op;
  args: Record<string, unknown>;
}

/**
 * The describe form's request to the overlord (`submit_request`): the
 * description, every hint with whose it is, and `confirmed` when the user
 * sends back the overlord's corrected hints as they are.
 */
export function request(f: NewForm): Send | { error: string } {
  if (!f.prompt.trim()) return { error: "describe what the session should do first" };
  const tags = tagList(f.tags);
  const chosen = HINTS.filter((h) => f.chosen.includes(h) && (h !== "tags" || tags.length > 0));
  const base = typedBase(f);
  const args: Record<string, unknown> = {
    description: f.prompt,
    agent: f.agent,
    model: f.model,
    workspace: f.workspace,
    // Empty: the configured default, sent as one (never over `[session] base`).
    base: base ?? f.defaultBase,
    tags,
    chosen,
    resolved: f.resolved,
    confirmed: f.confirmed,
  };
  if (base && f.prefer) args.prefer = f.prefer;
  if (f.location.kind === "repo") args.repo = f.location.path;
  if (f.location.kind === "folder") {
    args.project = f.location.path;
    args.workspace = false;
  }
  return { op: "overlord.new_session", args };
}

/**
 * The manual form's launch (`submit_manual`). A proposed session names its
 * proposal: the daemon adds the overlord's background.
 */
export function launch(f: NewForm): Send | { error: string } {
  const args: Record<string, unknown> = {
    agent: f.agent,
    model: f.model,
    workspace: f.workspace,
  };
  if (f.name.trim()) args.name = f.name.trim();
  if (f.prompt.trim()) args.prompt = f.prompt.trim();
  if (f.branch.trim()) args.branch = f.branch.trim();
  const base = typedBase(f);
  if (base) {
    args.base = base;
    if (f.prefer) args.prefer = f.prefer;
  }
  if (f.proposal) args.proposal = f.proposal;
  switch (f.location.kind) {
    case "repo":
      args.repo = f.location.path;
      break;
    case "folder":
      args.project = f.location.path;
      args.workspace = false;
      break;
    case "auto":
      return { error: "pick a repo" };
  }
  return { op: "session.new", args };
}

/** The overlord's proposal (`propose_session`), on a manual form. */
export function prefill(f: NewForm, p: NewPrefill, agents: AgentChoice[]): NewForm {
  let out: NewForm = { ...f };
  if (p.repo) out = setLocation(out, { kind: "repo", path: p.repo });
  else if (p.project) out = setLocation(out, { kind: "folder", path: p.project });
  if (p.name) out.name = p.name;
  if (p.branch) out.branch = p.branch;
  if (p.agent) out = setAgent(out, p.agent, agents);
  if (p.model) out.model = p.model;
  if (p.workspace !== undefined) out.workspace = p.workspace && out.location.kind === "repo";
  if (p.base) out.base = p.base;
  if (p.prefer) out.prefer = p.prefer;
  if (p.prompt) out.prompt = p.prompt;
  out.proposal = p.proposal;
  out.context = p.context ?? "";
  out.relatedNames = p.related_names ?? p.related ?? [];
  return out;
}

function trimmed(v: string | undefined): string {
  return (v ?? "").trim();
}

/**
 * The describe form reopened with the overlord's hints
 * (`prefill_confirm`): the user's description, the hints as it would
 * launch them, whose each is (worked out by the daemon), its note.
 * Sending confirms them.
 */
export function prefillConfirm(f: NewForm, p: ConfirmPrefill, agents: AgentChoice[]): NewForm {
  let out: NewForm = { ...f, kind: "describe" };
  const s = trimmed;
  if (s(p.description)) out.prompt = s(p.description);
  if (s(p.repo)) out = setLocation(out, { kind: "repo", path: s(p.repo) });
  else if (s(p.project)) out = setLocation(out, { kind: "folder", path: s(p.project) });
  else out = setLocation(out, { kind: "auto" });
  if (s(p.agent)) out = setAgent(out, s(p.agent), agents);
  // "" is the agent's default (the overlord reset the model).
  if (p.model !== undefined) out.model = p.model.trim();
  if (p.workspace !== undefined) out.workspace = p.workspace && out.location.kind !== "folder";
  out.prefer = s(p.prefer);
  // The configured default is shown as such, not as a pick.
  const b = s(p.base);
  out.base = b.toLowerCase() === out.defaultBase ? "" : b;
  out.tags = (p.tags ?? []).join(" ");
  out.chosen = HINTS.filter((h) => p.chosen.includes(h));
  out.resolved = HINTS.filter((h) => p.resolved.includes(h));
  out.reopened = true;
  out.confirmed = true;
  out.changed = p.changed ?? [];
  out.note = s(p.note) || null;
  return out;
}

/** Why the agent can't launch, and the install command to suggest (rlord never runs it). */
export function agentBlocker(
  agents: AgentChoice[],
  name: string,
): { why: string; command: string | null } | null {
  const a = agents.find((x) => x.name === name);
  if (!a) return null;
  const command = a.install?.command ?? null;
  if (!a.supported) {
    const have = a.installed ? "" : "isn't installed, and ";
    return { why: `${a.label} ${have}can't run rlord sessions yet (support coming)`, command };
  }
  if (a.too_old) return { why: `${a.too_old}. Upgrade it yourself:`, command };
  if (!a.installed) {
    return {
      why:
        a.hooks === false
          ? `${a.label} isn't installed. Install it yourself:`
          : `${a.label} isn't installed. Install it yourself, then \`rlord install-hooks\`:`,
      command,
    };
  }
  return null;
}
