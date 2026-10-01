//! Log files of the tool runs, in a `tools` folder next to the journal.
//!
//! Each run gets two files with a common stem such as `20260925-141502-sfc_verify`:
//! - `<stem>.raw`, the tool's own output exactly as it wrote it (UTF-16LE with a byte
//!   order mark for System File Checker, the OEM code page for the others);
//! - `<stem>.log`, a readable UTF-8 transcript: a header, the decoded lines and a footer
//!   with the result. This is the file "Open log" shows.
//!
//! Only the newest runs are kept.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

use super::catalog::{OutputEncoding, ToolRequest};
use crate::safety::state_log;
use crate::{Error, Result};

/// Runs whose files [`prune`] keeps.
pub const KEEP_RUNS: usize = 20;

const UTF16_BOM: &[u8] = &[0xFF, 0xFE];
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// `tools` next to the journal: `%LOCALAPPDATA%\PCOptimizer\tools` by default.
pub fn default_dir() -> PathBuf {
    let journal = state_log::default_path();
    journal
        .parent()
        .map(|dir| dir.join("tools"))
        .unwrap_or_else(|| PathBuf::from("tools"))
}

/// The two files of one run, freshly created.
#[derive(Debug)]
pub struct LogFiles {
    pub stem: String,
    pub raw_path: PathBuf,
    /// The tool's output file; handed to the tool's process.
    pub raw: File,
    pub log_path: PathBuf,
    /// The transcript, positioned after its header.
    pub log: File,
}

fn is_link(path: &Path) -> io::Result<bool> {
    let meta = fs::symlink_metadata(path)?;
    Ok(meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0)
}

/// Refuses a log folder that is, or sits directly in, a link or mount point: the files
/// would land somewhere else. Paths that do not exist yet pass.
pub(crate) fn refuse_links(dir: &Path) -> Result<()> {
    for path in std::iter::once(dir).chain(dir.parent()) {
        if path.as_os_str().is_empty() {
            continue;
        }
        let link = match is_link(path) {
            Ok(link) => link,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        if link {
            return Err(Error::Other(format!(
                "the log folder {} is inside a link or mount point; no logs are written there",
                dir.display()
            )));
        }
    }
    Ok(())
}

/// Creates the transcript of a new run in `dir`: the folder is checked with
/// [`refuse_links`] before and after it is created when missing, then `<stem>.log` is
/// created new with a UTF-8 byte order mark and `header` followed by CRLF. The stem is
/// `<YYYYMMDD-HHMMSS>-<kind>`, with `-2` to `-9` appended when a run of the same kind
/// started in the same second, so [`prune`] recognises it. `kind` must match `[a-z_]+`.
/// Returns the stem, the path and the file positioned after the header.
pub fn create_transcript(
    dir: &Path,
    kind: &str,
    header: &str,
    now: DateTime<Local>,
) -> Result<(String, PathBuf, File)> {
    if kind.is_empty() || !kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        return Err(Error::Other(format!(
            "not a run kind: {kind:?}; expected lowercase letters and underscores"
        )));
    }
    refuse_links(dir)?;
    fs::create_dir_all(dir)?;
    refuse_links(dir)?;
    let base = format!("{}-{kind}", now.format("%Y%m%d-%H%M%S"));
    for attempt in 1..=9 {
        let stem = if attempt == 1 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        };
        let path = dir.join(format!("{stem}.log"));
        let mut file = match create_new(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        };
        let written = file
            .write_all(UTF8_BOM)
            .and_then(|()| file.write_all(format!("{header}\r\n").as_bytes()));
        if let Err(e) = written {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(e.into());
        }
        return Ok((stem, path, file));
    }
    Err(Error::Other(format!(
        "too many {kind} runs started in the same second in {}",
        dir.display()
    )))
}

fn create_new(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn header(request: &ToolRequest, now: DateTime<Local>) -> String {
    format!(
        "Cairn {}  ·  {}\r\nStarted {}\r\n\r\n",
        crate::VERSION,
        request.command_line(),
        now.format("%Y-%m-%d %H:%M:%S")
    )
}

/// Creates the files of a new run in `dir` (created when missing): the raw file, which
/// starts with a UTF-16LE byte order mark for tools that write UTF-16LE, and the transcript
/// with its UTF-8 byte order mark and header. The stem is `<YYYYMMDD-HHMMSS>-<tool>`, with
/// `-2` to `-9` appended when a run of the same tool started in the same second.
pub fn create(dir: &Path, request: &ToolRequest, now: DateTime<Local>) -> Result<LogFiles> {
    // Checked before anything is created through a link, and again once the folder exists.
    refuse_links(dir)?;
    fs::create_dir_all(dir)?;
    refuse_links(dir)?;
    let base = format!("{}-{}", now.format("%Y%m%d-%H%M%S"), request.tool.as_str());
    for attempt in 1..=9 {
        let stem = if attempt == 1 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        };
        let raw_path = dir.join(format!("{stem}.raw"));
        let log_path = dir.join(format!("{stem}.log"));
        let mut raw = match create_new(&raw_path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        };
        let mut log = match create_new(&log_path) {
            Ok(file) => file,
            Err(e) => {
                drop(raw);
                let _ = fs::remove_file(&raw_path);
                if e.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(e.into());
            }
        };
        let written = (|| -> io::Result<()> {
            if request.info().encoding == OutputEncoding::Utf16Le {
                raw.write_all(UTF16_BOM)?;
            }
            log.write_all(UTF8_BOM)?;
            log.write_all(header(request, now).as_bytes())
        })();
        if let Err(e) = written {
            drop((raw, log));
            let _ = fs::remove_file(&raw_path);
            let _ = fs::remove_file(&log_path);
            return Err(e.into());
        }
        return Ok(LogFiles {
            stem,
            raw_path,
            raw,
            log_path,
            log,
        });
    }
    Err(Error::Other(format!(
        "too many runs of {} started in the same second in {}",
        request.tool,
        dir.display()
    )))
}

/// Appends lines to a transcript, each ended with CRLF.
pub fn append_lines(log: &mut File, lines: &[String]) -> Result<()> {
    let mut text = String::with_capacity(lines.iter().map(|l| l.len() + 2).sum());
    for line in lines {
        text.push_str(line);
        text.push_str("\r\n");
    }
    log.write_all(text.as_bytes())?;
    Ok(())
}

/// Appends the closing block of a transcript after an empty line.
pub fn append_footer(log: &mut File, text: &str) -> Result<()> {
    let mut block = String::from("\r\n");
    for line in text.lines() {
        block.push_str(line);
        block.push_str("\r\n");
    }
    log.write_all(block.as_bytes())?;
    Ok(())
}

/// The stem of a run file name (`^\d{8}-\d{6}-[a-z_]+(-\d)?\.(log|raw)$`); `None` for any
/// other name.
fn run_stem(name: &str) -> Option<&str> {
    let stem = name
        .strip_suffix(".log")
        .or_else(|| name.strip_suffix(".raw"))?;
    let bytes = stem.as_bytes();
    if bytes.len() < 17 {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if !digits(0..8) || bytes[8] != b'-' || !digits(9..15) || bytes[15] != b'-' {
        return None;
    }
    let tail = &bytes[16..];
    let tool = match tail {
        [head @ .., b'-', d] if d.is_ascii_digit() => head,
        _ => tail,
    };
    let valid = !tool.is_empty() && tool.iter().all(|&b| b.is_ascii_lowercase() || b == b'_');
    valid.then_some(stem)
}

/// Deletes the files of old runs in `dir`: every run beyond the newest `keep`, oldest
/// first, except the runs whose stems are in `protect`. Only regular files named like run
/// files are touched; links, folders and other files are left alone. Errors are logged
/// and otherwise ignored.
pub fn prune(dir: &Path, keep: usize, protect: &[String]) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "cannot list the tool logs");
            return;
        }
    };
    let mut runs: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(run_stem) else {
            continue;
        };
        // Reads the entry itself; a link is not followed.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            continue;
        }
        runs.entry(stem.to_string()).or_default().push(entry.path());
    }
    let doomed: Vec<&String> = runs
        .keys()
        .rev()
        .skip(keep)
        .filter(|stem| !protect.contains(stem))
        .collect();
    for stem in doomed.into_iter().rev() {
        for path in &runs[stem] {
            if let Err(e) = fs::remove_file(path) {
                tracing::warn!(path = %path.display(), error = %e, "cannot delete an old tool log");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use chrono::TimeZone;

    use super::*;
    use crate::tools::catalog::ToolId;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 9, 25, h, m, s).unwrap()
    }

    fn read(path: &Path) -> Vec<u8> {
        let mut bytes = Vec::new();
        File::open(path).unwrap().read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn run_file_names_are_recognised() {
        for name in [
            "20260925-141502-sfc_verify.log",
            "20260925-141502-sfc_verify.raw",
            "20260925-141502-disk_check-2.raw",
        ] {
            assert!(run_stem(name).is_some(), "{name}");
        }
        assert_eq!(
            run_stem("20260925-141502-disk_check-2.raw"),
            Some("20260925-141502-disk_check-2")
        );
        for name in [
            "notes.txt",
            "20260925-141502-SFC.log",
            "20260925-141502-.log",
            "20260925-141502--2.log",
            "2026092-141502-sfc.log",
            "20260925_141502-sfc.log",
            "20260925-141502-sfc-12.log",
            "20260925-141502-sfc.txt",
            "20260925-141502-sfc.log.bak",
        ] {
            assert_eq!(run_stem(name), None, "{name}");
        }
    }

    #[test]
    fn files_are_created_with_boms_header_and_unique_stems() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join(r"PCOptimizer\tools");
        let sfc = ToolRequest::new(ToolId::SfcVerify, None).unwrap();
        let chkdsk = ToolRequest::new(ToolId::DiskCheck, Some("C:")).unwrap();

        let first = create(&logs, &sfc, at(14, 15, 2)).unwrap();
        assert_eq!(first.stem, "20260925-141502-sfc_verify");
        let second = create(&logs, &sfc, at(14, 15, 2)).unwrap();
        assert_eq!(second.stem, "20260925-141502-sfc_verify-2");
        let other = create(&logs, &chkdsk, at(14, 15, 2)).unwrap();
        assert_eq!(other.stem, "20260925-141502-disk_check");

        assert_eq!(read(&first.raw_path), UTF16_BOM);
        assert!(read(&other.raw_path).is_empty(), "no BOM for OEM output");
        let text = String::from_utf8(read(&first.log_path)).unwrap();
        assert!(text.starts_with('\u{feff}'));
        assert!(text.contains("Cairn "), "{text}");
        assert!(text.contains("  ·  sfc.exe /verifyonly\r\n"), "{text}");
        assert!(
            text.contains("Started 2026-09-25 14:15:02\r\n\r\n"),
            "{text}"
        );

        let mut log = first.log;
        append_lines(&mut log, &["one".into(), "twö".into()]).unwrap();
        append_footer(&mut log, "Finished\nexit 0").unwrap();
        drop(log);
        let text = String::from_utf8(read(&first.log_path)).unwrap();
        assert!(
            text.ends_with("\r\n\r\none\r\ntwö\r\n\r\nFinished\r\nexit 0\r\n"),
            "{text:?}"
        );
    }

    #[test]
    fn prune_keeps_twenty_runs_and_ignores_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let stems: Vec<String> = (0..25)
            .map(|i| format!("20260925-1400{i:02}-sfc_verify"))
            .collect();
        for stem in &stems {
            fs::write(path.join(format!("{stem}.log")), b"x").unwrap();
            fs::write(path.join(format!("{stem}.raw")), b"x").unwrap();
        }
        let foreign = [
            "notes.txt",
            "20260925-140000-SFC.log",
            "20250101-000000-sfc_verify.log.bak",
        ];
        for name in foreign {
            fs::write(path.join(name), b"keep").unwrap();
        }
        // A folder named like a run is not a run file.
        fs::create_dir(path.join("20200101-000000-sfc_verify.log")).unwrap();

        prune(path, KEEP_RUNS, &[stems[1].clone()]);

        let exists = |stem: &str| path.join(format!("{stem}.log")).exists();
        for (i, stem) in stems.iter().enumerate() {
            let kept = i >= 5 || i == 1;
            assert_eq!(exists(stem), kept, "{stem}");
            assert_eq!(path.join(format!("{stem}.raw")).exists(), kept, "{stem}");
        }
        for name in foreign {
            assert!(path.join(name).exists(), "{name}");
        }
        assert!(path.join("20200101-000000-sfc_verify.log").is_dir());

        // A missing folder is not an error.
        prune(&path.join("missing"), KEEP_RUNS, &[]);
    }

    #[test]
    fn transcripts_have_a_bom_a_header_and_unique_stems() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join(r"jobs\winget");
        let header = "Cairn 0.0.0  ·  winget upgrade --all";
        let (stem, path, mut file) =
            create_transcript(&logs, "winget_scan", header, at(8, 5, 9)).unwrap();
        assert_eq!(stem, "20260925-080509-winget_scan");
        assert_eq!(path, logs.join("20260925-080509-winget_scan.log"));
        file.write_all(b"line\r\n").unwrap();
        drop(file);
        let text = String::from_utf8(read(&path)).unwrap();
        assert_eq!(text, format!("\u{feff}{header}\r\nline\r\n"));

        let (second, _, _) = create_transcript(&logs, "winget_scan", header, at(8, 5, 9)).unwrap();
        assert_eq!(second, "20260925-080509-winget_scan-2");
        for stem in [&stem, &second] {
            assert_eq!(run_stem(&format!("{stem}.log")), Some(stem.as_str()));
        }
        for attempt in 3..=9 {
            let (next, _, _) =
                create_transcript(&logs, "winget_scan", header, at(8, 5, 9)).unwrap();
            assert_eq!(next, format!("20260925-080509-winget_scan-{attempt}"));
        }
        assert!(create_transcript(&logs, "winget_scan", header, at(8, 5, 9)).is_err());

        for bad in ["", "Winget", "win-get", "scan1", r"..\x"] {
            assert!(
                create_transcript(&logs, bad, header, at(8, 5, 9)).is_err(),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn log_dir_that_is_a_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        if let Err(e) = std::os::windows::fs::symlink_dir(&real, &link) {
            eprintln!("skipped: cannot create a directory symbolic link here ({e})");
            return;
        }
        let request = ToolRequest::new(ToolId::SfcVerify, None).unwrap();
        let err = create(&link, &request, at(9, 0, 0)).unwrap_err();
        assert!(err.to_string().contains("link or mount point"), "{err}");
        assert_eq!(
            fs::read_dir(&real).unwrap().count(),
            0,
            "a file was written"
        );
        // A folder directly inside a link is refused as well, before it is created.
        assert!(create(&link.join("tools"), &request, at(9, 0, 0)).is_err());
        assert!(!real.join("tools").exists(), "a folder was created");
        // Transcripts follow the same rule.
        assert!(create_transcript(&link, "maintenance", "header", at(9, 0, 0)).is_err());
        assert!(create_transcript(&link.join("jobs"), "maintenance", "h", at(9, 0, 0)).is_err());
        assert_eq!(fs::read_dir(&real).unwrap().count(), 0);
        fs::remove_dir(&link).unwrap();
    }
}
