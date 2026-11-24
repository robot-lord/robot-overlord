// The phone's key row, above the keyboard: one scrolling row of 44-point
// keys (Esc, Tab, ⇧Tab, sticky Ctrl and Alt, the arrows, ⏎, ^C, Paste,
// the compose line, hide the keyboard; then the font steps and Copy
// screen). The compose line is an ordinary text field (autocorrect,
// dictation, emoji) sent as one paste, then Enter. Bytes come from
// term/keys.ts; the terminal sends them only while this screen holds the
// pane.

import { useState } from "preact/hooks";
import type { TermHandle } from "../term";
import { type Key, keyBytes, type Mods, PASTE_MAX, tooLong } from "../term/keys";
import { FONT_MAX, FONT_MIN } from "../term/layout";

/** Buttons that keep the keyboard up: no focus change on press. */
const keep = (e: Event): void => e.preventDefault();

const KEYS: { key: Key; label: string; title: string }[] = [
  { key: "esc", label: "Esc", title: "Escape" },
  { key: "tab", label: "Tab", title: "Tab" },
  { key: "backtab", label: "⇧Tab", title: "Shift-Tab (Claude Code's mode)" },
];
const ARROWS: { key: Key; label: string; title: string }[] = [
  { key: "left", label: "←", title: "left" },
  { key: "down", label: "↓", title: "down" },
  { key: "up", label: "↑", title: "up" },
  { key: "right", label: "→", title: "right" },
];

export function KeyRow({
  term,
  mods,
  paste,
  copy,
  font,
  changeFont,
  echo,
}: {
  /** None until the pane is open: the row is there, its keys do nothing. */
  term: TermHandle | null;
  mods: Mods;
  paste: () => Promise<string | null>;
  copy: (text: string) => void;
  font: number;
  changeFont: (by: -1 | 1) => void;
  /** The last key-to-echo time (ms), for the read-out. */
  echo: number | null;
}) {
  const [composing, setComposing] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const press = (key: Key): void => {
    if (!term) return;
    const bytes = keyBytes(key, { ...mods, appCursor: term.appCursor() });
    // ^C stands alone; the others use the sticky modifiers up.
    if (key !== "ctrl_c" && (mods.ctrl || mods.alt)) term.setMods({ ctrl: false, alt: false });
    term.send(bytes);
  };
  const toggle = (m: keyof Mods): void => term?.setMods({ ...mods, [m]: !mods[m] });

  const pasteNow = async (): Promise<void> => {
    setNote(null);
    if (!term) return;
    const t = await paste();
    if (t === null) setNote("Couldn't read the clipboard.");
    else if (!t) setNote("The clipboard is empty.");
    else if (tooLong(t)) setNote(`Too long to paste (over ${PASTE_MAX / 1024} KiB).`);
    else term.paste(t, false);
  };

  return (
    <div class="keys-wrap">
      {composing && term && <Compose term={term} />}
      {note && (
        <p class="err keys-note" role="status">
          {note}
        </p>
      )}
      <div
        class={`keys${term ? "" : " off"}`}
        role="toolbar"
        aria-label="terminal keys"
        aria-disabled={!term}
      >
        {KEYS.map((k) => (
          <button type="button" title={k.title} onPointerDown={keep} onClick={() => press(k.key)}>
            {k.label}
          </button>
        ))}
        <button
          type="button"
          class={mods.ctrl ? "on" : ""}
          aria-pressed={mods.ctrl}
          title="Ctrl for the next key"
          onPointerDown={keep}
          onClick={() => toggle("ctrl")}
        >
          Ctrl
        </button>
        <button
          type="button"
          class={mods.alt ? "on" : ""}
          aria-pressed={mods.alt}
          title="Alt for the next key"
          onPointerDown={keep}
          onClick={() => toggle("alt")}
        >
          Alt
        </button>
        {ARROWS.map((k) => (
          <button
            type="button"
            title={k.title}
            aria-label={k.title}
            onPointerDown={keep}
            onClick={() => press(k.key)}
          >
            {k.label}
          </button>
        ))}
        <button type="button" title="Enter" onPointerDown={keep} onClick={() => press("enter")}>
          ⏎
        </button>
        <button type="button" title="Ctrl-C" onPointerDown={keep} onClick={() => press("ctrl_c")}>
          ^C
        </button>
        <button type="button" onPointerDown={keep} onClick={() => void pasteNow()}>
          Paste
        </button>
        <button
          type="button"
          class={composing ? "on" : ""}
          aria-pressed={composing}
          title="type a line in a text field, sent as one paste"
          onClick={() => setComposing(!composing)}
        >
          ✎
        </button>
        <button type="button" aria-label="hide the keyboard" onClick={() => term?.blur()}>
          ⌄
        </button>
        <button
          type="button"
          aria-label="smaller text"
          disabled={font <= FONT_MIN}
          onPointerDown={keep}
          onClick={() => changeFont(-1)}
        >
          A−
        </button>
        <button
          type="button"
          aria-label="larger text"
          disabled={font >= FONT_MAX}
          onPointerDown={keep}
          onClick={() => changeFont(1)}
        >
          A+
        </button>
        <button type="button" onPointerDown={keep} onClick={() => term && copy(term.text())}>
          Copy screen
        </button>
        {echo !== null && <span class="dim echo">{echo} ms</span>}
      </div>
    </div>
  );
}

/** The compose line: a text field, sent as one paste then Enter (or without). */
function Compose({ term }: { term: TermHandle }) {
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const send = (enter: boolean): void => {
    if (!text) return;
    if (tooLong(text)) {
      setError(`Too long to paste (over ${PASTE_MAX / 1024} KiB).`);
      return;
    }
    if (term.paste(text, enter)) {
      setText("");
      setError(null);
    }
  };
  return (
    <form
      class="compose"
      onSubmit={(e) => {
        e.preventDefault();
        send(true);
      }}
    >
      <textarea
        rows={1}
        placeholder="type, then send"
        aria-label="compose a line"
        value={text}
        onInput={(e) => {
          setText(e.currentTarget.value);
          setError(null);
        }}
      />
      <button type="submit" class="primary" disabled={!text}>
        Send
      </button>
      <button
        type="button"
        title="paste it without Enter"
        disabled={!text}
        onClick={() => send(false)}
      >
        Paste
      </button>
      {error && <p class="err compose-err">{error}</p>}
    </form>
  );
}
