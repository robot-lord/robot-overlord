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
use rlord_daemon::overlord::TMUX_NAME;

/// Frame interval: how often the overlord's pane is re-captured.
const FRAME_MS: u64 = 40;

/// `token` identifies this overlay to the daemon (it was minted when the
/// daemon opened the popup).
pub fn run(token: Option<u64>) -> Result<()> {
    let pane = tmux::pane_id(TMUX_NAME)?;
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
    let r = run_loop(&mut terminal, &pane);
    let _ = ratatui::crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    report(false);
    // The window follows clients again once nobody is embedding it.
    let _ = tmux::auto_size_window(TMUX_NAME);
    r
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal, pane: &str) -> Result<()> {
    let mut size = (0u16, 0u16);
    let mut screen = Text::default();
    let mut cursor: Option<(u16, u16)> = None;
    let mut error: Option<String> = None;
    let mut last = Instant::now() - Duration::from_secs(1);
    loop {
        let area = terminal.size()?;
        if (area.width, area.height) != size && area.width > 0 && area.height > 0 {
            size = (area.width, area.height);
            if let Err(e) = tmux::resize_window(TMUX_NAME, size.0, size.1) {
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
                if is_close(&k) {
                    return Ok(());
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
fn is_close(k: &KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::ALT) && matches!(k.code, KeyCode::Char('o' | 'd'))
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
    fn option_o_closes() {
        assert!(is_close(&ev(KeyCode::Char('o'), KeyModifiers::ALT)));
        assert!(is_close(&ev(KeyCode::Char('d'), KeyModifiers::ALT)));
        assert!(!is_close(&ev(KeyCode::Char('d'), KeyModifiers::NONE)));
        assert!(!is_close(&ev(KeyCode::Char('o'), KeyModifiers::NONE)));
        assert!(!is_close(&ev(KeyCode::Esc, KeyModifiers::NONE)));
    }
}
