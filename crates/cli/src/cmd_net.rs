//! `optctl net`: network adapters, DNS servers, the DNS cache, DHCP leases and the network
//! stack.

use std::sync::Arc;

use anyhow::{bail, Context};
use clap::{ArgGroup, Args, Subcommand, ValueEnum};
use optimizer_core::network::{
    self, Adapter, AddressInfo, DnsConfig, DnsMode, DnsReport, DnsRequest, IpFamily, LinkStatus,
    NetworkReport, ResetReport,
};
use optimizer_core::safety::state_log::Journal;
use optimizer_core::safety::{RestorePointPolicy, Safety, SafetyOptions};

#[derive(Subcommand, Debug)]
pub(crate) enum NetCmd {
    /// List the network adapters with their addresses and DNS servers. Read-only.
    List {
        /// Include idle virtual adapters.
        #[arg(long)]
        all: bool,
        /// Print the adapters as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the DNS presets. Read-only.
    Presets {
        /// Print the presets as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Change an adapter's DNS servers to a preset or to custom servers. Journaled: the
    /// change reverts with `optctl revert --all` or `optctl rollback`.
    Dns(DnsArgs),
    /// Clear the DNS resolver cache. Logged, not journaled.
    FlushDns,
    /// Renew an adapter's IPv4 DHCP lease. Logged, not journaled.
    Renew(RenewArgs),
    /// Reset the Winsock catalog and TCP/IP stacks. Irreversible; needs a restart.
    Reset(ResetArgs),
}

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("servers").required(true).multiple(true).args(["preset", "ipv4", "ipv6"])))]
pub(crate) struct DnsArgs {
    /// Adapter id (interface GUID), name or interface index.
    adapter: String,
    /// Preset id, e.g. `cloudflare` or `automatic` (see `optctl net presets`).
    #[arg(long, conflicts_with_all = ["ipv4", "ipv6"])]
    preset: Option<String>,
    /// Custom IPv4 servers, comma-separated. Omit to keep the current IPv4 setting.
    #[arg(long, value_name = "A,B")]
    ipv4: Option<String>,
    /// Custom IPv6 servers, comma-separated. Omit to keep the current IPv6 setting.
    #[arg(long, value_name = "A,B")]
    ipv6: Option<String>,
    /// Print the plan without changing anything.
    #[arg(long)]
    dry_run: bool,
    /// Required to change the servers.
    #[arg(short = 'y', long)]
    yes: bool,
    /// System Restore point before the change.
    #[arg(long, value_enum, default_value_t = RestorePointChoice::Skip)]
    restore_point: RestorePointChoice,
}

#[derive(Args, Debug)]
pub(crate) struct RenewArgs {
    /// Adapter id (interface GUID), name or interface index.
    adapter: String,
    /// Release the lease before renewing it.
    #[arg(long)]
    release: bool,
    /// Required to renew the lease.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Args, Debug)]
pub(crate) struct ResetArgs {
    /// Print the plan without changing anything.
    #[arg(long)]
    dry_run: bool,
    /// Required to reset the stack.
    #[arg(short = 'y', long)]
    yes: bool,
    /// System Restore point before the reset.
    #[arg(long, value_enum, default_value_t = RestorePointChoice::Try)]
    restore_point: RestorePointChoice,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub(crate) enum RestorePointChoice {
    /// Do not create a restore point.
    Skip,
    /// Create one; continue with the journal alone if that fails.
    Try,
    /// Create one; abort if that fails.
    Require,
}

impl From<RestorePointChoice> for RestorePointPolicy {
    fn from(choice: RestorePointChoice) -> Self {
        match choice {
            RestorePointChoice::Skip => RestorePointPolicy::Skip,
            RestorePointChoice::Try => RestorePointPolicy::Try,
            RestorePointChoice::Require => RestorePointPolicy::Require,
        }
    }
}

pub(crate) fn run(
    cmd: NetCmd,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    match cmd {
        NetCmd::List { all, json } => {
            let journal = open_journal().ok();
            let mut report = network::list(journal.as_ref())?;
            if !all {
                report.adapters.retain(|a| !a.minor);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_list(&report);
            }
        }
        NetCmd::Presets { json } => {
            if json {
                println!("{}", serde_json::to_string_pretty(network::PRESETS)?);
            } else {
                for preset in network::PRESETS {
                    let servers = |list: &[&str]| {
                        if list.is_empty() {
                            "automatic".to_string()
                        } else {
                            list.join(", ")
                        }
                    };
                    println!("{:<20} {}", preset.id, preset.title);
                    println!("{:<20} IPv4 {}", "", servers(preset.ipv4));
                    println!("{:<20} IPv6 {}", "", servers(preset.ipv6));
                }
            }
        }
        NetCmd::Dns(args) => run_dns(args, open_journal)?,
        NetCmd::FlushDns => {
            let safety = session(
                open_journal,
                "network: flush DNS cache",
                RestorePointPolicy::Skip,
                false,
            )?;
            let report = network::flush_dns_cache(&safety)?;
            println!(
                "DNS resolver cache flushed (session #{})",
                report.session_id
            );
        }
        NetCmd::Renew(args) => {
            let listed = network::list(None)?;
            let adapter = network::find_adapter(&listed.adapters, &args.adapter)?;
            if !args.yes {
                println!(
                    "would {}renew the IPv4 lease of {} ({})",
                    if args.release { "release and " } else { "" },
                    adapter.name,
                    adapter.id
                );
                bail!("lease renewal planned; re-run with --yes");
            }
            let safety = session(
                open_journal,
                "network: renew lease",
                RestorePointPolicy::Skip,
                true,
            )?;
            let report = network::renew_lease(&safety, &adapter.id, args.release)?;
            println!(
                "{} renewed its lease: {}",
                report.adapter_name,
                if report.ipv4.is_empty() {
                    "no IPv4 address yet".to_string()
                } else {
                    report.ipv4.join(", ")
                }
            );
        }
        NetCmd::Reset(args) => {
            if args.dry_run || !args.yes {
                let plan = network::plan_reset()?;
                print_reset(&plan);
                if args.dry_run {
                    return Ok(());
                }
                bail!("network reset planned; re-run with --yes");
            }
            let safety = session(
                open_journal,
                "network: reset",
                args.restore_point.into(),
                true,
            )?;
            let report = network::reset_stack(&safety)?;
            print_reset(&report);
        }
    }
    Ok(())
}

fn run_dns(
    args: DnsArgs,
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
) -> anyhow::Result<()> {
    let request = match &args.preset {
        Some(preset) => DnsRequest::preset(preset)?,
        None => {
            let list = |value: &Option<String>| value.iter().cloned().collect::<Vec<String>>();
            DnsRequest::custom(&list(&args.ipv4), &list(&args.ipv6))?
        }
    };
    let listed = network::list(None)?;
    let adapter = network::find_adapter(&listed.adapters, &args.adapter)?;
    if args.dry_run || !args.yes {
        let plan = network::plan_dns(&adapter.id, &request)?;
        print_dns(&plan);
        if args.dry_run {
            return Ok(());
        }
        bail!("DNS change planned; re-run with --yes");
    }
    let safety = session(
        open_journal,
        &format!("dns: {}", adapter.id),
        args.restore_point.into(),
        true,
    )?;
    let report = network::set_dns(&safety, &adapter.id, &request)?;
    print_dns(&report);
    Ok(())
}

fn session(
    open_journal: &dyn Fn() -> anyhow::Result<Journal>,
    label: &str,
    restore_point: RestorePointPolicy,
    require_elevation: bool,
) -> anyhow::Result<Safety> {
    let journal = Arc::new(open_journal()?);
    Safety::begin(
        journal,
        SafetyOptions {
            label: label.to_string(),
            restore_point,
            require_elevation,
            ..Default::default()
        },
    )
    .with_context(|| format!("start the journal session \"{label}\""))
}

/// The serialized name of an enum value, such as `link_local` or `completed_with_errors`.
fn snake(value: serde_json::Result<serde_json::Value>) -> String {
    value
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn speed_text(bps: Option<u64>) -> Option<String> {
    let bps = bps?;
    Some(if bps >= 1_000_000_000 {
        format!("{:.1} Gbps", bps as f64 / 1e9)
    } else {
        format!("{} Mbps", bps / 1_000_000)
    })
}

fn addresses_text(addresses: &[AddressInfo]) -> String {
    addresses
        .iter()
        .map(|a| {
            format!(
                "{}/{} ({})",
                a.address,
                a.prefix_length,
                snake(serde_json::to_value(a.origin))
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn dns_text(config: &DnsConfig) -> String {
    let servers = if config.servers.is_empty() {
        "none".to_string()
    } else {
        config.servers.join(", ")
    };
    match config.mode {
        DnsMode::Automatic if config.servers.is_empty() => "automatic".to_string(),
        DnsMode::Automatic => format!("automatic ({servers})"),
        DnsMode::Manual => match &config.preset {
            Some(preset) => format!("{servers} [{preset}]"),
            None => servers,
        },
        DnsMode::Profile => format!("set for this Wi-Fi network: {servers}"),
        DnsMode::Unknown => "could not be read".to_string(),
    }
}

fn print_list(report: &NetworkReport) {
    if !report.dns_policy.is_empty() {
        println!(
            "DNS servers set by policy: {} (adapter DNS settings have no effect)",
            report.dns_policy.join(", ")
        );
    }
    if report.vpn_connected {
        println!("A VPN is connected: DNS changes are paused until it disconnects.");
    }
    for adapter in &report.adapters {
        print_adapter(adapter);
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    println!(
        "{} adapter(s) read in {} ms",
        report.adapters.len(),
        report.duration_ms
    );
}

fn print_adapter(a: &Adapter) {
    println!();
    for line in adapter_lines(a) {
        println!("{line}");
    }
}

/// The lines `optctl net list` prints for one adapter. Only addresses in use (`preferred`)
/// are listed, and gateways only while the adapter is connected: a disconnected adapter
/// keeps tentative addresses and the gateway of an old lease that it cannot use.
fn adapter_lines(a: &Adapter) -> Vec<String> {
    let mut tags = vec![
        snake(serde_json::to_value(a.kind)),
        snake(serde_json::to_value(a.status)),
    ];
    if a.primary {
        tags.push("primary".into());
    }
    if a.limited {
        tags.push("limited".into());
    }
    if a.dns_revertible {
        tags.push("DNS change recorded".into());
    }
    let mut lines = vec![format!(
        "{}  [{}]  #{}  {}",
        a.name,
        tags.join(", "),
        a.if_index,
        a.id
    )];
    let mut line = vec![a.description.clone()];
    line.extend(speed_text(a.receive_bps));
    if !a.mac.is_empty() {
        line.push(a.mac.clone());
    }
    lines.push(format!("  {}", line.join("  ·  ")));
    for (label, addresses) in [("IPv4", &a.ipv4), ("IPv6", &a.ipv6)] {
        let in_use: Vec<AddressInfo> = addresses.iter().filter(|x| x.preferred).cloned().collect();
        if !in_use.is_empty() {
            lines.push(format!("  {label}      {}", addresses_text(&in_use)));
        }
    }
    if a.status == LinkStatus::Connected && !a.gateways.is_empty() {
        lines.push(format!("  gateway   {}", a.gateways.join(", ")));
    }
    for family in IpFamily::ALL {
        if a.family_enabled(family) {
            lines.push(format!(
                "  {} DNS  {}",
                family.label(),
                dns_text(a.dns(family))
            ));
        }
    }
    if let Some(note) = &a.note {
        lines.push(format!("  note      {note}"));
    }
    lines
}

fn print_dns(report: &DnsReport) {
    println!(
        "{}DNS servers of {} ({})",
        if report.dry_run { "planned: " } else { "" },
        report.adapter_name,
        report.adapter_id
    );
    for change in &report.changes {
        let outcome = snake(serde_json::to_value(change.outcome));
        println!(
            "  {:<5} {} -> {}  [{}]{}",
            change.family.label(),
            dns_text(&change.previous),
            dns_text(&change.target),
            outcome,
            change
                .detail
                .as_deref()
                .map(|d| format!(" {d}"))
                .unwrap_or_default()
        );
    }
    if let Some(session) = report.session_id {
        println!("  journal session #{session}");
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
}

fn print_reset(report: &ResetReport) {
    println!(
        "{}network stack reset",
        if report.dry_run { "planned: " } else { "" }
    );
    for step in &report.steps {
        let status = snake(serde_json::to_value(step.status));
        let code = step
            .exit_code
            .map(|c| format!(" (exit {c})"))
            .unwrap_or_default();
        println!("  {:<26} {:<22} {status}{code}", step.title, step.command);
        for line in step.output.lines().filter(|l| !l.trim().is_empty()) {
            println!("      {line}");
        }
    }
    if report.manual_settings.is_empty() {
        println!("  no manual IP addresses or DNS servers were found");
    } else {
        println!("  manual settings that are removed:");
        for setting in &report.manual_settings {
            println!("    {}: {}", setting.adapter, setting.detail);
        }
    }
    if let Some(point) = &report.restore_point {
        println!("  restore point #{} created", point.sequence);
    }
    if report.dry_run {
        println!("  Windows must restart afterwards; the reset cannot be undone");
    } else if report.restart_required {
        println!("  restart Windows to finish the reset");
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
}
