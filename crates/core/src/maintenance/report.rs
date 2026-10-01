//! What a maintenance run did: the report stored with its run row, its progress while it runs
//! and the run as the Maintenance section and the CLI show it.
//!
//! Every type here serializes to the JSON the run row keeps (`maintenance_runs.report`,
//! `.progress`, `.request`), so a run written by one copy of Cairn reads back in another.

use serde::{Deserialize, Serialize};

use super::run::{RunOrigin, RunRequest, RunState, StepOutcome};
use crate::safety::state_log::MaintenanceRunRow;
use crate::tools::ToolId;
use crate::win::mutex::MutexPresence;

/// The cleanup step of a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CleanupStep {
    pub outcome: StepOutcome,
    pub freed_bytes: u64,
    pub deleted_files: u64,
    pub skipped_files: u64,
    /// One entry per requested target, in catalog order.
    pub targets: Vec<CleanupTargetResult>,
    /// Why the whole step failed (the cleanup could not start).
    pub error: Option<String>,
}

/// One cleanup target of a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CleanupTargetResult {
    pub id: String,
    pub title: String,
    /// `cleaned`, `skipped` or `failed`.
    pub outcome: String,
    pub freed_bytes: u64,
    pub deleted_files: u64,
    /// Why it was skipped, or its first error.
    pub reason: Option<String>,
}

/// One read-only check of a run (System File Checker or DISM CheckHealth).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckStep {
    pub tool: ToolId,
    pub title: String,
    pub command_line: String,
    pub outcome: StepOutcome,
    /// One sentence about the result.
    pub text: String,
    /// The tool's own verdict line, verbatim and in the language it printed.
    pub windows_message: Option<String>,
    /// What to do about it.
    pub hint: Option<String>,
    pub exit_code_hex: Option<String>,
    pub restart_required: bool,
    /// The tool's transcript.
    pub log_path: Option<String>,
    pub elapsed_ms: u64,
}

/// The result of one run, stored with its run row when it ends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaintenanceReport {
    /// 0 when the run was skipped before its run row was written.
    pub run_id: i64,
    pub origin: RunOrigin,
    pub state: RunState,
    /// RFC 3339, UTC.
    pub started_at: String,
    pub ended_at: Option<String>,
    pub duration_ms: u64,
    pub request: RunRequest,
    pub cleanup: Option<CleanupStep>,
    pub checks: Vec<CheckStep>,
    /// Why the run stopped early or was skipped.
    pub stopped_reason: Option<String>,
    /// Things worth looking at, most important first.
    pub attention: Vec<String>,
    /// The result in one line, parts joined with "  ·  ".
    pub headline: String,
    /// The run's transcript.
    pub log_path: Option<String>,
}

/// Where a running run is, stored with its run row at most every few seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunProgress {
    /// `cleanup`, `system_files`, `component_store` or `finishing`.
    pub step: String,
    /// What the step does now, for example "Checking system files…".
    pub title: String,
    /// 1-based number of the step.
    pub index: u32,
    pub count: u32,
    pub percent: Option<f64>,
    /// RFC 3339, UTC.
    pub updated_at: String,
}

/// A run as its row stores it, with the JSON columns read back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaintenanceRun {
    pub id: i64,
    pub origin: RunOrigin,
    pub state: RunState,
    /// RFC 3339, UTC.
    pub started_at: String,
    pub ended_at: Option<String>,
    pub request: RunRequest,
    pub progress: Option<RunProgress>,
    pub report: Option<MaintenanceReport>,
    pub log_path: Option<String>,
    pub acknowledged: bool,
    /// The row says `running`, but no administrator holds the run lock: the run ended
    /// without finishing. It is shown as interrupted; the row is closed when the run is
    /// marked as seen or the next run starts.
    pub stale: bool,
}

impl MaintenanceRun {
    /// Reads a run row. `lock` is who holds the run lock now; a `running` row is stale
    /// unless an administrator does. Unreadable JSON columns read as absent.
    pub(crate) fn from_row(row: &MaintenanceRunRow, lock: MutexPresence) -> MaintenanceRun {
        let state = RunState::parse(&row.state).unwrap_or(RunState::Interrupted);
        let origin = RunOrigin::parse(&row.origin).unwrap_or(RunOrigin::Task);
        let request =
            serde_json::from_str::<RunRequest>(&row.request_json).unwrap_or_else(|_| RunRequest {
                targets: Vec::new(),
                system_file_check: false,
                component_store_check: false,
                origin,
            });
        MaintenanceRun {
            id: row.id,
            origin,
            state,
            started_at: row.started_at.clone(),
            ended_at: row.ended_at.clone(),
            request,
            progress: parse(&row.progress_json),
            report: parse(&row.report_json),
            log_path: row.log_path.clone(),
            acknowledged: row.acknowledged_at.is_some(),
            stale: state == RunState::Running && lock != MutexPresence::Admin,
        }
    }
}

/// A JSON column read back; absent when it is empty or unreadable.
fn parse<T: serde::de::DeserializeOwned>(json: &Option<String>) -> Option<T> {
    json.as_deref().and_then(|j| serde_json::from_str(j).ok())
}

/// A byte count as the UI's cleanup view shows it: 1024 as the step, whole B and KB, one
/// decimal for MB and GB ("512 B", "3 KB", "324.1 MB", "1.2 GB").
pub fn fmt_size(bytes: u64) -> String {
    let step = 1024.0;
    let mut size = bytes as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if size < step || unit == "GB" {
            return if unit == "B" || unit == "KB" {
                format!("{size:.0} {unit}")
            } else {
                format!("{size:.1} {unit}")
            };
        }
        size /= step;
    }
    format!("{size:.1} GB")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: &str) -> MaintenanceRunRow {
        MaintenanceRunRow {
            id: 7,
            started_at: "2026-09-27T12:00:00Z".into(),
            ended_at: None,
            state: state.into(),
            origin: "task".into(),
            request_json: r#"{"targets":["user_temp"],"system_file_check":true,"component_store_check":false,"origin":"task"}"#.into(),
            progress_json: Some("not json".into()),
            report_json: None,
            log_path: None,
            acknowledged_at: None,
        }
    }

    #[test]
    fn sizes_follow_the_ui_format() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(3 * 1024), "3 KB");
        assert_eq!(fmt_size(339_847_578), "324.1 MB");
        assert_eq!(fmt_size(1_288_490_189), "1.2 GB");
        assert_eq!(fmt_size(5 * 1024 * 1024 * 1024 * 1024), "5120.0 GB");
    }

    #[test]
    fn running_rows_are_stale_unless_an_administrator_holds_the_lock() {
        let admin = MaintenanceRun::from_row(&row("running"), MutexPresence::Admin);
        assert!(!admin.stale);
        for lock in [MutexPresence::Absent, MutexPresence::Other] {
            assert!(
                MaintenanceRun::from_row(&row("running"), lock).stale,
                "{lock:?}"
            );
        }
        let done = MaintenanceRun::from_row(&row("completed"), MutexPresence::Absent);
        assert!(!done.stale);
        assert_eq!(done.state, RunState::Completed);
        assert_eq!(done.request.targets, ["user_temp"]);
        assert!(done.request.system_file_check);
        assert_eq!(done.progress, None, "unreadable JSON reads as absent");
        assert!(!done.acknowledged);
    }

    #[test]
    fn unknown_states_read_as_interrupted() {
        let run = MaintenanceRun::from_row(&row("exploded"), MutexPresence::Admin);
        assert_eq!(run.state, RunState::Interrupted);
        assert!(!run.stale);
    }
}
