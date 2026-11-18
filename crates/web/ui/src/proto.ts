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
  /** This page's id: the daemon addresses UI commands to it. */
  page: string;
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

/** A UI command from the overlord, for this page (`UiCmd`). */
export interface UiEvent {
  event: "ui";
  cmd: UiCmd;
}

export type ControlMessage = Hello | Changed | UiEvent | Reply;

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
  "view.default",
  "keys.list",
  "form.defaults",
  "form.repos",
  "form.setup",
  "form.setup_save",
  "overlord.new_session",
  "overlord.setup_request",
  "session.new",
  "session.fork",
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
  /** The filter box's text; the daemon reads its `tag:` / `status:` tokens. */
  query?: string;
  /** The usage measure; none: the user's own (or the configured one). */
  usage?: string;
}

export type Sort = "activity" | "user" | "state" | "created" | "name" | "usage";

// The overlord's UI commands for this page (crates/daemon/src/ui.rs; one
// of each in test/fixtures/ui_cmds.json, written by a daemon test).

/** A list view as the daemon and the TUI hold it (`ui::normalize_view`). */
export interface ListView {
  /** The filter box's text (the TUI's syntax: `tag:x status:needs …`). */
  query: string;
  /** A sort label (`updated`, `user`, `state`, `created`, `name`, `usage`). */
  sort: string;
  reverse: boolean;
  /** The usage measure (`turns`, `tokens`, `context`); null: the default. */
  usage: string | null;
  archived: string;
  tab: string;
  /** The picked group ("" untagged); null: none. */
  group: string | null;
}

/** The overlord's new-session proposal (`propose_session`). */
export interface NewPrefill {
  repo?: string;
  project?: string;
  name?: string;
  branch?: string;
  agent?: string;
  model?: string;
  prompt?: string;
  /** The overlord's background, shown read-only (sent back by `proposal`). */
  context?: string;
  base?: string;
  prefer?: string;
  workspace?: boolean;
  related?: string[];
  related_names?: string[];
  /** The id the launch names so the daemon adds the background. */
  proposal: string;
}

/** The describe form reopened with the overlord's hints (`confirm_new_session`). */
export interface ConfirmPrefill {
  description: string;
  repo?: string;
  project?: string;
  agent?: string;
  /** "" is the agent's default. */
  model?: string;
  base?: string;
  prefer?: string;
  workspace?: boolean;
  tags?: string[];
  changed?: string[];
  note?: string;
  /** Hints that are the user's own picks. */
  chosen: string[];
  /** Hints the overlord set. */
  resolved: string[];
}

/** The code-folder form as the overlord proposes it (`confirm_setup`). */
export interface SetupPrefill {
  code_dir?: string;
  repo_roots?: string[];
  description?: string;
  note?: string;
  changed?: string[];
}

/** The code folder as configured now (`setup.status`). */
export interface SetupStatus {
  code_dir: string;
  done: boolean;
  needed: boolean;
  repo_roots: string[];
  roots_default: string;
  skipped: boolean;
}

export type UiCmd =
  | { cmd: "goto"; id: string; overlord: boolean }
  | { cmd: "show_list" }
  | { cmd: "set_view"; view: ListView; select?: string }
  | { cmd: "new_session"; prefill: NewPrefill }
  | { cmd: "confirm_new_session"; prefill: ConfirmPrefill }
  | { cmd: "setup"; prefill: SetupPrefill; status: SetupStatus }
  | { cmd: "keys"; row: string }
  | { cmd: "notice"; text: string };

/** One agent the forms offer (`session.defaults`' `agents`). */
export interface AgentChoice {
  name: string;
  label: string;
  installed: boolean;
  /** rlord can run it (false: `too_old` says the version). */
  supported: boolean;
  too_old: string | null;
  hooks: boolean | null;
  model: string;
  models: string[];
  install: { command: string; docs: string; via: string; alternatives: unknown[] } | null;
}

/** The forms' defaults (op `form.defaults`). */
export interface FormDefaults {
  agent: string;
  agents: AgentChoice[];
  base: string;
  code_dir: string;
  default_repo: string;
  model: string;
  workspace: boolean;
}

/** One repo the finder knows. */
export interface RepoChoice {
  path: string;
  default: boolean;
  last_used_at: number | null;
}

/** The repo finder (op `form.repos`). */
export interface FormRepos {
  repos: RepoChoice[];
  /** The first disk scan is still running: ask again shortly. */
  scanning: boolean;
  skipped: string[];
  skipped_more: number;
  roots: string[];
  code_dir: string;
  depth: number;
}

/** A code folder rlord suggests, or an alternative. */
export interface Candidate {
  path: string;
  [k: string]: unknown;
}

/** The code-folder form's look-up (op `form.setup`). */
export interface FormSetup {
  status: SetupStatus;
  suggest: {
    suggestion: Candidate | null;
    alternatives: Candidate[];
    code_dir: string;
    needed: boolean;
    done: boolean;
    roots: { configured: string[]; effective: string[] };
    roots_default: string;
    sources: Record<string, unknown>;
  };
}

/** One key rlord binds (op `keys.list`). */
export interface KeyRow {
  id: string;
  does: string;
  where: string;
  key: string;
  default: string;
  findings: { level: string; text: string }[];
}
