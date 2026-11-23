//! The one-time question about `[limits] auto_resume`: while it is unset,
//! the first time a listed session is stopped on a usage limit, rlord
//! asks whether to pick limited sessions up by itself when their limit
//! resets. `y` turns it on, `n` off, both saved in rlord's own config
//! (`limits.auto_resume`); Esc asks again next start. The first-run
//! setup form asks the same (`setupform`).

use super::*;
use ratatui::widgets::Clear;
use std::time::Instant;

/// The dialog's width, borders included.
const WIDTH: u16 = 72;

/// Keys pressed this soon after it opens are dropped.
const GRACE: std::time::Duration = std::time::Duration::from_millis(800);

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
    let m = cfg.limits.resume_message.trim();
    if m.is_empty() { "continue" } else { m }.to_string()
}

impl App {
    /// Open the question if it applies, once a run, when nothing else is
    /// open. The daemon's config is asked first: another rlord window may
    /// have answered it already.
    pub(crate) fn offer_auto_resume(&mut self) {
        if self.auto_resume_asked
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
        self.mode = Mode::AutoResume(Instant::now());
    }

    pub(crate) fn handle_auto_resume(&mut self, k: KeyEvent, opened: Instant) {
        // It opens on a background refresh, maybe mid-typing: keys pressed
        // before the user could have seen it are dropped.
        if opened.elapsed() < GRACE {
            self.mode = Mode::AutoResume(opened);
            return;
        }
        // Quitting is never trapped; it asks again next start.
        if self.keys.action(&k) == Some(Action::Quit) {
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
                self.mode = Mode::AutoResume(opened);
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
                self.mode = Mode::AutoResume(opened);
            }
        }
    }

    pub(crate) fn draw_auto_resume(&self, f: &mut Frame) {
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
        let rows: usize = lines
            .iter()
            .map(|l| l.width().div_ceil(usize::from(WIDTH - 2)).max(1))
            .sum();
        let area = newform::centered(f.area(), WIDTH, u16::try_from(rows + 2).unwrap_or(u16::MAX));
        f.render_widget(Clear, area);
        self.modal_hits(f.area(), area, &lines);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(theme::modal("Auto-resume after limits")),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
