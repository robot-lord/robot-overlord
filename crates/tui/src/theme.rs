//! The TUI's look in one place: the active palette (from config `theme`,
//! following the system's light/dark mode for theme families), rounded
//! panels whose border lights up when focused, status and tag chips. Every
//! view draws through these so the app looks like one thing.

use std::sync::RwLock;

use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding};
use rlord_core::themes::{self, Palette, Rgb};

static ACTIVE: RwLock<Option<&'static Palette>> = RwLock::new(None);

/// Switch the palette everything draws with.
pub fn set(p: &'static Palette) {
    *ACTIVE.write().expect("theme lock") = Some(p);
}

pub fn palette() -> &'static Palette {
    ACTIVE
        .read()
        .expect("theme lock")
        .unwrap_or_else(|| themes::by_name("rlord-dark").expect("built-in theme"))
}

pub fn color(c: Rgb) -> Color {
    match c {
        Rgb::Indexed(n) => Color::Indexed(n),
        Rgb::Hex(r, g, b) => Color::Rgb(r, g, b),
    }
}

macro_rules! colors {
    ($($f:ident),*) => {$(
        pub fn $f() -> Color {
            color(palette().$f)
        }
    )*};
}
colors!(surface, chip, border, text, on_accent, accent, overlord, green, yellow, red, cyan);

pub fn faint_c() -> Color {
    color(palette().faint)
}

pub fn muted_c() -> Color {
    color(palette().muted)
}

/// The background to paint, if the theme has one.
pub fn bg() -> Option<Color> {
    palette().bg.map(color)
}

/// Base style: theme text on theme background.
pub fn base() -> Style {
    let s = Style::default().fg(text());
    match bg() {
        Some(b) => s.bg(b),
        None => s,
    }
}

pub fn muted() -> Style {
    Style::default().fg(muted_c())
}

/// The logo's colours, the same in every theme: (crown: dark gold, robot:
/// dark red).
pub fn logo() -> (Color, Color) {
    (Color::Rgb(201, 150, 24), Color::Rgb(150, 28, 36))
}

pub fn faint() -> Style {
    Style::default().fg(faint_c())
}

/// A rounded panel. Focused panels get an accent border and title.
pub fn panel<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    let (border, title_style) = if focused {
        (
            Style::default().fg(accent()),
            Style::default().fg(accent()).bold(),
        )
    } else {
        (
            Style::default().fg(border()),
            Style::default().fg(text()).bold(),
        )
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(title.into().style(title_style))
        .padding(Padding::horizontal(1))
        .style(base())
}

/// A modal dialog: rounded, accent border, title.
pub fn modal<'a>(title: &str) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent()))
        .title(Line::from(format!(" {title} ")).style(Style::default().fg(accent()).bold()))
        .padding(Padding::horizontal(1))
        .style(base())
}

/// Panel title text with breathing room.
pub fn title(text: &str) -> String {
    format!(" {text} ")
}

/// How a status reads: glyph, short label, color.
pub fn status_parts(status: &str) -> (&'static str, &'static str, Color) {
    match status {
        "working" => ("●", "working", green()),
        "waiting_input" => ("◐", "waiting", yellow()),
        "idle" => ("○", "idle", cyan()),
        "error" => ("✕", "error", red()),
        "starting" => ("◌", "starting", overlord()),
        "stopped" => ("■", "stopped", faint_c()),
        _ => ("·", "unknown", faint_c()),
    }
}

/// `● working`, colored.
pub fn status_chip(status: &str) -> Span<'static> {
    let (glyph, label, color) = status_parts(status);
    Span::styled(format!("{glyph} {label}"), Style::default().fg(color))
}

/// A stable color per tag, so a group reads the same everywhere.
pub fn tag_color(tag: &str) -> Color {
    let palette = palette().tag_colors();
    let h = tag
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33) ^ u32::from(b));
    color(palette[h as usize % palette.len()])
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
    Style::default().bg(surface()).add_modifier(Modifier::BOLD)
}

/// The bar drawn left of the selected row.
pub const SELECT_BAR: &str = "▌ ";
