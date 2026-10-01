//! Integration tests for disk cleanup. Nothing here deletes real files: the scan is
//! read-only, and `clean` is only called with ids that match no target.

use std::collections::HashSet;
use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::mem::size_of;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use optimizer_core::cleanup::{self, CleanupScan};
use optimizer_core::safety::state_log::Journal;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FileBasicInfo, SetFileInformationByHandle, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
};

const EXPECTED_IDS: [&str; 12] = [
    "user_temp",
    "windows_temp",
    "update_cache",
    "delivery_optimization",
    "crash_dumps",
    "error_reports",
    "thumbnail_cache",
    "recycle_bin",
    "shader_cache",
    "browser_chrome",
    "browser_edge",
    "browser_firefox",
];

/// 100 ns intervals between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET: i64 = 116_444_736_000_000_000;

fn temp_journal() -> (tempfile::TempDir, Journal) {
    let dir = tempfile::tempdir().expect("temp dir");
    let journal = Journal::open(dir.path().join("journal.db")).expect("open journal");
    (dir, journal)
}

/// Moves the creation, last-write and change times of a file or folder back by `by`.
fn age(path: &Path, by: Duration) {
    let since = (SystemTime::now() - by).duration_since(UNIX_EPOCH).unwrap();
    let ticks = (since.as_nanos() / 100) as i64 + FILETIME_UNIX_OFFSET;
    let entry = OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
        .unwrap();
    let info = FILE_BASIC_INFO {
        CreationTime: ticks,
        LastAccessTime: 0,
        LastWriteTime: ticks,
        ChangeTime: ticks,
        FileAttributes: 0,
    };
    // SAFETY: `info` is a FILE_BASIC_INFO that outlives the call; the size matches it.
    unsafe {
        SetFileInformationByHandle(
            HANDLE(entry.as_raw_handle()),
            FileBasicInfo,
            &info as *const FILE_BASIC_INFO as *const c_void,
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    }
    .unwrap();
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn print_scan(scan: &CleanupScan) {
    println!(
        "cleanup scan: {:.1} MiB in {} ms",
        mib(scan.total_bytes),
        scan.duration_ms
    );
    for t in &scan.targets {
        println!(
            "  {:<22} {:>10.1} MiB {:>8} files  admin={:<5} default={:<5} recent_kept={:<5} blocked={:?}",
            t.id,
            mib(t.bytes),
            t.files,
            t.requires_admin,
            t.default_on,
            t.recent_files_kept,
            t.blocked_reason
        );
        for path in &t.paths {
            println!("      {path}");
        }
    }
}

#[test]
fn catalog_ids_are_unique_and_in_display_order() {
    let catalog = cleanup::catalog();
    let ids: Vec<&str> = catalog.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, EXPECTED_IDS);
    let unique: HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len());
    for t in &catalog {
        assert!(!t.title.is_empty(), "{} has no title", t.id);
        assert!(t.description.ends_with('.'), "{} description", t.id);
    }
    let off: Vec<&str> = catalog
        .iter()
        .filter(|t| !t.default_on)
        .map(|t| t.id.as_str())
        .collect();
    assert_eq!(off, ["thumbnail_cache", "recycle_bin", "shader_cache"]);
    let keeps_recent: Vec<&str> = catalog
        .iter()
        .filter(|t| t.recent_files_kept)
        .map(|t| t.id.as_str())
        .collect();
    assert_eq!(keeps_recent, ["user_temp", "windows_temp"]);
    let shader = catalog.iter().find(|t| t.id == "shader_cache").unwrap();
    assert!(shader.description.contains("stutter"));
}

#[test]
fn scan_is_read_only_and_lists_every_target() {
    // An old file in an old folder in the user temp folder is eligible, so the scan must
    // count it and leave it in place.
    let sandbox = tempfile::tempdir().expect("sandbox in temp");
    let probe = sandbox.path().join("scan-probe.tmp");
    fs::write(&probe, vec![0u8; 4096]).unwrap();
    age(&probe, Duration::from_secs(3 * 24 * 60 * 60));
    age(sandbox.path(), Duration::from_secs(3 * 24 * 60 * 60));

    let scan = cleanup::scan().expect("scan");
    print_scan(&scan);

    assert!(probe.exists(), "scan deleted a file");
    assert_eq!(fs::read(&probe).unwrap().len(), 4096);
    let ids: Vec<&str> = scan.targets.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, EXPECTED_IDS);
    assert_eq!(
        scan.total_bytes,
        scan.targets.iter().map(|t| t.bytes).sum::<u64>()
    );
    let temp = scan.targets.iter().find(|t| t.id == "user_temp").unwrap();
    assert!(temp.recent_files_kept);
    match &temp.blocked_reason {
        None => {
            assert!(
                temp.bytes >= 4096 && temp.files >= 1,
                "probe not counted: {temp:?}"
            );
            // The standard folder or a numbered per-session folder inside it (`Temp\2`).
            let root = sandbox.path().parent().unwrap();
            let resolved = fs::canonicalize(root).unwrap().display().to_string();
            let shown = resolved.strip_prefix(r"\\?\").unwrap_or(&resolved);
            assert!(
                temp.paths.iter().any(|p| p == shown),
                "{shown} not listed in {:?}",
                temp.paths
            );
        }
        // A non-standard %TMP% in this environment blocks the target instead.
        Some(reason) => {
            assert!(reason.contains("temp folder"), "{reason}");
            assert_eq!((temp.bytes, temp.files), (0, 0));
            assert!(temp.paths.is_empty());
        }
    }
    for t in &scan.targets {
        if t.files == 0 {
            assert_eq!(t.bytes, 0, "{} has bytes without files", t.id);
        }
        if let Some(reason) = &t.blocked_reason {
            assert!(!reason.is_empty());
            if reason.contains("DISM") {
                assert!(
                    t.id == "update_cache" || t.id == "delivery_optimization",
                    "{} waits for DISM",
                    t.id
                );
            }
        }
        if t.requires_admin && !optimizer_core::is_elevated() {
            assert_eq!(
                t.blocked_reason.as_deref(),
                Some("needs administrator rights"),
                "{} is not cleanable without elevation",
                t.id
            );
        }
        assert_eq!(
            t.recent_files_kept,
            t.id == "user_temp" || t.id == "windows_temp",
            "{}",
            t.id
        );
        for path in &t.paths {
            let bytes = path.as_bytes();
            assert!(
                bytes.len() > 3 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == b":\\",
                "{}: {path} is not a local drive path",
                t.id
            );
        }
    }
}

#[test]
fn clean_unknown_id_is_skipped_and_journaled() {
    let (_dir, journal) = temp_journal();
    let ids = vec!["no_such_target".to_string(), "no_such_target".to_string()];
    let report = cleanup::clean(&journal, &ids).expect("clean");

    assert_eq!(report.freed_bytes, 0);
    assert_eq!(report.results.len(), 1, "repeated ids run once");
    let result = &report.results[0];
    assert_eq!(result.id, "no_such_target");
    assert_eq!(result.skipped_reason.as_deref(), Some("unknown target"));
    assert_eq!((result.freed_bytes, result.deleted_files), (0, 0));
    assert!(result.errors.is_empty());

    let sessions = journal.sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].label, "cleanup");
    assert!(sessions[0].ended_at.is_some(), "session left open");
    let ops = journal.ops(10).unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].op, "cleanup");
    assert_eq!(ops[0].target, "no_such_target");
    assert_eq!(ops[0].outcome, "skipped");
    assert_eq!(ops[0].session_id, Some(sessions[0].id));
    // Cleanup never records anything that a rollback would replay.
    assert!(!journal.summary().unwrap().has_pending_changes());
}

#[test]
fn clean_keeps_every_result_when_the_audit_log_cannot_be_written() {
    let (dir, journal) = temp_journal();
    // Break the audit table behind the journal's back: every audit entry now fails.
    let other = rusqlite::Connection::open(dir.path().join("journal.db")).unwrap();
    other.execute_batch("DROP TABLE ops_log;").unwrap();
    drop(other);

    let ids = vec!["no_such_target".to_string(), "also_missing".to_string()];
    let report = cleanup::clean(&journal, &ids).expect("clean still reports");
    let got: Vec<&str> = report.results.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(got, ["no_such_target", "also_missing"], "every target ran");
    for result in &report.results {
        assert_eq!(result.skipped_reason.as_deref(), Some("unknown target"));
        assert!(
            result
                .errors
                .first()
                .is_some_and(|e| e.starts_with("not recorded in the audit log")),
            "{result:?}"
        );
    }
    let sessions = journal.sessions().unwrap();
    assert!(sessions[0].ended_at.is_some(), "session left open");
}

#[test]
fn clean_with_no_ids_opens_and_closes_a_session() {
    let (_dir, journal) = temp_journal();
    let report = cleanup::clean(&journal, &[]).expect("clean");
    assert!(report.results.is_empty());
    assert_eq!(report.freed_bytes, 0);

    let sessions = journal.sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].label, "cleanup");
    assert!(sessions[0].ended_at.is_some());
    assert!(journal.ops(10).unwrap().is_empty());
}
