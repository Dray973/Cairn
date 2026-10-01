//! Python surface of `optimizer_core::profiles`.

use std::path::PathBuf;
use std::sync::Arc;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use optimizer_core::profiles::{self, Profile, MAX_KEYS, MAX_KEY_CHARS};
use optimizer_core::safety::state_log::Journal;

use crate::{blocking, parse_restore_point, to_py};

/// Parses and validates profile text; an invalid profile raises ValueError with the reason.
fn checked(text: &str) -> PyResult<Profile> {
    profiles::parse(text).map_err(|e| PyValueError::new_err(e.to_string()))
}

/// At most [`MAX_KEYS`] row keys of at most [`MAX_KEY_CHARS`] characters each.
fn check_keys(keys: Option<&[String]>) -> PyResult<()> {
    let Some(keys) = keys else {
        return Ok(());
    };
    if keys.len() > MAX_KEYS {
        return Err(PyValueError::new_err(format!(
            "at most {MAX_KEYS} keys, got {}",
            keys.len()
        )));
    }
    if let Some(key) = keys.iter().find(|k| k.chars().count() > MAX_KEY_CHARS) {
        return Err(PyValueError::new_err(format!(
            "a key is longer than {MAX_KEY_CHARS} characters: {:?}",
            key.chars().take(40).collect::<String>()
        )));
    }
    Ok(())
}

/// Starter profiles: `[{"id", "name", "description", "counts", "text"}]`, in the order gaming,
/// privacy, clean. `counts` is `{"tweaks", "apps", "startup", "dns", "windows_update",
/// "maintenance"}`; `text` is the profile as a file holds it. Pure data.
#[pyfunction]
fn profile_starters(py: Python<'_>) -> PyResult<Py<PyAny>> {
    to_py(py, &profiles::starters())
}

/// Validates profile text. Returns `{"name", "description", "created", "created_with",
/// "counts", "text"}` with the canonical text. An invalid profile raises ValueError with the
/// reason, which is written to be shown to the user.
#[pyfunction]
fn profile_check(py: Python<'_>, text: &str) -> PyResult<Py<PyAny>> {
    let profile = checked(text)?;
    to_py(py, &profiles::summary(&profile))
}

/// Reads and validates a profile file at the absolute `path` (else ValueError). Returns the
/// dict of `profile_check`. Content that is not a valid profile raises ValueError; a file that
/// cannot be read raises RuntimeError.
#[pyfunction]
fn profile_read(py: Python<'_>, path: &str) -> PyResult<Py<PyAny>> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(PyValueError::new_err(format!(
            "the profile path must be absolute, got {:?}",
            path.display().to_string()
        )));
    }
    let loaded = py
        .allow_threads(|| profiles::load(&path).map_err(|e| e.to_string()))
        .map_err(PyRuntimeError::new_err)?;
    match loaded {
        Ok(summary) => to_py(py, &summary),
        Err(invalid) => Err(PyValueError::new_err(invalid.to_string())),
    }
}

/// This PC's exportable settings. Read-only. Returns `{"rows": [{"key", "section", "title",
/// "detail", "selected", "caution", "per_user"}], "other_account", "warnings"}`.
#[pyfunction]
fn profile_candidates(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, || {
        profiles::candidates(Arc::new(Journal::open_default()?))
    })
}

/// Writes a profile of the candidate rows `keys` selects (None: the default selection) to
/// the absolute `.json` path. The file holds no computer, user or adapter name, package
/// version or server address. Returns `{"path", "name", "counts", "missing"}`, where `missing`
/// lists chosen keys that were no longer settings of this PC. ValueError: a bad name (empty
/// after trimming, longer than 60 characters, control characters), a description longer than
/// 300 characters, a bad path, more than 2000 keys or a key longer than 300 characters.
/// RuntimeError: nothing selected, or the file could not be written.
#[pyfunction]
#[pyo3(signature = (path, name, description = "", keys = None))]
fn profile_export(
    py: Python<'_>,
    path: &str,
    name: &str,
    description: &str,
    keys: Option<Vec<String>>,
) -> PyResult<Py<PyAny>> {
    profiles::check_export_fields(name, description)
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
    let path = PathBuf::from(path);
    if !profiles::is_profile_path(&path) {
        return Err(PyValueError::new_err(profiles::PROFILE_PATH_TEXT));
    }
    check_keys(keys.as_deref())?;
    let name = name.to_string();
    let description = description.to_string();
    blocking(py, move || {
        profiles::export(
            Arc::new(Journal::open_default()?),
            &path,
            &name,
            &description,
            keys.as_deref(),
        )
    })
}

/// With `dry_run=True`: the plan `{"dry_run", "name", "rows": [{"key", "section", "title",
/// "status", "detail", "reason", "caution", "risk", "restart", "per_user", "selected"}],
/// "changes", "already", "skipped", "restart", "elevated", "other_account", "warnings",
/// "duration_ms"}`; `keys` and `restore_point` are not used and nothing is opened or written.
///
/// With `dry_run=False`: applies the rows `keys` selects (None: the change rows whose
/// `selected` is true) in one journal session labelled `profile: <name>` and returns
/// `{"dry_run", "name", "session_id", "restore_point", "results": [{"key", "section", "title",
/// "outcome", "details"}], "applied", "already", "skipped", "failed", "restart", "warnings",
/// "undo"}`, where `undo` is the `revert_targets` filter of what was changed.
///
/// ValueError: invalid text, a bad `restore_point`, more than 2000 keys or a key longer than
/// 300 characters. RuntimeError: not elevated, or a journal or engine failure.
#[pyfunction]
#[pyo3(signature = (text, keys = None, restore_point = "try", dry_run = false))]
fn profile_apply(
    py: Python<'_>,
    text: &str,
    keys: Option<Vec<String>>,
    restore_point: &str,
    dry_run: bool,
) -> PyResult<Py<PyAny>> {
    let profile = checked(text)?;
    let policy = parse_restore_point(restore_point)?;
    check_keys(keys.as_deref())?;
    blocking(py, move || {
        profiles::plan_or_apply(
            Arc::new(Journal::open_default()?),
            &profile,
            keys.as_deref(),
            policy,
            dry_run,
        )
    })
}

/// Registers the six `profile_*` functions on the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(profile_starters, m)?)?;
    m.add_function(wrap_pyfunction!(profile_check, m)?)?;
    m.add_function(wrap_pyfunction!(profile_read, m)?)?;
    m.add_function(wrap_pyfunction!(profile_candidates, m)?)?;
    m.add_function(wrap_pyfunction!(profile_export, m)?)?;
    m.add_function(wrap_pyfunction!(profile_apply, m)?)?;
    Ok(())
}
