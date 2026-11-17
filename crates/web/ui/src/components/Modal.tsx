// A dialog over the page: a title, its body, and Escape or the backdrop to
// close it (unless it is busy sending).

import type { ComponentChildren } from "preact";
import { useEffect, useRef } from "preact/hooks";

export function Modal({
  title,
  onClose,
  busy = false,
  children,
}: {
  title: string;
  onClose: () => void;
  busy?: boolean;
  children: ComponentChildren;
}) {
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);
  useEffect(() => {
    // The first field, else the dialog itself.
    const first = box.current?.querySelector<HTMLElement>("textarea, input, select, button");
    (first ?? box.current)?.focus();
  }, []);
  return (
    <div
      class="modal-back"
      onClick={(e) => {
        if (e.target === e.currentTarget && !busy) onClose();
      }}
    >
      <div class="modal" role="dialog" aria-modal="true" aria-label={title} ref={box} tabIndex={-1}>
        <header class="modal-head">
          <h2>{title}</h2>
          <button type="button" class="close" aria-label="close" disabled={busy} onClick={onClose}>
            ×
          </button>
        </header>
        {children}
      </div>
    </div>
  );
}
