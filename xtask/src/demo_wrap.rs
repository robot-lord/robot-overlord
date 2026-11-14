//! `cargo xtask demo-wrap COLS`: stdin to stdout, each line wrapped at
//! word breaks to fit COLS columns. The demo (`scripts/demo/demo.sh`)
//! pipes its stand-in agents' screens through it when `cargo xtask
//! screenshots` asks for a phone width; without that the demo prints them
//! as written.
//!
//! Lines may carry SGR colour sequences: they take no width, and a style
//! still open where a line breaks is closed there and reopened on the next
//! piece. A bullet's continuation lines (`● `, `• `, `✻ `, `⎿  `, `└ `) are
//! indented under its text.

use std::io::Read;

use anyhow::{Context, Result};

use crate::cmd;

pub fn main(args: &[String]) -> Result<()> {
    let [cols] = args else {
        return cmd::usage("usage: cargo xtask demo-wrap COLS  (stdin to stdout)");
    };
    let cols: usize = cols.parse().context("COLS")?;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    print!("{}", wrap(&input, cols));
    Ok(())
}

/// Every line of `text` wrapped to `cols` (see the module doc).
pub fn wrap(text: &str, cols: usize) -> String {
    // A line indented under the previous one's text (its bullet's hang)
    // continues it: joined first, so the paragraph rewraps as a whole.
    let mut logical: Vec<String> = Vec::new();
    let mut under: Option<usize> = None;
    let (body, nl) = match text.strip_suffix('\n') {
        Some(b) => (b, "\n"),
        None => (text, ""),
    };
    for line in body.split('\n') {
        let (lead, hang) = shape(line);
        let joins =
            hang == 0 && under.is_some_and(|u| u > 0 && lead == u) && !line.trim().is_empty();
        match logical.last_mut() {
            Some(prev) if joins => {
                prev.push(' ');
                prev.push_str(line.trim_start_matches(' '));
            }
            _ => {
                logical.push(line.to_string());
                under = (hang > 0).then_some(lead + hang);
            }
        }
    }
    let lines: Vec<String> = logical
        .iter()
        .map(|l| wrap_line(l, cols).join("\n"))
        .collect();
    lines.join("\n") + nl
}

/// A line's indent and its bullet's width (`● `: 2).
fn shape(line: &str) -> (usize, usize) {
    let lead = line.len() - line.trim_start_matches(' ').len();
    let visible = strip(&line[lead..]);
    let hang = ["⎿  ", "● ", "• ", "✻ ", "└ ", "> "]
        .iter()
        .find(|b| visible.starts_with(**b))
        .map_or(0, |b| b.chars().count());
    (lead, hang)
}

/// `s` without its escape sequences.
fn strip(s: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in s.chars() {
        match (esc, c) {
            (true, c) if c.is_ascii_alphabetic() => esc = false,
            (true, _) => {}
            (false, '\x1b') => esc = true,
            (false, c) => out.push(c),
        }
    }
    out
}

/// Visible width: characters outside escape sequences.
fn width(s: &str) -> usize {
    let mut n = 0;
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if c.is_ascii_alphabetic() {
                esc = false;
            }
        } else if c == '\x1b' {
            esc = true;
        } else {
            n += 1;
        }
    }
    n
}

/// The SGR sequences in `s`, in order.
fn sgrs(s: &str) -> Vec<&str> {
    let mut v = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find('\x1b') {
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| c.is_ascii_alphabetic())
            .map_or(tail.len(), |e| e + 1);
        v.push(&tail[..end]);
        rest = &tail[end..];
    }
    v
}

fn wrap_line(line: &str, cols: usize) -> Vec<String> {
    if width(line) <= cols {
        return vec![line.to_string()];
    }
    let (lead, hang) = shape(line);
    let rest = &line[lead..];
    let indent = " ".repeat(lead);
    let more = " ".repeat(lead + hang);

    let mut lines = Vec::new();
    let mut cur = indent.clone();
    let mut cur_w = lead;
    let mut first_word = true;
    // The style open at this point, to carry across a break.
    let mut open: Option<String> = None;
    for word in rest.split(' ') {
        let ww = width(word);
        if !first_word && cur_w + 1 + ww > cols && cur_w > lead + hang {
            if open.is_some() {
                cur.push_str("\x1b[0m");
            }
            lines.push(std::mem::take(&mut cur));
            cur = more.clone();
            cur_w = lead + hang;
            if let Some(s) = &open {
                cur.push_str(s);
            }
        } else if !first_word {
            cur.push(' ');
            cur_w += 1;
        }
        cur.push_str(word);
        cur_w += ww;
        first_word = false;
        for s in sgrs(word) {
            open = if s == "\x1b[0m" || s == "\x1b[m" {
                None
            } else {
                Some(s.to_string())
            };
        }
    }
    lines.push(cur);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_lines_stay() {
        assert_eq!(wrap("a b\n\nc\n", 10), "a b\n\nc\n");
    }

    #[test]
    fn long_lines_wrap_under_their_bullet() {
        assert_eq!(
            wrap("● one two three four\n", 12),
            "● one two\n  three four\n"
        );
        assert_eq!(wrap("  plain words here", 10), "  plain\n  words\n  here");
    }

    #[test]
    fn continuation_lines_rewrap_with_their_paragraph() {
        assert_eq!(
            wrap("● aaa bbb\n  ccc\n\n> dd ee ff\n", 9),
            "● aaa bbb\n  ccc\n\n> dd ee\n  ff\n"
        );
        // Not under a bullet: lines stay apart.
        assert_eq!(wrap("  1. a\n  2. b\n", 20), "  1. a\n  2. b\n");
        assert_eq!(wrap("  • x y\n    z\n", 20), "  • x y z\n");
    }

    #[test]
    fn escapes_take_no_width_and_styles_carry() {
        let d = "\x1b[2m";
        let r = "\x1b[0m";
        assert_eq!(
            wrap(&format!("  {d}⎿  aaa bbb ccc{r}"), 12),
            format!("  {d}⎿  aaa bbb{r}\n     {d}ccc{r}")
        );
        // A bold name mid-line: closed and reopened only while open.
        assert_eq!(
            wrap(&format!("● x \x1b[1mnamed{r} y z"), 9),
            format!("● x \x1b[1mnamed{r}\n  y z")
        );
        assert_eq!(width(&format!("{d}abc{r}")), 3);
    }
}
