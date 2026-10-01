//! Python surface of `optimizer_core::storage`.
//!
//! Speed tests, space scans and duplicate searches run inside the engine as jobs of the
//! "storage" lane, one at a time: a start returns once its job runs, and `storage_job`,
//! `storage_jobs`, `storage_cancel`, `storage_result` and `storage_scan_children` only read
//! or flag in-memory state, so they keep the GIL and return at once.
//!
//! Job snapshots have the job host's 25 keys: `{"id", "lane" ("storage"), "kind"
//! ("speed_test" | "space_scan" | "duplicates"), "title", "command_line", "state"
//! ("running" | "succeeded" | "cancelled" | "failed"), "started_at", "finished_at",
//! "elapsed_ms", "idle_ms", "progress" (0-100 or None), "progress_line", "cancellable",
//! "cancel_requested", "detached", "restart_required", "summary", "hint", "notes", "detail",
//! "log_path", "line_count", "logged", "has_result", "result_revision"}`. `detail` is
//! `{"phase", "speed", "scan", "duplicates"}`, where the block of the job's kind is set and
//! the other two are None:
//! - `speed`: `{"test", "direction", "run", "runs", "step", "steps", "live_mb_s",
//!   "bytes_written", "done": [Measurement]}`;
//! - `scan`: `{"files", "folders", "logical_bytes", "allocated_bytes", "denied_folders",
//!   "current"}`;
//! - `duplicates`: `{"files_total", "files_done", "bytes_total", "bytes_done",
//!   "groups_found", "current"}`.
//!
//! Phases: speed `preparing`, `measuring`, `pausing`, `cleaning_up`, `saving`; scan
//! `scanning`, `summarizing`; duplicates `sampling`, `hashing`; every kind `done` at the end.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use serde::Serialize;
use serde_json::Value;

use optimizer_core::jobs::{HostJobId, HostJobSnapshot, JobState};
use optimizer_core::safety::state_log::Journal;
use optimizer_core::storage::duplicates::MIN_SIZE;
use optimizer_core::storage::scan::CHILDREN_LIMIT_MAX;
use optimizer_core::storage::speed::{check_folder_name, MAX_SIZE, MIB};
use optimizer_core::storage::{
    self, plan_or_start_duplicates, plan_or_start_scan, plan_or_start_speed_test,
    remove_leftover_opening, speed_history, DuplicatesRequest, ScanRequest, ScanResult, SortOrder,
    SpeedEnv, SpeedTestRequest,
};

use crate::{blocking, err, to_py};

/// Smallest test size a caller may ask for.
const MIN_TEST_SIZE: u64 = 16 * MIB;

#[derive(Serialize)]
struct StartResult<P: Serialize> {
    plan: P,
    job: Option<HostJobSnapshot>,
}

fn value_error(e: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(e.to_string())
}

/// The fixed volumes with a letter, the Windows volume first: `[{"letter" ("C:"), "label",
/// "file_system", "size_bytes", "free_bytes", "media" ("ssd" | "hdd" | "unknown"), "bus",
/// "model", "system", "read_only", "persistent_acls", "not_responding", "error",
/// "speed_test_blocked", "scan_blocked", "leftovers": [{"path", "bytes", "in_use",
/// "other_entries"}]}]`. `leftovers` are test folders a speed test left at the volume's root.
/// Read-only.
#[pyfunction]
fn storage_volumes(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, storage::volumes)
}

/// Plans and, unless `dry_run`, starts a disk speed test of `volume` ("C:", "c" or "c:\\")
/// with a test file of `size_bytes` (a whole number of MiB from 16 MiB to 64 GiB) and `runs`
/// runs (1 to 9) of each measurement. Returns `{"plan": SpeedTestPlan, "job": snapshot or
/// None}`; the job is None in a dry run.
///
/// SpeedTestPlan: `{"volume", "size_bytes", "runs", "tests": [{"id", "label", "block_bytes",
/// "queue_depth", "random"}], "max_write_bytes", "estimated_seconds", "folder_pattern",
/// "requires_admin", "media", "leftovers", "blocked_reason" (None when it can start),
/// "notes"}`.
///
/// A start needs an elevated process and refuses a blocked plan (RuntimeError with the
/// reason) before anything is written. The test is recorded in the audit log
/// (`disk_speed_test`) before its folder is created; it is never journaled for rollback,
/// since it leaves nothing behind. Bad arguments raise ValueError.
#[pyfunction]
#[pyo3(signature = (volume, size_bytes = 1073741824, runs = 3, dry_run = false))]
fn storage_speed_start(
    py: Python<'_>,
    volume: &str,
    size_bytes: i64,
    runs: i64,
    dry_run: bool,
) -> PyResult<Py<PyAny>> {
    let size = u64::try_from(size_bytes)
        .ok()
        .filter(|s| (MIN_TEST_SIZE..=MAX_SIZE).contains(s) && s % MIB == 0)
        .ok_or_else(|| {
            value_error(format!(
                "size_bytes must be a whole number of MiB from 16 MiB to 64 GiB, got {size_bytes}"
            ))
        })?;
    let runs =
        u32::try_from(runs).map_err(|_| value_error(format!("runs must be 1 to 9, got {runs}")))?;
    let request = SpeedTestRequest::new(volume, size, runs).map_err(value_error)?;
    blocking(py, move || {
        let open = || Ok(Arc::new(Journal::open_default()?));
        let (plan, job) =
            plan_or_start_speed_test(storage::lane(), &SpeedEnv::SYSTEM, &request, dry_run, open)?;
        Ok(StartResult { plan, job })
    })
}

/// Earlier speed-test results, newest first (at most 50): `[SpeedTestResult]` with
/// `{"volume", "label", "model", "bus", "media", "file_system", "size_bytes", "runs",
/// "started_at", "finished_at", "elapsed_ms", "completed", "error" (only for a test an error
/// ended early), "measurements": [{"test", "label", "direction", "block_bytes", "queue_depth",
/// "threads", "mb_s", "iops", "latency_us", "runs", "duration_ms", "bytes"}], "skipped",
/// "bytes_written", "notes", "app_version", "history_saved"}`. RuntimeError when the history
/// file cannot be read.
#[pyfunction]
fn storage_speed_history(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, || speed_history(&storage::data_dir()))
}

/// Removes a test folder a speed test left at a volume root: its test file, then the folder
/// when it holds nothing else. Returns `{"path", "removed", "bytes", "detail"}`. Needs an
/// elevated process (RuntimeError); a path that does not name a speed-test folder raises
/// ValueError. A refused call opens no journal; an allowed one is recorded in the audit log
/// (`remove_speed_test_file`) before anything is touched.
#[pyfunction]
fn storage_remove_leftover(py: Python<'_>, path: &str) -> PyResult<Py<PyAny>> {
    let folder = PathBuf::from(path.trim());
    check_folder_name(&folder).map_err(value_error)?;
    blocking(py, move || {
        remove_leftover_opening(&SpeedEnv::SYSTEM, &folder, Journal::open_default)
    })
}

/// Plans and, unless `dry_run`, starts a scan of the folder `path` (an absolute path on one
/// of this PC's fixed drives). Returns `{"plan": ScanPlan, "job": snapshot or None}`.
///
/// ScanPlan: `{"root", "volume", "whole_volume", "file_system", "media", "threads",
/// "elevated", "blocked_reason", "notes"}`. Read-only: it writes no audit rows. A blocked
/// start raises RuntimeError with the reason; an empty or relative path raises ValueError.
#[pyfunction]
#[pyo3(signature = (path, dry_run = false))]
fn storage_scan_start(py: Python<'_>, path: &str, dry_run: bool) -> PyResult<Py<PyAny>> {
    let path = path.trim();
    if path.is_empty() || !Path::new(path).is_absolute() {
        return Err(value_error(format!(
            "the folder to scan must be an absolute path, got {path:?}"
        )));
    }
    let request = ScanRequest {
        path: PathBuf::from(path),
    };
    blocking(py, move || {
        let (plan, job) = plan_or_start_scan(storage::lane(), &request, dry_run)?;
        Ok(StartResult { plan, job })
    })
}

/// Plans and, unless `dry_run`, starts a search for identical files of at least `min_size`
/// bytes (1 MiB or more) among the files of the finished scan `scan_job`. Returns `{"plan":
/// DuplicatesPlan, "job": snapshot or None}`.
///
/// DuplicatesPlan: `{"scan_job", "root", "min_size", "candidates", "size_groups",
/// "bytes_to_read_max", "blocked_reason", "notes"}`. Read-only. A blocked start raises
/// RuntimeError with the reason; a `min_size` below 1 MiB raises ValueError.
#[pyfunction]
#[pyo3(signature = (scan_job, min_size = 1048576, dry_run = false))]
fn storage_duplicates_start(
    py: Python<'_>,
    scan_job: u64,
    min_size: i64,
    dry_run: bool,
) -> PyResult<Py<PyAny>> {
    let min_size = u64::try_from(min_size)
        .ok()
        .filter(|m| *m >= MIN_SIZE)
        .ok_or_else(|| value_error(format!("min_size must be at least 1 MiB, got {min_size}")))?;
    let request = DuplicatesRequest::new(HostJobId(scan_job), min_size).map_err(value_error)?;
    blocking(py, move || {
        let (plan, job) = plan_or_start_duplicates(storage::lane(), &request, dry_run)?;
        Ok(StartResult { plan, job })
    })
}

/// The storage job's snapshot, or None for an unknown id. Keeps the GIL.
#[pyfunction]
fn storage_job(py: Python<'_>, job_id: u64) -> PyResult<Py<PyAny>> {
    to_py(py, &storage::lane().snapshot(HostJobId(job_id)))
}

/// The running storage job and the retained finished ones, newest first. Keeps the GIL.
#[pyfunction]
fn storage_jobs(py: Python<'_>) -> PyResult<Py<PyAny>> {
    to_py(py, &storage::lane().jobs())
}

/// Asks a running storage job to stop; it ends within a moment (a speed test deletes its
/// test file first). False when it already finished; RuntimeError for an unknown id. Keeps
/// the GIL: it only sets a flag.
#[pyfunction]
fn storage_cancel(job_id: u64) -> PyResult<bool> {
    storage::lane().cancel(HostJobId(job_id)).map_err(err)
}

/// The result of a finished storage job with its `"revision"`, or None while it runs, for an
/// unknown id and once a newer job of its kind replaced it. Keeps the GIL.
///
/// - speed test: `{"kind": "speed", **SpeedTestResult}`;
/// - scan: `{"kind": "scan", "job_id", "summary", "root": TreeRow, "largest_files"
///   (by size on disk), "largest_files_by_size", "warnings"}`;
/// - duplicates: `{"kind": "duplicates", "job_id", "scan_job", "root", "min_size",
///   "completed", "groups": [{"size", "count", "wasted", "hash", "files": [{"path",
///   "modified", "allocated"}], "more_files"}], "group_count", "wasted_bytes",
///   "files_compared", "bytes_read", "skipped_in_use", "skipped_unreadable",
///   "skipped_changed", "skipped_online_only"}`.
#[pyfunction]
fn storage_result(py: Python<'_>, job_id: u64) -> PyResult<Py<PyAny>> {
    let id = HostJobId(job_id);
    let lane = storage::lane();
    if lane
        .snapshot(id)
        .map_or(true, |job| job.state == JobState::Running)
    {
        return Ok(py.None());
    }
    let Some((revision, published)) = lane.result(id, 0) else {
        return Ok(py.None());
    };
    let mut value = (*published).clone();
    if let Value::Object(map) = &mut value {
        map.insert("revision".to_string(), Value::from(revision));
    }
    to_py(py, &value)
}

/// One folder of a finished scan: `{"node": TreeRow, "order", "children": [TreeRow],
/// "total"}`, the entries sorted by `order` ("allocated" = size on disk, or "logical" =
/// size), largest first, at most `limit` (1 to 5000) of them, the rest as one "more" row.
/// None for an unknown job, a job that is not a scan, a scan whose result was cleared by a
/// newer scan, and an unknown node. Keeps the GIL.
///
/// TreeRow: `{"kind" ("folder" | "link" | "file" | "small_files" | "more"), "node", "name",
/// "path", "logical", "allocated", "online_only", "files", "folders", "count",
/// "has_children", "denied", "error", "modified"}`.
#[pyfunction]
#[pyo3(signature = (job_id, node, order = "allocated", limit = 500))]
fn storage_scan_children(
    py: Python<'_>,
    job_id: u64,
    node: u64,
    order: &str,
    limit: i64,
) -> PyResult<Py<PyAny>> {
    let order = SortOrder::parse(order).ok_or_else(|| {
        value_error(format!(
            "order must be 'allocated' or 'logical', got {order:?}"
        ))
    })?;
    let limit = usize::try_from(limit)
        .ok()
        .filter(|l| (1..=CHILDREN_LIMIT_MAX).contains(l))
        .ok_or_else(|| value_error(format!("limit must be 1 to 5000, got {limit}")))?;
    let page = u32::try_from(node).ok().and_then(|node| {
        storage::lane()
            .typed::<ScanResult>(HostJobId(job_id))
            .and_then(|scan| scan.children(node, order, limit))
    });
    to_py(py, &page)
}

/// Called when Cairn closes: refuses new starts and stops the running storage job, waiting a
/// few seconds at most. Returns `[{"id", "kind", "action" ("stopped" | "stop_timed_out" |
/// "left_running")}]`; a job that finished on its own is not listed. Releases the GIL while
/// it waits.
#[pyfunction]
fn storage_shutdown(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let outcomes = py.allow_threads(|| storage::lane().shutdown());
    to_py(py, &outcomes)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(storage_volumes, m)?)?;
    m.add_function(wrap_pyfunction!(storage_speed_start, m)?)?;
    m.add_function(wrap_pyfunction!(storage_speed_history, m)?)?;
    m.add_function(wrap_pyfunction!(storage_remove_leftover, m)?)?;
    m.add_function(wrap_pyfunction!(storage_scan_start, m)?)?;
    m.add_function(wrap_pyfunction!(storage_duplicates_start, m)?)?;
    m.add_function(wrap_pyfunction!(storage_job, m)?)?;
    m.add_function(wrap_pyfunction!(storage_jobs, m)?)?;
    m.add_function(wrap_pyfunction!(storage_cancel, m)?)?;
    m.add_function(wrap_pyfunction!(storage_result, m)?)?;
    m.add_function(wrap_pyfunction!(storage_scan_children, m)?)?;
    m.add_function(wrap_pyfunction!(storage_shutdown, m)?)?;
    Ok(())
}
