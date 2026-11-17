// Times as people read them in a list.

import { signal } from "@preact/signals";

/** Now, moved every 30 s so relative times redraw. */
export const now = signal(Date.now());

export function tick(): void {
  setInterval(() => {
    now.value = Date.now();
  }, 30_000);
}

/** "now", "5m", "3h", "2d", "6w" since `t` (ms). */
export function ago(t: number, at: number): string {
  const s = Math.max(0, Math.floor((at - t) / 1000));
  if (s < 60) return "now";
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h}h`;
  const d = Math.floor(h / 24);
  if (d < 14) return `${d}d`;
  return `${Math.floor(d / 7)}w`;
}

/** A session's last update: its latest message either way, else its creation. */
export function lastUpdate(r: {
  last_user_msg_at: number | null;
  last_agent_msg_at: number | null;
  created_at: number;
}): number {
  return Math.max(r.last_user_msg_at ?? 0, r.last_agent_msg_at ?? 0) || r.created_at;
}
