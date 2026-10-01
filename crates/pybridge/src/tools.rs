//! Python surface of `optimizer_core::tools`.
//!
//! Tool jobs run inside the engine, not on a Python thread: `tools_start` returns once the
//! tool's process runs, and `tools_job`, `tools_jobs` and `tools_cancel` only read or flag
//! in-memory state, so they keep the GIL and return at once.

use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use serde::Serialize;

use optimizer_core::safety::state_log::Journal;
use optimizer_core::tools::{
    self, runner, JobId, JobSnapshot, ToolId, ToolPlan, ToolRequest, MAX_LINES_PER_VIEW,
};

use crate::{blocking, err, to_py};

#[derive(Serialize)]
struct StartResult {
    plan: ToolPlan,
    job: Option<JobSnapshot>,
}

fn parse_tool(tool: &str) -> PyResult<ToolId> {
    ToolId::parse(tool).ok_or_else(|| {
        let valid: Vec<&str> = ToolId::ALL.iter().map(|t| t.as_str()).collect();
        PyValueError::new_err(format!(
            "unknown tool {tool:?}; expected one of: {}",
            valid.join(", ")
        ))
    })
}

/// The maintenance tools in display order: `[{"id", "group" ("system_files" | "drives"),
/// "title", "verb", "description", "needs_volume", "requires_admin", "cancellable",
/// "requires_detach", "changes_system", "duration_hint", "program"}]`. `program` is the
/// file name in System32. Pure data.
#[pyfunction]
fn tools_catalog(py: Python<'_>) -> PyResult<Py<PyAny>> {
    to_py(py, &tools::catalog())
}

/// The fixed drives with a letter, the Windows drive first: `[{"letter" ("C:"), "label",
/// "file_system", "size_bytes", "free_bytes", "media" ("ssd" | "hdd" | "unknown"), "trim"
/// (bool or None), "system", "error", "optimize_blocked", "retrim_blocked",
/// "check_blocked"}]`. Each `*_blocked` is the reason that drive tool cannot run on the
/// drive, or None. Read-only.
#[pyfunction]
fn tools_volumes(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, tools::volumes)
}

/// Plans and, unless `dry_run`, starts a maintenance tool as a background job. `volume`
/// ("C:", "c" or "c:\\") is required for the drive tools and refused for the others.
/// Returns `{"plan": ToolPlan, "job": JobSnapshot or None}`; the job is None in a dry run.
///
/// ToolPlan: `{"tool", "title", "volume", "program" (absolute path), "args",
/// "command_line", "requires_admin", "cancellable", "requires_detach", "changes_system",
/// "duration_hint", "blocked_reason" (None when it can start), "notes"}`.
///
/// A start needs an elevated process and refuses while another tool runs, when the plan is
/// blocked, and once `tools_shutdown` was called (RuntimeError). The run is recorded in the
/// journal's audit log before the process starts; it is never journaled for rollback,
/// because repairs cannot be undone. These tools change only machine-wide state, so they
/// are not refused when this process runs as another account than the signed-in user. An
/// unknown tool or a wrong volume raises ValueError.
#[pyfunction]
#[pyo3(signature = (tool, volume = None, dry_run = false))]
fn tools_start(
    py: Python<'_>,
    tool: &str,
    volume: Option<&str>,
    dry_run: bool,
) -> PyResult<Py<PyAny>> {
    let request = ToolRequest::new(parse_tool(tool)?, volume)
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
    blocking(py, move || {
        let open = || Ok(Arc::new(Journal::open_default()?));
        let (plan, job) = runner().plan_or_start(dry_run, open, &request)?;
        Ok(StartResult { plan, job })
    })
}

/// A job's JobSnapshot with up to 500 output lines numbered after `after`, or None for an
/// unknown id. Lines are numbered from 1: `"lines"`, `"first"` (number of the first line,
/// None when there are none), `"next"` (pass it as `after` next time), `"skipped"` (lines
/// after `after` no longer kept in memory, only in the log file) and `"more"` (more lines
/// are waiting).
///
/// JobSnapshot: `{"id", "tool", "title", "command_line", "volume", "state" ("running" |
/// "succeeded" | "completed" | "attention" | "failed" | "cancelled"), "started_at",
/// "finished_at", "elapsed_ms", "idle_ms", "progress" (0-100 or None), "progress_line",
/// "exit_code", "exit_code_hex", "cancellable", "cancel_requested", "detached",
/// "restart_required", "hint", "summary", "log_path", "raw_log_path", "line_count",
/// "logged"}`. `exit_code` is set as soon as the process has ended, while the state can
/// still be "running" for a moment as the result is judged (after a DISM check that
/// includes reading the component store state, which takes a few seconds).
///
/// Keeps the GIL: it only reads in-memory state and never waits for I/O.
#[pyfunction]
#[pyo3(signature = (job_id, after = 0))]
fn tools_job(py: Python<'_>, job_id: u64, after: u64) -> PyResult<Py<PyAny>> {
    to_py(py, &runner().view(JobId(job_id), after, MAX_LINES_PER_VIEW))
}

/// The running job and the ten newest finished ones as JobSnapshots, newest first. Keeps
/// the GIL: it only reads in-memory state.
#[pyfunction]
fn tools_jobs(py: Python<'_>) -> PyResult<Py<PyAny>> {
    to_py(py, &runner().jobs())
}

/// Asks a running job to stop; it ends within a moment. False when it already finished.
/// Only Check Disk can be stopped; any other tool, and an unknown id, raise RuntimeError.
/// Keeps the GIL: it only sets a flag.
#[pyfunction]
fn tools_cancel(job_id: u64) -> PyResult<bool> {
    runner().cancel(JobId(job_id)).map_err(err)
}

/// Opens the job's readable transcript in Notepad. RuntimeError for an unknown id.
#[pyfunction]
fn tools_open_log(py: Python<'_>, job_id: u64) -> PyResult<()> {
    py.allow_threads(|| runner().open_log(JobId(job_id)).map_err(|e| e.to_string()))
        .map_err(err)
}

/// Called when Cairn closes: refuses new starts; waits up to 10 s for the result of
/// a tool whose process has already ended, and records its exit code when that takes
/// longer; stops a running Check Disk (waiting up to 3 s); and records in the audit log
/// that a tool that cannot be stopped was left running. Returns `[{"id", "tool", "action"
/// ("stopped" | "stop_timed_out" | "left_running" | "may_end_with_app")}]`; "stopped" also
/// covers an ended tool whose exit code was recorded, and a tool that finished on its own
/// while this waited is not listed. Releases the GIL while it waits.
#[pyfunction]
fn tools_shutdown(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let outcomes = py.allow_threads(|| runner().shutdown());
    to_py(py, &outcomes)
}

/// Creates a System Restore point now, in a journal session of its own. Needs an elevated
/// process and System Protection on the system drive. Returns `{"sequence", "description",
/// "created_at"}`; RuntimeError when no restore point was created.
#[pyfunction]
#[pyo3(signature = (description = "Cairn manual checkpoint"))]
fn tools_restore_point(py: Python<'_>, description: &str) -> PyResult<Py<PyAny>> {
    let description = description.trim().to_string();
    if description.is_empty() {
        return Err(PyValueError::new_err(
            "the restore point description must not be empty",
        ));
    }
    blocking(py, move || {
        let journal = Arc::new(Journal::open_default()?);
        tools::create_restore_point_now(journal, &description)
    })
}

/// Built-in Windows tools the Tools section lists: `[{"id", "title", "description",
/// "requires_admin"}]`. Pure data. `tools_open_windows` also opens the tools the Security
/// checkup offers as fixes, which this list leaves out.
#[pyfunction]
fn tools_windows(py: Python<'_>) -> PyResult<Py<PyAny>> {
    to_py(py, &tools::windows_tools())
}

/// Opens a built-in Windows tool from System32 and returns at once: one of `tools_windows`
/// or one of the Security checkup's fix tools. An unknown id raises ValueError; a tool that
/// needs administrator rights from a standard process raises RuntimeError.
#[pyfunction]
fn tools_open_windows(py: Python<'_>, tool_id: &str) -> PyResult<()> {
    if tools::windows_tool(tool_id).is_none() {
        let valid: Vec<&str> = tools::windows_tools()
            .iter()
            .chain(tools::fix_tools())
            .map(|t| t.id)
            .collect();
        return Err(PyValueError::new_err(format!(
            "unknown Windows tool {tool_id:?}; expected one of: {}",
            valid.join(", ")
        )));
    }
    let tool_id = tool_id.to_string();
    py.allow_threads(|| tools::open_windows_tool(&tool_id).map_err(|e| e.to_string()))
        .map_err(err)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(tools_catalog, m)?)?;
    m.add_function(wrap_pyfunction!(tools_volumes, m)?)?;
    m.add_function(wrap_pyfunction!(tools_start, m)?)?;
    m.add_function(wrap_pyfunction!(tools_job, m)?)?;
    m.add_function(wrap_pyfunction!(tools_jobs, m)?)?;
    m.add_function(wrap_pyfunction!(tools_cancel, m)?)?;
    m.add_function(wrap_pyfunction!(tools_open_log, m)?)?;
    m.add_function(wrap_pyfunction!(tools_shutdown, m)?)?;
    m.add_function(wrap_pyfunction!(tools_restore_point, m)?)?;
    m.add_function(wrap_pyfunction!(tools_windows, m)?)?;
    m.add_function(wrap_pyfunction!(tools_open_windows, m)?)?;
    Ok(())
}
