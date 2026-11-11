//! The TUI's look in one place: a soft 256-color palette, rounded panels
//! whose border lights up when focused, status chips and tag chips. Every
//! view draws through these so the app looks like one thing.

use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding};

pub const ACCENT: Color = Color::Indexed(75); // soft blue
pub const OVERLORD: Color = Color::Indexed(141); // lavender; the tmux bar and overlay border use colour141 too
pub const GREEN: Color = Color::Indexed(114);
pub const YELLOW: Color = Color::Indexed(221);
pub const RED: Color = Color::Indexed(203);
pub const CYAN: Color = Color::Indexed(80);
pub const TEXT: Color = Color::Indexed(252);
pub const MUTED: Color = Color::Indexed(245);
pub const FAINT: Color = Color::Indexed(240);
pub const BORDER: Color = Color::Indexed(238);
/// Selected row background.
pub const SURFACE: Color = Color::Indexed(236);
/// Chip background (tabs, keycaps).
pub const CHIP: Color = Color::Indexed(237);

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn faint() -> Style {
    Style::default().fg(FAINT)
}

/// A rounded panel. Focused panels get an accent border and title.
pub fn panel<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    let (border, title_style) = if focused {
        (
            Style::default().fg(ACCENT),
            Style::default().fg(ACCENT).bold(),
        )
    } else {
        (
            Style::default().fg(BORDER),
            Style::default().fg(TEXT).bold(),
        )
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(title.into().style(title_style))
        .padding(Padding::horizontal(1))
}

/// A modal dialog: rounded, accent border, title with a leading glyph.
pub fn modal<'a>(title: &str) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Line::from(format!(" {title} ")).style(Style::default().fg(ACCENT).bold()))
        .padding(Padding::horizontal(1))
}

/// Panel title text with breathing room.
pub fn title(text: &str) -> String {
    format!(" {text} ")
}

/// How a status reads: glyph, short label, color.
pub fn status_parts(status: &str) -> (&'static str, &'static str, Color) {
    match status {
        "working" => ("●", "working", GREEN),
        "waiting_input" => ("◐", "waiting", YELLOW),
        "idle" => ("○", "idle", CYAN),
        "error" => ("✕", "error", RED),
        "starting" => ("◌", "starting", OVERLORD),
        "stopped" => ("■", "stopped", FAINT),
        _ => ("·", "unknown", FAINT),
    }
}

/// `● working`, colored.
pub fn status_chip(status: &str) -> Span<'static> {
    let (glyph, label, color) = status_parts(status);
    Span::styled(format!("{glyph} {label}"), Style::default().fg(color))
}

/// A stable color per tag, so a group reads the same everywhere.
pub fn tag_color(tag: &str) -> Color {
    const PALETTE: [u8; 8] = [75, 114, 221, 141, 80, 209, 176, 150];
    let h = tag
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33) ^ u32::from(b));
    Color::Indexed(PALETTE[h as usize % PALETTE.len()])
}

/// `#tag` spans, space-separated.
pub fn tag_spans<'a>(tags: impl IntoIterator<Item = &'a str>) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for t in tags {
        if !out.is_empty() {
            out.push(Span::raw(" "));
        }
        out.push(Span::styled(
            format!("#{t}"),
            Style::default().fg(tag_color(t)),
        ));
    }
    out
}

/// Selected row: a lifted surface, bold.
pub fn selected() -> Style {
    Style::default().bg(SURFACE).add_modifier(Modifier::BOLD)
}

/// The bar drawn left of the selected row.
pub const SELECT_BAR: &str = "▌ ";
