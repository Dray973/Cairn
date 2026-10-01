//! The table `winget upgrade` prints, read without its headers, footers or column widths.
//!
//! The table text is localized and its columns are sized to their contents, so rows are split
//! into whitespace-separated words and read from the right: source, available version,
//! installed version, id; the name is everything before the id. The installed package ids of
//! `winget export` serve as a dictionary to find the id among the words, which also covers
//! versions with spaces ("< 1.2", "2.0 (build 5)"). winget does not shorten ids when its
//! output is redirected; an id cut with "…" anyway is looked up in the installed packages.

use serde::Serialize;

use super::export::Inventory;

/// One app with an update, as the table lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableRow {
    pub name: String,
    pub id: String,
    pub installed: String,
    pub available: String,
    pub source: String,
    /// Listed under winget's "require explicit targeting" heading: winget updates it only
    /// when it is named.
    pub explicit_only: bool,
    /// The id ends with "…" and could not be completed from the installed packages.
    pub id_truncated: bool,
    /// Set when the app is installed more than once ("2 installations").
    pub note: Option<String>,
}

/// The rows of every table in the output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpgradeTable {
    pub rows: Vec<TableRow>,
    /// Lines that end in a source name, so they look like rows, but could not be read.
    pub unparsed: u32,
    /// Tables found (separator lines).
    pub tables: u32,
}

/// Sources every winget knows; the export adds the configured ones.
const BUILTIN_SOURCES: [&str; 2] = ["winget", "msstore"];
/// Characters no package id contains.
const FORBIDDEN_ID_CHARS: &str = "\\/:*?\"<>|";
const ELLIPSIS: char = '\u{2026}';

/// A line of at least 10 dashes and nothing else.
fn is_separator(line: &str) -> bool {
    let line = line.trim();
    line.chars().count() >= 10 && line.chars().all(|c| c == '-')
}

/// Whitespace-separated words of `line` with their byte offsets.
fn words(line: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in line.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = start.take() {
                out.push((s, &line[s..i]));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push((s, &line[s..]));
    }
    out
}

/// Whether `id` can be a package id of `source`. At most 128 characters, no whitespace,
/// control characters or `\ / : * ? " < > |`, not starting with '-'. A Microsoft Store id
/// is 12 to 14 letters and digits; any other is 2 to 8 non-empty parts separated by dots
/// (winget's manifest pattern, with the total length relaxed to 128).
pub fn valid_package_id(id: &str, source: &str) -> bool {
    let count = id.chars().count();
    if count == 0 || count > 128 || id.starts_with('-') {
        return false;
    }
    if id
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || FORBIDDEN_ID_CHARS.contains(c))
    {
        return false;
    }
    if source.eq_ignore_ascii_case("msstore") {
        return (12..=14).contains(&count) && id.chars().all(|c| c.is_ascii_alphanumeric());
    }
    let parts: Vec<&str> = id.split('.').collect();
    (2..=8).contains(&parts.len()) && parts.iter().all(|p| !p.is_empty())
}

/// `^[A-Za-z0-9._-]{1,64}$`.
pub fn valid_source(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

struct Reader<'a> {
    inventory: Option<&'a Inventory>,
    sources: Vec<String>,
}

impl Reader<'_> {
    fn known_source(&self, word: &str) -> bool {
        self.sources.iter().any(|s| s.eq_ignore_ascii_case(word))
    }

    /// The row a line holds, if it can be read.
    fn row(&self, line: &str, explicit_only: bool) -> Option<TableRow> {
        let words = words(line);
        let n = words.len();
        if n < 5 {
            return None;
        }
        let source = words[n - 1].1;
        let available = words[n - 2].1;
        if !self.known_source(source) {
            return None;
        }
        let make = |id: String, name: &str, installed: String, id_truncated: bool| TableRow {
            name: name.to_string(),
            id,
            installed,
            available: available.to_string(),
            source: source.to_string(),
            explicit_only,
            id_truncated,
            note: None,
        };
        if let Some(inventory) = self.inventory {
            for index in (1..=n - 3).rev() {
                let Some(package) = inventory.find(words[index].1) else {
                    continue;
                };
                let installed = words[index + 1..n - 2]
                    .iter()
                    .map(|w| w.1)
                    .collect::<Vec<_>>()
                    .join(" ");
                let name = line[..words[index].0].trim_end();
                if !name.is_empty() && !installed.is_empty() {
                    return Some(make(package.id.clone(), name, installed, false));
                }
            }
        }
        let (id_index, installed) = if matches!(words[n - 4].1, "<" | ">") {
            if n < 6 {
                return None;
            }
            (n - 5, format!("{} {}", words[n - 4].1, words[n - 3].1))
        } else {
            (n - 4, words[n - 3].1.to_string())
        };
        let id = words[id_index].1;
        let name = line[..words[id_index].0].trim_end();
        let truncated = id.ends_with(ELLIPSIS);
        let accepted = (truncated || valid_package_id(id, source))
            && available.bytes().any(|b| b.is_ascii_digit())
            && !name.is_empty();
        if !accepted {
            return None;
        }
        if truncated {
            if let Some(full) = self.complete(id, &installed) {
                return Some(make(full, name, installed, false));
            }
        }
        Some(make(id.to_string(), name, installed, truncated))
    }

    /// The installed package id that `cut` (ending with "…") stands for: the only one that
    /// starts with it, ignoring ASCII case, and has the version `installed`.
    fn complete(&self, cut: &str, installed: &str) -> Option<String> {
        let prefix = cut.trim_end_matches(ELLIPSIS).to_ascii_lowercase();
        let inventory = self.inventory?;
        let mut matches = inventory.packages.iter().filter(|p| {
            p.id.to_ascii_lowercase().starts_with(&prefix)
                && p.version.as_deref().map(str::trim) == Some(installed.trim())
        });
        let first = matches.next()?;
        matches.next().is_none().then(|| first.id.clone())
    }
}

/// Reads every table in the decoded lines of `winget upgrade`.
///
/// Each separator line (at least 10 dashes) opens a table; the first holds the regular rows,
/// later ones the rows winget updates only when they are named. A table's rows are the lines
/// after its separator up to an empty line or the header before the next separator; lines
/// before the first separator (agreements, progress) are ignored. A line that cannot be read
/// counts as unparsed when it ends in a source name, and as a footer otherwise. Rows are
/// kept once per id (ignoring ASCII case), the first one winning; an id listed with more than
/// one installed version gets the note "{k} installations".
pub fn parse_upgrade_table(lines: &[String], inventory: Option<&Inventory>) -> UpgradeTable {
    let mut sources: Vec<String> = BUILTIN_SOURCES.iter().map(|s| s.to_string()).collect();
    if let Some(inventory) = inventory {
        sources.extend(inventory.sources.iter().cloned());
    }
    let reader = Reader { inventory, sources };
    let separators: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_separator(line))
        .map(|(i, _)| i)
        .collect();
    let mut table = UpgradeTable {
        tables: separators.len() as u32,
        ..Default::default()
    };
    let mut rows: Vec<TableRow> = Vec::new();
    let mut versions: Vec<Vec<String>> = Vec::new();
    for (k, &start) in separators.iter().enumerate() {
        let end = separators
            .get(k + 1)
            .map_or(lines.len(), |&next| next.saturating_sub(1));
        for line in lines.iter().take(end).skip(start + 1) {
            if line.trim().is_empty() {
                break;
            }
            match reader.row(line, k >= 1) {
                Some(row) => {
                    let key = row.id.to_ascii_lowercase();
                    match rows.iter().position(|r| r.id.to_ascii_lowercase() == key) {
                        Some(at) => {
                            if !versions[at].contains(&row.installed) {
                                versions[at].push(row.installed);
                            }
                        }
                        None => {
                            versions.push(vec![row.installed.clone()]);
                            rows.push(row);
                        }
                    }
                }
                None => {
                    let looks_like_row =
                        words(line).last().is_some_and(|w| reader.known_source(w.1));
                    if looks_like_row {
                        table.unparsed += 1;
                    }
                }
            }
        }
    }
    for (row, installed) in rows.iter_mut().zip(&versions) {
        if installed.len() > 1 {
            row.note = Some(format!("{} installations", installed.len()));
        }
    }
    table.rows = rows;
    table
}

#[cfg(test)]
mod tests {
    use super::super::export::InstalledPackage;
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(|l| l.trim_end().to_string()).collect()
    }

    fn package(id: &str, version: &str, source: &str) -> InstalledPackage {
        InstalledPackage {
            id: id.to_string(),
            version: Some(version.to_string()),
            source: source.to_string(),
            scope: None,
        }
    }

    fn inventory() -> Inventory {
        Inventory {
            packages: vec![
                package("Contoso.Editor", "1.2.0", "winget"),
                package("Fabrikam.MediaPlayer", "2.0.11.6", "winget"),
                package("Northwind.Tools.x64", "4.0", "winget"),
                package("9NTAILSPIN0001", "1.0.0.0", "msstore"),
                package("Litware.Studio", "5.0.1", "winget"),
                package("Contoso.Builder", "2.0 (build 5)", "winget"),
                package("Contoso.VeryLongProductName", "3.1.0", "winget"),
                package("Fabrikam.SuiteBasic", "7.0", "winget"),
                package("Fabrikam.SuitePro", "7.0", "winget"),
            ],
            sources: vec!["winget".into(), "msstore".into()],
        }
    }

    /// (name, id, installed, available, source) of every row.
    fn summary(table: &UpgradeTable) -> Vec<(String, String, String, String, String)> {
        table
            .rows
            .iter()
            .map(|r| {
                (
                    r.name.clone(),
                    r.id.clone(),
                    r.installed.clone(),
                    r.available.clone(),
                    r.source.clone(),
                )
            })
            .collect()
    }

    fn expected_basic() -> Vec<(String, String, String, String, String)> {
        [
            (
                "Contoso Editor",
                "Contoso.Editor",
                "1.2.0",
                "1.3.0",
                "winget",
            ),
            (
                "Fabrikam Media Player 2",
                "Fabrikam.MediaPlayer",
                "2.0.11.6",
                "2.1.0",
                "winget",
            ),
            (
                "Northwind Tools (x64)",
                "Northwind.Tools.x64",
                "< 4.0",
                "4.2.1",
                "winget",
            ),
            (
                "Tailspin Notes",
                "9NTAILSPIN0001",
                "1.0.0.0",
                "1.1.0.0",
                "msstore",
            ),
        ]
        .iter()
        .map(|(a, b, c, d, e)| {
            (
                a.to_string(),
                b.to_string(),
                c.to_string(),
                d.to_string(),
                e.to_string(),
            )
        })
        .collect()
    }

    const EN_US: &str = include_str!("fixtures/en_us_basic.txt");

    #[test]
    fn the_basic_table_reads_with_and_without_the_dictionary() {
        let inventory = inventory();
        for inv in [Some(&inventory), None] {
            let table = parse_upgrade_table(&lines(EN_US), inv);
            assert_eq!(
                summary(&table),
                expected_basic(),
                "dictionary: {}",
                inv.is_some()
            );
            assert_eq!(table.tables, 1);
            assert_eq!(table.unparsed, 0, "the footers are not rows");
            assert!(table
                .rows
                .iter()
                .all(|r| !r.explicit_only && !r.id_truncated));
            assert!(table.rows.iter().all(|r| r.note.is_none()));
        }
    }

    #[test]
    fn localized_headers_and_footers_read_the_same() {
        let inventory = inventory();
        let expected = expected_basic();
        for text in [
            include_str!("fixtures/de_de.txt"),
            include_str!("fixtures/ko_kr.txt"),
        ] {
            for inv in [Some(&inventory), None] {
                let table = parse_upgrade_table(&lines(text), inv);
                assert_eq!(summary(&table), expected);
                assert_eq!(table.unparsed, 0);
            }
        }
        let table =
            parse_upgrade_table(&lines(include_str!("fixtures/ja_jp.txt")), Some(&inventory));
        let mut expected = expected.clone();
        expected[0].0 = "テスト エディター".to_string();
        assert_eq!(summary(&table), expected);
        assert_eq!(table.unparsed, 0);
    }

    #[test]
    fn the_explicit_targeting_table_marks_its_rows() {
        let table = parse_upgrade_table(
            &lines(include_str!("fixtures/explicit_targeting.txt")),
            Some(&inventory()),
        );
        assert_eq!(table.tables, 2);
        let ids: Vec<(&str, bool)> = table
            .rows
            .iter()
            .map(|r| (r.id.as_str(), r.explicit_only))
            .collect();
        assert_eq!(
            ids,
            [
                ("Contoso.Editor", false),
                ("Fabrikam.MediaPlayer", false),
                ("Litware.Studio", true)
            ]
        );
        assert_eq!(table.unparsed, 0);
    }

    #[test]
    fn versions_with_spaces_need_the_dictionary() {
        let text = lines(include_str!("fixtures/version_spaces.txt"));
        let with = parse_upgrade_table(&text, Some(&inventory()));
        assert_eq!(with.rows[0].id, "Contoso.Builder");
        assert_eq!(with.rows[0].installed, "2.0 (build 5)");
        assert_eq!(with.rows[0].available, "2.1.0");
        assert_eq!(with.rows.len(), 2);
        assert_eq!(with.unparsed, 0);

        let without = parse_upgrade_table(&text, None);
        assert_eq!(
            without
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["Contoso.Editor"]
        );
        assert_eq!(without.unparsed, 1);
    }

    #[test]
    fn a_cut_id_is_completed_only_when_unambiguous() {
        let table = parse_upgrade_table(
            &lines(include_str!("fixtures/truncated.txt")),
            Some(&inventory()),
        );
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0].id, "Contoso.VeryLongProductName");
        assert!(!table.rows[0].id_truncated);
        assert_eq!(table.rows[1].id, "Fabrikam.Suite…");
        assert!(
            table.rows[1].id_truncated,
            "two installed ids start with it"
        );

        let table = parse_upgrade_table(&lines(include_str!("fixtures/truncated.txt")), None);
        assert!(table.rows.iter().all(|r| r.id_truncated));
    }

    #[test]
    fn preamble_and_progress_before_the_table_are_ignored() {
        let table = parse_upgrade_table(
            &lines(include_str!("fixtures/preamble.txt")),
            Some(&inventory()),
        );
        assert_eq!(table.tables, 1);
        assert_eq!(table.rows.len(), 1);
        assert_eq!(table.rows[0].id, "Contoso.Editor");
        assert_eq!(table.unparsed, 0);
    }

    #[test]
    fn empty_output_and_no_upgrades() {
        let table = parse_upgrade_table(&[], Some(&inventory()));
        assert_eq!(table, UpgradeTable::default());
        let table = parse_upgrade_table(&lines(include_str!("fixtures/no_upgrades.txt")), None);
        assert_eq!(table, UpgradeTable::default());
    }

    #[test]
    fn duplicate_ids_are_kept_once_with_a_note() {
        let table = parse_upgrade_table(
            &lines(include_str!("fixtures/duplicates.txt")),
            Some(&inventory()),
        );
        let ids: Vec<&str> = table.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["Contoso.Editor", "Fabrikam.MediaPlayer"]);
        assert_eq!(table.rows[0].installed, "1.2.0", "the first row wins");
        assert_eq!(table.rows[0].note.as_deref(), Some("2 installations"));
        assert_eq!(table.rows[1].note, None);
        assert_eq!(
            table.unparsed, 1,
            "only the broken line that ends in a source"
        );
    }

    #[test]
    fn unparsed_counts_only_lines_ending_in_a_source() {
        let text = lines(
            "Name   Id   Version   Available   Source\n\
             ------------------------------------------\n\
             Contoso Editor  Contoso.Editor  1.2.0  1.3.0  winget\n\
             garbage garbage garbage garbage winget\n\
             garbage garbage garbage garbage garbage\n\
             Other thing  Contoso.Other  1.0  2.0  unknownsource\n",
        );
        let table = parse_upgrade_table(&text, None);
        assert_eq!(table.rows.len(), 1);
        assert_eq!(table.unparsed, 1);

        // A source the export names counts as known.
        let mut inv = Inventory::default();
        inv.sources.push("unknownsource".into());
        let table = parse_upgrade_table(&text, Some(&inv));
        assert_eq!(table.rows.len(), 2);
    }

    #[test]
    fn package_ids_and_sources() {
        for good in [
            "Mozilla.Firefox",
            "Adobe.Acrobat.Reader.64-bit",
            "Notepad++.Notepad++",
            "Python.Python.3.14",
            "a.b",
        ] {
            assert!(valid_package_id(good, "winget"), "{good}");
        }
        for bad in [
            "",
            "-h",
            "a b",
            "x\"y",
            "..",
            "Foo",
            "Foo.",
            ".Foo",
            "a/b.c",
            "a:b.c",
            "a\tb.c",
            "a.b.c.d.e.f.g.h.i",
        ] {
            assert!(!valid_package_id(bad, "winget"), "{bad:?}");
        }
        assert!(!valid_package_id(
            &format!("a.{}", "b".repeat(127)),
            "winget"
        ));
        assert!(valid_package_id("9NBLGGH4NNS1", "msstore"));
        assert!(valid_package_id("XP89DCGQ3K6VLD", "msstore"));
        assert!(!valid_package_id("9NBLGGH4NN", "msstore"));
        assert!(!valid_package_id("Mozilla.Firefox", "msstore"));
        assert!(!valid_package_id("9NBLGGH4NNS1", "winget"));

        assert!(valid_source("winget"));
        assert!(valid_source("my-source_2.test"));
        assert!(!valid_source(""));
        assert!(!valid_source("a b"));
        assert!(!valid_source(&"a".repeat(65)));
    }
}
