// A pane on screen: the terminal, the control bar when another screen
// holds it, what to say when it isn't showing, and on the phone the key
// row and the compose line. One link while mounted (`mountTerm` with the
// entry's `makeLink`); the view remounts it (by `key`) for another target.

import type { ComponentChildren } from "preact";
import { useEffect, useRef, useState } from "preact/hooks";
import { type ControlView, controlKey, controlView, controlWords } from "../control";
import type { MakeLink, PaneOpen, PaneState } from "../paneLink";
import type { PaneTaken } from "../proto";
import type { Store } from "../store";
import { mountTerm, type TermHandle } from "../term";
import type { Mods } from "../term/keys";
import { fontStep, loadFont, saveFont } from "../term/layout";
import { KeyRow } from "./KeyRow";

/** What only the phone has: the shell's pasteboard, the key row, the font steps. */
export interface PhoneExtras {
  /** The pasteboard's text, by the shell (null: no answer). */
  paste(): Promise<string | null>;
  copy(text: string): void;
  /** A debug build: mirror the screen's text into a hidden element (UI tests read it). */
  debug: boolean;
}

function safeStorage(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

/** How long "Take over to type" stays marked after a key press. */
const BLOCKED_MS = 2500;

export function Pane({
  store,
  makeLink,
  watch,
  expectId,
  notRunning,
  phone,
}: {
  store: Store;
  makeLink: MakeLink;
  /** When this changes and the pane isn't showing, try again (the target may be running now). */
  watch: string;
  /** The session the pane should show: reopen when it shows another (the overlord was switched). */
  expectId?: string | null;
  /** What to offer when the target isn't running. */
  notRunning: ComponentChildren;
  phone?: PhoneExtras;
}) {
  const el = useRef<HTMLDivElement>(null);
  const handle = useRef<TermHandle | null>(null);
  const [state, setState] = useState<PaneState>({ kind: "connecting" });
  const [mods, setMods] = useState<Mods>({ ctrl: false, alt: false });
  const [blocked, setBlocked] = useState(false);
  const [echo, setEcho] = useState<number | null>(null);
  const [mirror, setMirror] = useState("");
  const [font, setFont] = useState(() => loadFont(safeStorage()));
  const [took, setTook] = useState<string | null>(null);
  const [takeError, setTakeError] = useState<string | null>(null);
  const [taking, setTaking] = useState(false);

  useEffect(() => {
    if (!el.current) return;
    let blockTimer: ReturnType<typeof setTimeout> | null = null;
    const h = mountTerm(el.current, {
      makeLink,
      onState: setState,
      phone: phone ? { fontSize: font } : undefined,
      onMods: setMods,
      onBlocked: () => {
        setBlocked(true);
        if (blockTimer) clearTimeout(blockTimer);
        blockTimer = setTimeout(() => setBlocked(false), BLOCKED_MS);
      },
      onEcho: phone ? setEcho : undefined,
      mirror: phone?.debug ? setMirror : undefined,
    });
    handle.current = h;
    return () => {
      if (blockTimer) clearTimeout(blockTimer);
      handle.current = null;
      h.dispose();
    };
    // (Once per mount: the view remounts by `key` for another target.)
  }, [makeLink]);

  useEffect(() => {
    const l = handle.current?.link;
    if (!l) return;
    const st = l.state;
    if (st.kind === "not_running" || st.kind === "not_found" || st.kind === "retrying") {
      l.connect();
    } else if (st.kind === "open" && expectId && st.id && st.id !== expectId) {
      l.connect();
    }
  }, [watch, expectId]);

  const open: PaneOpen | null = state.kind === "open" ? state : null;
  const snapControl = store.controlOf(open?.id ?? null);
  const snapKey = controlKey(snapControl);
  const input = store.input.value && (open?.input ?? true);
  const view: ControlView = controlView({
    control: snapControl,
    me: store.surface.value,
    opened: open?.control ?? null,
    input,
    window: open?.window ?? null,
    took,
  });
  // A take counts until the snapshot moves on; a new open starts over.
  useEffect(() => {
    if (took !== null && took !== snapKey) setTook(null);
  }, [snapKey]);
  useEffect(() => setTook(null), [open]);

  const viewKey = JSON.stringify(view);
  useEffect(() => {
    handle.current?.setMode(view);
  }, [viewKey]);
  useEffect(() => {
    handle.current?.setReadOnly(!input);
  }, [input]);

  const take = async (): Promise<void> => {
    const h = handle.current;
    if (!h || !open?.id) return;
    setTaking(true);
    setTakeError(null);
    const size = h.fitSize();
    try {
      const r = await store.request<PaneTaken>("pane.take", { target: open.id, ...size });
      if (r.control) setTook(snapKey);
      setBlocked(false);
    } catch (e) {
      setTakeError(e instanceof Error ? e.message : String(e));
    } finally {
      setTaking(false);
    }
  };

  const changeFont = (by: -1 | 1): void => {
    const px = fontStep(font, by);
    if (px === font) return;
    setFont(px);
    saveFont(safeStorage(), px);
    handle.current?.setFont(px);
  };

  return (
    <div class={`pane-view${view.kind === "viewer" ? " viewing" : ""}`}>
      {open && view.kind === "viewer" && (
        <div class={`control-bar${blocked ? " attention" : ""}`} role="status">
          <span class="control-who">{controlWords(view)}</span>
          {view.canTake && (
            <button
              type="button"
              class={blocked ? "primary" : ""}
              disabled={taking}
              onClick={() => void take()}
            >
              {taking ? "taking…" : "Take over"}
            </button>
          )}
          {blocked && view.canTake && <span class="dim">Take over to type here.</span>}
          {takeError && <span class="err">{takeError}</span>}
        </div>
      )}
      <div class="pane">
        <div class="term" ref={el} />
        {state.kind !== "open" && (
          <div class="pane-note">
            <Note state={state} notRunning={notRunning} />
          </div>
        )}
      </div>
      {/* (From the first render, so the terminal is laid out above it.) */}
      {phone && view.kind === "holder" && input && state.kind !== "not_running" && (
        <KeyRow
          term={open ? handle.current : null}
          mods={mods}
          paste={() => phone.paste()}
          copy={(t) => phone.copy(t)}
          font={font}
          changeFont={changeFont}
          echo={echo}
        />
      )}
      {phone?.debug && (
        <pre class="term-mirror" aria-label="terminal text">
          {mirror}
        </pre>
      )}
    </div>
  );
}

function Note({ state, notRunning }: { state: PaneState; notRunning: ComponentChildren }) {
  switch (state.kind) {
    case "open":
      return null;
    case "connecting":
      return <p>connecting…</p>;
    case "not_running":
      return <div class="not-running">{notRunning}</div>;
    case "not_found":
      return <p>{state.message || "not found"}</p>;
    case "hidden":
      return <p>closed while hidden</p>;
    case "unauthorized":
      return <p>this link has expired: run `rlord web` again</p>;
    case "retrying":
      return <p>{state.message}: trying again…</p>;
  }
}
