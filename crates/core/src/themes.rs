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

impl Palette {
    /// The overlord overlay's background: a faint wash of the overlord's
    /// colour over the theme's background, so the overlay reads as its own
    /// place without a heavy block of grey.
    pub fn overlay_bg(&self) -> Rgb {
        let base = match self.bg {
            Some(Rgb::Hex(r, g, b)) => (r, g, b),
            _ if self.dark => (20, 18, 26),
            _ => (250, 249, 252),
        };
        let Rgb::Hex(r, g, b) = self.overlord else {
            return if self.dark { h(0x1d1828) } else { h(0xf4effb) };
        };
        // About a tenth of the overlord colour.
        let mix = |a: u8, o: u8| ((a as u16 * 9 + o as u16) / 10) as u8;
        Rgb::Hex(mix(base.0, r), mix(base.1, g), mix(base.2, b))
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
    /// Behind the session list and details while the archived view is
    /// shown: a faint tint of the background (warm where the theme has
    /// RGB; on rlord-dark, a darker grey), so it is clearly not the
    /// live list. Kept off `surface` so the selected row still stands out.
    pub archived_bg: Rgb,
    /// Chips: tabs, keycaps.
    pub chip: Rgb,
    pub border: Rgb,
    pub faint: Rgb,
    pub muted: Rgb,
    pub text: Rgb,
    /// Text on accent/colored chips.
    pub on_accent: Rgb,
    pub accent: Rgb,
    /// The accent, quieter: list headers whose content can be switched
    /// (the time and Usage columns) while they aren't the sort, and the
    /// toolbar controls that switch them.
    pub accent_dim: Rgb,
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
        archived_bg: i(234),
        chip: i(237),
        border: i(238),
        faint: i(240),
        muted: i(245),
        text: i(252),
        on_accent: i(16),
        accent: i(75),
        accent_dim: i(67),
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
        // Painted: a terminal left dark while macOS is light (kitty doesn't
        // follow the system by default) would otherwise show light-theme
        // text on black.
        bg: Some(i(255)),
        surface: i(254),
        archived_bg: i(230),
        chip: i(253),
        border: i(250),
        faint: i(246),
        muted: i(242),
        text: i(236),
        on_accent: i(231),
        accent: i(26),
        accent_dim: i(67),
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
        archived_bg: h(0x292834),
        chip: h(0x45475a),
        border: h(0x45475a),
        faint: h(0x6c7086),
        muted: h(0xa6adc8),
        text: h(0xcdd6f4),
        on_accent: h(0x1e1e2e),
        accent: h(0x89b4fa),
        accent_dim: h(0x7a92c0),
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
        archived_bg: h(0xede7df),
        chip: h(0xbcc0cc),
        border: h(0xbcc0cc),
        faint: h(0x9ca0b0),
        muted: h(0x6c6f85),
        text: h(0x4c4f69),
        on_accent: h(0xeff1f5),
        accent: h(0x1e66f5),
        accent_dim: h(0x456abd),
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
        archived_bg: h(0x242229),
        chip: h(0x3b4261),
        border: h(0x3b4261),
        faint: h(0x565f89),
        muted: h(0x737aa2),
        text: h(0xc0caf5),
        on_accent: h(0x1a1b26),
        accent: h(0x7aa2f7),
        accent_dim: h(0x6880c0),
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
        archived_bg: h(0xd8d6d6),
        chip: h(0xb7c1e3),
        border: h(0xa8aecb),
        faint: h(0x848cb5),
        muted: h(0x68709a),
        text: h(0x3760bf),
        on_accent: h(0xe1e2e7),
        accent: h(0x2e7de9),
        accent_dim: h(0x4b76c1),
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
        archived_bg: h(0x322f28),
        chip: h(0x504945),
        border: h(0x504945),
        faint: h(0x928374),
        muted: h(0xa89984),
        text: h(0xebdbb2),
        on_accent: h(0x282828),
        accent: h(0x83a598),
        accent_dim: h(0x458588),
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
        archived_bg: h(0xf4e5b5),
        chip: h(0xd5c4a1),
        border: h(0xd5c4a1),
        faint: h(0x928374),
        muted: h(0x7c6f64),
        text: h(0x3c3836),
        on_accent: h(0xfbf1c7),
        accent: h(0x076678),
        accent_dim: h(0x458588),
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
        archived_bg: h(0x093033),
        chip: h(0x0f4a57),
        border: h(0x0f4a57),
        faint: h(0x586e75),
        muted: h(0x839496),
        text: h(0x93a1a1),
        on_accent: h(0x002b36),
        accent: h(0x268bd2),
        accent_dim: h(0x3f7ca3),
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
        archived_bg: h(0xf6ebcc),
        chip: h(0xe4ddc8),
        border: h(0xddd6c1),
        faint: h(0x93a1a1),
        muted: h(0x657b83),
        text: h(0x586e75),
        on_accent: h(0xfdf6e3),
        accent: h(0x268bd2),
        accent_dim: h(0x4583aa),
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
        archived_bg: h(0x373c44),
        chip: h(0x434c5e),
        border: h(0x434c5e),
        faint: h(0x616e88),
        muted: h(0x9aa5b8),
        text: h(0xeceff4),
        on_accent: h(0x2e3440),
        accent: h(0x88c0d0),
        accent_dim: h(0x5e81ac),
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
        archived_bg: h(0x32343a),
        chip: h(0x44475a),
        border: h(0x44475a),
        faint: h(0x6272a4),
        muted: h(0x9ea8c7),
        text: h(0xf8f8f2),
        on_accent: h(0x282a36),
        accent: h(0xbd93f9),
        accent_dim: h(0x9082ce),
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

/// Whether the system is in dark mode. `RLORD_APPEARANCE=light|dark`
/// overrides. macOS: the global `AppleInterfaceStyle` (absent = light).
/// Linux: GNOME's `color-scheme` setting (`prefer-dark`), read with
/// `gsettings` when it's there. Otherwise, dark.
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
    let gsettings = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match gsettings {
        // `'default'` doesn't say light: many desktops leave it there.
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout);
            !v.contains("prefer-light")
        }
        _ => true,
    }
}

/// A shell command that succeeds when the system is in dark mode, for
/// tmux's `if-shell` (the same checks as [`system_is_dark`]).
pub fn dark_shell_check() -> &'static str {
    if cfg!(target_os = "macos") {
        "defaults read -g AppleInterfaceStyle 2>/dev/null | grep -q Dark"
    } else {
        "! gsettings get org.gnome.desktop.interface color-scheme 2>/dev/null | grep -q prefer-light"
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn overlay_background_is_a_faint_overlord_wash() {
        for name in ["rlord", "rlord-light"] {
            for dark in [true, false] {
                let p = resolve(name, dark);
                let Rgb::Hex(r, g, b) = p.overlay_bg() else {
                    panic!("hex expected");
                };
                // Near the background, not a loud colour.
                let lum = (r as u32 + g as u32 + b as u32) / 3;
                assert!(if p.dark { lum < 70 } else { lum > 190 }, "{name}: {lum}");
            }
        }
    }

    use super::*;

    /// Approximate RGB of a colour (xterm's 256-colour palette for an
    /// index; the 16 base colours vary by terminal and aren't used here).
    fn approx(c: Rgb) -> (i32, i32, i32) {
        match c {
            Rgb::Hex(r, g, b) => (r as i32, g as i32, b as i32),
            Rgb::Indexed(n @ 16..=231) => {
                let lvl = |v: u8| if v == 0 { 0 } else { 55 + 40 * v as i32 };
                let n = n - 16;
                (lvl(n / 36), lvl(n / 6 % 6), lvl(n % 6))
            }
            Rgb::Indexed(n @ 232..=255) => {
                let v = 8 + 10 * (n as i32 - 232);
                (v, v, v)
            }
            Rgb::Indexed(n) => panic!("base colour {n} has no fixed RGB"),
        }
    }

    /// Every theme has an archived tint: visibly off the background (so it
    /// shows) and off the selected row's `surface` (so the selection still
    /// does), but near the background's lightness (subtle, text readable).
    /// rlord-dark has no background of its own: its tint stays as dark as
    /// a typical terminal's, under `surface`.
    #[test]
    fn every_theme_has_a_subtle_archived_tint() {
        let dist = |a: (i32, i32, i32), b: (i32, i32, i32)| {
            (a.0 - b.0).abs() + (a.1 - b.1).abs() + (a.2 - b.2).abs()
        };
        let lum = |c: (i32, i32, i32)| (c.0 + c.1 + c.2) / 3;
        assert_eq!(approx(i(234)), (28, 28, 28));
        assert_eq!(approx(i(230)), (255, 255, 215));
        for p in THEMES {
            let tint = approx(p.archived_bg);
            let surface = approx(p.surface);
            // A real gap to the selected row, or the selection vanishes.
            assert!(dist(tint, surface) >= 20, "{}: too near surface", p.name);
            match p.bg {
                Some(bg) => {
                    let bg = approx(bg);
                    assert!(dist(tint, bg) >= 9, "{}: too near bg", p.name);
                    assert!(dist(tint, bg) <= 70, "{}: too loud", p.name);
                }
                // No background of its own: at least a clear step of
                // lightness under the selected row.
                None => assert!(lum(surface) - lum(tint) >= 15, "{}", p.name),
            }
            let l = lum(tint);
            assert!(if p.dark { l < 70 } else { l > 190 }, "{}: {l}", p.name);
        }
    }

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
