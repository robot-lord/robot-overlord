// A row's usage as the list shows it (crates/core/src/usage.rs).

import type { Row } from "../proto";

/** A count as the list's Usage column writes it (`usage::compact`). */
export function compact(n: number): string {
  if (n < 1000) return String(n);
  for (const [unit, suffix] of [
    [1e3, "k"],
    [1e6, "M"],
    [1e9, "G"],
    [1e12, "T"],
  ] as const) {
    const x = n / unit;
    if (x < 9.95) return `${x.toFixed(1)}${suffix}`;
    if (Math.round(x) < 1000) return `${Math.round(x)}${suffix}`;
  }
  return "999T";
}

/** A row's usage in the list's measure (`usage_metric`): text and its tooltip. */
export function usageFigure(r: Pick<Row, "usage" | "usage_metric">): [string, string] | null {
  const u = r.usage;
  if (!u) return null;
  switch (r.usage_metric) {
    case "tokens":
      return [compact(u.tokens), `${u.tokens.toLocaleString()} tokens`];
    case "context":
      return u.context_pct === null ? null : [`${u.context_pct}%`, "of the context window used"];
    default:
      return [String(u.turns), `${u.turns} ${u.turns === 1 ? "turn" : "turns"}`];
  }
}
