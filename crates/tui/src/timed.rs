//! A dialog that opens by itself (on a refresh, not a key), shared by
//! `autoresume` and `importoffer`. Keys pressed before the user could
//! have seen it are dropped: the grace runs from when it is first drawn,
//! not from when it was opened (it may open just before the TUI hands
//! the terminal to tmux). Its height fits its lines as wrapped at the
//! box's real width, so the key lines never clip on a narrow terminal.

use super::*;
use ratatui::widgets::Clear;
use std::cell::Cell;

/// Keys pressed this soon after it is first drawn are dropped.
const GRACE: Duration = Duration::from_millis(800);

/// When the dialog was first drawn (none: not yet).
#[derive(Default)]
pub(crate) struct Shown(Cell<Option<Instant>>);

impl Shown {
    /// Whether keys count yet: drawn, and long enough ago.
    pub(crate) fn ready(&self) -> bool {
        self.0.get().is_some_and(|t| t.elapsed() >= GRACE)
    }

    fn drawn(&self) {
        if self.0.get().is_none() {
            self.0.set(Some(Instant::now()));
        }
    }

    /// (Tests: as if drawn long ago.)
    #[cfg(test)]
    pub(crate) fn seen_long_ago() -> Self {
        Shown(Cell::new(Some(Instant::now() - GRACE * 2)))
    }
}

/// Rows `lines` take word-wrapped at `inner` columns, as drawn.
fn rows(lines: &[Line<'static>], inner: u16) -> usize {
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(inner.max(1))
}

impl App {
    /// Draw the dialog `width` wide (less on a narrow screen), tall enough
    /// for `lines` as wrapped, and start its grace.
    pub(crate) fn draw_timed(
        &self,
        f: &mut Frame,
        shown: &Shown,
        width: u16,
        title: &str,
        lines: Vec<Line<'static>>,
    ) {
        let w = width.min(f.area().width);
        // (Inside the borders and the modal's one-column padding.)
        let h = rows(&lines, w.saturating_sub(4)) + 2;
        let area = newform::centered(f.area(), w, u16::try_from(h).unwrap_or(u16::MAX));
        f.render_widget(Clear, area);
        self.hits.add(f.area(), mouse::Hit::Outside);
        self.hits.add(area, mouse::Hit::Inside);
        // Keycaps where each line lands as wrapped. A line that wraps gets
        // none: its keys may sit on any of its rows, and a click must never
        // land on the wrong key (they still work from the keyboard).
        let mut y = area.y + 1;
        for line in &lines {
            let n = rows(std::slice::from_ref(line), w.saturating_sub(4));
            if n == 1 {
                self.hits.keycaps(area.x + 2, y, std::slice::from_ref(line));
            }
            y = y.saturating_add(u16::try_from(n).unwrap_or(u16::MAX));
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(theme::modal(title)),
            area,
        );
        shown.drawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grace_starts_when_drawn() {
        let s = Shown::default();
        assert!(!s.ready(), "never drawn: not ready however long it waits");
        s.drawn();
        assert!(!s.ready(), "just drawn");
        assert!(Shown::seen_long_ago().ready());
    }

    #[test]
    fn rows_wrap_at_the_real_width() {
        let l = vec![Line::from("x".repeat(70)), Line::from("")];
        assert_eq!(rows(&l, 70), 2);
        assert_eq!(rows(&l, 30), 4);
        // Word wrap: more rows than the width alone says.
        let w = vec![Line::from("aaaa bbbb cccc")];
        assert_eq!(rows(&w, 7), 3);
    }
}
