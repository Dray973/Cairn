//! Python surface of `optimizer_core::maintenance`.

use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use optimizer_core::maintenance::{self, ScheduleConfig};
use optimizer_core::safety::state_log::Journal;

use crate::{blocking, err, to_py};

/// Everything the Maintenance section shows. Read-only; Task Scheduler errors become
/// warnings. Returns `{"elevated", "account" {"sid", "name", "other_user",
/// "service_account"}, "program" {"path", "safe", "reason"}, "task_path", "task" (None or
/// {"enabled", "state", "config", "drift", "program", "last_run_time", "last_result",
/// "last_result_hex", "last_result_text", "next_run_time", "missed_runs"}), "recorded",
/// "defaults", "targets", "runs", "running", "on_battery", "has_battery", "blocked_reason",
/// "warnings"}`. A run is `{"id", "origin", "state", "started_at", "ended_at", "request",
/// "progress", "report", "log_path", "acknowledged", "stale"}`.
#[pyfunction]
fn maintenance_status(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, maintenance::status)
}

/// A schedule from a dict or a JSON string: `{"day": "sunday", "time": "12:00", "targets":
/// [...], "system_file_check": bool, "component_store_check": bool}`, checked.
fn parse_config(py: Python<'_>, config: &Bound<'_, PyAny>) -> PyResult<ScheduleConfig> {
    let text = if let Ok(s) = config.extract::<String>() {
        s
    } else {
        let json = py.import("json")?;
        json.call_method1("dumps", (config,))?.extract::<String>()?
    };
    let config: ScheduleConfig = serde_json::from_str(&text)
        .map_err(|e| PyValueError::new_err(format!("invalid maintenance schedule: {e}")))?;
    config
        .validated()
        .map_err(|e| PyValueError::new_err(e.to_string()))
}

/// Turns scheduled maintenance on or saves it. `config` is a dict or JSON string (see
/// `maintenance_status()["defaults"]`). With `dry_run=True` returns the plan `{"config",
/// "task_path", "program", "arguments", "account", "next_run", "creates", "unchanged",
/// "blocked_reason", "notes"}` and opens no session; otherwise the task is recorded in the
/// journal before Task Scheduler registers it and `{"session_id", "task_path", "outcome"
/// (created, updated or unchanged), "next_run"}` is returned. A malformed schedule, an
/// unknown target and the Recycle Bin raise ValueError; refusals raise RuntimeError.
#[pyfunction]
#[pyo3(signature = (config, dry_run = false))]
fn maintenance_set_schedule(
    py: Python<'_>,
    config: &Bound<'_, PyAny>,
    dry_run: bool,
) -> PyResult<Py<PyAny>> {
    let config = parse_config(py, config)?;
    blocking(py, move || {
        let journal = Arc::new(Journal::open_default()?);
        maintenance::set_schedule(journal, &config, dry_run)
    })
}

/// Deletes this account's maintenance task when the journal has no record of it (the journal
/// was reset). Irreversible; logged before the delete. Returns `{"task_path", "removed"
/// (None in a dry run), "planned"}`.
#[pyfunction]
#[pyo3(signature = (dry_run = false))]
fn maintenance_remove_unrecorded(py: Python<'_>, dry_run: bool) -> PyResult<Py<PyAny>> {
    blocking(py, move || {
        maintenance::remove_unrecorded(&Journal::open_default()?, dry_run)
    })
}

/// Starts the scheduled task now; the run happens in the task's own process and keeps
/// running when Cairn closes. Returns `{"requested": True, "task_path"}`.
#[pyfunction]
fn maintenance_run_now(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, || maintenance::run_now(&Journal::open_default()?))
}

fn check_run_id(run_id: i64) -> PyResult<()> {
    if run_id < 1 {
        Err(PyValueError::new_err(format!(
            "run_id must be a positive run id, got {run_id}"
        )))
    } else {
        Ok(())
    }
}

/// Marks a run that is over as seen. False when it was seen before, is in progress or does
/// not exist. When the row of run `run_id` is still running while no administrator holds the
/// run lock, that run ended without finishing; only that run is closed as interrupted first,
/// so it can be marked. Every other run is left as it is.
#[pyfunction]
fn maintenance_acknowledge(py: Python<'_>, run_id: i64) -> PyResult<bool> {
    check_run_id(run_id)?;
    py.allow_threads(|| {
        Journal::open_default()
            .and_then(|journal| maintenance::acknowledge(&journal, run_id))
            .map_err(|e| e.to_string())
    })
    .map_err(err)
}

/// Opens a run's transcript in Notepad.
#[pyfunction]
fn maintenance_open_log(py: Python<'_>, run_id: i64) -> PyResult<()> {
    check_run_id(run_id)?;
    py.allow_threads(|| {
        Journal::open_default()
            .and_then(|journal| maintenance::open_log(&journal, run_id))
            .map_err(|e| e.to_string())
    })
    .map_err(err)
}

/// Starts the engine's run monitor once and returns at once. With `expect_start=True` a run
/// is expected within a minute (after Run now).
#[pyfunction]
#[pyo3(signature = (expect_start = false))]
fn maintenance_watch(expect_start: bool) {
    maintenance::monitor().watch(expect_start);
}

/// The run monitor's latest observation `{"running", "waiting_for_start",
/// "start_timed_out", "run", "observed_at", "error"}`, or None before its first read. No
/// I/O.
#[pyfunction]
fn maintenance_progress(py: Python<'_>) -> PyResult<Py<PyAny>> {
    match maintenance::monitor().latest() {
        Some(observation) => to_py(py, &observation),
        None => Ok(py.None()),
    }
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(maintenance_status, m)?)?;
    m.add_function(wrap_pyfunction!(maintenance_set_schedule, m)?)?;
    m.add_function(wrap_pyfunction!(maintenance_remove_unrecorded, m)?)?;
    m.add_function(wrap_pyfunction!(maintenance_run_now, m)?)?;
    m.add_function(wrap_pyfunction!(maintenance_acknowledge, m)?)?;
    m.add_function(wrap_pyfunction!(maintenance_open_log, m)?)?;
    m.add_function(wrap_pyfunction!(maintenance_watch, m)?)?;
    m.add_function(wrap_pyfunction!(maintenance_progress, m)?)?;
    Ok(())
}
