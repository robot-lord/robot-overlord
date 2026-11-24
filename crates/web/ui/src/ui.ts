// The overlord's UI commands on this page, and what the page tells the
// daemon it shows. Pure: main.tsx carries the effects out.
//
// A command lands here when this page is the screen the user used last
// (crates/daemon/src/ui.rs `target`). As in the TUI, a form command never
// replaces a form the user has typed into: it is parked, and a banner
// offers to open it.

import type { ListView, SessionBase, UiCmd } from "./proto";
import type { Route } from "./route";
import { type Caps, LOCAL_CAPS } from "./transport";

/** The forms a page can have open (`page.report`'s `form`). */
export type FormKind = "describe" | "confirm" | "manual" | "fork" | "setup" | "keys" | "devices";

/** The open form: its kind, the command that opened it, and whether the user typed into it. */
export interface OpenForm {
  kind: FormKind;
  cmd: UiCmd | null;
  touched: boolean;
  /** The session a fork form forks. */
  session?: SessionBase;
}

/** What to do for a command. */
export type Effect =
  | { do: "route"; to: Route; closeForm: boolean }
  | { do: "view"; view: ListView; select: string | null }
  | { do: "open"; form: OpenForm }
  | { do: "park"; cmd: UiCmd }
  | { do: "toast"; text: string };

/** The form a command opens, if it opens one. */
export function formOf(cmd: UiCmd): FormKind | null {
  switch (cmd.cmd) {
    case "new_session":
      return "manual";
    case "confirm_new_session":
      return "confirm";
    case "setup":
      return "setup";
    case "keys":
      return "keys";
    case "goto":
    case "show_list":
    case "set_view":
    case "notice":
      return null;
  }
}

/**
 * What the page does for `cmd`, given the form open now. A form this
 * connection can't use (a phone has no code-folder form or key screen;
 * the daemon never sends one there) is only a notice.
 */
export function decide(cmd: UiCmd, open: OpenForm | null, caps: Caps = LOCAL_CAPS): Effect[] {
  if (cmd.cmd === "setup" && !caps.setup) {
    return [
      { do: "toast", text: "The overlord opened the code-folder form: it opens on your computer." },
    ];
  }
  if (cmd.cmd === "keys" && !caps.keys) {
    return [
      { do: "toast", text: "The overlord opened the key screen: it opens on your computer." },
    ];
  }
  switch (cmd.cmd) {
    case "goto":
      return [
        {
          do: "route",
          to: cmd.overlord ? { kind: "overlord" } : { kind: "session", id: cmd.id },
          closeForm: false,
        },
      ];
    case "show_list":
      return [{ do: "route", to: { kind: "list" }, closeForm: !open?.touched }];
    case "set_view":
      return [{ do: "view", view: cmd.view, select: cmd.select ?? null }];
    case "notice":
      return [{ do: "toast", text: cmd.text }];
    case "new_session":
    case "confirm_new_session":
    case "setup":
    case "keys": {
      const kind = formOf(cmd);
      if (!kind) return [];
      if (open?.touched) return [{ do: "park", cmd }];
      return [{ do: "open", form: { kind, cmd, touched: false } }];
    }
  }
}

/** The route as `page.report` sends it. */
export function routeReport(r: Route): { kind: Route["kind"]; id?: string } {
  return r.kind === "session" ? { kind: r.kind, id: r.id } : { kind: r.kind };
}

/** What a page reports it shows. */
export interface Shown {
  route: Route;
  view: ListView;
  visible: boolean;
  form: FormKind | null;
}

/** Input is reported at most this often. */
export const INPUT_EVERY_MS = 5000;

/**
 * Sends `page.report`: what changed of what the page shows, and that the
 * user did something (pointer or key input, at most every 5 s). `reset`
 * after a reconnect: the next `shown` sends everything.
 */
export class Reporter {
  private last: Record<string, string> | null = null;
  private lastInput = Number.NEGATIVE_INFINITY;
  private readonly send: (args: Record<string, unknown>) => void;
  private readonly now: () => number;

  constructor(send: (args: Record<string, unknown>) => void, now: () => number = Date.now) {
    this.send = send;
    this.now = now;
  }

  reset(): void {
    this.last = null;
    this.lastInput = Number.NEGATIVE_INFINITY;
  }

  shown(s: Shown): void {
    const fields: Record<string, unknown> = {
      route: routeReport(s.route),
      view: s.view,
      visible: s.visible,
      form: s.form,
    };
    const out: Record<string, unknown> = {};
    const seen: Record<string, string> = {};
    for (const [k, v] of Object.entries(fields)) {
      const j = JSON.stringify(v);
      seen[k] = j;
      if (this.last?.[k] !== j) out[k] = v;
    }
    this.last = seen;
    if (Object.keys(out).length > 0) this.send(out);
  }

  input(): void {
    const t = this.now();
    if (t - this.lastInput < INPUT_EVERY_MS) return;
    this.lastInput = t;
    this.send({ input: true });
  }
}
