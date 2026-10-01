//! Output lines of a job kept in memory for polling, numbered from 1.

use std::collections::VecDeque;

use crate::tools::runner::MAX_JOB_LINES;

/// The newest [`MAX_JOB_LINES`] lines of a job; older ones live only in its log file.
#[derive(Debug, Default)]
pub(crate) struct Lines {
    pub(crate) kept: VecDeque<String>,
    /// Lines ever received; the newest kept line has this number.
    pub(crate) total: u64,
}

impl Lines {
    pub(crate) fn push_all(&mut self, lines: Vec<String>) {
        self.total += lines.len() as u64;
        let skip = lines.len().saturating_sub(MAX_JOB_LINES);
        for line in lines.into_iter().skip(skip) {
            if self.kept.len() == MAX_JOB_LINES {
                self.kept.pop_front();
            }
            self.kept.push_back(line);
        }
    }

    /// Up to `max` (at least one) lines numbered after `after`, as
    /// `(lines, first, next, skipped, more)`: the number of the first line returned (`None`
    /// when none is), the number of the last one (`after` when none is), the lines after
    /// `after` that were evicted, and whether more lines wait after this page.
    pub(crate) fn page(
        &self,
        after: u64,
        max: usize,
    ) -> (Vec<String>, Option<u64>, u64, u64, bool) {
        let after = after.min(self.total);
        let oldest = self.total + 1 - self.kept.len() as u64;
        let start = (after + 1).max(oldest);
        let skipped = start - (after + 1);
        let available = self.total + 1 - start;
        let count = available.min(max.max(1) as u64);
        let offset = (start - oldest) as usize;
        let lines: Vec<String> = self
            .kept
            .iter()
            .skip(offset)
            .take(count as usize)
            .cloned()
            .collect();
        let first = (count > 0).then_some(start);
        let next = if count > 0 { start + count - 1 } else { after };
        (lines, first, next, skipped, available > count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_pages_number_from_one_and_skip_evicted_lines() {
        let mut lines = Lines::default();
        assert_eq!(lines.page(0, 10), (vec![], None, 0, 0, false));
        lines.push_all((1..=5).map(|i| i.to_string()).collect());
        assert_eq!(
            lines.page(0, 2),
            (vec!["1".into(), "2".into()], Some(1), 2, 0, true)
        );
        assert_eq!(
            lines.page(2, 10),
            (
                vec!["3".into(), "4".into(), "5".into()],
                Some(3),
                5,
                0,
                false
            )
        );
        assert_eq!(lines.page(5, 10), (vec![], None, 5, 0, false));
        assert_eq!(lines.page(99, 10), (vec![], None, 5, 0, false));
        lines.push_all(
            (6..=MAX_JOB_LINES as u64 + 10)
                .map(|i| i.to_string())
                .collect(),
        );
        let (page, first, next, skipped, more) = lines.page(3, 1);
        assert_eq!((first, next, skipped, more), (Some(11), 11, 7, true));
        assert_eq!(page, ["11"]);
        assert_eq!(lines.kept.len(), MAX_JOB_LINES);
    }

    #[test]
    fn a_batch_larger_than_the_cap_keeps_its_newest_lines() {
        let mut lines = Lines::default();
        lines.push_all(
            (1..=MAX_JOB_LINES as u64 * 2)
                .map(|i| i.to_string())
                .collect(),
        );
        assert_eq!(lines.total, MAX_JOB_LINES as u64 * 2);
        assert_eq!(lines.kept.len(), MAX_JOB_LINES);
        assert_eq!(
            lines.kept.front().map(String::as_str),
            Some((MAX_JOB_LINES + 1).to_string().as_str())
        );
    }
}
