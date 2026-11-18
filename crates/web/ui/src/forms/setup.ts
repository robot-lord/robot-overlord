// The code-folder form's rules, as the TUI has them (crates/tui/src/
// setupform.rs): the folder (rlord's suggestion, its alternatives, or
// one typed), what the repo finder scans, and what saving does. With a
// description (and not a form the overlord reopened), the overlord is
// asked; otherwise rlord saves by plain code. Pure: forms/Setup.tsx draws
// it.
//
// The browser can't look at the disk: a folder that doesn't exist, or the
// home folder, is refused by the daemon's save with a message that asks;
// `askFor` turns that into the form's question, and the answer goes back
// with `create` or `allow_home`.

import type { FormSetup, SetupPrefill } from "../proto";

/** What the repo finder scans, as the form will save it. */
export type Roots =
  | { kind: "follow" }
  | { kind: "keep"; list: string[] }
  | { kind: "set"; list: string[] };

export interface SetupForm {
  /** The description for the overlord; null when there is no overlord. */
  describe: string | null;
  codeDir: string;
  codeDirChosen: boolean;
  /** rlord's suggestion, then alternatives. */
  choices: string[];
  roots: Roots;
  /** `repo_roots` as configured. */
  configured: string[];
  /** The overlord's proposed roots, kept to come back to. */
  proposedRoots: string[] | null;
  rootsFromDefault: boolean;
  /** The folder as configured now ("" when unset). */
  current: string;
  /** Reopened by the overlord (`confirm_setup`). */
  proposal: boolean;
  /** Reopened, description untouched since: saving saves what is shown. */
  confirmed: boolean;
  changed: string[];
  note: string | null;
  /** The folder the user agreed to create, or to use though it is home. */
  confirmCreate: string | null;
  confirmHome: string | null;
}

/** The form for a `form.setup` answer. `overlord`: one exists. */
export function fromSetup(a: FormSetup, overlord: boolean): SetupForm {
  const s = a.suggest;
  const choices = [s.suggestion, ...s.alternatives]
    .map((c) => (c && typeof c.path === "string" ? c.path : ""))
    .filter((p) => p);
  const configured = s.roots.configured.map((r) => r.trim()).filter((r) => r);
  const current = a.status.code_dir || s.code_dir || "";
  const follow = configured.length === 0 || s.roots_default === "follow";
  return {
    describe: overlord ? "" : null,
    codeDir: current || choices[0] || "",
    codeDirChosen: false,
    choices,
    roots: follow ? { kind: "follow" } : { kind: "keep", list: configured },
    configured,
    proposedRoots: null,
    rootsFromDefault: configured.length > 0 && s.roots_default === "follow",
    current,
    proposal: false,
    confirmed: false,
    changed: [],
    note: null,
    confirmCreate: null,
    confirmHome: null,
  };
}

/** The overlord's proposal (`confirm_setup`). */
export function prefill(f: SetupForm, p: SetupPrefill): SetupForm {
  const out: SetupForm = { ...f, proposal: true, confirmed: true };
  if (p.code_dir) out.codeDir = p.code_dir;
  if (p.repo_roots) {
    out.roots =
      p.repo_roots.length === 0 ? { kind: "follow" } : { kind: "set", list: p.repo_roots };
    out.proposedRoots = p.repo_roots.length > 0 ? p.repo_roots : null;
    out.rootsFromDefault = false;
  } else {
    out.roots =
      f.configured.length === 0 ? { kind: "follow" } : { kind: "keep", list: f.configured };
  }
  if (p.description !== undefined) out.describe = p.description;
  out.changed = p.changed ?? [];
  out.note = p.note?.trim() || null;
  return out;
}

/** The finder's choices the form offers: the overlord's, only the folder, the configured. */
export function rootsOptions(f: SetupForm): Roots[] {
  const v: Roots[] = [];
  if (f.proposedRoots) v.push({ kind: "set", list: f.proposedRoots });
  v.push({ kind: "follow" });
  if (f.configured.length > 0) v.push({ kind: "keep", list: f.configured });
  return v;
}

/** What a roots choice says. */
export function rootsLabel(r: Roots): string {
  switch (r.kind) {
    case "follow":
      return "only the code folder";
    case "keep":
      return `the code folder and ${r.list.join(", ")} (as configured)`;
    case "set":
      return `the code folder and ${r.list.join(", ")}`;
  }
}

/** The description changed: the overlord's confirmation no longer holds. */
export function described(f: SetupForm, text: string): SetupForm {
  return { ...f, describe: text, confirmed: false, changed: [], note: null };
}

/** The folder, as the user typed or picked it. */
export function setFolder(f: SetupForm, dir: string): SetupForm {
  return { ...f, codeDir: dir, codeDirChosen: true, confirmCreate: null, confirmHome: null };
}

/** Whether Save goes to the overlord: a description, not yet confirmed. */
export function describing(f: SetupForm): boolean {
  return Boolean(f.describe?.trim()) && !f.confirmed;
}

export type SaveCall =
  | { op: "overlord.setup_request"; args: Record<string, unknown> }
  | { op: "form.setup_save"; args: Record<string, unknown> }
  | { error: string };

/** What Save sends (`save_call`). */
export function saveCall(f: SetupForm, loading = false): SaveCall {
  const dir = f.codeDir.trim();
  if (describing(f)) {
    let source = "suggested by rlord";
    if (!dir) source = "not set";
    else if (f.codeDirChosen) source = "picked by the user";
    else if (dir === f.current) source = "current setting";
    else if (f.proposal) source = "proposed by the overlord";
    const [roots, rootsSource] =
      f.roots.kind === "follow"
        ? ["follow", f.rootsFromDefault ? "default" : "user"]
        : [f.roots.list, f.roots.kind === "keep" ? "config" : "user"];
    return {
      op: "overlord.setup_request",
      args: {
        description: f.describe?.trim() ?? "",
        code_dir: dir,
        code_dir_source: source,
        roots,
        roots_source: rootsSource,
      },
    };
  }
  if (!dir) {
    return {
      error: loading
        ? "still finding a suggestion: type a folder, or wait a moment"
        : "type the folder you keep your repos in",
    };
  }
  const roots =
    f.roots.kind === "follow" ? "follow" : f.roots.kind === "keep" ? "leave" : f.roots.list;
  const args: Record<string, unknown> = { code_dir: dir, roots };
  if (f.confirmCreate === dir) args.create = true;
  if (f.confirmHome === dir) args.allow_home = true;
  return { op: "form.setup_save", args };
}

/** The question a refused save asks the user, if it asks one. */
export function askFor(message: string): "create" | "home" | null {
  if (message.includes("(create it?)")) return "create";
  if (message.includes("whole home folder")) return "home";
  return null;
}
