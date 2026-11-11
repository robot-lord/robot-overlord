//! The TUI's side of the overlord:
//!
//! - **Startup check** — no overlord yet → a setup modal that is all defaults
//!   (Enter launches); an overlord on an outdated model → an upgrade modal.
//! - **UI commands** pushed by the daemon on the overlord's behalf: set the
//!   list view, open the new-session form pre-filled.
//! - **Opening it** — asks the daemon to pop the overlay (`rlord overlay`,
//!   see overlay.rs) up over this terminal.

use super::*;
use ratatui::widgets::Clear;

/// First-run setup: everything defaulted, Enter launches. Two rows: the
/// overlord (front desk) and the mastermind, each with its model.
pub(crate) struct Setup {
    agent: String,
    models: Vec<String>,
    /// Model index per row: [overlord, mastermind].
    idx: [usize; 2],
    /// Effort per row (from config; shown, set in config.toml).
    effort: [String; 2],
    /// Which row the arrows change.
    row: usize,
    /// family → latest id, for the "→ resolves to" hint.
    catalog: Vec<(String, String)>,
    /// Remote Control for the overlord: Some(on?) when this machine can
    /// use it (a third row), None when it can't or isn't known yet.
    remote: Option<bool>,
    /// Why it can't, or a caveat when it can.
    remote_why: Option<String>,
    error: Option<String>,
}

/// A one-time offer of Remote Control for an existing overlord.
pub(crate) struct RemoteOffer {
    note: Option<String>,
    error: Option<String>,
}

/// An existing overlord seat on an old model.
pub(crate) struct Upgrade {
    /// `overlord` or `mastermind`.
    seat: String,
    note: String,
    /// Model to switch to; None when switching can't help (the agent CLI
    /// resolves the alias to an old model — it needs updating instead).
    suggest: Option<String>,
    /// An offer of the seat's default model: esc means "don't ask again".
    declinable: bool,
    error: Option<String>,
}

fn catalog_of(st: &Value) -> Vec<(String, String)> {
    st["catalog"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    Some((
                        c["family"].as_str()?.to_string(),
                        c["latest"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

impl App {
    /// Called once before the event loop.
    pub(crate) fn startup(&mut self) {
        if tmux::inside_rlord() {
            if let Some(home) = tmux::current_session() {
                let _ = tmux::set_home(&home);
            }
        }
        if let Some(v) = self.call("ui.view.default", Value::Null) {
            self.apply_view(&v);
        }
        if let Some(cmd) = self
            .call("ui.pending", Value::Null)
            .filter(|c| !c.is_null())
        {
            self.handle_ui_cmd(cmd);
            return; // the overlord asked for something; don't bury it
        }
        self.check_overlord();
    }

    pub(crate) fn check_overlord(&mut self) {
        let Some(st) = self.call("overlord.status", Value::Null) else {
            return;
        };
        if st["exists"] != true {
            let front = st["default_model"].as_str().unwrap_or("").to_string();
            let mind = st["mastermind"]["default_model"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let mut models: Vec<String> = self.config.session.models.clone();
            for m in [&mind, &front] {
                if !m.is_empty() && !models.contains(m) {
                    models.insert(0, m.clone());
                }
            }
            let at = |m: &str| models.iter().position(|x| x == m).unwrap_or(0);
            let idx = [at(&front), at(&mind)];
            self.mode = Mode::OverlordSetup(Box::new(Setup {
                agent: st["default_agent"].as_str().unwrap_or("claude").to_string(),
                idx,
                effort: [
                    st["default_effort"].as_str().unwrap_or("").to_string(),
                    st["mastermind"]["default_effort"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                ],
                row: 0,
                models,
                catalog: catalog_of(&st),
                // On by default where it works: the point is to reach the
                // overlord from the Claude app.
                remote: (st["remote"]["available"] == true).then_some(true),
                remote_why: st["remote"]["reason"]
                    .as_str()
                    .or(st["remote"]["note"].as_str())
                    .map(str::to_string),
                error: None,
            }));
            return;
        }
        // Either seat on an old model, or on another model than its default:
        // offer the switch (the overlord first).
        for (seat, st) in [("overlord", &st), ("mastermind", &st["mastermind"])] {
            let f = &st["freshness"];
            if st["exists"] != true {
                continue;
            }
            if let Some(d) = st["suggest_default"].as_str() {
                self.mode = Mode::OverlordUpgrade(Box::new(Upgrade {
                    seat: seat.into(),
                    note: format!(
                        "it runs {}; rlord's default for the {seat} is now {d}{}",
                        st["model"].as_str().unwrap_or("?"),
                        if seat == "overlord" {
                            " (a fast front desk that hands deeper work to the mastermind)"
                        } else {
                            ""
                        }
                    ),
                    suggest: Some(d.to_string()),
                    declinable: true,
                    error: None,
                }));
                return;
            }
            if f["outdated"] == true {
                let suggest = f["suggest"]
                    .as_str()
                    .filter(|s| Some(*s) != st["model"].as_str())
                    .map(str::to_string);
                self.mode = Mode::OverlordUpgrade(Box::new(Upgrade {
                    seat: seat.into(),
                    note: f["note"].as_str().unwrap_or("outdated model").to_string(),
                    suggest,
                    declinable: false,
                    error: None,
                }));
                return;
            } else if let Some(n) = f["note"].as_str() {
                self.status_line = format!("{seat}: {n}");
            }
        }
        // Remote Control for the overlord, offered once where it works.
        if st["remote"]["offer"] == true {
            self.mode = Mode::RemoteOffer(Box::new(RemoteOffer {
                note: st["remote"]["note"].as_str().map(str::to_string),
                error: None,
            }));
        }
    }

    pub(crate) fn handle_setup(&mut self, k: KeyEvent, mut s: Box<Setup>) {
        match k.code {
            KeyCode::Esc => {
                self.status_line = format!(
                    "overlord not started — press {} any time",
                    self.keys.label(Action::Overlord)
                );
            }
            KeyCode::Enter => {
                let model = |i: usize| s.models.get(s.idx[i]).cloned().unwrap_or_default();
                match client_call(
                    &paths::socket_path(),
                    "overlord.ensure",
                    json!({"agent": s.agent, "model": model(0), "mastermind_model": model(1),
                           "remote_control": s.remote == Some(true)}),
                ) {
                    Ok(_) => {
                        self.dirty = true;
                        self.show_overlord();
                    }
                    Err(e) => {
                        s.error = Some(format!("{e:#}"));
                        self.mode = Mode::OverlordSetup(s);
                    }
                }
            }
            // Rows: tab / j / k / ↑ / ↓. Model of the row: ← / → / h / l.
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Char('j' | 'k')
            | KeyCode::Up
            | KeyCode::Down => {
                let rows = if s.remote.is_some() { 3 } else { 2 };
                let back = matches!(k.code, KeyCode::BackTab | KeyCode::Char('k') | KeyCode::Up);
                s.row = if back {
                    (s.row + rows - 1) % rows
                } else {
                    (s.row + 1) % rows
                };
                self.mode = Mode::OverlordSetup(s);
            }
            // The Remote Control row: any of ← → space toggles it.
            KeyCode::Right | KeyCode::Left | KeyCode::Char('l' | 'h' | ' ') if s.row == 2 => {
                s.remote = s.remote.map(|on| !on);
                self.mode = Mode::OverlordSetup(s);
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if !s.models.is_empty() {
                    s.idx[s.row] = (s.idx[s.row] + 1) % s.models.len();
                }
                self.mode = Mode::OverlordSetup(s);
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if !s.models.is_empty() {
                    let n = s.models.len();
                    s.idx[s.row] = (s.idx[s.row] + n - 1) % n;
                }
                self.mode = Mode::OverlordSetup(s);
            }
            _ => self.mode = Mode::OverlordSetup(s),
        }
    }

    pub(crate) fn handle_upgrade(&mut self, k: KeyEvent, mut u: Box<Upgrade>) {
        match k.code {
            KeyCode::Esc => {
                if u.declinable {
                    let _ = client_call(
                        &paths::socket_path(),
                        "overlord.decline_default",
                        json!({"seat": u.seat, "model": u.suggest}),
                    );
                }
                self.status_line = format!("{} model left as is", u.seat);
            }
            KeyCode::Enter => {
                let r = match &u.suggest {
                    Some(m) => client_call(
                        &paths::socket_path(),
                        "overlord.set_model",
                        json!({"model": m, "seat": u.seat}),
                    ),
                    None => client_call(
                        &paths::socket_path(),
                        "overlord.ensure",
                        json!({"seat": u.seat}),
                    )
                    .and_then(|s| {
                        client_call(
                            &paths::socket_path(),
                            "session.restart",
                            json!({"ref": s["id"]}),
                        )
                    }),
                };
                match r {
                    Ok(_) => {
                        self.status_line = format!("{} restarted (conversation resumed)", u.seat);
                        self.dirty = true;
                    }
                    Err(e) => {
                        u.error = Some(format!("{e:#}"));
                        self.mode = Mode::OverlordUpgrade(u);
                    }
                }
            }
            _ => self.mode = Mode::OverlordUpgrade(u),
        }
    }

    /// Apply a `{query, sort, reverse, archived}` view.
    pub(crate) fn apply_view(&mut self, v: &Value) {
        if let Some(q) = v["query"].as_str() {
            self.filter = q.to_string();
        }
        if let Some(m) = v["sort"].as_str().and_then(SortMode::parse) {
            self.sort = m;
        }
        if let Some(r) = v["reverse"].as_bool() {
            self.reverse = r;
        }
        if let Some(a) = v["archived"].as_str() {
            self.show_archived = a == "only";
        }
        if let Some(t) = v["tab"].as_str() {
            self.tab = if t == "groups" {
                Tab::Groups
            } else {
                Tab::Sessions
            };
            self.group_focus = self.tab == Tab::Groups;
        }
        if let Some(g) = v["group"].as_str() {
            self.group_sel = Some(g.to_string());
            self.group_focus = false;
        }
        self.table.select(Some(0));
        self.dirty = true;
    }

    pub(crate) fn current_view(&self) -> Value {
        json!({
            "query": self.filter,
            "sort": self.sort.label(),
            "reverse": self.reverse,
            "archived": if self.show_archived { "only" } else { "live" },
            "tab": self.tab.label(),
            "group": if self.tab == Tab::Groups { json!(self.group_sel) } else { Value::Null },
        })
    }

    /// A command the overlord sent (via the daemon).
    pub(crate) fn handle_ui_cmd(&mut self, cmd: Value) {
        match cmd["cmd"].as_str() {
            Some("set_view") => {
                self.mode = Mode::Normal;
                self.apply_view(&cmd["view"]);
                self.pending_select = cmd["select"].as_str().map(str::to_string);
                self.status_line = "overlord set the view".into();
            }
            Some("overlord_setup") => self.check_overlord(),
            Some("confirm_new_session") => {
                self.open_new_form(newform::FormKind::Overlord);
                if let Mode::NewSession(form) = &mut self.mode {
                    form.prefill_confirm(&cmd["prefill"]);
                }
                self.status_line =
                    "the overlord updated your new session — check it, then send".into();
            }
            Some("new_session") => {
                self.open_new_form(newform::FormKind::Manual);
                if let Mode::NewSession(form) = &mut self.mode {
                    form.prefill(&cmd["prefill"]);
                }
                self.status_line =
                    "overlord proposed a session — review, then enter to launch".into();
            }
            _ => {}
        }
    }

    /// Open the overlord over this terminal (a tmux popup running
    /// `rlord overlay`); the daemon starts it if needed. No overlord yet →
    /// the setup dialog.
    pub(crate) fn show_overlord(&mut self) {
        self.show_seat("overlord");
    }

    /// Open an overlord seat (`overlord` / `mastermind`) over this terminal.
    pub(crate) fn show_seat(&mut self, seat: &str) {
        let params = json!({
            "client": tmux::current_client(),
            "from": tmux::current_session(),
            "seat": seat,
        });
        self.call("ui.overlord", params);
    }

    pub(crate) fn draw_setup(&self, f: &mut Frame, s: &Setup) {
        let area = newform::centered(f.area(), 78, 25);
        f.render_widget(Clear, area);
        let keys = |k: &str| tmux::key_label(k);
        let row = |i: usize, name: &str, key: &str, why: [&str; 2]| -> Vec<Line<'static>> {
            let model = s.models.get(s.idx[i]).cloned().unwrap_or_default();
            let resolves = s
                .catalog
                .iter()
                .find(|(fam, _)| *fam == model)
                .map(|(_, id)| format!(" → {id}"))
                .unwrap_or_default();
            let focused = s.row == i;
            let effort = if s.effort[i].is_empty() {
                String::new()
            } else {
                format!("  · effort {}", s.effort[i])
            };
            vec![
                Line::from(vec![
                    Span::styled(
                        if focused { "▌ " } else { "  " },
                        Style::default().fg(theme::accent()),
                    ),
                    Span::styled(format!("{name:<12}"), Style::default().bold()),
                    Span::styled(format!("{key:<4}"), theme::faint()),
                    Span::styled(
                        format!("◀ {model} ▶"),
                        if focused {
                            Style::default().bold().fg(theme::accent())
                        } else {
                            Style::default().fg(theme::text())
                        },
                    ),
                    Span::styled(format!("{resolves}{effort}"), theme::faint()),
                ]),
                Line::from(Span::styled(format!("    {}", why[0]), theme::muted())),
                Line::from(Span::styled(format!("    {}", why[1]), theme::muted())),
                Line::from(""),
            ]
        };
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  The overlord is how you run rlord. It's two sessions:",
                Style::default().fg(theme::text()),
            )),
            Line::from(""),
        ];
        lines.extend(row(
            0,
            "◆ Overlord",
            &keys(&self.config.tmux.overlord),
            [
                "Your quick front desk: finds, jumps, organizes, answers from",
                "rlord. A fast model so it replies in seconds; it hands the rest on.",
            ],
        ));
        lines.extend(row(
            1,
            "↳ Mastermind",
            &keys(self.config.tmux.mastermind_switch()),
            [
                "Launches sessions and takes questions that need thought. A strong",
                "model, where judgment pays; starts the first time it's needed.",
            ],
        ));
        match (s.remote, &s.remote_why) {
            (Some(on), why) => {
                let focused = s.row == 2;
                lines.push(Line::from(vec![
                    Span::styled(
                        if focused { "▌ " } else { "  " },
                        Style::default().fg(theme::accent()),
                    ),
                    Span::styled(format!("{:<16}", "⇄ Remote"), Style::default().bold()),
                    Span::styled(
                        if on { "◀ on ▶" } else { "◀ off ▶" },
                        if focused {
                            Style::default().bold().fg(theme::accent())
                        } else {
                            Style::default().fg(theme::text())
                        },
                    ),
                ]));
                lines.push(Line::from(Span::styled(
                    "    Talk to the overlord from the Claude app (Code tab) or claude.ai,",
                    theme::muted(),
                )));
                lines.push(Line::from(Span::styled(
                    format!(
                        "    away from this terminal.{}",
                        why.as_deref()
                            .map(|w| format!(" Note: {w}."))
                            .unwrap_or_default()
                    ),
                    theme::muted(),
                )));
                lines.push(Line::from(""));
            }
            (None, Some(why)) => {
                lines.push(Line::from(Span::styled(
                    format!("  ⇄ Remote Control isn't available here: {why}."),
                    theme::faint(),
                )));
                lines.push(Line::from(""));
            }
            (None, None) => {}
        }
        lines.push(Line::from(Span::styled(
            "  Effort and the defaults live in config.toml under [overlord].",
            theme::faint(),
        )));
        lines.push(Line::from(""));
        lines.push(keys_line(&[
            ("enter", "launch", Cap::Context),
            ("tab", "row", Cap::Global),
            ("←/→", "model", Cap::Global),
            ("esc", "later", Cap::Global),
        ]));
        if let Some(e) = &s.error {
            lines.push(Line::from(Span::styled(
                format!("  {e}"),
                Style::default().fg(theme::red()),
            )));
        }
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                theme::modal("◆ Start your overlord")
                    .border_style(Style::default().fg(theme::overlord())),
            ),
            area,
        );
    }

    pub(crate) fn draw_upgrade(&self, f: &mut Frame, u: &Upgrade) {
        let area = newform::centered(f.area(), 70, 10);
        f.render_widget(Clear, area);
        let action = match &u.suggest {
            Some(m) => format!("switch to `{m}` and restart (conversation resumes)"),
            None => "restart it (after updating the CLI)".to_string(),
        };
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                if u.seat == "mastermind" {
                    "  Your mastermind could be on a better model:"
                } else {
                    "  Your overlord could be on a better model:"
                },
                Style::default().bold(),
            )),
            Line::from(format!("  {}", u.note)),
            Line::from(""),
            keys_line(&[("enter", &action, Cap::Context)]),
            keys_line(&[("esc", "keep it", Cap::Global)]),
        ];
        if let Some(e) = &u.error {
            lines.push(Line::from(Span::styled(
                format!("  {e}"),
                Style::default().fg(theme::red()),
            )));
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(theme::modal(if u.seat == "mastermind" {
                    "◆ Mastermind model"
                } else {
                    "◆ Overlord model"
                })),
            area,
        );
    }
}

impl App {
    pub(crate) fn handle_remote_offer(&mut self, k: KeyEvent, mut o: Box<RemoteOffer>) {
        match k.code {
            KeyCode::Esc => {
                let _ = client_call(&paths::socket_path(), "remote.decline_offer", Value::Null);
                self.status_line =
                    "Remote Control off — ask the overlord to turn it on any time".into();
            }
            KeyCode::Enter => match client_call(
                &paths::socket_path(),
                "remote.set",
                json!({"ref": "overlord", "on": true}),
            ) {
                Ok(r) => {
                    self.status_line = match r["remote"]["url"].as_str() {
                        Some(u) => format!("overlord on Remote Control: {u}"),
                        None => format!(
                            "overlord: Remote Control on — {}",
                            r["note"].as_str().unwrap_or("connecting")
                        ),
                    };
                    self.dirty = true;
                }
                Err(e) => {
                    o.error = Some(format!("{e:#}"));
                    self.mode = Mode::RemoteOffer(o);
                }
            },
            _ => self.mode = Mode::RemoteOffer(o),
        }
    }

    pub(crate) fn draw_remote_offer(&self, f: &mut Frame, o: &RemoteOffer) {
        let area = newform::centered(f.area(), 72, 12);
        f.render_widget(Clear, area);
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Reach your overlord from your phone?",
                Style::default().bold(),
            )),
            Line::from(Span::styled(
                "  Remote Control lets you talk to it from the Claude app (Code tab) or",
                theme::muted(),
            )),
            Line::from(Span::styled(
                "  claude.ai while it keeps running here. Any session can have it too:",
                theme::muted(),
            )),
            Line::from(Span::styled(
                "  ask the overlord (\"turn on remote control for <session>\").",
                theme::muted(),
            )),
        ];
        if let Some(n) = &o.note {
            lines.push(Line::from(Span::styled(
                format!("  Note: {n}."),
                theme::faint(),
            )));
        }
        lines.push(Line::from(""));
        lines.push(keys_line(&[("enter", "turn it on", Cap::Context)]));
        lines.push(keys_line(&[(
            "esc",
            "no thanks (won't ask again)",
            Cap::Global,
        )]));
        if let Some(e) = &o.error {
            lines.push(Line::from(Span::styled(
                format!("  {e}"),
                Style::default().fg(theme::red()),
            )));
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(theme::modal("⇄ Remote Control")),
            area,
        );
    }
}
