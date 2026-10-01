//! Live network tests. Read-only: every call only queries the adapters, their DNS settings
//! and the registry, or plans a change without making it. Nothing here sets DNS servers,
//! restores a record, flushes the resolver cache, renews a lease or resets the stack.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

use optimizer_core::network::{
    self, canonical_guid, split_servers, AdapterKind, ChangeOutcome, DnsMode, DnsRequest, IpFamily,
    StepStatus,
};
use optimizer_core::safety::state_log::DnsRecord;
use optimizer_core::win::registry::{read_value, Hive, RegValue};

/// Interface GUID that no adapter has.
const UNKNOWN_INTERFACE: &str = "{00000000-0000-0000-0000-00000000c0de}";

fn is_canonical_guid(id: &str) -> bool {
    let Some(inner) = id.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return false;
    };
    let groups: Vec<&str> = inner.split('-').collect();
    groups.iter().map(|g| g.len()).collect::<Vec<_>>() == [8, 4, 4, 4, 12]
        && inner
            .chars()
            .all(|c| c == '-' || c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn is_placeholder(server: &str) -> bool {
    server.parse::<IpAddr>().is_ok_and(
        |ip| matches!(ip, IpAddr::V6(v6) if v6.segments()[..4] == [0xfec0, 0, 0, 0xffff]),
    )
}

/// `NameServer` of the interface's TCP/IP key; `""` when it is missing.
fn registry_name_servers(id: &str, family: IpFamily) -> String {
    let service = match family {
        IpFamily::Ipv4 => "Tcpip",
        IpFamily::Ipv6 => "Tcpip6",
    };
    let key = format!(r"SYSTEM\CurrentControlSet\Services\{service}\Parameters\Interfaces\{id}");
    match read_value(Hive::LocalMachine, &key, "NameServer").unwrap() {
        Some(RegValue::Sz(s)) | Some(RegValue::ExpandSz(s)) => s,
        _ => String::new(),
    }
}

#[test]
fn list_is_read_only_and_well_formed() {
    let first = network::list(None).unwrap();
    let mut ids = HashSet::new();
    for adapter in &first.adapters {
        assert!(is_canonical_guid(&adapter.id), "{}", adapter.id);
        assert_eq!(canonical_guid(&adapter.id), adapter.id);
        assert!(ids.insert(adapter.id.clone()), "duplicate {}", adapter.id);
        assert!(
            !adapter
                .name
                .to_ascii_lowercase()
                .starts_with("loopback pseudo-interface"),
            "{}",
            adapter.name
        );
        for server in adapter
            .dns_servers
            .iter()
            .chain(&adapter.dns_ipv4.servers)
            .chain(&adapter.dns_ipv6.servers)
        {
            assert!(!is_placeholder(server), "{} lists {server}", adapter.name);
        }
        if adapter.can_change_dns {
            assert!(!matches!(
                adapter.kind,
                AdapterKind::Vpn | AdapterKind::Tunnel
            ));
            assert!(!first.vpn_connected);
        } else {
            assert!(adapter.note.is_some(), "{} has no reason", adapter.name);
        }
        assert_eq!(
            adapter.minor,
            !adapter.hardware && adapter.status != network::LinkStatus::Connected
        );
    }
    assert!(first.adapters.iter().filter(|a| a.primary).count() <= 1);

    let second = network::list(None).unwrap();
    let configs: HashMap<&str, (&network::DnsConfig, &network::DnsConfig)> = first
        .adapters
        .iter()
        .map(|a| (a.id.as_str(), (&a.dns_ipv4, &a.dns_ipv6)))
        .collect();
    for adapter in &second.adapters {
        if let Some((v4, v6)) = configs.get(adapter.id.as_str()) {
            assert_eq!(adapter.dns_ipv4.mode, v4.mode, "{}", adapter.name);
            assert_eq!(adapter.dns_ipv6.mode, v6.mode, "{}", adapter.name);
            if adapter.dns_ipv4.mode == DnsMode::Manual {
                assert_eq!(&adapter.dns_ipv4, *v4, "{}", adapter.name);
            }
            if adapter.dns_ipv6.mode == DnsMode::Manual {
                assert_eq!(&adapter.dns_ipv6, *v6, "{}", adapter.name);
            }
        }
    }
    let json = serde_json::to_value(&first).unwrap();
    assert!(json["adapters"].is_array());
}

#[test]
fn static_dns_matches_registry() {
    let report = network::list(None).unwrap();
    for adapter in &report.adapters {
        for family in IpFamily::ALL {
            let from_api = network::static_dns(&adapter.id, family).unwrap();
            let from_registry = registry_name_servers(&adapter.id, family);
            assert_eq!(
                split_servers(&from_api),
                split_servers(&from_registry),
                "{} {family:?}",
                adapter.name
            );
            let config = adapter.dns(family);
            if matches!(adapter.kind, AdapterKind::Ethernet | AdapterKind::Bluetooth) {
                assert!(config.profile_servers.is_empty(), "{}", adapter.name);
            }
            if config.mode == DnsMode::Manual {
                assert_eq!(config.servers, split_servers(&from_api), "{}", adapter.name);
            }
        }
    }
    assert_eq!(
        network::static_dns(UNKNOWN_INTERFACE, IpFamily::Ipv4).unwrap(),
        ""
    );
}

#[test]
fn dns_plan_is_read_only() {
    let report = network::list(None).unwrap();
    let Some(adapter) = report.adapters.iter().find(|a| a.can_change_dns) else {
        eprintln!("no adapter whose DNS servers can be changed; nothing to plan");
        return;
    };
    let before: Vec<String> = IpFamily::ALL
        .iter()
        .map(|f| network::static_dns(&adapter.id, *f).unwrap())
        .collect();
    let plan = network::plan_dns(&adapter.id, &DnsRequest::preset("cloudflare").unwrap()).unwrap();
    assert!(plan.dry_run);
    assert_eq!(plan.session_id, None);
    assert_eq!(plan.adapter_id, adapter.id);
    assert!(!plan.changes.is_empty());
    for change in &plan.changes {
        assert!(
            matches!(
                change.outcome,
                ChangeOutcome::Planned | ChangeOutcome::AlreadySet | ChangeOutcome::Skipped
            ),
            "{change:?}"
        );
    }
    let after: Vec<String> = IpFamily::ALL
        .iter()
        .map(|f| network::static_dns(&adapter.id, *f).unwrap())
        .collect();
    assert_eq!(before, after, "planning changed nothing");
}

#[test]
fn drift_of_unknown_interface_is_none() {
    let rec = DnsRecord {
        id: 1,
        session_id: 1,
        recorded_at: "2026-09-25T10:00:00+00:00".into(),
        interface_guid: UNKNOWN_INTERFACE.into(),
        family: IpFamily::Ipv4,
        adapter_name: "Cairn self-test".into(),
        previous_servers: String::new(),
        target_servers: "1.1.1.1".into(),
        active: true,
        reverted_at: None,
    };
    assert_eq!(network::dns_drift(&rec).unwrap(), None);
}

#[test]
fn reset_plan_runs_nothing() {
    let plan = network::plan_reset().unwrap();
    assert!(plan.dry_run);
    assert_eq!(plan.session_id, None);
    assert!(plan.restore_point.is_none());
    assert!(plan.restart_required);
    let ids: Vec<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["winsock", "ipv4", "ipv6"]);
    assert!(plan.steps.iter().all(|s| s.status == StepStatus::Planned));
    assert!(plan
        .steps
        .iter()
        .all(|s| s.exit_code.is_none() && s.output.is_empty()));
    // The settings kept for adapters that are not listed are read from the registry too.
    assert!(
        !plan
            .warnings
            .iter()
            .any(|w| w.starts_with("cannot read the settings of unplugged or disabled adapters")),
        "{:?}",
        plan.warnings
    );
    for setting in &plan.manual_settings {
        assert!(!setting.adapter.trim().is_empty(), "{setting:?}");
        assert!(
            setting.detail.starts_with("static IPv") || setting.detail.ends_with("(set manually)"),
            "{setting:?}"
        );
    }
}

#[test]
fn disconnected_adapters_list_no_automatic_servers_in_use() {
    let report = network::list(None).unwrap();
    for adapter in report
        .adapters
        .iter()
        .filter(|a| a.status != network::LinkStatus::Connected)
    {
        assert!(adapter.dns_servers.is_empty(), "{}", adapter.name);
        for family in IpFamily::ALL {
            let config = adapter.dns(family);
            if config.mode == DnsMode::Automatic {
                assert!(config.servers.is_empty(), "{} {family:?}", adapter.name);
            }
        }
    }
}
