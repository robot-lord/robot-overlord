// A pane's terminal: xterm.js drawing whichever link it is given (the
// local page's pane socket, pane.ts; the phone's bridge stream,
// remotePane.ts). Both bundles take this file; neither gets the other's
// link.
//
// - WebGL draws it. A lost context (iOS takes them from a backgrounded
//   app) gets a new WebGL addon, never the DOM renderer: the phone page's
//   CSP refuses the DOM renderer's inline styles.
// - As the pane's holder it is sized to its box; as a viewer it is drawn
//   at the window's size and scaled down to fit (components/Pane.tsx
//   decides which, from control.ts).
// - On the phone the keyboard never resizes it: its rows are laid out for
//   the whole view with the keyboard down; with the keyboard up it slides
//   (its bottom rows stay above the key row) and a drag shows the top. A
//   new size goes out only on a rotation or a font-size change, 200 ms
//   after it settles. A drag past the top is the wheel (SGR reports), when
//   the pane asked for the mouse.

import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import type { ControlView } from "./control";
import type { MakeLink, PaneLink, PaneState } from "./paneLink";
import { applyMods, type Mods, tooLong, wheel } from "./term/keys";
import { Drag, fitScale, fullHeight } from "./term/layout";

/** The terminal's look. */
const THEME = {
  background: "#0b0b0d",
  foreground: "#d8d8dc",
  cursor: "#c79af2",
  selectionBackground: "#4a3d5c",
};

/** The phone's font (gate 1c: Claude Code's glyphs are all there). */
const PHONE_FONT = "ui-monospace, Menlo, monospace";
const DESKTOP_FONT = 'Menlo, "SF Mono", "DejaVu Sans Mono", monospace';
/** The box's inset around the terminal (app.css `.term`). */
const INSET = 4;
/** A rotation or font change settles this long before a new size goes out. */
export const SETTLE_MS = 200;
/** New WebGL contexts at most this often in a row before giving up. */
const GL_RETRIES = 5;

export interface TermOpts {
  makeLink: MakeLink;
  onState: (s: PaneState) => void;
  /** The phone's layout and font size (px); none: the desktop's. */
  phone?: { fontSize: number } | undefined;
  /** A key was typed while viewing: point at Take over. */
  onBlocked?: (() => void) | undefined;
  /** The key row's sticky Ctrl and Alt changed (used up by a key). */
  onMods?: ((m: Mods) => void) | undefined;
  /** Milliseconds from the last key sent to the next output drawn (the debug read-out). */
  onEcho?: ((ms: number) => void) | undefined;
  /** The screen's text, after output (debug builds: a hidden mirror for UI tests). */
  mirror?: ((text: string) => void) | undefined;
}

export interface TermHandle {
  readonly link: PaneLink;
  focus(): void;
  /** Hide the keyboard. */
  blur(): void;
  /** Bytes from the key row, as the holder's input. False: not sent (viewing). */
  send(data: string): boolean;
  /**
   * Text as one paste (bracketed when the pane asked), then Enter if
   * `enter`. Refused (false) over 64 KiB or while viewing.
   */
  paste(text: string, enter: boolean): boolean;
  /** The sticky modifiers for the next key typed. */
  setMods(m: Mods): void;
  /** Holder or viewer (control.ts). */
  setMode(v: ControlView): void;
  /** The phone's font size, then a new size after it settles. */
  setFont(px: number): void;
  /** The size this terminal would hold the pane at (for Take over). */
  fitSize(): { cols: number; rows: number };
  /** The pane asked for application cursor keys. */
  appCursor(): boolean;
  /** The text on screen, one line per row. */
  text(): string;
  /** A view-only device: no keyboard at all. */
  setReadOnly(ro: boolean): void;
  dispose(): void;
}

/** A mouse report or focus report, which a viewer drops silently. */
function notTyping(d: string): boolean {
  return d.startsWith("\x1b[<") || d.startsWith("\x1b[M") || d === "\x1b[I" || d === "\x1b[O";
}

/** A terminal in `el` (inside its box, `el.parentElement`) on the link `opts.makeLink` builds. */
export function mountTerm(el: HTMLElement, opts: TermOpts): TermHandle {
  const phone = opts.phone ?? null;
  const box = el.parentElement ?? el;
  const term = new Terminal({
    cursorBlink: true,
    fontFamily: phone ? PHONE_FONT : DESKTOP_FONT,
    fontSize: phone ? phone.fontSize : 13,
    // tmux keeps the history (copy mode, by the wheel); the page draws
    // only the screen.
    scrollback: 0,
    // Option is Meta (Option-D and friends reach the agent); Option-click
    // selects text past tmux's mouse mode.
    macOptionIsMeta: true,
    macOptionClickForcesSelection: true,
    theme: THEME,
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.open(el);

  let disposed = false;
  let glFails = 0;
  const loadGl = (): void => {
    if (disposed || glFails >= GL_RETRIES) return;
    try {
      const gl = new WebglAddon();
      gl.onContextLoss(() => {
        gl.dispose();
        glFails++;
        // A new addon, a new context (never the DOM renderer).
        setTimeout(loadGl, 0);
      });
      term.loadAddon(gl);
    } catch {
      // No WebGL at all: the desktop has xterm.js's DOM renderer.
      glFails = GL_RETRIES;
    }
  };
  loadGl();

  let holding = true;
  let viewing: Extract<ControlView, { kind: "viewer" }> | null = null;
  let mods: Mods = { ctrl: false, alt: false };
  let pasting = false;
  let keyAt = 0;
  const drag = new Drag();

  /** The cell's size on screen, from the renderer's screen element. */
  const cell = (): { w: number; h: number } => {
    const s = el.querySelector<HTMLElement>(".xterm-screen");
    if (!s || term.cols === 0 || term.rows === 0) return { w: 0, h: 0 };
    return { w: s.clientWidth / term.cols, h: s.clientHeight / term.rows };
  };

  /** The phone's holder layout: the full view's height, anchored at the bottom. */
  const phoneHeight = (): number => {
    const vv = window.visualViewport;
    const full = fullHeight(box.clientHeight, window.innerHeight, vv?.height ?? window.innerHeight);
    return Math.max(0, full - 2 * INSET);
  };

  const clearStyles = (): void => {
    for (const k of ["top", "bottom", "height", "width", "transform", "transformOrigin"] as const) {
      el.style[k] = "";
    }
  };

  /** The phone's box had a size when last laid out (else lay out again when it has). */
  let laidOut = false;

  /** Lay the holder out and fit (a new size goes to the link from `onResize`). */
  const holdLayout = (): void => {
    clearStyles();
    if (phone) {
      laidOut = box.clientHeight > 0;
      el.style.top = "auto";
      el.style.height = `${phoneHeight()}px`;
      drag.pan = 0;
      slide();
    }
    fit.fit();
  };

  /** With the keyboard up the phone's terminal is taller than its box: slide it. */
  const slide = (): void => {
    if (!phone || !holding) return;
    const over = el.offsetHeight + 2 * INSET - box.clientHeight;
    drag.pan = Math.min(drag.pan, Math.max(0, over));
    el.style.transform = drag.pan ? `translateY(${drag.pan}px)` : "";
  };

  /** A viewer: the window's size, scaled down into the box (once the renderer has it). */
  let viewFrame = 0;
  const viewLayout = (): void => {
    if (!viewing) return;
    const { cols, rows } = viewing;
    if (cols && rows && (cols !== term.cols || rows !== term.rows)) term.resize(cols, rows);
    cancelAnimationFrame(viewFrame);
    viewFrame = requestAnimationFrame(scaleView);
  };
  const scaleView = (): void => {
    if (!viewing) return;
    const s = el.querySelector<HTMLElement>(".xterm-screen");
    const w = s?.offsetWidth ?? 0;
    const h = s?.offsetHeight ?? 0;
    const k = fitScale(box.clientWidth - 2 * INSET, box.clientHeight - 2 * INSET, w, h);
    el.style.top = `${INSET}px`;
    el.style.bottom = "auto";
    el.style.width = `${w}px`;
    el.style.height = `${h}px`;
    el.style.transformOrigin = "0 0";
    el.style.transform = k < 1 ? `scale(${k})` : "";
  };

  const send = (d: string | Uint8Array<ArrayBuffer>): void => {
    keyAt = performance.now();
    link.input(d);
  };

  const link = opts.makeLink(
    {
      get cols() {
        return term.cols;
      },
      get rows() {
        return term.rows;
      },
      reset: () => term.reset(),
      write: (b, parsed) => {
        term.write(b, parsed);
        if (keyAt && opts.onEcho) opts.onEcho(Math.round(performance.now() - keyAt));
        keyAt = 0;
      },
    },
    (s) => {
      if (s.kind === "open" && !phone) term.focus();
      opts.onState(s);
    },
  );

  const setMods = (m: Mods): void => {
    mods = m;
    opts.onMods?.(m);
  };

  const subs = [
    term.onData((d) => {
      if (!holding) {
        if (!notTyping(d)) opts.onBlocked?.();
        return;
      }
      if (!pasting && (mods.ctrl || mods.alt)) {
        const m = mods;
        setMods({ ctrl: false, alt: false });
        send(applyMods(d, m));
        return;
      }
      send(d);
    }),
    // Mouse reports in X10 encoding can carry bytes over 127: sent raw.
    term.onBinary((d) => {
      if (holding) send(Uint8Array.from(d, (c) => c.charCodeAt(0)));
    }),
    term.onResize(({ cols, rows }) => {
      if (holding) link.resize(cols, rows);
    }),
  ];

  let mirrorTimer: ReturnType<typeof setTimeout> | null = null;
  const text = (): string => {
    const b = term.buffer.active;
    const lines: string[] = [];
    for (let i = 0; i < term.rows; i++) {
      lines.push(b.getLine(b.viewportY + i)?.translateToString(true) ?? "");
    }
    return lines.join("\n");
  };
  if (opts.mirror) {
    const mirror = opts.mirror;
    subs.push(
      term.onWriteParsed(() => {
        if (mirrorTimer) return;
        mirrorTimer = setTimeout(() => {
          mirrorTimer = null;
          if (!disposed) mirror(text());
        }, 100);
      }),
    );
  }

  // Sizing. The desktop fits on every change of the box; the phone only
  // slides (or rescales a viewer), and fits again on a rotation.
  let frame = 0;
  let settle: ReturnType<typeof setTimeout> | null = null;
  const later = (): void => {
    if (settle) clearTimeout(settle);
    settle = setTimeout(() => {
      settle = null;
      if (holding) holdLayout();
      else viewLayout();
    }, SETTLE_MS);
  };
  const ro = new ResizeObserver(() => {
    cancelAnimationFrame(frame);
    frame = requestAnimationFrame(() => {
      if (!holding) viewLayout();
      else if (phone && !laidOut) holdLayout();
      else if (phone) slide();
      else fit.fit();
    });
  });
  ro.observe(box);
  let width = window.innerWidth;
  const onWindow = (): void => {
    if (window.innerWidth === width) return;
    width = window.innerWidth;
    later();
  };
  if (phone) window.addEventListener("resize", onWindow);

  // A vertical drag: slide the terminal, then the wheel.
  let lastY: number | null = null;
  const onTouchStart = (e: TouchEvent): void => {
    lastY = e.touches.length === 1 ? (e.touches[0]?.clientY ?? null) : null;
  };
  const onTouchMove = (e: TouchEvent): void => {
    const t = e.touches[0];
    if (lastY === null || !t || !holding) return;
    const dy = t.clientY - lastY;
    lastY = t.clientY;
    const c = cell();
    const over = el.offsetHeight + 2 * INSET - box.clientHeight;
    const r = drag.move(dy, over, c.h);
    el.style.transform = drag.pan ? `translateY(${drag.pan}px)` : "";
    e.preventDefault();
    if (r.wheel === 0 || term.modes.mouseTrackingMode === "none") return;
    const rect = el.getBoundingClientRect();
    const col = c.w ? Math.floor((t.clientX - rect.left) / c.w) + 1 : 1;
    const row = c.h ? Math.floor((t.clientY - rect.top) / c.h) + 1 : 1;
    const one = wheel(r.wheel > 0, Math.min(col, term.cols), Math.min(Math.max(row, 1), term.rows));
    send(one.repeat(Math.abs(r.wheel)));
  };
  const onTouchEnd = (): void => {
    lastY = null;
    drag.end();
  };
  if (phone) {
    el.addEventListener("touchstart", onTouchStart, { passive: true });
    el.addEventListener("touchmove", onTouchMove, { passive: false });
    el.addEventListener("touchend", onTouchEnd, { passive: true });
    el.addEventListener("touchcancel", onTouchEnd, { passive: true });
  }

  const onVisibility = (): void => link.visible(document.visibilityState === "visible");
  document.addEventListener("visibilitychange", onVisibility);

  // Sized before the link opens: the open carries the size.
  holdLayout();
  link.connect();
  onVisibility();

  return {
    link,
    focus: () => term.focus(),
    blur: () => term.blur(),
    send: (d) => {
      if (!holding) {
        opts.onBlocked?.();
        return false;
      }
      send(d);
      return true;
    },
    paste: (t, enter) => {
      if (!holding) {
        opts.onBlocked?.();
        return false;
      }
      if (tooLong(t)) return false;
      pasting = true;
      try {
        term.paste(t);
      } finally {
        pasting = false;
      }
      if (enter) send("\r");
      return true;
    },
    setMods,
    setMode: (v) => {
      if (v.kind === "holder") {
        const was = holding;
        holding = true;
        viewing = null;
        if (!was) holdLayout();
        return;
      }
      holding = false;
      viewing = v;
      drag.pan = 0;
      viewLayout();
    },
    setFont: (px) => {
      term.options.fontSize = px;
      later();
    },
    fitSize: () => {
      if (holding) return { cols: term.cols, rows: term.rows };
      // Laid out as the holder for a moment, to measure.
      const saved = el.getAttribute("style") ?? "";
      clearStyles();
      if (phone) {
        el.style.top = "auto";
        el.style.height = `${phoneHeight()}px`;
      }
      const d = fit.proposeDimensions();
      el.setAttribute("style", saved);
      return { cols: d?.cols ?? term.cols, rows: d?.rows ?? term.rows };
    },
    appCursor: () => term.modes.applicationCursorKeysMode,
    text,
    setReadOnly: (only) => {
      term.options.disableStdin = only;
      if (term.textarea) term.textarea.readOnly = only;
      if (only) term.blur();
    },
    dispose: () => {
      disposed = true;
      link.stop();
      document.removeEventListener("visibilitychange", onVisibility);
      if (phone) window.removeEventListener("resize", onWindow);
      ro.disconnect();
      cancelAnimationFrame(frame);
      cancelAnimationFrame(viewFrame);
      if (settle) clearTimeout(settle);
      if (mirrorTimer) clearTimeout(mirrorTimer);
      el.removeEventListener("touchstart", onTouchStart);
      el.removeEventListener("touchmove", onTouchMove);
      el.removeEventListener("touchend", onTouchEnd);
      el.removeEventListener("touchcancel", onTouchEnd);
      for (const d of subs) d.dispose();
      term.dispose();
    },
  };
}
