// Hash routes: `#/` the list, `#/overlord` the overlord, `#/s/<id>` a
// session. (The token's `#t=` is taken off before routing.)

import { signal } from "@preact/signals";

export type Route = { kind: "list" } | { kind: "overlord" } | { kind: "session"; id: string };

export function parse(hash: string): Route {
  const path = hash.replace(/^#/, "");
  if (path === "/overlord") return { kind: "overlord" };
  const m = /^\/s\/([^/]+)$/.exec(path);
  if (m?.[1]) {
    try {
      return { kind: "session", id: decodeURIComponent(m[1]) };
    } catch {
      return { kind: "list" };
    }
  }
  return { kind: "list" };
}

export function href(r: Route): string {
  if (r.kind === "overlord") return "#/overlord";
  if (r.kind === "session") return `#/s/${encodeURIComponent(r.id)}`;
  return "#/";
}

/** The current route, following the address bar. */
export const route = signal<Route>(parse(typeof location === "undefined" ? "" : location.hash));

export function followHash(): void {
  window.addEventListener("hashchange", () => {
    route.value = parse(location.hash);
  });
}
