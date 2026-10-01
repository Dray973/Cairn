//! `optimizer_engine`: Python extension module (PyO3, abi3-py312) over `optimizer_core`.
//!
//! Long-running calls release the GIL. Results are returned as plain Python
//! dicts/lists built from the core's serde representation, so the UI never needs
//! to know about Rust types. Invalid arguments raise `ValueError`; engine failures
//! raise `RuntimeError` carrying the engine's error text.

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use pyo3::IntoPyObjectExt;
use serde::Serialize;

mod app;
mod cleanup;
mod health;
mod maintenance;
mod network;
mod permissions;
mod profiles;
mod startup;
mod storage;
mod sysinfo;
mod tools;
mod updates;

use optimizer_core::debloat::{self, ApplyOptions, Category, Engine};
use optimizer_core::safety::{self, state_log::Journal, RestorePointPolicy};

fn json_to_py(py: Python<'_>, v: &serde_json::Value) -> PyResult<Py<PyAny>> {
    use serde_json::Value;
    match v {
        Value::Null => Ok(py.None()),
        Value::Bool(b) => b.into_py_any(py),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_py_any(py)
            } else if let Some(u) = n.as_u64() {
                u.into_py_any(py)
            } else {
                n.as_f64().unwrap_or(f64::NAN).into_py_any(py)
            }
        }
        Value::String(s) => s.into_py_any(py),
        Value::Array(items) => {
            let converted = items
                .iter()
                .map(|i| json_to_py(py, i))
                .collect::<PyResult<Vec<_>>>()?;
            Ok(PyList::new(py, converted)?.into_any().unbind())
        }
        Value::Object(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, json_to_py(py, v)?)?;
            }
            Ok(dict.into_any().unbind())
        }
    }
}

pub(crate) fn to_py<T: Serialize>(py: Python<'_>, value: &T) -> PyResult<Py<PyAny>> {
    let json = serde_json::to_value(value).map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    json_to_py(py, &json)
}

pub(crate) fn err(e: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

/// Runs a blocking core operation without holding the GIL and converts its result.
pub(crate) fn blocking<T, F>(py: Python<'_>, f: F) -> PyResult<Py<PyAny>>
where
    T: Serialize + Send,
    F: FnOnce() -> optimizer_core::Result<T> + Send,
{
    let value = py
        .allow_threads(|| f().map_err(|e| e.to_string()))
        .map_err(err)?;
    to_py(py, &value)
}

fn parse_restore_point(value: &str) -> PyResult<RestorePointPolicy> {
    match value.trim().to_ascii_lowercase().as_str() {
        "skip" => Ok(RestorePointPolicy::Skip),
        "try" => Ok(RestorePointPolicy::Try),
        "require" => Ok(RestorePointPolicy::Require),
        _ => Err(PyValueError::new_err(format!(
            "restore_point must be 'skip', 'try' or 'require', got {value:?}"
        ))),
    }
}

fn parse_category(value: &str) -> PyResult<Category> {
    Category::parse(value).ok_or_else(|| {
        let valid: Vec<&str> = Category::ALL.iter().map(|c| c.label()).collect();
        PyValueError::new_err(format!(
            "unknown category {value:?}; expected one of: {}",
            valid.join(", ")
        ))
    })
}
#[pyfunction]
fn version() -> &'static str {
    optimizer_core::VERSION
}

#[pyfunction]
fn is_elevated() -> bool {
    optimizer_core::is_elevated()
}

/// True when this process runs as a different account than the user signed in to its
/// session (UAC approved with another administrator's credentials); False when the session
/// has no signed-in user. While it is True, applying per-user changes is refused (HKCU
/// settings, Store app removal, per-user startup entries), because they would land in that
/// account's profile. Reverts are not refused: they restore the records of the journal of
/// the account this process runs as, in that account's profile. Raises `RuntimeError` when
/// the two accounts cannot be compared; applying per-user changes is refused then as well.
#[pyfunction]
fn elevated_as_other_user() -> PyResult<bool> {
    optimizer_core::win::session::elevated_as_other_user().map_err(err)
}

#[pyfunction]
fn journal_path() -> String {
    safety::state_log::default_path().display().to_string()
}

#[pyfunction]
fn system_restore_enabled() -> PyResult<bool> {
    safety::is_system_restore_enabled().map_err(err)
}

#[pyfunction]
#[pyo3(signature = (drive = "C:\\"))]
fn enable_system_restore(py: Python<'_>, drive: &str) -> PyResult<()> {
    let drive = drive.to_string();
    py.allow_threads(|| {
        safety::restore_point::enable_system_restore(&drive).map_err(|e| e.to_string())
    })
    .map_err(err)
}

/// Creates a System Restore point. Returns `{"sequence", "description", "created_at"}`.
#[pyfunction]
#[pyo3(signature = (description = safety::restore_point::DEFAULT_DESCRIPTION, force = true))]
fn create_restore_point(py: Python<'_>, description: &str, force: bool) -> PyResult<Py<PyAny>> {
    let description = description.to_string();
    blocking(py, move || {
        safety::create_restore_point(&description, force)
    })
}

#[pyfunction]
fn journal_summary(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, || Journal::open_default()?.summary())
}

#[pyfunction]
fn journal_export_json(py: Python<'_>) -> PyResult<String> {
    py.allow_threads(|| {
        Journal::open_default()
            .and_then(|j| j.export_json())
            .map_err(|e| e.to_string())
    })
    .map_err(err)
}

/// Reverts every active journal record. With `dry_run=True` nothing changes and the
/// returned `actions` list describes the plan. `restart` names the restart the restored
/// tweaks need ("none", "explorer", "sign_out" or "restart").
#[pyfunction]
#[pyo3(signature = (dry_run = false))]
fn rollback_to_baseline(py: Python<'_>, dry_run: bool) -> PyResult<Py<PyAny>> {
    revert_all(py, dry_run)
}

/// Every tweak and bloatware package pattern in catalog order, without scanning.
#[pyfunction]
fn catalog(py: Python<'_>) -> PyResult<Py<PyAny>> {
    to_py(py, &debloat::catalog_view())
}

/// Category labels in display order.
#[pyfunction]
fn categories() -> Vec<&'static str> {
    Category::ALL.iter().map(|c| c.label()).collect()
}

/// Current state of every catalog item and installed bloatware package. Read-only.
#[pyfunction]
fn scan(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, || Engine::open_default()?.scan())
}

/// Applies the given item ids. `restore_point` is "skip", "try" or "require".
#[pyfunction]
#[pyo3(signature = (ids, restore_point = "try", dry_run = false))]
fn apply(
    py: Python<'_>,
    ids: Vec<String>,
    restore_point: &str,
    dry_run: bool,
) -> PyResult<Py<PyAny>> {
    let opts = ApplyOptions {
        restore_point: parse_restore_point(restore_point)?,
        dry_run,
    };
    blocking(py, move || Engine::open_default()?.apply(&ids, &opts))
}

/// Applies every recommended item of `category` that is not applied yet.
#[pyfunction]
#[pyo3(signature = (category, restore_point = "try", dry_run = false))]
fn apply_category(
    py: Python<'_>,
    category: &str,
    restore_point: &str,
    dry_run: bool,
) -> PyResult<Py<PyAny>> {
    let category = parse_category(category)?;
    let opts = ApplyOptions {
        restore_point: parse_restore_point(restore_point)?,
        dry_run,
    };
    blocking(py, move || {
        Engine::open_default()?.apply_category(category, &opts)
    })
}

/// Reverts the journaled changes that belong to the given item ids.
#[pyfunction]
#[pyo3(signature = (ids, dry_run = false))]
fn revert(py: Python<'_>, ids: Vec<String>, dry_run: bool) -> PyResult<Py<PyAny>> {
    blocking(py, move || Engine::open_default()?.revert(&ids, dry_run))
}

/// Reverts every item of `category`, including removed bloatware packages.
#[pyfunction]
#[pyo3(signature = (category, dry_run = false))]
fn revert_category(py: Python<'_>, category: &str, dry_run: bool) -> PyResult<Py<PyAny>> {
    let category = parse_category(category)?;
    blocking(py, move || {
        Engine::open_default()?.revert_category(category, dry_run)
    })
}

/// Reverts every active journal record.
#[pyfunction]
#[pyo3(signature = (dry_run = false))]
fn revert_all(py: Python<'_>, dry_run: bool) -> PyResult<Py<PyAny>> {
    blocking(py, move || Engine::open_default()?.revert_all(dry_run))
}

/// Reverts the active journal records selected by `filter`, a dict (or JSON string) with
/// optional keys `registry` (list of {hive, key_path, value_name}), `services` (list of
/// names), `appx_families` (list of package family names), `power` (bool),
/// `scheduled_tasks` (list of task paths, compared ignoring case), `dns` (list of
/// adapter interface GUIDs, braces optional; selects both address families) and
/// `task_definitions` (list of paths of scheduled tasks Cairn registered, compared ignoring
/// case; they are deleted). App permission entries and Windows Update settings are
/// registry targets. The report's `restart` is the strongest restart need of the catalog
/// tweaks whose records were restored.
#[pyfunction]
#[pyo3(signature = (filter, dry_run = false))]
fn revert_targets(py: Python<'_>, filter: &Bound<'_, PyAny>, dry_run: bool) -> PyResult<Py<PyAny>> {
    let text = if let Ok(s) = filter.extract::<String>() {
        s
    } else {
        let json = py.import("json")?;
        json.call_method1("dumps", (filter,))?.extract::<String>()?
    };
    let filter: safety::RollbackFilter = serde_json::from_str(&text)
        .map_err(|e| PyValueError::new_err(format!("invalid revert filter: {e}")))?;
    blocking(py, move || {
        let journal = Journal::open_default()?;
        safety::rollback_filtered(&journal, &filter, dry_run)
    })
}

/// The engine's log: events that pass the `OPTIMIZER_LOG` filter (default `info`) go to
/// `writer`, with terminal colour codes only when `colours`. A windowed start points stderr at
/// logs\native.log, so colours are kept for a console only.
fn log_subscriber<W>(writer: W, colours: bool) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("OPTIMIZER_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(writer)
        .with_ansi(colours)
        .finish()
}

#[pymodule]
fn optimizer_engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    use std::io::IsTerminal;
    use tracing_subscriber::util::SubscriberInitExt;

    let _ = log_subscriber(std::io::stderr, std::io::stderr().is_terminal()).try_init();

    m.add("__version__", optimizer_core::VERSION)?;
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(is_elevated, m)?)?;
    m.add_function(wrap_pyfunction!(elevated_as_other_user, m)?)?;
    m.add_function(wrap_pyfunction!(journal_path, m)?)?;
    m.add_function(wrap_pyfunction!(system_restore_enabled, m)?)?;
    m.add_function(wrap_pyfunction!(enable_system_restore, m)?)?;
    m.add_function(wrap_pyfunction!(create_restore_point, m)?)?;
    m.add_function(wrap_pyfunction!(journal_summary, m)?)?;
    m.add_function(wrap_pyfunction!(journal_export_json, m)?)?;
    m.add_function(wrap_pyfunction!(rollback_to_baseline, m)?)?;
    m.add_function(wrap_pyfunction!(catalog, m)?)?;
    m.add_function(wrap_pyfunction!(categories, m)?)?;
    m.add_function(wrap_pyfunction!(scan, m)?)?;
    m.add_function(wrap_pyfunction!(apply, m)?)?;
    m.add_function(wrap_pyfunction!(apply_category, m)?)?;
    m.add_function(wrap_pyfunction!(revert, m)?)?;
    m.add_function(wrap_pyfunction!(revert_category, m)?)?;
    m.add_function(wrap_pyfunction!(revert_all, m)?)?;
    m.add_function(wrap_pyfunction!(revert_targets, m)?)?;
    cleanup::register(m)?;
    startup::register(m)?;
    network::register(m)?;
    sysinfo::register(m)?;
    tools::register(m)?;
    app::register(m)?;
    permissions::register(m)?;
    health::register(m)?;
    storage::register(m)?;
    updates::register(m)?;
    maintenance::register(m)?;
    profiles::register(m)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::log_subscriber;

    /// Collects what a subscriber writes.
    #[derive(Debug, Clone, Default)]
    struct Collected(Arc<Mutex<Vec<u8>>>);

    impl Write for Collected {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn logged(colours: bool) -> String {
        let collected = Collected::default();
        let sink = collected.clone();
        let subscriber = log_subscriber(move || sink.clone(), colours);
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "optimizer_core::safety", "journal refused");
        });
        let bytes = collected.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn log_lines_carry_colour_codes_only_for_a_console() {
        let plain = logged(false);
        assert!(
            plain.contains("ERROR") && plain.contains("journal refused"),
            "{plain:?}"
        );
        assert!(!plain.contains('\u{1b}'), "{plain:?}");
        assert!(
            logged(true).contains('\u{1b}'),
            "a console keeps its colours"
        );
    }
}
