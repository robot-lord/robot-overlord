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

/// Groups over `sessions`: (tag, count), most sessions first, then
/// "untagged" last when there are any.
pub(crate) fn group_list(sessions: &[Value]) -> Vec<(String, usize)> {
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
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
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
        self.groups = group_list(&self.all);
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
        self.refresh_preview();
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
            KeyCode::Right if self.group_focus => {
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
            Some(Action::Rename) => match self.group_sel.clone() {
                Some(g) if g != UNTAGGED => self.mode = Mode::RenameGroup(g),
                _ => self.status_line = "untagged isn't a group you can rename".into(),
            },
            _ => return false,
        }
        true
    }

    /// Tab bar + filter bar (two lines over the whole screen).
    pub(crate) fn draw_bars(&self, f: &mut Frame, area: Rect) {
        let tab = |t: Tab, count: String| {
            let label = format!(" {} {count} ", t.label());
            if self.tab == t {
                Span::styled(
                    label,
                    Style::default().fg(Color::Black).bg(Color::Cyan).bold(),
                )
            } else {
                Span::styled(label, Style::default().fg(Color::Gray))
            }
        };
        let n_sessions = self.all.len();
        let tabs = Line::from(vec![
            tab(Tab::Sessions, n_sessions.to_string()),
            Span::raw(" "),
            tab(Tab::Groups, self.groups_count().to_string()),
            Span::styled(
                format!("   {} switch", self.keys.label(Action::SwitchTab)),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        let dim = Style::default().fg(Color::DarkGray);
        let filter = if self.filter.trim().is_empty() {
            Span::styled("none", dim)
        } else {
            Span::styled(
                self.filter.clone(),
                Style::default().fg(Color::Yellow).bold(),
            )
        };
        let mut bar = vec![
            Span::raw(" filter: "),
            filter,
            Span::raw("   sort: "),
            Span::styled(
                format!(
                    "{}{}",
                    self.sort.label(),
                    if self.reverse { " ↑" } else { "" }
                ),
                Style::default().bold(),
            ),
            Span::raw("   showing: "),
            Span::styled(
                if self.show_archived {
                    "archived"
                } else {
                    "live"
                },
                if self.show_archived {
                    Style::default().fg(Color::Yellow).bold()
                } else {
                    Style::default().bold()
                },
            ),
        ];
        let mut hint = format!("   {} search", self.keys.label(Action::Filter));
        if !self.filter.is_empty() {
            hint.push_str(&format!(" · {} clear", self.keys.label(Action::Cancel)));
        }
        hint.push_str(&format!(
            " · {} sort · {} archived",
            self.keys.label(Action::Sort),
            self.keys.label(Action::ToggleArchived)
        ));
        bar.push(Span::styled(hint, dim));
        f.render_widget(Paragraph::new(vec![tabs, Line::from(bar)]), area);
    }

    fn groups_count(&self) -> usize {
        group_list(&self.all)
            .iter()
            .filter(|(t, _)| t != UNTAGGED)
            .count()
    }

    /// The groups tab: group column + the selected group's sessions.
    pub(crate) fn draw_groups(&mut self, f: &mut Frame, area: Rect) {
        let col_w = self
            .groups
            .iter()
            .map(|(t, _)| group_label(t).chars().count() + 6)
            .max()
            .unwrap_or(12)
            .clamp(16, 28) as u16;
        let [left, right] =
            RLayout::horizontal([Constraint::Length(col_w), Constraint::Min(20)]).areas(area);
        let focus = Style::default().fg(Color::Cyan);
        let items: Vec<ListItem> = self
            .groups
            .iter()
            .map(|(t, n)| {
                let label = group_label(t);
                let pad = (col_w as usize).saturating_sub(label.chars().count() + 7);
                let style = if t == UNTAGGED {
                    Style::default().fg(Color::DarkGray).italic()
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(label, style),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(format!("{n:>2}"), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let mut block = Block::default().borders(Borders::ALL).title(" groups ");
        if self.group_focus {
            block = block.border_style(focus);
        }
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
                .highlight_style(
                    Style::default()
                        .bg(if self.group_focus {
                            Color::DarkGray
                        } else {
                            Color::Reset
                        })
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol(if self.group_focus { "▶" } else { "›" });
            f.render_stateful_widget(list, left, &mut self.group_state);
        }
        self.draw_list(f, right);
    }

    /// Title for the session table on either tab.
    pub(crate) fn list_title(&self) -> String {
        match self.tab {
            Tab::Sessions => format!(
                " {} · {} ",
                if self.show_archived {
                    "archived sessions"
                } else {
                    "all sessions"
                },
                self.all.len()
            ),
            Tab::Groups => match &self.group_sel {
                Some(g) => format!(
                    " {} · {}{} ",
                    group_label(g),
                    self.sessions.len(),
                    if self.group_focus {
                        "  (→ to open)"
                    } else {
                        ""
                    }
                ),
                None => " no sessions ".into(),
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
        let g = group_list(&all);
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
    }
}
