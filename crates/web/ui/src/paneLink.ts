// What a terminal (term.ts) needs of a pane's connection, whichever it
// is: the local page's pane socket (pane.ts) or a stream over the phone's
// bridge (remotePane.ts). Types and one constant only: both bundles take
// this file, and neither gets the other's link.

/** A pane on screen, as its link reports it when opened. */
export interface PaneOpen {
  kind: "open";
  /** The session shown (the overlord's id for `overlord`). */
  id: string | null;
  name: string | null;
  /**
   * This screen holds the pane (null: the host didn't say, as before
   * control existed: it does). The snapshot's `control` says it later.
   */
  control: boolean | null;
  /** This screen may type (false: a view-only device). */
  input: boolean;
  /** The size the view is attached at. */
  cols: number | null;
  rows: number | null;
  /** The window's size when opened. */
  window: { cols: number; rows: number } | null;
}

export type PaneState =
  | { kind: "connecting" }
  | PaneOpen
  /** It isn't running: the view offers Resume. */
  | { kind: "not_running" }
  /** No such session (or no overlord yet). */
  | { kind: "not_found"; message: string }
  /** Closed on purpose while the tab is hidden. */
  | { kind: "hidden" }
  | { kind: "unauthorized" }
  /** Lost or refused: retrying (or, on the phone, waiting for the computer). */
  | { kind: "retrying"; message: string };

/** What a link needs of the terminal (a fake in tests). */
export interface Screen {
  readonly cols: number;
  readonly rows: number;
  reset(): void;
  /** Draw `bytes`; `parsed` runs once the terminal has taken them in. */
  write(bytes: Uint8Array, parsed?: () => void): void;
}

/**
 * A pane's connection, whichever it is: the local socket or a stream over
 * the phone's bridge. Knows nothing of xterm.js: it writes to a `Screen`
 * and is fed input by the terminal.
 */
export interface PaneLink {
  readonly state: PaneState;
  /** Open now (again), whatever the state: after Resume, or a change. */
  connect(): void;
  /** Bytes typed into the terminal. */
  input(data: string | Uint8Array<ArrayBuffer>): void;
  /** The terminal's new size (only the holder's matters). */
  resize(cols: number, rows: number): void;
  /** The tab or app was hidden or shown. */
  visible(v: boolean): void;
  /** For good (the view went away). */
  stop(): void;
}

/** Builds a link onto a terminal's screen (the entry decides which kind). */
export type MakeLink = (screen: Screen, onState: (s: PaneState) => void) => PaneLink;

/** How long a hidden tab keeps its pane open. */
export const HIDE_CLOSE_MS = 3000;
