//! DNS server changes: the preset table, request validation, the plan and apply algorithm
//! with its baseline-first journaling, and restoring a recorded baseline.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use super::adapters::{list_with, Adapter, DnsConfig, DnsMode};
use super::{
    canonical_guid, dns_target, servers_text, split_servers, DnsRestore, IpFamily, NetStack,
    FLUSH_TARGET, OP_FLUSH,
};
use crate::safety::state_log::{DnsRecord, NewDnsRecord};
use crate::safety::Safety;
use crate::{Error, Result};

/// Audit log operation of a DNS server change.
pub(crate) const OP_SET_DNS: &str = "set_dns_servers";
/// Most servers accepted per address family.
pub const MAX_SERVERS: usize = 4;
/// Warning of a change whose resolver cache flush failed.
pub(crate) const FLUSH_WARNING: &str =
    "The DNS cache could not be flushed; old lookups may be used for a few minutes.";

/// A named set of public DNS servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DnsPreset {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// Empty for [`DnsChoice::Automatic`].
    pub ipv4: &'static [&'static str],
    pub ipv6: &'static [&'static str],
}

/// The DNS presets, in menu order.
pub const PRESETS: &[DnsPreset] = &[
    DnsPreset {
        id: "automatic",
        title: "Automatic (DHCP)",
        description: "Uses the DNS servers the network hands out, usually your router's.",
        ipv4: &[],
        ipv6: &[],
    },
    DnsPreset {
        id: "cloudflare",
        title: "Cloudflare",
        description: "Cloudflare's fast public DNS, without filtering.",
        ipv4: &["1.1.1.1", "1.0.0.1"],
        ipv6: &["2606:4700:4700::1111", "2606:4700:4700::1001"],
    },
    DnsPreset {
        id: "cloudflare_security",
        title: "Cloudflare (blocks malware)",
        description: "Cloudflare DNS that blocks known malware sites.",
        ipv4: &["1.1.1.2", "1.0.0.2"],
        ipv6: &["2606:4700:4700::1112", "2606:4700:4700::1002"],
    },
    DnsPreset {
        id: "cloudflare_family",
        title: "Cloudflare (blocks malware and adult content)",
        description: "Cloudflare DNS that blocks malware and adult content.",
        ipv4: &["1.1.1.3", "1.0.0.3"],
        ipv6: &["2606:4700:4700::1113", "2606:4700:4700::1003"],
    },
    DnsPreset {
        id: "google",
        title: "Google Public DNS",
        description: "Google's public DNS, without filtering.",
        ipv4: &["8.8.8.8", "8.8.4.4"],
        ipv6: &["2001:4860:4860::8888", "2001:4860:4860::8844"],
    },
    DnsPreset {
        id: "quad9",
        title: "Quad9 (blocks malware)",
        description: "Quad9 DNS that blocks domains known to be malicious.",
        ipv4: &["9.9.9.9", "149.112.112.112"],
        ipv6: &["2620:fe::fe", "2620:fe::9"],
    },
    DnsPreset {
        id: "quad9_unfiltered",
        title: "Quad9 (no filtering)",
        description: "Quad9 DNS without any blocking.",
        ipv4: &["9.9.9.10", "149.112.112.10"],
        ipv6: &["2620:fe::10", "2620:fe::fe:10"],
    },
];

impl DnsPreset {
    /// Servers of `family`.
    pub fn servers(&self, family: IpFamily) -> &'static [&'static str] {
        match family {
            IpFamily::Ipv4 => self.ipv4,
            IpFamily::Ipv6 => self.ipv6,
        }
    }
}

/// What to do with one address family's DNS servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsChoice {
    /// Leave the family as it is.
    Unchanged,
    /// Obtain the servers automatically (DHCP or router advertisements).
    Automatic,
    /// Use these servers, in this order.
    Servers(Vec<IpAddr>),
}

/// A DNS change of one adapter, per address family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsRequest {
    pub ipv4: DnsChoice,
    pub ipv6: DnsChoice,
}

impl DnsRequest {
    /// Both families of preset `id` (ASCII case ignored).
    pub fn preset(id: &str) -> Result<DnsRequest> {
        let wanted = id.trim();
        let preset = PRESETS
            .iter()
            .find(|p| p.id.eq_ignore_ascii_case(wanted))
            .ok_or_else(|| {
                let valid: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
                Error::Other(format!(
                    "unknown DNS preset {wanted:?}; valid presets: {}",
                    valid.join(", ")
                ))
            })?;
        let choice = |family: IpFamily| -> Result<DnsChoice> {
            let servers: Vec<String> = preset
                .servers(family)
                .iter()
                .map(|s| s.to_string())
                .collect();
            if servers.is_empty() {
                Ok(DnsChoice::Automatic)
            } else {
                Ok(DnsChoice::Servers(parse_servers(&servers, family)?))
            }
        };
        Ok(DnsRequest {
            ipv4: choice(IpFamily::Ipv4)?,
            ipv6: choice(IpFamily::Ipv6)?,
        })
    }

    /// Servers typed by the user. An empty list leaves that family unchanged; both empty is
    /// an error.
    pub fn custom(ipv4: &[String], ipv6: &[String]) -> Result<DnsRequest> {
        let choice = |items: &[String], family: IpFamily| -> Result<DnsChoice> {
            let servers = parse_servers(items, family)?;
            Ok(if servers.is_empty() {
                DnsChoice::Unchanged
            } else {
                DnsChoice::Servers(servers)
            })
        };
        let request = DnsRequest {
            ipv4: choice(ipv4, IpFamily::Ipv4)?,
            ipv6: choice(ipv6, IpFamily::Ipv6)?,
        };
        if request.ipv4 == DnsChoice::Unchanged && request.ipv6 == DnsChoice::Unchanged {
            return Err(Error::Other(
                "custom DNS needs at least one IPv4 or IPv6 server".into(),
            ));
        }
        Ok(request)
    }

    /// The choice for `family`.
    pub fn choice(&self, family: IpFamily) -> &DnsChoice {
        match family {
            IpFamily::Ipv4 => &self.ipv4,
            IpFamily::Ipv6 => &self.ipv6,
        }
    }
}

/// Parses DNS servers of `family`. Each item may hold several addresses separated by commas
/// or spaces. Unspecified, multicast, broadcast, link-local and site-local addresses are
/// refused; loopback is allowed. Duplicates are dropped, keeping the first. At most
/// [`MAX_SERVERS`].
pub fn parse_servers(items: &[String], family: IpFamily) -> Result<Vec<IpAddr>> {
    let mut servers: Vec<IpAddr> = Vec::new();
    for item in items {
        for text in split_servers(item) {
            let addr = text
                .parse::<IpAddr>()
                .ok()
                .filter(|a| IpFamily::of(a) == family)
                .ok_or_else(|| {
                    Error::Other(format!("{text} is not a valid {} address", family.label()))
                })?;
            let unusable = addr.is_unspecified()
                || addr.is_multicast()
                || match addr {
                    IpAddr::V4(v4) => v4.is_broadcast(),
                    IpAddr::V6(v6) => matches!(v6.segments()[0] & 0xffc0, 0xfe80 | 0xfec0),
                };
            if unusable {
                return Err(Error::Other(format!(
                    "{text} cannot be used as a DNS server"
                )));
            }
            if !servers.contains(&addr) {
                servers.push(addr);
            }
        }
    }
    if servers.len() > MAX_SERVERS {
        return Err(Error::Other(format!(
            "at most {MAX_SERVERS} {} DNS servers",
            family.label()
        )));
    }
    Ok(servers)
}

/// Servers as Windows stores them: comma-separated, without spaces.
pub(crate) fn servers_string(servers: &[IpAddr]) -> String {
    servers
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Normalized form of one server for comparisons: the address's canonical text, or the
/// text itself when it is not an address.
fn server_key(s: &str) -> String {
    s.parse::<IpAddr>()
        .map_or_else(|_| s.to_ascii_lowercase(), |ip| ip.to_string())
}

/// Whether two server lists name the same servers in the same order.
pub fn same_servers(a: &str, b: &str) -> bool {
    let keys =
        |s: &str| -> Vec<String> { split_servers(s).iter().map(|x| server_key(x)).collect() };
    keys(a) == keys(b)
}

/// Id of the preset whose `family` servers are exactly `servers`, in order.
pub(crate) fn preset_matching(servers: &[String], family: IpFamily) -> Option<String> {
    if servers.is_empty() {
        return None;
    }
    let wanted = servers.join(",");
    PRESETS
        .iter()
        .find(|p| {
            !p.servers(family).is_empty() && same_servers(&p.servers(family).join(","), &wanted)
        })
        .map(|p| p.id.to_string())
}

/// What happened to one address family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOutcome {
    /// Dry run: the change would be made.
    Planned,
    Applied,
    /// The adapter already uses these servers; nothing was recorded or written.
    AlreadySet,
    /// Not changed, for the reason in `detail`.
    Skipped,
    /// The change failed; see `detail`. A recorded baseline stays in the journal.
    Failed,
}

/// The change of one address family.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsChange {
    pub family: IpFamily,
    pub previous: DnsConfig,
    pub target: DnsConfig,
    pub outcome: ChangeOutcome,
    pub detail: Option<String>,
}

/// Result of planning or applying a DNS change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsReport {
    pub dry_run: bool,
    pub adapter_id: String,
    pub adapter_name: String,
    /// Journal session of the change; `None` in a dry run.
    pub session_id: Option<i64>,
    pub changes: Vec<DnsChange>,
    pub warnings: Vec<String>,
}

/// Whether [`run`] only plans or changes the adapter.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Mode<'a> {
    Plan,
    Apply(&'a Safety),
}

/// Configuration an adapter gets for `family` from `servers` (empty: automatic).
fn target_config(servers: &[IpAddr], family: IpFamily) -> DnsConfig {
    let servers: Vec<String> = servers.iter().map(ToString::to_string).collect();
    DnsConfig {
        mode: if servers.is_empty() {
            DnsMode::Automatic
        } else {
            DnsMode::Manual
        },
        preset: preset_matching(&servers, family),
        servers,
        profile_servers: Vec::new(),
    }
}

/// Why `family` of `adapter` is left alone, if it is.
fn skip_reason(adapter: &Adapter, family: IpFamily, automatic: bool) -> Option<&'static str> {
    match family {
        IpFamily::Ipv6 if !adapter.ipv6_enabled => Some("IPv6 is turned off on this adapter"),
        IpFamily::Ipv4 if !adapter.ipv4_enabled => Some("IPv4 is turned off on this adapter"),
        IpFamily::Ipv4 if automatic && !adapter.dhcp_enabled => Some(
            "this adapter has a manually set IPv4 address, so it would get no IPv4 DNS servers \
             automatically",
        ),
        _ => None,
    }
}

/// Plans `request` when `dry_run` is set, else opens a session with `begin` and applies it.
/// A plan never calls `begin`, so it opens no journal session and needs no elevation.
pub(crate) fn plan_or_apply(
    stack: &dyn NetStack,
    dry_run: bool,
    begin: impl FnOnce() -> Result<Safety>,
    adapter_id: &str,
    request: &DnsRequest,
) -> Result<DnsReport> {
    if dry_run {
        return run(stack, Mode::Plan, adapter_id, request);
    }
    let safety = begin()?;
    run(stack, Mode::Apply(&safety), adapter_id, request)
}

/// Plans or applies `request` on the adapter `adapter_id` (an interface GUID, braces
/// optional). Applying records each family's current static servers before writing them
/// (an older baseline is kept, and its target updated after a successful write), logs every
/// family's outcome and flushes the resolver cache after a write. Refused adapters (see
/// [`super::adapters::capabilities`]) fail before anything is recorded.
pub(crate) fn run(
    stack: &dyn NetStack,
    mode: Mode<'_>,
    adapter_id: &str,
    request: &DnsRequest,
) -> Result<DnsReport> {
    let mut warnings = Vec::new();
    if let Mode::Apply(safety) = mode {
        safety.ensure_elevated()?;
        warnings.extend(safety.warnings().iter().cloned());
    }
    let listed = list_with(stack, None)?;
    let id = canonical_guid(adapter_id);
    let adapter = listed.adapters.iter().find(|a| a.id == id).ok_or_else(|| {
        Error::Other(format!(
            "network adapter {adapter_id} is not on this PC; it may have been removed or \
                 disabled"
        ))
    })?;
    if !adapter.can_change_dns {
        return Err(Error::Other(adapter.note.clone().unwrap_or_else(|| {
            "DNS servers cannot be changed on this adapter".to_string()
        })));
    }

    let mut changes = Vec::new();
    let mut written = false;
    for family in IpFamily::ALL {
        let servers = match request.choice(family) {
            DnsChoice::Unchanged => continue,
            DnsChoice::Automatic => Vec::new(),
            DnsChoice::Servers(servers) => servers.clone(),
        };
        let target = servers_string(&servers);
        let op_target = dns_target(family, &adapter.name);
        let mut change = DnsChange {
            family,
            previous: adapter.dns(family).clone(),
            target: target_config(&servers, family),
            outcome: ChangeOutcome::Planned,
            detail: None,
        };
        let log = |outcome: &str, detail: Option<&str>| -> Result<()> {
            match mode {
                Mode::Plan => Ok(()),
                Mode::Apply(safety) => safety.log_op(OP_SET_DNS, &op_target, outcome, detail),
            }
        };

        if let Some(reason) = skip_reason(adapter, family, servers.is_empty()) {
            log("skipped", Some(reason))?;
            change.outcome = ChangeOutcome::Skipped;
            change.detail = Some(reason.to_string());
            changes.push(change);
            continue;
        }
        let current = match stack.static_dns(&adapter.id, family) {
            Ok(stored) => stored.servers,
            Err(e) => {
                let detail = format!(
                    "cannot read the current {} DNS servers: {e}",
                    family.label()
                );
                log("failed", Some(&detail))?;
                change.outcome = ChangeOutcome::Failed;
                change.detail = Some(detail);
                changes.push(change);
                continue;
            }
        };
        if same_servers(&current, &target) {
            log("already_in_desired_state", None)?;
            change.outcome = ChangeOutcome::AlreadySet;
            changes.push(change);
            continue;
        }
        if let Mode::Apply(safety) = mode {
            let captured = safety.record_dns(&NewDnsRecord {
                interface_guid: adapter.id.clone(),
                family,
                adapter_name: adapter.name.clone(),
                previous_servers: current.clone(),
                target_servers: target.clone(),
            })?;
            match stack.set_static_dns(&adapter.id, family, &target) {
                Ok(()) => {
                    if !captured {
                        // An older baseline stays; its target follows what was written, so
                        // this change is not mistaken for one made outside Cairn.
                        safety.update_dns_target(&adapter.id, family, &target)?;
                    }
                    let detail = format!("{} → {}", servers_text(&current), servers_text(&target));
                    log("applied", Some(&detail))?;
                    change.outcome = ChangeOutcome::Applied;
                    change.detail = Some(detail);
                    written = true;
                }
                Err(e) => {
                    let detail = e.to_string();
                    log("failed", Some(&detail))?;
                    change.outcome = ChangeOutcome::Failed;
                    change.detail = Some(detail);
                }
            }
        }
        changes.push(change);
    }

    if let (Mode::Apply(safety), true) = (mode, written) {
        match stack.flush_resolver_cache() {
            Ok(()) => safety.log_op(OP_FLUSH, FLUSH_TARGET, "flushed", None)?,
            Err(e) => {
                safety.log_op(OP_FLUSH, FLUSH_TARGET, "failed", Some(&e.to_string()))?;
                warnings.push(FLUSH_WARNING.to_string());
            }
        }
    }

    Ok(DnsReport {
        dry_run: matches!(mode, Mode::Plan),
        adapter_id: adapter.id.clone(),
        adapter_name: adapter.name.clone(),
        session_id: match mode {
            Mode::Plan => None,
            Mode::Apply(safety) => Some(safety.session_id()),
        },
        changes,
        warnings,
    })
}

/// Writes the recorded servers of `rec` back, unless the adapter already has them. An
/// adapter that is not listed is written only while its TCP/IP interface key still exists
/// (it is disabled or disconnected); without the key it is gone ([`DnsRestore::NotFound`]).
/// Automatic IPv4 DNS is not restored on an adapter that now has a manually set IPv4
/// address.
pub(crate) fn restore_with(stack: &dyn NetStack, rec: &DnsRecord) -> Result<DnsRestore> {
    let guid = canonical_guid(&rec.interface_guid);
    let adapters = stack.adapters()?;
    let Some(adapter) = adapters.iter().find(|a| a.id == guid) else {
        if !stack.interface_key_exists(&guid, rec.family)? {
            return Ok(DnsRestore::NotFound);
        }
        return match stack.set_static_dns(&guid, rec.family, &rec.previous_servers) {
            Ok(()) => Ok(DnsRestore::Written),
            Err(e) => Err(Error::Other(format!(
                "network adapter {} ({guid}) is disabled or disconnected; enable it and undo \
                 again ({e})",
                rec.adapter_name
            ))),
        };
    };
    let current = stack.static_dns(&guid, rec.family)?.servers;
    if same_servers(&current, &rec.previous_servers) {
        return Ok(DnsRestore::AlreadyInState);
    }
    if rec.family == IpFamily::Ipv4
        && split_servers(&rec.previous_servers).is_empty()
        && !adapter.dhcp_enabled
    {
        return Err(Error::Other(format!(
            "{} now has a manually set IPv4 address; automatic DNS would leave it without IPv4 \
             DNS servers. Set its DNS servers in Windows Settings instead.",
            adapter.name
        )));
    }
    stack.set_static_dns(&guid, rec.family, &rec.previous_servers)?;
    Ok(DnsRestore::Written)
}

/// Read-only. The servers now set for the record's interface and family, as text, when
/// they are neither the recorded baseline nor the recorded target.
pub(crate) fn drift_with(stack: &dyn NetStack, rec: &DnsRecord) -> Result<Option<String>> {
    let guid = canonical_guid(&rec.interface_guid);
    if !stack.adapters()?.iter().any(|a| a.id == guid) {
        return Ok(None);
    }
    let current = stack.static_dns(&guid, rec.family)?.servers;
    let drifted = !same_servers(&current, &rec.previous_servers)
        && !same_servers(&current, &rec.target_servers);
    Ok(drifted.then(|| servers_text(&current)))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashSet;
    use std::rc::Rc;
    use std::sync::Arc;

    use super::*;
    use crate::network::adapters::NOTE_VPN_CONNECTED;
    use crate::network::adapters::{AdapterKind, LinkStatus, NOTE_PROFILE, NOTE_VPN_ADAPTER};
    use crate::network::tests::{adapter, guid, journal, FakeStack};
    use crate::safety::state_log::Journal;
    use crate::safety::test_safety;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn addrs(items: &[&str]) -> Vec<IpAddr> {
        items.iter().map(|s| s.parse().unwrap()).collect()
    }

    fn cloudflare() -> DnsRequest {
        DnsRequest::preset("cloudflare").unwrap()
    }

    fn wifi_stack() -> FakeStack {
        FakeStack::with(vec![adapter(1, "Wi-Fi", AdapterKind::Wifi)])
    }

    fn ops_of(journal: &Journal, op: &str) -> Vec<(String, String, Option<String>)> {
        let mut rows: Vec<_> = journal
            .ops(100)
            .unwrap()
            .into_iter()
            .filter(|o| o.op == op)
            .map(|o| (o.target, o.outcome, o.detail))
            .collect();
        rows.reverse();
        rows
    }

    fn record(family: IpFamily, previous: &str, target: &str) -> DnsRecord {
        DnsRecord {
            id: 1,
            session_id: 1,
            recorded_at: "2026-09-25T10:00:00+00:00".into(),
            interface_guid: guid(1).to_uppercase(),
            family,
            adapter_name: "Wi-Fi".into(),
            previous_servers: previous.into(),
            target_servers: target.into(),
            active: true,
            reverted_at: None,
        }
    }

    #[test]
    fn presets_are_valid() {
        let mut ids = HashSet::new();
        for preset in PRESETS {
            assert!(ids.insert(preset.id), "duplicate id {}", preset.id);
            assert!(!preset.title.is_empty());
            assert!(preset.description.ends_with('.'), "{}", preset.id);
            for family in IpFamily::ALL {
                let servers = strings(preset.servers(family));
                let parsed = parse_servers(&servers, family).unwrap();
                assert_eq!(parsed.len(), servers.len(), "{} {family:?}", preset.id);
                assert_eq!(servers_string(&parsed), servers.join(","), "canonical text");
            }
            let request = DnsRequest::preset(preset.id).unwrap();
            if preset.id == "automatic" {
                assert_eq!(request.ipv4, DnsChoice::Automatic);
                assert_eq!(request.ipv6, DnsChoice::Automatic);
            } else {
                assert_eq!(preset.ipv4.len(), 2, "{}", preset.id);
                assert_eq!(preset.ipv6.len(), 2, "{}", preset.id);
                assert!(matches!(request.ipv4, DnsChoice::Servers(_)));
                assert_eq!(
                    preset_matching(&strings(preset.ipv4), IpFamily::Ipv4).as_deref(),
                    Some(preset.id)
                );
            }
        }
        assert_eq!(PRESETS[0].id, "automatic");
        assert_eq!(
            DnsRequest::preset(" Google ").unwrap().ipv4,
            DnsChoice::Servers(addrs(&["8.8.8.8", "8.8.4.4"]))
        );
        let json = serde_json::to_value(PRESETS).unwrap();
        assert_eq!(json[1]["id"], "cloudflare");
        assert_eq!(json[1]["ipv6"][0], "2606:4700:4700::1111");
        assert_eq!(json[0]["ipv4"], serde_json::json!([]));
    }

    #[test]
    fn parse_servers_validates() {
        let v4 = |items: &[&str]| parse_servers(&strings(items), IpFamily::Ipv4);
        let v6 = |items: &[&str]| parse_servers(&strings(items), IpFamily::Ipv6);
        assert_eq!(
            v4(&["1.1.1.1, 1.0.0.1", " 8.8.8.8 "]).unwrap(),
            addrs(&["1.1.1.1", "1.0.0.1", "8.8.8.8"])
        );
        assert_eq!(
            v4(&["1.1.1.1 1.1.1.1", "1.1.1.1"]).unwrap(),
            addrs(&["1.1.1.1"])
        );
        assert_eq!(v4(&[]).unwrap(), Vec::<IpAddr>::new());
        assert_eq!(v4(&["", " , "]).unwrap(), Vec::<IpAddr>::new());
        assert_eq!(
            v4(&["127.0.0.1"]).unwrap(),
            addrs(&["127.0.0.1"]),
            "loopback"
        );
        assert_eq!(v6(&["::1"]).unwrap(), addrs(&["::1"]), "loopback");
        assert_eq!(
            v6(&["2606:4700:4700:0:0:0:0:1111"]).unwrap(),
            addrs(&["2606:4700:4700::1111"])
        );

        let err = |r: Result<Vec<IpAddr>>| r.unwrap_err().to_string();
        assert_eq!(
            err(v4(&["2606:4700:4700::1111"])),
            "2606:4700:4700::1111 is not a valid IPv4 address"
        );
        assert_eq!(err(v6(&["1.1.1.1"])), "1.1.1.1 is not a valid IPv6 address");
        assert_eq!(
            err(v4(&["dns.google"])),
            "dns.google is not a valid IPv4 address"
        );
        assert_eq!(err(v4(&["1.1.1"])), "1.1.1 is not a valid IPv4 address");
        for bad in ["0.0.0.0", "224.0.0.1", "255.255.255.255"] {
            assert_eq!(
                err(v4(&[bad])),
                format!("{bad} cannot be used as a DNS server")
            );
        }
        for bad in ["::", "ff02::1", "fe80::1", "febf::1", "fec0::1", "feff::1"] {
            assert_eq!(
                err(v6(&[bad])),
                format!("{bad} cannot be used as a DNS server")
            );
        }
        assert_eq!(
            err(v4(&["1.1.1.1,1.0.0.1,8.8.8.8,8.8.4.4,9.9.9.9"])),
            "at most 4 IPv4 DNS servers"
        );
        assert_eq!(
            v4(&["1.1.1.1,1.0.0.1,8.8.8.8,8.8.4.4,1.1.1.1"])
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn custom_with_both_lists_empty_is_rejected() {
        let err = DnsRequest::custom(&[], &[]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "custom DNS needs at least one IPv4 or IPv6 server"
        );
        assert!(DnsRequest::custom(&strings(&[" "]), &strings(&[""])).is_err());
        let v4_only = DnsRequest::custom(&strings(&["10.0.0.53"]), &[]).unwrap();
        assert_eq!(v4_only.ipv4, DnsChoice::Servers(addrs(&["10.0.0.53"])));
        assert_eq!(v4_only.ipv6, DnsChoice::Unchanged);
        let v6_only = DnsRequest::custom(&[], &strings(&["2620:fe::fe"])).unwrap();
        assert_eq!(v6_only.ipv4, DnsChoice::Unchanged);
        assert!(DnsRequest::custom(&strings(&["bad"]), &strings(&["::1"])).is_err());
    }

    #[test]
    fn unknown_preset_is_rejected() {
        let err = DnsRequest::preset("opendns").unwrap_err().to_string();
        assert!(
            err.starts_with("unknown DNS preset \"opendns\"; valid presets: automatic, cloudflare"),
            "{err}"
        );
        assert!(DnsRequest::preset("").is_err());
        assert!(DnsRequest::preset("custom").is_err());
    }

    #[test]
    fn same_servers_compares_addresses_in_order() {
        assert!(same_servers("1.1.1.1,1.0.0.1", "1.1.1.1 1.0.0.1"));
        assert!(same_servers(
            "2606:4700:4700:0::1111",
            "2606:4700:4700::1111"
        ));
        assert!(same_servers("", " "));
        assert!(!same_servers("1.1.1.1,1.0.0.1", "1.0.0.1,1.1.1.1"));
        assert!(!same_servers("1.1.1.1", ""));
        assert!(same_servers("Not-An-Address", "not-an-address"));
    }

    #[test]
    fn plan_changes_nothing_and_opens_no_session() {
        let (_dir, journal) = journal();
        let stack = wifi_stack();
        stack.set_static(&guid(1), IpFamily::Ipv6, "2001:db8::53");
        let begun = Cell::new(0);
        let begin = || {
            begun.set(begun.get() + 1);
            Ok(test_safety(Arc::clone(&journal), "dns", false))
        };
        let report = plan_or_apply(&stack, true, begin, &guid(1), &cloudflare()).unwrap();
        assert_eq!(begun.get(), 0, "a plan never opens a session");
        assert!(report.dry_run);
        assert_eq!(report.session_id, None);
        assert_eq!(report.adapter_name, "Wi-Fi");
        assert_eq!(report.changes.len(), 2);
        assert!(report
            .changes
            .iter()
            .all(|c| c.outcome == ChangeOutcome::Planned));
        let v4 = &report.changes[0];
        assert_eq!(v4.family, IpFamily::Ipv4);
        assert_eq!(v4.previous.mode, DnsMode::Automatic);
        assert_eq!(v4.previous.servers, vec!["192.168.0.1"]);
        assert_eq!(v4.target.mode, DnsMode::Manual);
        assert_eq!(v4.target.servers, vec!["1.1.1.1", "1.0.0.1"]);
        assert_eq!(v4.target.preset.as_deref(), Some("cloudflare"));
        assert_eq!(report.changes[1].previous.mode, DnsMode::Manual);
        assert!(stack.calls().is_empty(), "{:?}", stack.calls());
        assert!(journal.sessions().unwrap().is_empty());
        assert!(journal.ops(10).unwrap().is_empty());
        assert_eq!(stack.static_of(&guid(1), IpFamily::Ipv6), "2001:db8::53");
    }

    #[test]
    fn apply_opens_its_session_before_anything_else() {
        let (_dir, journal) = journal();
        let stack = wifi_stack();
        let begun = Cell::new(0);
        let begin = || {
            begun.set(begun.get() + 1);
            assert!(stack.calls().is_empty(), "the session comes first");
            Ok(test_safety(Arc::clone(&journal), "dns", false))
        };
        let report = plan_or_apply(&stack, false, begin, &guid(1), &cloudflare()).unwrap();
        assert_eq!(begun.get(), 1);
        assert!(!report.dry_run);
        let sessions = journal.sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(report.session_id, Some(sessions[0].id));
        assert_eq!(journal.active_dns().unwrap().len(), 2);

        // A session that cannot be opened stops the change before anything is read or written.
        let stack = wifi_stack();
        let err = plan_or_apply(
            &stack,
            false,
            || Err(Error::NotElevated),
            &guid(1),
            &cloudflare(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");
        assert!(stack.calls().is_empty());
        assert_eq!(journal.sessions().unwrap().len(), 1);
    }

    #[test]
    fn apply_records_baseline_before_writing() {
        let (_dir, journal) = journal();
        let mut stack = wifi_stack();
        stack.set_static(&guid(1), IpFamily::Ipv6, "2001:db8::53");
        let seen = Rc::new(RefCell::new(Vec::new()));
        let hook_journal = Arc::clone(&journal);
        let hook_seen = Rc::clone(&seen);
        stack.on_set = Some(Box::new(move |_, family, _| {
            let active = hook_journal.active_dns().unwrap();
            let rec = active
                .iter()
                .find(|r| r.family == family)
                .expect("the baseline is recorded before the write");
            hook_seen.borrow_mut().push((
                family,
                rec.previous_servers.clone(),
                rec.target_servers.clone(),
            ));
        }));
        let safety = test_safety(Arc::clone(&journal), "dns", false);
        let report = run(&stack, Mode::Apply(&safety), &guid(1), &cloudflare()).unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.session_id, Some(safety.session_id()));
        assert!(report
            .changes
            .iter()
            .all(|c| c.outcome == ChangeOutcome::Applied));
        assert_eq!(
            report.changes[0].detail.as_deref(),
            Some("automatic → 1.1.1.1, 1.0.0.1")
        );
        assert_eq!(
            *seen.borrow(),
            vec![
                (IpFamily::Ipv4, String::new(), "1.1.1.1,1.0.0.1".to_string()),
                (
                    IpFamily::Ipv6,
                    "2001:db8::53".to_string(),
                    "2606:4700:4700::1111,2606:4700:4700::1001".to_string()
                ),
            ]
        );
        assert_eq!(stack.static_of(&guid(1), IpFamily::Ipv4), "1.1.1.1,1.0.0.1");
        let records = journal.active_dns().unwrap();
        assert_eq!(records.len(), 2);
        assert!(records
            .iter()
            .all(|r| r.interface_guid == guid(1) && r.adapter_name == "Wi-Fi"));
        assert_eq!(
            ops_of(&journal, OP_SET_DNS),
            vec![
                (
                    "IPv4 DNS servers of Wi-Fi".to_string(),
                    "applied".to_string(),
                    Some("automatic → 1.1.1.1, 1.0.0.1".to_string())
                ),
                (
                    "IPv6 DNS servers of Wi-Fi".to_string(),
                    "applied".to_string(),
                    Some("2001:db8::53 → 2606:4700:4700::1111, 2606:4700:4700::1001".to_string())
                ),
            ]
        );
        assert_eq!(
            stack.calls(),
            vec![
                format!("set {} ipv4 1.1.1.1,1.0.0.1", guid(1)),
                format!(
                    "set {} ipv6 2606:4700:4700::1111,2606:4700:4700::1001",
                    guid(1)
                ),
                "flush".to_string(),
            ]
        );
    }

    #[test]
    fn already_set_writes_no_record() {
        let (_dir, journal) = journal();
        let stack = wifi_stack();
        stack.set_static(&guid(1), IpFamily::Ipv4, "1.1.1.1 1.0.0.1");
        let safety = test_safety(Arc::clone(&journal), "dns", false);
        let request = DnsRequest::custom(&strings(&["1.1.1.1,1.0.0.1"]), &[]).unwrap();
        let report = run(&stack, Mode::Apply(&safety), &guid(1), &request).unwrap();
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].outcome, ChangeOutcome::AlreadySet);
        assert!(journal.active_dns().unwrap().is_empty());
        assert!(stack.calls().is_empty(), "no write and no flush");
        assert_eq!(
            ops_of(&journal, OP_SET_DNS),
            vec![(
                "IPv4 DNS servers of Wi-Fi".to_string(),
                "already_in_desired_state".to_string(),
                None
            )]
        );
        assert!(ops_of(&journal, OP_FLUSH).is_empty());
    }

    #[test]
    fn automatic_ipv4_on_static_adapter_is_skipped() {
        let (_dir, journal) = journal();
        let mut ethernet = adapter(1, "Ethernet", AdapterKind::Ethernet);
        ethernet.dhcp_enabled = false;
        let stack = FakeStack::with(vec![ethernet]);
        stack.set_static(&guid(1), IpFamily::Ipv4, "10.0.0.53");
        stack.set_static(&guid(1), IpFamily::Ipv6, "2620:fe::fe");
        let safety = test_safety(Arc::clone(&journal), "dns", false);
        let request = DnsRequest::preset("automatic").unwrap();
        let report = run(&stack, Mode::Apply(&safety), &guid(1), &request).unwrap();
        let v4 = &report.changes[0];
        assert_eq!(v4.outcome, ChangeOutcome::Skipped);
        assert_eq!(
            v4.detail.as_deref(),
            Some(
                "this adapter has a manually set IPv4 address, so it would get no IPv4 DNS \
                 servers automatically"
            )
        );
        assert_eq!(report.changes[1].outcome, ChangeOutcome::Applied);
        assert_eq!(stack.static_of(&guid(1), IpFamily::Ipv4), "10.0.0.53");
        assert_eq!(stack.static_of(&guid(1), IpFamily::Ipv6), "");
        let rows = ops_of(&journal, OP_SET_DNS);
        assert_eq!(rows[0].1, "skipped");
        assert_eq!(rows[0].2.as_deref(), v4.detail.as_deref());
        assert_eq!(journal.active_dns().unwrap().len(), 1, "only IPv6 recorded");

        let plan = run(&stack, Mode::Plan, &guid(1), &request).unwrap();
        assert_eq!(plan.changes[0].outcome, ChangeOutcome::Skipped);
    }

    #[test]
    fn disabled_ipv6_is_skipped() {
        let (_dir, journal) = journal();
        let mut wifi = adapter(1, "Wi-Fi", AdapterKind::Wifi);
        wifi.ipv6_enabled = false;
        let stack = FakeStack::with(vec![wifi]);
        let safety = test_safety(Arc::clone(&journal), "dns", false);
        let report = run(&stack, Mode::Apply(&safety), &guid(1), &cloudflare()).unwrap();
        assert_eq!(report.changes[0].outcome, ChangeOutcome::Applied);
        assert_eq!(report.changes[1].outcome, ChangeOutcome::Skipped);
        assert_eq!(
            report.changes[1].detail.as_deref(),
            Some("IPv6 is turned off on this adapter")
        );
        assert_eq!(stack.static_of(&guid(1), IpFamily::Ipv6), "");

        let mut no_v4 = adapter(2, "Ethernet", AdapterKind::Ethernet);
        no_v4.ipv4_enabled = false;
        let stack = FakeStack::with(vec![no_v4]);
        let plan = run(&stack, Mode::Plan, &guid(2), &cloudflare()).unwrap();
        assert_eq!(
            plan.changes[0].detail.as_deref(),
            Some("IPv4 is turned off on this adapter")
        );
        assert_eq!(plan.changes[1].outcome, ChangeOutcome::Planned);
    }

    fn assert_refused(stack: &FakeStack, id: &str, expected: &str) {
        let (_dir, journal) = journal();
        let safety = test_safety(Arc::clone(&journal), "dns", false);
        let err = run(stack, Mode::Apply(&safety), id, &cloudflare()).unwrap_err();
        assert_eq!(err.to_string(), expected);
        let err = run(stack, Mode::Plan, id, &cloudflare()).unwrap_err();
        assert_eq!(err.to_string(), expected);
        assert!(
            journal.active_dns().unwrap().is_empty(),
            "nothing journaled"
        );
        assert!(journal.ops(10).unwrap().is_empty(), "nothing logged");
        assert!(stack.calls().is_empty(), "nothing written");
    }

    #[test]
    fn refused_adapter_journals_and_writes_nothing() {
        let stack = FakeStack::with(vec![adapter(1, "ProtonVPN", AdapterKind::Vpn)]);
        assert_refused(&stack, &guid(1), NOTE_VPN_ADAPTER);
        let mut gone = adapter(2, "Ethernet", AdapterKind::Ethernet);
        gone.status = LinkStatus::NotPresent;
        let stack = FakeStack::with(vec![gone]);
        assert_refused(&stack, &guid(2), "This adapter is not present.");
        let stack = wifi_stack();
        assert_refused(
            &stack,
            &guid(9),
            &format!(
                "network adapter {} is not on this PC; it may have been removed or disabled",
                guid(9)
            ),
        );
    }

    #[test]
    fn profile_dns_blocks_change() {
        let stack = wifi_stack();
        stack
            .profile_dns
            .borrow_mut()
            .insert((guid(1), IpFamily::Ipv4), "9.9.9.9".into());
        assert_refused(&stack, &guid(1), NOTE_PROFILE);
    }

    #[test]
    fn vpn_connected_refuses_apply_and_plan() {
        let stack = FakeStack::with(vec![
            adapter(1, "Wi-Fi", AdapterKind::Wifi),
            adapter(2, "ProtonVPN", AdapterKind::Vpn),
        ]);
        assert_refused(&stack, &guid(1), NOTE_VPN_CONNECTED);
        let braced_upper = guid(1).to_uppercase();
        assert_refused(&stack, &braced_upper, NOTE_VPN_CONNECTED);
    }

    #[test]
    fn second_change_keeps_first_baseline() {
        let (_dir, journal) = journal();
        let stack = wifi_stack();
        stack.set_static(&guid(1), IpFamily::Ipv4, "10.0.0.53");
        let first = test_safety(Arc::clone(&journal), "dns", false);
        let v4 = |id: &str| {
            DnsRequest::custom(
                &strings(PRESETS.iter().find(|p| p.id == id).unwrap().ipv4),
                &[],
            )
            .unwrap()
        };
        run(&stack, Mode::Apply(&first), &guid(1), &v4("cloudflare")).unwrap();
        drop(first);
        let second = test_safety(Arc::clone(&journal), "dns", false);
        let report = run(&stack, Mode::Apply(&second), &guid(1), &v4("google")).unwrap();
        assert_eq!(report.changes[0].outcome, ChangeOutcome::Applied);
        assert_eq!(stack.static_of(&guid(1), IpFamily::Ipv4), "8.8.8.8,8.8.4.4");
        let records = journal.active_dns().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].previous_servers, "10.0.0.53",
            "the first baseline wins"
        );
        assert_eq!(
            records[0].target_servers, "8.8.8.8,8.8.4.4",
            "the target follows the latest write"
        );
        assert_eq!(
            drift_with(&stack, &records[0]).unwrap(),
            None,
            "Cairn's own second change is not drift"
        );

        // A write that fails keeps the target of the last write that succeeded.
        let mut failing = FakeStack::with(vec![adapter(1, "Wi-Fi", AdapterKind::Wifi)]);
        failing.set_static(&guid(1), IpFamily::Ipv4, "8.8.8.8,8.8.4.4");
        failing.fail_set = Some(IpFamily::Ipv4);
        let third = test_safety(Arc::clone(&journal), "dns", false);
        let report = run(&failing, Mode::Apply(&third), &guid(1), &v4("quad9")).unwrap();
        assert_eq!(report.changes[0].outcome, ChangeOutcome::Failed);
        let records = journal.active_dns().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].previous_servers, "10.0.0.53");
        assert_eq!(records[0].target_servers, "8.8.8.8,8.8.4.4");
    }

    #[test]
    fn failed_write_keeps_record_and_logs_failure() {
        let (_dir, journal) = journal();
        let mut stack = wifi_stack();
        stack.fail_set = Some(IpFamily::Ipv6);
        let safety = test_safety(Arc::clone(&journal), "dns", false);
        let report = run(&stack, Mode::Apply(&safety), &guid(1), &cloudflare()).unwrap();
        assert_eq!(report.changes[0].outcome, ChangeOutcome::Applied);
        assert_eq!(report.changes[1].outcome, ChangeOutcome::Failed);
        assert_eq!(
            report.changes[1].detail.as_deref(),
            Some("Access is denied.")
        );
        let records = journal.active_dns().unwrap();
        assert_eq!(records.len(), 2, "the failed family keeps its baseline");
        let rows = ops_of(&journal, OP_SET_DNS);
        assert_eq!(
            rows[1],
            (
                "IPv6 DNS servers of Wi-Fi".to_string(),
                "failed".to_string(),
                Some("Access is denied.".to_string())
            )
        );
        assert_eq!(
            ops_of(&journal, OP_FLUSH)[0].1,
            "flushed",
            "IPv4 was written"
        );

        let (_dir, journal) = journal_pair();
        let mut stack = wifi_stack();
        stack.fail_set = Some(IpFamily::Ipv4);
        let safety = test_safety(Arc::clone(&journal), "dns", false);
        let request = DnsRequest::custom(&strings(&["1.1.1.1"]), &[]).unwrap();
        let report = run(&stack, Mode::Apply(&safety), &guid(1), &request).unwrap();
        assert_eq!(report.changes[0].outcome, ChangeOutcome::Failed);
        assert!(
            !stack.calls().contains(&"flush".to_string()),
            "nothing written, no flush"
        );
        assert!(report.warnings.is_empty());
    }

    fn journal_pair() -> (tempfile::TempDir, Arc<Journal>) {
        journal()
    }

    #[test]
    fn apply_flushes_cache_best_effort() {
        let (_dir, journal) = journal();
        let mut stack = wifi_stack();
        stack.fail_flush = Some("The DNS Client service is not running.".into());
        let safety = test_safety(Arc::clone(&journal), "dns", false);
        let report = run(&stack, Mode::Apply(&safety), &guid(1), &cloudflare()).unwrap();
        assert!(report
            .changes
            .iter()
            .all(|c| c.outcome == ChangeOutcome::Applied));
        assert_eq!(report.warnings, vec![FLUSH_WARNING.to_string()]);
        assert_eq!(
            ops_of(&journal, OP_FLUSH),
            vec![(
                FLUSH_TARGET.to_string(),
                "failed".to_string(),
                Some("The DNS Client service is not running.".to_string())
            )]
        );
        assert_eq!(stack.calls().last().map(String::as_str), Some("flush"));

        let (_dir2, journal) = journal_pair();
        let stack = wifi_stack();
        let safety = test_safety(Arc::clone(&journal), "dns", false);
        let report = run(&stack, Mode::Apply(&safety), &guid(1), &cloudflare()).unwrap();
        assert!(report.warnings.is_empty());
        assert_eq!(
            ops_of(&journal, OP_FLUSH),
            vec![(FLUSH_TARGET.to_string(), "flushed".to_string(), None)]
        );
    }

    #[test]
    fn unelevated_apply_is_refused_before_anything() {
        if crate::is_elevated() {
            return;
        }
        let (_dir, journal) = journal();
        let stack = wifi_stack();
        let safety = test_safety(Arc::clone(&journal), "dns", true);
        let err = run(&stack, Mode::Apply(&safety), &guid(1), &cloudflare()).unwrap_err();
        assert!(matches!(err, Error::NotElevated), "{err}");
        assert!(stack.calls().is_empty());
        assert!(journal.active_dns().unwrap().is_empty());
        assert!(journal.ops(10).unwrap().is_empty());
    }

    #[test]
    fn machine_wide_change_is_not_refused_for_another_account() {
        let (_dir, journal) = journal();
        let stack = wifi_stack();
        let safety = test_safety(Arc::clone(&journal), "dns", false);
        safety.assume_other_user();
        let report = run(&stack, Mode::Apply(&safety), &guid(1), &cloudflare()).unwrap();
        assert!(report
            .changes
            .iter()
            .all(|c| c.outcome == ChangeOutcome::Applied));
        assert_eq!(journal.active_dns().unwrap().len(), 2);
    }

    #[test]
    fn restore_writes_previous_verbatim_and_reports_written() {
        let stack = wifi_stack();
        stack.set_static(&guid(1), IpFamily::Ipv4, "1.1.1.1,1.0.0.1");
        let rec = record(
            IpFamily::Ipv4,
            "192.168.0.1, 198.51.100.53",
            "1.1.1.1,1.0.0.1",
        );
        assert_eq!(restore_with(&stack, &rec).unwrap(), DnsRestore::Written);
        assert_eq!(
            stack.calls(),
            vec![format!("set {} ipv4 192.168.0.1, 198.51.100.53", guid(1))]
        );

        let stack = wifi_stack();
        stack.set_static(&guid(1), IpFamily::Ipv6, "2606:4700:4700::1111");
        let rec = record(IpFamily::Ipv6, "", "2606:4700:4700::1111");
        assert_eq!(restore_with(&stack, &rec).unwrap(), DnsRestore::Written);
        assert_eq!(
            stack.static_of(&guid(1), IpFamily::Ipv6),
            "",
            "back to automatic"
        );
    }

    #[test]
    fn restore_already_in_state_writes_nothing() {
        let stack = wifi_stack();
        stack.set_static(&guid(1), IpFamily::Ipv4, "10.0.0.53,10.0.0.54");
        let rec = record(IpFamily::Ipv4, "10.0.0.53 10.0.0.54", "1.1.1.1");
        assert_eq!(
            restore_with(&stack, &rec).unwrap(),
            DnsRestore::AlreadyInState
        );
        let rec = record(IpFamily::Ipv6, "", "2606:4700:4700::1111");
        assert_eq!(
            restore_with(&stack, &rec).unwrap(),
            DnsRestore::AlreadyInState
        );
        assert!(stack.calls().is_empty());
    }

    #[test]
    fn restore_of_removed_adapter_is_not_found() {
        let stack = FakeStack::with(vec![adapter(2, "Ethernet", AdapterKind::Ethernet)]);
        let rec = record(IpFamily::Ipv4, "", "1.1.1.1");
        assert_eq!(restore_with(&stack, &rec).unwrap(), DnsRestore::NotFound);
        assert!(stack.calls().is_empty(), "no write");
        assert_eq!(drift_with(&stack, &rec).unwrap(), None);
    }

    #[test]
    fn restore_of_disabled_adapter_tries_and_fails_with_enable_hint() {
        let mut stack = FakeStack::with(Vec::new());
        stack.keys.borrow_mut().insert((guid(1), IpFamily::Ipv4));
        stack.fail_set = Some(IpFamily::Ipv4);
        let rec = record(IpFamily::Ipv4, "", "1.1.1.1");
        let err = restore_with(&stack, &rec).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "network adapter Wi-Fi ({}) is disabled or disconnected; enable it and undo \
                 again (Access is denied.)",
                guid(1)
            )
        );
        assert_eq!(stack.calls(), vec![format!("set {} ipv4 ", guid(1))]);

        stack.fail_set = None;
        assert_eq!(restore_with(&stack, &rec).unwrap(), DnsRestore::Written);
        let rec6 = record(IpFamily::Ipv6, "", "::1");
        assert_eq!(
            restore_with(&stack, &rec6).unwrap(),
            DnsRestore::NotFound,
            "keys are per family"
        );
    }

    #[test]
    fn restore_refuses_automatic_ipv4_on_static_adapter() {
        let mut wifi = adapter(1, "Wi-Fi", AdapterKind::Wifi);
        wifi.dhcp_enabled = false;
        let stack = FakeStack::with(vec![wifi]);
        stack.set_static(&guid(1), IpFamily::Ipv4, "1.1.1.1");
        let rec = record(IpFamily::Ipv4, "", "1.1.1.1");
        let err = restore_with(&stack, &rec).unwrap_err().to_string();
        assert_eq!(
            err,
            "Wi-Fi now has a manually set IPv4 address; automatic DNS would leave it without \
             IPv4 DNS servers. Set its DNS servers in Windows Settings instead."
        );
        assert!(stack.calls().is_empty());

        let rec = record(IpFamily::Ipv4, "10.0.0.53", "1.1.1.1");
        assert_eq!(restore_with(&stack, &rec).unwrap(), DnsRestore::Written);
        let rec6 = record(IpFamily::Ipv6, "", "::1");
        stack.set_static(&guid(1), IpFamily::Ipv6, "::1");
        assert_eq!(
            restore_with(&stack, &rec6).unwrap(),
            DnsRestore::Written,
            "IPv6 is fine"
        );
    }

    #[test]
    fn drift_reports_servers_changed_elsewhere() {
        let stack = wifi_stack();
        let rec = record(IpFamily::Ipv4, "", "1.1.1.1,1.0.0.1");
        stack.set_static(&guid(1), IpFamily::Ipv4, "8.8.8.8,8.8.4.4");
        assert_eq!(
            drift_with(&stack, &rec).unwrap().as_deref(),
            Some("8.8.8.8, 8.8.4.4")
        );
        stack.set_static(&guid(1), IpFamily::Ipv4, "1.1.1.1 1.0.0.1");
        assert_eq!(drift_with(&stack, &rec).unwrap(), None, "equals the target");
        stack.set_static(&guid(1), IpFamily::Ipv4, "");
        assert_eq!(
            drift_with(&stack, &rec).unwrap(),
            None,
            "equals the baseline"
        );

        let rec = record(IpFamily::Ipv4, "10.0.0.53", "1.1.1.1");
        assert_eq!(
            drift_with(&stack, &rec).unwrap().as_deref(),
            Some("automatic")
        );
        stack.unreadable.borrow_mut().insert(guid(1));
        assert!(
            drift_with(&stack, &rec).is_err(),
            "a read error is reported"
        );
        assert!(stack.calls().is_empty(), "read-only");
    }

    #[test]
    fn report_serializes_with_the_documented_keys() {
        let stack = wifi_stack();
        let report = run(&stack, Mode::Plan, &guid(1), &cloudflare()).unwrap();
        let json = serde_json::to_value(&report).unwrap();
        for key in [
            "dry_run",
            "adapter_id",
            "adapter_name",
            "session_id",
            "changes",
            "warnings",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        let change = &json["changes"][0];
        for key in ["family", "previous", "target", "outcome", "detail"] {
            assert!(change.get(key).is_some(), "missing {key}");
        }
        assert_eq!(change["family"], "ipv4");
        assert_eq!(change["outcome"], "planned");
        assert_eq!(json["session_id"], serde_json::Value::Null);
        assert_eq!(
            serde_json::to_value(ChangeOutcome::AlreadySet).unwrap(),
            "already_set"
        );
    }
}
