//! Color themes: rlord's own plus ports of popular terminal themes. Plain
//! data, shared by the TUI (ratatui colors) and the daemon (tmux colors for
//! the overlord's bar and overlay border).
//!
//! `theme` in the config is a family (`catppuccin`: follows the system's
//! light/dark appearance) or one variant (`catppuccin-latte`: fixed).

/// A color: 256-palette index (follows the terminal's palette) or RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rgb {
    Indexed(u8),
    Hex(u8, u8, u8),
}

impl Rgb {
    /// tmux's spelling (`colour141`, `#cba6f7`).
    pub fn tmux(self) -> String {
        match self {
            Rgb::Indexed(i) => format!("colour{i}"),
            Rgb::Hex(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        }
    }
}

const fn h(hex: u32) -> Rgb {
    Rgb::Hex((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}
const fn i(n: u8) -> Rgb {
    Rgb::Indexed(n)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub name: &'static str,
    pub dark: bool,
    /// Painted behind everything; None keeps the terminal's own background.
    pub bg: Option<Rgb>,
    /// Selected row.
    pub surface: Rgb,
    /// Chips: tabs, keycaps.
    pub chip: Rgb,
    pub border: Rgb,
    pub faint: Rgb,
    pub muted: Rgb,
    pub text: Rgb,
    /// Text on accent/colored chips.
    pub on_accent: Rgb,
    pub accent: Rgb,
    pub overlord: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub red: Rgb,
    pub cyan: Rgb,
    pub orange: Rgb,
    pub pink: Rgb,
}

impl Palette {
    /// Colors handed out to groups (`#tag` chips), stable per tag.
    pub fn tag_colors(&self) -> [Rgb; 8] {
        [
            self.accent,
            self.green,
            self.yellow,
            self.overlord,
            self.cyan,
            self.orange,
            self.pink,
            self.red,
        ]
    }
}

pub const THEMES: &[Palette] = &[
    Palette {
        name: "rlord-dark",
        dark: true,
        bg: None,
        surface: i(236),
        chip: i(237),
        border: i(238),
        faint: i(240),
        muted: i(245),
        text: i(252),
        on_accent: i(16),
        accent: i(75),
        overlord: i(141),
        green: i(114),
        yellow: i(221),
        red: i(203),
        cyan: i(80),
        orange: i(209),
        pink: i(176),
    },
    Palette {
        name: "rlord-light",
        dark: false,
        bg: None,
        surface: i(254),
        chip: i(253),
        border: i(250),
        faint: i(246),
        muted: i(242),
        text: i(236),
        on_accent: i(231),
        accent: i(26),
        overlord: i(91),
        green: i(28),
        yellow: i(136),
        red: i(160),
        cyan: i(30),
        orange: i(166),
        pink: i(162),
    },
    Palette {
        name: "catppuccin-mocha",
        dark: true,
        bg: Some(h(0x1e1e2e)),
        surface: h(0x313244),
        chip: h(0x45475a),
        border: h(0x45475a),
        faint: h(0x6c7086),
        muted: h(0xa6adc8),
        text: h(0xcdd6f4),
        on_accent: h(0x1e1e2e),
        accent: h(0x89b4fa),
        overlord: h(0xcba6f7),
        green: h(0xa6e3a1),
        yellow: h(0xf9e2af),
        red: h(0xf38ba8),
        cyan: h(0x94e2d5),
        orange: h(0xfab387),
        pink: h(0xf5c2e7),
    },
    Palette {
        name: "catppuccin-latte",
        dark: false,
        bg: Some(h(0xeff1f5)),
        surface: h(0xccd0da),
        chip: h(0xbcc0cc),
        border: h(0xbcc0cc),
        faint: h(0x9ca0b0),
        muted: h(0x6c6f85),
        text: h(0x4c4f69),
        on_accent: h(0xeff1f5),
        accent: h(0x1e66f5),
        overlord: h(0x8839ef),
        green: h(0x40a02b),
        yellow: h(0xdf8e1d),
        red: h(0xd20f39),
        cyan: h(0x179299),
        orange: h(0xfe640b),
        pink: h(0xea76cb),
    },
    Palette {
        name: "tokyonight-night",
        dark: true,
        bg: Some(h(0x1a1b26)),
        surface: h(0x292e42),
        chip: h(0x3b4261),
        border: h(0x3b4261),
        faint: h(0x565f89),
        muted: h(0x737aa2),
        text: h(0xc0caf5),
        on_accent: h(0x1a1b26),
        accent: h(0x7aa2f7),
        overlord: h(0xbb9af7),
        green: h(0x9ece6a),
        yellow: h(0xe0af68),
        red: h(0xf7768e),
        cyan: h(0x7dcfff),
        orange: h(0xff9e64),
        pink: h(0x1abc9c),
    },
    Palette {
        name: "tokyonight-day",
        dark: false,
        bg: Some(h(0xe1e2e7)),
        surface: h(0xc4c8da),
        chip: h(0xb7c1e3),
        border: h(0xa8aecb),
        faint: h(0x848cb5),
        muted: h(0x68709a),
        text: h(0x3760bf),
        on_accent: h(0xe1e2e7),
        accent: h(0x2e7de9),
        overlord: h(0x9854f1),
        green: h(0x587539),
        yellow: h(0x8c6c3e),
        red: h(0xf52a65),
        cyan: h(0x007197),
        orange: h(0xb15c00),
        pink: h(0x118c74),
    },
    Palette {
        name: "gruvbox-dark",
        dark: true,
        bg: Some(h(0x282828)),
        surface: h(0x3c3836),
        chip: h(0x504945),
        border: h(0x504945),
        faint: h(0x928374),
        muted: h(0xa89984),
        text: h(0xebdbb2),
        on_accent: h(0x282828),
        accent: h(0x83a598),
        overlord: h(0xd3869b),
        green: h(0xb8bb26),
        yellow: h(0xfabd2f),
        red: h(0xfb4934),
        cyan: h(0x8ec07c),
        orange: h(0xfe8019),
        pink: h(0xd3869b),
    },
    Palette {
        name: "gruvbox-light",
        dark: false,
        bg: Some(h(0xfbf1c7)),
        surface: h(0xebdbb2),
        chip: h(0xd5c4a1),
        border: h(0xd5c4a1),
        faint: h(0x928374),
        muted: h(0x7c6f64),
        text: h(0x3c3836),
        on_accent: h(0xfbf1c7),
        accent: h(0x076678),
        overlord: h(0x8f3f71),
        green: h(0x79740e),
        yellow: h(0xb57614),
        red: h(0x9d0006),
        cyan: h(0x427b58),
        orange: h(0xaf3a03),
        pink: h(0x8f3f71),
    },
    Palette {
        name: "solarized-dark",
        dark: true,
        bg: Some(h(0x002b36)),
        surface: h(0x073642),
        chip: h(0x0f4a57),
        border: h(0x0f4a57),
        faint: h(0x586e75),
        muted: h(0x839496),
        text: h(0x93a1a1),
        on_accent: h(0x002b36),
        accent: h(0x268bd2),
        overlord: h(0x6c71c4),
        green: h(0x859900),
        yellow: h(0xb58900),
        red: h(0xdc322f),
        cyan: h(0x2aa198),
        orange: h(0xcb4b16),
        pink: h(0xd33682),
    },
    Palette {
        name: "solarized-light",
        dark: false,
        bg: Some(h(0xfdf6e3)),
        surface: h(0xeee8d5),
        chip: h(0xe4ddc8),
        border: h(0xddd6c1),
        faint: h(0x93a1a1),
        muted: h(0x657b83),
        text: h(0x586e75),
        on_accent: h(0xfdf6e3),
        accent: h(0x268bd2),
        overlord: h(0x6c71c4),
        green: h(0x859900),
        yellow: h(0xb58900),
        red: h(0xdc322f),
        cyan: h(0x2aa198),
        orange: h(0xcb4b16),
        pink: h(0xd33682),
    },
    Palette {
        name: "nord",
        dark: true,
        bg: Some(h(0x2e3440)),
        surface: h(0x3b4252),
        chip: h(0x434c5e),
        border: h(0x434c5e),
        faint: h(0x616e88),
        muted: h(0x9aa5b8),
        text: h(0xeceff4),
        on_accent: h(0x2e3440),
        accent: h(0x88c0d0),
        overlord: h(0xb48ead),
        green: h(0xa3be8c),
        yellow: h(0xebcb8b),
        red: h(0xbf616a),
        cyan: h(0x8fbcbb),
        orange: h(0xd08770),
        pink: h(0x81a1c1),
    },
    Palette {
        name: "dracula",
        dark: true,
        bg: Some(h(0x282a36)),
        surface: h(0x44475a),
        chip: h(0x44475a),
        border: h(0x44475a),
        faint: h(0x6272a4),
        muted: h(0x9ea8c7),
        text: h(0xf8f8f2),
        on_accent: h(0x282a36),
        accent: h(0xbd93f9),
        overlord: h(0xff79c6),
        green: h(0x50fa7b),
        yellow: h(0xf1fa8c),
        red: h(0xff5555),
        cyan: h(0x8be9fd),
        orange: h(0xffb86c),
        pink: h(0xff79c6),
    },
];

/// Families with a dark and a light variant; `theme = "<family>"` follows
/// the system appearance. Single-variant themes (nord, dracula) are used in
/// both.
const FAMILIES: &[(&str, &str, &str)] = &[
    ("rlord", "rlord-dark", "rlord-light"),
    ("catppuccin", "catppuccin-mocha", "catppuccin-latte"),
    ("tokyonight", "tokyonight-night", "tokyonight-day"),
    ("gruvbox", "gruvbox-dark", "gruvbox-light"),
    ("solarized", "solarized-dark", "solarized-light"),
];

/// Every name `theme` accepts.
pub fn names() -> Vec<&'static str> {
    FAMILIES
        .iter()
        .map(|f| f.0)
        .chain(THEMES.iter().map(|t| t.name))
        .collect()
}

pub fn by_name(name: &str) -> Option<&'static Palette> {
    THEMES.iter().find(|t| t.name == name)
}

/// Whether `theme` changes with the system appearance (a family name).
pub fn follows_system(theme: &str) -> bool {
    FAMILIES.iter().any(|f| f.0 == theme.trim())
}

/// The palette for config `theme` under the given appearance. Unknown names
/// fall back to rlord's own.
pub fn resolve(theme: &str, dark: bool) -> &'static Palette {
    let theme = theme.trim();
    let name = FAMILIES
        .iter()
        .find(|f| f.0 == theme)
        .map(|f| if dark { f.1 } else { f.2 })
        .unwrap_or(theme);
    by_name(name)
        .unwrap_or_else(|| by_name(if dark { "rlord-dark" } else { "rlord-light" }).unwrap())
}

/// Whether the system is in dark mode. macOS: the global
/// `AppleInterfaceStyle` (absent = light). `RLORD_APPEARANCE=light|dark`
/// overrides; elsewhere, dark.
pub fn system_is_dark() -> bool {
    match std::env::var("RLORD_APPEARANCE").ok().as_deref() {
        Some("light") => return false,
        Some("dark") => return true,
        _ => {}
    }
    if cfg!(target_os = "macos") {
        return std::process::Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .stderr(std::process::Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "Dark")
            .unwrap_or(true);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_follow_the_appearance_and_names_resolve() {
        assert_eq!(resolve("catppuccin", true).name, "catppuccin-mocha");
        assert_eq!(resolve("catppuccin", false).name, "catppuccin-latte");
        assert_eq!(resolve("gruvbox-light", true).name, "gruvbox-light");
        assert_eq!(resolve("nord", false).name, "nord");
        assert_eq!(resolve("no-such", false).name, "rlord-light");
        assert!(follows_system("rlord") && !follows_system("dracula"));
        for n in names() {
            let p = resolve(n, true);
            assert!(by_name(p.name).is_some(), "{n}");
        }
        assert_eq!(h(0xcba6f7).tmux(), "#cba6f7");
        assert_eq!(i(141).tmux(), "colour141");
    }
}
