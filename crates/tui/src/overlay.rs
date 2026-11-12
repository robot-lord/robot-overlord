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
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
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
    let _ =
        ratatui::crossterm::execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture);
    let keys = SeatKeys::from_config(&rlord_core::config::Config::load().unwrap_or_default());
    // The user's tmux prefix there (prefix then `[` enters copy mode).
    let prefix = tmux::session_option(seat.tmux, "prefix")
        .ok()
        .and_then(|p| parse_key(&p));
    let r = run_loop(&mut terminal, &pane, seat, &keys, prefix);
    // Never leave the seat in copy mode: what's sent to it next would be
    // taken by the mode's keys.
    let _ = tmux::cancel_mode(&pane);
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture
    );
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
    prefix: Option<(bool, bool, char)>,
) -> Result<()> {
    // tmux's copy mode on the seat's pane: the overlay isn't a pane, so it
    // puts the real pane in copy mode, draws that mode's screen, and keys go
    // to the mode (the user's copy bindings: select, yank, search, q).
    let mut in_mode = false;
    let mut after_prefix = false;
    // vi copy mode's prompts (search, jump) draw on the client, under the
    // popup: the overlay reads them itself.
    let vi = tmux::session_option(seat.tmux, "mode-keys").is_ok_and(|m| m == "vi");
    let mut prompt: Option<Prompt> = None;
    // The newest paste buffer when copy mode began: a new one when it ends
    // goes to the clipboard (a yank to tmux's buffer alone would otherwise
    // reach no client: none shows the seat's window).
    let mut buffer_at_entry: Option<Option<String>> = None;
    // The pane's history size when copy mode began: the mode's snapshot,
    // which its selection lines count in (taken just before the overlay
    // enters it; on first sight if it was entered elsewhere).
    let mut history_at_entry: Option<i64> = None;
    let mut size = (0u16, 0u16);
    let mut screen = Text::default();
    let mut cursor: Option<(u16, u16)> = None;
    let mut error: Option<String> = None;
    // An error worth reading (a failed seat switch) stays up a few seconds.
    let mut hold_error_until = Instant::now();
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
                    if f.in_mode && !in_mode {
                        buffer_at_entry = Some(tmux::newest_buffer());
                        history_at_entry.get_or_insert(f.history);
                    } else if !f.in_mode {
                        history_at_entry = None;
                    }
                    if !f.in_mode && in_mode {
                        prompt = None;
                        if let Some(before) = buffer_at_entry.take() {
                            let now = tmux::newest_buffer();
                            if now.is_some() && now != before {
                                copy_to_clipboard(now.as_deref().unwrap_or_default());
                            }
                        }
                    }
                    in_mode = f.in_mode;
                    screen = f.ansi.as_bytes().into_text().unwrap_or_default();
                    let on_screen = f
                        .selection
                        .zip(history_at_entry)
                        .and_then(|(sel, h0)| tmux::screen_selection(sel, h0, f.scroll, f.height));
                    if let Some(sel) = on_screen {
                        highlight(&mut screen, sel);
                    }
                    cursor = f.cursor_visible.then_some((f.cursor_x, f.cursor_y));
                    if Instant::now() >= hold_error_until {
                        error = None;
                    }
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
                    if let Some(p) = &prompt {
                        let row = ratatui::layout::Rect::new(
                            a.x,
                            a.y + a.height.saturating_sub(1),
                            a.width,
                            1,
                        );
                        f.render_widget(
                            Paragraph::new(p.label()).style(
                                ratatui::style::Style::default()
                                    .fg(Color::Black)
                                    .bg(Color::Yellow),
                            ),
                            row,
                        );
                    }
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
                // A prompt open in copy mode takes the keys.
                if let Some(p) = prompt.as_mut() {
                    match p.key(&k) {
                        PromptStep::Typing => {}
                        PromptStep::Cancel => prompt = None,
                        PromptStep::Run(cmd, arg) => {
                            prompt = None;
                            if let Err(e) = tmux::copy_command(pane, cmd, &arg) {
                                error = Some(format!("{e:#}"));
                            }
                        }
                    }
                    last = Instant::now() - Duration::from_secs(1);
                    continue;
                }
                // Prefix, then `[`: copy mode. Prefix twice sends one prefix
                // to the agent; any other key after it is taken as itself (the
                // prefix is dropped, as tmux drops an unbound one).
                let is_prefix = prefix.is_some_and(|p| {
                    let KeyCode::Char(c) = k.code else {
                        return false;
                    };
                    p == (
                        k.modifiers.contains(KeyModifiers::ALT),
                        k.modifiers.contains(KeyModifiers::CONTROL),
                        c,
                    )
                });
                if after_prefix {
                    after_prefix = false;
                    if k.code == KeyCode::Char('[') {
                        if !in_mode {
                            history_at_entry = tmux::history_size(pane);
                            if let Err(e) = tmux::copy_mode(pane) {
                                error = Some(format!("{e:#}"));
                            }
                        }
                        last = Instant::now() - Duration::from_secs(1);
                        continue;
                    }
                    if is_prefix {
                        if !in_mode {
                            if let Some(TmuxKey::Named(n)) = tmux_key(&k) {
                                let _ = tmux::send_keys(pane, &[n.as_str()]);
                            }
                        }
                        continue;
                    }
                } else if is_prefix {
                    after_prefix = true;
                    continue;
                }
                // vi copy mode's prompt keys.
                if in_mode && vi && k.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
                    if let KeyCode::Char(c) = k.code {
                        if let Some(p) = Prompt::for_key(c) {
                            prompt = Some(p);
                            last = Instant::now() - Duration::from_secs(1);
                            continue;
                        }
                    }
                }
                match keys.seat_key(&k) {
                    // This seat's key (or Option-D) closes the overlay.
                    Some(s) if s == seat.key || s == "close" => return Ok(()),
                    // The other seat's key switches to it (the daemon opens
                    // its overlay in place of this one).
                    Some(other) => {
                        // Hand this seat's window its size back first, and out
                        // of copy mode: the new overlay replaces (kills) this
                        // one.
                        let _ = tmux::auto_size_window(seat.tmux);
                        let _ = tmux::cancel_mode(pane);
                        match client_call(
                            &paths::socket_path(),
                            "ui.overlord",
                            json!({"seat": other, "show": true}),
                        ) {
                            Ok(_) => return Ok(()),
                            // Stay, and say why the switch didn't happen.
                            Err(e) => {
                                error = Some(format!("couldn't switch to the {other}: {e:#}"));
                                hold_error_until = Instant::now() + Duration::from_secs(4);
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
            // The wheel scrolls the pane's history (entering copy mode, as
            // tmux's own wheel binding does).
            // (Motion and clicks are ignored: no redraw for them.)
            Event::Mouse(m) => {
                let r = match m.kind {
                    MouseEventKind::ScrollUp => {
                        if !in_mode {
                            history_at_entry = tmux::history_size(pane);
                        }
                        tmux::scroll(pane, true)
                    }
                    MouseEventKind::ScrollDown if in_mode => tmux::scroll(pane, false),
                    _ => continue,
                };
                if let Err(e) = r {
                    error = Some(format!("{e:#}"));
                }
                last = Instant::now() - Duration::from_secs(1);
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

/// A vi copy-mode prompt the overlay reads itself (tmux would draw it on the
/// client, under the popup).
enum Prompt {
    Search { forward: bool, text: String },
    Jump(&'static str),
}

enum PromptStep {
    Typing,
    Cancel,
    Run(&'static str, String),
}

impl Prompt {
    /// vi copy mode's prompt keys: / ? (search), f F t T (jump).
    fn for_key(c: char) -> Option<Prompt> {
        Some(match c {
            '/' => Prompt::Search {
                forward: true,
                text: String::new(),
            },
            '?' => Prompt::Search {
                forward: false,
                text: String::new(),
            },
            'f' => Prompt::Jump("jump-forward"),
            'F' => Prompt::Jump("jump-backward"),
            't' => Prompt::Jump("jump-to-forward"),
            'T' => Prompt::Jump("jump-to-backward"),
            _ => return None,
        })
    }

    fn label(&self) -> String {
        match self {
            Prompt::Search { forward, text } => {
                format!(
                    "{}{text}▏",
                    if *forward {
                        "(search down) "
                    } else {
                        "(search up) "
                    }
                )
            }
            Prompt::Jump(cmd) => format!("({}) ", cmd.replace('-', " ")),
        }
    }

    fn key(&mut self, k: &KeyEvent) -> PromptStep {
        // Chords aren't text: C-c / C-g cancel (as tmux's prompt), C-u
        // clears a search, the rest are ignored.
        if k.modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return match (self, k.code) {
                (_, KeyCode::Char('c' | 'g')) => PromptStep::Cancel,
                (Prompt::Search { text, .. }, KeyCode::Char('u')) => {
                    text.clear();
                    PromptStep::Typing
                }
                _ => PromptStep::Typing,
            };
        }
        match (self, k.code) {
            (_, KeyCode::Esc) => PromptStep::Cancel,
            (Prompt::Jump(cmd), KeyCode::Char(c)) => PromptStep::Run(cmd, c.to_string()),
            (Prompt::Jump(_), _) => PromptStep::Cancel,
            (Prompt::Search { forward, text }, KeyCode::Enter) => PromptStep::Run(
                if *forward {
                    "search-forward"
                } else {
                    "search-backward"
                },
                std::mem::take(text),
            ),
            (Prompt::Search { text, .. }, KeyCode::Backspace) => {
                if text.pop().is_none() {
                    return PromptStep::Cancel;
                }
                PromptStep::Typing
            }
            (Prompt::Search { text, .. }, KeyCode::Char(c)) => {
                text.push(c);
                PromptStep::Typing
            }
            _ => PromptStep::Typing,
        }
    }
}

/// Put a paste buffer on the Mac clipboard (best effort).
fn copy_to_clipboard(buffer: &str) {
    let Ok(text) = tmux::buffer_text(buffer) else {
        return;
    };
    use std::io::Write;
    if let Ok(mut c) = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        if let Some(mut i) = c.stdin.take() {
            let _ = i.write_all(text.as_bytes());
        }
        let _ = c.wait();
    }
}

/// Draw copy mode's selection (tmux leaves it out of a capture): reversed
/// cells from `a` to `b` (column, row; inclusive, in reading order).
fn highlight(text: &mut Text<'static>, (a, b): ((u16, u16), (u16, u16))) {
    use ratatui::style::Modifier;
    use ratatui::text::{Line, Span};
    use unicode_width::UnicodeWidthChar;
    for (row, line) in text.lines.iter_mut().enumerate() {
        let row = row as u16;
        if row < a.1 || row > b.1 {
            continue;
        }
        let from = if row == a.1 { a.0 } else { 0 };
        let to = if row == b.1 { b.0 } else { u16::MAX };
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col: u16 = 0;
        for span in line.spans.drain(..) {
            // Split each span at the selection's edges, by display column.
            let mut buf = String::new();
            let mut inside: Option<bool> = None;
            for ch in span.content.chars() {
                let w = ch.width().unwrap_or(0) as u16;
                let sel = col >= from && col <= to;
                if inside.is_some_and(|i| i != sel) {
                    let st = if inside == Some(true) {
                        span.style.add_modifier(Modifier::REVERSED)
                    } else {
                        span.style
                    };
                    spans.push(Span::styled(std::mem::take(&mut buf), st));
                }
                inside = Some(sel);
                buf.push(ch);
                col = col.saturating_add(w);
            }
            if !buf.is_empty() {
                let st = if inside == Some(true) {
                    span.style.add_modifier(Modifier::REVERSED)
                } else {
                    span.style
                };
                spans.push(Span::styled(buf, st));
            }
        }
        // A selected empty line still shows one cell.
        if spans.is_empty() && from == 0 {
            spans.push(Span::styled(
                " ",
                ratatui::style::Style::default().add_modifier(Modifier::REVERSED),
            ));
        }
        *line = Line::from(spans).style(line.style);
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
    // `Space` (a common prefix: C-Space) is a space.
    if rest == "Space" {
        return Some((alt, ctrl, ' '));
    }
    let mut chars = rest.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some((alt, ctrl, c))
}

impl SeatKeys {
    fn from_config(cfg: &rlord_core::config::Config) -> Self {
        // An empty or unreadable key falls back to the default.
        let or = |k: &str, d: &str| parse_key(k).or_else(|| parse_key(d));
        SeatKeys {
            overlord: or(&cfg.tmux.overlord, "M-o"),
            mastermind: or(cfg.tmux.mastermind_switch(), "M-m"),
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
