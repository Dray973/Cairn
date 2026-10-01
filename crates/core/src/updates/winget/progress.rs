//! How far one winget step is, read from its progress display.
//!
//! winget draws a download bar followed by "12.0 MB / 32.5 MB", and an install bar followed
//! by a percentage. What it writes while its output is redirected is not specified, so the
//! fraction is best effort: `None` leaves the progress indeterminate.

use crate::tools::progress::percent;

/// One word of a progress display.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    Number(f64),
    /// A size unit and its size in bytes.
    Unit(f64),
    Slash,
    Other,
}

fn unit_bytes(word: &str) -> Option<f64> {
    let bytes = match word.to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" | "kib" => 1024.0,
        "mb" | "mib" => 1024.0 * 1024.0,
        "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(bytes)
}

/// Splits `text` into numbers (with a '.' or ',' decimal separator), size units, slashes and
/// anything else, each with the text it was read from. Spaces, including no-break spaces,
/// only separate.
fn tokens(text: &str) -> Vec<(Token, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        let token = if c.is_whitespace() || c == '\u{00A0}' || c == '\u{202F}' {
            i += 1;
            continue;
        } else if c.is_ascii_digit() {
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i + 1 < chars.len()
                && (chars[i] == '.' || chars[i] == ',')
                && chars[i + 1].is_ascii_digit()
            {
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let number: String = chars[start..i]
                .iter()
                .map(|&c| if c == ',' { '.' } else { c })
                .collect();
            number.parse().map_or(Token::Other, Token::Number)
        } else if c.is_ascii_alphabetic() {
            while i < chars.len() && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            unit_bytes(&word).map_or(Token::Other, Token::Unit)
        } else if c == '/' {
            i += 1;
            Token::Slash
        } else {
            i += 1;
            Token::Other
        };
        out.push((token, chars[start..i].iter().collect()));
    }
    out
}

/// The last "A unit / B unit" pair of `text` (B, KB, MB, GB, KiB, MiB or GiB, any case) whose
/// total is above zero: its fraction (0 to 1), and the pair with each number and unit as
/// printed, separated by single spaces.
fn last_sizes(text: &str) -> Option<(f64, String)> {
    let tokens = tokens(text);
    tokens.windows(5).rev().find_map(|w| {
        let kinds = [w[0].0, w[1].0, w[2].0, w[3].0, w[4].0];
        let [Token::Number(a), Token::Unit(ua), Token::Slash, Token::Number(b), Token::Unit(ub)] =
            kinds
        else {
            return None;
        };
        let total = b * ub;
        (total > 0.0).then(|| {
            let sizes = format!("{} {} / {} {}", w[0].1, w[1].1, w[3].1, w[4].1);
            ((a * ua / total).clamp(0.0, 1.0), sizes)
        })
    })
}

/// The fraction (0 to 1) a progress display shows: the last "A unit / B unit" pair (B, KB,
/// MB, GB, KiB, MiB or GiB, any case), else the last percentage. `None` when it shows
/// neither.
pub fn step_fraction(text: &str) -> Option<f64> {
    last_sizes(text)
        .map(|(fraction, _)| fraction)
        .or_else(|| percent(text).map(|p| f64::from(p) / 100.0))
}

/// The sizes a download's progress display shows, such as "12.0 MB / 32.5 MB": the pair
/// [`step_fraction`] reads its fraction from. `None` when the display shows no sizes.
pub fn step_sizes(text: &str) -> Option<String> {
    last_sizes(text).map(|(_, sizes)| sizes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(value: Option<f64>, expected: f64) {
        let value = value.expect("a fraction");
        assert!((value - expected).abs() < 1e-6, "{value} != {expected}");
    }

    #[test]
    fn sizes_are_read_with_either_decimal_separator() {
        close(
            step_fraction("  ██████▒▒▒▒  12.0 MB / 32.5 MB"),
            12.0 / 32.5,
        );
        close(step_fraction("12,0 MB / 32,5 MB"), 12.0 / 32.5);
        close(step_fraction("1.2 GB / 3.0 GB"), 0.4);
        close(step_fraction("512 KB / 1.0 MB"), 0.5);
        close(
            step_fraction("100\u{00A0}MiB\u{00A0}/\u{00A0}200\u{00A0}MiB"),
            0.5,
        );
        close(step_fraction("1 MB / 2 MB then 3 MB / 4 MB"), 0.75);
        close(step_fraction("5 MB / 4 MB"), 1.0);
    }

    #[test]
    fn sizes_are_kept_as_printed() {
        assert_eq!(
            step_sizes("  ██████▒▒▒▒  12.0 MB / 32.5 MB").as_deref(),
            Some("12.0 MB / 32.5 MB")
        );
        assert_eq!(
            step_sizes("12,0 MB / 32,5 MB").as_deref(),
            Some("12,0 MB / 32,5 MB")
        );
        assert_eq!(
            step_sizes("100\u{00A0}MiB\u{00A0}/\u{00A0}200\u{00A0}MiB").as_deref(),
            Some("100 MiB / 200 MiB")
        );
        assert_eq!(
            step_sizes("512KB/1.0MB").as_deref(),
            Some("512 KB / 1.0 MB")
        );
        // The pair the fraction is read from: the last one with a total.
        assert_eq!(
            step_sizes("1 MB / 2 MB then 3 MB / 4 MB").as_deref(),
            Some("3 MB / 4 MB")
        );
        assert_eq!(
            step_sizes("3 MB / 4 MB then 0 MB / 0 MB").as_deref(),
            Some("3 MB / 4 MB")
        );
        close(step_fraction("3 MB / 4 MB then 0 MB / 0 MB"), 0.75);
        assert_eq!(step_sizes("  ███████  45%"), None);
        assert_eq!(step_sizes("0 MB / 0 MB"), None);
        assert_eq!(step_sizes("12 apples / 30 pears"), None);
        assert_eq!(step_sizes(""), None);
    }

    #[test]
    fn percentages_are_the_fallback() {
        close(step_fraction("  ███████  45%"), 0.45);
        close(step_fraction("100 %"), 1.0);
    }

    #[test]
    fn anything_else_is_indeterminate() {
        assert_eq!(step_fraction("Starting package install..."), None);
        assert_eq!(step_fraction("0 MB / 0 MB"), None);
        assert_eq!(step_fraction("12 apples / 30 pears"), None);
        assert_eq!(step_fraction(""), None);
        assert_eq!(step_fraction("  - \\ | /"), None);
    }
}
