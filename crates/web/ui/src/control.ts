// Who holds a pane, as this screen sees it (decision 3 of slice 5: one
// screen controls a pane, the others view it and may take it over). Pure:
// the pane's view (components/Pane.tsx) draws the control bar from it,
// control.test.ts checks it from snapshots.

import type { Control, Hello } from "./proto";

/** This screen's name as a holder: `device:<id>` for a phone, `page:<id>` for a page. */
export function surfaceOf(h: Pick<Hello, "page" | "device">): string {
  return h.device ? `device:${h.device}` : `page:${h.page}`;
}

/** A snapshot's control, as a key (to tell when it changed). */
export function controlKey(c: Control | null | undefined): string {
  return c === undefined ? "?" : JSON.stringify(c);
}

export type ControlView =
  /** This screen holds the pane: its size, its keys. */
  | { kind: "holder" }
  /**
   * Another screen holds it (or this one may only view): drawn at the
   * window's size, scaled to fit; keys go nowhere.
   */
  | {
      kind: "viewer";
      /** Who holds it ("" when not known). */
      name: string;
      away: boolean;
      /** The window's size, to draw at (null: not known yet). */
      cols: number | null;
      rows: number | null;
      /** Take over is offered (never to a view-only device). */
      canTake: boolean;
    };

export interface ControlFacts {
  /** The snapshot's `control` for the pane's session (undefined: not in it). */
  control: Control | null | undefined;
  /** This screen (`surfaceOf`). */
  me: string | null;
  /** The open's answer: this screen holds it (null: the host didn't say). */
  opened: boolean | null;
  /** This screen may type (the device isn't view-only). */
  input: boolean;
  /** The window's size from the open's answer. */
  window: { cols: number; rows: number } | null;
  /**
   * `controlKey` of the snapshot's control when this screen took over:
   * while the snapshot still says that, the take counts.
   */
  took: string | null;
}

/** Holder or viewer, and what the bar says. */
export function controlView(f: ControlFacts): ControlView {
  const c = f.control;
  const viewer = (name: string, away: boolean, canTake: boolean): ControlView => ({
    kind: "viewer",
    name,
    away,
    cols: c?.cols ?? f.window?.cols ?? null,
    rows: c?.rows ?? f.window?.rows ?? null,
    canTake,
  });
  if (!f.input) return viewer(c?.name ?? "", c?.away ?? false, false);
  if (f.took !== null && f.took === controlKey(c)) return { kind: "holder" };
  if (c) {
    if (f.me !== null && c.holder === f.me) return { kind: "holder" };
    return viewer(c.name, c.away, true);
  }
  // Nobody holds it by the snapshot (it's behind, or doesn't say): the
  // open's answer decides; a free pane went to its opener.
  return f.opened === false ? viewer("", false, true) : { kind: "holder" };
}

/** The control bar's words for a viewer. */
export function controlWords(v: Extract<ControlView, { kind: "viewer" }>): string {
  if (!v.canTake && !v.name) return "View only";
  const who = v.name ? `Controlled by ${v.name}` : "Controlled by another screen";
  const line = v.away ? `${who} (away)` : who;
  return v.canTake ? line : `${line} · view only`;
}
