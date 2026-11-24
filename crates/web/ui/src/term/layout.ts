// The terminal's sizing on a phone, pure: font steps kept per device, the
// rows laid out for the whole view (the keyboard never resizes a pane),
// a viewer's scale, and a vertical drag split between sliding the
// terminal and wheel reports. term.ts applies them; layout.test.ts checks.

export const FONT_MIN = 10;
export const FONT_MAX = 16;
/** About 52 columns on a 390-point phone (gate 1c). */
export const FONT_DEFAULT = 12;
const FONT_KEY = "rlord.termFont";

type FontStorage = Pick<Storage, "getItem" | "setItem">;

/** The device's font size, or the default. */
export function loadFont(storage: FontStorage | null): number {
  try {
    const n = Number(storage?.getItem(FONT_KEY));
    return Number.isInteger(n) && n >= FONT_MIN && n <= FONT_MAX ? n : FONT_DEFAULT;
  } catch {
    return FONT_DEFAULT;
  }
}

export function saveFont(storage: FontStorage | null, px: number): void {
  try {
    storage?.setItem(FONT_KEY, String(px));
  } catch {
    // Private mode: this session only.
  }
}

/** One step smaller or larger, within 10 to 16 px. */
export function fontStep(px: number, by: -1 | 1): number {
  return Math.min(FONT_MAX, Math.max(FONT_MIN, px + by));
}

/**
 * The height the terminal is laid out for: its box as it would be with
 * the keyboard down. The app sits over the visual viewport, so the
 * keyboard takes `inner - visible` from the box.
 */
export function fullHeight(box: number, inner: number, visible: number): number {
  return Math.round(box + Math.max(0, inner - visible));
}

/** A viewer's scale: the window's whole screen in the box, never enlarged. */
export function fitScale(boxW: number, boxH: number, w: number, h: number): number {
  if (w <= 0 || h <= 0 || boxW <= 0 || boxH <= 0) return 1;
  return Math.min(1, boxW / w, boxH / h);
}

/**
 * A vertical drag on the terminal. While the keyboard hides its top
 * (`maxPan` > 0) a drag first slides it (`pan`, 0 is the bottom rows
 * showing); past that edge each row's height of drag is one wheel report.
 * Down (`dy` > 0) shows the top, then scrolls back (wheel up).
 */
export class Drag {
  pan = 0;
  private acc = 0;

  /** The new pan, and the wheel reports to send (> 0 up, < 0 down). */
  move(dy: number, maxPan: number, cell: number): { pan: number; wheel: number } {
    const max = Math.max(0, maxPan);
    this.pan = Math.min(this.pan, max);
    let rest = dy;
    if (rest > 0 && this.pan < max) {
      const take = Math.min(rest, max - this.pan);
      this.pan += take;
      rest -= take;
    } else if (rest < 0 && this.pan > 0) {
      const take = Math.min(-rest, this.pan);
      this.pan -= take;
      rest += take;
    }
    if (rest === 0 || cell <= 0) return { pan: this.pan, wheel: 0 };
    // (A change of direction starts the count again.)
    if (Math.sign(rest) !== Math.sign(this.acc)) this.acc = 0;
    this.acc += rest;
    // (`|| 0`: no -0 for a part row.)
    const rows = Math.trunc(this.acc / cell) || 0;
    this.acc -= rows * cell;
    return { pan: this.pan, wheel: rows };
  }

  /** The finger lifted: a part row is dropped. */
  end(): void {
    this.acc = 0;
  }
}
