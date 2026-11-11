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

/// First-run setup: everything defaulted, Enter launches.
pub(crate) struct Setup {
    agent: String,
    models: Vec<String>,
    model_idx: usize,
    /// family → latest id, for the "→ resolves to" hint.
    catalog: Vec<(String, String)>,
    error: Option<String>,
}

/// An existing overlord on an old model.
pub(crate) struct Upgrade {
    note: String,
    /// Model to switch to; None when switching can't help (the agent CLI
    /// resolves the alias to an old model — it needs updating instead).
    suggest: Option<String>,
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
            let default = st["default_model"].as_str().unwrap_or("").to_string();
            let mut models: Vec<String> = self.config.session.models.clone();
            if !default.is_empty() && !models.contains(&default) {
                models.insert(0, default.clone());
            }
            let model_idx = models.iter().position(|m| *m == default).unwrap_or(0);
            self.mode = Mode::OverlordSetup(Box::new(Setup {
                agent: st["default_agent"].as_str().unwrap_or("claude").to_string(),
                models,
                model_idx,
                catalog: catalog_of(&st),
                error: None,
            }));
            return;
        }
        let f = &st["freshness"];
        if f["outdated"] == true {
            let suggest = f["suggest"]
                .as_str()
                .filter(|s| Some(*s) != st["model"].as_str())
                .map(str::to_string);
            self.mode = Mode::OverlordUpgrade(Box::new(Upgrade {
                note: f["note"].as_str().unwrap_or("outdated model").to_string(),
                suggest,
                error: None,
            }));
        } else if let Some(n) = f["note"].as_str() {
            self.status_line = format!("overlord: {n}");
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
                let model = s.models.get(s.model_idx).cloned().unwrap_or_default();
                match client_call(
                    &paths::socket_path(),
                    "overlord.ensure",
                    json!({"agent": s.agent, "model": model}),
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
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab | KeyCode::Char('m') => {
                if !s.models.is_empty() {
                    s.model_idx = (s.model_idx + 1) % s.models.len();
                }
                self.mode = Mode::OverlordSetup(s);
            }
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => {
                if !s.models.is_empty() {
                    s.model_idx = (s.model_idx + s.models.len() - 1) % s.models.len();
                }
                self.mode = Mode::OverlordSetup(s);
            }
            _ => self.mode = Mode::OverlordSetup(s),
        }
    }

    pub(crate) fn handle_upgrade(&mut self, k: KeyEvent, mut u: Box<Upgrade>) {
        match k.code {
            KeyCode::Esc => self.status_line = "overlord model left as is".into(),
            KeyCode::Enter => {
                let r = match &u.suggest {
                    Some(m) => client_call(
                        &paths::socket_path(),
                        "overlord.set_model",
                        json!({"model": m}),
                    ),
                    None => client_call(&paths::socket_path(), "overlord.ensure", json!({}))
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
                        self.status_line = "overlord restarted (conversation resumed)".into();
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
        let params = json!({
            "client": tmux::current_client(),
            "from": tmux::current_session(),
        });
        self.call("ui.overlord", params);
    }

    pub(crate) fn draw_setup(&self, f: &mut Frame, s: &Setup) {
        let area = newform::centered(f.area(), 66, 13);
        f.render_widget(Clear, area);
        let model = s.models.get(s.model_idx).cloned().unwrap_or_default();
        let resolves = s
            .catalog
            .iter()
            .find(|(fam, _)| *fam == model)
            .map(|(_, id)| format!("  → {id} (latest)"))
            .unwrap_or_default();
        let mut lines = vec![
            Line::from(""),
            Line::from("  The overlord is the session you talk to to run rlord:"),
            Line::from("  launch sessions, search, jump around, set the list view."),
            Line::from(""),
            Line::from(vec![
                Span::raw("  Tool   "),
                Span::styled(s.agent.clone(), Style::default().bold()),
            ]),
            Line::from(vec![
                Span::raw("  Model  "),
                Span::styled(
                    format!("◀ {model} ▶"),
                    Style::default().bold().fg(Color::Cyan),
                ),
                Span::styled(resolves, Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(""),
            keys_line(&[
                ("enter", "launch", Cap::Context),
                ("j/k", "model", Cap::Global),
                ("esc", "later", Cap::Global),
            ]),
        ];
        if let Some(e) = &s.error {
            lines.push(Line::from(Span::styled(
                format!("  {e}"),
                Style::default().fg(Color::Red),
            )));
        }
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Magenta))
                    .title(" ◆ start your overlord "),
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
                "  Your overlord is not on the latest model:",
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
                Style::default().fg(Color::Red),
            )));
        }
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(" ◆ overlord model "),
            ),
            area,
        );
    }
}
