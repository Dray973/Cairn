//! Python surface of `optimizer_core::permissions`.

use pyo3::prelude::*;

use optimizer_core::permissions;

use crate::{blocking, err};

/// Camera, microphone and location permissions, as a guide: Windows 11 manages them itself, in
/// Settings › Privacy & security, and on this version of Windows an app like Cairn cannot
/// change them, so there is no permission state here. Read-only: reads Windows' own usage
/// records and opens no journal.
///
/// Returns `{"capabilities": [{"capability", "label", "settings_uri", "recent_desktop_apps"}],
/// "warnings"}`, with camera, microphone and location in that order. `settings_uri` is the
/// capability's Settings page (`ms-settings:privacy-webcam`, `ms-settings:privacy-microphone`
/// or `ms-settings:privacy-location`). `recent_desktop_apps` holds the desktop programs Windows
/// recorded using the device, newest first and at most 50, each with `path`, `last_used` (RFC
/// 3339 UTC, or None) and `in_use`. A usage record that cannot be read adds a warning.
#[pyfunction]
fn permissions_list(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, || Ok(permissions::list()))
}

/// Refused for every request: no app permission is changed here (see `permissions_list`).
/// Raises RuntimeError with one message whatever `id`, `allow`, `restore_point` and `dry_run`
/// are, dry runs included. Nothing is read or written, no journal is opened and no session
/// is started.
#[pyfunction]
#[pyo3(signature = (id, allow, restore_point = "skip", dry_run = false))]
fn permissions_set(id: &str, allow: bool, restore_point: &str, dry_run: bool) -> PyResult<()> {
    // The request is refused whatever it names.
    let _ = (id, allow, restore_point, dry_run);
    Err(err(permissions::change_refused()))
}

/// Registers `permissions_list` and `permissions_set` on the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(permissions_list, m)?)?;
    m.add_function(wrap_pyfunction!(permissions_set, m)?)?;
    Ok(())
}
