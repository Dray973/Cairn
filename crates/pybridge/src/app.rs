//! Python surface of `optimizer_core::app` and of opening web pages.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use optimizer_core::app;
use optimizer_core::win::shell;

use crate::{blocking, err};

/// The installed copy of Cairn as its installer registered it, or None when Cairn is not
/// installed on this PC (or its uninstall entry names no local folder that holds Cairn.exe
/// and optctl.exe). Returns `{"dir", "version", "launcher", "cli"}` (paths as strings,
/// `version` "" when the entry has none). Read-only; needs no elevation; runs without the
/// GIL. Raises RuntimeError when the uninstall entry cannot be read.
#[pyfunction]
fn install_info(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, app::installed)
}

/// Opens the web page `url` in the signed-in user's default browser through the desktop
/// shell, so the browser runs without this process's administrator rights. Accepts only an
/// `https://` address of a web site in printable ASCII without spaces, at most 2048
/// characters; anything else raises ValueError and opens nothing. Raises RuntimeError when
/// the desktop shell cannot be asked (for example when no Explorer runs in this session).
/// Runs without the GIL.
#[pyfunction]
fn open_url(py: Python<'_>, url: &str) -> PyResult<()> {
    shell::check_web_url(url).map_err(|e| PyValueError::new_err(e.to_string()))?;
    py.allow_threads(|| shell::open_url_as_desktop_user(url).map_err(|e| e.to_string()))
        .map_err(err)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(install_info, m)?)?;
    m.add_function(wrap_pyfunction!(open_url, m)?)?;
    Ok(())
}
