//! Python surface of `optimizer_core::startup` and `optimizer_core::win::shell`.

use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use serde::Serialize;

use optimizer_core::safety::{state_log::Journal, MutationOutcome, Safety, SafetyOptions};
use optimizer_core::startup::{self, StartupSource};
use optimizer_core::win::shell;

use crate::{blocking, err, parse_restore_point};

#[derive(Serialize)]
struct SetEnabledResult {
    id: String,
    outcome: MutationOutcome,
    enabled: bool,
}

/// Startup entries sorted by name: the Run keys (HKCU, HKLM, HKLM 32-bit), the Startup
/// folders, the startup tasks of packaged apps installed for the current user, and the
/// Group Policy Run keys (listed with `can_toggle` false). Each entry carries `can_toggle`
/// and `note` (why it cannot be toggled); per-user entries are not toggleable while this
/// process runs as a different account than the signed-in user or that cannot be
/// confirmed. Read-only.
#[pyfunction]
fn startup_list(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, startup::list)
}

/// Enables or disables one startup entry the way Task Manager does. The change is
/// journaled and reverts with the other journal records. `restore_point` is "skip", "try"
/// or "require". Returns `{"id", "outcome", "enabled"}`. A malformed id or restore point
/// policy raises `ValueError`; an id that is not listed, a Group Policy entry, and a
/// per-user entry while this process runs as a different account than the signed-in user
/// (or while that cannot be confirmed) raise `RuntimeError`.
#[pyfunction]
#[pyo3(signature = (id, enabled, restore_point = "skip"))]
fn startup_set_enabled(
    py: Python<'_>,
    id: &str,
    enabled: bool,
    restore_point: &str,
) -> PyResult<Py<PyAny>> {
    let policy = parse_restore_point(restore_point)?;
    let source = StartupSource::of_id(id).ok_or_else(|| {
        PyValueError::new_err(format!(
            "malformed startup entry id {id:?}; expected '<source>:<name>'"
        ))
    })?;
    let id = id.to_string();
    // Only machine-wide entries need an elevated session; per-user entries live in HKCU.
    let require_elevation = source.requires_admin();
    blocking(py, move || {
        // Refused before the journal is opened: it would be the other account's journal.
        if source.is_per_user() {
            startup::ensure_per_user_changes_allowed()?;
        }
        let journal = Arc::new(Journal::open_default()?);
        let safety = Safety::begin(
            journal,
            SafetyOptions {
                label: format!("startup: {id}"),
                restore_point: policy,
                require_elevation,
                ..Default::default()
            },
        )?;
        let outcome = startup::set_enabled(&safety, &id, enabled)?;
        Ok(SetEnabledResult {
            id,
            outcome,
            enabled,
        })
    })
}

/// Restarts File Explorer in the current session.
#[pyfunction]
fn restart_explorer(py: Python<'_>) -> PyResult<()> {
    py.allow_threads(|| shell::restart_explorer().map_err(|e| e.to_string()))
        .map_err(err)
}

/// Registers `startup_list`, `startup_set_enabled` and `restart_explorer` on the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(startup_list, m)?)?;
    m.add_function(wrap_pyfunction!(startup_set_enabled, m)?)?;
    m.add_function(wrap_pyfunction!(restart_explorer, m)?)?;
    Ok(())
}
