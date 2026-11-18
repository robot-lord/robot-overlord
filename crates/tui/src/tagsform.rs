//! The tags editor (`#`): a modal listing the session's tags, with an input
//! that suggests known tags (every group in use) and asks before creating a
//! new one, so a half-typed tag isn't saved by accident.

use super::*;
use ratatui::widgets::Clear;

/// How many suggestions show under the input.
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

/// Known tags matching `query`, best first: prefix matches, then substring,
/// then in-order (fuzzy) matches; ties by how many sessions use the tag,
/// then by name. Tags in `exclude` (already on the session) are left out.
pub(crate) fn suggest(query: &str, known: &[(String, usize)], exclude: &[String]) -> Vec<String> {
    let q = normalize(query);
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(u8, usize, &str)> = known
        .iter()
        .filter(|(t, _)| !t.is_empty() && !exclude.contains(t))
        .filter_map(|(t, n)| {
            let rank = if t.starts_with(&q) {
                0
            } else if t.contains(&q) {
                1
            } else if subsequence(&q, t) {
                2
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

fn subsequence(q: &str, t: &str) -> bool {
    let mut it = t.chars();
    q.chars().all(|c| it.any(|x| x == c))
}

/// What `enter` on the input does with a typed tag.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AddOutcome {
    /// Nothing typed, or the session already has it.
    Nothing,
    /// A known tag: added.
    Added,
    /// Not a tag any session uses: ask first.
    AskNew(String),
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
    /// Highlighted suggestion (input non-empty) or tag (input empty).
    pub sel: usize,
    /// A new tag waiting for y/n.
    pub confirm_new: Option<String>,
}

impl TagEditor {
    pub fn new(session: &str, name: &str, tags: Vec<String>, known: Vec<(String, usize)>) -> Self {
        let sel = tags.len().saturating_sub(1);
        TagEditor {
            session: session.into(),
            name: name.into(),
            original: tags.clone(),
            tags,
            known,
            input: String::new(),
            sel,
            confirm_new: None,
        }
    }

    pub fn suggestions(&self) -> Vec<String> {
        suggest(&self.input, &self.known, &self.tags)
    }

    fn is_known(&self, t: &str) -> bool {
        self.known.iter().any(|(k, _)| k == t)
    }

    /// Add the typed tag if known; a new one waits for confirmation.
    pub fn submit(&mut self) -> AddOutcome {
        let t = normalize(&self.input);
        if t.is_empty() {
            return AddOutcome::Nothing;
        }
        if self.tags.contains(&t) {
            self.input.clear();
            return AddOutcome::Nothing;
        }
        // The original tags count as known: re-adding one is no surprise.
        if self.is_known(&t) || self.original.contains(&t) {
            self.add(t);
            AddOutcome::Added
        } else {
            self.confirm_new = Some(t.clone());
            AddOutcome::AskNew(t)
        }
    }

    pub fn add(&mut self, t: String) {
        if !self.tags.contains(&t) {
            self.tags.push(t);
        }
        self.input.clear();
        self.confirm_new = None;
        self.sel = self.tags.len() - 1;
    }

    /// Remove the highlighted tag.
    pub fn remove_selected(&mut self) {
        if self.sel < self.tags.len() {
            self.tags.remove(self.sel);
            self.sel = self.sel.min(self.tags.len().saturating_sub(1));
        }
    }

    /// Fill the input with the highlighted suggestion.
    pub fn accept_suggestion(&mut self) {
        if let Some(s) = self.suggestions().get(self.sel) {
            self.input = s.clone();
            self.sel = 0;
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
        if let Some(t) = self.confirm_new.clone() {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.add(t),
                // n / esc: back to the input to fix it.
                _ => self.confirm_new = None,
            }
            return None;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return Some(false),
            KeyCode::Char('s') if ctrl => return Some(true),
            KeyCode::Enter if self.input.trim().is_empty() => return Some(true),
            KeyCode::Enter => {
                // A highlighted suggestion other than the first wins over
                // the typed text; else the text as typed.
                let sugg = self.suggestions();
                if self.sel > 0 {
                    if let Some(s) = sugg.get(self.sel) {
                        self.add(s.clone());
                        return None;
                    }
                }
                self.submit();
            }
            KeyCode::Tab => self.accept_suggestion(),
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => {
                let n = if self.input.is_empty() {
                    self.tags.len()
                } else {
                    self.suggestions().len()
                };
                if self.sel + 1 < n {
                    self.sel += 1;
                }
            }
            KeyCode::Backspace | KeyCode::Delete if self.input.is_empty() => self.remove_selected(),
            KeyCode::Backspace => {
                self.input.pop();
                self.sel = 0;
            }
            // Space or comma ends a tag, like enter.
            KeyCode::Char(' ') | KeyCode::Char(',') if !ctrl => {
                if !self.input.trim().is_empty() {
                    self.submit();
                }
            }
            KeyCode::Char('#') if !ctrl && self.input.is_empty() => {}
            KeyCode::Char(c) if !ctrl => {
                if self.input.is_empty() {
                    self.sel = 0;
                }
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
        let tags: Vec<String> = s["tags"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let ed = TagEditor::new(
            s["id"].as_str().unwrap_or(""),
            s["name"].as_str().unwrap_or("?"),
            tags,
            self.known_tags(),
        );
        self.mode = Mode::Tags(Box::new(ed));
    }

    /// Every tag in use across the sessions rlord has loaded, with counts.
    fn known_tags(&self) -> Vec<(String, usize)> {
        let mut m: std::collections::BTreeMap<String, usize> = Default::default();
        for s in &self.all {
            for t in s["tags"].as_array().into_iter().flatten() {
                if let Some(t) = t.as_str() {
                    *m.entry(t.to_string()).or_default() += 1;
                }
            }
        }
        for (t, n) in &self.groups {
            if t != groups::UNTAGGED {
                let e = m.entry(t.clone()).or_default();
                *e = (*e).max(*n);
            }
        }
        m.into_iter().collect()
    }

    pub(crate) fn handle_tags_form(&mut self, k: KeyEvent, mut ed: Box<TagEditor>) {
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
        let sugg = ed.suggestions();
        let h = (ed.tags.len().max(1) + sugg.len() + 10) as u16;
        let area = newform::centered(f.area(), 60, h.min(f.area().height));
        f.render_widget(Clear, area);
        let mut lines = vec![Line::from(Span::styled(
            "  Tags (= groups)",
            Style::default().bold(),
        ))];
        if ed.tags.is_empty() {
            lines.push(Line::from(Span::styled("    no tags", theme::faint())));
        }
        let list_focus = ed.input.is_empty() && ed.confirm_new.is_none();
        for (i, t) in ed.tags.iter().enumerate() {
            let on = list_focus && i == ed.sel;
            let mut spans = vec![Span::raw(if on { "  ▸ " } else { "    " })];
            spans.push(Span::styled(
                format!("#{t}"),
                Style::default().fg(theme::tag_color(t)),
            ));
            if !ed.original.contains(t) {
                spans.push(Span::styled("  (added)", theme::faint()));
            }
            let mut l = Line::from(spans);
            if on {
                l = l.style(theme::selected());
            }
            lines.push(l);
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  add: #", theme::muted()),
            Span::styled(format!("{}▏", ed.input), Style::default().fg(theme::text())),
        ]));
        for (i, s) in sugg.iter().enumerate() {
            let on = i == ed.sel;
            let l = Line::from(vec![
                Span::raw(if on { "    ▸ " } else { "      " }),
                Span::styled(format!("#{s}"), Style::default().fg(theme::tag_color(s))),
            ]);
            lines.push(if on { l.style(theme::selected()) } else { l });
        }
        lines.push(Line::from(""));
        if let Some(t) = &ed.confirm_new {
            lines.push(Line::from(Span::styled(
                format!("  #{t} is a new tag. Create it?"),
                Style::default().fg(theme::yellow()).bold(),
            )));
            lines.push(keys_line(&[
                ("y", "create", Cap::Context),
                ("n", "back", Cap::Global),
            ]));
        } else if ed.input.is_empty() {
            lines.push(keys_line(&[
                ("enter", "save", Cap::Context),
                ("⌫", "remove tag", Cap::Context),
                ("↑↓", "choose", Cap::Global),
                ("esc", "cancel", Cap::Global),
            ]));
        } else {
            lines.push(keys_line(&[
                ("enter", "add", Cap::Context),
                ("tab", "complete", Cap::Global),
                ("↑↓", "choose", Cap::Global),
                ("esc", "cancel", Cap::Global),
            ]));
            lines.push(keys_line(&[("ctrl-s", "save", Cap::Global)]));
        }
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

    fn typ(e: &mut TagEditor, s: &str) {
        for c in s.chars() {
            press(e, KeyCode::Char(c));
        }
    }

    #[test]
    fn suggestions_rank_prefix_then_substring_then_fuzzy() {
        // "w": prefix wip (5) before web (3); "tooling"? no w.
        assert_eq!(suggest("w", &known(), &[]), ["wip", "web"]);
        // "l": no prefix; substring release, tooling (count 2 first).
        assert_eq!(suggest("l", &known(), &[]), ["tooling", "release"]);
        // "tlg": only in order.
        assert_eq!(suggest("tlg", &known(), &[]), ["tooling"]);
        assert_eq!(suggest("#WE", &known(), &[]), ["web"]);
        assert!(suggest("", &known(), &[]).is_empty());
        assert!(suggest("zzz", &known(), &[]).is_empty());
        // Tags the session already has aren't offered.
        assert_eq!(suggest("w", &known(), &["wip".into()]), ["web"]);
    }

    #[test]
    fn known_tag_adds_unknown_asks() {
        let mut e = ed(&[]);
        typ(&mut e, "web");
        assert_eq!(e.submit(), AddOutcome::Added);
        assert_eq!(e.tags, ["web"]);
        typ(&mut e, "#Newthing");
        assert_eq!(e.submit(), AddOutcome::AskNew("newthing".into()));
        assert!(e.tags == ["web"]);
        // n goes back to the input with the text kept.
        press(&mut e, KeyCode::Char('n'));
        assert!(e.confirm_new.is_none());
        assert_eq!(e.input, "Newthing");
        press(&mut e, KeyCode::Enter);
        press(&mut e, KeyCode::Char('y'));
        assert_eq!(e.tags, ["web", "newthing"]);
        assert!(e.input.is_empty());
        // Duplicate: nothing.
        typ(&mut e, "web");
        assert_eq!(e.submit(), AddOutcome::Nothing);
        assert_eq!(e.tags.len(), 2);
    }

    #[test]
    fn partial_tag_is_never_saved_silently() {
        let mut e = ed(&["web"]);
        typ(&mut e, "too");
        // Enter on a partial unknown tag asks; it doesn't save.
        assert_eq!(press(&mut e, KeyCode::Enter), None);
        assert_eq!(e.confirm_new.as_deref(), Some("too"));
        // Esc in the question backs out, not the whole editor.
        assert_eq!(press(&mut e, KeyCode::Esc), None);
        // Tab completes it to the known tag.
        press(&mut e, KeyCode::Tab);
        assert_eq!(e.input, "tooling");
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.tags, ["web", "tooling"]);
        // Empty input + enter saves.
        assert_eq!(press(&mut e, KeyCode::Enter), Some(true));
        assert_eq!(e.diff(), (vec!["tooling".into()], vec![]));
    }

    #[test]
    fn arrows_pick_a_suggestion() {
        let mut e = ed(&[]);
        typ(&mut e, "w");
        press(&mut e, KeyCode::Down);
        press(&mut e, KeyCode::Enter);
        assert_eq!(e.tags, ["web"]);
    }

    #[test]
    fn remove_and_cancel() {
        let mut e = ed(&["web", "wip", "ui"]);
        press(&mut e, KeyCode::Up);
        press(&mut e, KeyCode::Backspace);
        assert_eq!(e.tags, ["web", "ui"]);
        assert!(e.changed());
        assert_eq!(e.diff(), (vec![], vec!["wip".into()]));
        assert_eq!(press(&mut e, KeyCode::Esc), Some(false));
        // Re-adding an original tag needs no confirmation.
        let mut e = ed(&["oddone"]);
        press(&mut e, KeyCode::Backspace);
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
