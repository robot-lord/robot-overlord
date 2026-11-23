//! The one-time question about `[limits] auto_resume`: while it is unset,
//! the first time a listed session is stopped on a usage limit, rlord
//! asks whether to pick limited sessions up by itself when their limit
//! resets. `y` turns it on, `n` off, both saved in rlord's own config
//! (`limits.auto_resume`); Esc asks again next start. The first-run
//! setup form asks the same (`setupform`).

use super::*;
use timed::Shown;

/// The dialog's width, borders included.
const WIDTH: u16 = 72;

/// Whether to ask now: not chosen yet (`auto_resume` unset) and a session
/// other than an overlord in `rows` is stopped on a usage limit.
pub(crate) fn wants_ask(auto_resume: Option<bool>, rows: &[Value]) -> bool {
    auto_resume.is_none()
        && rows
            .iter()
            .any(|s| !s["limit"].is_null() && !is_overlord_role(s))
}

/// The daemon couldn't be reached at all (the socket: an I/O error), as
/// opposed to answering with an error. Only then is it asked again on a
/// later refresh; an error answer would only repeat on every one.
pub(crate) fn unreachable(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<std::io::Error>())
}

/// The message a resume types, as the dialog says it (`[limits]
/// resume_message`, "continue" when blank, as the daemon sends).
pub(crate) fn resume_text(cfg: &Config) -> String {
    cfg.limits.message()
}

impl App {
    /// Ask the question if it applies, once a run: it opens once nothing
    /// else is (`poll_auto_resume`). The daemon's config is asked first:
    /// another rlord window may have answered it already.
    pub(crate) fn offer_auto_resume(&mut self) {
        // (Only from the list: the daemon call below blocks.)
        if self.auto_resume_asked
            || self.auto_resume_pending
            || !matches!(self.mode, Mode::Normal)
            || !wants_ask(self.config.limits.auto_resume, &self.all)
        {
            return;
        }
        self.auto_resume_asked = true;
        // (Tests never reach a real daemon.)
        if cfg!(test) {
            return;
        }
        let v = match client_call(&paths::socket_path(), "limits.auto_resume", json!({})) {
            Ok(v) => v,
            Err(e) => {
                // Not reached: asked again on a later refresh. Any other
                // error: not again this run.
                self.auto_resume_asked = !unreachable(&e);
                return;
            }
        };
        if let Some(on) = v["auto_resume"].as_bool() {
            self.config.limits.auto_resume = Some(on);
            return;
        }
        self.auto_resume_error = None;
        self.auto_resume_pending = true;
    }

    /// Open a pending question once nothing else is open. Not from inside
    /// `refresh`: a refresh right before an attach would open it while
    /// the terminal goes to tmux. True when the screen changed.
    pub(crate) fn poll_auto_resume(&mut self) -> bool {
        if !self.auto_resume_pending || !matches!(self.mode, Mode::Normal) {
            return false;
        }
        self.auto_resume_pending = false;
        // Answered meanwhile (the setup form, the overlord's tool, another
        // window): don't ask over it. The daemon's value first.
        if !cfg!(test) {
            if let Ok(v) = client_call(&paths::socket_path(), "limits.auto_resume", json!({})) {
                if let Some(on) = v["auto_resume"].as_bool() {
                    self.config.limits.auto_resume = Some(on);
                }
            }
        }
        if !wants_ask(self.config.limits.auto_resume, &self.all) {
            return false;
        }
        self.mode = Mode::AutoResume(Shown::default());
        true
    }

    pub(crate) fn handle_auto_resume(&mut self, k: KeyEvent, shown: Shown) {
        // It opens on a background refresh, maybe mid-typing: keys pressed
        // before the user could have seen it are dropped.
        if !shown.ready() {
            self.mode = Mode::AutoResume(shown);
            return;
        }
        // Quitting is never trapped; staying brings the question back
        // (on the next refresh).
        if self.keys.action(&k) == Some(Action::Quit) {
            self.auto_resume_asked = false;
            self.mode = Mode::ConfirmQuit;
            return;
        }
        let on = match k.code {
            KeyCode::Char('y') => true,
            KeyCode::Char('n') => false,
            // Later: nothing saved, so it asks again next start.
            KeyCode::Esc => {
                self.status_line =
                    "auto-resume not chosen · asks again next start · [limits] auto_resume".into();
                return;
            }
            _ => {
                self.mode = Mode::AutoResume(shown);
                return;
            }
        };
        match client_call(
            &paths::socket_path(),
            "limits.auto_resume",
            json!({"on": on}),
        ) {
            Ok(_) => {
                self.config.limits.auto_resume = Some(on);
                self.status_line = if on {
                    "auto-resume on: limited sessions carry on when their limit resets · [limits] auto_resume"
                } else {
                    "auto-resume off: resume a session from its menu · [limits] auto_resume"
                }
                .into();
                self.dirty = true;
            }
            // Not saved: the question stays, with why.
            Err(e) => {
                self.auto_resume_error = Some(format!("not saved: {e:#}"));
                self.mode = Mode::AutoResume(shown);
            }
        }
    }

    pub(crate) fn draw_auto_resume(&self, f: &mut Frame, shown: &Shown) {
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  A session stopped on a usage limit.",
                Style::default().bold(),
            )),
            Line::from(Span::styled(
                "  Pick limited sessions up by themselves when their limit resets?",
                theme::muted(),
            )),
            Line::from(Span::styled(
                format!(
                    "  rlord types \"{}\" ([limits] resume_message) a minute after",
                    resume_text(&self.config)
                ),
                theme::muted(),
            )),
            Line::from(Span::styled(
                "  the reset, unless you used the session meanwhile. Cancel one from",
                theme::muted(),
            )),
            Line::from(Span::styled(
                "  its menu. Not the overlord: switch agents for that.",
                theme::muted(),
            )),
            Line::from(Span::styled(
                "  Saved as [limits] auto_resume in rlord's config.toml.",
                theme::faint(),
            )),
            Line::from(""),
            keys_line(&[
                ("y", "on", Cap::Context),
                ("n", "off (resume from a session's menu)", Cap::Context),
            ]),
            keys_line(&[("esc", "later (asks next start)", Cap::Global)]),
        ];
        if let Some(e) = &self.auto_resume_error {
            lines.push(Line::from(Span::styled(
                format!("  {e}"),
                Style::default().fg(theme::red()),
            )));
        }
        self.draw_timed(f, shown, WIDTH, "Auto-resume after limits", lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Quitting from the question, then staying: it comes back.
    #[test]
    fn backing_out_of_quit_brings_the_question_back() {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.auto_resume_asked = true;
        app.handle_auto_resume(
            KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::NONE),
            Shown::seen_long_ago(),
        );
        assert!(matches!(app.mode, Mode::ConfirmQuit));
        assert!(!app.auto_resume_asked, "asked again once back in the list");
    }

    fn app() -> App {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        App::new(rx, Config::default(), keys)
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// Answered meanwhile (the setup form, the overlord): the pending
    /// question doesn't open over the answer.
    #[test]
    fn a_pending_question_answered_meanwhile_stays_shut() {
        let mut app = app();
        app.all = vec![json!({"id": "a", "limit": {"until": "3:00pm"}})];
        app.auto_resume_pending = true;
        app.config.limits.auto_resume = Some(true);
        assert!(!app.poll_auto_resume());
        assert!(matches!(app.mode, Mode::Normal));
        assert!(!app.auto_resume_pending);
    }

    /// Not asked (nor the daemon called) outside the list.
    #[test]
    fn offered_only_from_the_list() {
        let mut app = app();
        app.all = vec![json!({"id": "a", "limit": {"until": "3:00pm"}})];
        app.mode = Mode::Filter(String::new());
        app.offer_auto_resume();
        assert!(!app.auto_resume_asked);
        app.mode = Mode::Normal;
        app.offer_auto_resume();
        assert!(app.auto_resume_asked);
    }

    /// Asked during a refresh right before an attach: it doesn't open
    /// then, and once open, a key before it was drawn doesn't answer it
    /// however long it has been open.
    #[test]
    fn a_key_before_the_dialog_is_drawn_doesnt_answer_it() {
        let mut app = app();
        app.all = vec![json!({"id": "a", "limit": {"until": "3:00pm"}})];
        app.auto_resume_pending = true;
        app.mode = Mode::Filter(String::new());
        assert!(!app.poll_auto_resume(), "not over another mode");
        app.mode = Mode::Normal;
        assert!(app.poll_auto_resume());
        assert!(!app.auto_resume_pending);
        let Mode::AutoResume(shown) = std::mem::replace(&mut app.mode, Mode::Normal) else {
            panic!("not opened");
        };
        app.handle_auto_resume(key('y'), shown);
        assert!(matches!(app.mode, Mode::AutoResume(_)), "y dropped");
        assert_eq!(app.config.limits.auto_resume, None);
    }

    /// A form the overlord pushes over the question doesn't lose it: it
    /// opens again once the list is back.
    #[test]
    fn a_pushed_form_keeps_the_question_pending() {
        let mut app = app();
        app.all = vec![json!({"id": "a", "limit": {"until": "3:00pm"}})];
        app.mode = Mode::AutoResume(Shown::default());
        app.handle_ui_cmd(json!({"cmd": "new_session", "prefill": {}}));
        assert!(!matches!(app.mode, Mode::AutoResume(_)));
        assert!(app.auto_resume_pending);
        app.mode = Mode::Normal;
        assert!(app.poll_auto_resume());
        assert!(matches!(app.mode, Mode::AutoResume(_)));
    }

    /// On a narrow terminal the dialog is tall enough for its wrapped
    /// lines: the key lines still show.
    #[test]
    fn the_keys_show_on_a_narrow_terminal() {
        let app = app();
        let backend = ratatui::backend::TestBackend::new(40, 40);
        let mut t = ratatui::Terminal::new(backend).unwrap();
        let shown = Shown::default();
        t.draw(|f| app.draw_auto_resume(f, &shown)).unwrap();
        let text: String = t
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("later"), "esc line clipped");
        assert!(!shown.ready(), "grace starts at the draw");
        // Every keycap click area sits on the row its key is drawn on.
        let buf = t.backend().buffer().clone();
        for y in 0..40 {
            let row: String = (0..40).map(|x| buf[(x, y)].symbol().to_string()).collect();
            for x in 0..40 {
                if let Some(mouse::Hit::Key(k)) = app.hits.at(x, y) {
                    let cap = match k.code {
                        KeyCode::Esc => "esc",
                        KeyCode::Char('y') => " y ",
                        KeyCode::Char('n') => " n ",
                        _ => continue,
                    };
                    assert!(row.contains(cap), "{cap} hit on row {y}: {row}");
                }
            }
        }
    }

    #[test]
    fn the_dialog_names_the_configured_message() {
        let mut cfg = Config::default();
        assert_eq!(resume_text(&cfg), "continue");
        cfg.limits.resume_message = "  keep going ".into();
        assert_eq!(resume_text(&cfg), "keep going");
        cfg.limits.resume_message = " ".into();
        assert_eq!(resume_text(&cfg), "continue");
    }

    /// Only a daemon that couldn't be reached is asked again.
    #[test]
    fn only_an_unreachable_daemon_is_asked_again() {
        let io = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
            .context("connecting to daemon");
        assert!(unreachable(&io));
        assert!(!unreachable(&anyhow::anyhow!(
            "unknown method limits.auto_resume"
        )));
    }

    #[test]
    fn asked_only_while_unset_and_a_session_is_limited() {
        let limited = json!({"id": "a", "limit": {"until": "3:00pm"}});
        let free = json!({"id": "b", "limit": null});
        assert!(wants_ask(None, &[free.clone(), limited.clone()]));
        assert!(!wants_ask(None, std::slice::from_ref(&free)));
        assert!(!wants_ask(Some(true), std::slice::from_ref(&limited)));
        assert!(!wants_ask(Some(false), std::slice::from_ref(&limited)));
        // The overlord has its own offer (switch agents).
        let overlord = json!({"id": "o", "role": "overlord", "limit": {"until": "3:00pm"}});
        assert!(is_overlord_role(&overlord));
        assert!(!wants_ask(None, &[overlord]));
    }
}
