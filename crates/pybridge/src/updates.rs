//! Python surface of `optimizer_core::updates`.
//!
//! winget work runs as jobs of the engine's `winget` lane: `updates_start` returns once the
//! job's thread runs, and `updates_job`, `updates_jobs`, `updates_result` and
//! `updates_cancel` only read or flag in-memory state, so they keep the GIL and return at
//! once. Windows Update settings are journaled registry values; app updates and installs
//! are irreversible and only recorded in the audit log.

use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBool;

use optimizer_core::jobs::HostJobId;
use optimizer_core::safety::{state_log::Journal, Safety, SafetyOptions};
use optimizer_core::tools::MAX_LINES_PER_VIEW;
use optimizer_core::updates::apps::{self, AppEntry};
use optimizer_core::updates::winget::{
    self, lane, UpdateItem, UpdatesKind, UpdatesRequest, MAX_BATCH_ITEMS,
};
use optimizer_core::updates::wu::{self, WuChange, WuSettingId};

use crate::{blocking, err, json_to_py, parse_restore_point, to_py};

/// A list of dicts (or any JSON-serializable value) as JSON text.
fn json_text(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<String> {
    py.import("json")?
        .call_method1("dumps", (value,))?
        .extract::<String>()
}

fn parse_kind(kind: &str) -> PyResult<UpdatesKind> {
    UpdatesKind::parse(kind).ok_or_else(|| {
        PyValueError::new_err(format!(
            "unknown kind {kind:?}; expected one of: scan, upgrade, install"
        ))
    })
}

fn parse_setting(setting: &str) -> PyResult<WuSettingId> {
    WuSettingId::parse(setting).ok_or_else(|| {
        let valid: Vec<&str> = WuSettingId::ALL.iter().map(|id| id.as_str()).collect();
        PyValueError::new_err(format!(
            "unknown Windows Update setting {setting:?}; expected one of: {}",
            valid.join(", ")
        ))
    })
}

/// A whole number that is not a bool.
fn whole_number(value: &Bound<'_, PyAny>, what: &str) -> PyResult<u32> {
    if value.is_instance_of::<PyBool>() {
        return Err(PyValueError::new_err(format!("{what} must be a number")));
    }
    value
        .extract::<u32>()
        .map_err(|_| PyValueError::new_err(format!("{what} must be a whole number")))
}

/// The change `updates_wu_set` makes for `setting` and `value`.
fn wu_change(id: WuSettingId, value: Option<&Bound<'_, PyAny>>) -> PyResult<WuChange> {
    let change = match (id, value) {
        (WuSettingId::Pause, None) => WuChange::Resume,
        (WuSettingId::Pause, Some(v)) => WuChange::Pause {
            days: whole_number(v, "pause days")?,
        },
        (WuSettingId::ActiveHours, None) => WuChange::AutomaticActiveHours,
        (WuSettingId::ActiveHours, Some(v)) => {
            let hours: Vec<Bound<'_, PyAny>> = v
                .extract()
                .map_err(|_| PyValueError::new_err("active_hours takes [start, end] or None"))?;
            let [start, end] = hours.as_slice() else {
                return Err(PyValueError::new_err(
                    "active_hours takes [start, end] or None",
                ));
            };
            WuChange::ActiveHours {
                start: whole_number(start, "the start hour")?,
                end: whole_number(end, "the end hour")?,
            }
        }
        (WuSettingId::DeferFeature, None) => WuChange::DeferFeature(None),
        (WuSettingId::DeferFeature, Some(v)) => match whole_number(v, "defer_feature days")? {
            0 => WuChange::DeferFeature(None),
            days => WuChange::DeferFeature(Some(days)),
        },
        (WuSettingId::ExcludeDrivers | WuSettingId::RestartNotify, None) => {
            return Err(PyValueError::new_err(format!(
                "{} takes True or False",
                id.as_str()
            )));
        }
        (WuSettingId::ExcludeDrivers | WuSettingId::RestartNotify, Some(v)) => {
            let on = v
                .downcast::<PyBool>()
                .map_err(|_| PyValueError::new_err(format!("{} takes True or False", id.as_str())))?
                .is_true();
            if id == WuSettingId::ExcludeDrivers {
                WuChange::ExcludeDrivers(on)
            } else {
                WuChange::RestartNotify(on)
            }
        }
    };
    change
        .validate()
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(change)
}

/// winget as Cairn sees it, without starting it or using the network: `{"availability"
/// ("ready" | "missing" | "other_user" | "user_unknown"), "message" (why it can't be used,
/// None when ready), "location" ({"path", "package_full_name", "package_version"} or None),
/// "elevated", "min_version", "store_uri"}`.
#[pyfunction]
fn updates_winget_status(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, || Ok(winget::winget_status()))
}

/// Plans and, unless `dry_run`, starts a winget job: `kind` "scan" (check for app updates;
/// read-only, no admin), "upgrade" or "install" (the apps in `items`, one after another;
/// need an elevated Cairn run by the signed-in user). `items` is a list of dicts
/// `{"id", "source" ("winget"), "name" (""), "from" (None), "to" (None)}`, 1 to 200 of
/// them, and must be left out for a scan. The journal is opened only to start an upgrade or
/// install: each app gets an audit row "started" before its winget starts and one final
/// row; nothing is journaled for undo.
///
/// Returns `{"plan", "job"}`; `job` is the job snapshot, None in a dry run. The plan is
/// `{"kind", "title", "items", "program", "command_lines", "requires_admin",
/// "irreversible", "cancellable", "blocked_reason" (None when it can start), "notes"}`.
/// Invalid arguments raise ValueError; a start that is not elevated (upgrade, install), a
/// blocked plan and a start after `updates_shutdown` raise RuntimeError.
#[pyfunction]
#[pyo3(signature = (kind, items = None, dry_run = false))]
fn updates_start(
    py: Python<'_>,
    kind: &str,
    items: Option<&Bound<'_, PyAny>>,
    dry_run: bool,
) -> PyResult<Py<PyAny>> {
    let kind = parse_kind(kind)?;
    let items: Vec<UpdateItem> = match items.filter(|i| !i.is_none()) {
        None => Vec::new(),
        Some(_) if kind == UpdatesKind::Scan => {
            return Err(PyValueError::new_err("a scan takes no items"));
        }
        Some(value) => serde_json::from_str(&json_text(py, value)?)
            .map_err(|e| PyValueError::new_err(format!("invalid items: {e}")))?,
    };
    if items.len() > MAX_BATCH_ITEMS {
        return Err(PyValueError::new_err(format!(
            "at most {MAX_BATCH_ITEMS} apps can be updated or installed at once"
        )));
    }
    let request =
        UpdatesRequest::new(kind, items).map_err(|e| PyValueError::new_err(e.to_string()))?;
    blocking(py, move || {
        let open = || Ok(Arc::new(Journal::open_default()?));
        winget::plan_or_start(&request, dry_run, open)
    })
}

/// A job's snapshot with up to 500 output lines numbered after `after`, or None for an
/// unknown id: the 25 snapshot keys (`"id"`, `"lane"`, `"kind"` ("winget_scan",
/// "winget_upgrade" or "winget_install"), `"title"`, `"command_line"`, `"state"`,
/// `"started_at"`, `"finished_at"`, `"elapsed_ms"`, `"idle_ms"`, `"progress"`,
/// `"progress_line"`, `"cancellable"`, `"cancel_requested"`, `"detached"`,
/// `"restart_required"`, `"summary"`, `"hint"`, `"notes"`, `"detail"`, `"log_path"`,
/// `"line_count"`, `"logged"`, `"has_result"`, `"result_revision"`) plus `"lines"`,
/// `"first"`, `"next"` (pass it as `after` next time), `"skipped"` and `"more"`. While an app
/// of a batch runs, `"detail"` is `{"item": index, "progress": "12.0 MB / 32.5 MB" | "45%" |
/// None}`, what winget's display shows for it; otherwise None. Keeps the GIL: it only reads
/// in-memory state.
#[pyfunction]
#[pyo3(signature = (job_id, after = 0))]
fn updates_job(py: Python<'_>, job_id: u64, after: u64) -> PyResult<Py<PyAny>> {
    to_py(
        py,
        &lane().view(HostJobId(job_id), after, MAX_LINES_PER_VIEW),
    )
}

/// The running job and the retained finished ones as snapshots, newest first. Keeps the
/// GIL.
#[pyfunction]
fn updates_jobs(py: Python<'_>) -> PyResult<Py<PyAny>> {
    to_py(py, &lane().jobs())
}

/// The job's published result with its `"revision"`, only when the revision is newer than
/// `since`; otherwise None. A check publishes `{"kind": "scan", "winget_version",
/// "checked_at", "upgrades": [{"id", "name", "installed", "available", "source",
/// "explicit_only", "selectable", "note"}], "installed": [ids], "inventory_complete",
/// "unparsed_rows", "warnings", "error" ({"code", "message", "availability", "outdated"} or
/// None)}`; an update or install batch `{"kind", "items": [{"id", "name", "source", "from",
/// "to", "state", "exit_code", "exit_code_hex", "message", "elapsed_ms", "detached",
/// "retry"}], "current", "done", "total", "stopping", "restart_required"}`, where an item's
/// `"retry"` is False when winget's answer can't change on a retry (an app installed in a way
/// winget can't update, a policy, no installer for this PC). Keeps the GIL.
#[pyfunction]
#[pyo3(signature = (job_id, since = 0))]
fn updates_result(py: Python<'_>, job_id: u64, since: u64) -> PyResult<Py<PyAny>> {
    match lane().result(HostJobId(job_id), since) {
        None => Ok(py.None()),
        Some((revision, value)) => {
            let mut value = (*value).clone();
            if let serde_json::Value::Object(map) = &mut value {
                map.insert("revision".into(), revision.into());
            }
            json_to_py(py, &value)
        }
    }
}

/// Stops a check at once, or an update or install batch after the app that runs now (an
/// app's installer is never interrupted). False when the job already finished; RuntimeError
/// for an unknown id. Keeps the GIL: it only sets a flag.
#[pyfunction]
fn updates_cancel(job_id: u64) -> PyResult<bool> {
    lane().cancel(HostJobId(job_id)).map_err(err)
}

/// Called when Cairn closes: refuses new starts, stops a running check and asks a batch to
/// stop, waiting up to about 3 s. An app whose installer still runs keeps running and gets
/// a "left_running" audit row. Returns `[{"id", "kind", "action" ("stopped" |
/// "stop_timed_out" | "left_running")}]`. Releases the GIL while it waits.
#[pyfunction]
fn updates_shutdown(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let outcomes = py.allow_threads(|| lane().shutdown());
    to_py(py, &outcomes)
}

/// Opens the job's transcript in Notepad. RuntimeError for an unknown id.
#[pyfunction]
fn updates_open_log(py: Python<'_>, job_id: u64) -> PyResult<()> {
    py.allow_threads(|| {
        lane()
            .open_log(HostJobId(job_id))
            .map_err(|e| e.to_string())
    })
    .map_err(err)
}

/// The Windows Update settings of this PC. Registry reads only; the journal is opened only
/// to tell which settings Cairn changed. Returns `{"edition": {"id", "name", "home",
/// "version", "build"}, "service" ("automatic" | "manual" | "disabled" | "missing" |
/// "unknown"), "restart_pending", "managed": [notes], "settings": [{"id", "title",
/// "available", "unavailable_reason", "caveat", "value": {"kind": "pause" | "active_hours" |
/// "switch" | "defer", ...}, "by_cairn", "differs", "targets": [{"hive", "key_path",
/// "value_name"}]}], "warnings"}`.
#[pyfunction]
fn updates_wu_state(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, || {
        let journal = Journal::open_default();
        if let Err(e) = &journal {
            tracing::warn!(error = %e, "cannot open the journal for the Windows Update state");
        }
        wu::wu_state(journal.as_ref().ok())
    })
}

/// The settings as History titles them: `[{"id", "title", "targets": [target strings]}]`.
/// Static; no I/O. Keeps the GIL.
#[pyfunction]
fn updates_wu_catalog(py: Python<'_>) -> PyResult<Py<PyAny>> {
    to_py(py, &wu::wu_catalog())
}

/// Changes one Windows Update setting, journaled so it can be undone. `value` per
/// `setting`: "pause" days 1 to 35, or None to resume (a running pause is extended by that
/// many days instead, to at most 35 days after it began); "active_hours" `[start, end]`
/// hours, or None for automatic; "exclude_drivers" and "restart_notify" True or False;
/// "defer_feature" days 1 to 365, or None or 0 for no delay. Needs an elevated process;
/// these settings are machine-wide, so another account is not refused. `restore_point` is
/// "skip", "try" or "require". With `dry_run=True` nothing is recorded or written and no
/// journal session is opened.
///
/// Returns `{"dry_run", "setting", "session_id" (None in a dry run), "writes": [{"target",
/// "before", "after", "outcome"}], "warnings"}`. Invalid values raise ValueError; a delay on
/// Home, a policy that turns the setting off, a pause already at its 35 days and an engine
/// error raise RuntimeError.
#[pyfunction]
#[pyo3(signature = (setting, value = None, restore_point = "skip", dry_run = false))]
fn updates_wu_set(
    py: Python<'_>,
    setting: &str,
    value: Option<&Bound<'_, PyAny>>,
    restore_point: &str,
    dry_run: bool,
) -> PyResult<Py<PyAny>> {
    let id = parse_setting(setting)?;
    let change = wu_change(id, value.filter(|v| !v.is_none()))?;
    let policy = parse_restore_point(restore_point)?;
    blocking(py, move || {
        // A dry run never opens the journal session.
        let begin = || {
            Safety::begin(
                Arc::new(Journal::open_default()?),
                SafetyOptions {
                    label: change.label(),
                    restore_point: policy,
                    require_elevation: true,
                    ..Default::default()
                },
            )
        };
        wu::plan_or_apply_wu(&change, dry_run, begin)
    })
}

/// The apps the Install apps view offers: `{"apps": [{"id", "name", "category"
/// ("browsers" | "chat" | "gaming" | "media" | "productivity" | "utilities" |
/// "developer"), "source"}], "custom" (the account's own list), "path", "warnings"}`.
/// Reading changes nothing.
#[pyfunction]
fn updates_app_list(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, || Ok(apps::app_list()))
}

/// Saves `apps` (a list of `{"id", "name", "category", "source" ("winget")}` dicts) as this
/// account's list, or, with None, restores the built-in list. Returns the list as
/// `updates_app_list` does. Invalid entries raise ValueError before anything is written.
#[pyfunction]
#[pyo3(signature = (apps = None))]
fn updates_save_app_list(py: Python<'_>, apps: Option<&Bound<'_, PyAny>>) -> PyResult<Py<PyAny>> {
    let entries: Option<Vec<AppEntry>> = match apps.filter(|a| !a.is_none()) {
        None => None,
        Some(value) => {
            let entries: Vec<AppEntry> = serde_json::from_str(&json_text(py, value)?)
                .map_err(|e| PyValueError::new_err(format!("invalid app list: {e}")))?;
            Some(apps::validate_apps(&entries).map_err(|e| PyValueError::new_err(e.to_string()))?)
        }
    };
    blocking(py, move || apps::save_app_list(entries.as_deref()))
}

/// Registers the `updates_*` functions on the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(updates_winget_status, m)?)?;
    m.add_function(wrap_pyfunction!(updates_start, m)?)?;
    m.add_function(wrap_pyfunction!(updates_job, m)?)?;
    m.add_function(wrap_pyfunction!(updates_jobs, m)?)?;
    m.add_function(wrap_pyfunction!(updates_result, m)?)?;
    m.add_function(wrap_pyfunction!(updates_cancel, m)?)?;
    m.add_function(wrap_pyfunction!(updates_shutdown, m)?)?;
    m.add_function(wrap_pyfunction!(updates_open_log, m)?)?;
    m.add_function(wrap_pyfunction!(updates_wu_state, m)?)?;
    m.add_function(wrap_pyfunction!(updates_wu_catalog, m)?)?;
    m.add_function(wrap_pyfunction!(updates_wu_set, m)?)?;
    m.add_function(wrap_pyfunction!(updates_app_list, m)?)?;
    m.add_function(wrap_pyfunction!(updates_save_app_list, m)?)?;
    Ok(())
}
