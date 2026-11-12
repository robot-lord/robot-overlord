//! The key screen: every key rlord binds, what stands in each one's way,
//! and rebinding by pressing the key.
//!
//! Pressing the key (rather than typing its name) is the test: while the
//! screen listens, the list's tmux session is pointed at an empty key table,
//! so the key reaches rlord unless something before tmux takes it — the
//! terminal, macOS, another app. Then it's checked (`rlord_core::keys`)
//! against rlord's other keys, the user's tmux, the shell and Claude Code,
//! and saved to `config.toml` only once the user confirms.

use std::collections::HashSet;
use std::time::Instant;

use super::*;
use ratatui::widgets::Clear;
use rlord_core::keys::{self, Env, Finding, Key, Level, Slot, SLOTS};

/// An empty key table: nothing in it, so every key goes to the pane.
const LISTEN_TABLE: &str = "rlord-listen";

pub(crate) struct KeyScreen {
    sel: usize,
    env: Env,
    /// Slots whose key arrived in a live test on this screen.
    tested: HashSet<String>,
    phase: Phase,
    /// The last thing that happened (saved, failed).
    msg: Option<(Level, String)>,
}

enum Phase {
    Browse,
    Listen { since: Instant, _guard: Listening },
    Review(Review),
}

struct Review {
    key: Key,
    findings: Vec<Finding>,
    /// The key arrived as a character Option types (Option isn't Meta).
    option_char: Option<char>,
}

/// While alive, this TUI's tmux session reads keys from an empty table.
struct Listening(Option<String>);

impl Listening {
    fn start() -> Self {
        let s = tmux::current_session();
        if let Some(s) = &s {
            let _ = tmux::set_key_table(s, Some(LISTEN_TABLE));
        }
        Listening(s)
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        if let Some(s) = &self.0 {
            let _ = tmux::set_key_table(s, None);
        }
    }
}

impl KeyScreen {
    pub(crate) fn new(at: Option<&str>) -> Self {
        let sel = at
            .and_then(|id| SLOTS.iter().position(|s| s.id() == id))
            .unwrap_or(0);
        KeyScreen {
            sel,
            env: Env::read(),
            tested: HashSet::new(),
            phase: Phase::Browse,
            msg: None,
        }
    }

    pub(crate) fn listening(&self) -> bool {
        matches!(self.phase, Phase::Listen { .. })
    }

    fn slot(&self) -> &'static Slot {
        &SLOTS[self.sel]
    }

    fn findings(&self, cfg: &Config, s: &Slot) -> Vec<Finding> {
        Key::parse(&s.current(cfg))
            .map(|k| keys::check(cfg, &self.env, s, &k))
            .unwrap_or_default()
    }
}

/// The key a terminal event stands for.
fn key_of(k: &ratatui::crossterm::event::KeyEvent) -> Option<Key> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let named = |n: &str| Some(Key::named(n, ctrl, alt));
    match k.code {
        KeyCode::Char(c) => Some(Key::char(c, ctrl, alt)),
        KeyCode::Enter => named("enter"),
        KeyCode::Tab => named("tab"),
        KeyCode::BackTab => Some(Key::named("tab", ctrl, alt)),
        KeyCode::Backspace => named("backspace"),
        KeyCode::Up => named("up"),
        KeyCode::Down => named("down"),
        KeyCode::Left => named("left"),
        KeyCode::Right => named("right"),
        KeyCode::Home => named("home"),
        KeyCode::End => named("end"),
        KeyCode::PageUp => named("pageup"),
        KeyCode::PageDown => named("pagedown"),
        KeyCode::Delete => named("delete"),
        KeyCode::Insert => named("insert"),
        KeyCode::F(n) => named(&format!("f{n}")),
        _ => None,
    }
}

impl App {
    /// Open the key screen (at one key's row).
    pub(crate) fn open_keys(&mut self, at: Option<&str>) {
        self.mode = Mode::Keys(Box::new(KeyScreen::new(at.filter(|s| !s.is_empty()))));
    }

    pub(crate) fn handle_keys(
        &mut self,
        k: ratatui::crossterm::event::KeyEvent,
        mut ks: Box<KeyScreen>,
    ) {
        let n = SLOTS.len();
        match std::mem::replace(&mut ks.phase, Phase::Browse) {
            Phase::Browse => match k.code {
                KeyCode::Esc | KeyCode::Char('q') => return,
                KeyCode::Down | KeyCode::Char('j') => ks.sel = (ks.sel + 1).min(n - 1),
                KeyCode::Up | KeyCode::Char('k') => ks.sel = ks.sel.saturating_sub(1),
                KeyCode::Enter => {
                    ks.msg = None;
                    ks.phase = Phase::Listen {
                        since: Instant::now(),
                        _guard: Listening::start(),
                    };
                }
                KeyCode::Char('d') => {
                    let s = ks.slot();
                    let spec = s.default_spec();
                    let r = self.save_key(s, &spec);
                    ks.msg = Some(r);
                }
                KeyCode::Char('x') if ks.slot().optional => {
                    let s = ks.slot();
                    let r = self.save_key(s, "");
                    ks.msg = Some(r);
                }
                _ => {}
            },
            Phase::Listen { .. } => {
                // The guard drops here: the list's keys are back.
                if k.code == KeyCode::Esc && k.modifiers.is_empty() {
                    ks.msg = None;
                } else if let Some(key) = key_of(&k) {
                    let s = ks.slot();
                    // Option typed a character: the terminal doesn't send
                    // Option as Meta. Check the key it was meant to be.
                    let oc = match (&k.code, key.ctrl, key.alt) {
                        (KeyCode::Char(c), false, false) => keys::option_char(*c).map(|l| (*c, l)),
                        _ => None,
                    };
                    let key = oc.map(|(_, l)| Key::char(l, false, true)).unwrap_or(key);
                    if oc.is_none() {
                        ks.tested.insert(s.id());
                    }
                    ks.phase = Phase::Review(Review {
                        findings: keys::check(&self.config, &ks.env, s, &key),
                        key,
                        option_char: oc.map(|(c, _)| c),
                    });
                }
            }
            Phase::Review(r) => match k.code {
                KeyCode::Enter if !r.findings.iter().any(|f| f.level == Level::Taken) => {
                    let s = ks.slot();
                    let spec = s.spell(&r.key);
                    let res = self.save_key(s, &spec);
                    ks.msg = Some(res);
                }
                KeyCode::Esc => {}
                // Try another key.
                _ => {
                    ks.phase = Phase::Listen {
                        since: Instant::now(),
                        _guard: Listening::start(),
                    };
                }
            },
        }
        self.mode = Mode::Keys(ks);
    }

    /// Write one key to config.toml and apply it: this TUI's keys now, the
    /// global ones via the daemon.
    fn save_key(&mut self, s: &Slot, spec: &str) -> (Level, String) {
        if let Err(e) = Config::set_value(s.section, s.field, spec) {
            return (Level::Taken, format!("not saved: {e:#}"));
        }
        let cfg = match Config::load() {
            Ok(c) => c,
            Err(e) => {
                return (
                    Level::Taken,
                    format!("saved, but config.toml won't load: {e:#}"),
                )
            }
        };
        match KeyMap::from_config(&cfg.keys) {
            Ok(km) => self.keys = km,
            Err(e) => {
                return (
                    Level::Taken,
                    format!("saved, but the list can't use it: {e:#}"),
                )
            }
        }
        self.config = cfg;
        if s.section == "tmux" {
            self.call("config.reload", json!({}));
        }
        let what = if spec.is_empty() {
            "off".to_string()
        } else {
            Key::parse(spec)
                .map(|k| k.label())
                .unwrap_or(spec.to_string())
        };
        (
            Level::Note,
            format!("saved: {} is {what} (in config.toml)", s.does),
        )
    }

    pub(crate) fn draw_keys(&self, f: &mut Frame, ks: &KeyScreen) {
        let area = newform::centered(f.area(), 100, f.area().height.saturating_sub(2));
        f.render_widget(Clear, area);
        let block = theme::modal("Keys");
        let inner = block.inner(area);
        f.render_widget(block, area);
        // The rows, then the selected key's details below.
        let detail_h = 8u16.min(inner.height / 2);
        let [list_area, detail_area] =
            RLayout::vertical([Constraint::Min(0), Constraint::Length(detail_h)]).areas(inner);

        let mark = |fs: &[Finding], tested: bool| -> Span<'static> {
            match fs.first().map(|f| f.level) {
                Some(Level::Taken) => Span::styled(" ✗ ", Style::default().fg(theme::red())),
                Some(Level::Shadows) => Span::styled(" ⚠ ", Style::default().fg(theme::yellow())),
                _ if tested => Span::styled(" ✓ ", Style::default().fg(theme::green())),
                _ => Span::raw("   "),
            }
        };
        let mut lines: Vec<Line> = Vec::new();
        let mut sel_line = 0usize;
        let mut scope = None;
        for (i, s) in SLOTS.iter().enumerate() {
            if scope != Some(s.scope) {
                scope = Some(s.scope);
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                lines.push(Line::styled(
                    match s.scope {
                        keys::Scope::Global => "Anywhere: every session, the list and the overlay",
                        keys::Scope::List => "In the list",
                    },
                    Style::default().fg(theme::accent()).bold(),
                ));
            }
            let spec = s.current(&self.config);
            let key = Key::parse(&spec)
                .map(|k| k.label())
                .unwrap_or_else(|| "off".into());
            let fs = ks.findings(&self.config, s);
            let mut line = Line::from(vec![
                mark(&fs, ks.tested.contains(&s.id())),
                Span::styled(
                    format!("{key:<10}"),
                    Style::default().fg(theme::text()).bold(),
                ),
                Span::styled(s.does.to_string(), theme::muted()),
            ]);
            if i == ks.sel {
                sel_line = lines.len();
                line = line.style(theme::selected());
            }
            lines.push(line);
        }
        let h = list_area.height as usize;
        let top = (sel_line + 2).saturating_sub(h);
        f.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), list_area);

        // Details: what's going on with the selected key.
        let s = ks.slot();
        let rule = Line::styled(
            "─".repeat(detail_area.width as usize),
            Style::default().fg(theme::border()),
        );
        let level_line = |f: &Finding| {
            let (sym, c) = match f.level {
                Level::Taken => ("✗ ", theme::red()),
                Level::Shadows => ("⚠ ", theme::yellow()),
                Level::Note => ("· ", theme::muted_c()),
            };
            Line::from(vec![
                Span::styled(sym, Style::default().fg(c)),
                Span::styled(f.text.clone(), Style::default().fg(theme::text())),
            ])
        };
        let hints = |items: &[(&str, &str)]| {
            keys_line(
                &items
                    .iter()
                    .map(|(k, l)| (*k, *l, Cap::Context))
                    .collect::<Vec<_>>(),
            )
        };
        let mut d = vec![rule];
        match &ks.phase {
            Phase::Browse => {
                if let Some((lv, m)) = &ks.msg {
                    d.push(level_line(&Finding {
                        level: *lv,
                        text: m.clone(),
                    }));
                }
                let fs = ks.findings(&self.config, s);
                if fs.is_empty() && ks.msg.is_none() {
                    d.push(Line::styled(
                        if ks.tested.contains(&s.id()) {
                            "reaches rlord, and nothing else uses it"
                        } else {
                            "nothing in the way that rlord can see (enter tests it live)"
                        },
                        theme::muted(),
                    ));
                }
                d.extend(fs.iter().map(level_line));
                let mut h = vec![("enter", "change"), ("d", "default")];
                if s.optional {
                    h.push(("x", "turn off"));
                }
                h.push(("esc", "close"));
                d.push(Line::from(""));
                d.push(hints(&h));
            }
            Phase::Listen { since, .. } => {
                d.push(Line::from(vec![
                    Span::styled("Press the new key for ", Style::default().fg(theme::text())),
                    Span::styled(s.does, Style::default().fg(theme::accent()).bold()),
                    Span::styled("  (esc: cancel)", theme::faint()),
                ]));
                if since.elapsed() > Duration::from_secs(3) {
                    d.push(Line::styled(
                        "Nothing yet? If pressing it does nothing here, it never reaches rlord: your terminal, macOS or another app takes it (or it's a tmux prefix).",
                        Style::default().fg(theme::yellow()),
                    ));
                }
            }
            Phase::Review(r) => {
                let label = r.key.label();
                if let Some(c) = r.option_char {
                    d.push(Line::styled(
                        format!(
                            "Your terminal typed \"{c}\": Option isn't sent as Meta, so {label} can't reach rlord yet. {}",
                            keys::option_as_meta_fix(&tmux::client_termtype())
                        ),
                        Style::default().fg(theme::yellow()),
                    ));
                } else {
                    d.push(Line::from(vec![
                        Span::styled("✓ ", Style::default().fg(theme::green())),
                        Span::styled(
                            format!("{label} reaches rlord"),
                            Style::default().fg(theme::text()),
                        ),
                    ]));
                }
                d.extend(r.findings.iter().map(level_line));
                let blocked = r.findings.iter().any(|f| f.level == Level::Taken);
                d.push(Line::from(""));
                if blocked {
                    d.push(hints(&[("any key", "try another"), ("esc", "back")]));
                } else {
                    let save = format!("save {label}");
                    d.push(keys_line(&[
                        ("enter", save.as_str(), Cap::Context),
                        ("any key", "try another", Cap::Global),
                        ("esc", "back", Cap::Global),
                    ]));
                }
            }
        }
        f.render_widget(
            Paragraph::new(d).wrap(ratatui::widgets::Wrap { trim: false }),
            detail_area,
        );
    }
}
