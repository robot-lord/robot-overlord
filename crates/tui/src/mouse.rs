//! The mouse: everything a key does, a click can do.
//!
//! Drawing records what is where: each frame, the draw functions add a hit
//! region (`Rect` + `Hit`) for every row, tab, keycap, field and button
//! they put on screen. A mouse event looks up the topmost region under the
//! pointer and is turned into what the keyboard would have done there: most
//! regions are simply "press this key" (`Hit::Key`), so a click runs the
//! same handler the key does and keyboard behaviour stays the one
//! definition. Regions added later lie on top; a modal first adds
//! `Outside` over the whole screen, so nothing under it can be hit.

use std::cell::RefCell;

use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::widgets::Clear;
use unicode_width::UnicodeWidthStr;

use super::*;

/// What a spot on screen does.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Hit {
    /// Press this key in the current mode: a footer or dialog keycap, a
    /// toolbar setting.
    Key(KeyEvent),
    Tab(Tab),
    /// A session row / a group, by index.
    Row(usize),
    Group(usize),
    /// A sortable column header: sort by it, or flip the direction if the
    /// list is already sorted by it.
    SortBy(rlord_core::db::SortMode),
    /// One of the overlord's earlier conversations in the details pane
    /// (1: the newest): show it in the preview.
    Conv(usize),
    /// Wheel areas: the session table, the group column, the preview.
    List,
    Groups,
    Preview,
    /// Item `i` of the open modal: a form field, a picker or launcher
    /// item, a key-screen row, a menu entry.
    Pick(usize),
    /// A modal's body, and everything around it.
    Inside,
    Outside,
}

/// Two clicks on the same thing within this are a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// The frame's hit regions, in drawing order (later ones on top).
#[derive(Default)]
pub(crate) struct Hits(RefCell<Vec<(Rect, Hit)>>);

impl Hits {
    pub(crate) fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    pub(crate) fn add(&self, area: Rect, hit: Hit) {
        if area.width > 0 && area.height > 0 {
            self.0.borrow_mut().push((area, hit));
        }
    }

    /// One row of a list: `Pick(i)` across `area`'s width at line `y`.
    pub(crate) fn row(&self, area: Rect, y: u16, hit: Hit) {
        if y >= area.y && y < area.bottom() {
            self.add(
                Rect {
                    y,
                    height: 1,
                    ..area
                },
                hit,
            );
        }
    }

    /// Spans laid out from `(x, y)`: each (width, what it does).
    pub(crate) fn spans(
        &self,
        x: u16,
        y: u16,
        parts: impl IntoIterator<Item = (u16, Option<Hit>)>,
    ) {
        let mut at = x;
        for (w, hit) in parts {
            if let Some(h) = hit {
                self.add(Rect::new(at, y, w, 1), h);
            }
            at = at.saturating_add(w);
        }
    }

    /// The keycaps in a dialog's lines (drawn from `(x, y)`, one line per
    /// row): each keycap and its label press that key. Hints that aren't
    /// one key ("↑↓") do nothing.
    pub(crate) fn keycaps(&self, x: u16, y: u16, lines: &[Line]) {
        for (i, line) in lines.iter().enumerate() {
            let mut parts = Vec::new();
            let mut spans = line.spans.iter().peekable();
            while let Some(s) = spans.next() {
                let w = s.width() as u16;
                if !is_keycap(s) {
                    parts.push((w, None));
                    continue;
                }
                let hit = key_from_hint(s.content.trim()).map(Hit::Key);
                parts.push((w, hit.clone()));
                // Its label, minus the gap after it.
                if let Some(label) = spans.next() {
                    let lw = label.width() as u16;
                    let text = label.content.trim_end().width() as u16;
                    parts.push((text, hit));
                    parts.push((lw - text, None));
                }
            }
            self.spans(x, y.saturating_add(i as u16), parts);
        }
    }

    /// The topmost region under the pointer.
    pub(crate) fn at(&self, x: u16, y: u16) -> Option<Hit> {
        hit_at(&self.0.borrow(), x, y, |_| true).cloned()
    }

    /// What the wheel turns under the pointer: a modal if one is open
    /// (`Inside`/`Outside`), else the list, group column or preview.
    pub(crate) fn wheel_at(&self, x: u16, y: u16) -> Option<Hit> {
        hit_at(&self.0.borrow(), x, y, |h| {
            matches!(
                h,
                Hit::List | Hit::Groups | Hit::Preview | Hit::Inside | Hit::Outside
            )
        })
        .cloned()
    }
}

/// The last-added region containing `(x, y)` that `want` accepts.
pub(crate) fn hit_at(
    regions: &[(Rect, Hit)],
    x: u16,
    y: u16,
    want: impl Fn(&Hit) -> bool,
) -> Option<&Hit> {
    regions
        .iter()
        .rev()
        .find(|(r, h)| r.contains((x, y).into()) && want(h))
        .map(|(_, h)| h)
}

/// A span drawn by `keycap`: bold, on the chip or accent background.
fn is_keycap(s: &Span) -> bool {
    s.style.add_modifier.contains(Modifier::BOLD)
        && (s.style.bg == Some(theme::chip()) || s.style.bg == Some(theme::accent()))
        && s.content.starts_with(' ')
        && s.content.ends_with(' ')
}

/// The key a dialog hint names: `enter`, `esc`, `tab`, `shift-enter`, `^E`,
/// a character. Of alternatives (`y/enter`, `←/→`) the last; "any key" is
/// esc. None for hints that aren't a key press.
pub(crate) fn key_from_hint(label: &str) -> Option<KeyEvent> {
    let one = |l: &str| -> Option<KeyEvent> {
        let plain = |code| Some(KeyEvent::new(code, KeyModifiers::NONE));
        match l {
            "any key" => plain(KeyCode::Esc),
            "shift-enter" => Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            "←" => plain(KeyCode::Left),
            "→" => plain(KeyCode::Right),
            "↑" => plain(KeyCode::Up),
            "↓" => plain(KeyCode::Down),
            _ => {
                if let Some(c) = l.strip_prefix('^').filter(|c| c.chars().count() == 1) {
                    let c = c.chars().next()?.to_ascii_lowercase();
                    return Some(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
                }
                // (Not Alt chords: those are rlord's global keys, shown as
                // tips, not something the dialog handles.)
                let b = KeyBinding::parse(l).ok().filter(|b| !b.alt)?;
                Some(b.event())
            }
        }
    };
    if label.chars().count() > 1 && label.contains('/') {
        return label.split('/').filter_map(one).next_back();
    }
    one(label)
}

/// What a menu or a confirmation is about. The list can change under it
/// (a push from the daemon): it is found again by this, never by position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// A session, by id.
    Session(String),
    /// A group, by name.
    Group(String),
}

/// "Stop web-fix?": asked before a click stops or restarts a session (a
/// key does it at once, as always), before a session is archived however
/// that was asked (key, click, menu, launcher), and before the overlord
/// goes back to an earlier conversation.
pub(crate) struct Confirm {
    question: String,
    /// What it does, for the button ("stop").
    verb: &'static str,
    target: Target,
    /// What a yes does.
    then: Then,
}

/// What a confirmation does once answered yes.
enum Then {
    /// Press this key on the target (a click's stop or restart).
    Key(KeyEvent),
    Archive,
    /// Continue the overlord's earlier conversation with this id.
    Continue(String),
}

/// The right-click menu: what the row (or group) under the pointer offers,
/// each entry with its key.
pub(crate) struct Menu {
    x: u16,
    y: u16,
    /// What it was opened on: an entry acts on that, or not at all.
    target: Target,
    title: String,
    /// (key label, what it does, the key).
    items: Vec<(String, &'static str, KeyEvent)>,
    sel: usize,
}

impl App {
    /// Returns true to quit.
    pub(crate) fn handle_mouse(
        &mut self,
        m: MouseEvent,
        terminal: &mut DefaultTerminal,
    ) -> Result<bool> {
        // The key screen is waiting for a key: nothing else counts.
        if matches!(&self.mode, Mode::Keys(ks) if ks.listening()) {
            return Ok(false);
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click(m.column, m.row, false, terminal),
            MouseEventKind::Down(MouseButton::Right) => self.click(m.column, m.row, true, terminal),
            MouseEventKind::ScrollUp => self.wheel(m.column, m.row, true, terminal),
            MouseEventKind::ScrollDown => self.wheel(m.column, m.row, false, terminal),
            _ => Ok(false),
        }
    }

    fn click(
        &mut self,
        x: u16,
        y: u16,
        right: bool,
        terminal: &mut DefaultTerminal,
    ) -> Result<bool> {
        let Some(hit) = self.hits.at(x, y) else {
            return Ok(false);
        };
        let double = !right
            && self
                .last_click
                .take()
                .is_some_and(|(at, on)| on == self.clicked(&hit) && at.elapsed() < DOUBLE_CLICK);
        if !right && !double {
            self.last_click = Some((Instant::now(), self.clicked(&hit)));
        }
        // Typing in the footer (search, rename, …): a click elsewhere
        // doesn't throw the text away.
        if matches!(
            self.mode,
            Mode::Filter(_)
                | Mode::Rename(_)
                | Mode::Describe(_)
                | Mode::Tags(_)
                | Mode::RenameGroup(_)
        ) {
            return Ok(false);
        }
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        match hit {
            Hit::Key(k) if !right => {
                // Stop, restart, archive: a click asks first.
                if matches!(self.mode, Mode::Normal) && self.confirm_first(k) {
                    return Ok(false);
                }
                return self.handle_key(k, terminal);
            }
            Hit::Tab(t) if !right => self.switch_tab(t),
            Hit::SortBy(m) if !right => {
                if self.sort == m {
                    self.reverse = !self.reverse;
                } else {
                    self.sort = m;
                    self.reverse = false;
                }
                self.dirty = true;
            }
            Hit::Row(i) if i < self.sessions.len() => {
                self.group_focus = false;
                if self.table.selected() != Some(i) {
                    self.table.select(Some(i));
                    self.preview_soon();
                }
                if right {
                    self.open_menu(x, y, false);
                } else if double {
                    // As Enter: attach (an archived row says how to open it).
                    return self.do_action(Action::Attach, terminal);
                }
            }
            Hit::Group(i) if i < self.groups.len() => {
                self.group_focus = true;
                let cur = self.group_state.selected().unwrap_or(0);
                if cur != i {
                    self.move_group(i as isize - cur as isize);
                }
                if right {
                    self.open_menu(x, y, true);
                } else if double {
                    // As Enter on a group: into its sessions.
                    self.group_focus = self.sessions.is_empty();
                }
            }
            // The same one again: back to the live pane.
            Hit::Conv(i) if !right => {
                self.show_conversation(if self.conv_sel == i { 0 } else { i })
            }
            Hit::Pick(i) if !right => return self.pick(i, double, terminal),
            // Around a light modal: close it. (A form or a setup dialog
            // stays: a stray click mustn't lose what was entered.)
            Hit::Outside
                if matches!(
                    self.mode,
                    Mode::Help
                        | Mode::Menu(_)
                        | Mode::Confirm(_)
                        | Mode::Command(_)
                        | Mode::ConfirmQuit
                ) =>
            {
                return self.handle_key(esc, terminal)
            }
            Hit::Inside if matches!(self.mode, Mode::Help) => {
                return self.handle_key(esc, terminal)
            }
            _ => {}
        }
        Ok(false)
    }

    /// What a click landed on, for telling a double-click: a row by its
    /// session and a group by its name (the list may reorder between the
    /// two clicks), anything else by its place.
    fn clicked(&self, hit: &Hit) -> String {
        match hit {
            Hit::Row(i) => format!(
                "session {}",
                self.sessions
                    .get(*i)
                    .and_then(|s| s["id"].as_str())
                    .unwrap_or("?")
            ),
            Hit::Group(i) => format!(
                "group {}",
                self.groups.get(*i).map_or("?", |g| g.0.as_str())
            ),
            other => format!("{other:?}"),
        }
    }

    /// Put the highlight back on what a menu or confirmation is about.
    /// False (with a note) when it is no longer in the list.
    fn reselect(&mut self, target: &Target) -> bool {
        let found = match target {
            Target::Session(id) => {
                let at = self.sessions.iter().position(|s| s["id"] == id.as_str());
                if let Some(i) = at {
                    self.group_focus = false;
                    self.table.select(Some(i));
                }
                at.is_some()
            }
            Target::Group(g) => {
                let at = self.groups.iter().position(|(name, _)| name == g);
                if let Some(i) = at {
                    self.group_focus = true;
                    let cur = self.group_state.selected().unwrap_or(0);
                    if self.group_sel.as_deref() != Some(g.as_str()) {
                        self.move_group(i as isize - cur as isize);
                    }
                }
                at.is_some()
            }
        };
        if !found {
            self.status_line = "it's no longer in the list: nothing done".into();
        }
        found
    }

    /// A click asked for `k`: if that stops or restarts the highlighted
    /// session, ask first (true: a confirmation is now open). (Archiving
    /// asks by itself, however it was asked for: `confirm_archive`.)
    fn confirm_first(&mut self, k: KeyEvent) -> bool {
        if self.tab == Tab::Groups && self.group_focus {
            return false;
        }
        let verb = match self.keys.action(&k) {
            Some(Action::Stop) => "stop",
            Some(Action::Restart) => "restart",
            _ => return false,
        };
        let Some(s) = self.selected() else {
            return false;
        };
        let (Some(id), name) = (s["id"].as_str(), s["name"].as_str().unwrap_or("it")) else {
            return false;
        };
        let mut question = format!("{verb} {name}?");
        if let Some(first) = question.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        self.mode = Mode::Confirm(Box::new(Confirm {
            question,
            verb,
            target: Target::Session(id.to_string()),
            then: Then::Key(k),
        }));
        true
    }

    /// Archive the highlighted session, once the user says yes: `x` is
    /// too easy to hit by accident. (Bulk archiving goes through the
    /// overlord, which doesn't ask.)
    pub(crate) fn confirm_archive(&mut self) {
        let Some(s) = self.selected() else {
            return;
        };
        let (Some(id), name) = (s["id"].as_str(), s["name"].as_str().unwrap_or("it")) else {
            return;
        };
        self.mode = Mode::Confirm(Box::new(Confirm {
            question: format!("Archive {name}?"),
            verb: "archive",
            target: Target::Session(id.to_string()),
            then: Then::Archive,
        }));
    }

    /// Go back to the overlord's earlier conversation shown in the
    /// preview, once the user says yes.
    pub(crate) fn confirm_continue(&mut self) {
        let Some(conv) = self
            .conv_sel
            .checked_sub(1)
            .and_then(|i| self.earlier_convs().get(i))
            .and_then(|c| c["id"].as_str())
            .map(str::to_string)
        else {
            return;
        };
        let Some(id) = self.selected_id() else {
            return;
        };
        self.mode = Mode::Confirm(Box::new(Confirm {
            question: "Continue this conversation in the overlord? (Its current one ends.)".into(),
            verb: "continue",
            target: Target::Session(id),
            then: Then::Continue(conv),
        }));
    }

    /// Returns true to quit.
    pub(crate) fn handle_confirm(
        &mut self,
        k: KeyEvent,
        c: Confirm,
        terminal: &mut DefaultTerminal,
    ) -> Result<bool> {
        if !matches!(
            k.code,
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y')
        ) {
            self.status_line = "cancelled".into();
            return Ok(false);
        }
        if !self.reselect(&c.target) {
            return Ok(false);
        }
        match c.then {
            Then::Key(k) => return self.handle_key(k, terminal),
            Then::Archive => self.action("session.archive", "archived"),
            Then::Continue(conv) => {
                if self
                    .call("overlord.continue", json!({"id": conv}))
                    .is_some()
                {
                    self.status_line = "the overlord is back on that conversation".into();
                }
                self.conv_sel = 0;
                self.dirty = true;
            }
        }
        Ok(false)
    }

    pub(crate) fn draw_confirm(&self, f: &mut Frame, c: &Confirm) {
        let w = (c.question.width() as u16 + 8).max(40);
        let area = newform::centered(f.area(), w, 6);
        f.render_widget(Clear, area);
        let lines = vec![
            Line::from(""),
            Line::from(format!("  {}", c.question)).bold(),
            Line::from(""),
            keys_line(&[
                ("y/enter", c.verb, Cap::Context),
                ("n/esc", "cancel", Cap::Global),
            ]),
        ];
        self.modal_hits(f.area(), area, &lines);
        f.render_widget(Paragraph::new(lines).block(theme::modal("Confirm")), area);
    }

    /// Run a menu entry's key on what the menu was opened for.
    fn run_menu_item(
        &mut self,
        target: &Target,
        key: KeyEvent,
        terminal: &mut DefaultTerminal,
    ) -> Result<bool> {
        if !self.reselect(target) {
            return Ok(false);
        }
        // (The menu came from a click: stop and restart ask first; archive
        // asks by itself.)
        if self.confirm_first(key) {
            return Ok(false);
        }
        self.handle_key(key, terminal)
    }

    /// A click on item `i` of the open modal.
    fn pick(&mut self, i: usize, double: bool, terminal: &mut DefaultTerminal) -> Result<bool> {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        // What to press once the item is selected, if anything.
        let then = match &mut self.mode {
            Mode::Command(l) => {
                l.select(i);
                Some(KeyCode::Enter)
            }
            Mode::Menu(m) => {
                m.sel = i.min(m.items.len().saturating_sub(1));
                Some(KeyCode::Enter)
            }
            Mode::Keys(ks) => {
                ks.select(i);
                double.then_some(KeyCode::Enter)
            }
            Mode::NewSession(form) => form.click(i).then_some(KeyCode::Enter),
            // A row: select it; again: change it, as → does.
            Mode::OverlordSetup(s) => s.click_row(i).then_some(KeyCode::Right),
            _ => None,
        };
        match then {
            Some(code) => self.handle_key(key(code), terminal),
            None => Ok(false),
        }
    }

    fn wheel(&mut self, x: u16, y: u16, up: bool, terminal: &mut DefaultTerminal) -> Result<bool> {
        let key = KeyEvent::new(
            if up { KeyCode::Up } else { KeyCode::Down },
            KeyModifiers::NONE,
        );
        match self.hits.wheel_at(x, y) {
            Some(Hit::List) if matches!(self.mode, Mode::Normal) => {
                let n = self.sessions.len();
                if n > 0 {
                    let i = self.table.selected().unwrap_or(0);
                    let to = if up {
                        i.saturating_sub(1)
                    } else {
                        (i + 1).min(n - 1)
                    };
                    if to != i {
                        self.table.select(Some(to));
                        self.preview_soon();
                    }
                }
            }
            Some(Hit::Groups) if matches!(self.mode, Mode::Normal) => {
                self.move_group(if up { -1 } else { 1 })
            }
            Some(Hit::Preview) if matches!(self.mode, Mode::Normal) => {
                self.preview_scroll = if up {
                    (self.preview_scroll + 3).min(self.preview_scroll_max.get())
                } else {
                    self.preview_scroll.saturating_sub(3)
                };
            }
            // A modal with a list: the wheel moves its selection.
            Some(Hit::Inside | Hit::Outside) => {
                let moves = match &self.mode {
                    Mode::Command(_) | Mode::Menu(_) | Mode::Keys(_) => true,
                    Mode::NewSession(form) => form.has_list(),
                    _ => false,
                };
                if moves {
                    return self.handle_key(key, terminal);
                }
            }
            _ => {}
        }
        Ok(false)
    }

    /// Open the menu for the selected row, or (`group`) the selected group:
    /// exactly what its footer offers, so the same rules hold (archived,
    /// the overlord's fixed row).
    fn open_menu(&mut self, x: u16, y: u16, group: bool) {
        let legend = context_legend(group, self.group_sel.as_deref(), self.selected());
        let mut items: Vec<(String, &'static str, KeyEvent)> = legend
            .into_iter()
            .filter_map(|(a, label)| Some((self.keys.label(a), label, self.keys.event(a)?)))
            .collect();
        let target = if group {
            Target::Group(self.group_sel.clone().unwrap_or_default())
        } else {
            match self.selected_id() {
                Some(id) => Target::Session(id),
                None => return,
            }
        };
        let title = if group {
            let g = self.group_sel.clone().unwrap_or_default();
            if g != groups::UNTAGGED {
                if let Some(k) = self.keys.event(Action::Pin) {
                    let label = if self.is_favourite(&g) {
                        "unfavourite"
                    } else {
                        "favourite"
                    };
                    items.push((self.keys.label(Action::Pin), label, k));
                }
            }
            groups::group_label(&g)
        } else {
            self.selected()
                .and_then(|s| s["name"].as_str())
                .unwrap_or("")
                .to_string()
        };
        if items.is_empty() {
            return;
        }
        self.mode = Mode::Menu(Box::new(Menu {
            x,
            y,
            target,
            title,
            items,
            sel: 0,
        }));
    }

    /// Returns true to quit.
    pub(crate) fn handle_menu(
        &mut self,
        k: KeyEvent,
        mut m: Box<Menu>,
        terminal: &mut DefaultTerminal,
    ) -> Result<bool> {
        let n = m.items.len();
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => m.sel = (m.sel + 1).min(n - 1),
            KeyCode::Up | KeyCode::Char('k') => m.sel = m.sel.saturating_sub(1),
            // Run it as its key would (the menu is closed by now).
            KeyCode::Enter => return self.run_menu_item(&m.target, m.items[m.sel].2, terminal),
            // One of the listed keys runs that entry; anything else closes.
            _ => {
                let item = m.items.iter().find(|(_, _, key)| {
                    key.code == k.code
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                            == k.modifiers.contains(KeyModifiers::CONTROL)
                });
                return match item {
                    Some((_, _, key)) => self.run_menu_item(&m.target, *key, terminal),
                    None => Ok(false),
                };
            }
        }
        self.mode = Mode::Menu(m);
        Ok(false)
    }

    pub(crate) fn draw_menu(&self, f: &mut Frame, m: &Menu) {
        let screen = f.area();
        let key_w = m.items.iter().map(|(k, _, _)| k.width()).max().unwrap_or(1);
        let label_w = m.items.iter().map(|(_, l, _)| l.width()).max().unwrap_or(1);
        // Borders, padding, the key, a gap, the label.
        let w = ((key_w + label_w + 7).max(m.title.width() + 6) as u16).min(screen.width);
        let h = (m.items.len() as u16 + 2).min(screen.height);
        // At the pointer, pulled back inside the screen.
        let area = Rect {
            x: m.x.min(screen.right().saturating_sub(w)),
            y: m.y.min(screen.bottom().saturating_sub(h)),
            width: w,
            height: h,
        };
        self.hits.add(screen, Hit::Outside);
        self.hits.add(area, Hit::Inside);
        f.render_widget(Clear, area);
        let block = theme::modal(&m.title);
        let inner = block.inner(area);
        let lines: Vec<Line> = m
            .items
            .iter()
            .enumerate()
            .map(|(i, (key, label, _))| {
                self.hits.row(inner, inner.y + i as u16, Hit::Pick(i));
                let line = Line::from(vec![
                    Span::styled(
                        format!("{key:<key_w$}"),
                        Style::default().fg(theme::accent()).bold(),
                    ),
                    Span::raw("  "),
                    Span::styled(label.to_string(), Style::default().fg(theme::text())),
                ]);
                if i == m.sel {
                    line.style(theme::selected())
                } else {
                    line
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines).block(block), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn the_topmost_region_under_the_pointer_wins() {
        let regions = vec![
            (Rect::new(0, 0, 80, 24), Hit::List),
            (Rect::new(1, 3, 40, 1), Hit::Row(0)),
            (Rect::new(1, 4, 40, 1), Hit::Row(1)),
            (Rect::new(50, 0, 30, 20), Hit::Preview),
        ];
        let at = |x, y| hit_at(&regions, x, y, |_| true).cloned();
        assert_eq!(at(5, 4), Some(Hit::Row(1)));
        assert_eq!(
            at(5, 10),
            Some(Hit::List),
            "below the rows: the list itself"
        );
        assert_eq!(at(60, 4), Some(Hit::Preview));
        assert_eq!(at(90, 4), None);
        // The wheel looks through a row to the area it scrolls.
        let wheel = |h: &Hit| matches!(h, Hit::List | Hit::Preview);
        assert_eq!(hit_at(&regions, 5, 4, wheel), Some(&Hit::List));
        // A modal covers everything under it.
        let mut modal = regions.clone();
        modal.push((Rect::new(0, 0, 80, 24), Hit::Outside));
        modal.push((Rect::new(20, 5, 30, 6), Hit::Inside));
        modal.push((Rect::new(21, 6, 28, 1), Hit::Pick(0)));
        let at = |x, y| hit_at(&modal, x, y, |_| true).cloned();
        assert_eq!(at(5, 4), Some(Hit::Outside), "not the row underneath");
        assert_eq!(at(25, 6), Some(Hit::Pick(0)));
        assert_eq!(at(25, 8), Some(Hit::Inside));
    }

    /// Draw a small fleet and return the screen's rows (one char a cell).
    fn drawn(app: &mut App) -> Vec<String> {
        let (w, h) = (150, 34);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    /// Where `text` is on screen (its first cell).
    fn find(rows: &[String], text: &str) -> (u16, u16) {
        rows.iter()
            .enumerate()
            .find_map(|(y, r)| {
                r.find(text)
                    .map(|b| (r[..b].chars().count() as u16, y as u16))
            })
            .unwrap_or_else(|| panic!("{text:?} not on screen:\n{}", rows.join("\n")))
    }

    fn fleet() -> App {
        let (_tx, rx) = mpsc::channel();
        let keys = KeyMap::from_config(&Keys::default()).unwrap();
        let mut app = App::new(rx, Config::default(), keys);
        app.sessions = vec![
            json!({"id": "s1", "name": "login-flake", "status": "working", "agent": "claude",
                   "agent_session": "a", "tags": ["web"], "archived_at": null}),
            json!({"id": "s2", "name": "image-pipeline", "status": "idle", "agent": "codex",
                   "agent_session": "b", "tags": [], "archived_at": null}),
            json!({"id": "s3", "name": "old-migration", "status": "stopped", "agent": "claude",
                   "agent_session": "c", "tags": [], "archived_at": 5}),
        ];
        app.all = app.sessions.clone();
        app.table.select(Some(0));
        app
    }

    #[test]
    fn what_is_drawn_can_be_clicked() {
        let mut app = fleet();
        let rows = drawn(&mut app);
        let at = |text: &str, dx: u16| {
            let (x, y) = find(&rows, text);
            app.hits.at(x + dx, y)
        };
        // Rows, by what they show.
        assert_eq!(at("✻  login-flake", 4), Some(Hit::Row(0)));
        assert_eq!(at(">_ image-pipe", 4), Some(Hit::Row(1)));
        assert_eq!(at("✻  old-migration", 4), Some(Hit::Row(2)));
        // The header: tabs and the settings beside them.
        assert_eq!(at(" groups ", 2), Some(Hit::Tab(Tab::Groups)));
        let press = |a: Action| app.keys.event(a).map(Hit::Key);
        assert_eq!(at("⌕ search", 3), press(Action::Filter));
        assert_eq!(at("◷ live", 3), press(Action::ToggleArchived));
        // Column headers sort: the time column's whole label and its arrow
        // (it's the current sort), and Name.
        use rlord_core::db::SortMode;
        let (hx, hy) = find(&rows, "Last message ▼");
        let arrow = "Last message ".chars().count() as u16;
        assert_eq!(app.hits.at(hx, hy), Some(Hit::SortBy(SortMode::Activity)));
        assert_eq!(
            app.hits.at(hx + arrow, hy),
            Some(Hit::SortBy(SortMode::Activity))
        );
        assert_eq!(at("Name", 1), Some(Hit::SortBy(SortMode::Name)));
        // The toolbar's direction arrow flips it.
        assert_eq!(at("▼ ", 0), press(Action::ReverseSort));
        // The footer: a keycap's label presses its key; so do row keys.
        assert_eq!(at(" quit", 2), press(Action::Quit));
        assert_eq!(at(" help", 2), press(Action::Help));
        assert_eq!(at(" restart", 2), press(Action::Restart));
        assert_eq!(at(" archive", 2), press(Action::Archive));
        // The wheel over a row turns the list, not the row.
        let (x, y) = find(&rows, ">_ image-pipe");
        assert_eq!(app.hits.wheel_at(x, y), Some(Hit::List));
        let (x, y) = find(&rows, "live pane");
        assert_eq!(app.hits.wheel_at(x, y + 3), Some(Hit::Preview));
    }

    #[test]
    fn the_right_click_menu_offers_what_the_row_can_do() {
        let labels = |app: &App| -> Vec<&'static str> {
            match &app.mode {
                Mode::Menu(m) => m.items.iter().map(|i| i.1).collect(),
                _ => Vec::new(),
            }
        };
        let mut app = fleet();
        app.open_menu(30, 8, false);
        assert_eq!(
            labels(&app),
            [
                "attach", "restart", "stop", "archive", "rename", "pin", "tags", "describe",
                "notes"
            ]
        );
        // Drawn at the pointer, over everything; its entries are picks.
        let rows = drawn(&mut app);
        let (x, y) = find(&rows, "restart");
        assert_eq!(app.hits.at(x, y), Some(Hit::Pick(1)));
        assert_eq!(
            app.hits.at(2, 20),
            Some(Hit::Outside),
            "not the list under it"
        );
        assert!((y, x) > (8, 30), "below and right of the pointer: {x},{y}");
        // An archived row: only what an archived row can do.
        app.mode = Mode::Normal;
        app.table.select(Some(2));
        app.open_menu(30, 8, false);
        assert_eq!(
            labels(&app),
            ["unarchive", "rename", "tags", "describe", "notes"]
        );
        // A group: open, rename, favourite.
        app.mode = Mode::Normal;
        app.tab = Tab::Groups;
        app.groups = vec![("web".into(), 1)];
        app.group_sel = Some("web".into());
        app.group_focus = true;
        app.open_menu(3, 8, true);
        assert_eq!(labels(&app), ["open", "rename group", "favourite"]);
        // At the screen's edge the menu is pulled back inside it.
        app.mode = Mode::Normal;
        app.open_menu(149, 33, true);
        let rows = drawn(&mut app);
        let (x, y) = find(&rows, "favourite");
        assert!(x < 150 && y < 34);
    }

    #[test]
    fn a_menu_acts_on_what_it_was_opened_for_or_not_at_all() {
        let mut app = fleet();
        app.table.select(Some(1));
        app.open_menu(30, 8, false);
        let target = match &app.mode {
            Mode::Menu(m) => m.target.clone(),
            _ => panic!("no menu"),
        };
        assert_eq!(target, Target::Session("s2".into()));
        // The list reorders under the open menu: the entry still finds
        // its session, not the row that took its place.
        app.sessions.swap(0, 1);
        assert!(app.reselect(&target));
        assert_eq!(app.selected_id().as_deref(), Some("s2"));
        // It left the list (archived elsewhere): nothing is done.
        app.sessions.retain(|s| s["id"] != "s2");
        assert!(!app.reselect(&target));
        assert!(app.status_line.contains("no longer"));
        assert_ne!(app.selected_id().as_deref(), Some("s2"));
    }

    #[test]
    fn a_click_asks_before_stopping_restarting_or_archiving() {
        let mut app = fleet();
        app.mode = Mode::Normal;
        let key = |a: Action| app.keys.event(a).unwrap();
        let (stop, rename) = (key(Action::Stop), key(Action::Rename));
        assert!(app.confirm_first(stop));
        match &app.mode {
            Mode::Confirm(c) => {
                assert_eq!(c.question, "Stop login-flake?");
                assert_eq!(c.target, Target::Session("s1".into()));
            }
            _ => panic!("no confirmation"),
        }
        // Drawn with its buttons; a click on "stop" presses enter.
        let rows = drawn(&mut app);
        let (x, y) = find(&rows, "y/enter");
        assert_eq!(app.hits.at(x, y), Some(Hit::Key(key_enter())));
        assert_eq!(
            app.hits.at(x + 10, y),
            Some(Hit::Key(key_enter())),
            "its label"
        );
        // Nothing destructive: no question.
        app.mode = Mode::Normal;
        assert!(!app.confirm_first(rename));
        assert!(matches!(app.mode, Mode::Normal));
    }

    fn key_enter() -> KeyEvent {
        key(KeyCode::Enter)
    }

    #[test]
    fn a_double_click_is_two_clicks_on_the_same_session() {
        let mut app = fleet();
        let first = app.clicked(&Hit::Row(0));
        assert_eq!(first, "session s1");
        // The list reordered between the clicks: row 0 is another session,
        // so the second click is not a double-click on the first.
        app.sessions.swap(0, 1);
        assert_ne!(app.clicked(&Hit::Row(0)), first);
        assert_eq!(app.clicked(&Hit::Row(1)), first);
    }

    #[test]
    fn dialog_hints_name_their_key() {
        assert_eq!(key_from_hint("enter"), Some(key(KeyCode::Enter)));
        assert_eq!(key_from_hint("esc"), Some(key(KeyCode::Esc)));
        assert_eq!(key_from_hint("tab"), Some(key(KeyCode::Tab)));
        assert_eq!(key_from_hint("y/enter"), Some(key(KeyCode::Enter)));
        assert_eq!(key_from_hint("←/→"), Some(key(KeyCode::Right)));
        assert_eq!(key_from_hint("any key"), Some(key(KeyCode::Esc)));
        assert_eq!(key_from_hint("/"), Some(key(KeyCode::Char('/'))));
        assert_eq!(
            key_from_hint("shift-enter"),
            Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
        );
        assert_eq!(
            key_from_hint("^E"),
            Some(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
        );
        // Not one key, or a global chord shown as a tip: nothing to press.
        assert_eq!(key_from_hint("↑↓"), None);
        assert_eq!(key_from_hint("M-o"), None);
    }

    #[test]
    fn keycaps_in_dialog_lines_become_regions() {
        let hits = Hits::default();
        let lines = vec![
            Line::from("  Quit rlord?"),
            keys_line(&[
                ("y/enter", "quit", Cap::Context),
                ("any key", "stay", Cap::Global),
            ]),
        ];
        hits.keycaps(10, 5, &lines);
        // "  " then ` y/enter ` (9) + ` quit` (5) + gap (2), then ` any key `.
        assert_eq!(hits.at(12, 6), Some(Hit::Key(key(KeyCode::Enter))));
        assert_eq!(
            hits.at(25, 6),
            Some(Hit::Key(key(KeyCode::Enter))),
            "its label"
        );
        assert_eq!(hits.at(26, 6), None, "the gap between two hints");
        assert_eq!(hits.at(28, 6), Some(Hit::Key(key(KeyCode::Esc))));
        assert_eq!(hits.at(12, 5), None, "plain text");
    }
}
