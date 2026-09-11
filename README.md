# AI Overlord

Session management and orchestration for AI coding agents — a ground-up
replacement for the parts of [another tool](https://github.com)
we actually want, with full control over the design.

## Why

another tool is a tmux-based TUI for running many AI coding agent sessions. It's
useful — especially its ability to restart a session (even after a reboot) and
land right back in the agent's conversation — but it falls short in ways that
matter to us:

- **Weak session meta-context.** Last-message time / last-updated aren't
  first-class; you can't sort and pivot on them well.
- **Naming isn't sticky/ergonomic enough.**
- **AI-mediated everything.** It was clearly designed around phone-driven,
  fully AI-mediated control loops, which are expensive relative to their value.
  We want a *software product* over the sessions — tracking, sorting,
  organization done in plain code — with AI mediation only where it earns its
  cost.
- **Cross-session agent control is possible but clunky.** "Go tell that other
  agent to do X" / "go get context from that other agent" should be first-class
  and cheap.

## Goals

1. **Session management + meta-context first.** Rich, queryable per-session
   metadata: name (sticky), agent type, project, status, created / last-sent /
   last-activity times, archived state. Sort, filter, group, pivot.
2. **Durable resume.** Restart any session — including across machine reboots —
   retaining full conversation context (leaning on each agent's native resume).
3. **First-class inter-session communication.** An agent in one session can
   message, query, or task another session through a cheap, well-defined
   product surface (not an expensive agent-relay chain).
4. **Organization.** Running vs. archived, groups, and honest views of what
   exists and what state it's in.
5. **Narrow, deep agent support.** Claude Code first-class; OpenAI Codex
   supported; the integration layer generic enough to add others, but we will
   not chase every agent at the expense of doing Claude well.

## Non-goals (for now)

- Phone-first control and always-on AI mediation loops.
- Support for every agent CLI in existence.

## Open questions

- **Language:** Rust (probable).
- **Frontend:** mirror another tool's tmux/terminal approach vs. a GUI. Depends
  heavily on what the agent-integration research says is possible/necessary.

## Docs

- `docs/research-other-tool.md` — baseline research: how another tool actually
  works, which of its features are its own machinery vs. delegation to agent
  CLIs, and what that implies for our design.
