//! Python surface of `optimizer_core::sysinfo`.

use pyo3::prelude::*;

use optimizer_core::sysinfo;

use crate::blocking;

/// Read-only snapshot of the hardware and Windows configuration; nothing is written or
/// journaled and no elevation is needed. Runs without the GIL and usually takes well under
/// a second. Drive queries share a 5-second deadline: a volume that does not answer by then
/// is reported as not responding, and a disk is listed without its size. Times are shown in
/// the local time zone, each with the offset it had at that time.
/// Returns `{"info", "summary", "sections", "text"}`:
///
/// - `info`: the raw data (`taken_at`, `duration_ms`, `os`, `cpu`, `memory`, `gpus`,
///   `displays`, `displays_unavailable`, `board`, `disks`, `volumes`, `security`,
///   `errors`); a section that could not be read is None or empty and has an entry
///   `{"section", "message"}` in `errors`.
/// - `summary`: one line such as "Windows 11 Home 25H2  ·  Intel Core Ultra 7 265F  ·
///   32 GB RAM  ·  NVIDIA GeForce RTX 5060 Ti  ·  1.02 TB NVMe SSD".
/// - `sections`: in the order windows, processor, memory, graphics, displays, board,
///   storage, security; each is `{"id", "title", "error", "note", "rows", "groups"}` with
///   rows `{"label", "value", "level" ("normal" | "good" | "warning"), "note", "fraction",
///   "private"}` and groups `{"title", "rows"}`. A failed section has `error` set and no
///   rows; `note` is a remark about the whole section (for example that display details
///   are not available in a remote or locked session).
/// - `text`: the plain-text report for the clipboard, without private rows (the computer
///   name).
#[pyfunction]
fn sysinfo_snapshot(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, sysinfo::snapshot)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(sysinfo_snapshot, m)?)?;
    Ok(())
}
