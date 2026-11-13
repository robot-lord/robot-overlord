//! Pairing a phone with this machine for Codex Remote Control: the dialog
//! that shows the code. The overlord asks for it (`remote_pair`); the daemon
//! starts the pairing and tells the TUI to open this, and the TUI fetches
//! the code itself (`remote.pair_code`), so it never passes through the
//! overlord. It refreshes when the daemon says something changed (a phone
//! used the code).

use super::*;
use ratatui::widgets::Clear;

pub(crate) struct Pair {
    code: String,
    /// ms since the epoch (None: not said).
    expires_at: Option<i64>,
    claimed: bool,
    note: Option<String>,
}

impl Pair {
    fn from(v: &Value) -> Option<Pair> {
        if v["open"] != true {
            return None;
        }
        Some(Pair {
            code: v["code"].as_str()?.to_string(),
            expires_at: v["expires_at"].as_i64(),
            claimed: v["claimed"] == true,
            note: None,
        })
    }

    /// "4:12 left", or that it expired.
    fn left(&self, now_ms: i64) -> Option<String> {
        let e = self.expires_at?;
        let s = (e - now_ms) / 1000;
        Some(if s <= 0 {
            "expired: ask for a new one".into()
        } else {
            format!("{}:{:02} left", s / 60, s % 60)
        })
    }
}

fn fetch() -> Option<Pair> {
    client_call(&paths::socket_path(), "remote.pair_code", json!({}))
        .ok()
        .as_ref()
        .and_then(Pair::from)
}

impl App {
    /// The daemon says a pairing code is ready: show it (never over
    /// something the user is typing into).
    pub(crate) fn open_pairing(&mut self) {
        if !matches!(self.mode, Mode::Normal | Mode::Help | Mode::RemotePair(_)) {
            self.status_line =
                "a phone pairing code is ready: finish here, then run `rlord remote pair`".into();
            return;
        }
        match fetch() {
            Some(p) => {
                self.mode = Mode::RemotePair(Box::new(p));
                self.status_line = "the overlord opened phone pairing".into();
            }
            None => self.status_line = "the pairing code expired: ask for a new one".into(),
        }
    }

    /// On a refresh: whether a phone used the code yet.
    pub(crate) fn refresh_pairing(&mut self) {
        if let Mode::RemotePair(p) = &mut self.mode {
            if p.claimed {
                return;
            }
            if let Some(n) = fetch() {
                p.claimed = n.claimed;
            }
        }
    }

    pub(crate) fn handle_pairing(&mut self, k: KeyEvent, mut p: Box<Pair>) {
        match k.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.status_line = if p.claimed {
                    "phone paired: Codex sessions with Remote Control are in the ChatGPT app".into()
                } else {
                    "pairing closed (the code works until it expires)".into()
                };
            }
            KeyCode::Char('y') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                overlay::copy_text("rlord-pair", &p.code);
                p.note = Some("copied".into());
                self.mode = Mode::RemotePair(p);
            }
            _ => self.mode = Mode::RemotePair(p),
        }
    }

    pub(crate) fn draw_pairing(&self, f: &mut Frame, p: &Pair) {
        let area = newform::centered(f.area(), 66, 13);
        f.render_widget(Clear, area);
        let now = rlord_core::types::now_ms();
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Pair the ChatGPT app on your phone with this machine:",
                Style::default().bold(),
            )),
            Line::from(Span::styled(
                "  in the app, connect to a computer for Codex and enter",
                theme::muted(),
            )),
            Line::from(""),
            Line::from(vec![
                Span::raw("      "),
                Span::styled(p.code.clone(), Style::default().bold().fg(theme::accent())),
                Span::raw("   "),
                Span::styled(
                    if p.claimed {
                        "✓ paired".to_string()
                    } else {
                        p.left(now).unwrap_or_default()
                    },
                    if p.claimed {
                        Style::default().fg(theme::green())
                    } else {
                        theme::faint()
                    },
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  A paired phone reaches every Codex session here on Codex's",
                theme::faint(),
            )),
            Line::from(Span::styled(
                "  background server. Don't share the code.",
                theme::faint(),
            )),
            Line::from(""),
        ];
        let mut keys = vec![("esc", "close", Cap::Global)];
        if !p.claimed {
            keys.insert(0, ("^Y", "copy", Cap::Context));
        }
        lines.push(keys_line(&keys));
        if let Some(n) = &p.note {
            lines.push(Line::from(Span::styled(format!("  {n}"), theme::faint())));
        }
        self.modal_hits(f.area(), area, &lines);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(theme::modal("⇄ Pair your phone")),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialog_reads_the_code_and_counts_down() {
        assert!(Pair::from(&json!({"open": false})).is_none());
        let p = Pair::from(
            &json!({"open": true, "code": "AB-12", "expires_at": 100_000,
                                   "claimed": false}),
        )
        .unwrap();
        assert_eq!(p.code, "AB-12");
        assert_eq!(
            p.left(100_000).as_deref(),
            Some("expired: ask for a new one")
        );
        assert_eq!(p.left(100_000 - 252_000).as_deref(), Some("4:12 left"));
        let none = Pair::from(&json!({"open": true, "code": "X"})).unwrap();
        assert_eq!(none.left(0), None);
    }
}
