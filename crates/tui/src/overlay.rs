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
    let cfg = rlord_core::config::Config::load().unwrap_or_default();
    let keys = SeatKeys::from_config(&cfg);
    // The overlay draws in the user's theme: the copy bar wears the seat's
    // colour.
    crate::theme::set(rlord_core::themes::resolve(
        &cfg.theme,
        rlord_core::themes::system_is_dark(),
    ));
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
    // The user's own keys into copy mode: the session's key table (e.g.
    // M-a) and after the prefix (e.g. a, PPage; `[` always).
    let table = tmux::session_option(seat.tmux, "key-table")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "root".into());
    let direct_copy = tmux::copy_mode_bindings(&table);
    let mut prefixed_copy = tmux::copy_mode_bindings("prefix");
    // `[` only when nothing in the user's config enters copy mode.
    if prefixed_copy.is_empty() && direct_copy.is_empty() {
        prefixed_copy.push(("[".into(), false));
    }
    // Copy mode's own table: its prompts (tmux draws them on the client,
    // under the popup, so the overlay reads them) and the hints in its bar.
    let mode_keys = ModeKeys::read(if vi { "copy-mode-vi" } else { "copy-mode" });
    // A count typed through the repeat prompt, for the next key.
    let mut repeat: Option<u32> = None;
    // The newest paste buffer when copy mode began: a new one when it ends
    // goes to the clipboard (a yank to tmux's buffer alone would otherwise
    // reach no client: none shows the seat's window).
    let mut buffer_at_entry: Option<Option<String>> = None;
    // The pane's history size when copy mode began: the mode's snapshot,
    // which its selection lines count in (taken just before the overlay
    // enters it; on first sight if it was entered elsewhere).
    let mut history_at_entry: Option<i64> = None;
    // Where copy mode is in the history (tmux draws its own `[n/m]`
    // marker on the client, not into a capture): shown in the mode's bar.
    let mut position = (0i64, 0i64);
    // `touch <runtime dir>/overlay-debug` logs the events the overlay gets
    // (to find why a key does nothing on some terminal setup).
    let debug_log = {
        let dir = paths::runtime_dir();
        dir.join("overlay-debug")
            .exists()
            .then(|| dir.join("overlay-debug.log"))
    };
    let log = |what: &str| {
        if let Some(p) = &debug_log {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
            {
                let _ = writeln!(f, "{what}");
            }
        }
    };
    log(&format!(
        "start: prefix={prefix:?} vi={vi} copy keys: {direct_copy:?} prefix+{prefixed_copy:?}"
    ));
    // Enter copy mode (scrolled up a page for `copy-mode -u`).
    let enter = |up: bool, history_at_entry: &mut Option<i64>, error: &mut Option<String>| {
        *history_at_entry = tmux::history_size(pane);
        let r = tmux::copy_mode(pane).and_then(|_| {
            if up {
                tmux::send_keys(pane, &["-X", "page-up"])
            } else {
                Ok(())
            }
        });
        if let Err(e) = r {
            *error = Some(format!("{e:#}"));
        }
    };
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
            // Resizing reflows copy mode's snapshot, so its line numbers no
            // longer match what was recorded on entry: leave the mode
            // rather than draw a wrong selection.
            // (Or just entered by the overlay: `in_mode` lags a frame,
            // `history_at_entry` doesn't.)
            if in_mode || history_at_entry.is_some() {
                let _ = tmux::cancel_mode(pane);
                history_at_entry = None;
            }
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
                        repeat = None;
                        if let Some(before) = buffer_at_entry.take() {
                            let now = tmux::newest_buffer();
                            if now.is_some() && now != before {
                                copy_to_clipboard(now.as_deref().unwrap_or_default());
                            }
                        }
                    }
                    if f.in_mode != in_mode {
                        log(&format!("pane in_mode={}", f.in_mode));
                    }
                    in_mode = f.in_mode;
                    position = (f.scroll, history_at_entry.unwrap_or(f.history));
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
                    // Copy mode looks like the live screen at the bottom of
                    // the history: say so, with its keys.
                    let bar = match &prompt {
                        Some(p) => Some(bar_line(seat, "COPY", p.label(), String::new())),
                        None if in_mode => Some(bar_line(
                            seat,
                            "COPY",
                            format!(
                                "{}/{}{}",
                                position.0,
                                position.1,
                                repeat.map(|n| format!(" ×{n}")).unwrap_or_default(),
                            ),
                            mode_keys.hints.clone(),
                        )),
                        None => None,
                    };
                    if let Some(line) = bar {
                        let row = ratatui::layout::Rect::new(
                            a.x,
                            a.y + a.height.saturating_sub(1),
                            a.width,
                            1,
                        );
                        f.render_widget(
                            Paragraph::new(line).style(
                                ratatui::style::Style::default()
                                    .fg(crate::theme::text())
                                    .bg(crate::theme::chip()),
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
        let ev = event::read()?;
        if !matches!(ev, Event::Mouse(_)) {
            log(&format!(
                "event {ev:?} after_prefix={after_prefix} in_mode={in_mode}"
            ));
        }
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                // A prompt open in copy mode takes the keys.
                if let Some(p) = prompt.as_mut() {
                    let step = p.key(&k);
                    last = Instant::now() - Duration::from_secs(1);
                    match step {
                        PromptStep::Typing => continue,
                        // Cancelled: a count it was meant to take goes too.
                        PromptStep::Cancel => {
                            prompt = None;
                            repeat = None;
                            continue;
                        }
                        PromptStep::Run(Action::Copy(cmd), arg) => {
                            prompt = None;
                            // A pending count applies to it (3fx: the 3rd x).
                            if let Err(e) = tmux::copy_command(pane, &cmd, &arg, repeat.take()) {
                                error = Some(format!("{e:#}"));
                            }
                            continue;
                        }
                        PromptStep::Run(Action::Repeat, arg) => {
                            prompt = None;
                            repeat = arg.trim().parse().ok().filter(|n| *n > 0);
                            continue;
                        }
                        // A count ended by a non-digit: the count, then that
                        // key as usual (5j).
                        PromptStep::CountThen(arg) => {
                            prompt = None;
                            repeat = arg.trim().parse().ok().filter(|n| *n > 0);
                        }
                    }
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
                    if let Some((_, up)) =
                        prefixed_copy.iter().find(|(spec, _)| key_matches(spec, &k))
                    {
                        if !in_mode {
                            enter(*up, &mut history_at_entry, &mut error);
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
                } else if !in_mode {
                    // The user's direct key into copy mode (e.g. M-a).
                    if let Some((_, up)) =
                        direct_copy.iter().find(|(spec, _)| key_matches(spec, &k))
                    {
                        enter(*up, &mut history_at_entry, &mut error);
                        last = Instant::now() - Duration::from_secs(1);
                        continue;
                    }
                }
                // Copy mode's prompt keys (the user's own: search, jump, goto
                // line, a count).
                if in_mode {
                    if let Some(p) = mode_keys.prompt_for(&k, pane) {
                        prompt = Some(p);
                        last = Instant::now() - Duration::from_secs(1);
                        continue;
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
                let r = match (tmux_key(&k), repeat.take().filter(|_| in_mode)) {
                    (Some(TmuxKey::Text(t)), Some(n)) => tmux::send_key_repeat(pane, n, &t, true),
                    (Some(TmuxKey::Named(key)), Some(n)) => {
                        tmux::send_key_repeat(pane, n, &key, false)
                    }
                    (Some(TmuxKey::Text(t)), None) => tmux::send_text(pane, &t),
                    (Some(TmuxKey::Named(key)), None) => tmux::send_keys(pane, &[key.as_str()]),
                    (None, _) => Ok(()),
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

/// What a copy-mode prompt runs when it's answered.
#[derive(Clone)]
enum Action {
    /// `send-keys -X <cmd> -- <answer>` (search, jump, goto line).
    Copy(String),
    /// A count for the next key (`send-keys -N <answer>`).
    Repeat,
}

/// A copy-mode prompt the overlay reads itself: tmux would draw it on the
/// client, under the popup. Built from the user's own binding.
struct Prompt {
    label: String,
    text: String,
    /// One key answers it (`command-prompt -1`: jumps).
    single: bool,
    /// Digits only (`command-prompt -N`: a count); another key ends it.
    numeric: bool,
    action: Action,
}

enum PromptStep {
    Typing,
    Cancel,
    Run(Action, String),
    /// A count prompt ended by another key: keep the count, then handle the
    /// key itself.
    CountThen(String),
}

impl Prompt {
    fn label(&self) -> String {
        format!("{} {}▏", self.label, self.text)
    }

    fn key(&mut self, k: &KeyEvent) -> PromptStep {
        // A count ends at any non-digit, a chord too (5 C-e scrolls 5),
        // except the cancel chords.
        let chord = k
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if self.numeric && chord && !matches!(k.code, KeyCode::Char('c' | 'g')) {
            return PromptStep::CountThen(std::mem::take(&mut self.text));
        }
        // Chords aren't text: C-c / C-g cancel (as tmux's prompt), C-u
        // clears, the rest are ignored.
        if k.modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return match k.code {
                KeyCode::Char('c' | 'g') => PromptStep::Cancel,
                KeyCode::Char('u') => {
                    self.text.clear();
                    PromptStep::Typing
                }
                _ => PromptStep::Typing,
            };
        }
        match k.code {
            KeyCode::Esc => PromptStep::Cancel,
            KeyCode::Char(c) if self.numeric && c.is_ascii_digit() => {
                self.text.push(c);
                PromptStep::Typing
            }
            KeyCode::Enter if self.numeric => {
                PromptStep::Run(self.action.clone(), std::mem::take(&mut self.text))
            }
            KeyCode::Backspace if self.numeric => {
                if self.text.pop().is_none() {
                    return PromptStep::Cancel;
                }
                PromptStep::Typing
            }
            _ if self.numeric => PromptStep::CountThen(std::mem::take(&mut self.text)),
            KeyCode::Char(c) if self.single => PromptStep::Run(self.action.clone(), c.to_string()),
            KeyCode::Char(c) => {
                self.text.push(c);
                PromptStep::Typing
            }
            KeyCode::Enter => PromptStep::Run(self.action.clone(), std::mem::take(&mut self.text)),
            KeyCode::Backspace => {
                if self.text.pop().is_none() {
                    return PromptStep::Cancel;
                }
                PromptStep::Typing
            }
            _ if self.single => PromptStep::Cancel,
            _ => PromptStep::Typing,
        }
    }
}

/// One `command-prompt` binding of the copy-mode table.
struct PromptBinding {
    key: String,
    label: String,
    single: bool,
    /// `-I` initial text (may be a format: `#{pane_search_string}`).
    initial: String,
    numeric: bool,
    action: Action,
}

/// The user's copy-mode table as the overlay needs it: its prompt keys and
/// a hint line naming their keys for select, copy, search and leave.
struct ModeKeys {
    prompts: Vec<PromptBinding>,
    hints: String,
}

/// The argument after `flag` in a tmux command (`-p "(search up)"`).
fn arg_after(cmd: &str, flag: &str) -> Option<String> {
    let rest = cmd.split_once(&format!(" {flag} "))?.1.trim_start();
    Some(match rest.strip_prefix('"') {
        Some(q) => q.split('"').next()?.to_string(),
        None => rest.split_whitespace().next()?.to_string(),
    })
}

impl ModeKeys {
    fn read(table: &str) -> ModeKeys {
        let all = tmux::bindings(table);
        ModeKeys::from_bindings(&all)
    }

    fn from_bindings(all: &[(String, String)]) -> ModeKeys {
        let mut prompts = Vec::new();
        for (key, cmd) in all {
            if !cmd.starts_with("command-prompt") {
                continue;
            }
            // The flag cluster(s) before the body: -1 is single-key.
            let head = cmd.split(" { ").next().unwrap_or(cmd);
            let numeric = head
                .split_whitespace()
                .any(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('N'));
            let single = head.split_whitespace().any(|w| {
                w.starts_with('-') && !w.starts_with("--") && w.contains('1') && !w.contains('I')
            });
            let action = if let Some(x) = cmd.split_once("send-keys -X ") {
                let c = x.1.split_whitespace().next().unwrap_or("");
                // Incremental search runs as a plain search on Enter.
                Action::Copy(c.trim_end_matches("-incremental").to_string())
            } else if cmd.contains("send-keys -N") {
                Action::Repeat
            } else {
                continue;
            };
            prompts.push(PromptBinding {
                key: key.clone(),
                label: arg_after(head, "-p").unwrap_or_else(|| "(prompt)".into()),
                single,
                initial: arg_after(head, "-I").unwrap_or_default(),
                numeric,
                action,
            });
        }
        // Hints: the first plain key bound to each.
        let plain =
            |k: &str| !k.starts_with("Mouse") && !k.contains("Click") && !k.starts_with("Wheel");
        let key_for = |want: &dyn Fn(&str) -> bool| {
            all.iter()
                .filter(|(k, c)| plain(k) && want(c))
                .map(|(k, _)| k.as_str())
                // Prefer a single character, then the rest.
                .min_by_key(|k| (k.chars().count() > 1, *k == "Enter"))
                .map(str::to_string)
        };
        let mut hints = Vec::new();
        for (what, f) in [
            (
                "select",
                &(|c: &str| c == "send-keys -X begin-selection") as &dyn Fn(&str) -> bool,
            ),
            // (Not the end-of-line copies: D in vi.)
            ("copy", &|c: &str| {
                (c.contains("-X copy-selection") || c.contains("-X copy-pipe"))
                    && !c.contains("end-of-line")
            }),
            ("search", &|c: &str| c.contains("-X search-")),
            ("leave", &|c: &str| c == "send-keys -X cancel"),
        ] {
            if let Some(k) = key_for(f) {
                // Named keys read better in lower case; a letter keeps its
                // case (D isn't d).
                let k = if k.chars().count() > 1 {
                    k.to_lowercase()
                } else {
                    k
                };
                hints.push(format!("{k} {what}"));
            }
        }
        ModeKeys {
            prompts,
            hints: hints.join(" · "),
        }
    }

    /// The prompt a key opens, if it's one of the table's prompt keys.
    fn prompt_for(&self, k: &KeyEvent, pane: &str) -> Option<Prompt> {
        let b = self.prompts.iter().find(|b| key_matches(&b.key, k))?;
        let text = if b.initial.contains("#{") {
            tmux::format(pane, &b.initial)
        } else {
            b.initial.clone()
        };
        Some(Prompt {
            label: b.label.clone(),
            text,
            single: b.single,
            numeric: b.numeric,
            action: b.action.clone(),
        })
    }
}

/// Put a paste buffer on the clipboard (best effort): through tmux's own
/// `copy-command` if the user set one, else the Mac's `pbcopy`.
fn copy_to_clipboard(buffer: &str) {
    let Ok(text) = tmux::buffer_text(buffer) else {
        return;
    };
    use std::io::Write;
    let custom = tmux::server_option("copy-command");
    let mut cmd = if custom.trim().is_empty() {
        if !cfg!(target_os = "macos") {
            return;
        }
        std::process::Command::new("pbcopy")
    } else {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", custom.trim()]);
        c
    };
    if let Ok(mut c) = cmd.stdin(std::process::Stdio::piped()).spawn() {
        if let Some(mut i) = c.stdin.take() {
            let _ = i.write_all(text.as_bytes());
        }
        let _ = c.wait();
    }
}

/// The copy bar: a chip in the seat's colour naming the mode, then what
/// it's at, then the user's keys, quieter.
fn bar_line(seat: &Seat, mode: &str, what: String, hints: String) -> ratatui::text::Line<'static> {
    use crate::theme;
    use ratatui::style::Style;
    use ratatui::text::Span;
    let hue = if seat.key == "mastermind" {
        theme::mastermind()
    } else {
        theme::overlord()
    };
    let mut spans = vec![
        Span::styled(
            format!(" {mode} "),
            Style::default().bold().fg(theme::on_accent()).bg(hue),
        ),
        Span::styled(format!(" {what}"), Style::default().fg(hue)),
    ];
    if !hints.trim().is_empty() {
        spans.push(Span::styled(
            format!("   {}", hints.trim()),
            Style::default().fg(theme::muted_c()),
        ));
    }
    ratatui::text::Line::from(spans)
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
    overlord: Option<rlord_core::keys::Key>,
    mastermind: Option<rlord_core::keys::Key>,
    close: Option<rlord_core::keys::Key>,
}

/// Whether a key event is the tmux key `spec` (`M-a`, `a`, `[`, `C-Space`,
/// `PPage`, …).
fn key_matches(spec: &str, k: &KeyEvent) -> bool {
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let named = |rest: &str| match rest {
        "PPage" | "PageUp" => Some(KeyCode::PageUp),
        "NPage" | "PageDown" => Some(KeyCode::PageDown),
        "Enter" => Some(KeyCode::Enter),
        "Escape" => Some(KeyCode::Esc),
        "Home" => Some(KeyCode::Home),
        "End" => Some(KeyCode::End),
        _ => None,
    };
    // Modifiers, then a character or a named key.
    let (mut a, mut c, mut rest) = (false, false, spec);
    loop {
        if let Some(r) = rest.strip_prefix("M-") {
            a = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("C-") {
            c = true;
            rest = r;
        } else {
            break;
        }
    }
    if (a, c) != (alt, ctrl) {
        return false;
    }
    if let Some(code) = named(rest) {
        return k.code == code;
    }
    parse_key(rest).is_some_and(|(_, _, ch)| k.code == KeyCode::Char(ch))
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
        // The same notation the key screen saves (F-keys and named keys too).
        use rlord_core::keys::Key;
        let or = |k: &str, d: &str| Key::parse(k).or_else(|| Key::parse(d));
        SeatKeys {
            overlord: or(&cfg.tmux.overlord, "M-o"),
            mastermind: or(cfg.tmux.mastermind_switch(), "M-m"),
            close: or(&cfg.tmux.back, "M-d"),
        }
    }

    fn seat_key(&self, k: &KeyEvent) -> Option<&'static str> {
        let got = crate::keyscreen::key_of(k);
        got.as_ref()?;
        if self.overlord == got {
            Some(overlord::FRONT.key)
        } else if self.mastermind == got {
            Some(overlord::MASTERMIND.key)
        } else if self.close == got {
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

    fn b(k: &str, c: &str) -> (String, String) {
        (k.to_string(), c.to_string())
    }

    #[test]
    fn copy_mode_table_gives_prompts_and_hints() {
        // vi, as a user might have it.
        let vi = ModeKeys::from_bindings(&[
            b("Enter", "send-keys -X copy-pipe-and-cancel"),
            b("Space", "send-keys -X begin-selection"),
            b(
                "/",
                r#"command-prompt -P -T search -p "(search down)" { send-keys -X search-forward -- "%%" }"#,
            ),
            b(
                "5",
                r#"command-prompt -NP -I 5 -p (repeat) { send-keys -N "%%" }"#,
            ),
            b(
                "f",
                r#"command-prompt -1P -p "(jump forward)" { send-keys -X jump-forward -- "%%" }"#,
            ),
            b("q", "send-keys -X cancel"),
            b("v", "send-keys -X begin-selection"),
            b("y", "send-keys -X copy-pipe-and-cancel pbcopy"),
            b(
                "MouseDragEnd1Pane",
                "send-keys -X copy-pipe-and-cancel pbcopy",
            ),
        ]);
        assert_eq!(vi.hints, "v select · y copy · / search · q leave");
        let ev = |c| {
            let mut k = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            k.kind = KeyEventKind::Press;
            k
        };
        let p = vi.prompt_for(&ev('f'), "%0").unwrap();
        assert!(p.single && p.label == "(jump forward)");
        assert!(matches!(p.action, Action::Copy(ref c) if c == "jump-forward"));
        let p = vi.prompt_for(&ev('5'), "%0").unwrap();
        assert!(!p.single && p.numeric && p.text == "5" && matches!(p.action, Action::Repeat));
        // 5j: j ends the count.
        let mut p = vi.prompt_for(&ev('5'), "%0").unwrap();
        assert!(matches!(p.key(&ev('2')), PromptStep::Typing));
        assert!(matches!(p.key(&ev('j')), PromptStep::CountThen(ref n) if n == "52"));
        assert!(vi.prompt_for(&ev('j'), "%0").is_none());

        // emacs defaults: incremental search runs as a plain search.
        let emacs = ModeKeys::from_bindings(&[
            b("C-Space", "send-keys -X begin-selection"),
            b("M-w", "send-keys -X copy-pipe-and-cancel"),
            b(
                "C-s",
                r##"command-prompt -Pi -I "#{pane_search_string}" -T search -p "(search down)" { send-keys -X search-forward-incremental -- "%%" }"##,
            ),
            b("q", "send-keys -X cancel"),
        ]);
        assert_eq!(
            emacs.hints,
            "c-space select · m-w copy · c-s search · q leave"
        );
        let p = &emacs.prompts[0];
        assert!(matches!(p.action, Action::Copy(ref c) if c == "search-forward"));
        assert_eq!(p.initial, "#{pane_search_string}");
    }

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
