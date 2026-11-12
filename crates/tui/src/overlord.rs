//! The TUI's side of the overlord:
//!
//! - **Startup check** — no overlord yet → a setup modal that is all defaults
//!   (Enter launches; agent, model, effort and Remote Control can be
//!   picked, and are saved to config.toml); an overlord on an outdated
//!   model → an upgrade modal.
//! - **UI commands** pushed by the daemon on the overlord's behalf: set the
//!   list view, open the new-session form pre-filled.
//! - **Opening it** — asks the daemon to pop the overlay (`rlord overlay`,
//!   see overlay.rs) up over this terminal.

use super::*;
use ratatui::widgets::Clear;

/// First-run setup: everything defaulted, Enter launches. One overlord:
/// its agent, model, effort and Remote Control, each pickable.
pub(crate) struct Setup {
    /// The agents that can run the overlord here (installed ones).
    agents: Vec<AgentChoice>,
    agent: usize,
    /// The model and effort picked, as indexes into the agent's choices.
    model: usize,
    effort: usize,
    /// Which row the arrows change (an index into `rows()`).
    row: usize,
    /// Remote Control for the overlord: Some(on?) when this machine can
    /// use it, None when it can't or isn't known yet.
    remote: Option<bool>,
    /// Why it can't, or a caveat when it can.
    remote_why: Option<String>,
    error: Option<String>,
}

/// One agent the overlord can run on, and what the dialog offers for it.
struct AgentChoice {
    name: String,
    label: String,
    /// Model choices ("" = the agent's own default) and where it starts.
    models: Vec<String>,
    model: usize,
    /// Effort choices ("" = the agent's default); empty: it has none.
    efforts: Vec<String>,
    effort: usize,
    /// It has Remote Control.
    remote: bool,
    /// family → latest id, for the "→ resolves to" hint.
    catalog: Vec<(String, String)>,
}

/// A row of the setup dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SetupRow {
    Agent,
    Model,
    Effort,
    Remote,
}

/// A one-time offer of Remote Control for an existing overlord.
pub(crate) struct RemoteOffer {
    note: Option<String>,
    error: Option<String>,
}

/// An existing overlord on an old model.
pub(crate) struct Upgrade {
    note: String,
    /// Model to switch to; None when switching can't help (the agent CLI
    /// resolves the alias to an old model — it needs updating instead).
    suggest: Option<String>,
    /// An offer of the default model: esc means "don't ask again".
    declinable: bool,
    error: Option<String>,
}

impl AgentChoice {
    fn from_status(a: &Value) -> Self {
        let list = |k: &str| -> Vec<String> {
            a[k].as_array()
                .map(|v| {
                    v.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        // The default heads the choices when it isn't one of them.
        let with = |mut choices: Vec<String>, dflt: &str| {
            let at = match choices.iter().position(|c| c == dflt) {
                Some(i) => i,
                None => {
                    choices.insert(0, dflt.to_string());
                    0
                }
            };
            (choices, at)
        };
        let (models, model) = with(list("models"), a["model"].as_str().unwrap_or(""));
        // No efforts: it has no such setting (no row).
        let (efforts, effort) = match list("efforts") {
            e if e.is_empty() => (e, 0),
            e => with(e, a["effort"].as_str().unwrap_or("")),
        };
        AgentChoice {
            name: a["name"].as_str().unwrap_or("claude").to_string(),
            label: a["label"].as_str().unwrap_or("Claude Code").to_string(),
            models,
            model,
            efforts,
            effort,
            remote: a["remote_control"] == true,
            catalog: catalog_of(a),
        }
    }
}

impl Setup {
    fn from_status(st: &Value) -> Self {
        let mut agents: Vec<AgentChoice> = st["agents"]
            .as_array()
            .map(|a| a.iter().map(AgentChoice::from_status).collect())
            .unwrap_or_default();
        if agents.is_empty() {
            // None found installed: offer the configured one (launching
            // says what's missing).
            agents.push(AgentChoice::from_status(&json!({
                "name": st["default_agent"], "label": st["default_agent"],
                "model": st["default_model"], "effort": st["default_effort"],
                "models": [], "efforts": [],
                "remote_control": st["default_agent"] == "claude",
            })));
        }
        let agent = agents
            .iter()
            .position(|a| Some(a.name.as_str()) == st["default_agent"].as_str())
            .unwrap_or(0);
        let mut s = Setup {
            model: agents[agent].model,
            effort: agents[agent].effort,
            agents,
            agent,
            row: 0,
            // On by default where it works: the point is to reach the
            // overlord from the Claude app.
            remote: (st["remote"]["available"] == true).then_some(true),
            remote_why: st["remote"]["reason"]
                .as_str()
                .or(st["remote"]["note"].as_str())
                .map(str::to_string),
            error: None,
        };
        // Start on the first row there is something to pick in.
        s.row = usize::from(s.agents.len() < 2);
        s
    }

    fn choice(&self) -> &AgentChoice {
        &self.agents[self.agent]
    }

    /// The rows shown, top to bottom: effort only for an agent that has
    /// one, Remote Control only where this machine and the agent can.
    fn rows(&self) -> Vec<SetupRow> {
        let mut v = vec![SetupRow::Agent, SetupRow::Model];
        if !self.choice().efforts.is_empty() {
            v.push(SetupRow::Effort);
        }
        if self.remote.is_some() && self.choice().remote {
            v.push(SetupRow::Remote);
        }
        v
    }

    fn model(&self) -> &str {
        self.choice().models.get(self.model).map_or("", |m| m)
    }

    fn effort(&self) -> &str {
        self.choice().efforts.get(self.effort).map_or("", |e| e)
    }

    /// ← / →: the focused row's next or previous choice. Another agent
    /// starts on its own default model and effort.
    fn step(&mut self, forward: bool) {
        let next = |i: usize, n: usize| {
            if n == 0 {
                0
            } else if forward {
                (i + 1) % n
            } else {
                (i + n - 1) % n
            }
        };
        match self.rows().get(self.row) {
            Some(SetupRow::Agent) => {
                let to = next(self.agent, self.agents.len());
                if to != self.agent {
                    self.agent = to;
                    self.model = self.choice().model;
                    self.effort = self.choice().effort;
                }
            }
            Some(SetupRow::Model) => self.model = next(self.model, self.choice().models.len()),
            Some(SetupRow::Effort) => self.effort = next(self.effort, self.choice().efforts.len()),
            Some(SetupRow::Remote) => self.remote = self.remote.map(|on| !on),
            None => {}
        }
        // (Another agent may have fewer rows.)
        self.row = self.row.min(self.rows().len().saturating_sub(1));
    }

    /// A click on row `i`: select it. True when it already was (the click
    /// then changes it, as → does).
    pub(crate) fn click_row(&mut self, i: usize) -> bool {
        if self.row == i {
            return true;
        }
        self.row = i.min(self.rows().len().saturating_sub(1));
        false
    }
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
            keyscreen::clear_stuck_listen();
            if let Some(home) = tmux::current_session() {
                let _ = tmux::set_home(&home);
            }
        }
        if let Some(v) = self.call("ui.view.default", Value::Null) {
            self.apply_view(&v);
        }
        // Back on the session the user last had selected.
        if let Some(id) = self
            .call("ui.selected", json!({}))
            .and_then(|v| v["id"].as_str().map(str::to_string))
        {
            self.saved_sel = Some(id.clone());
            self.pending_select = Some(id);
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
            self.mode = Mode::OverlordSetup(Box::new(Setup::from_status(&st)));
            return;
        }
        // On an old model, or on another model than the default: offer the
        // switch.
        let f = &st["freshness"];
        if let Some(d) = st["suggest_default"].as_str() {
            self.mode = Mode::OverlordUpgrade(Box::new(Upgrade {
                note: format!(
                    "it runs {}; rlord's default for the overlord is now {d}",
                    st["model"].as_str().unwrap_or("?"),
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
                note: f["note"].as_str().unwrap_or("outdated model").to_string(),
                suggest,
                declinable: false,
                error: None,
            }));
            return;
        } else if let Some(n) = f["note"].as_str() {
            self.status_line = format!("overlord: {n}");
        }
        // Remote Control for the overlord, offered once where it works.
        if st["remote"]["offer"] == true {
            self.mode = Mode::RemoteOffer(Box::new(RemoteOffer {
                note: st["remote"]["note"].as_str().map(str::to_string),
                error: None,
            }));
        }
    }

    /// Save the dialog's agent, model and effort as the `[overlord]`
    /// config (so later starts reuse them) when they differ from it.
    fn save_setup(&mut self, s: &Setup) -> Result<()> {
        let o = &self.config.overlord;
        let (agent, model, effort) = (s.choice().name.as_str(), s.model(), s.effort());
        if (o.agent.trim(), o.model.trim(), o.effort.trim()) == (agent, model, effort) {
            return Ok(());
        }
        Config::set_overlord(agent, model, effort)?;
        self.config = Config::load()?;
        // The daemon reads it too: the effort is a launch flag.
        client_call(&paths::socket_path(), "config.reload", json!({}))?;
        Ok(())
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
                let remote = s.remote == Some(true) && s.choice().remote;
                let r = self.save_setup(&s).and_then(|_| {
                    client_call(
                        &paths::socket_path(),
                        "overlord.ensure",
                        json!({"agent": s.choice().name, "model": s.model(),
                               "effort": s.effort(), "remote_control": remote}),
                    )
                });
                match r {
                    Ok(_) => {
                        // Off by choice: don't offer it again later.
                        if s.remote == Some(false) {
                            let _ = client_call(
                                &paths::socket_path(),
                                "remote.decline_offer",
                                Value::Null,
                            );
                        }
                        self.dirty = true;
                        self.show_overlord();
                    }
                    Err(e) => {
                        s.error = Some(format!("{e:#}"));
                        self.mode = Mode::OverlordSetup(s);
                    }
                }
            }
            // Rows: tab / j / k / ↑ / ↓. The row's choice: ← / → / h / l
            // (space too).
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Char('j' | 'k')
            | KeyCode::Up
            | KeyCode::Down => {
                let rows = s.rows().len();
                let back = matches!(k.code, KeyCode::BackTab | KeyCode::Char('k') | KeyCode::Up);
                s.row = if back {
                    (s.row + rows - 1) % rows
                } else {
                    (s.row + 1) % rows
                };
                self.mode = Mode::OverlordSetup(s);
            }
            KeyCode::Right | KeyCode::Char('l' | ' ') => {
                s.step(true);
                self.mode = Mode::OverlordSetup(s);
            }
            KeyCode::Left | KeyCode::Char('h') => {
                s.step(false);
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
                        json!({"model": u.suggest}),
                    );
                }
                self.status_line = "overlord model left as is".into();
            }
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
            // Never over something the user is typing into.
            Some("keys") if !matches!(self.mode, Mode::Normal | Mode::Help | Mode::Keys(_)) => {
                self.status_line =
                    "the overlord wants to open the key screen: finish here, then :keys".into();
            }
            Some("keys") => {
                self.open_keys(cmd["row"].as_str());
                self.status_line = "the overlord opened the key screen".into();
            }
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
        let params = json!({
            "client": tmux::current_client(),
            "from": tmux::current_session(),
        });
        self.call("ui.overlord", params);
    }

    pub(crate) fn draw_setup(&self, f: &mut Frame, s: &Setup) {
        let area = newform::centered(f.area(), 78, 26);
        f.render_widget(Clear, area);
        let rows = s.rows();
        let choice = s.choice();
        let or_default = |v: &str| {
            if v.is_empty() {
                "agent default".to_string()
            } else {
                v.to_string()
            }
        };
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!(
                    "  The overlord is how you run rlord: one session you talk to ({}).",
                    tmux::key_label(&self.config.tmux.overlord)
                ),
                Style::default().fg(theme::text()),
            )),
            Line::from(Span::styled(
                "  It finds, jumps, organizes and launches; anything heavy it hands to",
                Style::default().fg(theme::text()),
            )),
            Line::from(Span::styled(
                "  subagents and new sessions.",
                Style::default().fg(theme::text()),
            )),
            Line::from(""),
        ];
        // Where each row's lines start, for the mouse.
        let mut starts = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            let focused = s.row == i;
            let (name, value, extra, why): (&str, String, String, String) = match row {
                SetupRow::Agent => (
                    "Agent",
                    choice.label.clone(),
                    String::new(),
                    if s.agents.len() > 1 {
                        "The coding agent that runs it. Changing it resets model and effort."
                            .into()
                    } else {
                        "The coding agent that runs it (the only one installed that can)."
                            .into()
                    },
                ),
                SetupRow::Model => (
                    "Model",
                    or_default(s.model()),
                    choice
                        .catalog
                        .iter()
                        .find(|(fam, _)| fam == s.model())
                        .map(|(_, id)| format!(" → {id}"))
                        .unwrap_or_default(),
                    "A strong model: it decides what to launch and where.".into(),
                ),
                SetupRow::Effort => (
                    "Effort",
                    or_default(s.effort()),
                    String::new(),
                    "Medium: light work itself, the rest delegated. Lower is quicker, higher deeper."
                        .into(),
                ),
                SetupRow::Remote => (
                    "⇄ Remote",
                    if s.remote == Some(true) { "on" } else { "off" }.to_string(),
                    String::new(),
                    format!(
                        "Reach it from the Claude app or claude.ai; kept running for that.{}",
                        s.remote_why
                            .as_deref()
                            .map(|w| format!(" Note: {w}."))
                            .unwrap_or_default()
                    ),
                ),
            };
            starts.push(lines.len());
            // One choice only: nothing to step through.
            let fixed = *row == SetupRow::Agent && s.agents.len() < 2;
            lines.push(Line::from(vec![
                Span::styled(
                    if focused { "▌ " } else { "  " },
                    Style::default().fg(theme::accent()),
                ),
                Span::styled(format!("{name:<12}"), Style::default().bold()),
                Span::styled(
                    if fixed {
                        format!("  {value}  ")
                    } else {
                        format!("◀ {value} ▶")
                    },
                    if focused {
                        Style::default().bold().fg(theme::accent())
                    } else {
                        Style::default().fg(theme::text())
                    },
                ),
                Span::styled(extra, theme::faint()),
            ]));
            lines.push(Line::from(Span::styled(
                format!("    {why}"),
                theme::muted(),
            )));
            lines.push(Line::from(""));
        }
        if s.remote.is_none() {
            if let Some(why) = &s.remote_why {
                lines.push(Line::from(Span::styled(
                    format!("  ⇄ Remote Control isn't available here: {why}."),
                    theme::faint(),
                )));
                lines.push(Line::from(""));
            }
        } else if !s.choice().remote {
            // The row is gone for this agent: say why, so it isn't missed.
            lines.push(Line::from(Span::styled(
                format!(
                    "  ⇄ Remote Control is a Claude Code feature: none on {}.",
                    s.choice().label
                ),
                theme::faint(),
            )));
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            "  Saved to config.toml under [overlord]; change it there any time.",
            theme::faint(),
        )));
        lines.push(Line::from(""));
        lines.push(keys_line(&[
            ("enter", "launch", Cap::Context),
            ("tab", "row", Cap::Global),
            ("←/→", "change", Cap::Global),
            ("esc", "later", Cap::Global),
        ]));
        if let Some(e) = &s.error {
            lines.push(Line::from(Span::styled(
                format!("  {e}"),
                Style::default().fg(theme::red()),
            )));
        }
        self.modal_hits(f.area(), area, &lines);
        // Each row (its line and the one under it): a click selects it;
        // again changes it.
        for (i, at) in starts.iter().enumerate() {
            for l in 0..2 {
                self.hits
                    .row(area, area.y + 1 + *at as u16 + l, mouse::Hit::Pick(i));
            }
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
                "  Your overlord could be on a better model:",
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
        self.modal_hits(f.area(), area, &lines);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(theme::modal("◆ Overlord model")),
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
                // Don't hold the list up waiting for the link: the list
                // shows it (⇄) once connected.
                json!({"overlord": true, "on": true, "wait": false}),
            ) {
                Ok(r) => {
                    self.status_line = format!(
                        "overlord: Remote Control on — {}",
                        r["note"].as_str().unwrap_or("connecting")
                    );
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
        self.modal_hits(f.area(), area, &lines);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(theme::modal("⇄ Remote Control")),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(agents: Value, remote: bool) -> Value {
        json!({"exists": false, "default_agent": "claude", "default_model": "opus",
               "default_effort": "medium", "agents": agents,
               "remote": {"available": remote}})
    }

    fn claude() -> Value {
        json!({"name": "claude", "label": "Claude Code", "model": "opus", "effort": "medium",
               "models": ["opus", "sonnet", "haiku"],
               "efforts": ["low", "medium", "high", "max"], "remote_control": true})
    }

    #[test]
    fn setup_defaults_and_rows() {
        // One agent: Opus at medium effort, Remote Control on; the first row
        // with a choice is the model.
        let s = Setup::from_status(&status(json!([claude()]), true));
        assert_eq!(
            (s.choice().name.as_str(), s.model(), s.effort()),
            ("claude", "opus", "medium")
        );
        assert_eq!(
            s.rows(),
            [
                SetupRow::Agent,
                SetupRow::Model,
                SetupRow::Effort,
                SetupRow::Remote
            ]
        );
        assert_eq!(s.rows()[s.row], SetupRow::Model);
        assert_eq!(s.remote, Some(true));
        // Remote Control not available here: no row for it.
        let s = Setup::from_status(&status(json!([claude()]), false));
        assert_eq!(
            s.rows(),
            [SetupRow::Agent, SetupRow::Model, SetupRow::Effort]
        );
    }

    #[test]
    fn setup_rows_are_pickable_and_an_agent_change_resets_them() {
        let codex = json!({"name": "codex", "label": "Codex", "model": "", "effort": "medium",
                           "models": ["gpt-x"], "efforts": ["low", "medium", "high"],
                           "remote_control": false});
        let opencode = json!({"name": "opencode", "label": "OpenCode", "model": "",
                              "effort": "", "models": [], "efforts": [],
                              "remote_control": false});
        let mut s = Setup::from_status(&status(json!([claude(), codex, opencode]), true));
        // Several agents: the agent row comes first.
        assert_eq!(s.rows()[s.row], SetupRow::Agent);
        // Model and effort are pickable.
        s.row = 1;
        s.step(true);
        assert_eq!(s.model(), "sonnet");
        s.row = 2;
        s.step(true);
        assert_eq!(s.effort(), "high");
        s.step(false);
        s.step(false);
        assert_eq!(s.effort(), "low");
        // Remote Control toggles.
        s.row = 3;
        s.step(true);
        assert_eq!(s.remote, Some(false));
        // Another agent: its own default model and effort; Codex has no
        // Remote Control row.
        s.row = 0;
        s.step(true);
        assert_eq!(
            (s.choice().name.as_str(), s.model(), s.effort()),
            ("codex", "", "medium")
        );
        assert_eq!(
            s.rows(),
            [SetupRow::Agent, SetupRow::Model, SetupRow::Effort]
        );
        // OpenCode: its default model, and no effort row at all.
        s.step(true);
        assert_eq!(
            (s.choice().name.as_str(), s.model(), s.effort()),
            ("opencode", "", "")
        );
        assert_eq!(s.rows(), [SetupRow::Agent, SetupRow::Model]);
        // Back to Claude: its defaults again, not what was picked before.
        s.step(true);
        assert_eq!((s.model(), s.effort()), ("opus", "medium"));
    }

    #[test]
    fn setup_draws_one_overlord() {
        use ratatui::backend::TestBackend;
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let app = App::new(rx, Config::default(), keys);
        let s = Setup::from_status(&status(json!([claude()]), true));
        let mut term = ratatui::Terminal::new(TestBackend::new(100, 34)).unwrap();
        term.draw(|f| app.draw_setup(f, &s)).unwrap();
        let buf = term.backend().buffer();
        let out: String = (0..34)
            .map(|y| {
                (0..100)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        for want in [
            "Agent",
            "Claude Code",
            "Model",
            "◀ opus ▶",
            "Effort",
            "◀ medium ▶",
            "Remote",
        ] {
            assert!(out.contains(want), "{want:?} missing:\n{out}");
        }
        assert!(!out.to_lowercase().contains("mastermind"), "{out}");
    }
}
