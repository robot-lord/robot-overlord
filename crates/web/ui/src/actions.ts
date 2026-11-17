// What the page asks the daemon to do to a session, worked out from what
// the user typed. Pure: the views send the requests.

import type { Op, SessionBase } from "./proto";

/** One request to send. */
export interface Request {
  op: Op;
  args: Record<string, unknown>;
}

/** The edit form's fields, as typed. */
export interface Edit {
  name: string;
  description: string;
  /** Space- or comma-separated; a leading `#` is dropped. */
  tags: string;
  notes: string;
}

/** The form's starting values for a session. */
export function editOf(s: SessionBase): Edit {
  return {
    name: s.name,
    description: s.description,
    tags: s.tags.join(" "),
    notes: s.notes,
  };
}

/** Tags as the list's own tag editor reads them: lower case, one word each. */
export function parseTags(text: string): string[] {
  const out: string[] = [];
  for (const t of text.split(/[\s,]+/)) {
    const tag = t.trim().replace(/^#+/, "").toLowerCase();
    if (tag && !out.includes(tag)) out.push(tag);
  }
  return out;
}

/**
 * The requests that turn `s` into what the form says: only the fields
 * that changed, so a field someone else (the overlord) changed meanwhile
 * is left alone unless the user changed it too.
 */
export function editRequests(s: SessionBase, e: Edit): Request[] {
  const ref = s.id;
  const out: Request[] = [];
  const name = e.name.trim();
  if (name && name !== s.name) out.push({ op: "session.rename", args: { ref, name } });
  const description = e.description.trim();
  if (description !== s.description) {
    out.push({ op: "session.describe", args: { ref, description } });
  }
  const notes = e.notes.trimEnd();
  if (notes !== s.notes) out.push({ op: "session.notes", args: { ref, notes } });
  const tags = parseTags(e.tags);
  const add = tags.filter((t) => !s.tags.includes(t));
  const remove = s.tags.filter((t) => !tags.includes(t));
  if (add.length > 0 || remove.length > 0) {
    out.push({ op: "session.tag", args: { ref, add, remove } });
  }
  return out;
}

/** Whether a session's agent is up (else the page offers Resume). */
export function isRunning(s: Pick<SessionBase, "status" | "tmux_session">): boolean {
  return s.status !== "stopped" && s.tmux_session !== null;
}
