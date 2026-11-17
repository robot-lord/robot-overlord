// A filtered list to pick from: type to narrow it (every word must
// appear), click or Enter to pick. `typed` turns the text itself into
// extra choices (a path typed by hand).

import { useState } from "preact/hooks";

export interface Choice {
  value: string;
  label: string;
  /** Dimmed after the label ("★ default", "3d ago"). */
  note?: string;
}

/** The choices whose label has every word of `text` (any case). */
export function filterChoices(items: Choice[], text: string): Choice[] {
  const words = text.toLowerCase().split(/\s+/).filter(Boolean);
  return items.filter((c) => {
    const l = c.label.toLowerCase();
    return words.every((w) => l.includes(w));
  });
}

export function Picker({
  items,
  onPick,
  placeholder,
  typed,
  note,
  max = 50,
}: {
  items: Choice[];
  onPick: (c: Choice) => void;
  placeholder: string;
  typed?: (text: string) => Choice[];
  note?: string | null;
  max?: number;
}) {
  const [text, setText] = useState("");
  const [at, setAt] = useState(0);
  const shown = [...(typed?.(text) ?? []), ...filterChoices(items, text)].slice(0, max);
  const pick = (c: Choice | undefined) => c && onPick(c);
  return (
    <div class="picker">
      <input
        type="text"
        placeholder={placeholder}
        aria-label={placeholder}
        value={text}
        onInput={(e) => {
          setText(e.currentTarget.value);
          setAt(0);
        }}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown") setAt((i) => Math.min(i + 1, shown.length - 1));
          else if (e.key === "ArrowUp") setAt((i) => Math.max(i - 1, 0));
          else if (e.key === "Enter") {
            e.preventDefault();
            pick(shown[at]);
          } else return;
          e.preventDefault();
        }}
      />
      {note && <p class="dim small">{note}</p>}
      <ul class="choices" role="listbox">
        {shown.map((c, i) => (
          <li key={c.value + c.label}>
            <button
              type="button"
              role="option"
              aria-selected={i === at}
              class={i === at ? "on" : ""}
              onClick={() => pick(c)}
            >
              {c.label}
              {c.note && <span class="dim"> {c.note}</span>}
            </button>
          </li>
        ))}
        {shown.length === 0 && <li class="dim small">nothing matches</li>}
      </ul>
    </div>
  );
}
