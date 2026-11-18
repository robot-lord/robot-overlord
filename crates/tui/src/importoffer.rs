//! The first-run offer to import existing work (`setup.import_offer`):
//! during initial setup (no sessions besides the overlords), once the
//! code-folder step is behind the user, rlord explains import and asks
//! once. `y` opens the overlord and sends it a fixed request that starts
//! its import flow (the user approves the list there); `n` is remembered.

use super::*;
use ratatui::widgets::Clear;

/// Where you can do it later, said on `n` and in the dialog.
pub(crate) const LATER: &str = "ask the overlord \"import my sessions\" any time";

impl App {
    /// Open the offer if the daemon says it applies, once a run, when
    /// nothing else is open (else on the next pass: `poll_import_offer`).
    pub(crate) fn start_import_offer(&mut self) {
        if self.import_offer_checked {
            return;
        }
        self.import_offer_checked = true;
        // (Tests never reach a real daemon.)
        if cfg!(test) {
            return;
        }
        // Cheap: one kv row and the session list.
        self.import_offer_ready = self
            .call("setup.import_offer", Value::Null)
            .is_some_and(|v| v["offer"] == true);
    }

    /// Open a pending offer once nothing else is open. True when the
    /// screen changed.
    pub(crate) fn poll_import_offer(&mut self) -> bool {
        if !self.import_offer_ready || !matches!(self.mode, Mode::Normal) {
            return false;
        }
        self.import_offer_ready = false;
        self.mode = Mode::ImportOffer;
        true
    }

    pub(crate) fn handle_import_offer(&mut self, k: KeyEvent) {
        if self.keys.action(&k) == Some(Action::Quit) {
            self.mode = Mode::ConfirmQuit;
            return;
        }
        match k.code {
            KeyCode::Char('y') => {
                // In the background: a stopped overlord can take a while
                // to start. The answer is recorded before it's sent.
                std::thread::spawn(|| {
                    let _ = client_call(
                        &paths::socket_path(),
                        "setup.import_answer",
                        json!({"answer": "yes"}),
                    );
                });
                self.status_line =
                    "sent to the overlord: it proposes a list for you to approve".into();
                self.show_overlord();
            }
            KeyCode::Char('n') => {
                if self
                    .call("setup.import_answer", json!({"answer": "no"}))
                    .is_some()
                {
                    self.status_line = format!("not importing · {LATER}");
                }
            }
            // Later: nothing recorded, so it asks again next start.
            KeyCode::Esc => {
                self.status_line = "asks again next start · n stops asking".into();
            }
            _ => self.mode = Mode::ImportOffer,
        }
    }

    pub(crate) fn draw_import_offer(&self, f: &mut Frame) {
        let area = newform::centered(f.area(), 74, 13);
        f.render_widget(Clear, area);
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Bring in the work you already have?",
                Style::default().bold(),
            )),
            Line::from(Span::styled(
                "  rlord can import conversations you started outside it (a terminal,",
                theme::muted(),
            )),
            Line::from(Span::styled(
                "  the desktop app, another session manager). Each becomes a session",
                theme::muted(),
            )),
            Line::from(Span::styled(
                "  here that resumes it. The overlord finds them and proposes which to",
                theme::muted(),
            )),
            Line::from(Span::styled(
                "  import, archive or skip; nothing is imported until you approve.",
                theme::muted(),
            )),
            Line::from(Span::styled(format!("  Later: {LATER}."), theme::faint())),
            Line::from(""),
            keys_line(&[
                ("y", "import my sessions", Cap::Context),
                ("n", "not now (won't ask again)", Cap::Context),
            ]),
            keys_line(&[("esc", "ask next start", Cap::Global)]),
        ];
        self.modal_hits(f.area(), area, &lines);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(theme::modal("Import your sessions")),
            area,
        );
    }
}
