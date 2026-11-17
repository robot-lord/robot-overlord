import type { Status as S } from "../proto";

/** A status as people see it: a coloured dot and its label. */
export function Status({ status, label }: { status: S | null; label: string | null }) {
  return (
    <span class={`status st-${status ?? "none"}`}>
      <span class="dot" aria-hidden="true" />
      {label ?? "not started"}
    </span>
  );
}
