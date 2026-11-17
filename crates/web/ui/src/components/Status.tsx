import type { BackgroundTask, Limit, SessionBase } from "../proto";
import { ago, now } from "./time";

/** What a status chip needs of a row, the overlord or a roster entry. */
export type Shown = Pick<SessionBase, "status" | "status_label"> &
  Partial<Pick<SessionBase, "shown_status" | "async_tasks" | "async_work" | "async_shells">> & {
    limit?: Limit | null;
  };

/**
 * The status a row shows and its words, as the TUI draws it
 * (`crates/tui/src/lib.rs` `state_chip`): a usage limit, then background
 * work, else the stored status. `full`: in words (a pane's header);
 * otherwise the list's short form.
 */
export function shown(s: Shown, full: boolean): { cls: string; label: string } {
  const st = s.shown_status ?? s.status;
  if (st === "limited" && s.limit) {
    const l = s.limit;
    const resume = l.resume ? " ↻" : "";
    let label: string;
    if (l.reset) {
      if (full) label = l.resume ? "limit reset · resuming" : "limit reset just now";
      else label = `reset${resume}`;
    } else if (full) {
      label = `limited until ${l.until}${l.resume ? " · resumes then" : ""}`;
    } else {
      label = `till ${l.until}${resume}`;
    }
    return { cls: "limited", label };
  }
  if (st === "background") {
    const n = s.async_tasks ?? 0;
    const label = full ? `waiting on ${n} background ${plural(n, "task")}` : `bg ${n}`;
    return { cls: "background", label };
  }
  return { cls: s.status ?? "none", label: s.status_label ?? "not started" };
}

/**
 * What the chip doesn't say: the background work by name, and background
 * shells (dev servers, watchers: information, never a status), as the
 * TUI's details panel says it (`crates/tui/src/statustext.rs`).
 */
export function workLines(s: Shown, at: number): string[] {
  const out: string[] = [];
  if ((s.shown_status ?? s.status) === "background") {
    const n = s.async_tasks ?? 0;
    const tasks = s.async_work ?? [];
    let line = `Waiting on ${n} background ${plural(n, "task")}`;
    if (tasks.length > 0) line += `: ${listed(tasks, n, false, at)}`;
    const starts = tasks.map((t) => t.started_at).filter((t) => t > 0);
    if (starts.length > 0) line += `, started ${since(Math.min(...starts), at)}`;
    out.push(`${line}. Its turn is over; it goes on when they finish.`);
  }
  const sh = s.async_shells;
  if (sh && sh.count > 0) {
    let line = `${sh.count} background ${plural(sh.count, "shell")}`;
    if (sh.tasks.length > 0) line += `: ${listed(sh.tasks, sh.count, true, at)}`;
    out.push(`${line}.`);
  }
  return out;
}

/** "5m ago", "just now". */
function since(t: number, at: number): string {
  const a = ago(t, at);
  return a === "now" ? "just now" : `${a} ago`;
}

function plural(n: number, word: string): string {
  return n === 1 ? word : `${word}s`;
}

/** "a, b, +3 more"; `when`: each with its age. */
function listed(tasks: BackgroundTask[], n: number, when: boolean, at: number): string {
  const items = tasks.map((t) =>
    when && t.started_at > 0 ? `${t.label} (${since(t.started_at, at)})` : t.label,
  );
  if (n > items.length) items.push(`+${n - items.length} more`);
  return items.join(", ");
}

/** A pane header's lines about background work (nothing when none). */
export function Work({ s }: { s: Shown }) {
  const lines = workLines(s, now.value);
  return lines.length === 0 ? null : (
    <>
      {lines.map((l) => (
        <p class="work-line">{l}</p>
      ))}
    </>
  );
}

/**
 * A status as people see it: a coloured dot and its label. `at` (now):
 * the background work in its tooltip.
 */
export function Status({ s, full = false, at }: { s: Shown; full?: boolean; at?: number }) {
  const { cls, label } = shown(s, full);
  const lines = at === undefined ? [] : workLines(s, at);
  return (
    <span class={`status st-${cls}`} title={lines.length > 0 ? lines.join("\n") : undefined}>
      <span class="dot" aria-hidden="true" />
      {label}
    </span>
  );
}
