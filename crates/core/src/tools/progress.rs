//! Percentages in the progress output of the maintenance tools.

use crate::win::console_text::OutputEvent;

fn is_percent_sign(c: char) -> bool {
    c == '%' || c == '\u{FF05}'
}

/// A space that may separate a number from its percent sign: ASCII, no-break or narrow
/// no-break (as French and other locales write "45 %").
fn is_percent_space(c: char) -> bool {
    c == ' ' || c == '\u{00A0}' || c == '\u{202F}'
}

/// The last percentage between 0 and 100 in `text`, if any. A number with a '.' or ','
/// decimal separator counts when a percent sign ('%' or the fullwidth '％') follows it or
/// comes right before it, optionally with one space between them: "45%", "45 %", "45,5 %",
/// "%45" and "% 45" all read as percentages. Values above 100 are ignored.
pub fn percent(text: &str) -> Option<f32> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut last = None;
    let mut i = 0;
    while i < len {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < len && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i + 1 < len && (chars[i] == '.' || chars[i] == ',') && chars[i + 1].is_ascii_digit() {
            i += 1;
            while i < len && chars[i].is_ascii_digit() {
                i += 1;
            }
        }
        let end = i;

        let mut after = end;
        if after < len && is_percent_space(chars[after]) {
            after += 1;
        }
        let suffix = after < len && is_percent_sign(chars[after]);
        let prefix = match start {
            0 => false,
            1 => is_percent_sign(chars[0]),
            _ => {
                is_percent_sign(chars[start - 1])
                    || (is_percent_space(chars[start - 1]) && is_percent_sign(chars[start - 2]))
            }
        };
        if !(suffix || prefix) {
            continue;
        }
        let number: String = chars[start..end]
            .iter()
            .map(|&c| if c == ',' { '.' } else { c })
            .collect();
        if let Ok(value) = number.parse::<f32>() {
            if (0.0..=100.0).contains(&value) {
                last = Some(value);
            }
        }
    }
    last
}

/// Which output a percentage is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressRule {
    /// Progress segments, plus the one line that directly follows a progress segment (the
    /// last redraw of a progress bar, which ends with a line end). Other lines are ignored,
    /// so a report line such as defrag's "Total fragmented space = 5%" is not progress.
    Transient,
    /// Progress segments and every line.
    AnyLine,
}

/// Follows a tool's output events and reports the percentages its [`ProgressRule`] reads.
#[derive(Debug, Clone)]
pub struct ProgressTracker {
    rule: ProgressRule,
    after_progress: bool,
}

impl ProgressTracker {
    pub fn new(rule: ProgressRule) -> ProgressTracker {
        ProgressTracker {
            rule,
            after_progress: false,
        }
    }

    /// The percentage `event` reports under the rule, if any.
    pub fn observe(&mut self, event: &OutputEvent) -> Option<f32> {
        match event {
            OutputEvent::Progress(text) => {
                self.after_progress = true;
                percent(text)
            }
            OutputEvent::Line(text) => {
                let read = self.rule == ProgressRule::AnyLine || self.after_progress;
                self.after_progress = false;
                if read {
                    percent(text)
                } else {
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages_are_read_in_every_supported_form() {
        let cases: [(&str, Option<f32>); 16] = [
            ("Verification 45% complete.", Some(45.0)),
            ("[====  17.9%  ]", Some(17.9)),
            (
                "Progress: 1 of 9 done; Stage: 45%; Total: 30%; ETA: 0:00:10",
                Some(30.0),
            ),
            ("45 %", Some(45.0)),
            ("45\u{a0}%", Some(45.0)),
            ("45\u{202f}%", Some(45.0)),
            ("45,5 %", Some(45.5)),
            ("%45", Some(45.0)),
            ("% 45", Some(45.0)),
            ("45％", Some(45.0)),
            ("150%", None),
            ("no percent", None),
            ("100.0%", Some(100.0)),
            (
                "[==========================100.0%==========================]",
                Some(100.0),
            ),
            ("Stage 3 of 7", None),
            ("", None),
        ];
        for (text, expected) in cases {
            assert_eq!(percent(text), expected, "{text:?}");
        }
        // The last value within range wins; out-of-range values are skipped.
        assert_eq!(percent("20% then 150%"), Some(20.0));
        assert_eq!(percent("45  %"), None, "two spaces before the sign");
    }

    #[test]
    fn progress_follows_the_transient_rule() {
        let line = |t: &str| OutputEvent::Line(t.to_string());
        let progress = |t: &str| OutputEvent::Progress(t.to_string());
        let mut tracker = ProgressTracker::new(ProgressRule::Transient);
        assert_eq!(tracker.observe(&line("Total fragmented space = 5%")), None);
        assert_eq!(
            tracker.observe(&progress("Verification 12% complete.")),
            Some(12.0)
        );
        assert_eq!(tracker.observe(&progress("Working...")), None);
        assert_eq!(
            tracker.observe(&line("Verification 100% complete.")),
            Some(100.0)
        );
        assert_eq!(tracker.observe(&line("Fragmented space = 7%")), None);

        let mut any = ProgressTracker::new(ProgressRule::AnyLine);
        assert_eq!(any.observe(&line("Total fragmented space = 5%")), Some(5.0));
    }
}
