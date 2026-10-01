//! Python surface of `optimizer_core::cleanup`.

use optimizer_core::cleanup;
use optimizer_core::safety::state_log::Journal;
use pyo3::prelude::*;

use crate::{blocking, to_py};

/// Every cleanup target in display order: `[{"id", "title", "description",
/// "requires_admin", "default_on", "recent_files_kept"}, ...]`.
#[pyfunction]
fn cleanup_catalog(py: Python<'_>) -> PyResult<Py<PyAny>> {
    to_py(py, &cleanup::catalog())
}

/// Measures every target without deleting anything. Returns `{"targets": [...],
/// "total_bytes", "duration_ms"}`; each target carries `bytes`, `files`,
/// `blocked_reason` (None when it can be cleaned now), `recent_files_kept` (files changed
/// in the last 24 hours are left in place) and `paths` (the folders and files a clean works
/// on, as display paths).
#[pyfunction]
fn cleanup_scan(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, cleanup::scan)
}

/// Deletes the eligible files of the given target ids and records the run in the journal.
/// Returns `{"results": [...], "freed_bytes", "duration_ms"}`. Deleted files cannot be
/// restored. Raises only when the journal cannot be opened or its session started;
/// problems with a target are reported in that result's `skipped_reason` and `errors`.
#[pyfunction]
fn cleanup_run(py: Python<'_>, ids: Vec<String>) -> PyResult<Py<PyAny>> {
    blocking(py, move || cleanup::clean(&Journal::open_default()?, &ids))
}

/// Registers `cleanup_catalog`, `cleanup_scan` and `cleanup_run` on the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(cleanup_catalog, m)?)?;
    m.add_function(wrap_pyfunction!(cleanup_scan, m)?)?;
    m.add_function(wrap_pyfunction!(cleanup_run, m)?)?;
    Ok(())
}
