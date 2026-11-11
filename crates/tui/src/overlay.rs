//! `rlord overlay`: the overlord as an overlay. It runs inside a tmux popup
//! (`display-popup`) over whatever the user is looking at — an agent session
//! or the rlord list — and draws the overlord's live pane, forwarding every
//! keystroke to it. The overlord's tmux window is sized to the popup, so the
//! agent lays itself out for exactly the space it's shown in.
//!
//! Option-O (or Option-D) closes it: inside a popup, keys reach this program
//! rather than tmux's key table, so the key that opened it closes it.

use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::style::{Color, Stylize};
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use serde_json::json;

use rlord_core::paths;
use rlord_core::tmux;
use rlord_daemon::client_call;
use rlord_daemon::overlord::{self, Seat};

/// Frame interval: how often the overlord's pane is re-captured.
const FRAME_MS: u64 = 40;

/// `token` identifies this overlay to the daemon (it was minted when the
/// daemon opened the popup).
pub fn run(token: Option<u64>, seat: &str) -> Result<()> {
    let seat = overlord::seat(seat);
    let pane = tmux::pane_id(seat.tmux)?;
    let report = |open: bool| {
        let _ = client_call(
            &paths::socket_path(),
            "ui.overlay.report",
            json!({"open": open, "token": token}),
        );
    };
    report(true);
    let mut terminal = ratatui::init();
    let _ = ratatui::crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
    let keys = SeatKeys::from_config(&rlord_core::config::Config::load().unwrap_or_default());
    let r = run_loop(&mut terminal, &pane, seat, &keys);
    let _ = ratatui::crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    report(false);
    // The window follows clients again once nobody is embedding it.
    let _ = tmux::auto_size_window(seat.tmux);
    r
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    pane: &str,
    seat: &Seat,
    keys: &SeatKeys,
) -> Result<()> {
    let mut size = (0u16, 0u16);
    let mut screen = Text::default();
    let mut cursor: Option<(u16, u16)> = None;
    let mut error: Option<String> = None;
    let mut last = Instant::now() - Duration::from_secs(1);
    loop {
        let area = terminal.size()?;
        if (area.width, area.height) != size && area.width > 0 && area.height > 0 {
            size = (area.width, area.height);
            if let Err(e) = tmux::resize_window(seat.tmux, size.0, size.1) {
                error = Some(format!("{e:#}"));
            }
            last = Instant::now() - Duration::from_secs(1);
        }
        if last.elapsed() >= Duration::from_millis(FRAME_MS) {
            last = Instant::now();
            match tmux::capture_frame(pane) {
                Ok(f) => {
                    screen = f.ansi.as_bytes().into_text().unwrap_or_default();
                    cursor = f.cursor_visible.then_some((f.cursor_x, f.cursor_y));
                    error = None;
                }
                Err(e) => error = Some(format!("{e:#}")),
            }
        }
        terminal.draw(|f| {
            let a = f.area();
            match &error {
                Some(e) => f.render_widget(
                    Paragraph::new(format!("overlord unavailable: {e}")).fg(Color::Red),
                    a,
                ),
                None => {
                    f.render_widget(Paragraph::new(screen.clone()), a);
                    if let Some((x, y)) = cursor.filter(|(x, y)| *x < a.width && *y < a.height) {
                        f.set_cursor_position((a.x + x, a.y + y));
                    }
                }
            }
        })?;
        if !event::poll(Duration::from_millis(FRAME_MS / 2))? {
            continue;
        }
        match event::read()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                match keys.seat_key(&k) {
                    // This seat's key (or Option-D) closes the overlay.
                    Some(s) if s == seat.key || s == "close" => return Ok(()),
                    // The other seat's key switches to it (the daemon opens
                    // its overlay in place of this one).
                    Some(other) => {
                        // Hand this seat's window its size back first: the
                        // new overlay replaces (kills) this one.
                        let _ = tmux::auto_size_window(seat.tmux);
                        match client_call(
                            &paths::socket_path(),
                            "ui.overlord",
                            json!({"seat": other, "show": true}),
                        ) {
                            Ok(_) => return Ok(()),
                            // Stay, and say why the switch didn't happen.
                            Err(e) => {
                                error = Some(format!("couldn't switch to the {other}: {e:#}"))
                            }
                        }
                        continue;
                    }
                    None => {}
                }
                let r = match tmux_key(&k) {
                    Some(TmuxKey::Text(t)) => tmux::send_text(pane, &t),
                    Some(TmuxKey::Named(n)) => tmux::send_keys(pane, &[n.as_str()]),
                    None => Ok(()),
                };
                if let Err(e) = r {
                    error = Some(format!("{e:#}"));
                }
                last = Instant::now() - Duration::from_secs(1); // show the echo now
            }
            Event::Paste(t) => {
                if let Err(e) = tmux::paste(pane, &t) {
                    error = Some(format!("{e:#}"));
                }
                last = Instant::now() - Duration::from_secs(1);
            }
            _ => {}
        }
    }
}

/// Option-O (the key that opened it) or Option-D (the "back" key) closes it.
/// The overlay's own keys, from the configured tmux keys: the overlord's
/// (Option-O), the mastermind's (Option-M) and back (Option-D, closes).
/// Inside a popup, keys reach this program, not tmux's key table.
struct SeatKeys {
    overlord: Option<(bool, bool, char)>,
    mastermind: Option<(bool, bool, char)>,
    close: Option<(bool, bool, char)>,
}

/// A tmux key spec (`M-o`, `C-g`, `M-C-x`) as (alt, ctrl, char).
fn parse_key(spec: &str) -> Option<(bool, bool, char)> {
    let (mut alt, mut ctrl, mut rest) = (false, false, spec.trim());
    loop {
        if let Some(r) = rest.strip_prefix("M-") {
            alt = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("C-") {
            ctrl = true;
            rest = r;
        } else {
            break;
        }
    }
    let mut chars = rest.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some((alt, ctrl, c))
}

impl SeatKeys {
    fn from_config(cfg: &rlord_core::config::Config) -> Self {
        let or = |k: &str, d: &str| parse_key(if k.trim().is_empty() { d } else { k });
        SeatKeys {
            overlord: or(&cfg.tmux.overlord, "M-o"),
            mastermind: parse_key(cfg.tmux.mastermind_switch()),
            close: or(&cfg.tmux.back, "M-d"),
        }
    }

    fn seat_key(&self, k: &KeyEvent) -> Option<&'static str> {
        let KeyCode::Char(c) = k.code else {
            return None;
        };
        let got = (
            k.modifiers.contains(KeyModifiers::ALT),
            k.modifiers.contains(KeyModifiers::CONTROL),
            c,
        );
        if self.overlord == Some(got) {
            Some(overlord::FRONT.key)
        } else if self.mastermind == Some(got) {
            Some(overlord::MASTERMIND.key)
        } else if self.close == Some(got) {
            Some("close")
        } else {
            None
        }
    }
}

enum TmuxKey {
    Text(String),
    Named(String),
}

/// A crossterm key as tmux `send-keys` input.
fn tmux_key(k: &KeyEvent) -> Option<TmuxKey> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    let prefix = || {
        let mut n = String::new();
        if ctrl {
            n.push_str("C-");
        }
        if alt {
            n.push_str("M-");
        }
        n
    };
    let named = |base: &str| {
        let mut n = prefix();
        if shift {
            n.push_str("S-");
        }
        n.push_str(base);
        Some(TmuxKey::Named(n))
    };
    match k.code {
        KeyCode::Char(c) if ctrl || alt => Some(TmuxKey::Named(format!("{}{c}", prefix()))),
        KeyCode::Char(c) => Some(TmuxKey::Text(c.to_string())),
        KeyCode::Enter => named("Enter"),
        KeyCode::Esc => named("Escape"),
        KeyCode::Backspace => named("BSpace"),
        KeyCode::Tab => named("Tab"),
        KeyCode::BackTab => Some(TmuxKey::Named("BTab".into())),
        KeyCode::Up => named("Up"),
        KeyCode::Down => named("Down"),
        KeyCode::Left => named("Left"),
        KeyCode::Right => named("Right"),
        KeyCode::Home => named("Home"),
        KeyCode::End => named("End"),
        KeyCode::PageUp => named("PPage"),
        KeyCode::PageDown => named("NPage"),
        KeyCode::Delete => named("DC"),
        KeyCode::Insert => named("IC"),
        KeyCode::F(n) => named(&format!("F{n}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        let mut k = KeyEvent::new(code, m);
        k.kind = KeyEventKind::Press;
        k
    }

    fn name(k: KeyEvent) -> String {
        match tmux_key(&k) {
            Some(TmuxKey::Named(n)) => n,
            Some(TmuxKey::Text(t)) => format!("text:{t}"),
            None => "none".into(),
        }
    }

    #[test]
    fn keys_map_to_tmux_names() {
        assert_eq!(name(ev(KeyCode::Char('a'), KeyModifiers::NONE)), "text:a");
        assert_eq!(name(ev(KeyCode::Char('A'), KeyModifiers::SHIFT)), "text:A");
        assert_eq!(name(ev(KeyCode::Char('c'), KeyModifiers::CONTROL)), "C-c");
        assert_eq!(name(ev(KeyCode::Char('x'), KeyModifiers::ALT)), "M-x");
        assert_eq!(name(ev(KeyCode::Enter, KeyModifiers::NONE)), "Enter");
        assert_eq!(name(ev(KeyCode::Enter, KeyModifiers::SHIFT)), "S-Enter");
        assert_eq!(name(ev(KeyCode::Esc, KeyModifiers::NONE)), "Escape");
        assert_eq!(name(ev(KeyCode::Backspace, KeyModifiers::NONE)), "BSpace");
        assert_eq!(name(ev(KeyCode::BackTab, KeyModifiers::SHIFT)), "BTab");
        assert_eq!(name(ev(KeyCode::PageUp, KeyModifiers::NONE)), "PPage");
        assert_eq!(name(ev(KeyCode::Up, KeyModifiers::CONTROL)), "C-Up");
    }

    #[test]
    fn option_keys_pick_the_seat_or_close() {
        let keys = SeatKeys::from_config(&rlord_core::config::Config::default());
        let k = |c, m| keys.seat_key(&ev(KeyCode::Char(c), m));
        assert_eq!(k('o', KeyModifiers::ALT), Some("overlord"));
        assert_eq!(k('m', KeyModifiers::ALT), Some("mastermind"));
        assert_eq!(k('d', KeyModifiers::ALT), Some("close"));
        assert_eq!(k('o', KeyModifiers::NONE), None);
        assert_eq!(keys.seat_key(&ev(KeyCode::Esc, KeyModifiers::NONE)), None);
        assert_eq!(parse_key("M-C-x"), Some((true, true, 'x')));
        assert_eq!(parse_key("M-xy"), None);
    }
}
