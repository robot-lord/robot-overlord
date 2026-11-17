// The messages between the page and rlord-web, as the plugin sends them
// (crates/web/src/http.rs, pane.rs). Hand-written: keep in step with the
// Rust side.

/** The control socket's first message after the token. */
export interface Hello {
  event: "hello";
  version: string;
  /** The plugin's version plus a hash of its embedded page: a page from another build reloads. */
  build: string;
  rev: string;
}

/** Something in the daemon changed: ask for a new snapshot. */
export interface Changed {
  event: "changed";
  rev: string;
}

export type ErrorCode = "not_allowed" | "failed" | "bad_request" | "daemon_unavailable";

export interface Reply {
  id: number;
  ok?: unknown;
  error?: { code: ErrorCode; message: string };
}

export type ControlMessage = Hello | Changed | Reply;

/** The close code for a missing or wrong token. */
export const CLOSE_UNAUTHORIZED = 4401;
