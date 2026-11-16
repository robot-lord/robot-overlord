//! The fork form (`f` in the list): a new session that goes on from a copy
//! of the selected session's conversation (`session.fork`).
//!
//! The describe-then-confirm shape, small: what the fork should do first
//! (the user's words, optional), then hints with their defaults (its name,
//! where it works). Enter forks by plain code, with no overlord turn: the
//! options are few and each has a safe default.

use super::*;
use ratatui::widgets::Clear;
use rlord_core::config::contract_tilde;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Row {
    Prompt,
    Name,
    Folder,
    Fork,
}

/// A fork on its way: its answer (the session, or the error), and the form
/// it came from.
pub(crate) type ForkSend = (mpsc::Receiver<Result<Value, String>>, Box<ForkForm>);

#[derive(Debug, Clone)]
pub(crate) struct ForkForm {
    pub parent_id: String,
    pub parent_name: String,
    pub agent: String,
    /// The parent's folder, as shown.
    pub folder: String,
    /// What the fork should do first (the user's words; empty: it opens
    /// idle).
    pub prompt: String,
    /// Its name (empty: chosen on fork, `default_hint`).
    pub name: String,
    /// In a new worktree rather than the parent's folder.
    pub worktree: bool,
    /// A new worktree is possible: the agent can fork elsewhere (Claude
    /// Code, Codex) and the parent is in a git repo.
    pub can_worktree: bool,
    pub row: usize,
    pub error: Option<String>,
}

impl ForkForm {
    /// The form for forking session `s` (a list row).
    pub(crate) fn for_session(s: &Value) -> ForkForm {
        let agent = s["agent"].as_str().unwrap_or("").to_string();
        let can_worktree = rlord_core::agents::Agent::parse(&agent)
            .is_some_and(|a| a.forks_elsewhere())
            && s["repo_path"].as_str().is_some_and(|r| !r.is_empty());
        ForkForm {
            parent_id: s["id"].as_str().unwrap_or("").to_string(),
            parent_name: s["name"].as_str().unwrap_or("").to_string(),
            agent,
            folder: contract_tilde(s["project_path"].as_str().unwrap_or("")),
            prompt: String::new(),
            name: String::new(),
            worktree: false,
            can_worktree,
            row: 0,
            error: None,
        }
    }

    pub(crate) fn rows(&self) -> Vec<Row> {
        let mut rows = vec![Row::Prompt, Row::Name];
        if self.can_worktree {
            rows.push(Row::Folder);
        }
        rows.push(Row::Fork);
        rows
    }

    pub(crate) fn current(&self) -> Row {
        let rows = self.rows();
        rows[self.row.min(rows.len() - 1)]
    }

    /// What the name field says when empty: the daemon picks the name on
    /// fork (`<parent>-fork`, or the next one no session has; the list may
    /// not show them all).
    pub(crate) fn default_hint(&self) -> String {
        format!(
            "(optional) chosen on fork: {}-fork, or the next free one",
            self.parent_name
        )
    }

    /// `session.fork`'s params, as the form stands.
    pub(crate) fn params(&self) -> Value {
        let opt = |s: &str| {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        };
        let mut p = json!({"ref": self.parent_id});
        if let Some(v) = opt(&self.prompt) {
            p["prompt"] = json!(v);
        }
        if let Some(v) = opt(&self.name) {
            p["name"] = json!(v);
        }
        if self.worktree && self.can_worktree {
            p["workspace"] = json!(true);
        }
        p
    }

    /// A click on row `i`: selects it. Returns whether it was already
    /// selected (a second click acts).
    pub(crate) fn click_row(&mut self, i: usize) -> bool {
        let again = self.row == i;
        self.row = i.min(self.rows().len() - 1);
        again
    }

    /// What a key does to the form. Returns true when it should fork now.
    pub(crate) fn key(&mut self, k: KeyEvent) -> bool {
        let n = self.rows().len();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Tab | KeyCode::Down => self.row = (self.row + 1) % n,
            KeyCode::BackTab | KeyCode::Up => self.row = (self.row + n - 1) % n,
            KeyCode::Enter => return true,
            KeyCode::Left | KeyCode::Right if self.current() == Row::Folder => {
                self.worktree = !self.worktree
            }
            KeyCode::Char(' ') if self.current() == Row::Folder => self.worktree = !self.worktree,
            KeyCode::Backspace => match self.current() {
                Row::Prompt => {
                    self.prompt.pop();
                }
                Row::Name => {
                    self.name.pop();
                }
                _ => {}
            },
            KeyCode::Char(c) if !ctrl => match self.current() {
                Row::Prompt => self.prompt.push(c),
                Row::Name => self.name.push(c),
                _ => {}
            },
            _ => {}
        }
        self.error = None;
        false
    }
}

impl App {
    /// `f`: the fork form for the selected session (not the overlord).
    pub(crate) fn open_fork_form(&mut self) {
        let Some(s) = self.selected() else {
            self.status_line = "nothing selected to fork".into();
            return;
        };
        if s["role"].as_str().is_some_and(|r| !r.is_empty()) {
            self.status_line = "the overlord can't be forked".into();
            return;
        }
        self.mode = Mode::Fork(Box::new(ForkForm::for_session(s)));
    }

    pub(crate) fn handle_fork_form(&mut self, k: KeyEvent, mut f: Box<ForkForm>) {
        if k.code == KeyCode::Esc {
            return;
        }
        if !f.key(k) {
            self.mode = Mode::Fork(f);
            return;
        }
        if self.fork_send.is_some() {
            f.error = Some("a fork is still being made: a moment".into());
            self.mode = Mode::Fork(f);
            return;
        }
        // Off the UI thread (OpenCode's fork call, a worktree, the hooks
        // check take a while): the list stays live and says so.
        self.status_line = format!("forking {}…", f.parent_name);
        let (tx, rx) = mpsc::channel();
        let params = f.params();
        std::thread::spawn(move || {
            let r = client_call(&paths::socket_path(), "session.fork", params)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(r);
        });
        self.fork_send = Some((rx, f));
    }

    /// A fork sent from the form has an answer: the new session selected,
    /// or the form again with what went wrong. Returns whether to redraw.
    pub(crate) fn poll_fork(&mut self) -> bool {
        let Some((rx, _)) = &self.fork_send else {
            return false;
        };
        let r = match rx.try_recv() {
            Ok(r) => r,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Err("the request was lost".into()),
        };
        let (_, mut f) = self.fork_send.take().expect("checked");
        match r {
            Ok(v) => {
                self.status_line = fork_status(&v);
                // The new session, wherever the list puts it.
                self.pending_select = v["id"].as_str().map(str::to_string);
            }
            Err(e) => {
                self.status_line = format!("{} wasn't forked", f.parent_name);
                f.error = Some(e);
                // (Over whatever is open meanwhile only when nothing is.)
                if matches!(self.mode, Mode::Normal) {
                    self.mode = Mode::Fork(f);
                }
            }
        }
        true
    }

    pub(crate) fn draw_fork_form(&self, f: &mut Frame, s: &ForkForm) {
        let accent = Style::default().fg(theme::accent());
        let text = Style::default().fg(theme::text());
        let width = f.area().width.min(84).saturating_sub(8).max(10) as usize;
        let mut lines: Vec<Line> = vec![
            Line::from(""),
            Line::from(vec![
                Span::styled("  A new session with a copy of ", text),
                Span::styled(s.parent_name.clone(), text.bold()),
                Span::styled("'s whole conversation; the original stays as it is.", text),
            ]),
            Line::from(Span::styled(
                format!("  {} · {}", s.agent, s.folder),
                theme::muted(),
            )),
            Line::from(""),
        ];
        let label = |focused: bool, name: &str| {
            vec![
                Span::styled(if focused { "▌ " } else { "  " }, accent),
                Span::styled(
                    format!("{name:<13}"),
                    if focused {
                        Style::default().fg(theme::yellow()).bold()
                    } else {
                        Style::default().bold()
                    },
                ),
            ]
        };
        let field = |focused: bool, value: &str, empty: &str| -> Vec<Span<'static>> {
            let cursor = if focused { "▏" } else { "" };
            if value.is_empty() {
                vec![
                    Span::styled(cursor.to_string(), accent),
                    Span::styled(empty.to_string(), theme::faint()),
                ]
            } else {
                let shown: String = {
                    let chars: Vec<char> = value.chars().collect();
                    let room = width.saturating_sub(16).max(10);
                    // (The end of a long line, where the typing is.)
                    chars[chars.len().saturating_sub(room)..].iter().collect()
                };
                vec![Span::styled(format!("{shown}{cursor}"), text)]
            }
        };
        let mut starts = Vec::new();
        let rows = s.rows();
        for (i, row) in rows.iter().enumerate() {
            let focused = s.row == i;
            if *row == Row::Name {
                lines.push(Line::from(Span::styled(
                    "  Optional — leave as they are:",
                    theme::muted(),
                )));
            }
            if *row == Row::Fork {
                lines.push(Line::from(""));
            }
            starts.push(lines.len());
            let mut spans = match row {
                Row::Prompt => label(focused, "First prompt"),
                Row::Name => label(focused, "Name"),
                Row::Folder => label(focused, "Works in"),
                Row::Fork => vec![
                    Span::styled(if focused { "▌ " } else { "  " }, accent),
                    Span::styled(
                        "[ Fork ]",
                        if focused {
                            accent.bold()
                        } else {
                            Style::default().bold()
                        },
                    ),
                ],
            };
            match row {
                Row::Prompt => spans.extend(field(
                    focused,
                    &s.prompt,
                    "(optional) what the fork should do; empty: it opens idle",
                )),
                Row::Name => spans.extend(field(focused, &s.name, &s.default_hint())),
                Row::Folder => {
                    let v = if s.worktree {
                        "a new worktree (from its last commit)"
                    } else {
                        "the same folder"
                    };
                    spans.push(Span::styled(
                        if focused {
                            format!("◀ {v} ▶")
                        } else {
                            v.to_string()
                        },
                        text,
                    ));
                }
                Row::Fork => {}
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        lines.push(keys_line(&[
            ("enter", "fork", Cap::Context),
            ("tab", "move", Cap::Global),
            ("esc", "cancel", Cap::Global),
        ]));
        if let Some(e) = &s.error {
            lines.push(Line::from(Span::styled(
                format!("  {e}"),
                Style::default().fg(theme::red()),
            )));
        }
        let area = newform::centered(f.area(), 84, lines.len() as u16 + 3);
        f.render_widget(Clear, area);
        self.modal_hits(f.area(), area, &lines);
        for (i, at) in starts.iter().enumerate() {
            self.hits
                .row(area, area.y + 1 + *at as u16, mouse::Hit::Pick(i));
        }
        f.render_widget(
            Paragraph::new(lines).block(theme::modal(&format!("◆ Fork {}", s.parent_name))),
            area,
        );
    }
}

/// The status line after a fork: the new session, then what to know.
pub(crate) fn fork_status(v: &Value) -> String {
    let mut out = format!(
        "forked {} → {}",
        v["forked_from_name"].as_str().unwrap_or("?"),
        v["name"].as_str().unwrap_or("?")
    );
    for w in v["warnings"].as_array().into_iter().flatten() {
        if let Some(w) = w.as_str() {
            out.push_str(" · ");
            out.push_str(w);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn row(agent: &str, repo: Option<&str>) -> Value {
        json!({"id": "01p", "name": "api", "agent": agent, "project_path": "/src/api",
               "repo_path": repo})
    }

    #[test]
    fn the_form_forks_with_what_was_typed() {
        let mut f = ForkForm::for_session(&row("codex", Some("/src/api")));
        assert_eq!(f.rows(), [Row::Prompt, Row::Name, Row::Folder, Row::Fork]);
        // Left as it is: the parent only (opens idle, default name, same
        // folder).
        assert_eq!(f.params(), json!({"ref": "01p"}));
        for c in "try a queue".chars() {
            assert!(!f.key(key(KeyCode::Char(c))));
        }
        f.key(key(KeyCode::Tab));
        for c in "q".chars() {
            f.key(key(KeyCode::Char(c)));
        }
        f.key(key(KeyCode::Tab));
        assert_eq!(f.current(), Row::Folder);
        f.key(key(KeyCode::Right));
        assert!(f.worktree);
        assert!(f.key(key(KeyCode::Enter)), "enter forks");
        assert_eq!(
            f.params(),
            json!({"ref": "01p", "prompt": "try a queue", "name": "q", "workspace": true})
        );
        // Backspace edits the row it is on.
        f.row = 1;
        f.key(key(KeyCode::Backspace));
        assert!(f.params().get("name").is_none());
        // The name is the daemon's to choose (the list may be filtered).
        assert!(f.default_hint().contains("chosen on fork: api-fork"));
    }

    #[test]
    fn opencode_and_plain_folders_have_no_worktree_row() {
        let f = ForkForm::for_session(&row("opencode", Some("/src/api")));
        assert_eq!(f.rows(), [Row::Prompt, Row::Name, Row::Fork]);
        let mut f = ForkForm::for_session(&row("claude", None));
        assert_eq!(f.rows(), [Row::Prompt, Row::Name, Row::Fork]);
        // Even if set, it isn't asked for.
        f.worktree = true;
        assert!(f.params().get("workspace").is_none());
    }

    #[test]
    fn the_status_line_says_what_to_know() {
        let v = json!({"name": "api-fork", "forked_from_name": "api",
                       "warnings": ["it shares its folder with api (working)"]});
        assert_eq!(
            fork_status(&v),
            "forked api → api-fork · it shares its folder with api (working)"
        );
    }

    #[test]
    fn the_form_draws_its_hints() {
        use ratatui::backend::TestBackend;
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let app = App::new(rx, Config::default(), keys);
        let f = ForkForm::for_session(&row("claude", Some("/src/api")));
        let mut term = ratatui::Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|fr| app.draw_fork_form(fr, &f)).unwrap();
        let buf = term.backend().buffer();
        let out: String = (0..24)
            .map(|y| {
                (0..100)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        for want in [
            "Fork api",
            "First prompt",
            "(optional)",
            "chosen on fork",
            "the same folder",
            "[ Fork ]",
        ] {
            assert!(out.contains(want), "{want:?} missing:\n{out}");
        }
    }

    /// The fork's answer comes in while the list stays live: the new
    /// session is selected, or the form comes back with the error.
    #[test]
    fn a_fork_answer_selects_it_or_reopens_the_form() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        assert!(!app.poll_fork(), "nothing on its way");
        let form = || Box::new(ForkForm::for_session(&row("claude", Some("/src/api"))));
        let (tx, answer) = mpsc::channel();
        app.fork_send = Some((answer, form()));
        assert!(!app.poll_fork(), "not answered yet");
        tx.send(Ok(
            json!({"id": "01f", "name": "api-fork", "forked_from_name": "api",
                          "warnings": []}),
        ))
        .unwrap();
        assert!(app.poll_fork());
        assert_eq!(app.pending_select.as_deref(), Some("01f"));
        assert_eq!(app.status_line, "forked api → api-fork");
        assert!(app.fork_send.is_none());
        let (tx, answer) = mpsc::channel();
        app.fork_send = Some((answer, form()));
        tx.send(Err("api's folder is gone".into())).unwrap();
        assert!(app.poll_fork());
        let Mode::Fork(f) = &app.mode else {
            panic!("form not reopened")
        };
        assert_eq!(f.error.as_deref(), Some("api's folder is gone"));
    }

    #[test]
    fn the_overlord_and_an_empty_list_open_nothing() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.open_fork_form();
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status_line.contains("nothing selected"));
    }
}
