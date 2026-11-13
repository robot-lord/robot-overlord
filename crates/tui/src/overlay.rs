//! `rlord overlay`: the overlord as an overlay. It runs inside a tmux popup
//! (`display-popup`) over whatever the user is looking at — an agent session
//! or the rlord list — and draws the overlord's live pane, forwarding every
//! keystroke to it. The overlord's tmux window is sized to the popup, so the
//! agent lays itself out for exactly the space it's shown in.
//!
//! Option-O (or Option-D) closes it: inside a popup, keys reach this program
//! rather than tmux's key table, so the key that opened it closes it.
//!
//! With more than one agent installed, the top row is a switcher: each
//! agent's marker, the active overlord's highlighted. A click on one, or
//! the switch key (`[tmux] overlord_switch`, Option-S), makes that agent's
//! overlord the active one and shows its pane instead, without closing
//! the overlay (`View`, `SwitchBar`).

use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::style::{Color, Stylize};
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use serde_json::json;

use rlord_core::paths;
use rlord_core::tmux;
use rlord_daemon::client_call;
use rlord_daemon::overlord;

/// Frame interval: how often the overlord's pane is re-captured.
const FRAME_MS: u64 = 40;

/// `token` identifies this overlay to the daemon (it was minted when the
/// daemon opened the popup).
pub fn run(token: Option<u64>) -> Result<()> {
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
    let keys = OverlayKeys::from_config(&cfg);
    // The overlay draws in the user's theme: the copy bar wears the
    // overlord's colour.
    crate::theme::set(rlord_core::themes::resolve(
        &cfg.theme,
        rlord_core::themes::system_is_dark(),
    ));
    let r = show(&mut terminal, &keys);
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture
    );
    ratatui::restore();
    report(false);
    r
}

/// Show the active overlord until the overlay is closed; when the user
/// switches, the newly active one, in the same overlay.
fn show(terminal: &mut ratatui::DefaultTerminal, keys: &OverlayKeys) -> Result<()> {
    // Why the last switch didn't happen (shown in the switcher).
    let mut notice: Option<String> = None;
    loop {
        let view = View::load(keys, notice.take());
        let pane = tmux::pane_id(&view.tmux)?;
        // The user's tmux prefix there (prefix then `[` enters copy mode).
        let prefix = tmux::session_option(&view.tmux, "prefix")
            .ok()
            .and_then(|p| parse_key(&p));
        let exit = run_loop(terminal, &view, &pane, keys, prefix);
        // Never leave the overlord in copy mode: what's sent to it next
        // would be taken by the mode's keys.
        let _ = tmux::cancel_mode(&pane);
        // The window follows clients again once nobody is embedding it.
        let _ = tmux::auto_size_window(&view.tmux);
        match exit? {
            Exit::Close => return Ok(()),
            Exit::Switch(agent) => {
                // (Starting one for the first time takes a moment.)
                terminal.draw(|f| {
                    f.render_widget(
                        Paragraph::new(format!(" switching the overlord to {agent}…"))
                            .fg(crate::theme::overlord()),
                        f.area(),
                    )
                })?;
                if let Err(e) = client_call(
                    &paths::socket_path(),
                    "overlord.switch",
                    json!({"agent": agent}),
                ) {
                    notice = Some(format!("{e:#}"));
                }
            }
        }
    }
}

/// How `run_loop` ended.
enum Exit {
    Close,
    /// Make this agent's overlord the active one, and show it.
    Switch(String),
}

/// What the overlay shows: the active overlord's tmux session, and the
/// switcher when there is more than one agent to run an overlord on.
struct View {
    tmux: String,
    bar: Option<SwitchBar>,
}

/// The switcher row: every installed agent, the active overlord's first
/// among equals.
#[derive(Clone)]
struct SwitchBar {
    /// (agent, its marker, active, has an overlord).
    agents: Vec<(String, String, bool, bool)>,
    /// About the active one: "fresh 2h ago · 14 earlier", a usage limit.
    info: String,
    /// It is stopped on a usage limit (the info says until when).
    limited: bool,
    /// The switch key, as it reads ("⌥s").
    key: String,
    /// Why the last switch failed.
    notice: Option<String>,
    /// A passing word in place of the switch hint (`ONLY_ONE`).
    hint: Option<String>,
}

impl View {
    fn load(keys: &OverlayKeys, notice: Option<String>) -> View {
        let o = client_call(&paths::socket_path(), "overlord.get", json!({})).unwrap_or_default();
        let tmux = o["tmux_session"]
            .as_str()
            .unwrap_or(overlord::TMUX_NAME)
            .to_string();
        let agents: Vec<(String, String, bool, bool)> = o["overlords"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["installed"] == true || a["active"] == true)
            .filter_map(|a| {
                let name = a["agent"].as_str()?;
                let marker = rlord_core::agents::install::tool(name)
                    .map(|t| t.marker.iter().map(|(text, _)| *text).collect::<String>())
                    .unwrap_or_default();
                Some((
                    name.to_string(),
                    marker,
                    a["active"] == true,
                    a["exists"] == true,
                ))
            })
            .collect();
        let limit = o["limit"]["until"].as_str();
        let info = match (
            limit,
            rlord_daemon::recall::fresh_label(&o["conversations"]),
        ) {
            (Some(_), _) if o["limit"]["reset"] == true => "usage limit reset just now".into(),
            (Some(until), _) => format!("out of usage until {until}: switch to another"),
            (None, Some(fresh)) => fresh,
            (None, None) => String::new(),
        };
        let bar = (agents.len() > 1).then(|| SwitchBar {
            agents,
            info,
            limited: limit.is_some(),
            key: keys.switch.as_ref().map(|k| k.label()).unwrap_or_default(),
            notice,
            hint: None,
        });
        View { tmux, bar }
    }
}

impl SwitchBar {
    /// The row, and where each agent is on it: (first column, one past the
    /// last, agent).
    fn line(&self) -> (ratatui::text::Line<'static>, Vec<(u16, u16, String)>) {
        use crate::theme;
        use ratatui::style::Style;
        use ratatui::text::Span;
        use unicode_width::UnicodeWidthStr;
        let hue = theme::overlord();
        let mut spans = Vec::new();
        let mut hits = Vec::new();
        let mut x = 0u16;
        for (i, (agent, marker, active, exists)) in self.agents.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("│", Style::default().fg(theme::faint_c())));
                x += 1;
            }
            let text = format!(" {marker} {agent} ");
            let w = text.width() as u16;
            let style = if *active {
                Style::default().bold().fg(theme::on_accent()).bg(hue)
            } else if *exists {
                Style::default().fg(theme::text())
            } else {
                // Not started yet: the first switch starts it.
                Style::default().fg(theme::muted_c())
            };
            spans.push(Span::styled(text, style));
            hits.push((x, x + w, agent.clone()));
            x += w;
        }
        let mut tail = Vec::new();
        if let Some(n) = &self.notice {
            spans.push(Span::styled(
                format!("  {n}"),
                Style::default().fg(theme::red()),
            ));
        } else {
            if !self.info.is_empty() {
                tail.push(self.info.clone());
            }
            if let Some(h) = &self.hint {
                tail.push(h.clone());
            } else if !self.key.is_empty() && self.next().is_some() {
                tail.push(format!("{} or click to switch", self.key));
            } else {
                tail.push("click to switch".into());
            }
            spans.push(Span::styled(
                format!("  {}", tail.join(" · ")),
                Style::default().fg(if self.limited {
                    theme::yellow()
                } else {
                    theme::muted_c()
                }),
            ));
        }
        (ratatui::text::Line::from(spans), hits)
    }

    /// The overlord after the active one (round again at the end): only
    /// agents that have one. The key never creates an overlord; that is
    /// a click on the row, `:overlord <agent>` or the CLI.
    fn next(&self) -> Option<String> {
        let at = self.agents.iter().position(|a| a.2)?;
        let n = self.agents.len();
        (1..n)
            .map(|i| &self.agents[(at + i) % n])
            .find(|a| a.3)
            .map(|a| a.0.clone())
    }
}

/// Shown for `HINT_FOR` when the switch key is pressed with only one
/// overlord.
const ONLY_ONE: &str = "only one overlord: open :overlord <agent> to add another";
const HINT_FOR: Duration = Duration::from_secs(4);

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    view: &View,
    pane: &str,
    keys: &OverlayKeys,
    prefix: Option<(bool, bool, char)>,
) -> Result<Exit> {
    let target = view.tmux.as_str();
    // The switcher takes the top row; the overlord's pane gets the rest.
    let mut switcher = view.bar.as_ref().map(|b| b.line());
    // tmux's copy mode on the overlord's pane: the overlay isn't a pane, so it
    // puts the real pane in copy mode, draws that mode's screen, and keys go
    // to the mode (the user's copy bindings: select, yank, search, q).
    let mut in_mode = false;
    let mut after_prefix = false;
    // vi copy mode's prompts (search, jump) draw on the client, under the
    // popup: the overlay reads them itself.
    let vi = tmux::session_option(target, "mode-keys").is_ok_and(|m| m == "vi");
    let mut prompt: Option<Prompt> = None;
    // The user's own keys into copy mode: the session's key table (e.g.
    // M-a) and after the prefix (e.g. a, PPage; `[` always).
    let table = tmux::session_option(target, "key-table")
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
    // reach no client: none shows the overlord's window).
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
    let mut last = Instant::now() - Duration::from_secs(1);
    // When a passing hint on the switcher goes.
    let mut hint_until: Option<Instant> = None;
    loop {
        if hint_until.is_some_and(|t| Instant::now() >= t) {
            hint_until = None;
            switcher = view.bar.as_ref().map(|b| b.line());
            last = Instant::now() - Duration::from_secs(1);
        }
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
            let top = u16::from(switcher.is_some() && size.1 > 3);
            if let Err(e) = tmux::resize_window(target, size.0, size.1 - top) {
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
                    error = None;
                }
                Err(e) => error = Some(format!("{e:#}")),
            }
        }
        terminal.draw(|f| {
            let full = f.area();
            // Under the switcher, when there is one (and room for it).
            let top = u16::from(switcher.is_some() && full.height > 3);
            if let Some((line, _)) = switcher.as_ref().filter(|_| top == 1) {
                f.render_widget(
                    Paragraph::new(line.clone()).style(
                        ratatui::style::Style::default()
                            .fg(crate::theme::text())
                            .bg(crate::theme::chip()),
                    ),
                    ratatui::layout::Rect::new(full.x, full.y, full.width, 1),
                );
            }
            let a = ratatui::layout::Rect::new(full.x, full.y + top, full.width, full.height - top);
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
                        Some(p) => Some(bar_line("COPY", p.label(), String::new())),
                        None if in_mode => Some(bar_line(
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
                // The overlord's key (or the back key) closes the overlay.
                if keys.closes(&k) {
                    return Ok(Exit::Close);
                }
                // The switch key: the next agent's overlord, here.
                // (With one overlord there is nothing to switch to: say
                // how to add one rather than swallow the key silently.)
                if let Some(bar) = view.bar.as_ref().filter(|_| keys.switches(&k)) {
                    match bar.next() {
                        Some(agent) => return Ok(Exit::Switch(agent)),
                        None => {
                            let mut b = bar.clone();
                            b.hint = Some(ONLY_ONE.into());
                            switcher = Some(b.line());
                            hint_until = Some(Instant::now() + HINT_FOR);
                            last = Instant::now() - Duration::from_secs(1);
                            continue;
                        }
                    }
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
                    // A click on an agent in the switcher: its overlord.
                    MouseEventKind::Down(MouseButton::Left) if m.row == 0 => {
                        let picked = switcher.as_ref().zip(view.bar.as_ref()).and_then(
                            |((_, hits), bar)| {
                                let (_, _, agent) =
                                    hits.iter().find(|(a, b, _)| (*a..*b).contains(&m.column))?;
                                // (The active one: already here.)
                                bar.agents
                                    .iter()
                                    .any(|x| &x.0 == agent && !x.2)
                                    .then(|| agent.clone())
                            },
                        );
                        match picked {
                            Some(agent) => return Ok(Exit::Switch(agent)),
                            None => continue,
                        }
                    }
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
/// `copy-command` if the user set one; else the system's clipboard tool
/// (`pbcopy` on macOS; `wl-copy` under Wayland, else `xclip` or `xsel`);
/// else OSC 52 through tmux (`load-buffer -w`), which reaches the user's
/// terminal even over ssh if it allows clipboard writes.
fn copy_to_clipboard(buffer: &str) {
    let Ok(text) = tmux::buffer_text(buffer) else {
        return;
    };
    copy_text(buffer, &text);
}

/// Put `text` on the clipboard (see `copy_to_clipboard`); `buffer` names
/// the tmux paste buffer used when it has to go through tmux.
pub(crate) fn copy_text(buffer: &str, text: &str) {
    let custom = tmux::server_option("copy-command");
    if !custom.trim().is_empty() {
        pipe_to(
            std::process::Command::new("sh").args(["-c", custom.trim()]),
            text,
        );
        return;
    }
    let has = |v: &str| std::env::var_os(v).is_some_and(|x| !x.is_empty());
    let mut tools: Vec<&[&str]> = Vec::new();
    if cfg!(target_os = "macos") {
        tools.push(&["pbcopy"]);
    } else {
        if has("WAYLAND_DISPLAY") {
            tools.push(&["wl-copy"]);
        }
        if has("DISPLAY") {
            tools.push(&["xclip", "-selection", "clipboard"]);
            tools.push(&["xsel", "-ib"]);
        }
    }
    for t in tools {
        if pipe_to(std::process::Command::new(t[0]).args(&t[1..]), text) {
            return;
        }
    }
    let _ = tmux::buffer_to_clipboard(buffer, text);
}

/// Run `cmd` with `text` on its stdin; whether it ran and succeeded.
fn pipe_to(cmd: &mut std::process::Command, text: &str) -> bool {
    use std::io::Write;
    use std::process::Stdio;
    // The clipboard tools may leave a child behind holding the selection:
    // it must not keep the popup's terminal open.
    let Ok(mut c) = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut i) = c.stdin.take() {
        let _ = i.write_all(text.as_bytes());
    }
    c.wait().is_ok_and(|s| s.success())
}

/// The copy bar: a chip in the overlord's colour naming the mode, then
/// what it's at, then the user's keys, quieter.
fn bar_line(mode: &str, what: String, hints: String) -> ratatui::text::Line<'static> {
    use crate::theme;
    use ratatui::style::Style;
    use ratatui::text::Span;
    let hue = theme::overlord();
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
/// (Option-O) and back (Option-D); both close it.
/// Inside a popup, keys reach this program, not tmux's key table.
struct OverlayKeys {
    overlord: Option<rlord_core::keys::Key>,
    close: Option<rlord_core::keys::Key>,
    /// The next agent's overlord (`[tmux] overlord_switch`; empty: off).
    switch: Option<rlord_core::keys::Key>,
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

impl OverlayKeys {
    fn from_config(cfg: &rlord_core::config::Config) -> Self {
        // An empty or unreadable key falls back to the default.
        // The same notation the key screen saves (F-keys and named keys too).
        use rlord_core::keys::Key;
        OverlayKeys {
            overlord: Key::parse(&cfg.tmux.overlord).or_else(|| Key::parse("M-o")),
            // Off is off (the overlord's key still closes it).
            close: Key::parse(&cfg.tmux.back),
            switch: Key::parse(cfg.tmux.switch_key()),
        }
    }

    /// Whether this key switches to the next agent's overlord. (Never a
    /// key that closes the overlay: that wins.)
    fn switches(&self, k: &KeyEvent) -> bool {
        let got = crate::keyscreen::key_of(k);
        got.is_some() && self.switch == got
    }

    /// Whether this key closes the overlay.
    fn closes(&self, k: &KeyEvent) -> bool {
        let got = crate::keyscreen::key_of(k);
        got.is_some() && (self.overlord == got || self.close == got)
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

    /// The switcher: every agent on the row, clickable where it is drawn;
    /// the key goes round them.
    #[test]
    fn the_switcher_row_says_where_each_agent_is() {
        use unicode_width::UnicodeWidthStr;
        let agent = |n: &str, m: &str, active: bool| (n.to_string(), m.to_string(), active, true);
        let mut bar = super::SwitchBar {
            agents: vec![agent("claude", "✻", true), agent("codex", ">_", false)],
            info: "fresh 2h ago · 14 earlier".into(),
            limited: false,
            key: "⌥s".into(),
            notice: None,
            hint: None,
        };
        let (line, hits) = bar.line();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            text,
            " ✻ claude │ >_ codex   fresh 2h ago · 14 earlier · ⌥s or click to switch"
        );
        // Each hit covers exactly its agent's text.
        for (a, b, name) in &hits {
            let mut col = 0;
            let covered: String = text
                .chars()
                .filter(|c| {
                    let at = col;
                    col += c.to_string().width() as u16;
                    (*a..*b).contains(&at)
                })
                .collect();
            assert!(covered.trim().ends_with(name.as_str()), "{covered:?}");
        }
        assert_eq!(hits.len(), 2);
        assert_eq!(bar.next().as_deref(), Some("codex"));
        bar.agents[0].2 = false;
        bar.agents[1].2 = true;
        assert_eq!(bar.next().as_deref(), Some("claude"), "round again");
        // The key skips agents without an overlord (it never creates one).
        bar.agents
            .push(("opencode".into(), "o".into(), false, false));
        assert_eq!(bar.next().as_deref(), Some("claude"));
        bar.agents[0].3 = false;
        assert_eq!(bar.next(), None);
        let (line, _) = bar.line();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.ends_with("click to switch") && !text.contains("⌥s"),
            "{text}"
        );
        // The switch key with one overlord: a passing hint, the info kept.
        bar.info = "out of usage until 3:00pm: switch to another".into();
        bar.hint = Some(ONLY_ONE.into());
        let (line, _) = bar.line();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            text.contains("out of usage until 3:00pm") && text.ends_with(ONLY_ONE),
            "{text}"
        );
        bar.hint = None;
        bar.agents[0].3 = true;
        bar.agents.pop();
        // A failed switch says why, in place of the hints.
        bar.notice = Some("codex isn't installed".into());
        let (line, _) = bar.line();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with("codex isn't installed"), "{text}");
    }

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
    fn option_keys_close_the_overlay() {
        let keys = OverlayKeys::from_config(&rlord_core::config::Config::default());
        let k = |c, m| keys.closes(&ev(KeyCode::Char(c), m));
        assert!(k('o', KeyModifiers::ALT));
        assert!(k('d', KeyModifiers::ALT));
        // (Once the overlord's other session's key: now the overlord's own.)
        assert!(!k('m', KeyModifiers::ALT));
        assert!(!k('o', KeyModifiers::NONE));
        assert!(!keys.closes(&ev(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(parse_key("M-C-x"), Some((true, true, 'x')));
        assert_eq!(parse_key("M-xy"), None);
    }
}
