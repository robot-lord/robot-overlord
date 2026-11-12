//! The list's two tabs and the always-visible filter bar.
//!
//! - **Sessions** — the flat list.
//! - **Groups** — tags are the groups. A column of groups (each tag with its
//!   session count, then "untagged") beside the sessions in the selected
//!   one. A session with several tags is in several groups. `←`/`→` move
//!   between the column and the sessions; nothing is hidden or nested.
//!
//! Both tabs show the same filter and sort, spelled out on the filter bar.

use super::*;
use ratatui::widgets::{List, ListItem};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tab {
    Sessions,
    Groups,
}

impl Tab {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Tab::Sessions => "sessions",
            Tab::Groups => "groups",
        }
    }
}

/// The "untagged" pseudo-group (tags are never empty).
pub(crate) const UNTAGGED: &str = "";

fn tags_of(s: &Value) -> Vec<&str> {
    s["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Groups over `sessions`: (tag, count). The user's favourites first, in
/// their order (`order`), then the rest, most sessions first, then
/// "untagged" last when there are any. Favourites with no sessions here
/// aren't shown (they keep their place for when they have some).
pub(crate) fn group_list(sessions: &[Value], order: &[String]) -> Vec<(String, usize)> {
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    let mut untagged = 0;
    for s in sessions {
        let tags = tags_of(s);
        if tags.is_empty() {
            untagged += 1;
        }
        for t in tags {
            *counts.entry(t).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(t, n)| (t.to_string(), n))
        .collect();
    let rank = |t: &str| order.iter().position(|o| o == t).unwrap_or(usize::MAX);
    v.sort_by(|a, b| {
        rank(&a.0)
            .cmp(&rank(&b.0))
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    if untagged > 0 {
        v.push((UNTAGGED.to_string(), untagged));
    }
    v
}

pub(crate) fn in_group(s: &Value, group: &str) -> bool {
    let tags = tags_of(s);
    if group == UNTAGGED {
        tags.is_empty()
    } else {
        tags.contains(&group)
    }
}

pub(crate) fn group_label(g: &str) -> String {
    if g == UNTAGGED {
        "untagged".into()
    } else {
        format!("#{g}")
    }
}

impl App {
    /// Rebuild the groups column from `all` and show the selected group's
    /// sessions (keeping the group selected by name across refreshes).
    pub(crate) fn regroup(&mut self) {
        self.groups = group_list(&self.all, &self.group_order);
        let idx = self
            .group_sel
            .as_ref()
            .and_then(|g| self.groups.iter().position(|(t, _)| t == g))
            .unwrap_or(0)
            .min(self.groups.len().saturating_sub(1));
        self.group_state.select(if self.groups.is_empty() {
            None
        } else {
            Some(idx)
        });
        self.group_sel = self.groups.get(idx).map(|(t, _)| t.clone());
        self.sessions = match &self.group_sel {
            Some(g) => self
                .all
                .iter()
                .filter(|s| in_group(s, g))
                .cloned()
                .collect(),
            None => vec![],
        };
    }

    pub(crate) fn switch_tab(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            self.group_focus = tab == Tab::Groups;
            self.table.select(Some(0));
            self.dirty = true;
        }
    }

    pub(crate) fn is_favourite(&self, g: &str) -> bool {
        self.group_order.iter().any(|o| o == g)
    }

    /// `p` on the group list: add the highlighted group to the favourites
    /// (at the end) or take it out.
    fn toggle_favourite(&mut self) {
        let Some(g) = self.group_sel.clone().filter(|g| g != UNTAGGED) else {
            self.status_line = "untagged can't be a favourite".into();
            return;
        };
        let mut order = self.group_order.clone();
        if self.is_favourite(&g) {
            order.retain(|o| *o != g);
            self.status_line = format!("#{g} is no longer a favourite");
        } else {
            order.push(g.clone());
            self.status_line = format!("#{g} is a favourite (J/K to reorder)");
        }
        self.save_group_order(order);
    }

    /// `K`/`J` on a favourite: move it up or down among the favourites.
    fn move_favourite(&mut self, delta: isize) {
        let Some(g) = self.group_sel.clone() else {
            return;
        };
        let mut order = self.group_order.clone();
        let Some(i) = order.iter().position(|o| *o == g) else {
            self.status_line = format!(
                "{} first makes it a favourite",
                self.keys.label(Action::Pin)
            );
            return;
        };
        let j = (i as isize + delta).clamp(0, order.len() as isize - 1) as usize;
        order.swap(i, j);
        self.save_group_order(order);
    }

    fn save_group_order(&mut self, order: Vec<String>) {
        if let Some(v) = self.call("tag.order", json!({"order": order})) {
            self.group_order = v["order"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
        }
        self.regroup();
    }

    /// Move the selected group by `delta` (clamped).
    pub(crate) fn move_group(&mut self, delta: isize) {
        if self.groups.is_empty() {
            return;
        }
        let i = self.group_state.selected().unwrap_or(0) as isize + delta;
        let i = i.clamp(0, self.groups.len() as isize - 1) as usize;
        self.group_sel = Some(self.groups[i].0.clone());
        self.regroup();
        self.table.select(if self.sessions.is_empty() {
            None
        } else {
            Some(0)
        });
        self.preview_soon();
    }

    /// Keys that mean something different on the groups tab. Returns true
    /// when the key was handled here.
    pub(crate) fn handle_groups_key(&mut self, k: &KeyEvent, action: Option<Action>) -> bool {
        if self.tab != Tab::Groups {
            return false;
        }
        match k.code {
            KeyCode::Left => {
                self.group_focus = true;
                return true;
            }
            // `l` mirrors `h`: into the highlighted group (on the group list
            // it means that, not the log toggle it is elsewhere).
            KeyCode::Right | KeyCode::Char('l') if self.group_focus => {
                self.group_focus = self.sessions.is_empty();
                return true;
            }
            // Inside a group: esc, h or ← go back to the group list. (`h`
            // only while it isn't bound to something else.)
            KeyCode::Char('h') if !self.group_focus && action.is_none() => {
                self.group_focus = true;
                return true;
            }
            _ if !self.group_focus && action == Some(Action::Cancel) => {
                self.group_focus = true;
                return true;
            }
            _ => {}
        }
        if !self.group_focus {
            return false;
        }
        match action {
            Some(Action::Down) => self.move_group(1),
            Some(Action::Up) => self.move_group(-1),
            Some(Action::Top) => self.move_group(-(self.groups.len() as isize)),
            Some(Action::Bottom) => self.move_group(self.groups.len() as isize),
            // Enter opens the group: focus moves to its sessions.
            Some(Action::Attach) => self.group_focus = self.sessions.is_empty(),
            Some(Action::Pin) => self.toggle_favourite(),
            None if matches!(k.code, KeyCode::Char('K') | KeyCode::Char('J')) => {
                self.move_favourite(if k.code == KeyCode::Char('K') { -1 } else { 1 })
            }
            Some(Action::Rename) => match self.group_sel.clone() {
                Some(g) if g != UNTAGGED => self.mode = Mode::RenameGroup(g),
                _ => self.status_line = "untagged isn't a group you can rename".into(),
            },
            _ => return false,
        }
        true
    }

    /// Header: the logo and the name beside it, then one toolbar under the
    /// name: tabs │ search │ sort │ live/archived, each with its key.
    pub(crate) fn draw_bars(&self, f: &mut Frame, area: Rect) {
        let tab = |t: Tab| -> Span<'static> {
            let active = self.tab == t;
            Span::styled(
                format!(" {} ", t.label()),
                if active {
                    Style::default()
                        .fg(theme::accent())
                        .bg(theme::chip())
                        .bold()
                } else {
                    theme::muted()
                },
            )
        };
        // Small, quiet keycaps: the key that changes each setting.
        let key = |a: Action| {
            Span::styled(
                format!(" {} ", self.keys.label(a)),
                Style::default().fg(theme::muted_c()).bg(theme::chip()),
            )
        };
        let sep = || Span::styled("  │  ", Style::default().fg(theme::border()));
        // The logo: a stern robot (dark red, heavy outline) with a crown
        // (dark gold) set into its head, four rows tall.
        let (crown, robot, eye) = theme::logo();
        let red = Style::default().fg(robot);
        let eyes = Style::default().fg(eye);
        let logo = [
            vec![
                Span::styled(" ┏", red),
                Span::styled("▲▲▲", Style::default().fg(crown)),
                Span::styled("┓", red),
            ],
            vec![
                Span::styled(" ┃", red),
                Span::styled("▾ ▾", eyes),
                Span::styled("┃", red),
            ],
            vec![
                Span::styled(" ┃ ", red),
                Span::styled("▬", eyes),
                Span::styled(" ┃", red),
            ],
            vec![Span::styled(" ┗━━━┛", red)],
        ];

        let mut bar = vec![tab(Tab::Sessions), tab(Tab::Groups), Span::raw(" ")];
        bar.push(key(Action::SwitchTab));
        bar.push(sep());
        bar.push(Span::styled("⌕ ", theme::muted()));
        if self.filter.trim().is_empty() {
            bar.push(Span::styled("search ", theme::faint()));
        } else {
            bar.push(Span::styled(
                format!("{} ", self.filter),
                Style::default().fg(theme::yellow()).bold(),
            ));
            bar.push(key(Action::Cancel));
            bar.push(Span::styled(" clear ", theme::faint()));
        }
        bar.push(key(Action::Filter));
        bar.push(sep());
        bar.push(Span::styled("⇅ ", theme::muted()));
        bar.push(Span::styled(
            format!(
                "{}{} ",
                self.sort.label(),
                if self.reverse { " ↑" } else { "" }
            ),
            Style::default().fg(theme::text()),
        ));
        bar.push(key(Action::Sort));
        bar.push(sep());
        bar.push(Span::styled("◷ ", theme::muted()));
        bar.push(if self.show_archived {
            Span::styled("archived ", Style::default().fg(theme::yellow()).bold())
        } else {
            Span::styled("live ", Style::default().fg(theme::text()))
        });
        bar.push(key(Action::ToggleArchived));

        // The name: ROBOT in the robot's red, OVERLORD in the crown's gold.
        // Box-drawn letters three rows tall where they fit, else one
        // spaced line.
        const BIG: [(&str, &str); 3] = [
            ("╦═╗╔═╗╔╗ ╔═╗╔╦╗", "  ╔═╗╦  ╦╔═╗╦═╗╦  ╔═╗╦═╗╔╦╗"),
            ("╠╦╝║ ║╠╩╗║ ║ ║ ", "  ║ ║╚╗╔╝║╣ ╠╦╝║  ║ ║╠╦╝ ║║"),
            ("╩╚═╚═╝╚═╝╚═╝ ╩ ", "  ╚═╝ ╚╝ ╚═╝╩╚═╩═╝╚═╝╩╚══╩╝"),
        ];
        let robot_s = Style::default().fg(robot).bold();
        let crown_s = Style::default().fg(crown).bold();
        let gap = || Span::raw("  ");
        let beside = |mut l: Vec<Span<'static>>, rest: Vec<Span<'static>>| {
            l.push(gap());
            l.extend(rest);
            Line::from(l)
        };
        let [l0, l1, l2, l3] = logo;
        let lines = if area.width >= 52 {
            let big = |(a, b): (&str, &str)| {
                vec![
                    Span::styled(a.to_string(), robot_s),
                    Span::styled(b.to_string(), crown_s),
                ]
            };
            vec![
                beside(l0, big(BIG[0])),
                beside(l1, big(BIG[1])),
                beside(l2, big(BIG[2])),
                beside(l3, bar),
            ]
        } else {
            vec![
                Line::from(l0),
                beside(
                    l1,
                    vec![
                        Span::styled("R O B O T", robot_s),
                        Span::raw("   "),
                        Span::styled("O V E R L O R D", crown_s),
                    ],
                ),
                beside(l2, bar),
                Line::from(l3),
            ]
        };
        f.render_widget(Paragraph::new(lines), area);
    }

    /// The groups tab: group column + the selected group's sessions.
    pub(crate) fn draw_groups(&mut self, f: &mut Frame, area: Rect) {
        let col_w = self
            .groups
            .iter()
            // label + space + 2-digit count + borders, padding and the
            // selection bar (2 + 2 + 2).
            .map(|(t, _)| group_label(t).chars().count() + 11)
            .max()
            .unwrap_or(12)
            .clamp(18, 30) as u16;
        let [left, right] =
            RLayout::horizontal([Constraint::Length(col_w), Constraint::Min(20)]).areas(area);
        let items: Vec<ListItem> = self
            .groups
            .iter()
            .map(|(t, n)| {
                let label = group_label(t);
                let pad = (col_w as usize)
                    .saturating_sub(label.chars().count() + 8)
                    .max(1);
                let style = if t == UNTAGGED {
                    theme::faint().italic()
                } else {
                    Style::default().fg(theme::tag_color(t))
                };
                let fav = self.group_order.iter().any(|o| o == t);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if fav { "★ " } else { "  " },
                        Style::default().fg(theme::yellow()),
                    ),
                    Span::styled(label, style),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(format!("{n:>2}"), theme::faint()),
                ]))
            })
            .collect();
        let block = theme::panel(theme::title("Groups"), self.group_focus);
        if items.is_empty() {
            let msg = vec![
                Line::from(""),
                Line::from(" no groups yet").dim(),
                Line::from(""),
                Line::from(format!(" {} tags a session", self.keys.label(Action::Tags))).dim(),
                Line::from(" or ask the overlord").dim(),
                Line::from(" to organize them").dim(),
            ];
            f.render_widget(Paragraph::new(msg).block(block), left);
        } else {
            let list = List::new(items)
                .block(block)
                .highlight_style(if self.group_focus {
                    theme::selected()
                } else {
                    Style::default().bold()
                })
                .highlight_symbol(if self.group_focus {
                    theme::SELECT_BAR
                } else {
                    "› "
                })
                .highlight_spacing(ratatui::widgets::HighlightSpacing::Always);
            f.render_stateful_widget(list, left, &mut self.group_state);
        }
        self.draw_list(f, right);
    }

    /// Title for the session table on either tab.
    pub(crate) fn list_title(&self) -> String {
        match self.tab {
            Tab::Sessions => format!(
                " {}  {} ",
                if self.show_archived {
                    "Archived"
                } else {
                    "Sessions"
                },
                self.all.len()
            ),
            Tab::Groups => match &self.group_sel {
                Some(g) => format!(" {}  {} ", group_label(g), self.sessions.len()),
                None => " No sessions ".into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_count_tags_and_untagged() {
        let s = |tags: &[&str]| json!({"tags": tags});
        let all = vec![s(&["web", "wip"]), s(&["web"]), s(&[]), s(&["api"])];
        let g = group_list(&all, &[]);
        assert_eq!(
            g,
            vec![
                ("web".to_string(), 2),
                ("api".to_string(), 1),
                ("wip".to_string(), 1),
                (UNTAGGED.to_string(), 1),
            ]
        );
        // A session with two tags is in both groups.
        assert!(in_group(&all[0], "web") && in_group(&all[0], "wip"));
        assert!(in_group(&all[2], UNTAGGED) && !in_group(&all[1], UNTAGGED));
    }

    #[test]
    fn favourites_lead_in_their_order() {
        let s = |tags: &[&str]| json!({"tags": tags});
        let all = vec![s(&["web", "wip"]), s(&["web"]), s(&[]), s(&["api"])];
        // "gone" has no sessions: not shown, but keeps its place.
        let order = ["wip".to_string(), "gone".to_string(), "api".to_string()];
        let names: Vec<String> = group_list(&all, &order).into_iter().map(|g| g.0).collect();
        assert_eq!(names, ["wip", "api", "web", UNTAGGED]);
    }
}

#[cfg(test)]
mod back_tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn esc_h_and_left_leave_a_group() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let keys = KeyMap::from_config(&Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.tab = Tab::Groups;
        for code in [KeyCode::Esc, KeyCode::Char('h'), KeyCode::Left] {
            app.group_focus = false;
            let k = KeyEvent::new(code, KeyModifiers::NONE);
            let action = app.keys.action(&k);
            assert!(app.handle_groups_key(&k, action), "{code:?}");
            assert!(app.group_focus, "{code:?} should return to the group list");
        }
        // H / L step along sessions tab ⇄ group list ⇄ inside the group.
        app.sessions = vec![json!({"name": "a"})];
        app.tab = Tab::Sessions;
        app.tab_step(1);
        assert!(
            app.tab == Tab::Groups && app.group_focus,
            "L: to the group list"
        );
        app.sessions = vec![json!({"name": "a"})];
        app.tab_step(1);
        assert!(!app.group_focus, "L again: into the group");
        app.tab_step(-1);
        assert!(
            app.tab == Tab::Groups && app.group_focus,
            "H: back to the list"
        );
        app.tab_step(-1);
        assert_eq!(app.tab, Tab::Sessions, "H again: the sessions tab");
        // `l` (and →) go into the highlighted group when it has sessions.
        app.tab = Tab::Groups;
        app.sessions = vec![json!({"name": "a"})];
        for code in [KeyCode::Char('l'), KeyCode::Right] {
            app.group_focus = true;
            let k = KeyEvent::new(code, KeyModifiers::NONE);
            let action = app.keys.action(&k);
            assert!(app.handle_groups_key(&k, action), "{code:?}");
            assert!(!app.group_focus, "{code:?} should enter the group");
        }
    }
}
