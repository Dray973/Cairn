//! Irreversible network maintenance: flushing the DNS resolver cache, renewing a DHCP lease
//! and resetting the Winsock catalog and TCP/IP stack. None of it is journaled for
//! rollback; every action is written to the audit log, and the reset records the manual
//! settings it will remove before anything runs.

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::adapters::{
    list_with, names_vpn, Adapter, AdapterKind, AddressOrigin, DnsMode, LinkStatus,
};
use super::runner::{CommandOutput, CommandRunner, OnTimeout};
use super::{
    canonical_guid, find_adapter, split_servers, IpFamily, NetStack, StoredInterface, FLUSH_TARGET,
    OP_FLUSH,
};
use crate::safety::{RestorePoint, Safety};
use crate::{Error, Result};

/// Audit log operation of the network stack reset.
pub(crate) const OP_RESET: &str = "network_reset";
/// Audit log operations of DHCP lease changes.
pub(crate) const OP_RELEASE: &str = "release_dhcp_lease";
pub(crate) const OP_RENEW: &str = "renew_dhcp_lease";

/// The reset steps in order: id, title and the arguments of `netsh.exe`.
pub(crate) const RESET_STEPS: [(&str, &str, &[&str]); 3] = [
    (
        "winsock",
        "Reset the Winsock catalog",
        &["winsock", "reset"],
    ),
    ("ipv4", "Reset TCP/IP for IPv4", &["int", "ip", "reset"]),
    ("ipv6", "Reset TCP/IP for IPv6", &["int", "ipv6", "reset"]),
];
/// How long one reset step may run before the rest are skipped.
pub(crate) const STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// Characters of a step's output kept in the report.
const REPORT_OUTPUT_CHARS: usize = 4000;
/// Characters of a step's output kept in its audit log row.
const LOG_OUTPUT_CHARS: usize = 1000;
/// Warning of a reset whose step did not finish in time.
pub(crate) const TIMEOUT_WARNING: &str = "A reset step did not finish within a minute; it may \
     still be running. Wait a few minutes, then restart Windows.";
/// Added to the name of an adapter Windows does not list now but whose settings it keeps.
pub(crate) const UNLISTED_SUFFIX: &str = " (unplugged or disabled)";
/// Warning when the settings kept for adapters that are not listed cannot be read.
pub(crate) const UNLISTED_WARNING: &str =
    "cannot read the settings of unplugged or disabled adapters";

/// Result of flushing the DNS resolver cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlushReport {
    pub session_id: i64,
}

/// Result of renewing an adapter's DHCP lease.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewReport {
    pub adapter_id: String,
    pub adapter_name: String,
    pub session_id: i64,
    /// The lease was released first.
    pub released: bool,
    pub renewed: bool,
    /// IPv4 addresses of the adapter after the renewal.
    pub ipv4: Vec<String>,
    pub duration_ms: u64,
}

/// State of one reset step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Dry run: the step would run.
    Planned,
    Succeeded,
    /// netsh reset most settings but reported some it could not reset.
    CompletedWithErrors,
    Failed,
    /// Still running at the deadline; it was left to finish on its own.
    TimedOut,
    /// Skipped because an earlier step timed out.
    NotRun,
}

impl StepStatus {
    fn as_str(self) -> &'static str {
        match self {
            StepStatus::Planned => "planned",
            StepStatus::Succeeded => "succeeded",
            StepStatus::CompletedWithErrors => "completed_with_errors",
            StepStatus::Failed => "failed",
            StepStatus::TimedOut => "timed_out",
            StepStatus::NotRun => "not_run",
        }
    }
}

/// One `netsh` command of the reset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetStep {
    /// `winsock`, `ipv4` or `ipv6`.
    pub id: String,
    pub title: String,
    /// The command line, for example `netsh winsock reset`.
    pub command: String,
    pub status: StepStatus,
    pub exit_code: Option<i32>,
    /// The command's output, trimmed to its last 4000 characters.
    pub output: String,
}

/// A manually set value the reset removes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualSetting {
    /// Adapter name.
    pub adapter: String,
    /// For example `static IPv4 192.168.0.50/24, gateway 192.168.0.1`.
    pub detail: String,
}

/// Result of planning or running the network stack reset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetReport {
    pub dry_run: bool,
    /// Journal session of the reset; `None` in a dry run.
    pub session_id: Option<i64>,
    /// The restore point created before the reset, if any.
    pub restore_point: Option<RestorePoint>,
    pub steps: Vec<ResetStep>,
    pub manual_settings: Vec<ManualSetting>,
    /// Windows must restart to finish the reset.
    pub restart_required: bool,
    pub warnings: Vec<String>,
}

/// Clears the resolver cache and logs the outcome. An error is returned after it is logged.
/// Needs no elevation.
pub(crate) fn flush_with(safety: &Safety, stack: &dyn NetStack) -> Result<FlushReport> {
    match stack.flush_resolver_cache() {
        Ok(()) => {
            safety.log_op(OP_FLUSH, FLUSH_TARGET, "flushed", None)?;
            Ok(FlushReport {
                session_id: safety.session_id(),
            })
        }
        Err(e) => {
            if let Err(log_error) =
                safety.log_op(OP_FLUSH, FLUSH_TARGET, "failed", Some(&e.to_string()))
            {
                tracing::warn!(error = %log_error, "cannot log the failed DNS cache flush");
            }
            Err(e)
        }
    }
}

/// Why the lease of `adapter` cannot be renewed; `None` when it can.
fn renew_refusal(adapter: &Adapter) -> Option<&'static str> {
    if adapter.can_renew {
        None
    } else if !adapter.dhcp_enabled || !adapter.ipv4_enabled {
        Some("this adapter does not get its IPv4 address from DHCP")
    } else if adapter.status != LinkStatus::Connected {
        Some("this adapter is not connected")
    } else {
        Some("the lease of this type of adapter cannot be renewed here")
    }
}

/// Renews the IPv4 DHCP lease of `adapter_id` (id, name or interface index), releasing it
/// first when asked. Both steps are logged. A renewal is attempted after every successful
/// release, so the adapter is never left without a lease request.
pub(crate) fn renew_with(
    safety: &Safety,
    stack: &dyn NetStack,
    adapter_id: &str,
    release_first: bool,
) -> Result<RenewReport> {
    safety.ensure_elevated()?;
    let started = Instant::now();
    let listed = list_with(stack, None)?;
    let adapter = find_adapter(&listed.adapters, adapter_id)?;
    if let Some(reason) = renew_refusal(adapter) {
        return Err(Error::Other(reason.to_string()));
    }
    let target = format!("IPv4 lease of {}", adapter.name);

    let mut release_log = Ok(());
    if release_first {
        if let Err(e) = stack.release_lease(adapter.if_index) {
            safety.log_op(OP_RELEASE, &target, "failed", Some(&e.to_string()))?;
            return Err(e);
        }
        release_log = safety.log_op(OP_RELEASE, &target, "released", None);
    }
    let renewed = stack.renew_lease(adapter.if_index);
    let renew_log = match &renewed {
        Ok(()) => safety.log_op(OP_RENEW, &target, "renewed", None),
        Err(e) => safety.log_op(OP_RENEW, &target, "failed", Some(&e.to_string())),
    };
    release_log?;
    renewed?;
    renew_log?;

    let ipv4 = match stack.adapters() {
        Ok(now) => now
            .iter()
            .find(|a| a.id == adapter.id)
            .map(|a| {
                a.ipv4
                    .iter()
                    .filter(|x| x.origin != AddressOrigin::LinkLocal)
                    .map(|x| x.address.clone())
                    .collect()
            })
            .unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the adapters after renewing a lease");
            Vec::new()
        }
    };
    Ok(RenewReport {
        adapter_id: adapter.id.clone(),
        adapter_name: adapter.name.clone(),
        session_id: safety.session_id(),
        released: release_first,
        renewed: true,
        ipv4,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// The manually set addresses and DNS servers of `adapters` that a reset removes. VPN and
/// tunnel adapters are left out: their software sets them up again.
///
/// A static IPv4 address names the adapter's IPv4 gateway, which DHCP cannot have set. A
/// static IPv6 address names an IPv6 gateway only when the adapter has no address from a
/// router advertisement or DHCPv6: otherwise the gateway most likely came from a router
/// advertisement, which provides it again after the reset.
pub(crate) fn manual_settings(adapters: &[Adapter]) -> Vec<ManualSetting> {
    let mut settings = Vec::new();
    for adapter in adapters {
        if matches!(adapter.kind, AdapterKind::Vpn | AdapterKind::Tunnel) {
            continue;
        }
        let gateway = |v6: bool| {
            adapter
                .gateways
                .iter()
                .find(|g| g.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6() == v6))
        };
        let advertised = adapter.ipv6.iter().any(|a| {
            matches!(
                a.origin,
                AddressOrigin::Autoconfigured | AddressOrigin::Dhcp | AddressOrigin::Temporary
            )
        });
        let families = [
            ("IPv4", &adapter.ipv4, gateway(false)),
            (
                "IPv6",
                &adapter.ipv6,
                if advertised { None } else { gateway(true) },
            ),
        ];
        for (label, addresses, gateway) in families {
            for address in addresses
                .iter()
                .filter(|a| a.origin == AddressOrigin::Manual)
            {
                let mut detail = format!(
                    "static {label} {}/{}",
                    address.address, address.prefix_length
                );
                if let Some(gateway) = gateway {
                    detail.push_str(&format!(", gateway {gateway}"));
                }
                settings.push(ManualSetting {
                    adapter: adapter.name.clone(),
                    detail,
                });
            }
        }
        for family in IpFamily::ALL {
            let dns = adapter.dns(family);
            if dns.mode == DnsMode::Manual && !dns.servers.is_empty() {
                settings.push(ManualSetting {
                    adapter: adapter.name.clone(),
                    detail: format!(
                        "{} DNS {} (set manually)",
                        family.label(),
                        dns.servers.join(", ")
                    ),
                });
            }
        }
    }
    settings
}

/// Prefix length of a contiguous IPv4 subnet mask such as `255.255.255.0`.
fn mask_prefix_length(mask: &str) -> Option<u32> {
    let bits = u32::from(mask.trim().parse::<Ipv4Addr>().ok()?);
    let length = bits.leading_ones();
    (bits.count_ones() == length).then_some(length)
}

/// An IPv4 address stored for an interface, unless it is empty or `0.0.0.0`.
fn stored_ipv4(text: &str) -> Option<Ipv4Addr> {
    text.trim()
        .parse::<Ipv4Addr>()
        .ok()
        .filter(|ip| !ip.is_unspecified())
}

/// Whether a stored interface belongs to a VPN client, from its name, driver description or
/// driver component id (the TAP, Wintun and WireGuard drivers).
fn stored_is_vpn(stored: &StoredInterface) -> bool {
    let component = stored.component_id.to_ascii_lowercase();
    names_vpn(&stored.name, &stored.description)
        || component.starts_with("tap")
        || component.contains("wintun")
        || component.contains("wireguard")
}

/// The manual settings Windows keeps for interfaces whose adapters `listed` does not
/// include (unplugged, disabled or removed): static IPv4 addresses with their gateway and
/// static DNS servers of both families. The reset removes them as well. VPN adapters are
/// left out, like listed ones. Static IPv6 addresses are not kept in these settings.
pub(crate) fn unlisted_manual_settings(
    stored: &[StoredInterface],
    listed: &[Adapter],
) -> Vec<ManualSetting> {
    let mut settings = Vec::new();
    for interface in stored {
        let guid = canonical_guid(&interface.guid);
        if listed.iter().any(|a| a.id == guid) || stored_is_vpn(interface) {
            continue;
        }
        let name = [&interface.name, &interface.description]
            .into_iter()
            .map(|s| s.trim())
            .find(|s| !s.is_empty())
            .unwrap_or(guid.as_str());
        let adapter = format!("{name}{UNLISTED_SUFFIX}");
        if interface.static_ipv4 {
            let gateway = interface.gateways.iter().find_map(|g| stored_ipv4(g));
            for (index, text) in interface.addresses.iter().enumerate() {
                let Some(address) = stored_ipv4(text) else {
                    continue;
                };
                let mut detail = match interface
                    .masks
                    .get(index)
                    .and_then(|m| mask_prefix_length(m))
                {
                    Some(length) => format!("static IPv4 {address}/{length}"),
                    None => format!("static IPv4 {address}"),
                };
                if let Some(gateway) = gateway {
                    detail.push_str(&format!(", gateway {gateway}"));
                }
                settings.push(ManualSetting {
                    adapter: adapter.clone(),
                    detail,
                });
            }
        }
        for family in IpFamily::ALL {
            let servers = split_servers(interface.dns(family));
            if !servers.is_empty() {
                settings.push(ManualSetting {
                    adapter: adapter.clone(),
                    detail: format!(
                        "{} DNS {} (set manually)",
                        family.label(),
                        servers.join(", ")
                    ),
                });
            }
        }
    }
    settings
}

/// Every manual setting a reset removes: those of the listed `adapters`, then those kept for
/// adapters that are not listed. When the latter cannot be read, the warning says so.
fn settings_removed(
    stack: &dyn NetStack,
    adapters: &[Adapter],
) -> (Vec<ManualSetting>, Option<String>) {
    let mut settings = manual_settings(adapters);
    match stack.stored_interfaces() {
        Ok(stored) => {
            settings.extend(unlisted_manual_settings(&stored, adapters));
            (settings, None)
        }
        Err(e) => (settings, Some(format!("{UNLISTED_WARNING}: {e}"))),
    }
}

/// The last `max` characters of `text` after trimming it.
pub(crate) fn tail(text: &str, max: usize) -> String {
    let text = text.trim();
    match text.char_indices().rev().nth(max.saturating_sub(1)) {
        Some((start, _)) if max > 0 => text[start..].to_string(),
        _ if max == 0 => String::new(),
        _ => text.to_string(),
    }
}

/// Both output streams of a command, stdout first.
fn combined_output(out: &CommandOutput) -> String {
    let stdout = out.stdout.trim();
    let stderr = out.stderr.trim();
    match (stdout.is_empty(), stderr.is_empty()) {
        (_, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn planned_steps() -> Vec<ResetStep> {
    RESET_STEPS
        .iter()
        .map(|(id, title, args)| ResetStep {
            id: id.to_string(),
            title: title.to_string(),
            command: format!("netsh {}", args.join(" ")),
            status: StepStatus::Planned,
            exit_code: None,
            output: String::new(),
        })
        .collect()
}

/// Assembles the report of a reset that ran: the timeout warning first, then the session's
/// warnings (the restore point outcome).
fn finished_report(
    session_id: i64,
    steps: Vec<ResetStep>,
    manual_settings: Vec<ManualSetting>,
    restore_point: Option<RestorePoint>,
    session_warnings: &[String],
) -> ResetReport {
    let mut warnings = Vec::new();
    if steps.iter().any(|s| s.status == StepStatus::TimedOut) {
        warnings.push(TIMEOUT_WARNING.to_string());
    }
    warnings.extend(session_warnings.iter().cloned());
    let restart_required = steps
        .iter()
        .any(|s| !matches!(s.status, StepStatus::Planned | StepStatus::NotRun));
    ResetReport {
        dry_run: false,
        session_id: Some(session_id),
        restore_point,
        steps,
        manual_settings,
        restart_required,
        warnings,
    }
}

/// Plans the reset when `dry_run` is set, else opens a session with `begin` (which creates
/// the restore point) and runs it. A plan never calls `begin`, so it opens no journal
/// session and needs no elevation.
pub(crate) fn plan_or_reset(
    stack: &dyn NetStack,
    runner: &dyn CommandRunner,
    system_dir: &Path,
    dry_run: bool,
    begin: impl FnOnce() -> Result<Safety>,
) -> Result<ResetReport> {
    if dry_run {
        return reset_with(None, stack, runner, system_dir);
    }
    let safety = begin()?;
    reset_with(Some(&safety), stack, runner, system_dir)
}

/// Plans (`safety` None) or runs the network stack reset with `netsh.exe` from
/// `system_dir`. A run records the manual settings it removes (of listed adapters and of
/// adapters that are unplugged or disabled) before the first step, logs every step as
/// started and with its outcome, never kills a step at its deadline and skips the steps
/// after one that timed out.
pub(crate) fn reset_with(
    safety: Option<&Safety>,
    stack: &dyn NetStack,
    runner: &dyn CommandRunner,
    system_dir: &Path,
) -> Result<ResetReport> {
    let Some(safety) = safety else {
        let (manual, warnings) = match list_with(stack, None) {
            Ok(listed) => {
                let mut warnings = listed.warnings;
                let (manual, unread) = settings_removed(stack, &listed.adapters);
                warnings.extend(unread);
                (manual, warnings)
            }
            Err(e) => (
                Vec::new(),
                vec![format!("cannot list the adapters' manual settings: {e}")],
            ),
        };
        return Ok(ResetReport {
            dry_run: true,
            session_id: None,
            restore_point: None,
            steps: planned_steps(),
            manual_settings: manual,
            restart_required: true,
            warnings,
        });
    };

    safety.ensure_elevated()?;
    let listed = list_with(stack, None)?;
    let (manual, unread) = settings_removed(stack, &listed.adapters);
    let mut before = if manual.is_empty() {
        "none".to_string()
    } else {
        manual
            .iter()
            .map(|m| format!("{}: {}", m.adapter, m.detail))
            .collect::<Vec<_>>()
            .join("; ")
    };
    if let Some(warning) = &unread {
        before.push_str(&format!(" ({warning})"));
    }
    safety.log_op(OP_RESET, "settings before reset", "recorded", Some(&before))?;

    let netsh = system_dir.join("netsh.exe");
    let mut steps = Vec::new();
    let mut stopped = false;
    for (id, title, args) in RESET_STEPS {
        let command = format!("netsh {}", args.join(" "));
        if stopped {
            safety.log_op(OP_RESET, title, "not_run", None)?;
            steps.push(ResetStep {
                id: id.to_string(),
                title: title.to_string(),
                command,
                status: StepStatus::NotRun,
                exit_code: None,
                output: String::new(),
            });
            continue;
        }
        safety.log_op(OP_RESET, title, "started", Some(&command))?;
        let (status, exit_code, output, detail) =
            match runner.run(&netsh, args, STEP_TIMEOUT, OnTimeout::Leave) {
                Ok(out) => {
                    let output = combined_output(&out);
                    let short = tail(&output, LOG_OUTPUT_CHARS);
                    if out.timed_out {
                        stopped = true;
                        let detail =
                            format!("still running after {} s: {short}", STEP_TIMEOUT.as_secs());
                        (StepStatus::TimedOut, None, output, detail)
                    } else {
                        let status = match out.exit_code {
                            Some(0) => StepStatus::Succeeded,
                            Some(_) if id != "winsock" => StepStatus::CompletedWithErrors,
                            _ => StepStatus::Failed,
                        };
                        let code = out
                            .exit_code
                            .map_or_else(|| "unknown".to_string(), |c| c.to_string());
                        (
                            status,
                            out.exit_code,
                            output,
                            format!("exit {code}: {short}"),
                        )
                    }
                }
                Err(e) => {
                    let text = format!("could not run netsh: {e}");
                    (StepStatus::Failed, None, text.clone(), text)
                }
            };
        safety.log_op(OP_RESET, title, status.as_str(), Some(detail.trim_end()))?;
        steps.push(ResetStep {
            id: id.to_string(),
            title: title.to_string(),
            command,
            status,
            exit_code,
            output: tail(&output, REPORT_OUTPUT_CHARS),
        });
    }
    let mut report = finished_report(
        safety.session_id(),
        steps,
        manual,
        safety.restore_point().cloned(),
        safety.warnings(),
    );
    report.warnings.extend(unread);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Arc;

    use chrono::Utc;

    use super::*;
    use crate::network::adapters::AddressInfo;
    use crate::network::tests::{adapter, guid, journal, FakeStack};
    use crate::safety::state_log::Journal;
    use crate::safety::{test_safety, test_safety_with_outcome};

    type Call = (PathBuf, Vec<String>, Duration, OnTimeout);

    /// Runs nothing: records each call and answers from a script (exit 0 once the script
    /// is used up).
    #[derive(Default)]
    struct FakeRunner {
        calls: RefCell<Vec<Call>>,
        script: RefCell<VecDeque<Result<CommandOutput>>>,
        before_run: Option<Box<dyn Fn(usize)>>,
    }

    impl FakeRunner {
        fn scripted(outputs: Vec<Result<CommandOutput>>) -> FakeRunner {
            FakeRunner {
                script: RefCell::new(outputs.into()),
                ..Default::default()
            }
        }

        fn args(&self) -> Vec<String> {
            self.calls.borrow().iter().map(|c| c.1.join(" ")).collect()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            program: &Path,
            args: &[&str],
            timeout: Duration,
            on_timeout: OnTimeout,
        ) -> Result<CommandOutput> {
            let index = self.calls.borrow().len();
            self.calls.borrow_mut().push((
                program.to_path_buf(),
                args.iter().map(|a| a.to_string()).collect(),
                timeout,
                on_timeout,
            ));
            if let Some(hook) = &self.before_run {
                hook(index);
            }
            self.script
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Ok(exit(0, "Ok.")))
        }
    }

    fn exit(code: i32, stdout: &str) -> CommandOutput {
        CommandOutput {
            exit_code: Some(code),
            stdout: stdout.to_string(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    fn timed_out(stdout: &str) -> CommandOutput {
        CommandOutput {
            exit_code: None,
            stdout: stdout.to_string(),
            stderr: String::new(),
            timed_out: true,
        }
    }

    fn system_dir() -> PathBuf {
        PathBuf::from(r"C:\Windows\System32")
    }

    /// Wi-Fi on DHCP with manual DNS, and Ethernet with a static address.
    fn stack() -> FakeStack {
        let wifi = adapter(1, "Wi-Fi", AdapterKind::Wifi);
        let mut ethernet = adapter(2, "Ethernet", AdapterKind::Ethernet);
        ethernet.dhcp_enabled = false;
        ethernet.ipv4 = vec![AddressInfo {
            address: "192.168.1.50".into(),
            prefix_length: 24,
            origin: AddressOrigin::Manual,
            preferred: true,
        }];
        ethernet.gateways = vec!["fe80::1".into(), "192.168.1.1".into()];
        let stack = FakeStack::with(vec![wifi, ethernet]);
        stack.set_static(&guid(1), IpFamily::Ipv4, "1.1.1.1,1.0.0.1");
        stack
    }

    /// Audit rows of the reset, oldest first: (target, outcome, detail).
    fn reset_rows(journal: &Journal) -> Vec<(String, String, Option<String>)> {
        let mut rows: Vec<_> = journal
            .ops(100)
            .unwrap()
            .into_iter()
            .filter(|o| o.op == OP_RESET)
            .map(|o| (o.target, o.outcome, o.detail))
            .collect();
        rows.reverse();
        rows
    }

    fn statuses(report: &ResetReport) -> Vec<StepStatus> {
        report.steps.iter().map(|s| s.status).collect()
    }

    #[test]
    fn plan_reset_runs_nothing() {
        let (_dir, journal) = journal();
        let runner = FakeRunner::default();
        let stack = stack();
        let begun = Cell::new(0);
        let begin = || {
            begun.set(begun.get() + 1);
            Ok(test_safety(Arc::clone(&journal), "reset", false))
        };
        let report = plan_or_reset(&stack, &runner, &system_dir(), true, begin).unwrap();
        assert_eq!(begun.get(), 0, "a plan never opens a session");
        assert!(report.dry_run);
        assert_eq!(report.session_id, None);
        assert!(report.restore_point.is_none());
        assert!(report.restart_required);
        assert_eq!(statuses(&report), vec![StepStatus::Planned; 3]);
        let commands: Vec<&str> = report.steps.iter().map(|s| s.command.as_str()).collect();
        assert_eq!(
            commands,
            vec![
                "netsh winsock reset",
                "netsh int ip reset",
                "netsh int ipv6 reset"
            ]
        );
        assert_eq!(report.manual_settings.len(), 2);
        assert!(runner.calls.borrow().is_empty());
        assert!(stack.calls().is_empty());
        assert!(journal.sessions().unwrap().is_empty());
        assert!(journal.ops(10).unwrap().is_empty());
    }

    #[test]
    fn a_reset_opens_its_session_before_anything_runs() {
        let (_dir, journal) = journal();
        let stack = stack();
        let runner = FakeRunner::default();
        let begun = Cell::new(0);
        let begin = || {
            begun.set(begun.get() + 1);
            assert!(runner.calls.borrow().is_empty(), "the session comes first");
            Ok(test_safety(Arc::clone(&journal), "reset", false))
        };
        let report = plan_or_reset(&stack, &runner, &system_dir(), false, begin).unwrap();
        assert_eq!(begun.get(), 1);
        assert!(!report.dry_run);
        let sessions = journal.sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(report.session_id, Some(sessions[0].id));
        assert_eq!(runner.calls.borrow().len(), 3);

        // A session that cannot be opened (no elevation, or a required restore point that
        // failed) stops the reset before any step runs.
        let runner = FakeRunner::default();
        let err = plan_or_reset(&stack, &runner, &system_dir(), false, || {
            Err(Error::NotElevated)
        })
        .unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");
        assert!(runner.calls.borrow().is_empty());
        assert_eq!(journal.sessions().unwrap().len(), 1);
    }

    #[test]
    fn reset_runs_three_netsh_steps_in_order_with_absolute_path() {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "reset", false);
        let runner = FakeRunner::default();
        let report = reset_with(Some(&safety), &stack(), &runner, &system_dir()).unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.session_id, Some(safety.session_id()));
        assert_eq!(statuses(&report), vec![StepStatus::Succeeded; 3]);
        assert!(report.restart_required);
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 3);
        for call in calls.iter() {
            assert_eq!(call.0, PathBuf::from(r"C:\Windows\System32\netsh.exe"));
            assert!(call.0.is_absolute());
            assert_eq!(call.2, STEP_TIMEOUT);
            assert_eq!(call.3, OnTimeout::Leave);
        }
        assert_eq!(
            runner.args(),
            vec!["winsock reset", "int ip reset", "int ipv6 reset"]
        );
        assert_eq!(report.steps[0].exit_code, Some(0));
        assert_eq!(report.steps[0].output, "Ok.");
        assert_eq!(report.steps[1].id, "ipv4");
        assert_eq!(report.steps[2].title, "Reset TCP/IP for IPv6");
    }

    #[test]
    fn manual_settings_are_audited_before_the_first_step() {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "reset", false);
        let seen = Arc::clone(&journal);
        let runner = FakeRunner {
            before_run: Some(Box::new(move |index| {
                if index == 0 {
                    let rows = reset_rows(&seen);
                    assert_eq!(rows.len(), 2, "{rows:?}");
                    assert_eq!(rows[0].0, "settings before reset");
                    assert_eq!(rows[0].1, "recorded");
                    assert_eq!(rows[1].1, "started");
                }
            })),
            ..Default::default()
        };
        reset_with(Some(&safety), &stack(), &runner, &system_dir()).unwrap();
        let rows = reset_rows(&journal);
        assert_eq!(
            rows[0].2.as_deref(),
            Some(
                "Wi-Fi: IPv4 DNS 1.1.1.1, 1.0.0.1 (set manually); Ethernet: static IPv4 \
                 192.168.1.50/24, gateway 192.168.1.1"
            )
        );

        let (_dir, journal) = crate::network::tests::journal();
        let safety = test_safety(Arc::clone(&journal), "reset", false);
        let plain = FakeStack::with(vec![adapter(1, "Wi-Fi", AdapterKind::Wifi)]);
        reset_with(Some(&safety), &plain, &FakeRunner::default(), &system_dir()).unwrap();
        assert_eq!(reset_rows(&journal)[0].2.as_deref(), Some("none"));
    }

    #[test]
    fn a_failed_adapter_listing_stops_the_reset_before_anything_runs() {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "reset", false);
        let stack = stack();
        *stack.fail_adapters.borrow_mut() = Some("The RPC server is unavailable.".into());
        let runner = FakeRunner::default();
        let err = reset_with(Some(&safety), &stack, &runner, &system_dir()).unwrap_err();
        assert_eq!(err.to_string(), "The RPC server is unavailable.");
        assert!(runner.calls.borrow().is_empty());
        assert!(reset_rows(&journal).is_empty());

        let plan = reset_with(None, &stack, &runner, &system_dir()).unwrap();
        assert!(plan.manual_settings.is_empty());
        assert_eq!(
            plan.warnings,
            vec!["cannot list the adapters' manual settings: The RPC server is unavailable."]
        );
    }

    #[test]
    fn each_step_logs_started_then_outcome() {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "reset", false);
        let runner = FakeRunner::scripted(vec![
            Ok(exit(0, "Sucessfully reset the Winsock Catalog.")),
            Ok(exit(0, "Resetting Interface, OK!")),
            Ok(exit(0, "Resetting Interface, OK!")),
        ]);
        reset_with(Some(&safety), &stack(), &runner, &system_dir()).unwrap();
        let rows: Vec<(String, String)> = reset_rows(&journal)
            .into_iter()
            .map(|r| (r.0, r.1))
            .collect();
        let expected: Vec<(String, String)> = [
            ("settings before reset", "recorded"),
            ("Reset the Winsock catalog", "started"),
            ("Reset the Winsock catalog", "succeeded"),
            ("Reset TCP/IP for IPv4", "started"),
            ("Reset TCP/IP for IPv4", "succeeded"),
            ("Reset TCP/IP for IPv6", "started"),
            ("Reset TCP/IP for IPv6", "succeeded"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        assert_eq!(rows, expected);
        let all = reset_rows(&journal);
        assert_eq!(all[1].2.as_deref(), Some("netsh winsock reset"));
        assert_eq!(
            all[2].2.as_deref(),
            Some("exit 0: Sucessfully reset the Winsock Catalog.")
        );
        assert!(
            journal.active_dns().unwrap().is_empty(),
            "nothing journaled"
        );
    }

    #[test]
    fn winsock_nonzero_is_failed_ip_nonzero_is_completed_with_errors() {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "reset", false);
        let runner = FakeRunner::scripted(vec![
            Ok(exit(1, "The requested operation requires elevation.")),
            Ok(exit(1, "Resetting Neighbor, failed.\nAccess is denied.")),
            Ok(exit(5, "Access is denied.")),
        ]);
        let report = reset_with(Some(&safety), &stack(), &runner, &system_dir()).unwrap();
        assert_eq!(
            statuses(&report),
            vec![
                StepStatus::Failed,
                StepStatus::CompletedWithErrors,
                StepStatus::CompletedWithErrors
            ]
        );
        assert_eq!(report.steps[2].exit_code, Some(5));
        let outcomes: Vec<String> = reset_rows(&journal)
            .into_iter()
            .filter(|r| r.1 != "started")
            .map(|r| r.1)
            .collect();
        assert_eq!(
            outcomes,
            vec![
                "recorded",
                "failed",
                "completed_with_errors",
                "completed_with_errors"
            ]
        );
    }

    #[test]
    fn timeout_leaves_the_process_and_skips_remaining_steps() {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "reset", false);
        let runner = FakeRunner::scripted(vec![
            Ok(exit(0, "Ok.")),
            Ok(timed_out("Resetting Compartment Forwarding, OK!")),
        ]);
        let report = reset_with(Some(&safety), &stack(), &runner, &system_dir()).unwrap();
        assert_eq!(
            statuses(&report),
            vec![
                StepStatus::Succeeded,
                StepStatus::TimedOut,
                StepStatus::NotRun
            ]
        );
        assert_eq!(runner.calls.borrow().len(), 2, "the IPv6 step never ran");
        assert!(runner
            .calls
            .borrow()
            .iter()
            .all(|c| c.3 == OnTimeout::Leave));
        assert_eq!(report.warnings, vec![TIMEOUT_WARNING.to_string()]);
        assert!(report.restart_required);
        assert_eq!(report.steps[1].exit_code, None);
        assert_eq!(
            report.steps[1].output,
            "Resetting Compartment Forwarding, OK!"
        );
        let rows = reset_rows(&journal);
        let last = rows.last().unwrap();
        assert_eq!(
            (last.0.as_str(), last.1.as_str()),
            ("Reset TCP/IP for IPv6", "not_run")
        );
        let timed = &rows[rows.len() - 2];
        assert_eq!(timed.1, "timed_out");
        assert_eq!(
            timed.2.as_deref(),
            Some("still running after 60 s: Resetting Compartment Forwarding, OK!")
        );
    }

    #[test]
    fn failed_step_continues_and_is_reported() {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "reset", false);
        let runner = FakeRunner::scripted(vec![Err(Error::Other(
            "the system cannot find the file specified".into(),
        ))]);
        let report = reset_with(Some(&safety), &stack(), &runner, &system_dir()).unwrap();
        assert_eq!(
            statuses(&report),
            vec![
                StepStatus::Failed,
                StepStatus::Succeeded,
                StepStatus::Succeeded
            ]
        );
        assert_eq!(
            report.steps[0].output,
            "could not run netsh: the system cannot find the file specified"
        );
        assert_eq!(runner.calls.borrow().len(), 3);
        assert!(report.restart_required);
        assert!(report.warnings.is_empty());
        let rows = reset_rows(&journal);
        assert_eq!(rows[2].1, "failed");
        assert_eq!(
            rows[2].2.as_deref(),
            Some("could not run netsh: the system cannot find the file specified")
        );
    }

    #[test]
    fn output_is_tail_limited_in_report_and_log() {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "reset", false);
        let long: String = (0..2000).map(|i| format!("line {i:04}\n")).collect();
        let runner = FakeRunner::scripted(vec![Ok(CommandOutput {
            exit_code: Some(0),
            stdout: long.clone(),
            stderr: "warning: Ä".into(),
            timed_out: false,
        })]);
        let report = reset_with(Some(&safety), &stack(), &runner, &system_dir()).unwrap();
        let output = &report.steps[0].output;
        assert_eq!(output.chars().count(), REPORT_OUTPUT_CHARS);
        assert!(
            output.ends_with("line 1999\nwarning: Ä"),
            "stderr follows stdout"
        );
        let detail = reset_rows(&journal)[2].2.clone().unwrap();
        assert!(detail.starts_with("exit 0: "));
        assert_eq!(detail.chars().count(), "exit 0: ".len() + LOG_OUTPUT_CHARS);
        assert!(detail.ends_with("warning: Ä"));

        assert_eq!(tail("  abc  ", 2), "bc");
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(tail("ÄÖÜ", 2), "ÖÜ");
        assert_eq!(tail("abc", 0), "");
    }

    #[test]
    fn reset_reports_restore_point_warning() {
        let steps = vec![ResetStep {
            id: "winsock".into(),
            title: "Reset the Winsock catalog".into(),
            command: "netsh winsock reset".into(),
            status: StepStatus::TimedOut,
            exit_code: None,
            output: String::new(),
        }];
        let warning = "restore point unavailable: System Protection is turned off".to_string();
        let report = finished_report(
            7,
            steps.clone(),
            Vec::new(),
            None,
            std::slice::from_ref(&warning),
        );
        assert_eq!(
            report.warnings,
            vec![TIMEOUT_WARNING.to_string(), warning.clone()]
        );
        assert!(report.restore_point.is_none());
        assert_eq!(report.session_id, Some(7));

        let point = RestorePoint {
            sequence: 9,
            description: "Cairn: network: reset".into(),
            created_at: Utc::now(),
        };
        let mut done = steps;
        done[0].status = StepStatus::Succeeded;
        let report = finished_report(7, done, Vec::new(), Some(point.clone()), &[]);
        assert_eq!(report.restore_point.as_ref().map(|p| p.sequence), Some(9));
        assert!(report.warnings.is_empty());
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["restore_point"]["sequence"], 9);
        assert_eq!(json["steps"][0]["status"], "succeeded");

        // A whole reset hands on what the session holds: the warning of a restore point
        // that could not be created, or the restore point itself.
        let (_dir, failed) = journal();
        let safety =
            test_safety_with_outcome(Arc::clone(&failed), "reset", None, vec![warning.clone()]);
        let report = reset_with(
            Some(&safety),
            &stack(),
            &FakeRunner::default(),
            &system_dir(),
        )
        .unwrap();
        assert_eq!(report.warnings, vec![warning]);
        assert!(report.restore_point.is_none());

        let (_dir, created) = journal();
        let safety =
            test_safety_with_outcome(Arc::clone(&created), "reset", Some(point), Vec::new());
        let report = reset_with(
            Some(&safety),
            &stack(),
            &FakeRunner::default(),
            &system_dir(),
        )
        .unwrap();
        assert_eq!(report.restore_point.as_ref().map(|p| p.sequence), Some(9));
        assert!(report.warnings.is_empty());
        assert_eq!(report.session_id, Some(safety.session_id()));
    }

    #[test]
    fn manual_settings_list_static_ip_and_manual_dns() {
        let mut vpn = adapter(3, "ProtonVPN", AdapterKind::Vpn);
        vpn.dhcp_enabled = false;
        vpn.ipv4[0].origin = AddressOrigin::Manual;
        let stack = stack();
        stack.adapters.borrow_mut().push(vpn);
        stack.set_static(&guid(2), IpFamily::Ipv6, "2620:fe::fe");
        stack.set_static(&guid(3), IpFamily::Ipv4, "10.2.0.1");
        let listed = list_with(&stack, None).unwrap();
        let settings = manual_settings(&listed.adapters);
        let text: Vec<String> = settings
            .iter()
            .map(|s| format!("{}: {}", s.adapter, s.detail))
            .collect();
        assert_eq!(
            text,
            vec![
                "Wi-Fi: IPv4 DNS 1.1.1.1, 1.0.0.1 (set manually)",
                "Ethernet: static IPv4 192.168.1.50/24, gateway 192.168.1.1",
                "Ethernet: IPv6 DNS 2620:fe::fe (set manually)",
            ]
        );

        let mut no_gateway = adapter(4, "Lab", AdapterKind::Ethernet);
        no_gateway.gateways.clear();
        no_gateway.ipv4[0].origin = AddressOrigin::Manual;
        let settings = manual_settings(&[no_gateway]);
        assert_eq!(settings[0].detail, "static IPv4 192.168.0.14/24");
    }

    fn v6(address: &str, origin: AddressOrigin) -> AddressInfo {
        AddressInfo {
            address: address.into(),
            prefix_length: 64,
            origin,
            preferred: true,
        }
    }

    #[test]
    fn manual_settings_list_static_ipv6_addresses() {
        // DHCP for IPv4, a static IPv6 address and gateway, and no router advertisements.
        let mut lab = adapter(1, "Ethernet", AdapterKind::Ethernet);
        lab.ipv6 = vec![
            v6("fe80::1234", AddressOrigin::LinkLocal),
            v6("2001:db8:1::50", AddressOrigin::Manual),
        ];
        lab.gateways = vec!["2001:db8:1::1".into(), "192.168.0.1".into()];
        let text = |adapters: &[Adapter]| -> Vec<String> {
            manual_settings(adapters)
                .iter()
                .map(|s| format!("{}: {}", s.adapter, s.detail))
                .collect()
        };
        assert_eq!(
            text(&[lab.clone()]),
            vec!["Ethernet: static IPv6 2001:db8:1::50/64, gateway 2001:db8:1::1"]
        );

        // With an address from a router advertisement, the IPv6 gateway most likely came
        // from it as well, so only the address is named.
        let mut advertised = lab.clone();
        advertised
            .ipv6
            .push(v6("2001:db8:2::abcd", AddressOrigin::Autoconfigured));
        advertised.gateways = vec!["fe80::1".into(), "192.168.0.1".into()];
        assert_eq!(
            text(&[advertised.clone()]),
            vec!["Ethernet: static IPv6 2001:db8:1::50/64"]
        );
        let mut temporary = advertised;
        temporary.ipv6[2].origin = AddressOrigin::Temporary;
        assert_eq!(
            text(&[temporary]),
            vec!["Ethernet: static IPv6 2001:db8:1::50/64"]
        );

        // Static addresses of both families each name their own family's gateway.
        let mut both = lab;
        both.dhcp_enabled = false;
        both.ipv4[0].origin = AddressOrigin::Manual;
        assert_eq!(
            text(&[both.clone()]),
            vec![
                "Ethernet: static IPv4 192.168.0.11/24, gateway 192.168.0.1",
                "Ethernet: static IPv6 2001:db8:1::50/64, gateway 2001:db8:1::1",
            ]
        );
        let mut vpn = both;
        vpn.kind = AdapterKind::Vpn;
        assert!(text(&[vpn]).is_empty(), "VPN software sets them up again");

        // The plan lists them through the whole listing.
        let stack = FakeStack::with(vec![]);
        let mut listed = adapter(2, "Ethernet 2", AdapterKind::Ethernet);
        listed.ipv6 = vec![v6("2001:db8:9::9", AddressOrigin::Manual)];
        stack.adapters.borrow_mut().push(listed);
        let plan = reset_with(None, &stack, &FakeRunner::default(), &system_dir()).unwrap();
        assert_eq!(
            plan.manual_settings,
            vec![ManualSetting {
                adapter: "Ethernet 2".into(),
                detail: "static IPv6 2001:db8:9::9/64".into(),
            }]
        );
    }

    fn stored(guid_number: u32, name: &str) -> StoredInterface {
        StoredInterface {
            guid: guid(guid_number).to_uppercase(),
            name: name.into(),
            description: format!("{name} adapter"),
            component_id: String::new(),
            static_ipv4: false,
            addresses: Vec::new(),
            masks: Vec::new(),
            gateways: Vec::new(),
            ipv4_dns: String::new(),
            ipv6_dns: String::new(),
        }
    }

    #[test]
    fn settings_of_unplugged_and_disabled_adapters_are_listed() {
        // A USB adapter with a static address, unplugged.
        let mut dongle = stored(7, "Ethernet 3");
        dongle.static_ipv4 = true;
        dongle.addresses = vec!["192.168.1.10".into(), "10.0.0.2".into()];
        dongle.masks = vec!["255.255.255.0".into(), "255.0.255.0".into()];
        dongle.gateways = vec!["0.0.0.0".into(), "192.168.1.1".into()];
        dongle.ipv4_dns = "192.168.1.1".into();
        // A disabled adapter with manual IPv6 DNS only, known by its driver alone.
        let mut disabled = stored(8, "");
        disabled.description = "Intel(R) Ethernet Connection I219-V".into();
        disabled.ipv6_dns = "2620:fe::fe 2620:fe::9".into();
        // A key that keeps an address although DHCP is on, and one without any name.
        let mut dhcp = stored(9, "Ethernet 4");
        dhcp.addresses = vec!["192.168.5.5".into()];
        let mut nameless = stored(10, "");
        nameless.description.clear();
        nameless.static_ipv4 = true;
        nameless.addresses = vec!["0.0.0.0".into(), "172.16.0.9".into()];
        nameless.masks = vec!["0.0.0.0".into()];
        // VPN adapters, recognised by name, description or driver.
        let mut vpn = stored(11, "Ethernet 5");
        vpn.description = "TAP-NordVPN Windows Adapter V9".into();
        vpn.ipv4_dns = "103.86.96.100".into();
        let mut wireguard = stored(12, "ProtonVPN");
        wireguard.ipv4_dns = "10.2.0.1".into();
        let mut tap = stored(13, "Ethernet 6");
        tap.component_id = "tap0901".into();
        tap.ipv4_dns = "10.8.0.1".into();
        // The listed Wi-Fi adapter's key is read through its listing instead.
        let mut wifi_key = stored(1, "Wi-Fi");
        wifi_key.ipv4_dns = "1.1.1.1,1.0.0.1".into();

        let stack = stack();
        *stack.stored.borrow_mut() = vec![
            wifi_key, dongle, disabled, dhcp, nameless, vpn, wireguard, tap,
        ];
        let plan = reset_with(None, &stack, &FakeRunner::default(), &system_dir()).unwrap();
        let text: Vec<String> = plan
            .manual_settings
            .iter()
            .map(|s| format!("{}: {}", s.adapter, s.detail))
            .collect();
        assert_eq!(
            text,
            vec![
                "Wi-Fi: IPv4 DNS 1.1.1.1, 1.0.0.1 (set manually)".to_string(),
                "Ethernet: static IPv4 192.168.1.50/24, gateway 192.168.1.1".to_string(),
                "Ethernet 3 (unplugged or disabled): static IPv4 192.168.1.10/24, gateway \
                 192.168.1.1"
                    .to_string(),
                "Ethernet 3 (unplugged or disabled): static IPv4 10.0.0.2, gateway 192.168.1.1"
                    .to_string(),
                "Ethernet 3 (unplugged or disabled): IPv4 DNS 192.168.1.1 (set manually)"
                    .to_string(),
                "Intel(R) Ethernet Connection I219-V (unplugged or disabled): IPv6 DNS \
                 2620:fe::fe, 2620:fe::9 (set manually)"
                    .to_string(),
                format!(
                    "{} (unplugged or disabled): static IPv4 172.16.0.9",
                    guid(10)
                ),
            ]
        );
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

        // The run audits them before the first step, like the listed adapters' settings.
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "reset", false);
        let report =
            reset_with(Some(&safety), &stack, &FakeRunner::default(), &system_dir()).unwrap();
        assert_eq!(report.manual_settings, plan.manual_settings);
        let audit = reset_rows(&journal)[0].2.clone().unwrap();
        assert!(
            audit.contains(
                "Ethernet 3 (unplugged or disabled): static IPv4 192.168.1.10/24, gateway \
                 192.168.1.1"
            ),
            "{audit}"
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn unreadable_stored_settings_are_a_warning_not_a_refusal() {
        let stack = FakeStack::with(vec![adapter(1, "Wi-Fi", AdapterKind::Wifi)]);
        *stack.fail_stored.borrow_mut() = Some("Access is denied.".into());
        let warning = format!("{UNLISTED_WARNING}: Access is denied.");

        let plan = reset_with(None, &stack, &FakeRunner::default(), &system_dir()).unwrap();
        assert!(plan.manual_settings.is_empty());
        assert_eq!(plan.warnings, vec![warning.clone()]);

        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "reset", false);
        let runner = FakeRunner::default();
        let report = reset_with(Some(&safety), &stack, &runner, &system_dir()).unwrap();
        assert_eq!(runner.calls.borrow().len(), 3, "the reset still runs");
        assert_eq!(report.warnings, vec![warning.clone()]);
        assert_eq!(
            reset_rows(&journal)[0].2.as_deref(),
            Some(format!("none ({warning})").as_str())
        );
    }

    #[test]
    fn subnet_masks_give_prefix_lengths() {
        assert_eq!(mask_prefix_length("255.255.255.0"), Some(24));
        assert_eq!(mask_prefix_length(" 255.255.252.0 "), Some(22));
        assert_eq!(mask_prefix_length("255.255.255.255"), Some(32));
        assert_eq!(mask_prefix_length("0.0.0.0"), Some(0));
        assert_eq!(mask_prefix_length("255.0.255.0"), None, "not contiguous");
        assert_eq!(mask_prefix_length("24"), None);
    }

    #[test]
    fn flush_logs_op() {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "flush", true);
        let stack = stack();
        let report = flush_with(&safety, &stack).unwrap();
        assert_eq!(report.session_id, safety.session_id());
        assert_eq!(stack.calls(), vec!["flush"]);
        let ops = journal.ops(10).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].op, OP_FLUSH);
        assert_eq!(ops[0].target, FLUSH_TARGET);
        assert_eq!(ops[0].outcome, "flushed");
        assert_eq!(ops[0].session_id, Some(safety.session_id()));
    }

    #[test]
    fn flush_error_propagates_after_logging() {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "flush", false);
        let mut stack = stack();
        stack.fail_flush = Some("the DNS Client service did not flush its cache".into());
        let err = flush_with(&safety, &stack).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the DNS Client service did not flush its cache"
        );
        let ops = journal.ops(10).unwrap();
        assert_eq!(ops[0].outcome, "failed");
        assert_eq!(
            ops[0].detail.as_deref(),
            Some("the DNS Client service did not flush its cache")
        );
    }

    #[test]
    fn renew_releases_first_only_when_asked() {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "renew", false);
        let stack = stack();
        let report = renew_with(&safety, &stack, &guid(1), false).unwrap();
        assert_eq!(stack.calls(), vec!["renew 1"]);
        assert!(!report.released);
        assert!(report.renewed);
        assert_eq!(report.adapter_name, "Wi-Fi");
        assert_eq!(report.adapter_id, guid(1));
        assert_eq!(report.ipv4, vec!["192.168.0.11"]);
        assert_eq!(report.session_id, safety.session_id());

        let stack = self::stack();
        let report = renew_with(&safety, &stack, "wi-fi", true).unwrap();
        assert_eq!(stack.calls(), vec!["release 1", "renew 1"]);
        assert!(report.released);
        let mut rows: Vec<(String, String, String)> = journal
            .ops(10)
            .unwrap()
            .into_iter()
            .map(|o| (o.op, o.target, o.outcome))
            .collect();
        rows.reverse();
        let lease = "IPv4 lease of Wi-Fi".to_string();
        assert_eq!(
            rows,
            vec![
                (OP_RENEW.to_string(), lease.clone(), "renewed".to_string()),
                (
                    OP_RELEASE.to_string(),
                    lease.clone(),
                    "released".to_string()
                ),
                (OP_RENEW.to_string(), lease, "renewed".to_string()),
            ]
        );

        let mut failing = self::stack();
        failing.fail_renew = Some("Unable to contact your DHCP server.".into());
        let err = renew_with(&safety, &failing, &guid(1), true).unwrap_err();
        assert_eq!(err.to_string(), "Unable to contact your DHCP server.");
        assert_eq!(failing.calls(), vec!["release 1", "renew 1"]);
        let last = &journal.ops(1).unwrap()[0];
        assert_eq!(
            (last.op.as_str(), last.outcome.as_str()),
            (OP_RENEW, "failed")
        );
    }

    #[test]
    fn renew_refuses_non_dhcp_adapter() {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "renew", false);
        let stack = stack();
        let err = renew_with(&safety, &stack, &guid(2), false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "this adapter does not get its IPv4 address from DHCP"
        );
        let mut idle = adapter(5, "Ethernet 2", AdapterKind::Ethernet);
        idle.status = LinkStatus::Disconnected;
        stack.adapters.borrow_mut().push(idle);
        let err = renew_with(&safety, &stack, "Ethernet 2", false).unwrap_err();
        assert_eq!(err.to_string(), "this adapter is not connected");
        stack
            .adapters
            .borrow_mut()
            .push(adapter(6, "ProtonVPN", AdapterKind::Vpn));
        let err = renew_with(&safety, &stack, "6", false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the lease of this type of adapter cannot be renewed here"
        );
        assert!(stack.calls().is_empty());
        assert!(journal.ops(10).unwrap().is_empty());
    }

    #[test]
    fn unelevated_renew_and_reset_are_refused_before_anything() {
        if crate::is_elevated() {
            return;
        }
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "network", true);
        let stack = stack();
        let runner = FakeRunner::default();
        assert!(matches!(
            renew_with(&safety, &stack, &guid(1), true),
            Err(Error::NotElevated)
        ));
        assert!(matches!(
            reset_with(Some(&safety), &stack, &runner, &system_dir()),
            Err(Error::NotElevated)
        ));
        assert!(stack.calls().is_empty());
        assert!(runner.calls.borrow().is_empty());
        assert!(journal.ops(10).unwrap().is_empty());
        flush_with(&safety, &stack).unwrap();
    }
}
