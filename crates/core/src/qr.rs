//! A QR code drawn from a string: the pairing offer (`RLORD1:…`) that
//! `rlord relay pair`, the TUI's Devices screen and the local page show.
//! It only draws: no network code, no keys (the offer comes from the
//! `rlord-web` plugin, which holds those).

use anyhow::{Context, Result};
use qrcode::{Color, EcLevel, QrCode};

/// A QR code's modules: `rows[y]` is a string of `1` (dark) and `0`
/// (light), square, without the quiet zone around it (whoever draws it
/// adds one: at least 4 modules on a screen, 2 in a terminal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Qr {
    pub rows: Vec<String>,
}

impl Qr {
    /// The code for `text`, with low error correction (it is shown on a
    /// screen, never printed, so the smallest code that holds it).
    pub fn new(text: &str) -> Result<Qr> {
        let code = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::L)
            .context("the text is too long for a QR code")?;
        let w = code.width();
        let rows = code
            .to_colors()
            .chunks(w)
            .map(|r| {
                r.iter()
                    .map(|c| if *c == Color::Dark { '1' } else { '0' })
                    .collect()
            })
            .collect();
        Ok(Qr { rows })
    }

    /// Modules a side.
    pub fn width(&self) -> usize {
        self.rows.len()
    }

    fn dark(&self, x: usize, y: usize) -> bool {
        self.rows
            .get(y)
            .and_then(|r| r.as_bytes().get(x))
            .is_some_and(|b| *b == b'1')
    }

    /// Terminal lines, two modules a character (`▀`, `▄`, `█`, space),
    /// with a quiet zone of `quiet` modules. The characters are the dark
    /// modules: draw them dark on a light background (`ANSI` below), so the
    /// code reads the same in a light or a dark terminal.
    pub fn half_blocks(&self, quiet: usize) -> Vec<String> {
        let w = self.width() + 2 * quiet;
        let at = |x: usize, y: usize| -> bool {
            x >= quiet && y >= quiet && self.dark(x - quiet, y - quiet)
        };
        (0..w.div_ceil(2))
            .map(|row| {
                let (top, bottom) = (row * 2, row * 2 + 1);
                (0..w)
                    .map(|x| {
                        let t = at(x, top);
                        let b = bottom < w && at(x, bottom);
                        match (t, b) {
                            (true, true) => '█',
                            (true, false) => '▀',
                            (false, true) => '▄',
                            (false, false) => ' ',
                        }
                    })
                    .collect()
            })
            .collect()
    }
}

/// ANSI: black on white, and back to normal (for `half_blocks` lines
/// printed to a terminal).
pub const ANSI_ON: &str = "\x1b[30;107m";
pub const ANSI_OFF: &str = "\x1b[0m";

#[cfg(test)]
mod tests {
    use super::*;

    /// A finder pattern (the 7×7 square in three corners) at (x, y).
    fn finder_at(q: &Qr, x: usize, y: usize) -> bool {
        (0..7).all(|dy| {
            (0..7).all(|dx| {
                let ring = dx == 0 || dy == 0 || dx == 6 || dy == 6;
                let core = (2..=4).contains(&dx) && (2..=4).contains(&dy);
                q.dark(x + dx, y + dy) == (ring || core)
            })
        })
    }

    #[test]
    fn a_known_string_gives_a_known_matrix() {
        let q = Qr::new("RLORD1:test").unwrap();
        // Version 1 (21 modules), low correction: fixed by the standard.
        assert_eq!(q.width(), 21);
        assert!(q.rows.iter().all(|r| r.len() == 21));
        let w = q.width();
        assert!(finder_at(&q, 0, 0));
        assert!(finder_at(&q, w - 7, 0));
        assert!(finder_at(&q, 0, w - 7));
        assert!(!finder_at(&q, w - 7, w - 7));
        // The timing pattern between the top finders alternates.
        let timing: String = (8..w - 8)
            .map(|x| q.rows[6].as_bytes()[x] as char)
            .collect();
        assert_eq!(timing, "10101");
        // The whole matrix, frozen from this encoder: a change here means
        // the encoder changed (a scanner still reads either).
        assert_eq!(
            q.rows,
            [
                "111111101111101111111",
                "100000101010101000001",
                "101110101000001011101",
                "101110101111001011101",
                "101110100000101011101",
                "100000101101001000001",
                "111111101010101111111",
                "000000000111100000000",
                "110011100101000101111",
                "001011011111101000011",
                "101010100101001011010",
                "001010000010010001110",
                "011110110010101100000",
                "000000001000101001111",
                "111111100010111011111",
                "100000101011100111111",
                "101110101101000001111",
                "101110100110101000111",
                "101110100011011011100",
                "100000101001010111101",
                "111111101100111110011",
            ]
        );
        // The same text, the same code; another text, another.
        assert_eq!(Qr::new("RLORD1:test").unwrap(), q);
        assert_ne!(Qr::new("RLORD1:tesu").unwrap(), q);
    }

    #[test]
    fn a_pairing_offer_fits_a_terminal() {
        // As long as `rlord_e2e`'s offer with a long origin.
        let offer = format!("RLORD1:{}", "A".repeat(200));
        let q = Qr::new(&offer).unwrap();
        let lines = q.half_blocks(2);
        assert!(q.width() <= 61, "{}", q.width());
        assert_eq!(lines.len(), (q.width() + 4).div_ceil(2));
        assert!(lines.iter().all(|l| l.chars().count() == q.width() + 4));
    }

    #[test]
    fn half_blocks_pair_rows_and_keep_the_quiet_zone() {
        let q = Qr {
            rows: vec!["10".into(), "11".into()],
        };
        assert_eq!(q.half_blocks(0), ["█▄"]);
        assert_eq!(q.half_blocks(1), [" ▄  ", " ▀▀ "]);
        let odd = Qr {
            rows: vec!["1".into()],
        };
        assert_eq!(odd.half_blocks(0), ["▀"]);
    }
}
