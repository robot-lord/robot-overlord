//! An archived session's record, where a live session shows its pane: what
//! it did (the archive summary), its notes, and when.

use super::*;

impl App {
    pub(crate) fn draw_record(&self, f: &mut Frame, area: Rect, s: &Value) {
        let name = s["name"].as_str().unwrap_or("");
        let title = Line::from(vec![
            Span::raw(" "),
            Span::styled(name.to_string(), Style::default().fg(theme::text()).bold()),
            Span::styled(" · archived ", theme::muted()),
        ]);
        let head =
            |t: &str| Line::styled(t.to_string(), Style::default().fg(theme::accent()).bold());
        let body = |t: &str| -> Vec<Line<'static>> {
            t.lines()
                .map(|l| Line::styled(l.to_string(), Style::default().fg(theme::text())))
                .collect()
        };
        let mut lines = vec![head("What it did")];
        let summary = s["summary"].as_str().unwrap_or("").trim();
        if !summary.is_empty() {
            lines.extend(body(summary));
        } else if s["summarizing"] == true {
            lines.push(Line::styled("being written…", theme::muted()));
        } else {
            lines.push(Line::styled(
                format!(
                    "no summary yet: {}summarize writes one (one cheap model call), or ask the overlord",
                    self.keys.label(Action::Command)
                ),
                theme::muted(),
            ));
        }
        let notes = s["notes"].as_str().unwrap_or("").trim();
        if !notes.is_empty() {
            lines.push(Line::from(""));
            lines.push(head("Notes"));
            lines.extend(body(notes));
        }
        lines.push(Line::from(""));
        let ts = |k: &str| s[k].as_i64();
        let mut when = vec![format!("archived {}", rel_age(ts("archived_at")))];
        if let Some(c) = ts("created_at") {
            when.push(format!("started {}", fmt_ts(c)));
        }
        if let Some(u) = updated_at(s) {
            when.push(format!("last message {}", rel_age(Some(u))));
        }
        lines.push(Line::styled(when.join(" · "), theme::faint()));
        lines.push(Line::styled(
            format!(
                "{} unarchives it · rlord search finds what it said",
                self.keys.label(Action::Unarchive)
            ),
            theme::faint(),
        ));
        let block = self.archived_tint(theme::panel(title, false));
        f.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
    }
}
