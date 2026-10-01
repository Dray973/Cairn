//! Integration tests for the storage section through the public API. Every file lives in a
//! temporary folder: the speed test runs with a place inside it and tiny sizes, and scans read
//! only folders the tests created.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use optimizer_core::jobs::{HostConfig, HostJobId, JobHost, JobState};
use optimizer_core::safety::state_log::Journal;
use optimizer_core::storage::speed::{RunningTool, MIB};
use optimizer_core::storage::{
    self, busy_on, plan_or_start_duplicates, plan_or_start_scan, plan_or_start_speed_test,
    remove_leftover_opening, DuplicatesRequest, ScanRequest, ScanResult, SortOrder, SpeedEnv,
    SpeedTestRequest, StorageVolume, TreeRowKind,
};
use optimizer_core::win::storage::MediaKind;

fn host(dir: &Path) -> JobHost {
    JobHost::new(HostConfig {
        tick: Duration::from_millis(5),
        settle_wait: Duration::from_millis(500),
        stop_wait: Duration::from_millis(500),
        log_dir: dir.join("jobs"),
        ..storage::lane_config()
    })
}

fn temp_letter() -> String {
    std::env::temp_dir().to_string_lossy()[..2].to_ascii_uppercase()
}

fn yes() -> bool {
    true
}
fn no() -> bool {
    false
}
fn temp_volume() -> optimizer_core::Result<Vec<StorageVolume>> {
    Ok(vec![StorageVolume {
        letter: temp_letter(),
        label: "Windows".to_string(),
        file_system: "NTFS".to_string(),
        size_bytes: Some(952 << 30),
        free_bytes: Some(611 << 30),
        media: MediaKind::Ssd,
        bus: Some("NVMe".to_string()),
        model: Some("Test NVMe SSD".to_string()),
        system: false,
        read_only: false,
        persistent_acls: true,
        not_responding: false,
        error: None,
        speed_test_blocked: None,
        scan_blocked: None,
        leftovers: Vec::new(),
    }])
}
fn plenty(_: &Path) -> optimizer_core::Result<u64> {
    Ok(1 << 50)
}
fn no_battery() -> Option<bool> {
    None
}
fn no_tool() -> Option<RunningTool> {
    None
}
fn no_processes() -> Option<HashSet<String>> {
    None
}

/// Elevated as far as the plan is concerned; the test folder gets no DACL, so the test runs
/// without administrator rights.
const ENV: SpeedEnv = SpeedEnv {
    elevated: yes,
    volumes: temp_volume,
    free_bytes: plenty,
    on_battery: no_battery,
    running_tool: no_tool,
    processes: no_processes,
    protect_folder: false,
};

fn journal(dir: &Path) -> Arc<Journal> {
    Arc::new(Journal::open(dir.join("journal.db")).unwrap())
}

fn write(path: &Path, len: usize, seed: u8) {
    let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8 ^ seed).collect();
    std::fs::write(path, data).unwrap();
}

#[test]
fn scan_children_and_duplicates_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path());
    let root = dir.path().join("data");
    std::fs::create_dir_all(root.join("photos").join("2026")).unwrap();
    std::fs::create_dir_all(root.join("backup")).unwrap();
    write(
        &root.join("photos").join("2026").join("beach.jpg"),
        3 << 20,
        1,
    );
    write(&root.join("backup").join("beach.jpg"), 3 << 20, 1);
    write(&root.join("backup").join("other.jpg"), 3 << 20, 2);
    write(&root.join("notes.txt"), 4000, 3);

    let request = ScanRequest { path: root.clone() };
    let (plan, job) = plan_or_start_scan(&host, &request, true).unwrap();
    assert!(job.is_none());
    assert_eq!(plan.blocked_reason, None);
    let (_, job) = plan_or_start_scan(&host, &request, false).unwrap();
    let scan_id = job.unwrap().id;
    let end = host.wait(scan_id, Duration::from_secs(30)).unwrap();
    assert_eq!(end.state, JobState::Succeeded, "{end:?}");
    assert!(!end.logged);
    // A finished job still carries the progress block of its kind.
    let detail = end.detail.unwrap();
    assert_eq!(detail["phase"], "done");
    assert_eq!(detail["scan"]["files"], 4);
    assert!(detail["speed"].is_null() && detail["duplicates"].is_null());

    let (revision, json) = host.result(scan_id, 0).unwrap();
    assert!(revision >= 1);
    assert!(host.result(scan_id, revision).is_none());
    assert_eq!(json["summary"]["files"], 4);
    assert_eq!(json["summary"]["folders"], 3);
    assert_eq!(json["summary"]["duplicate_candidates"], 3);

    let tree: Arc<ScanResult> = host.typed(scan_id).unwrap();
    let top = tree.children(0, SortOrder::Allocated, 500).unwrap();
    let names: Vec<&str> = top.children.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["backup", "photos", "1 smaller file"]);
    let photos = top.children[1].node.unwrap();
    assert_eq!(tree.path_of(photos).unwrap(), root.join("photos"));
    let year = tree.children(photos, SortOrder::Logical, 500).unwrap();
    assert_eq!(year.children[0].kind, TreeRowKind::Folder);
    assert_eq!(year.children[0].name, "2026");

    let request = DuplicatesRequest::new(scan_id, MIB).unwrap();
    let (_, job) = plan_or_start_duplicates(&host, &request, false).unwrap();
    let dup_id = job.unwrap().id;
    let end = host.wait(dup_id, Duration::from_secs(30)).unwrap();
    assert_eq!(end.state, JobState::Succeeded, "{end:?}");
    let detail = end.detail.unwrap();
    assert_eq!(detail["phase"], "done");
    assert_eq!(detail["duplicates"]["groups_found"], 1);
    assert!(detail["speed"].is_null() && detail["scan"].is_null());
    let (_, json) = host.result(dup_id, 0).unwrap();
    assert_eq!(json["kind"], "duplicates");
    assert_eq!(json["group_count"], 1);
    assert_eq!(json["wasted_bytes"], 3 << 20);
    let paths: Vec<PathBuf> = json["groups"][0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| PathBuf::from(f["path"].as_str().unwrap()))
        .collect();
    assert_eq!(
        paths,
        [
            root.join("backup").join("beach.jpg"),
            root.join("photos").join("2026").join("beach.jpg")
        ]
    );
    // Nothing was changed or deleted.
    assert_eq!(std::fs::read_dir(root.join("backup")).unwrap().count(), 2);
    // An unknown scan is refused.
    let gone = DuplicatesRequest::new(HostJobId(u64::MAX), MIB).unwrap();
    assert!(plan_or_start_duplicates(&host, &gone, false).is_err());
}

#[test]
fn a_speed_test_in_a_place_is_audited_and_marks_its_volume_busy() {
    let dir = tempfile::tempdir().unwrap();
    let place = dir.path().join("place");
    std::fs::create_dir(&place).unwrap();
    let journal = journal(dir.path());
    let letter = temp_letter();
    let request = SpeedTestRequest::new(&letter, 4 * MIB, 1)
        .unwrap()
        .with_place(&place)
        .with_timing(Duration::from_millis(150), Duration::ZERO)
        .with_history_dir(dir.path().join("history"));
    // Without the place, the volume root is refused in every cargo run.
    let at_root = SpeedTestRequest::new(&letter, 4 * MIB, 1).unwrap();
    let (plan, _) = plan_or_start_speed_test(storage::lane(), &ENV, &at_root, true, || {
        panic!("a dry run opens no journal")
    })
    .unwrap();
    assert_eq!(
        plan.blocked_reason.as_deref(),
        Some(storage::speed::FORBIDDEN_TEXT)
    );

    let open = {
        let journal = Arc::clone(&journal);
        move || Ok(journal)
    };
    let (plan, job) =
        plan_or_start_speed_test(storage::lane(), &ENV, &request, false, open).unwrap();
    assert_eq!(plan.max_write_bytes, 20 * MIB);
    let id = job.unwrap().id;
    assert_eq!(
        busy_on(&letter).as_deref(),
        Some(storage::SPEED_TEST_BUSY_TITLE)
    );
    assert_eq!(
        busy_on(&letter.to_ascii_lowercase()).as_deref(),
        Some(storage::SPEED_TEST_BUSY_TITLE)
    );
    let other = if letter == "Z:" { "Y:" } else { "Z:" };
    assert_eq!(busy_on(other), None);
    let end = storage::lane().wait(id, Duration::from_secs(60)).unwrap();
    assert_eq!(end.state, JobState::Succeeded, "{end:?}");
    assert_eq!(busy_on(&letter), None);

    let mut rows = journal.ops(10).unwrap();
    rows.reverse();
    let outcomes: Vec<&str> = rows.iter().map(|r| r.outcome.as_str()).collect();
    assert_eq!(outcomes, ["started", "succeeded"]);
    assert!(rows
        .iter()
        .all(|r| r.op == storage::speed::OP && r.target == letter));
    assert_eq!(std::fs::read_dir(&place).unwrap().count(), 0);
    let history = storage::speed_history(&dir.path().join("history")).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].measurements.len(), 8);
}

#[test]
fn a_refused_leftover_removal_opens_no_journal() {
    fn never_open() -> optimizer_core::Result<Journal> {
        panic!("a refused removal opens no journal")
    }
    // Without administrator rights nothing is looked at, not even the folder's name.
    let unelevated = SpeedEnv {
        elevated: no,
        ..ENV
    };
    let at_root = Path::new(r"C:\CairnSpeedTest-0123abcd");
    let err = remove_leftover_opening(&unelevated, at_root, never_open).unwrap_err();
    assert!(matches!(err, optimizer_core::Error::NotElevated), "{err}");
    // A folder that is not a test folder, and a test folder that is not at a volume root.
    let err = remove_leftover_opening(&ENV, Path::new(r"C:\Windows"), never_open).unwrap_err();
    assert!(
        err.to_string().contains("is not a speed-test folder"),
        "{err}"
    );
    let dir = tempfile::tempdir().unwrap();
    let inside = dir.path().join("CairnSpeedTest-0123abcd");
    std::fs::create_dir(&inside).unwrap();
    let err = remove_leftover_opening(&ENV, &inside, never_open).unwrap_err();
    assert!(err.to_string().contains("is not where Cairn puts"), "{err}");
    assert!(inside.is_dir());
}
