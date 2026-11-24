//! The Devices screen (`:relay`): remote access through a relay. The
//! paired devices, a pairing offer's QR code, and the device waiting for
//! the user's confirmation (its name and code), with keys to pair, confirm,
//! turn away, unpair, and make a device view-only (or let it type again). Everything goes through the daemon's `relay.*`
//! commands (crates/daemon/src/relay.rs), which ask the web plugin; the
//! screen holds no keys and only draws the offer's string
//! (`rlord_core::qr`).

use ratatui::style::Color;
use ratatui::widgets::Clear;
use rlord_core::qr::Qr;

use super::*;

/// How long an offer stands before the plugin's reports must show it.
const OFFER_GRACE: Duration = Duration::from_secs(3);

pub(crate) struct Devices {
    /// The daemon's `relay.status`.
    status: Value,
    /// The pairing offer this screen made: its code's terminal lines,
    /// when it ends (Unix seconds), and when it was made.
    offer: Option<(Vec<String>, Option<i64>, Instant)>,
    sel: usize,
    /// "Unpair this one?": the device's id and name, until y or n.
    revoking: Option<(String, String)>,
    /// What the last action did, or why it failed.
    msg: Option<String>,
}

/// What a key asks for (the screen's caller makes the call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Act {
    Close,
    Pair,
    CancelPair,
    Confirm,
    Reject,
    Revoke(String),
    /// Let the device type into sessions (true) or only watch (false).
    Input(String, bool),
}

impl Devices {
    pub(crate) fn new(status: Value) -> Devices {
        Devices {
            status,
            offer: None,
            sel: 0,
            revoking: None,
            msg: None,
        }
    }

    fn devices(&self) -> &[Value] {
        self.status["devices"].as_array().map_or(&[], Vec::as_slice)
    }

    fn pending(&self) -> Option<&Value> {
        Some(&self.status["pending"]).filter(|p| p.is_object())
    }

    /// A fresh `relay.status`. The offer goes once a device took it, or
    /// the plugin no longer has it open (it ran out, or was cancelled; a
    /// report a moment older than the offer doesn't count).
    pub(crate) fn set_status(&mut self, status: Value) {
        self.status = status;
        let reported = |made: &Instant| made.elapsed() >= OFFER_GRACE;
        if self.pending().is_some()
            || (!self.status["pairing"].is_object()
                && self.offer.as_ref().is_some_and(|o| reported(&o.2)))
        {
            self.offer = None;
        }
        self.sel = self.sel.min(self.devices().len().saturating_sub(1));
    }

    /// `relay.pair`'s answer: its code, to draw.
    pub(crate) fn set_offer(&mut self, offer: &Value) {
        self.offer = offer["qr"]
            .as_str()
            .and_then(|q| Qr::new(q).ok())
            .map(|q| (q.half_blocks(2), offer["expires"].as_i64(), Instant::now()));
        self.msg = Some("scan the code on the device you are pairing".into());
    }

    pub(crate) fn say(&mut self, msg: impl Into<String>) {
        self.msg = Some(msg.into());
    }

    /// One key. None: handled here (a move, a question), or nothing.
    pub(crate) fn key(&mut self, k: KeyEvent) -> Option<Act> {
        if let Some((id, _)) = self.revoking.take() {
            return match k.code {
                KeyCode::Char('y') | KeyCode::Enter => Some(Act::Revoke(id)),
                _ => {
                    self.msg = Some("kept".into());
                    None
                }
            };
        }
        let n = self.devices().len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => Some(Act::Close),
            KeyCode::Down | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(n.saturating_sub(1));
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.sel = self.sel.saturating_sub(1);
                None
            }
            KeyCode::Char('p') => Some(Act::Pair),
            KeyCode::Char('x') if self.offer.is_some() => Some(Act::CancelPair),
            KeyCode::Char('y') if self.pending().is_some() => Some(Act::Confirm),
            KeyCode::Char('n') if self.pending().is_some() => Some(Act::Reject),
            // A view-only device watches sessions and can't type or take
            // one over; new devices may type.
            KeyCode::Char('v') => {
                let d = self.devices().get(self.sel)?;
                let id = d["id"].as_str()?.to_string();
                Some(Act::Input(id, d["input"] == false))
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                let d = self.devices().get(self.sel)?;
                let id = d["id"].as_str()?.to_string();
                let name = d["name"].as_str().unwrap_or("").to_string();
                self.msg = None;
                self.revoking = Some((id, name));
                None
            }
            _ => None,
        }
    }

    /// The screen's lines, but for the code (drawn on its own).
    fn lines(&self) -> Vec<Line<'static>> {
        let st = &self.status;
        let s = |k: &str| st[k].as_str().unwrap_or("").to_string();
        let mut out = Vec::new();
        let head = Style::default().fg(theme::accent()).bold();
        // The relay and the link.
        let link = if s("relay").is_empty() {
            "no relay set up: run `rlord relay setup <relay address>` in a shell".to_string()
        } else if st["enabled"] != true {
            format!("{}: off (`rlord relay setup` turns it on)", s("relay"))
        } else if st["plugin"]["installed"] != true {
            format!(
                "{}: {}",
                s("relay"),
                st["plugin"]["problem"]
                    .as_str()
                    .unwrap_or("the web plugin is not installed")
            )
        } else if st["connected"] == true {
            format!("{}: connected", s("relay"))
        } else {
            match st["error"].as_str() {
                Some(e) => format!("{}: not connected ({e})", s("relay")),
                None => format!("{}: not connected", s("relay")),
            }
        };
        out.push(Line::from(vec![
            Span::styled("relay  ", head),
            Span::styled(link, theme::muted()),
        ]));
        out.push(Line::from(""));
        // The device waiting for the user.
        if let Some(p) = self.pending() {
            let code = p["code"].as_str().unwrap_or("?");
            let code = if code.len() == 6 && code.is_ascii() {
                format!("{} {}", &code[..3], &code[3..])
            } else {
                code.to_string()
            };
            out.push(Line::from(vec![
                Span::styled("waiting  ", head),
                Span::styled(
                    format!(
                        "\"{}\" wants to pair, code ",
                        p["name"].as_str().unwrap_or("?")
                    ),
                    Style::default().fg(theme::text()),
                ),
                Span::styled(code, Style::default().fg(theme::yellow()).bold()),
            ]));
            out.push(Line::styled(
                "         check the device shows the same code:  y pair it · n turn it away",
                theme::muted(),
            ));
            out.push(Line::from(""));
        }
        // The devices.
        out.push(Line::styled("Paired devices", head));
        if self.devices().is_empty() {
            out.push(Line::styled("  none yet: p pairs one", theme::muted()));
        }
        for (i, d) in self.devices().iter().enumerate() {
            let seen = match d["last_seen"].as_i64() {
                Some(t) => format!("seen {} ago", rel_age(Some(t * 1000))),
                None => "never seen".into(),
            };
            let mut line = Line::from(vec![
                Span::raw(if d["linked"] == true { " ● " } else { "   " }),
                Span::styled(
                    format!("{:<24}", d["name"].as_str().unwrap_or("")),
                    Style::default().fg(theme::text()).bold(),
                ),
                Span::styled(
                    format!("{:<18}", d["id"].as_str().unwrap_or("")),
                    theme::muted(),
                ),
                Span::styled(seen, theme::muted()),
            ]);
            if d["input"] == false {
                line.push_span(Span::styled(
                    "  view-only",
                    Style::default().fg(theme::yellow()),
                ));
            }
            if i == self.sel {
                line = line.style(theme::selected());
            }
            out.push(line);
        }
        out.push(Line::from(""));
        if let Some((_, name)) = &self.revoking {
            out.push(Line::styled(
                format!("unpair \"{name}\"? It has to pair again to come back.  y / n"),
                Style::default().fg(theme::yellow()),
            ));
        } else if let Some(m) = &self.msg {
            out.push(Line::styled(m.clone(), theme::muted()));
        }
        let mut keys = vec!["p pair a device"];
        if self.offer.is_some() {
            keys.push("x cancel the code");
        }
        if !self.devices().is_empty() {
            keys.push(
                if self
                    .devices()
                    .get(self.sel)
                    .is_some_and(|d| d["input"] == false)
                {
                    "v let it type"
                } else {
                    "v view-only"
                },
            );
            keys.push("d unpair");
        }
        keys.push("esc close");
        out.push(Line::styled(keys.join(" · "), theme::muted()));
        out
    }
}

impl App {
    /// `:relay`: open the Devices screen.
    pub(crate) fn open_devices(&mut self) {
        let st = match relay_call("relay.status", json!({})) {
            Ok(st) => st,
            Err(e) => {
                self.status_line = format!("devices: {e:#}");
                return;
            }
        };
        self.mode = Mode::Devices(Box::new(Devices::new(st)));
    }

    /// The open Devices screen, after a change: its status again.
    pub(crate) fn refresh_devices(&mut self) {
        if !matches!(self.mode, Mode::Devices(_)) {
            return;
        }
        let st = relay_call("relay.status", json!({}));
        if let Mode::Devices(s) = &mut self.mode {
            match st {
                Ok(st) => s.set_status(st),
                Err(e) => s.say(format!("{e:#}")),
            }
        }
    }

    pub(crate) fn handle_devices(&mut self, k: KeyEvent, mut s: Box<Devices>) {
        let (method, params) = match s.key(k) {
            None => {
                self.mode = Mode::Devices(s);
                return;
            }
            Some(Act::Close) => return,
            Some(Act::Pair) => ("relay.pair", json!({})),
            Some(Act::CancelPair) => ("relay.pair_cancel", json!({})),
            Some(Act::Confirm) => ("relay.confirm", json!({})),
            Some(Act::Reject) => ("relay.reject", json!({})),
            Some(Act::Revoke(id)) => ("relay.revoke", json!({"device": id})),
            Some(Act::Input(id, on)) => ("relay.input", json!({"device": id, "input": on})),
        };
        let typing = params["input"] == true;
        match relay_call(method, params) {
            Ok(v) => match method {
                "relay.pair" => s.set_offer(&v),
                "relay.pair_cancel" => {
                    s.offer = None;
                    s.say("the code is cancelled");
                }
                "relay.confirm" => s.say(format!(
                    "paired \"{}\"",
                    v["device"]["name"].as_str().unwrap_or("?")
                )),
                "relay.reject" => s.say("turned the device away"),
                "relay.input" if typing => s.say("it may type into sessions again"),
                "relay.input" => {
                    s.say("view-only: it watches sessions, and can't type or take one over")
                }
                _ => s.say("unpaired"),
            },
            Err(e) => s.say(format!("{e:#}")),
        }
        if let Ok(st) = relay_call("relay.status", json!({})) {
            s.set_status(st);
        }
        self.mode = Mode::Devices(s);
    }

    pub(crate) fn draw_devices(&self, f: &mut Frame, s: &Devices) {
        let area = newform::centered(f.area(), 90, f.area().height.saturating_sub(2));
        f.render_widget(Clear, area);
        self.hits.add(f.area(), mouse::Hit::Outside);
        self.hits.add(area, mouse::Hit::Inside);
        let block = theme::modal("Devices");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let lines = s.lines();
        let code = s.offer.as_ref().map(|o| o.0.as_slice()).unwrap_or(&[]);
        let code_h = if code.is_empty() {
            0
        } else {
            code.len() as u16 + 2
        };
        let fits = code_h + lines.len() as u16 <= inner.height
            && code.first().map_or(0, |l| l.chars().count()) as u16 <= inner.width;
        let [code_area, text_area] = if code.is_empty() || !fits {
            [Rect::default(), inner]
        } else {
            RLayout::vertical([Constraint::Length(code_h), Constraint::Min(0)]).areas(inner)
        };
        if !code.is_empty() && fits {
            // Dark modules on light, whatever the theme: what a camera
            // reads.
            let ink = Style::default().fg(Color::Black).bg(Color::White);
            let w = code[0].chars().count() as u16;
            let x = code_area.x + code_area.width.saturating_sub(w) / 2;
            let qr: Vec<Line> = code.iter().map(|l| Line::styled(l.clone(), ink)).collect();
            let h = qr.len() as u16;
            f.render_widget(
                Paragraph::new(qr),
                Rect {
                    x,
                    y: code_area.y,
                    width: w.min(code_area.width),
                    height: h.min(code_area.height),
                },
            );
            let ends = s.offer.as_ref().and_then(|o| o.1).map(|e| {
                let left = (e - rlord_core::types::now_ms() / 1000).max(0);
                format!(
                    "scan it on the device you are pairing: one device, {}m {:02}s left",
                    left / 60,
                    left % 60
                )
            });
            if let Some(t) = ends {
                f.render_widget(
                    Paragraph::new(Line::styled(t, theme::muted()).centered()),
                    Rect {
                        y: code_area.y + h,
                        height: 1,
                        ..code_area
                    },
                );
            }
        }
        let mut lines = lines;
        if !code.is_empty() && !fits {
            lines.insert(
                0,
                Line::styled(
                    "the window is too small for the code: make it bigger, or run \
                     `rlord relay pair --text` in a shell",
                    Style::default().fg(theme::yellow()),
                ),
            );
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), text_area);
    }
}

/// One `relay.*` call to the daemon.
fn relay_call(method: &str, params: Value) -> Result<Value> {
    client_call(&paths::socket_path(), method, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn status(pending: Value, pairing: Value) -> Value {
        json!({
            "enabled": true, "relay": "https://relay.example.com", "relay_key": "k",
            "plugin": {"installed": true, "state": "running", "problem": null},
            "reported": true, "connected": true, "error": null,
            "devices": [
                {"id": "ab12cd34ef560789", "name": "phone", "paired_at": 1, "last_seen": null,
                 "linked": true},
                {"id": "ab99000011112222", "name": "tablet", "paired_at": 1, "last_seen": 2,
                 "linked": false},
            ],
            "pairing": pairing, "pending": pending,
        })
    }

    fn text(s: &Devices) -> String {
        s.lines()
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|x| x.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_keys() {
        let mut s = Devices::new(status(Value::Null, Value::Null));
        assert_eq!(s.key(key(KeyCode::Char('p'))), Some(Act::Pair));
        // Nothing waiting, no offer: y, n and x do nothing.
        for c in ['y', 'n', 'x'] {
            assert_eq!(s.key(key(KeyCode::Char(c))), None, "{c}");
        }
        // Unpair asks first; anything but y keeps it.
        s.key(key(KeyCode::Down));
        assert_eq!(s.key(key(KeyCode::Char('d'))), None);
        assert!(text(&s).contains("unpair \"tablet\"?"), "{}", text(&s));
        assert_eq!(s.key(key(KeyCode::Char('n'))), None);
        assert!(text(&s).contains("kept"));
        s.key(key(KeyCode::Char('d')));
        assert_eq!(
            s.key(key(KeyCode::Char('y'))),
            Some(Act::Revoke("ab99000011112222".into()))
        );
        assert_eq!(s.key(key(KeyCode::Esc)), Some(Act::Close));
    }

    /// `v` makes the selected device view-only, or lets it type again; the
    /// list says which are view-only.
    #[test]
    fn a_device_can_be_made_view_only() {
        let mut st = status(Value::Null, Value::Null);
        st["devices"][1]["input"] = json!(false);
        let mut s = Devices::new(st);
        assert!(!text(&s)
            .lines()
            .any(|l| l.contains("phone") && l.contains("view-only")));
        assert!(text(&s).contains("v view-only"), "{}", text(&s));
        assert_eq!(
            s.key(key(KeyCode::Char('v'))),
            Some(Act::Input("ab12cd34ef560789".into(), false))
        );
        s.key(key(KeyCode::Down));
        assert!(text(&s)
            .lines()
            .any(|l| l.contains("tablet") && l.contains("view-only")));
        assert!(text(&s).contains("v let it type"), "{}", text(&s));
        assert_eq!(
            s.key(key(KeyCode::Char('v'))),
            Some(Act::Input("ab99000011112222".into(), true))
        );
    }

    #[test]
    fn a_waiting_device_shows_its_code_and_takes_y_or_n() {
        let pending = json!({"name": "laptop", "code": "123456", "expires": 9});
        let mut s = Devices::new(status(Value::Null, json!({"expires": 9})));
        s.set_offer(&json!({"qr": "RLORD1:AQIDBAUGBwgJCgsMDQ4PEA", "expires": 9}));
        assert!(s.offer.is_some());
        assert_eq!(s.key(key(KeyCode::Char('x'))), Some(Act::CancelPair));
        // The device came: the code goes, its name and code show.
        s.set_status(status(pending.clone(), json!({"expires": 9})));
        assert!(s.offer.is_none());
        let t = text(&s);
        assert!(t.contains("\"laptop\" wants to pair, code 123 456"), "{t}");
        assert_eq!(s.key(key(KeyCode::Char('y'))), Some(Act::Confirm));
        assert_eq!(s.key(key(KeyCode::Char('n'))), Some(Act::Reject));
        // An offer the plugin no longer has: gone too (once the plugin
        // has had a moment to report it).
        let mut s = Devices::new(status(Value::Null, json!({"expires": 9})));
        s.set_offer(&json!({"qr": "RLORD1:x", "expires": 9}));
        s.set_status(status(Value::Null, Value::Null));
        assert!(s.offer.is_some(), "a report older than the offer");
        if let Some(o) = s.offer.as_mut() {
            o.2 = Instant::now() - OFFER_GRACE;
        }
        s.set_status(status(Value::Null, Value::Null));
        assert!(s.offer.is_none());
    }

    fn drawn(s: Devices, w: u16, h: u16) -> String {
        let (_tx, rx) = mpsc::channel();
        let keys = crate::KeyMap::from_config(&rlord_core::config::Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.mode = Mode::Devices(Box::new(s));
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_code_is_drawn_when_it_fits() {
        let mut s = Devices::new(status(Value::Null, json!({"expires": 9})));
        s.set_offer(&json!({"qr": "RLORD1:AQIDBAUGBwgJCgsMDQ4PEA", "expires": 9}));
        let big = drawn(s, 120, 60);
        assert!(
            big.contains('█') && big.contains("scan it on the device"),
            "{big}"
        );
        assert!(
            big.contains("phone") && big.contains("x cancel the code"),
            "{big}"
        );
        let mut s = Devices::new(status(Value::Null, json!({"expires": 9})));
        s.set_offer(&json!({"qr": "RLORD1:AQIDBAUGBwgJCgsMDQ4PEA", "expires": 9}));
        let small = drawn(s, 80, 20);
        assert!(small.contains("too small for the code"), "{small}");
    }

    #[test]
    fn it_says_what_is_missing() {
        let s = Devices::new(json!({"enabled": false, "relay": "", "devices": []}));
        let t = text(&s);
        assert!(t.contains("rlord relay setup"), "{t}");
        assert!(t.contains("none yet"), "{t}");
    }
}
