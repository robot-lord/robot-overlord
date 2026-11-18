//! The first-run offer to import existing work (`setup.import_offer`):
//! during initial setup (no sessions besides the overlords), once the
//! code-folder step is behind the user, rlord explains import and asks
//! once. `y` opens the overlord and sends it a fixed request that starts
//! its import flow (the user approves the list there); `d` is remembered.

use super::*;
use ratatui::widgets::Clear;
use std::time::Instant;

/// The dialog's width, borders included.
const WIDTH: u16 = 74;

/// Keys pressed this soon after it opens are dropped.
const GRACE: std::time::Duration = std::time::Duration::from_millis(800);

/// Where you can do it later, said on `d`/Esc and in the dialog.
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
        // Off this thread: it may read a few transcripts. Errors are
        // silent (no offer); `poll_import_offer` takes the answer.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ =
                tx.send(client_call(&paths::socket_path(), "setup.import_offer", Value::Null).ok());
        });
        self.import_offer_rx = Some(rx);
    }

    /// Open a pending offer once nothing else is open, and say how a
    /// `y` went (`setup.import_answer` runs in the background: a stopped
    /// overlord can take a while). True when the screen changed.
    pub(crate) fn poll_import_offer(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.import_offer_rx {
            match rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => {}
                r => {
                    self.import_offer_rx = None;
                    match r.ok().flatten() {
                        Some(v) => self.import_offer_ready = v["offer"] == true,
                        // Not reached: not checked, so the next trigger asks.
                        None => self.import_offer_checked = false,
                    }
                }
            }
        }
        // The offer left the screen some way other than its own keys (a
        // pushed form, say), or turned out not to apply: the overlay it
        // held back opens now.
        if self.import_then_overlord
            && self.import_offer_rx.is_none()
            && !self.import_offer_ready
            && !matches!(self.mode, Mode::ImportOffer(_) | Mode::ConfirmQuit)
        {
            self.overlay_after_offer();
        }
        if let Some(rx) = &self.import_send {
            let r = match rx.try_recv() {
                Ok(r) => Some(r),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err("the request was lost".into())),
            };
            if let Some(r) = r {
                self.import_send = None;
                changed = true;
                match r {
                    Ok(()) => {
                        self.status_line =
                            "sent to the overlord: it proposes a list for you to approve".into();
                    }
                    Err(e) => {
                        self.status_line = format!("couldn't reach the overlord: {e}");
                        self.import_offer_error = Some(format!("not sent: {e}"));
                        self.import_offer_ready = true;
                    }
                }
            }
        }
        if !self.import_offer_ready || !matches!(self.mode, Mode::Normal) {
            return changed;
        }
        self.import_offer_ready = false;
        self.mode = Mode::ImportOffer(Instant::now());
        true
    }

    pub(crate) fn handle_import_offer(&mut self, k: KeyEvent, opened: Instant) {
        // It can open while the user is typing in the list: keys pressed
        // before they could have seen it are dropped.
        if opened.elapsed() < GRACE {
            self.mode = Mode::ImportOffer(opened);
            return;
        }
        // Quitting is never trapped; staying brings the offer back.
        if self.keys.action(&k) == Some(Action::Quit) {
            self.import_offer_ready = true;
            self.mode = Mode::ConfirmQuit;
            return;
        }
        match k.code {
            KeyCode::Char('y') => {
                // Sent in the background; `poll_import_offer` says how it
                // went. The daemon records the answer only once it's sent.
                self.import_offer_error = None;
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let r = client_call(
                        &paths::socket_path(),
                        "setup.import_answer",
                        json!({"answer": "yes"}),
                    )
                    .map(drop)
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(r);
                });
                self.import_send = Some(rx);
                self.status_line = "sending to the overlord…".into();
                self.import_then_overlord = false;
                self.show_overlord();
            }
            KeyCode::Char('d') => {
                match client_call(
                    &paths::socket_path(),
                    "setup.import_answer",
                    json!({"answer": "no"}),
                ) {
                    Ok(_) => {
                        self.import_offer_error = None;
                        self.status_line = format!("won't ask again · {LATER}");
                        self.overlay_after_offer();
                    }
                    // Not recorded: the offer comes back with why.
                    Err(e) => {
                        self.import_offer_error = Some(format!("not saved: {e:#}"));
                        self.import_offer_ready = true;
                    }
                }
            }
            // Later: nothing recorded, so it asks again next start.
            KeyCode::Esc => {
                self.status_line = format!("asks again next start · or {LATER}");
                self.overlay_after_offer();
            }
            _ => self.mode = Mode::ImportOffer(opened),
        }
    }

    /// The offer closed without `y`: the overlay it held back (first
    /// run) opens now.
    fn overlay_after_offer(&mut self) {
        if std::mem::take(&mut self.import_then_overlord) {
            self.show_overlord();
        }
    }

    /// Show the overlay unless the import offer is about to open: then
    /// it opens once the offer closes (`overlay_after_offer`), not under it.
    pub(crate) fn show_overlord_after_offer(&mut self) {
        if self.import_offer_ready || self.import_offer_rx.is_some() {
            self.import_then_overlord = true;
        } else {
            self.show_overlord();
        }
    }

    pub(crate) fn draw_import_offer(&self, f: &mut Frame) {
        let mut lines = vec![
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
            Line::from(Span::styled(
                "  Claude Code and Codex conversations can be imported (not OpenCode).",
                theme::muted(),
            )),
            Line::from(Span::styled(format!("  Later: {LATER}."), theme::faint())),
            Line::from(""),
            keys_line(&[
                ("y", "import my sessions", Cap::Context),
                ("d", "don't ask again", Cap::Context),
            ]),
            keys_line(&[("esc", "later (asks next start)", Cap::Global)]),
        ];
        if let Some(e) = &self.import_offer_error {
            lines.push(Line::from(Span::styled(
                format!("  {e}"),
                Style::default().fg(theme::red()),
            )));
        }
        // Tall enough for every line as wrapped, so the keys never clip.
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
                .block(theme::modal("Import your sessions")),
            area,
        );
    }
}
