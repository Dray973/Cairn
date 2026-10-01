//! Python surface of `optimizer_core::network`.

use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use optimizer_core::network::{self, DnsRequest};
use optimizer_core::safety::{state_log::Journal, RestorePointPolicy, Safety, SafetyOptions};

use crate::{blocking, parse_restore_point, to_py};

/// The network adapters of this PC. Read-only. Returns `{"adapters", "dns_policy",
/// "vpn_connected", "warnings", "duration_ms"}`. Each adapter has `id` (canonical interface
/// GUID, `{lowercase}`), `name`, `description`, `kind` (ethernet, wifi, cellular, bluetooth,
/// vpn, virtual, tunnel or other), `status` (connected, disconnected, not_present or
/// unknown), `limited`, `hardware`, `minor`, `primary`, `if_index`, `mac`, `mtu`,
/// `receive_bps`, `transmit_bps`, `dhcp_enabled`, `ipv4_enabled`, `ipv6_enabled`, `ipv4` and
/// `ipv6` (lists of `{"address", "prefix_length", "origin", "preferred"}`), `gateways`,
/// `dns_servers`, `dns_ipv4` and `dns_ipv6` (`{"mode", "servers", "preset",
/// "profile_servers"}`, mode automatic, manual, profile or unknown; an adapter that is not
/// connected lists no automatic servers and empty `dns_servers`), `dns_suffix`,
/// `ipv4_metric`, `can_change_dns`, `can_renew`, `dns_revertible` and `note`. A journal
/// that cannot be read adds a warning.
#[pyfunction]
fn network_list(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, || {
        let journal = Journal::open_default();
        let mut report = network::list(journal.as_ref().ok())?;
        if let Err(e) = &journal {
            report
                .warnings
                .push(format!("cannot read the journal: {e}"));
        }
        Ok(report)
    })
}

/// The DNS presets in menu order: `[{"id", "title", "description", "ipv4", "ipv6"}]`. The
/// `automatic` preset has empty server lists.
#[pyfunction]
fn network_dns_presets(py: Python<'_>) -> PyResult<Py<PyAny>> {
    to_py(py, &network::PRESETS)
}

/// Changes the DNS servers of adapter `adapter_id` to `preset` (a preset id, or "custom"
/// with the servers in `ipv4` and `ipv6`; an empty or missing list leaves that family
/// unchanged). The current servers are recorded in the journal first, so the change
/// reverts with the other records. DNS settings are machine-wide, so the change is not
/// refused for another account. Refused while a VPN is connected and on adapters whose DNS
/// servers are managed elsewhere. `restore_point` is "skip", "try" or "require". With
/// `dry_run=True` nothing is recorded or written and no journal session is opened.
///
/// Returns `{"dry_run", "adapter_id", "adapter_name", "session_id" (None in a dry run),
/// "changes": [{"family", "previous", "target", "outcome", "detail"}], "warnings"}` with
/// outcome planned, applied, already_set, skipped or failed. Invalid arguments raise
/// `ValueError`; a refused adapter or an engine error raises `RuntimeError`.
#[pyfunction]
#[pyo3(signature = (adapter_id, preset, ipv4 = None, ipv6 = None, restore_point = "skip", dry_run = false))]
fn network_set_dns(
    py: Python<'_>,
    adapter_id: &str,
    preset: &str,
    ipv4: Option<Vec<String>>,
    ipv6: Option<Vec<String>>,
    restore_point: &str,
    dry_run: bool,
) -> PyResult<Py<PyAny>> {
    let policy = parse_restore_point(restore_point)?;
    let adapter_id = adapter_id.trim().to_string();
    if adapter_id.is_empty() {
        return Err(PyValueError::new_err("adapter_id must not be empty"));
    }
    let ipv4 = ipv4.unwrap_or_default();
    let ipv6 = ipv6.unwrap_or_default();
    let request = if preset.trim().eq_ignore_ascii_case("custom") {
        DnsRequest::custom(&ipv4, &ipv6)
    } else if !ipv4.is_empty() || !ipv6.is_empty() {
        return Err(PyValueError::new_err(
            "ipv4 and ipv6 are only used with preset \"custom\"",
        ));
    } else {
        DnsRequest::preset(preset)
    }
    .map_err(|e| PyValueError::new_err(e.to_string()))?;
    blocking(py, move || {
        // A dry run never opens the journal session.
        let begin = || {
            Safety::begin(
                Arc::new(Journal::open_default()?),
                SafetyOptions {
                    label: format!("dns: {adapter_id}"),
                    restore_point: policy,
                    require_elevation: true,
                    ..Default::default()
                },
            )
        };
        network::plan_or_set_dns(dry_run, begin, &adapter_id, &request)
    })
}

/// Clears the DNS resolver cache, so names are looked up again. Allowed for standard
/// users; logged, not journaled. Returns `{"session_id"}`.
#[pyfunction]
fn network_flush_dns(py: Python<'_>) -> PyResult<Py<PyAny>> {
    blocking(py, || {
        let journal = Arc::new(Journal::open_default()?);
        let safety = Safety::begin(
            journal,
            SafetyOptions {
                label: "network: flush DNS cache".into(),
                restore_point: RestorePointPolicy::Skip,
                require_elevation: false,
                ..Default::default()
            },
        )?;
        network::flush_dns_cache(&safety)
    })
}

/// Renews the IPv4 DHCP lease of adapter `adapter_id` (its id, name or interface index),
/// releasing it first when `release_first` is set. Needs an elevated process; logged, not
/// journaled. Returns `{"adapter_id", "adapter_name", "session_id", "released", "renewed",
/// "ipv4", "duration_ms"}`.
#[pyfunction]
#[pyo3(signature = (adapter_id, release_first = false))]
fn network_renew_dhcp(
    py: Python<'_>,
    adapter_id: &str,
    release_first: bool,
) -> PyResult<Py<PyAny>> {
    let adapter_id = adapter_id.trim().to_string();
    if adapter_id.is_empty() {
        return Err(PyValueError::new_err("adapter_id must not be empty"));
    }
    blocking(py, move || {
        let journal = Arc::new(Journal::open_default()?);
        let safety = Safety::begin(
            journal,
            SafetyOptions {
                label: "network: renew lease".into(),
                restore_point: RestorePointPolicy::Skip,
                require_elevation: true,
                ..Default::default()
            },
        )?;
        network::renew_lease(&safety, &adapter_id, release_first)
    })
}

/// Resets the Winsock catalog and TCP/IP for IPv4 and IPv6 with `netsh`. Irreversible:
/// manual IP addresses and DNS servers are removed and Windows must restart. The manual
/// settings (of listed adapters and of unplugged or disabled ones) are written to the audit
/// log before anything runs. `restore_point` is "skip", "try" or "require". With
/// `dry_run=True` nothing runs, no journal session is opened and every step is planned.
///
/// Returns `{"dry_run", "session_id", "restore_point" ({"sequence", "description",
/// "created_at"} or None), "steps": [{"id" (winsock, ipv4 or ipv6), "title", "command",
/// "status", "exit_code", "output"}], "manual_settings": [{"adapter", "detail"}],
/// "restart_required", "warnings"}` with status planned, succeeded, completed_with_errors,
/// failed, timed_out or not_run.
#[pyfunction]
#[pyo3(signature = (restore_point = "try", dry_run = false))]
fn network_reset(py: Python<'_>, restore_point: &str, dry_run: bool) -> PyResult<Py<PyAny>> {
    let policy = parse_restore_point(restore_point)?;
    blocking(py, move || {
        // A dry run never opens the journal session or creates the restore point.
        let begin = || {
            Safety::begin(
                Arc::new(Journal::open_default()?),
                SafetyOptions {
                    label: "network: reset".into(),
                    restore_point: policy,
                    require_elevation: true,
                    ..Default::default()
                },
            )
        };
        network::plan_or_reset_stack(dry_run, begin)
    })
}

/// Registers the `network_*` functions on the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(network_list, m)?)?;
    m.add_function(wrap_pyfunction!(network_dns_presets, m)?)?;
    m.add_function(wrap_pyfunction!(network_set_dns, m)?)?;
    m.add_function(wrap_pyfunction!(network_flush_dns, m)?)?;
    m.add_function(wrap_pyfunction!(network_renew_dhcp, m)?)?;
    m.add_function(wrap_pyfunction!(network_reset, m)?)?;
    Ok(())
}
