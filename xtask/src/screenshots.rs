//! `cargo xtask screenshots --website DIR`: phone-sized screenshots of rlord
//! for the website, made the same way every time.
//!
//! It starts the throwaway demo (`scripts/demo/demo.sh`: made-up sessions,
//! stand-in agents, its own data, config and tmux server, no model), opens
//! the list in a phone-width terminal on a scratch tmux server, and captures
//! three views with `tmux capture-pane -e`: the session list, the groups tab
//! and the overlord's conversation. Each capture becomes a small HTML page
//! (`ansi_to_html`), which Playwright (the website's dev dependency)
//! screenshots as a phone would show it. The PNGs land in the output folder
//! and, as WebP, in the website's `public/img/phone-*.webp`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::cmd;

/// The terminal the views are captured in: about what a phone shows in
/// portrait with a small monospace font.
pub const COLS: u16 = 48;
pub const ROWS: u16 = 46;
/// The scratch tmux server the list runs in (never the user's default
/// server, never rlord's own `rlord`). The demo's sessions run on
/// `rlord-demo` (demo.sh).
const SHOTS_SOCKET: &str = "rlord-shots";
const DEMO_SOCKET: &str = "rlord-demo";
/// The phone Playwright emulates (viewport and pixel ratio).
const DEVICE: &str = "iPhone 15";

const USAGE: &str = "usage: cargo xtask screenshots --website DIR [--out DIR]";

pub fn main(args: &[String], invoked_from: &Path) -> Result<()> {
    let mut website = None;
    let mut out = PathBuf::from("target/screenshots");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--website" => website = it.next().map(|d| invoked_from.join(d)),
            "--out" => out = it.next().map(|d| invoked_from.join(d)).unwrap_or(out),
            _ => return cmd::usage(format!("unknown argument {a}\n{USAGE}")),
        }
    }
    let Some(website) = website else {
        return cmd::usage(USAGE);
    };
    if !website.join("node_modules/playwright").is_dir() {
        bail!(
            "{} has no Playwright: run `npm ci` there (and `npx playwright install webkit` once)",
            website.display()
        );
    }
    for tool in ["tmux", "cwebp", "npx", "sqlite3", "python3"] {
        if !cmd::on_path(tool) {
            bail!("needs {tool} on PATH");
        }
    }
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let out = std::fs::canonicalize(&out)?;

    let demo = Demo::start()?;
    let captures = demo.capture()?;
    drop(demo);

    let img = website.join("public/img");
    std::fs::create_dir_all(&img)?;
    for (name, ansi) in &captures {
        std::fs::write(out.join(format!("{name}.ansi")), ansi)?;
        let html = out.join(format!("{name}.html"));
        std::fs::write(&html, page(name, ansi))?;
        let png = out.join(format!("{name}.png"));
        cmd::run(
            Command::new("npx")
                .current_dir(&website)
                .args([
                    "playwright",
                    "screenshot",
                    "--device",
                    DEVICE,
                    "--full-page",
                ])
                .args(["--color-scheme", "dark"])
                .arg(format!("file://{}", html.display()))
                .arg(&png),
        )?;
        let webp = img.join(format!("phone-{name}.webp"));
        cmd::run(
            Command::new("cwebp")
                .args(["-quiet", "-q", "86", "-resize", "780", "0"])
                .arg(&png)
                .arg("-o")
                .arg(&webp),
        )?;
        println!("{}", webp.display());
    }
    Ok(())
}

/// The running demo, stopped (and its folder removed) when dropped.
struct Demo {
    dir: PathBuf,
}

impl Demo {
    fn start() -> Result<Self> {
        // A short path: the daemon's socket lives under it, and a unix
        // socket's path is limited to about 100 bytes.
        let dir = std::env::temp_dir().join(format!("rls{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let demo = Self { dir };
        cmd::run(
            Command::new("scripts/demo/demo.sh")
                .arg(&demo.dir)
                .env("DEMO_COLS", (COLS - 2).to_string())
                // The demo wraps its screens to DEMO_COLS with `demo-wrap`.
                .env("DEMO_XTASK", std::env::current_exe()?),
        )?;
        // The phone layout: the list on top, the live pane and a short
        // details pane (its name, state, description and groups) below.
        std::fs::write(
            demo.dir.join("config/robot-overlord/config.toml"),
            "layout = \"stack\"\ndetails_height = 7\n\n[overlord]\nmodel = \"opus\"\neffort = \"medium\"\n",
        )?;
        Ok(demo)
    }

    /// `sh -c` with the demo's environment (its env.sh) loaded.
    fn sh(&self, script: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg(format!(". \"$DEMO/env.sh\" && {script}"))
            .env("DEMO", &self.dir)
            .env("RLORD_APPEARANCE", "dark")
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor");
        c
    }

    fn shots(&self) -> Command {
        let mut c = Command::new("tmux");
        c.args(["-L", SHOTS_SOCKET]);
        c
    }

    fn keys(&self, keys: &[&str]) -> Result<()> {
        cmd::run(self.shots().args(["send-keys", "-t", "shots"]).args(keys))?;
        cmd::sleep(Duration::from_millis(1500))
    }

    fn capture(&self) -> Result<Vec<(&'static str, String)>> {
        // The list, in a terminal of the phone's size. (`rlord` runs its
        // list inside its own tmux session; this one is the terminal.)
        let _ = cmd::status_quiet(self.shots().arg("kill-server"));
        // (The server starts here, so its panes get the demo's environment.)
        cmd::run(
            self.sh(&format!(
                "exec tmux -L {SHOTS_SOCKET} -f /dev/null new-session -d -s shots -x {COLS} -y {ROWS} rlord"
            ))
            .stdin(std::process::Stdio::null()),
        )?;
        cmd::sleep(Duration::from_secs(4))?;
        let mut shots = Vec::new();
        // Past the first-run prompt; then a session that is waiting on you.
        self.keys(&["Escape"])?;
        self.keys(&["Down", "Down", "Down"])?;
        shots.push(("list", self.grab(SHOTS_SOCKET, "shots")?));
        self.keys(&["L"])?;
        shots.push(("groups", self.grab(SHOTS_SOCKET, "shots")?));

        // The overlord's own pane (the stand-in draws its conversation),
        // at the phone's width.
        // (`rlord ls` leaves the overlord out.)
        let tmux = cmd::output(&mut self.sh(
            "sqlite3 \"$XDG_DATA_HOME/robot-overlord/state.db\" \
             \"select tmux_session from sessions where role = 'overlord' and tmux_session is not null\"",
        ))?;
        let Some(tmux) = tmux.lines().next().map(str::to_string) else {
            bail!("the demo has no running overlord");
        };
        let demo_tmux = |args: &[&str]| {
            let mut c = Command::new("tmux");
            c.args(["-L", DEMO_SOCKET]).args(args);
            c
        };
        let target = format!("={tmux}:");
        cmd::run(&mut demo_tmux(&[
            "resize-window",
            "-t",
            &target,
            "-x",
            &COLS.to_string(),
            "-y",
            &ROWS.to_string(),
        ]))?;
        cmd::sleep(Duration::from_secs(1))?;
        shots.push(("overlord", self.grab(DEMO_SOCKET, &tmux)?));
        Ok(shots)
    }

    fn grab(&self, socket: &str, session: &str) -> Result<String> {
        let mut c = Command::new("tmux");
        c.args(["-L", socket, "capture-pane", "-e", "-p", "-t"])
            .arg(format!("={session}:"));
        // (Not cmd::output: it trims the trailing blank rows away, and the
        // capture keeps its full height.)
        let o = c.output().context("tmux capture-pane")?;
        if !o.status.success() {
            bail!("tmux capture-pane -t {session} failed");
        }
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    }
}

impl Drop for Demo {
    fn drop(&mut self) {
        let _ = cmd::status_quiet(self.shots().arg("kill-server"));
        let _ = cmd::status_quiet(
            Command::new("scripts/demo/demo.sh")
                .arg("--stop")
                .arg(&self.dir),
        );
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A capture as a page that fills a phone's width: `COLS` columns of
/// monospace across the viewport, on the terminal's black.
fn page(name: &str, ansi: &str) -> String {
    let body = ansi_to_html(ansi);
    format!(
        "<!doctype html>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>rlord {name}</title>\n<style>\n\
         html, body {{ margin: 0; background: {BG}; }}\n\
         pre {{ margin: 0; padding: 10px 8px; color: {FG};\n\
         font-family: Menlo, \"DejaVu Sans Mono\", monospace;\n\
         font-size: calc((100vw - 16px) / ({COLS} * 0.6021)); line-height: 1.22; }}\n\
         .b {{ font-weight: bold; }} .d {{ opacity: 0.6; }} .i {{ font-style: italic; }} .u {{ text-decoration: underline; }}\n\
         .c {{ display: inline-block; width: 1ch; }}\n\
         </style>\n<pre>{body}</pre>\n"
    )
}

/// The terminal's own colours (vhs's "Builtin Tango Dark", which the
/// README screenshots use).
const BG: &str = "#000000";
const FG: &str = "#ffffff";
const PALETTE: [&str; 16] = [
    "#000000", "#cc0000", "#4e9a06", "#c4a000", "#3465a4", "#75507b", "#06989a", "#d3d7cf",
    "#555753", "#ef2929", "#8ae234", "#fce94f", "#729fcf", "#ad7fa8", "#34e2e2", "#eeeeec",
];

#[derive(Clone, Default, PartialEq, Debug)]
struct Style {
    fg: Option<String>,
    bg: Option<String>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    reverse: bool,
}

impl Style {
    /// The span that opens this style, or None for the default.
    fn open(&self) -> Option<String> {
        let (mut fg, mut bg) = (self.fg.clone(), self.bg.clone());
        if self.reverse {
            (fg, bg) = (
                Some(bg.unwrap_or_else(|| BG.into())),
                Some(fg.unwrap_or_else(|| FG.into())),
            );
        }
        let mut class = Vec::new();
        for (on, c) in [
            (self.bold, "b"),
            (self.dim, "d"),
            (self.italic, "i"),
            (self.underline, "u"),
        ] {
            if on {
                class.push(c);
            }
        }
        let mut css = String::new();
        if let Some(c) = fg {
            css.push_str(&format!("color:{c};"));
        }
        if let Some(c) = bg {
            css.push_str(&format!("background:{c};"));
        }
        if class.is_empty() && css.is_empty() {
            return None;
        }
        let mut s = String::from("<span");
        if !class.is_empty() {
            s.push_str(&format!(" class=\"{}\"", class.join(" ")));
        }
        if !css.is_empty() {
            s.push_str(&format!(" style=\"{css}\""));
        }
        s.push('>');
        Some(s)
    }

    /// Apply one SGR sequence's parameters.
    fn apply(&mut self, params: &[u16]) {
        let mut i = 0;
        if params.is_empty() {
            *self = Style::default();
        }
        while i < params.len() {
            match params[i] {
                0 => *self = Style::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                7 => self.reverse = true,
                22 => (self.bold, self.dim) = (false, false),
                23 => self.italic = false,
                24 => self.underline = false,
                27 => self.reverse = false,
                n @ 30..=37 => self.fg = Some(PALETTE[(n - 30) as usize].into()),
                39 => self.fg = None,
                n @ 40..=47 => self.bg = Some(PALETTE[(n - 40) as usize].into()),
                49 => self.bg = None,
                n @ 90..=97 => self.fg = Some(PALETTE[(n - 90 + 8) as usize].into()),
                n @ 100..=107 => self.bg = Some(PALETTE[(n - 100 + 8) as usize].into()),
                n @ (38 | 48) => {
                    let color = match params.get(i + 1) {
                        Some(5) => {
                            let c = params.get(i + 2).map(|&c| color256(c));
                            i += 2;
                            c
                        }
                        Some(2) => {
                            let c = match params.get(i + 2..i + 5) {
                                Some([r, g, b]) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
                                _ => None,
                            };
                            i += 4;
                            c
                        }
                        _ => None,
                    };
                    if n == 38 {
                        self.fg = color;
                    } else {
                        self.bg = color;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// An xterm 256-colour index as CSS.
fn color256(n: u16) -> String {
    match n {
        0..=15 => PALETTE[n as usize].into(),
        16..=231 => {
            let n = n - 16;
            let level = |v: u16| if v == 0 { 0 } else { 55 + v * 40 };
            format!(
                "#{:02x}{:02x}{:02x}",
                level(n / 36),
                level(n / 6 % 6),
                level(n % 6)
            )
        }
        _ => {
            let v = 8 + (n.min(255) - 232) * 10;
            format!("#{v:02x}{v:02x}{v:02x}")
        }
    }
}

/// tmux's `capture-pane -e` output (text and SGR sequences) as HTML for a
/// `<pre>`: each styled run a span, the text escaped. Other escape
/// sequences are dropped.
pub fn ansi_to_html(ansi: &str) -> String {
    let mut out = String::new();
    let mut style = Style::default();
    let mut open = false;
    let mut chars = ansi.chars().peekable();
    let mut run = String::new();
    let flush = |out: &mut String, run: &mut String, style: &Style, open: &mut bool| {
        if run.is_empty() {
            return;
        }
        if let Some(tag) = style.open() {
            out.push_str(&tag);
            *open = true;
        }
        out.push_str(run);
        if *open {
            out.push_str("</span>");
            *open = false;
        }
        run.clear();
    };
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.peek() != Some(&'[') {
                    // Not a CSI sequence: skip its introducer (and a
                    // charset switch's charset).
                    if let Some('(' | ')') = chars.next() {
                        chars.next();
                    }
                    continue;
                }
                chars.next();
                let mut seq = String::new();
                let mut fin = None;
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        fin = Some(c);
                        break;
                    }
                    seq.push(c);
                }
                if fin == Some('m') {
                    flush(&mut out, &mut run, &style, &mut open);
                    let params: Vec<u16> = seq
                        .split([';', ':'])
                        .map(|p| p.parse().unwrap_or(0))
                        .collect();
                    style.apply(if seq.is_empty() { &[] } else { &params });
                }
            }
            '\n' => {
                flush(&mut out, &mut run, &style, &mut open);
                out.push('\n');
            }
            '&' => run.push_str("&amp;"),
            '<' => run.push_str("&lt;"),
            '>' => run.push_str("&gt;"),
            c if c.is_control() => {}
            // One cell each, whatever width the font that has it gives
            // it (a fallback font may draw `⎿` wider and push the row).
            c if !c.is_ascii() => {
                run.push_str("<span class=\"c\">");
                run.push(c);
                run.push_str("</span>");
            }
            c => run.push(c),
        }
    }
    flush(&mut out, &mut run, &style, &mut open);
    // tmux leaves the last row's newline on; the page needs no blank row.
    out.trim_end_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_escaped() {
        assert_eq!(ansi_to_html("a <b> & c\n"), "a &lt;b&gt; &amp; c");
    }

    #[test]
    fn styles_become_spans() {
        assert_eq!(
            ansi_to_html("\x1b[1mbold\x1b[0m plain"),
            "<span class=\"b\">bold</span> plain"
        );
        assert_eq!(
            ansi_to_html("\x1b[31mred\x1b[39m"),
            "<span style=\"color:#cc0000;\">red</span>"
        );
        assert_eq!(
            ansi_to_html("\x1b[38;2;1;2;255mx\x1b[48;5;196my\x1b[m"),
            "<span style=\"color:#0102ff;\">x</span>\
             <span style=\"color:#0102ff;background:#ff0000;\">y</span>"
        );
    }

    #[test]
    fn reverse_swaps_colours() {
        assert_eq!(
            ansi_to_html("\x1b[7mx"),
            format!("<span style=\"color:{BG};background:{FG};\">x</span>")
        );
    }

    #[test]
    fn a_style_carries_across_lines() {
        assert_eq!(
            ansi_to_html("\x1b[2ma\nb\x1b[0m"),
            "<span class=\"d\">a</span>\n<span class=\"d\">b</span>"
        );
    }

    #[test]
    fn non_ascii_gets_one_cell() {
        assert_eq!(ansi_to_html("a⎿"), "a<span class=\"c\">⎿</span>");
    }

    #[test]
    fn other_sequences_are_dropped() {
        assert_eq!(ansi_to_html("a\x1b[2Kb\x1b(Bc"), "abc");
    }

    #[test]
    fn colours_256() {
        assert_eq!(color256(1), "#cc0000");
        assert_eq!(color256(16), "#000000");
        assert_eq!(color256(231), "#ffffff");
        assert_eq!(color256(232), "#080808");
    }
}
