//! Python surface of `optimizer_core::app`.

use pyo3::prelude::*;

use optimizer_core::app;

use crate::blocking;

/// The installed copy of Cairn as its installer registered it, or None when Cairn is not
/// installed on this PC (or its uninstall entry names no local folder that holds Cairn.exe
/// and optctl.exe). Returns `{"dir", "version", "launcher", "cli"}` (paths as strings,
/// `version` "" when the entry has none). Read-only; needs no elevation; runs without the
/// GIL. Raises RuntimeError when the uninstall entry cannot be read.
#[pyfunction]
fn install_info(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, app::installed)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(install_info, m)?)?;
    Ok(())
}
