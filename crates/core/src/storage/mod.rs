//! Disk speed test, space analyzer and duplicate finder, run as polled in-process jobs of the
//! "storage" lane.
//!
//! - Read-only: [`volumes`], the space scan and the duplicate search. They write no audit
//!   rows and change nothing; the duplicate search only reads file contents.
//! - Audited, never journaled: the speed test (`disk_speed_test`) and the removal of a test
//!   file a speed test left behind (`remove_speed_test_file`). Both write their "started" row
//!   before they touch the disk and exactly one final row. Storage makes no reversible change,
//!   so nothing goes to a `*_journal` table and nothing appears in History's change list.
//! - No user file is ever deleted: the only files removed are the speed test's own file and
//!   folder.
//! - Locks guard in-memory state only; none is held across file, SQLite, thread-spawn or
//!   directory I/O, so reading a job, a result or a folder's children is a microsecond read,
//!   safe on the UI thread with the GIL held.

pub mod duplicates;
pub(crate) mod files;
mod hash;
pub mod scan;
pub mod speed;
pub(crate) mod volumes;

pub use duplicates::{
    duplicates_title, plan_or_start_duplicates, DuplicateFile, DuplicateGroup, DuplicatesPlan,
    DuplicatesProgress, DuplicatesRequest, DuplicatesResult,
};
pub use scan::{
    plan_or_start_scan, ChildrenPage, ScanPlan, ScanProgress, ScanRequest, ScanResult, ScanSummary,
    SortOrder, TreeRow, TreeRowKind,
};

pub use speed::{
    plan_or_start_speed_test, remove_leftover, remove_leftover_opening, speed_history, Direction,
    Leftover, LeftoverRemoval, Measurement, RunningTool, SpeedEnv, SpeedProgress, SpeedTestId,
    SpeedTestPlan, SpeedTestRequest, SpeedTestResult, DEFAULT_RUNS, DEFAULT_SIZE, RUN_CHOICES,
    SIZE_CHOICES,
};
pub use volumes::{volumes, StorageVolume};

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};
use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};

use crate::jobs::{HostConfig, JobHost};
use crate::safety::state_log;

/// Environment variable that turns off speed tests and leftover removal at volume roots.
pub const FORBID_DRIVE_TESTS_ENV: &str = "OPTIMIZER_FORBID_DRIVE_TESTS";
/// Lane name of the storage job host.
pub const LANE: &str = "storage";
/// Job kinds of the storage lane.
pub const KIND_SPEED_TEST: &str = "speed_test";
pub const KIND_SCAN: &str = "space_scan";
pub const KIND_DUPLICATES: &str = "duplicates";
/// What the tool runner reports as running on a volume under a speed test.
pub const SPEED_TEST_BUSY_TITLE: &str = "Disk speed test";

/// True in unit tests and when `OPTIMIZER_FORBID_DRIVE_TESTS` is "1": speed tests and
/// leftover removal at a volume root are refused.
pub fn drive_tests_forbidden() -> bool {
    cfg!(test) || std::env::var(FORBID_DRIVE_TESTS_ENV).is_ok_and(|v| v.trim() == "1")
}

/// `<data dir>\storage`: the speed-test history lives here (honours `OPTIMIZER_DATA_DIR`).
pub fn data_dir() -> PathBuf {
    state_log::data_dir().join("storage")
}

/// How the storage lane runs its jobs: logs under `<data dir>\jobs\storage` (storage jobs
/// keep no transcript), and only the newest result of each kind, since a scan tree can take
/// tens of megabytes.
pub fn lane_config() -> HostConfig {
    HostConfig {
        lane: LANE,
        log_dir: state_log::data_dir().join("jobs").join(LANE),
        keep_logs: 20,
        tick: Duration::from_millis(100),
        settle_wait: Duration::from_secs(3),
        stop_wait: Duration::from_secs(3),
        keep_finished: 10,
        keep_results_per_kind: Some(1),
    }
}

/// The process-wide storage job host.
pub fn lane() -> &'static JobHost {
    static LANE_HOST: OnceLock<JobHost> = OnceLock::new();
    LANE_HOST.get_or_init(|| JobHost::new(lane_config()))
}

/// Title of the speed-test job on `letter` ("C:").
pub fn speed_title(letter: &str) -> String {
    format!("Speed test of {letter}")
}

/// Title of the speed test running on volume `letter` ("C:"), for the tool planner; None when
/// no speed test runs there.
pub fn busy_on(letter: &str) -> Option<String> {
    let letter = crate::tools::catalog::normalize_volume(letter).ok()?;
    let running = lane().running()?;
    (running.kind == KIND_SPEED_TEST && running.title == speed_title(&letter))
        .then(|| SPEED_TEST_BUSY_TITLE.to_string())
}

/// The live `detail` of a storage job: its phase and the progress block of its kind.
pub(crate) fn detail(
    phase: &str,
    speed: Option<Value>,
    scan: Option<Value>,
    duplicates: Option<Value>,
) -> Value {
    json!({ "phase": phase, "speed": speed, "scan": scan, "duplicates": duplicates })
}

/// Now as RFC 3339 UTC in whole seconds.
pub(crate) fn utc_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A size as Cairn shows it (1024-based): "512 B", "8 KB", "64 MB", "9.4 GB".
pub(crate) fn size_text(bytes: u64) -> String {
    let mut size = bytes as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if size < 1024.0 || unit == "GB" {
            if unit == "B" || unit == "KB" {
                return format!("{size:.0} {unit}");
            }
            let text = format!("{size:.1}");
            let text = text.strip_suffix(".0").unwrap_or(&text);
            return format!("{text} {unit}");
        }
        size /= 1024.0;
    }
    format!("{bytes} B")
}

/// `n` with thousands separators: "1,204,332".
pub(crate) fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `n` and its noun: "1 file", "1,204,332 files".
pub(crate) fn counted(n: u64, noun: &str) -> String {
    format!("{} {noun}{}", grouped(n), if n == 1 { "" } else { "s" })
}

/// Keeps the PC from sleeping while a storage job runs on this thread.
pub(crate) struct AwakeGuard;

impl AwakeGuard {
    pub(crate) fn new() -> AwakeGuard {
        // SAFETY: only changes the calling thread's execution state; undone on drop.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
        }
        AwakeGuard
    }
}

impl Drop for AwakeGuard {
    fn drop(&mut self) {
        // SAFETY: clears the requirement this guard set on the same thread.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}

impl std::fmt::Debug for AwakeGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AwakeGuard")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_the_ui() {
        assert_eq!(size_text(512), "512 B");
        assert_eq!(size_text(8192), "8 KB");
        assert_eq!(size_text(64 << 20), "64 MB");
        assert_eq!(size_text(1 << 30), "1 GB");
        assert_eq!(size_text((9 << 30) + (400 << 20)), "9.4 GB");
        assert_eq!(size_text(2000 << 30), "2000 GB");
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1_204_332), "1,204,332");
        assert_eq!(counted(1, "folder"), "1 folder");
        assert_eq!(counted(0, "file"), "0 files");
        assert_eq!(counted(1_204_332, "file"), "1,204,332 files");
    }

    #[test]
    fn the_detail_holds_one_block() {
        let value = detail("scanning", None, Some(json!({"files": 1})), None);
        assert_eq!(value["phase"], "scanning");
        assert!(value["speed"].is_null() && value["duplicates"].is_null());
        assert_eq!(value["scan"]["files"], 1);
        assert!(utc_now().ends_with('Z'));
    }

    #[test]
    fn drive_tests_are_always_forbidden_in_unit_tests() {
        assert!(drive_tests_forbidden());
    }

    #[test]
    fn the_lane_keeps_one_result_per_kind() {
        let config = lane_config();
        assert_eq!(config.lane, "storage");
        assert_eq!(config.keep_results_per_kind, Some(1));
        assert_eq!(config.keep_finished, 10);
        assert!(config.log_dir.ends_with(r"jobs\storage"));
        assert!(data_dir().ends_with("storage"));
    }

    #[test]
    fn busy_on_is_none_without_a_speed_test() {
        assert_eq!(busy_on("C:"), None);
        assert_eq!(busy_on("not a drive"), None);
    }
}
