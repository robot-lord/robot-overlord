//! The tags editor (`#`): a modal listing the session's tags, with an input
//! that suggests known tags (every tag in use, archived sessions included)
//! and asks before creating a new one, so a half-typed tag isn't saved by
//! accident.

use super::*;
use ratatui::widgets::Clear;

/// How many known-tag suggestions show under the input.
const MAX_SUGGESTIONS: usize = 6;

/// A tag as rlord stores it: no `#`, lower case, no spaces or commas.
pub(crate) fn normalize(t: &str) -> String {
    t.trim()
        .trim_start_matches('#')
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',')
        .collect()
}

/// Known tags matching `query`, best first: the exact tag, then prefix
/// matches, then substring, then in-order (fuzzy) matches; ties by how many
/// sessions use the tag, then by name. Tags in `exclude` (already on the
/// session) are left out.
pub(crate) fn suggest(query: &str, known: &[(String, usize)], exclude: &[String]) -> Vec<String> {
    let q = normalize(query);
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u8, usize, &str)> = known
        .iter()
        .filter(|(t, _)| !t.is_empty() && !has(exclude, t))
        .filter_map(|(t, n)| {
            let lt = t.to_lowercase();
            let rank = if lt == q {
                0
            } else if lt.starts_with(&q) {
                1
            } else if lt.contains(&q) {
                2
            } else if subsequence(&q, &lt) {
                3
            } else {
                return None;
            };
            Some((rank, *n, t.as_str()))
        })
        .collect();
    hits.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
    hits.into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(_, _, t)| t.to_string())
        .collect()
}

/// Whether `list` has `t`, ignoring case (tags are stored lower case, but
/// one set elsewhere may not be).
pub(crate) fn has(list: &[String], t: &str) -> bool {
    let t = t.to_lowercase();
    list.iter().any(|x| x.to_lowercase() == t)
}

fn subsequence(q: &str, t: &str) -> bool {
    let mut it = t.chars();
    q.chars().all(|c| it.any(|x| x == c))
}

/// A row under the input: what `enter` adds when it is highlighted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Choice {
    /// A tag some session uses (or this one had).
    Known(String),
    /// The text as typed, a tag nobody uses: adding it asks first.
    New(String),
}

impl Choice {
    pub fn tag(&self) -> &str {
        match self {
            Choice::Known(t) | Choice::New(t) => t,
        }
    }
}

pub(crate) struct TagEditor {
    pub session: String,
    pub name: String,
    /// The tags as they were when the editor opened.
    pub original: Vec<String>,
    /// The tags as they will be saved.
    pub tags: Vec<String>,
    /// Every tag in use, with how many sessions have it.
    pub known: Vec<(String, usize)>,
    pub input: String,
    /// Highlighted row under the input (while typing).
    pub sel: usize,
    /// Highlighted tag in the list, picked with ↑↓ on an empty input;
    /// `d`/Delete removes it. None until the user picks one.
    pub tag_sel: Option<usize>,
    /// A new tag waiting for y/n.
    pub confirm_new: Option<String>,
    /// Save once the pending new tag is answered (ctrl-s with text typed).
    pub save_after: bool,
    /// Esc with unsaved changes: asking whether to discard them.
    pub confirm_discard: bool,
}

impl TagEditor {
    pub fn new(session: &str, name: &str, tags: Vec<String>, known: Vec<(String, usize)>) -> Self {
        TagEditor {
            session: session.into(),
            name: name.into(),
            original: tags.clone(),
            tags,
            known,
            input: String::new(),
            sel: 0,
            tag_sel: None,
            confirm_new: None,
            save_after: false,
            confirm_discard: false,
        }
    }

    fn is_known(&self, t: &str) -> bool {
        let t = t.to_lowercase();
        self.known.iter().any(|(k, _)| k.to_lowercase() == t)
            || self.original.iter().any(|k| k.to_lowercase() == t)
    }

    /// The rows under the input, in order. The typed text comes first when
    /// it isn't a known tag (as "new"), so the first row is always what the
    /// text itself would add.
    pub fn choices(&self) -> Vec<Choice> {
        let t = normalize(&self.input);
        // Empty, or a tag the session already has: no rows, nothing
        // highlighted, and enter just clears the input.
        if t.is_empty() || has(&self.tags, &t) {
            return Vec::new();
        }
        let mut known = self.known.clone();
        // An original tag removed this session can come back without asking.
        for o in &self.original {
            if !known.iter().any(|(k, _)| k == o) {
                known.push((o.clone(), 0));
            }
        }
        let mut v: Vec<Choice> = suggest(&t, &known, &self.tags)
            .into_iter()
            .map(Choice::Known)
            .collect();
        if !self.is_known(&t) {
            v.insert(0, Choice::New(t));
        }
        v
    }

    /// Add the highlighted row: a known tag goes straight in, a new one
    /// waits for y/n. False when there was nothing to add.
    fn take_choice(&mut self) -> bool {
        let typed = normalize(&self.input);
        let c = if has(&self.tags, &typed) {
            None
        } else {
            self.choices().into_iter().nth(self.sel)
        };
        let Some(c) = c else {
            // Typed a tag the session already has: just clear the input.
            self.input.clear();
            self.sel = 0;
            return false;
        };
        match c {
            Choice::Known(t) => self.add(t),
            Choice::New(t) => self.confirm_new = Some(t),
        }
        true
    }

    pub fn add(&mut self, t: String) {
        if !has(&self.tags, &t) {
            self.tags.push(t);
        }
        self.input.clear();
        self.sel = 0;
        self.confirm_new = None;
    }

    /// Remove the tag picked in the list.
    pub fn remove_selected(&mut self) {
        if let Some(i) = self.tag_sel.filter(|i| *i < self.tags.len()) {
            self.tags.remove(i);
            self.tag_sel = if self.tags.is_empty() {
                None
            } else {
                Some(i.min(self.tags.len() - 1))
            };
        }
    }

    pub fn changed(&self) -> bool {
        let mut a = self.original.clone();
        let mut b = self.tags.clone();
        a.sort();
        b.sort();
        a != b
    }

    /// (add, remove) relative to the original tags.
    pub fn diff(&self) -> (Vec<String>, Vec<String>) {
        let add = self
            .tags
            .iter()
            .filter(|t| !self.original.contains(t))
            .cloned()
            .collect();
        let remove = self
            .original
            .iter()
            .filter(|t| !self.tags.contains(t))
            .cloned()
            .collect();
        (add, remove)
    }

    /// One key. Returns Some(true) to save, Some(false) to cancel, None to
    /// stay open.
    pub fn key(&mut self, k: KeyEvent) -> Option<bool> {
        if self.confirm_discard {
            self.confirm_discard = false;
            // y or esc again: discard. Anything else: back to editing.
            return match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Esc => Some(false),
                _ => None,
            };
        }
        if let Some(t) = self.confirm_new.clone() {
            match k.code {
                // Only an explicit y creates it: a reflexive enter doesn't.
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.add(t);
                    if std::mem::take(&mut self.save_after) {
                        return Some(true);
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.confirm_new = None;
                    self.save_after = false;
                }
                // Anything else waits for the answer.
                _ => {}
            }
            return None;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let typing = !self.input.is_empty();
        match k.code {
            // Esc clears typed text first; on an empty input it cancels.
            KeyCode::Esc if typing => {
                self.input.clear();
                self.sel = 0;
            }
            KeyCode::Esc if self.changed() => self.confirm_discard = true,
            KeyCode::Esc => return Some(false),
            // Save: typed text is resolved first, never dropped.
            KeyCode::Char('s') if ctrl => {
                if typing && self.take_choice() && self.confirm_new.is_some() {
                    self.save_after = true;
                    return None;
                }
                return Some(true);
            }
            KeyCode::Enter if !typing => return Some(true),
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char(',') if typing && !ctrl => {
                self.take_choice();
            }
            KeyCode::Tab if typing => {
                if let Some(c) = self.choices().get(self.sel) {
                    self.input = c.tag().to_string();
                    self.sel = 0;
                }
            }
            KeyCode::Up if typing => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down if typing => {
                if self.sel + 1 < self.choices().len() {
                    self.sel += 1;
                }
            }
            KeyCode::Up if !self.tags.is_empty() => {
                let last = self.tags.len() - 1;
                self.tag_sel = Some(self.tag_sel.map_or(last, |i| i.saturating_sub(1)));
            }
            KeyCode::Down if !self.tags.is_empty() => {
                let last = self.tags.len() - 1;
                self.tag_sel = Some(self.tag_sel.map_or(0, |i| (i + 1).min(last)));
            }
            // Removing takes its own key on a tag picked with ↑↓, so a held
            // backspace never eats tags.
            KeyCode::Delete if !typing => self.remove_selected(),
            KeyCode::Char('d') if ctrl && !typing => self.remove_selected(),
            KeyCode::Backspace => {
                self.input.pop();
                self.sel = 0;
            }
            KeyCode::Char('#') if !ctrl && !typing => {}
            KeyCode::Char(c) if !ctrl && c != ' ' && c != ',' => {
                self.tag_sel = None;
                self.input.push(c);
                self.sel = 0;
            }
            _ => {}
        }
        None
    }
}

impl App {
    /// Open the tags editor on the selected session.
    pub(crate) fn open_tags_form(&mut self) {
        let Some(s) = self.selected() else {
            return;
        };
        let (id, name) = (
            s["id"].as_str().unwrap_or("").to_string(),
            s["name"].as_str().unwrap_or("?").to_string(),
        );
        let tags = groups::tags_of(s).into_iter().map(str::to_string).collect();
        let known = self.known_tags();
        self.mode = Mode::Tags(Box::new(TagEditor::new(&id, &name, tags, known)));
    }

    /// Every tag in use, with how many sessions have it, archived ones too
    /// and regardless of the filter (`tag.list` when the editor opens); the
    /// loaded list if that call fails.
    fn known_tags(&mut self) -> Vec<(String, usize)> {
        if let Some(v) = self.call("tag.list", json!({"archived": true})) {
            return v["groups"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|g| {
                    Some((
                        g["tag"].as_str()?.to_string(),
                        g["count"].as_u64()? as usize,
                    ))
                })
                .collect();
        }
        let mut m: std::collections::BTreeMap<String, usize> = Default::default();
        for s in &self.all {
            for t in groups::tags_of(s) {
                *m.entry(t.to_string()).or_default() += 1;
            }
        }
        m.into_iter().collect()
    }

    pub(crate) fn handle_tags_form(&mut self, k: KeyEvent, ed: Box<TagEditor>) {
        let mut ed = ed;
        match ed.key(k) {
            None => self.mode = Mode::Tags(ed),
            Some(false) => {}
            Some(true) => {
                if ed.changed() {
                    let (add, remove) = ed.diff();
                    self.save_tags(&ed.session, &ed.name, &ed.tags, &add, &remove);
                }
            }
        }
    }

    pub(crate) fn draw_tags_form(&self, f: &mut Frame, ed: &TagEditor) {
        let choices = ed.choices();
        // Fixed rows: title, blank, input, choices, blank, hints (2),
        // borders (2). The tag list gets what's left and scrolls.
        let fixed = 8 + choices.len() as u16;
        let screen = f.area();
        let want = fixed + ed.tags.len().max(1) as u16;
        let h = want.min(screen.height);
        let area = newform::centered(screen, 60, h);
        f.render_widget(Clear, area);
        let room = (h.saturating_sub(fixed) as usize).max(1);
        let mut lines = vec![Line::from(Span::styled(
            "  Tags (= groups)",
            Style::default().bold(),
        ))];
        if ed.tags.is_empty() {
            lines.push(Line::from(Span::styled("    no tags", theme::faint())));
        }
        // Scroll so the picked tag (else the newest) stays in view.
        let focus = ed.tag_sel.unwrap_or(ed.tags.len().saturating_sub(1));
        let top = (focus + 1).saturating_sub(room);
        let more_above = top > 0;
        let more_below = top + room < ed.tags.len();
        for (i, t) in ed.tags.iter().enumerate().skip(top).take(room) {
            let on = ed.tag_sel == Some(i) && ed.confirm_new.is_none();
            let mut spans = vec![Span::raw(if on { "  ▸ " } else { "    " })];
            spans.push(Span::styled(
                format!("#{t}"),
                Style::default().fg(theme::tag_color(t)),
            ));
            if !ed.original.contains(t) {
                spans.push(Span::styled("  (added)", theme::faint()));
            }
            if (i == top && more_above) || (i + 1 == top + room && more_below) {
                spans.push(Span::styled(
                    if i == top && more_above {
                        "  ↑ more"
                    } else {
                        "  ↓ more"
                    },
                    theme::faint(),
                ));
            }
            let l = Line::from(spans);
            lines.push(if on { l.style(theme::selected()) } else { l });
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  add: #", theme::muted()),
            Span::styled(format!("{}▏", ed.input), Style::default().fg(theme::text())),
        ]));
        for (i, c) in choices.iter().enumerate() {
            let on = i == ed.sel;
            let t = c.tag();
            let mut spans = vec![
                Span::raw(if on { "    ▸ " } else { "      " }),
                Span::styled(format!("#{t}"), Style::default().fg(theme::tag_color(t))),
            ];
            if matches!(c, Choice::New(_)) {
                spans.push(Span::styled("  (new tag)", theme::faint()));
            }
            let l = Line::from(spans);
            lines.push(if on { l.style(theme::selected()) } else { l });
        }
        lines.push(Line::from(""));
        if ed.confirm_discard {
            lines.push(Line::from(Span::styled(
                "  Discard your changes to the tags?",
                Style::default().fg(theme::yellow()).bold(),
            )));
            lines.push(keys_line(&[
                ("y/esc", "discard", Cap::Context),
                ("n", "keep editing", Cap::Global),
            ]));
        } else if let Some(t) = &ed.confirm_new {
            lines.push(Line::from(Span::styled(
                format!("  #{t} is a new tag. Create it?"),
                Style::default().fg(theme::yellow()).bold(),
            )));
            lines.push(keys_line(&[
                ("y", "create", Cap::Context),
                ("n", "back", Cap::Global),
            ]));
        } else if ed.input.is_empty() {
            // Two short rows: nothing runs past the modal's inner width.
            let mut keys = vec![("enter", "save", Cap::Context)];
            if ed.tag_sel.is_some() {
                keys.push(("^d", "remove", Cap::Context));
            }
            lines.push(keys_line(&keys));
            lines.push(keys_line(&[
                ("↑↓", "pick", Cap::Global),
                ("esc", "cancel", Cap::Global),
            ]));
        } else {
            lines.push(keys_line(&[
                ("enter", "add", Cap::Context),
                ("tab", "complete", Cap::Global),
                ("↑↓", "choose", Cap::Global),
            ]));
            lines.push(keys_line(&[
                ("^s", "add and save", Cap::Global),
                ("esc", "clear", Cap::Global),
            ]));
        }
        self.modal_hits(f.area(), area, &lines);
        f.render_widget(
            Paragraph::new(lines).block(theme::modal(&format!("# {}", ed.name))),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Vec<(String, usize)> {
        [
            ("web", 3),
            ("wip", 5),
            ("release", 1),
            ("tooling", 2),
            ("ui", 1),
            ("we", 1),
        ]
        .into_iter()
        .map(|(t, n)| (t.to_string(), n))
        .collect()
    }

    fn ed(tags: &[&str]) -> TagEditor {
        TagEditor::new(
            "id",
            "s",
            tags.iter().map(|t| t.to_string()).collect(),
            known(),
        )
    }

    fn press(e: &mut TagEditor, c: KeyCode) -> Option<bool> {
        e.key(KeyEvent::new(c, KeyModifiers::NONE))
    }

    fn ctrl_s(e: &mut TagEditor) -> Option<bool> {
        e.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
    }

    fn typ(e: &mut TagEditor, s: &str) {
        for c in s.chars() {
            press(e, KeyCode::Char(c));
        }
    }

    #[test]
    fn suggestions_rank_exact_prefix_substring_fuzzy() {
        // "w": prefix, most used first.
        assert_eq!(suggest("w", &known(), &[]), ["wip", "web", "we"]);
        // An exact match beats more-used prefix matches.
        assert_eq!(suggest("we", &known(), &[]), ["we", "web"]);
        assert_eq!(suggest("l", &known(), &[]), ["tooling", "release"]);
        assert_eq!(suggest("tlg", &known(), &[]), ["tooling"]);
        assert_eq!(suggest("#WE", &known(), &[]), ["we", "web"]);
        assert!(suggest("", &known(), &[]).is_empty());
        assert!(suggest("zzz", &known(), &[]).is_empty());
        assert_eq!(suggest("w", &known(), &["wip".into()]), ["web", "we"]);
    }

    #[test]
    fn first_row_is_what_enter_adds() {
        let mut e = ed(&[]);
        typ(&mut e, "too");
        // Unknown text: it heads the rows as new, ahead of completions.
        assert_eq!(
            e.choices(),
            [Choice::New("too".into()), Choice::Known("tooling".into())]
        );
        // Known exact text: it is the first row.
        let mut e = ed(&[]);
        typ(&mut e, "we");
        assert_eq!(e.choices()[0], Choice::Known("we".into()));
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.tags, ["we"]);
    }

    #[test]
    fn known_tag_adds_unknown_asks() {
        let mut e = ed(&[]);
        typ(&mut e, "web");
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.tags, ["web"]);
        typ(&mut e, "#Newthing");
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.confirm_new.as_deref(), Some("newthing"));
        // Other keys are ignored while asking: nothing typed, nothing lost.
        typ(&mut e, "xz");
        press(&mut e, KeyCode::Backspace);
        assert_eq!(e.input, "Newthing");
        assert!(e.confirm_new.is_some());
        // n goes back to the input with the text kept.
        press(&mut e, KeyCode::Char('n'));
        assert!(e.confirm_new.is_none());
        assert_eq!(e.input, "Newthing");
        press(&mut e, KeyCode::Enter);
        press(&mut e, KeyCode::Char('y'));
        assert_eq!(e.tags, ["web", "newthing"]);
        assert!(e.input.is_empty());
        // Already on the session: input cleared, nothing added.
        typ(&mut e, "web");
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.tags.len(), 2);
        assert!(e.input.is_empty());
    }

    #[test]
    fn partial_tag_is_never_saved_silently() {
        let mut e = ed(&["web"]);
        typ(&mut e, "too");
        assert_eq!(press(&mut e, KeyCode::Enter), None);
        assert_eq!(e.confirm_new.as_deref(), Some("too"));
        assert_eq!(press(&mut e, KeyCode::Esc), None);
        press(&mut e, KeyCode::Tab);
        // Tab on the "new" row keeps the text; ↓ then tab completes.
        assert_eq!(e.input, "too");
        press(&mut e, KeyCode::Down);
        press(&mut e, KeyCode::Tab);
        assert_eq!(e.input, "tooling");
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.tags, ["web", "tooling"]);
        assert_eq!(press(&mut e, KeyCode::Enter), Some(true));
        assert_eq!(e.diff(), (vec!["tooling".into()], vec![]));
    }

    #[test]
    fn ctrl_s_resolves_typed_text_first() {
        // Known: added, then saved.
        let mut e = ed(&[]);
        typ(&mut e, "ui");
        assert_eq!(ctrl_s(&mut e), Some(true));
        assert_eq!(e.tags, ["ui"]);
        // New: asks; y saves, n stays open without saving.
        let mut e = ed(&[]);
        typ(&mut e, "fresh");
        assert_eq!(ctrl_s(&mut e), None);
        assert_eq!(press(&mut e, KeyCode::Char('n')), None);
        assert!(e.tags.is_empty());
        assert_eq!(ctrl_s(&mut e), None);
        assert_eq!(press(&mut e, KeyCode::Char('y')), Some(true));
        assert_eq!(e.tags, ["fresh"]);
    }

    #[test]
    fn arrows_pick_a_suggestion() {
        let mut e = ed(&[]);
        typ(&mut e, "w");
        // Rows: new "w", wip, web, we.
        press(&mut e, KeyCode::Down);
        press(&mut e, KeyCode::Down);
        press(&mut e, KeyCode::Char(' '));
        assert_eq!(e.tags, ["web"]);
    }

    #[test]
    fn removing_needs_a_picked_tag_and_its_own_key() {
        let mut e = ed(&["web", "wip", "ui"]);
        // Held backspace on an empty input removes nothing.
        for _ in 0..5 {
            press(&mut e, KeyCode::Backspace);
        }
        assert_eq!(e.tags, ["web", "wip", "ui"]);
        // Nothing picked: Delete does nothing.
        press(&mut e, KeyCode::Delete);
        assert_eq!(e.tags.len(), 3);
        // A letter types, and drops a pick.
        press(&mut e, KeyCode::Up);
        press(&mut e, KeyCode::Char('d'));
        assert_eq!(e.input, "d");
        assert_eq!(e.tag_sel, None);
        press(&mut e, KeyCode::Backspace);
        assert_eq!(e.tags.len(), 3);
        // ↑ picks the last, ↑ again the one before; ctrl-d removes it.
        press(&mut e, KeyCode::Up);
        press(&mut e, KeyCode::Up);
        e.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(e.tags, ["web", "ui"]);
        press(&mut e, KeyCode::Delete);
        assert_eq!(e.tags, ["web"]);
        assert_eq!(e.diff(), (vec![], vec!["wip".into(), "ui".into()]));
        // Changes made: esc asks first; esc again discards.
        assert_eq!(press(&mut e, KeyCode::Esc), None);
        assert!(e.confirm_discard);
        assert_eq!(press(&mut e, KeyCode::Esc), Some(false));
    }

    #[test]
    fn discard_asks_and_n_keeps_editing() {
        let mut e = ed(&[]);
        typ(&mut e, "ui");
        press(&mut e, KeyCode::Enter);
        assert_eq!(press(&mut e, KeyCode::Esc), None);
        assert_eq!(press(&mut e, KeyCode::Char('n')), None);
        assert_eq!(e.tags, ["ui"]);
        press(&mut e, KeyCode::Esc);
        assert_eq!(press(&mut e, KeyCode::Char('y')), Some(false));
    }

    #[test]
    fn case_never_makes_a_duplicate() {
        let mut e = TagEditor::new(
            "id",
            "s",
            vec!["Web".into()],
            vec![("Web".into(), 2), ("Tools".into(), 1)],
        );
        // Typing the session's tag in another case: no rows, nothing added.
        typ(&mut e, "web");
        assert!(e.choices().is_empty());
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.tags, ["Web"]);
        // A known tag in another case is known, added as it is spelled.
        typ(&mut e, "tools");
        assert_eq!(e.choices(), [Choice::Known("Tools".into())]);
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.tags, ["Web", "Tools"]);
        assert!(e.confirm_new.is_none());
    }

    #[test]
    fn existing_tag_typed_highlights_nothing() {
        // "we" is on the session; "web" would otherwise be suggested.
        let mut e = ed(&["we"]);
        typ(&mut e, "we");
        assert!(e.choices().is_empty());
        press(&mut e, KeyCode::Down);
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.tags, ["we"]);
    }

    #[test]
    fn esc_clears_text_then_cancels() {
        let mut e = ed(&[]);
        typ(&mut e, "we");
        assert_eq!(press(&mut e, KeyCode::Esc), None);
        assert!(e.input.is_empty());
        assert_eq!(press(&mut e, KeyCode::Esc), Some(false));
    }

    #[test]
    fn enter_never_creates_a_new_tag() {
        let mut e = ed(&[]);
        typ(&mut e, "fresh");
        press(&mut e, KeyCode::Enter);
        press(&mut e, KeyCode::Enter);
        assert!(e.tags.is_empty());
        assert!(e.confirm_new.is_some());
    }

    #[test]
    fn typing_a_tag_it_has_adds_nothing() {
        // "we" is on the session; "web" would be the top suggestion.
        let mut e = ed(&["we"]);
        typ(&mut e, "we");
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.tags, ["we"]);
        assert!(e.input.is_empty());
    }

    #[test]
    fn removed_original_comes_back_without_asking() {
        let mut e = ed(&["oddone"]);
        press(&mut e, KeyCode::Up);
        press(&mut e, KeyCode::Delete);
        typ(&mut e, "oddone ");
        assert_eq!(e.tags, ["oddone"]);
        assert!(!e.changed());
    }

    #[test]
    fn normalize_strips_hash_case_space() {
        assert_eq!(normalize(" #Web,"), "web");
        assert_eq!(normalize("a b"), "ab");
    }
}
