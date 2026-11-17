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

/**
 * The ops the daemon takes from the page: its allow-list
 * (`crates/daemon/src/web.rs` `OPS`; a daemon test holds this list to it).
 */
export const OPS = [
  "snapshot",
  "session.get",
  "overlord.ensure",
  "session.resume",
  "session.restart",
  "session.stop",
  "overlord.ask",
  "session.archive",
  "session.unarchive",
  "session.rename",
  "session.describe",
  "session.notes",
  "session.tag",
  "session.pin",
  "overlord.switch",
  "page.report",
  "page.pending",
] as const;

export type Op = (typeof OPS)[number];

/** The close code for a missing or wrong token. */
export const CLOSE_UNAUTHORIZED = 4401;

// The `snapshot` op's answer (crates/daemon/src/web.rs `snapshot`),
// checked against test/fixtures/snapshot.json, which a daemon test keeps
// in step with the real answer (crates/daemon/tests/web_api.rs).

/** A session's stored status (`rlord_core::types::Status`). */
export type Status = "starting" | "working" | "waiting_input" | "idle" | "stopped" | "error";

/** Every session field (`rlord_core::types::Session`), as listed. */
export interface SessionBase {
  id: string;
  name: string;
  name_locked: boolean;
  agent: string;
  agent_session: string | null;
  agent_session_trust: string;
  project_path: string;
  tmux_session: string | null;
  status: Status;
  /** How the status is shown to people ("needs you", "your turn"). */
  status_label: string | null;
  group_path: string;
  pinned: boolean;
  /** Times are milliseconds since the epoch. */
  created_at: number;
  archived_at: number | null;
  last_user_msg_at: number | null;
  last_agent_msg_at: number | null;
  last_state_change_at: number | null;
  last_rlord_msg_at: number | null;
  repo_path: string | null;
  workspace_path: string | null;
  branch: string | null;
  description: string;
  notes: string;
  model: string | null;
  tags: string[];
  agents_running: number;
  agents_total: number;
  /** "" for a session, "overlord" for the overlord. */
  role: string;
  summary: string;
  summary_at: number | null;
  codex_daemon: boolean;
  forked_from: string | null;
  forked_from_name: string | null;
  fork_count: number;
  live_fork_count: number;
  /** Subagents still running after the turn ended (status idle), else null. */
  async_tasks: number | null;
  /** Each such subagent, oldest first (at most 5), else null. */
  async_work: BackgroundTask[] | null;
  /** Background shells, any status, else null. */
  async_shells: { count: number; tasks: BackgroundTask[] } | null;
  /** What the list shows: a usage limit, then background work, else the status. */
  shown_status: Status | "limited" | "background";
}

/** One piece of background work. */
export interface BackgroundTask {
  label: string;
  started_at: number;
}

/** A session stopped on a usage limit. */
export interface Limit {
  resets_at: number;
  /** The reset time as a clock ("14:05"). */
  until: string;
  reset: boolean;
  kind: string | null;
  /** A job will resume it. */
  resume: boolean;
}

export interface Usage {
  turns: number;
  tokens: number;
  context: number;
  window: number | null;
  context_pct: number | null;
}

/** A list row: a session, with what the daemon adds to rows. */
export interface Row extends SessionBase {
  status_label: string;
  /** Still being read at daemon start. */
  loading: boolean;
  summarizing: boolean;
  remote: unknown;
  limit: Limit | null;
  usage: Usage | null;
  usage_metric: string;
  usage_loading?: boolean;
}

/** One of the overlord's agents (Claude Code, Codex, OpenCode). */
export interface RosterEntry {
  active: boolean;
  agent: string;
  /** Its overlord session exists (it may be stopped). */
  exists: boolean;
  id: string | null;
  installed: boolean;
  label: string;
  running: boolean;
  status: Status | null;
  status_label: string | null;
  tmux_session: string | null;
}

/** The active overlord and its roster. */
export interface Overlord extends SessionBase {
  effort: string;
  limit: Limit | null;
  conversations: {
    earlier: number;
    fresh_at: number | null;
    reason: string | null;
    why: string | null;
  } | null;
  overlords: RosterEntry[];
}

export interface Group {
  /** "" is the untagged sessions. */
  tag: string;
  label: string;
  count: number;
  favourite: boolean;
}

export interface Snapshot {
  rev: string;
  rows: Row[];
  groups: Group[];
  /** Null before the overlord is set up. */
  overlord: Overlord | null;
  /** Every status, in the list's order, with its label. */
  statuses: { status: Status; label: string }[];
}

/** The answer when the asked `rev` is still current. */
export interface Unchanged {
  unchanged: true;
  rev: string;
}

/** The `snapshot` op's `view`: a `session.list` query. */
export interface Query {
  archived?: "live" | "only" | "all";
  sort?: Sort;
  reverse?: boolean;
  search?: string;
}

export type Sort = "activity" | "user" | "state" | "created" | "name" | "usage";
