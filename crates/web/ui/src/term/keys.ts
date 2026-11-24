// What the phone's key row and touch gestures send to a pane, as bytes
// (strings of byte values below 128). Pure: the key row (components/KeyRow.tsx)
// and the terminal (term.ts) call these; keys.test.ts checks them.

/** The key row's keys that send bytes. */
export type Key =
  | "esc"
  | "tab"
  /** ⇧Tab: `ESC [ Z` (Claude Code's mode switch). */
  | "backtab"
  | "up"
  | "down"
  | "right"
  | "left"
  | "enter"
  /** ^C, whatever the sticky modifiers. */
  | "ctrl_c";

export interface Mods {
  ctrl: boolean;
  alt: boolean;
}

export interface KeyOpts extends Mods {
  /** The pane asked for application cursor keys (DECCKM). */
  appCursor: boolean;
}

const ESC = "\x1b";

const ARROW: Record<"up" | "down" | "right" | "left", string> = {
  up: "A",
  down: "B",
  right: "C",
  left: "D",
};

/** The bytes for a key of the key row. */
export function keyBytes(key: Key, o: KeyOpts): string {
  switch (key) {
    case "ctrl_c":
      return "\x03";
    case "backtab":
      // (A modified ⇧Tab has no common encoding: the modifiers are left out.)
      return `${ESC}[Z`;
    case "up":
    case "down":
    case "right":
    case "left": {
      const f = ARROW[key];
      // xterm's modifier parameter: 1 + (alt ? 2 : 0) + (ctrl ? 4 : 0).
      const m = 1 + (o.alt ? 2 : 0) + (o.ctrl ? 4 : 0);
      if (m > 1) return `${ESC}[1;${m}${f}`;
      return o.appCursor ? `${ESC}O${f}` : `${ESC}[${f}`;
    }
    case "esc":
      return o.alt ? ESC + ESC : ESC;
    case "tab":
      return o.alt ? `${ESC}\t` : "\t";
    case "enter":
      return o.alt ? `${ESC}\r` : "\r";
  }
}

/** Control characters for the keys that have one besides the letters. */
const CTRL_OTHER: Record<string, string> = {
  "@": "\x00",
  " ": "\x00",
  "2": "\x00",
  "[": "\x1b",
  "3": "\x1b",
  "\\": "\x1c",
  "4": "\x1c",
  "]": "\x1d",
  "5": "\x1d",
  "^": "\x1e",
  "6": "\x1e",
  _: "\x1f",
  "-": "\x1f",
  "7": "\x1f",
  "?": "\x7f",
  "8": "\x7f",
};

/**
 * `data` with Ctrl held: its first character as a control character (a
 * letter, or one of `@[\]^_?` and the digits a terminal maps); anything
 * else unchanged.
 */
export function withCtrl(data: string): string {
  const c = data[0];
  if (c === undefined) return data;
  const rest = data.slice(1);
  const code = c.charCodeAt(0);
  if ((code >= 0x41 && code <= 0x5a) || (code >= 0x61 && code <= 0x7a)) {
    return String.fromCharCode(code & 0x1f) + rest;
  }
  const other = CTRL_OTHER[c];
  return other === undefined ? data : other + rest;
}

/** What the keyboard typed, with the key row's sticky modifiers applied. */
export function applyMods(data: string, m: Mods): string {
  const d = m.ctrl ? withCtrl(data) : data;
  return m.alt && d.length > 0 ? ESC + d : d;
}

/**
 * One SGR mouse wheel report at a cell (1-based): 64 is up (back into the
 * history), 65 down.
 */
export function wheel(up: boolean, col: number, row: number): string {
  const c = Math.max(1, Math.round(col));
  const r = Math.max(1, Math.round(row));
  return `${ESC}[<${up ? 64 : 65};${c};${r}M`;
}

/** The longest paste the page sends (the local pane socket's frame limit). */
export const PASTE_MAX = 64 * 1024;

/** Whether `text` is too long to paste (UTF-8 bytes over `PASTE_MAX`). */
export function tooLong(text: string): boolean {
  // (Cheap first: every UTF-16 unit is at most 3 bytes of UTF-8.)
  if (text.length * 3 <= PASTE_MAX) return false;
  return new TextEncoder().encode(text).length > PASTE_MAX;
}
