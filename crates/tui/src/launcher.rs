//! The less frequent things, kept off the key map:
//!
//! - **Command launcher** (`:`): type a few letters of a command, Enter runs
//!   it. Everything a key does is here too, by name, plus what has no key
//!   (the key screen, a specific sort, what the Usage column shows).
//! - **Cheat sheet** (`?`): every key, and what to ask the overlord instead.

use super::*;
use ratatui::widgets::Clear;
use rlord_core::db::SortMode;
use rlord_core::usage::UsageMetric;

#[derive(Default)]
pub(crate) struct Launcher {
    pub(crate) buf: String,
    sel: usize,
}

#[derive(Clone, Copy)]
enum Cmd {
    Act(Action),
    Keys,
    Sort(SortMode),
    /// None: back to config `usage` (forget the pick).
    Usage(Option<UsageMetric>),
    Summarize,
    /// Make this agent's overlord the active one.
    Switch(&'static str),
    /// Pick the selected session up when its usage limit resets.
    ResumeAfterLimit,
}

/// (name, what it does, command)
const COMMANDS: &[(&str, &str, Cmd)] = &[
    (
        "keys",
        "change key bindings (tests each key reaches rlord)",
        Cmd::Keys,
    ),
    ("help", "cheat sheet: every key", Cmd::Act(Action::Help)),
    (
        "new",
        "new session: describe it to the overlord",
        Cmd::Act(Action::New),
    ),
    (
        "new manual",
        "new session: the form",
        Cmd::Act(Action::NewManual),
    ),
    ("overlord", "open the overlord", Cmd::Act(Action::Overlord)),
    (
        "overlord claude",
        "switch to the overlord on Claude Code (its own conversation)",
        Cmd::Switch("claude"),
    ),
    (
        "overlord codex",
        "switch to the overlord on Codex (its own conversation)",
        Cmd::Switch("codex"),
    ),
    (
        "overlord opencode",
        "switch to the overlord on OpenCode (its own conversation)",
        Cmd::Switch("opencode"),
    ),
    (
        "resume after limit",
        "the selected session carries on when its usage limit resets",
        Cmd::ResumeAfterLimit,
    ),
    ("search", "filter the list", Cmd::Act(Action::Filter)),
    (
        "archive",
        "archive the selected session (asks first)",
        Cmd::Act(Action::Archive),
    ),
    (
        "sort last message",
        "newest message from you or the agent first",
        Cmd::Sort(SortMode::Activity),
    ),
    (
        "sort last prompt",
        "the session you last messaged first",
        Cmd::Sort(SortMode::User),
    ),
    (
        "sort status change",
        "the last to start, stop, finish or need you first",
        Cmd::Sort(SortMode::State),
    ),
    ("sort created", "newest first", Cmd::Sort(SortMode::Created)),
    ("sort name", "by name", Cmd::Sort(SortMode::Name)),
    (
        "sort usage",
        "most used first (what the Usage column shows)",
        Cmd::Sort(SortMode::Usage),
    ),
    (
        "usage turns",
        "Usage column: prompts you typed",
        Cmd::Usage(Some(UsageMetric::Turns)),
    ),
    (
        "usage tokens",
        "Usage column: tokens taken in or written",
        Cmd::Usage(Some(UsageMetric::Tokens)),
    ),
    (
        "usage context",
        "Usage column: how full the context window is",
        Cmd::Usage(Some(UsageMetric::Context)),
    ),
    (
        "usage default",
        "Usage column: back to config.toml's usage",
        Cmd::Usage(None),
    ),
    ("reverse", "reverse the sort", Cmd::Act(Action::ReverseSort)),
    (
        "archived",
        "show / hide archived sessions",
        Cmd::Act(Action::ToggleArchived),
    ),
    (
        "summarize",
        "write (or redo) the selected session's summary",
        Cmd::Summarize,
    ),
    (
        "log",
        "preview: live pane ⇄ event log",
        Cmd::Act(Action::ToggleLog),
    ),
    (
        "tabs",
        "sessions tab ⇄ groups tab",
        Cmd::Act(Action::SwitchTab),
    ),
    (
        "quit",
        "quit rlord (sessions keep running)",
        Cmd::Act(Action::Quit),
    ),
];

impl Launcher {
    /// Highlight the `i`th match (a click on it).
    pub(crate) fn select(&mut self, i: usize) {
        self.sel = i;
    }

    /// Commands matching what's typed: every word a substring of the name
    /// or what it does; names that start with it first.
    fn matches(&self) -> Vec<usize> {
        let q = self.buf.trim().to_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        let mut v: Vec<usize> = (0..COMMANDS.len())
            .filter(|&i| {
                let (n, d, _) = COMMANDS[i];
                let hay = format!("{n} {d}").to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .collect();
        v.sort_by_key(|&i| !COMMANDS[i].0.starts_with(&q));
        v
    }
}

impl App {
    pub(crate) fn handle_launcher(
        &mut self,
        k: ratatui::crossterm::event::KeyEvent,
        mut l: Box<Launcher>,
        terminal: &mut DefaultTerminal,
    ) -> Result<()> {
        let m = l.matches();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return Ok(()),
            KeyCode::Char('c' | 'g') if ctrl => return Ok(()),
            KeyCode::Enter => {
                let Some(&i) = m.get(l.sel.min(m.len().saturating_sub(1))) else {
                    self.status_line = format!("no command \"{}\"", l.buf.trim());
                    return Ok(());
                };
                return self.run_command(COMMANDS[i].2, terminal);
            }
            KeyCode::Tab => {
                if let Some(&i) = m.get(l.sel) {
                    l.buf = COMMANDS[i].0.to_string();
                    l.sel = 0;
                }
            }
            KeyCode::Up => l.sel = l.sel.saturating_sub(1),
            KeyCode::Char('p') if ctrl => l.sel = l.sel.saturating_sub(1),
            KeyCode::Down => l.sel = (l.sel + 1).min(m.len().saturating_sub(1)),
            KeyCode::Char('n') if ctrl => l.sel = (l.sel + 1).min(m.len().saturating_sub(1)),
            KeyCode::Char('u') if ctrl => {
                l.buf.clear();
                l.sel = 0;
            }
            KeyCode::Backspace => {
                if l.buf.pop().is_none() {
                    return Ok(()); // backspace on empty closes, like vim
                }
                l.sel = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                l.buf.push(c);
                l.sel = 0;
            }
            _ => {}
        }
        self.mode = Mode::Command(l);
        Ok(())
    }

    fn run_command(&mut self, c: Cmd, terminal: &mut DefaultTerminal) -> Result<()> {
        match c {
            Cmd::Act(a) => {
                self.do_action(a, terminal)?;
            }
            Cmd::Keys => self.open_keys(None),
            Cmd::Sort(s) => {
                // As a header click: a new sort starts in its natural direction.
                if self.sort != s {
                    self.reverse = false;
                }
                self.sort = s;
                self.dirty = true;
            }
            Cmd::Usage(m) => self.set_usage(m),
            Cmd::Switch(agent) => self.switch_overlord(agent),
            Cmd::ResumeAfterLimit => self.resume_after_limit(),
            Cmd::Summarize => {
                if let Some(id) = self.selected_id() {
                    if let Some(v) = self.call(
                        "session.summarize",
                        json!({"sessions": [id], "force": true}),
                    ) {
                        self.status_line = match v["skipped"][0]["why"].as_str() {
                            Some(w) => format!("no summary: {w}"),
                            None => "writing its summary in the background…".into(),
                        };
                        self.dirty = true;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn draw_launcher(&self, f: &mut Frame, l: &Launcher) {
        let m = l.matches();
        let a = f.area();
        let rows = (m.len() as u16).clamp(1, 10);
        let w = 64.min(a.width);
        // Just above the footer, at the left: where `:` was typed.
        let area = Rect {
            x: a.x,
            y: a.y + a.height.saturating_sub(rows + 2 + 2),
            width: w,
            height: (rows + 2).min(a.height),
        };
        f.render_widget(Clear, area);
        self.hits.add(f.area(), mouse::Hit::Outside);
        self.hits.add(area, mouse::Hit::Inside);
        let inner = theme::modal("Commands").inner(area);
        let sel = l.sel.min(m.len().saturating_sub(1));
        let lines: Vec<Line> = if m.is_empty() {
            vec![Line::styled("no matching command", theme::muted())]
        } else {
            m.iter()
                .enumerate()
                .skip(sel.saturating_sub(rows as usize - 1))
                .take(rows as usize)
                .enumerate()
                .map(|(row, (j, &i))| {
                    // Click it to run it.
                    self.hits
                        .row(inner, inner.y + row as u16, mouse::Hit::Pick(j));
                    let (n, d, _) = COMMANDS[i];
                    let name = Span::styled(format!("{n:<18} "), Style::default().bold());
                    let about = Span::styled(d.to_string(), theme::muted());
                    let line = Line::from(vec![name, about]);
                    if j == sel {
                        line.style(theme::selected())
                    } else {
                        line
                    }
                })
                .collect()
        };
        f.render_widget(Paragraph::new(lines).block(theme::modal("Commands")), area);
    }

    pub(crate) fn draw_help(&self, f: &mut Frame) {
        let k = |a| self.keys.label(a);
        let t = &self.config.tmux;
        let tk = |s: &str| {
            if s.trim().is_empty() {
                "off".to_string()
            } else {
                tmux::key_label(s)
            }
        };
        let head =
            |s: &str| Line::styled(s.to_string(), Style::default().fg(theme::accent()).bold());
        let row = |key: String, what: &str| {
            Line::from(vec![
                Span::styled(
                    format!("{key:>9}  "),
                    Style::default().fg(theme::text()).bold(),
                ),
                Span::styled(what.to_string(), theme::muted()),
            ])
        };
        let mut left = vec![
            head("In the list"),
            row(format!("{}/{}", k(Action::Down), k(Action::Up)), "move"),
            row(
                format!("{}/{}", k(Action::Top), k(Action::Bottom)),
                "top / bottom",
            ),
            row(k(Action::Attach), "attach (resumes a stopped one)"),
            row(k(Action::Filter), "search"),
            row(k(Action::New), "new session (ask the overlord)"),
            row(k(Action::NewManual), "new session (form)"),
            row(k(Action::SwitchTab), "sessions ⇄ groups"),
            row(
                format!("{}/{}", k(Action::SessionsTab), k(Action::GroupsTab)),
                "step between tabs",
            ),
            row(k(Action::Restart), "restart"),
            row(k(Action::Stop), "stop"),
            row(
                format!("{}/{}", k(Action::Archive), k(Action::Unarchive)),
                "archive / unarchive",
            ),
            row(k(Action::Rename), "rename"),
            row(k(Action::Pin), "pin"),
            row(k(Action::Tags), "tags (= groups)"),
            row(k(Action::Describe), "describe"),
            row(k(Action::Notes), "notes in $EDITOR"),
            row(k(Action::Sort), "next sort"),
            row(k(Action::ReverseSort), "reverse sort"),
            row(k(Action::ToggleArchived), "show archived"),
            row(k(Action::ToggleLog), "pane ⇄ log preview"),
            row(k(Action::Quit), "quit"),
        ];
        // (Unbound: its default `U` gave way to a key of the user's.)
        if self.keys.event(Action::Usage).is_some() {
            let at = left.len() - 3;
            left.insert(at, row(k(Action::Usage), "Usage: turns/tokens/context"));
        }
        if self.config.jump_digits {
            left.insert(4, row("1–9".into(), "attach the Nth session"));
        }
        let right = vec![
            head("Ask the overlord"),
            Line::styled(
                format!("{} opens it. Say what you want:", k(Action::Overlord)),
                theme::muted(),
            ),
            Line::from("  \"start a session on the auth bug\""),
            Line::from("  \"what was I doing yesterday?\""),
            Line::from("  \"tidy up my groups\""),
            Line::from("  \"change my keys\""),
            Line::from(""),
            head("Anywhere (every session)"),
            row(tk(&t.back), "home to this list"),
            row(tk(&t.overlord), "the overlord"),
            row(tk(&t.next), "next session"),
            row(tk(&t.prev), "previous session"),
            Line::from(""),
            head("In the overlay"),
            row(tk(&t.overlord), "close"),
            Line::styled(
                "    your tmux copy-mode keys work there too",
                theme::muted(),
            ),
            Line::from(""),
            head("Commands"),
            row(k(Action::Command), "launcher: keys, sort, log, archived…"),
            row(":keys".into(), "change a key (tests it reaches rlord)"),
            Line::from(""),
            head("Mouse"),
            Line::styled(
                "  click selects · double-click attaches · right-click: menu",
                theme::muted(),
            ),
            Line::styled(
                "  keycaps, tabs and fields are clickable; the wheel scrolls",
                theme::muted(),
            ),
            Line::from(""),
            head("Agents"),
            Line::from({
                // Each agent's marker in the list, then its name.
                let mut l = vec![Span::raw("  ")];
                for t in &rlord_core::agents::install::TOOLS {
                    l.extend(theme::agent_marker(t.name));
                    l.push(Span::styled(format!("{}   ", t.label), theme::muted()));
                }
                l
            }),
            Line::from(""),
            head("Status"),
            row("needs you".into(), "blocked on a prompt or question"),
            row("your turn".into(), "finished: the next prompt is yours"),
        ];
        // Side by side when wide, else stacked (both heights).
        let wide = f.area().width >= 84;
        let h = if wide {
            left.len().max(right.len())
        } else {
            left.len() + right.len() + 1
        } as u16
            + 3;
        let area = newform::centered(f.area(), 96, h);
        f.render_widget(Clear, area);
        // A click anywhere closes it, as any key does.
        self.hits.add(f.area(), mouse::Hit::Outside);
        self.hits.add(area, mouse::Hit::Inside);
        let block = theme::modal("Help");
        let inner = block.inner(area);
        f.render_widget(block, area);
        if wide {
            let [l, r] =
                RLayout::horizontal([Constraint::Percentage(48), Constraint::Min(0)]).areas(inner);
            f.render_widget(Paragraph::new(left), l);
            f.render_widget(Paragraph::new(right), r);
        } else {
            let mut all = right;
            all.push(Line::from(""));
            all.extend(left);
            f.render_widget(Paragraph::new(all), inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_matches_words() {
        let names = |q: &str| {
            let l = Launcher {
                buf: q.into(),
                sel: 0,
            };
            l.matches()
                .into_iter()
                .map(|i| COMMANDS[i].0)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("keys")[0], "keys");
        assert_eq!(names("so cr"), ["sort created"]);
        assert_eq!(names("new")[..2], ["new", "new manual"]);
        assert!(names("zzz").is_empty());
        assert_eq!(names("").len(), COMMANDS.len());
    }
}
