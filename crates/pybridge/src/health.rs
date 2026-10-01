//! Python surface of `optimizer_core::health`.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use optimizer_core::health::{self, MAX_BOOT_LIMIT};

use crate::{blocking, err, to_py};

/// Security checkup; read-only, nothing is journaled and no elevation is needed (drive
/// encryption is only checked when elevated). Runs without the GIL for up to about 8 s.
/// Returns `{"taken_at", "duration_ms", "elevated", "other_user", "home_edition", "score",
/// "checks", "update_scan", "update_scan_due", "notes", "errors"}`:
///
/// - `score`: `{"value" (0-100), "grade" ("good" | "fair" | "at_risk"), "to_fix",
///   "critical", "unknown", "checked"}`.
/// - `checks`: 24 checks in a fixed order, each `{"id", "group", "title", "state"
///   ("good" | "attention" | "checking" | "unknown" | "not_applicable"), "severity"
///   ("info" | "low" | "medium" | "high" | "critical"), "summary", "detail", "facts"
///   [{"label", "value"}], "fixes" [{"label", "note", "action"}], "needs_admin",
///   "per_user"}`; an action is `{"kind": "uri", "uri"}`, `{"kind": "windows_tool",
///   "tool", "requires_admin"}`, `{"kind": "tweak", "id"}`, `{"kind": "update_scan",
///   "online"}` or `{"kind": "elevate"}`.
/// - `update_scan`: the latest Windows Update search (see `health_update_scan`);
///   `update_scan_due` is true when none finished in the last 30 minutes and none runs.
/// - `errors`: `[{"source", "message"}]` of the sources that could not be read.
#[pyfunction]
fn health_security_checkup(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, || Ok(health::security_checkup()))
}

/// Starts a search for updates that are waiting to install on an engine thread and returns
/// its view at once; keeps the GIL. `online=False` searches Windows Update's cached data,
/// `online=True` asks Windows Update's servers (it only searches: nothing is downloaded or
/// installed). RuntimeError while a search runs.
#[pyfunction]
#[pyo3(signature = (online=false))]
fn health_update_scan_start(py: Python<'_>, online: bool) -> PyResult<Py<PyAny>> {
    let view = health::start_update_scan(online).map_err(err)?;
    to_py(py, &view)
}

/// The latest Windows Update search; keeps the GIL and reads in-memory state only.
/// `{"state" ("idle" | "running" | "done" | "failed" | "cancelled"), "online",
/// "started_at", "finished_at", "elapsed_ms", "updates" [{"title", "kb", "msrc_severity",
/// "security", "downloaded", "released_at"}], "error"}`.
#[pyfunction]
fn health_update_scan(py: Python<'_>) -> PyResult<Py<PyAny>> {
    to_py(py, &health::update_scan())
}

/// Asks a running Windows Update search to stop; keeps the GIL and never waits. False when
/// none runs.
#[pyfunction]
fn health_update_scan_cancel() -> bool {
    health::cancel_update_scan()
}

/// Start and shutdown history from the Diagnostics-Performance log, which only
/// administrators can read (`access` is then "needs_admin" and the lists are empty).
/// Read-only; runs without the GIL. ValueError unless 1 <= limit <= 500.
/// Returns `{"read_at", "access" ("ok" | "needs_admin" | "log_disabled" | "log_missing"),
/// "boots", "shutdowns", "slow_items", "unexpected_shutdowns", "stats", "fast_startup",
/// "startup_entries", "notes", "errors"}`; boots and shutdowns are newest first,
/// `startup_entries` are the startup entries (as `startup_list` returns them) that slow
/// apps belong to.
#[pyfunction]
#[pyo3(signature = (limit=60))]
fn health_boot_history(py: Python<'_>, limit: i64) -> PyResult<Py<PyAny>> {
    let limit = boot_limit(limit)?;
    blocking(py, move || health::boot_history(limit))
}

/// `limit` as a count of starts; ValueError outside 1 to `MAX_BOOT_LIMIT`, negative values
/// included.
fn boot_limit(limit: i64) -> PyResult<usize> {
    usize::try_from(limit)
        .ok()
        .filter(|l| (1..=MAX_BOOT_LIMIT).contains(l))
        .ok_or_else(|| {
            PyValueError::new_err(format!(
                "limit must be between 1 and {MAX_BOOT_LIMIT}, got {limit}"
            ))
        })
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(health_security_checkup, m)?)?;
    m.add_function(wrap_pyfunction!(health_update_scan_start, m)?)?;
    m.add_function(wrap_pyfunction!(health_update_scan, m)?)?;
    m.add_function(wrap_pyfunction!(health_update_scan_cancel, m)?)?;
    m.add_function(wrap_pyfunction!(health_boot_history, m)?)?;
    Ok(())
}
