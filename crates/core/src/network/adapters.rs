//! Network adapters as the Network section lists them: kind, link state, addresses, DNS
//! configuration and what may be changed on each. Everything here is derived from what a
//! `NetStack` reports; nothing is written.

use std::net::IpAddr;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::dns::preset_matching;
use super::{canonical_guid, split_servers, IpFamily, NetStack, StaticDns};
use crate::safety::state_log::Journal;
use crate::{Error, Result};

/// `IF_TYPE_ETHERNET_CSMACD`: Ethernet, and most virtual switch ports.
const IF_TYPE_ETHERNET: u32 = 6;
/// `IF_TYPE_PPP`: dial-up, PPPoE and PPP-based VPN connections.
const IF_TYPE_PPP: u32 = 23;
/// `IF_TYPE_SOFTWARE_LOOPBACK`.
const IF_TYPE_LOOPBACK: u32 = 24;
/// `IF_TYPE_PROP_VIRTUAL`: proprietary virtual interfaces, used by VPN clients.
const IF_TYPE_PROP_VIRTUAL: u32 = 53;
/// `IF_TYPE_IEEE80211`: Wi-Fi, and Wi-Fi Direct virtual adapters.
const IF_TYPE_WIFI: u32 = 71;
/// `IF_TYPE_TUNNEL`: Teredo, 6to4, ISATAP and IP-HTTPS.
const IF_TYPE_TUNNEL: u32 = 131;
/// `IF_TYPE_WWANPP` and `IF_TYPE_WWANPP2`: mobile broadband.
const IF_TYPE_WWAN: [u32; 2] = [243, 244];
/// `NdisPhysicalMediumBluetooth`: Bluetooth personal area network adapters.
const MEDIUM_BLUETOOTH: u32 = 10;

/// `IP_ADAPTER_DHCP_ENABLED`, `IP_ADAPTER_IPV4_ENABLED` and `IP_ADAPTER_IPV6_ENABLED`.
pub(crate) const FLAG_DHCP_ENABLED: u32 = 4;
pub(crate) const FLAG_IPV4_ENABLED: u32 = 128;
pub(crate) const FLAG_IPV6_ENABLED: u32 = 256;
/// `IpDadStatePreferred`.
pub(crate) const DAD_PREFERRED: i32 = 4;

/// Name or description fragments (lowercase) of VPN adapters that present themselves as
/// Ethernet: the OpenVPN TAP drivers, WireGuard's adapters, GlobalProtect, Cisco AnyConnect
/// and Secure Client, and every adapter that calls itself a VPN.
const VPN_MARKERS: [&str; 9] = [
    "tap-windows",
    "tap-win32",
    "wintun",
    "wireguard",
    "openvpn",
    "vpn",
    "pangp",
    "anyconnect",
    "secure client virtual miniport",
];
/// Start and fragment (lowercase) of the descriptions VPN vendors give their rebuilds of the
/// OpenVPN TAP driver, such as "TAP-Surfshark Windows Adapter V9".
const BRANDED_TAP: (&str, &str) = ("tap-", "windows adapter");
/// Name or description fragments (lowercase) of PPP connections that are not VPNs.
const PPPOE_MARKERS: [&str; 2] = ["pppoe", "broadband"];

/// Placeholder IPv6 DNS servers (site-local, deprecated) that Windows lists when an
/// adapter has no IPv6 DNS server.
const PLACEHOLDER_DNS: [&str; 3] = ["fec0:0:0:ffff::1", "fec0:0:0:ffff::2", "fec0:0:0:ffff::3"];

pub(crate) const NOTE_NOT_PRESENT: &str = "This adapter is not present.";
pub(crate) const NOTE_VPN_ADAPTER: &str = "VPN and tunnel adapters get their DNS servers from \
     the software that manages them; change DNS there.";
pub(crate) const NOTE_OTHER: &str = "DNS cannot be changed on this type of adapter here.";
pub(crate) const NOTE_UNREADABLE: &str = "Its DNS settings could not be read.";
pub(crate) const NOTE_PROFILE: &str = "DNS servers for this Wi-Fi network are set in Windows \
     Settings (Wi-Fi > network properties); change or clear them there.";
pub(crate) const NOTE_VPN_CONNECTED: &str = "Disconnect the VPN to change DNS servers; while \
     it is connected it may be managing them.";
pub(crate) const NOTE_VIRTUAL: &str = "Virtual adapter: the software that created it (for \
     example Hyper-V or a VPN) may replace these DNS servers.";

/// What kind of network an adapter connects to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    Ethernet,
    Wifi,
    Cellular,
    Bluetooth,
    /// A VPN client's adapter.
    Vpn,
    /// A virtual adapter of a hypervisor or similar software, including Wi-Fi Direct.
    Virtual,
    /// An IPv6 transition tunnel (Teredo, 6to4, ISATAP, IP-HTTPS); never counted as a VPN.
    Tunnel,
    Other,
}

/// Operational state of an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkStatus {
    Connected,
    Disconnected,
    /// The adapter's hardware is missing, for example an unplugged USB adapter.
    NotPresent,
    Unknown,
}

/// Where an address came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressOrigin {
    /// Set by hand in the adapter's settings.
    Manual,
    Dhcp,
    /// Formed from a router advertisement (SLAAC).
    Autoconfigured,
    /// A temporary (privacy) IPv6 address.
    Temporary,
    /// A link-local address (169.254/16 or fe80::/10).
    LinkLocal,
    Other,
}

/// How an adapter gets its DNS servers for one address family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DnsMode {
    /// From DHCP or router advertisements.
    Automatic,
    /// Set on the adapter.
    Manual,
    /// Set for the connected Wi-Fi network in Windows Settings; overrides the adapter.
    Profile,
    /// The settings could not be read.
    Unknown,
}

/// One unicast address of an adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressInfo {
    pub address: String,
    pub prefix_length: u8,
    pub origin: AddressOrigin,
    /// The address passed duplicate address detection and is in use.
    pub preferred: bool,
}

/// DNS servers of one address family of an adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsConfig {
    pub mode: DnsMode,
    /// The servers in use: the static ones, the Wi-Fi network's, or the ones obtained
    /// automatically (none while the adapter is not connected).
    pub servers: Vec<String>,
    /// Id of the preset whose servers these are ([`DnsMode::Manual`] only).
    pub preset: Option<String>,
    /// Servers set for the connected Wi-Fi network ([`DnsMode::Profile`] only).
    #[serde(default)]
    pub profile_servers: Vec<String>,
}

impl DnsConfig {
    /// Configuration of settings that were not read (yet).
    pub(crate) fn unknown() -> DnsConfig {
        DnsConfig {
            mode: DnsMode::Unknown,
            servers: Vec::new(),
            preset: None,
            profile_servers: Vec::new(),
        }
    }
}

/// One network adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Adapter {
    /// Canonical interface GUID: `{lowercase}`.
    pub id: String,
    /// Friendly name, such as "Wi-Fi".
    pub name: String,
    pub description: String,
    pub kind: AdapterKind,
    pub status: LinkStatus,
    /// Connected with IPv4 on, but holding only link-local IPv4 addresses (no DHCP answer).
    pub limited: bool,
    /// Backed by hardware rather than software.
    pub hardware: bool,
    /// A software adapter that is not connected; hidden unless every adapter is shown.
    pub minor: bool,
    /// The connected adapter the default IPv4 route most likely uses.
    pub primary: bool,
    pub if_index: u32,
    /// Dash-separated uppercase hardware address; empty when it has none.
    pub mac: String,
    pub mtu: u32,
    pub receive_bps: Option<u64>,
    pub transmit_bps: Option<u64>,
    pub dhcp_enabled: bool,
    pub ipv4_enabled: bool,
    pub ipv6_enabled: bool,
    pub ipv4: Vec<AddressInfo>,
    pub ipv6: Vec<AddressInfo>,
    pub gateways: Vec<String>,
    /// DNS servers in use, both families; empty while the adapter is not connected.
    pub dns_servers: Vec<String>,
    pub dns_ipv4: DnsConfig,
    pub dns_ipv6: DnsConfig,
    pub dns_suffix: String,
    pub ipv4_metric: u32,
    pub can_change_dns: bool,
    pub can_renew: bool,
    /// A DNS change of this adapter is recorded in the journal and can be undone.
    #[serde(default)]
    pub dns_revertible: bool,
    /// Why DNS cannot be changed here, or a caveat when it can.
    #[serde(default)]
    pub note: Option<String>,
    /// Problems met while this adapter was read; moved into the report's warnings.
    #[serde(skip)]
    pub(crate) read_warnings: Vec<String>,
}

impl Adapter {
    /// DNS configuration of `family`.
    pub fn dns(&self, family: IpFamily) -> &DnsConfig {
        match family {
            IpFamily::Ipv4 => &self.dns_ipv4,
            IpFamily::Ipv6 => &self.dns_ipv6,
        }
    }

    fn dns_mut(&mut self, family: IpFamily) -> &mut DnsConfig {
        match family {
            IpFamily::Ipv4 => &mut self.dns_ipv4,
            IpFamily::Ipv6 => &mut self.dns_ipv6,
        }
    }

    /// Whether the adapter has `family` turned on.
    pub fn family_enabled(&self, family: IpFamily) -> bool {
        match family {
            IpFamily::Ipv4 => self.ipv4_enabled,
            IpFamily::Ipv6 => self.ipv6_enabled,
        }
    }
}

/// The adapters of this PC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkReport {
    /// Connected adapters first, then the primary one, hardware ones and by name.
    pub adapters: Vec<Adapter>,
    /// DNS servers set by Group Policy for every adapter; adapter settings have no effect
    /// while it is set.
    #[serde(default)]
    pub dns_policy: Vec<String>,
    /// A VPN adapter is connected; DNS changes are refused on every adapter meanwhile.
    #[serde(default)]
    pub vpn_connected: bool,
    pub warnings: Vec<String>,
    pub duration_ms: u64,
}

/// One unicast address as the IP Helper API reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawUnicast {
    pub address: IpAddr,
    pub prefix_length: u8,
    pub prefix_origin: i32,
    pub suffix_origin: i32,
    pub dad_state: i32,
}

/// One adapter as the IP Helper API reports it, copied out of the API's buffers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RawAdapter {
    pub if_index: u32,
    /// Interface GUID as the API spells it (`{UPPERCASE}`).
    pub guid: String,
    pub name: String,
    pub description: String,
    pub dns_suffix: String,
    pub mac: Vec<u8>,
    pub flags: u32,
    pub mtu: u32,
    pub if_type: u32,
    pub oper_status: i32,
    pub tunnel_type: u32,
    pub transmit_bps: u64,
    pub receive_bps: u64,
    pub unicast: Vec<RawUnicast>,
    pub dns_servers: Vec<IpAddr>,
    pub gateways: Vec<IpAddr>,
    pub ipv4_metric: u32,
    pub luid: u64,
    /// `HardwareInterface` from the interface row.
    pub hardware: bool,
    /// `FilterInterface` from the interface row: a filter driver's layer, not an adapter.
    pub filter: bool,
    /// `NDIS_PHYSICAL_MEDIUM` value from the interface row.
    pub medium: u32,
    /// Why the interface row could not be read; `hardware`, `filter` and `medium` are then
    /// unknown (false, false, 0).
    pub link_error: Option<String>,
}

/// Whether an adapter's name or description marks it as a VPN client's adapter, whatever
/// interface type it reports.
pub(crate) fn names_vpn(name: &str, description: &str) -> bool {
    let text = format!("{name}\n{description}").to_ascii_lowercase();
    let description = description.trim().to_ascii_lowercase();
    VPN_MARKERS.iter().any(|m| text.contains(m))
        || (description.starts_with(BRANDED_TAP.0) && description.contains(BRANDED_TAP.1))
}

/// Kind of an adapter from its interface type, physical medium, hardware flag, tunnel type,
/// name and description. `None` for the loopback interface. The first matching rule wins.
pub(crate) fn classify(
    if_type: u32,
    medium: u32,
    hardware: bool,
    tunnel_type: u32,
    name: &str,
    description: &str,
) -> Option<AdapterKind> {
    let text = format!("{name}\n{description}").to_ascii_lowercase();
    let mentions = |markers: &[&str]| markers.iter().any(|m| text.contains(m));
    if if_type == IF_TYPE_LOOPBACK {
        return None;
    }
    if if_type == IF_TYPE_TUNNEL {
        return Some(AdapterKind::Tunnel);
    }
    if if_type == IF_TYPE_PROP_VIRTUAL || tunnel_type != 0 || names_vpn(name, description) {
        return Some(AdapterKind::Vpn);
    }
    if if_type == IF_TYPE_PPP {
        return Some(if mentions(&PPPOE_MARKERS) {
            AdapterKind::Other
        } else {
            AdapterKind::Vpn
        });
    }
    if if_type == IF_TYPE_WIFI {
        return Some(if hardware {
            AdapterKind::Wifi
        } else {
            AdapterKind::Virtual
        });
    }
    if IF_TYPE_WWAN.contains(&if_type) {
        return Some(AdapterKind::Cellular);
    }
    if medium == MEDIUM_BLUETOOTH {
        return Some(AdapterKind::Bluetooth);
    }
    if if_type == IF_TYPE_ETHERNET {
        return Some(if hardware {
            AdapterKind::Ethernet
        } else {
            AdapterKind::Virtual
        });
    }
    Some(AdapterKind::Other)
}

/// Link state from an `IF_OPER_STATUS` value.
pub(crate) fn link_status(oper_status: i32) -> LinkStatus {
    match oper_status {
        1 => LinkStatus::Connected,
        2 | 5 | 7 => LinkStatus::Disconnected,
        6 => LinkStatus::NotPresent,
        _ => LinkStatus::Unknown,
    }
}

/// Link speed in bits per second; `None` for the values that mean "unknown".
pub(crate) fn speed(bps: u64) -> Option<u64> {
    (bps != 0 && bps != u64::MAX).then_some(bps)
}

/// Hardware address as dash-separated uppercase hex (`00-00-5E-00-53-1A`).
pub(crate) fn format_mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join("-")
}

/// Whether `addr` is link-local: 169.254.0.0/16 or fe80::/10.
fn is_link_local(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.octets()[..2] == [169, 254],
        IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
    }
}

/// Origin of an address from its `NL_PREFIX_ORIGIN` and `NL_SUFFIX_ORIGIN` values. The
/// first matching rule wins.
pub(crate) fn origin(prefix_origin: i32, suffix_origin: i32, addr: &IpAddr) -> AddressOrigin {
    if is_link_local(addr) {
        AddressOrigin::LinkLocal
    } else if suffix_origin == 5 {
        AddressOrigin::Temporary
    } else if prefix_origin == 3 || suffix_origin == 3 {
        AddressOrigin::Dhcp
    } else if prefix_origin == 1 {
        AddressOrigin::Manual
    } else if prefix_origin == 4 {
        AddressOrigin::Autoconfigured
    } else {
        AddressOrigin::Other
    }
}

/// The DNS servers an adapter uses, without the placeholder IPv6 servers Windows lists
/// when it has none.
pub(crate) fn effective_dns(servers: &[IpAddr]) -> Vec<String> {
    let placeholders: Vec<IpAddr> = PLACEHOLDER_DNS
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();
    servers
        .iter()
        .filter(|s| !placeholders.contains(s))
        .map(ToString::to_string)
        .collect()
}

/// DNS configuration of `family` from the stored settings (`Err` when they could not be
/// read) and the servers the adapter uses (both families).
pub(crate) fn dns_config(
    stored: std::result::Result<&StaticDns, &Error>,
    effective: &[String],
    family: IpFamily,
) -> DnsConfig {
    let Ok(stored) = stored else {
        return DnsConfig::unknown();
    };
    let profile = split_servers(&stored.profile_servers);
    if !profile.is_empty() {
        return DnsConfig {
            mode: DnsMode::Profile,
            servers: profile.clone(),
            preset: None,
            profile_servers: profile,
        };
    }
    let servers = split_servers(&stored.servers);
    if servers.is_empty() {
        let servers = effective
            .iter()
            .filter(|s| {
                s.parse::<IpAddr>()
                    .is_ok_and(|ip| IpFamily::of(&ip) == family)
            })
            .cloned()
            .collect();
        return DnsConfig {
            mode: DnsMode::Automatic,
            servers,
            preset: None,
            profile_servers: Vec::new(),
        };
    }
    DnsConfig {
        mode: DnsMode::Manual,
        preset: preset_matching(&servers, family),
        servers,
        profile_servers: Vec::new(),
    }
}

/// Sets `can_change_dns`, `can_renew` and `note`. For DNS the first matching rule wins.
pub(crate) fn capabilities(adapter: &mut Adapter, vpn_connected: bool) {
    let modes = [adapter.dns_ipv4.mode, adapter.dns_ipv6.mode];
    let (can_change, note) = if adapter.status == LinkStatus::NotPresent {
        (false, Some(NOTE_NOT_PRESENT))
    } else if matches!(adapter.kind, AdapterKind::Vpn | AdapterKind::Tunnel) {
        (false, Some(NOTE_VPN_ADAPTER))
    } else if adapter.kind == AdapterKind::Other {
        (false, Some(NOTE_OTHER))
    } else if modes.contains(&DnsMode::Unknown) {
        (false, Some(NOTE_UNREADABLE))
    } else if modes.contains(&DnsMode::Profile) {
        (false, Some(NOTE_PROFILE))
    } else if vpn_connected {
        (false, Some(NOTE_VPN_CONNECTED))
    } else if adapter.kind == AdapterKind::Virtual {
        (true, Some(NOTE_VIRTUAL))
    } else {
        (true, None)
    };
    adapter.can_change_dns = can_change;
    adapter.note = note.map(str::to_string);
    adapter.can_renew = adapter.dhcp_enabled
        && adapter.ipv4_enabled
        && adapter.status == LinkStatus::Connected
        && matches!(
            adapter.kind,
            AdapterKind::Ethernet
                | AdapterKind::Wifi
                | AdapterKind::Virtual
                | AdapterKind::Bluetooth
        );
}

/// Connected with IPv4 on, but with only link-local IPv4 addresses.
fn is_limited(adapter: &Adapter) -> bool {
    adapter.status == LinkStatus::Connected
        && adapter.ipv4_enabled
        && !adapter.ipv4.is_empty()
        && adapter.ipv4.iter().all(|a| {
            a.address
                .parse::<IpAddr>()
                .is_ok_and(|ip| is_link_local(&ip))
        })
}

/// A software adapter that is not connected.
fn is_minor(adapter: &Adapter) -> bool {
    !adapter.hardware && adapter.status != LinkStatus::Connected
}

/// Marks at most one adapter primary: among the connected adapters with an IPv4 gateway,
/// the one with the lowest IPv4 metric, then the lowest interface index.
pub(crate) fn mark_primary(adapters: &mut [Adapter]) {
    let primary = adapters
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            a.status == LinkStatus::Connected
                && a.gateways
                    .iter()
                    .any(|g| g.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv4()))
        })
        .min_by_key(|(_, a)| (a.ipv4_metric, a.if_index))
        .map(|(i, _)| i);
    for (i, adapter) in adapters.iter_mut().enumerate() {
        adapter.primary = Some(i) == primary;
    }
}

/// Connected adapters first, then the primary one, hardware ones, then by name ignoring
/// case.
pub(crate) fn sort_adapters(adapters: &mut [Adapter]) {
    adapters.sort_by(|a, b| {
        let connected = |x: &Adapter| x.status == LinkStatus::Connected;
        connected(b)
            .cmp(&connected(a))
            .then(b.primary.cmp(&a.primary))
            .then(b.hardware.cmp(&a.hardware))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// The adapter a raw interface describes; `None` for the loopback interface and filter
/// interfaces. DNS settings, capabilities and the derived flags are filled in by
/// [`list_with`].
pub(crate) fn adapter_from_raw(raw: RawAdapter) -> Option<Adapter> {
    if raw.filter {
        return None;
    }
    let kind = classify(
        raw.if_type,
        raw.medium,
        raw.hardware,
        raw.tunnel_type,
        &raw.name,
        &raw.description,
    )?;
    let mut ipv4 = Vec::new();
    let mut ipv6 = Vec::new();
    for u in &raw.unicast {
        let info = AddressInfo {
            address: u.address.to_string(),
            prefix_length: u.prefix_length,
            origin: origin(u.prefix_origin, u.suffix_origin, &u.address),
            preferred: u.dad_state == DAD_PREFERRED,
        };
        match u.address {
            IpAddr::V4(_) => ipv4.push(info),
            IpAddr::V6(_) => ipv6.push(info),
        }
    }
    let read_warnings = raw
        .link_error
        .iter()
        .map(|e| format!("cannot read link details of {}: {e}", raw.name))
        .collect();
    Some(Adapter {
        id: canonical_guid(&raw.guid),
        description: raw.description,
        kind,
        status: link_status(raw.oper_status),
        limited: false,
        hardware: raw.hardware,
        minor: false,
        primary: false,
        if_index: raw.if_index,
        mac: format_mac(&raw.mac),
        mtu: raw.mtu,
        receive_bps: speed(raw.receive_bps),
        transmit_bps: speed(raw.transmit_bps),
        dhcp_enabled: raw.flags & FLAG_DHCP_ENABLED != 0,
        ipv4_enabled: raw.flags & FLAG_IPV4_ENABLED != 0,
        ipv6_enabled: raw.flags & FLAG_IPV6_ENABLED != 0,
        ipv4,
        ipv6,
        gateways: raw.gateways.iter().map(ToString::to_string).collect(),
        dns_servers: effective_dns(&raw.dns_servers),
        dns_ipv4: DnsConfig::unknown(),
        dns_ipv6: DnsConfig::unknown(),
        dns_suffix: raw.dns_suffix,
        ipv4_metric: raw.ipv4_metric,
        can_change_dns: false,
        can_renew: false,
        dns_revertible: false,
        note: None,
        read_warnings,
        name: raw.name,
    })
}

/// Lists the adapters of `stack` with their DNS configuration, capabilities and the flags
/// derived from them. An adapter that is not connected lists no servers in use: its
/// automatic configuration names none, while static and per-network servers stay listed.
/// With a journal, adapters with an active DNS record are marked revertible. Read-only.
pub(crate) fn list_with(stack: &dyn NetStack, journal: Option<&Journal>) -> Result<NetworkReport> {
    let started = Instant::now();
    let mut adapters = stack.adapters()?;
    let mut warnings: Vec<String> = Vec::new();
    for adapter in &mut adapters {
        warnings.append(&mut adapter.read_warnings);
        for family in IpFamily::ALL {
            let stored = stack.static_dns(&adapter.id, family);
            if let Err(e) = &stored {
                let warning = format!("cannot read the DNS settings of {}: {e}", adapter.name);
                if !warnings.contains(&warning) {
                    warnings.push(warning);
                }
            }
            let mut config = dns_config(stored.as_ref(), &adapter.dns_servers, family);
            if config.mode == DnsMode::Automatic && adapter.status != LinkStatus::Connected {
                // Without a link the effective servers are left over from an old lease and
                // are not in use; the adapter still gets its servers automatically.
                config.servers.clear();
            }
            *adapter.dns_mut(family) = config;
        }
        if adapter.status != LinkStatus::Connected {
            adapter.dns_servers.clear();
        }
        adapter.limited = is_limited(adapter);
        adapter.minor = is_minor(adapter);
    }
    let vpn_connected = adapters
        .iter()
        .any(|a| a.kind == AdapterKind::Vpn && a.status == LinkStatus::Connected);
    for adapter in &mut adapters {
        capabilities(adapter, vpn_connected);
    }
    if let Some(journal) = journal {
        match journal.active_dns() {
            Ok(records) => {
                for adapter in &mut adapters {
                    adapter.dns_revertible = records
                        .iter()
                        .any(|r| canonical_guid(&r.interface_guid) == adapter.id);
                }
            }
            Err(e) => warnings.push(format!("cannot read the recorded DNS changes: {e}")),
        }
    }
    let dns_policy = stack.dns_policy().unwrap_or_else(|e| {
        warnings.push(format!("cannot read the DNS client policy: {e}"));
        Vec::new()
    });
    mark_primary(&mut adapters);
    sort_adapters(&mut adapters);
    Ok(NetworkReport {
        adapters,
        dns_policy,
        vpn_connected,
        warnings,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::network::tests::{adapter, guid, FakeStack};
    use crate::safety::state_log::NewDnsRecord;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn stored(servers: &str, profile: &str) -> StaticDns {
        StaticDns {
            servers: servers.into(),
            profile_servers: profile.into(),
        }
    }

    #[test]
    fn classify_maps_interface_types() {
        let kind = |if_type, medium, hardware| classify(if_type, medium, hardware, 0, "x", "y");
        assert_eq!(kind(6, 0, true), Some(AdapterKind::Ethernet));
        assert_eq!(kind(6, 0, false), Some(AdapterKind::Virtual));
        assert_eq!(kind(71, 9, true), Some(AdapterKind::Wifi));
        assert_eq!(kind(243, 8, true), Some(AdapterKind::Cellular));
        assert_eq!(kind(244, 8, true), Some(AdapterKind::Cellular));
        assert_eq!(kind(6, 10, true), Some(AdapterKind::Bluetooth));
        assert_eq!(kind(53, 0, false), Some(AdapterKind::Vpn));
        assert_eq!(kind(131, 0, false), Some(AdapterKind::Tunnel));
        assert_eq!(kind(24, 0, false), None, "loopback is not listed");
        assert_eq!(kind(1, 0, true), Some(AdapterKind::Other));
        assert_eq!(kind(144, 0, true), Some(AdapterKind::Other));
    }

    #[test]
    fn wifi_direct_is_virtual() {
        assert_eq!(
            classify(
                71,
                9,
                false,
                0,
                "Local Area Connection* 1",
                "Microsoft Wi-Fi Direct Virtual Adapter"
            ),
            Some(AdapterKind::Virtual)
        );
    }

    #[test]
    fn tap_wintun_wireguard_openvpn_are_vpn() {
        for (name, description) in [
            ("Ethernet 3", "TAP-Windows Adapter V9"),
            ("OpenVPN Wintun", "Wintun Userspace Tunnel"),
            ("WireGuard Tunnel", "WireGuard Tunnel"),
            ("Ethernet 4", "OpenVPN Data Channel Offload"),
            // Vendor rebuilds of the TAP driver name the product between "TAP-" and "Windows".
            ("Ethernet 5", "TAP-NordVPN Windows Adapter V9"),
            ("Ethernet 6", "TAP-ProtonVPN Windows Adapter V9"),
            ("Ethernet 7", "TAP-Surfshark Windows Adapter V9"),
            ("Local Area Connection", "TAP-Win32 Adapter V9"),
            ("Ethernet 8", "PANGP Virtual Ethernet Adapter Secure"),
            (
                "Ethernet 9",
                "Cisco AnyConnect Secure Mobility Client Virtual Miniport Adapter for Windows x64",
            ),
            (
                "Ethernet 10",
                "Cisco Secure Client Virtual Miniport Adapter for Windows x64",
            ),
            ("Ethernet 11", "Fortinet SSL VPN Virtual Ethernet Adapter"),
        ] {
            assert_eq!(
                classify(6, 0, false, 0, name, description),
                Some(AdapterKind::Vpn),
                "{name} / {description}"
            );
            assert_eq!(
                classify(6, 0, true, 0, name, description),
                Some(AdapterKind::Vpn),
                "hardware flag does not matter for {description}"
            );
        }
        assert_eq!(
            classify(6, 0, false, 2, "Company Link", "Some tunnel"),
            Some(AdapterKind::Vpn),
            "a tunnel type marks a VPN"
        );
    }

    #[test]
    fn virtual_and_physical_adapters_are_not_taken_for_vpns() {
        for (if_type, hardware, name, description, kind) in [
            (
                6,
                false,
                "vEthernet (Default Switch)",
                "Hyper-V Virtual Ethernet Adapter",
                AdapterKind::Virtual,
            ),
            (
                71,
                false,
                "Local Area Connection* 1",
                "Microsoft Wi-Fi Direct Virtual Adapter",
                AdapterKind::Virtual,
            ),
            (
                6,
                false,
                "VirtualBox Host-Only Network",
                "VirtualBox Host-Only Ethernet Adapter",
                AdapterKind::Virtual,
            ),
            (
                6,
                true,
                "Ethernet",
                "Fabrikam 2.5GbE Controller",
                AdapterKind::Ethernet,
            ),
            (
                71,
                true,
                "Wi-Fi",
                "Contoso Wi-Fi 6E Adapter",
                AdapterKind::Wifi,
            ),
            (
                6,
                true,
                "Ethernet 2",
                "TAP adapter test fixture",
                AdapterKind::Ethernet,
            ),
        ] {
            assert_eq!(
                classify(if_type, 0, hardware, 0, name, description),
                Some(kind),
                "{name} / {description}"
            );
            assert!(!names_vpn(name, description), "{description}");
        }
    }

    #[test]
    fn teredo_is_tunnel_not_vpn() {
        assert_eq!(
            classify(
                131,
                0,
                false,
                14,
                "Teredo Tunneling Pseudo-Interface",
                "Microsoft Teredo Tunneling Adapter"
            ),
            Some(AdapterKind::Tunnel)
        );

        let mut teredo = adapter(1, "Teredo", AdapterKind::Tunnel);
        teredo.gateways.clear();
        let wifi = adapter(2, "Wi-Fi", AdapterKind::Wifi);
        let stack = FakeStack::with(vec![teredo, wifi]);
        let report = list_with(&stack, None).unwrap();
        assert!(!report.vpn_connected, "a connected tunnel is not a VPN");
        let wifi = report.adapters.iter().find(|a| a.name == "Wi-Fi").unwrap();
        assert!(wifi.can_change_dns);
        let teredo = report.adapters.iter().find(|a| a.name == "Teredo").unwrap();
        assert!(!teredo.can_change_dns);
        assert_eq!(teredo.note.as_deref(), Some(NOTE_VPN_ADAPTER));
    }

    #[test]
    fn pppoe_is_other_and_ppp_is_vpn() {
        assert_eq!(
            classify(
                23,
                0,
                false,
                0,
                "Broadband Connection",
                "WAN Miniport (PPPOE)"
            ),
            Some(AdapterKind::Other)
        );
        assert_eq!(
            classify(23, 0, false, 0, "Office", "WAN Miniport (PPTP)"),
            Some(AdapterKind::Vpn)
        );
    }

    #[test]
    fn link_status_maps_oper_status() {
        assert_eq!(link_status(1), LinkStatus::Connected);
        for down in [2, 5, 7] {
            assert_eq!(link_status(down), LinkStatus::Disconnected, "{down}");
        }
        assert_eq!(link_status(6), LinkStatus::NotPresent);
        for other in [0, 3, 4, 8, -1] {
            assert_eq!(link_status(other), LinkStatus::Unknown, "{other}");
        }
    }

    #[test]
    fn speed_unknown_values_are_none() {
        assert_eq!(speed(0), None);
        assert_eq!(speed(u64::MAX), None);
        assert_eq!(speed(1_000_000_000), Some(1_000_000_000));
    }

    #[test]
    fn mac_is_dash_separated_uppercase() {
        assert_eq!(
            format_mac(&[0x00, 0x00, 0x5e, 0x00, 0x53, 0x1a]),
            "00-00-5E-00-53-1A"
        );
        assert_eq!(format_mac(&[]), "");
    }

    #[test]
    fn origin_classifies_dhcp_manual_temporary_and_link_local() {
        let v4 = ip("192.168.0.23");
        let v6 = ip("2001:db8::23");
        assert_eq!(origin(3, 3, &v4), AddressOrigin::Dhcp);
        assert_eq!(origin(4, 3, &v6), AddressOrigin::Dhcp, "DHCPv6 suffix");
        assert_eq!(origin(1, 1, &v4), AddressOrigin::Manual);
        assert_eq!(origin(4, 5, &v6), AddressOrigin::Temporary);
        assert_eq!(origin(4, 4, &v6), AddressOrigin::Autoconfigured);
        assert_eq!(origin(2, 4, &ip("169.254.10.1")), AddressOrigin::LinkLocal);
        assert_eq!(origin(2, 4, &ip("fe80::1")), AddressOrigin::LinkLocal);
        assert_eq!(origin(1, 1, &ip("fe80::1")), AddressOrigin::LinkLocal);
        assert_eq!(origin(0, 0, &v4), AddressOrigin::Other);
    }

    #[test]
    fn placeholder_ipv6_dns_servers_are_dropped() {
        let servers = [
            ip("192.168.0.1"),
            ip("fec0:0:0:ffff::1"),
            ip("fec0::ffff:0:0:2"),
            ip("fec0:0:0:ffff::3"),
            ip("2606:4700:4700::1111"),
        ];
        assert_eq!(
            effective_dns(&servers),
            vec!["192.168.0.1", "fec0::ffff:0:0:2", "2606:4700:4700::1111"]
        );
    }

    #[test]
    fn dns_config_reports_automatic_manual_profile_unknown_and_preset() {
        let effective = vec![
            "192.168.0.1".to_string(),
            "198.51.100.53".to_string(),
            "2001:db8::53".to_string(),
        ];

        let auto = dns_config(Ok(&stored("", "")), &effective, IpFamily::Ipv4);
        assert_eq!(auto.mode, DnsMode::Automatic);
        assert_eq!(auto.servers, vec!["192.168.0.1", "198.51.100.53"]);
        assert_eq!(auto.preset, None);
        let auto6 = dns_config(Ok(&stored("", "")), &effective, IpFamily::Ipv6);
        assert_eq!(auto6.servers, vec!["2001:db8::53"]);

        let manual = dns_config(
            Ok(&stored("1.1.1.1,1.0.0.1", "")),
            &effective,
            IpFamily::Ipv4,
        );
        assert_eq!(manual.mode, DnsMode::Manual);
        assert_eq!(manual.servers, vec!["1.1.1.1", "1.0.0.1"]);
        assert_eq!(manual.preset.as_deref(), Some("cloudflare"));
        assert!(manual.profile_servers.is_empty());

        let custom = dns_config(Ok(&stored("10.0.0.53", "")), &effective, IpFamily::Ipv4);
        assert_eq!(custom.mode, DnsMode::Manual);
        assert_eq!(custom.preset, None);

        let v6 = dns_config(
            Ok(&stored("2606:4700:4700:0::1111 2606:4700:4700::1001", "")),
            &effective,
            IpFamily::Ipv6,
        );
        assert_eq!(
            v6.preset.as_deref(),
            Some("cloudflare"),
            "compared as addresses"
        );

        let profile = dns_config(
            Ok(&stored("8.8.8.8", "9.9.9.9")),
            &effective,
            IpFamily::Ipv4,
        );
        assert_eq!(profile.mode, DnsMode::Profile);
        assert_eq!(profile.servers, vec!["9.9.9.9"]);
        assert_eq!(profile.profile_servers, vec!["9.9.9.9"]);
        assert_eq!(profile.preset, None);

        let err = Error::Other("Access is denied.".into());
        let unknown = dns_config(Err(&err), &effective, IpFamily::Ipv4);
        assert_eq!(unknown, DnsConfig::unknown());
    }

    fn with_modes(kind: AdapterKind, v4: DnsMode, v6: DnsMode) -> Adapter {
        let mut a = adapter(1, "Test", kind);
        a.dns_ipv4.mode = v4;
        a.dns_ipv6.mode = v6;
        a
    }

    fn capable(mut a: Adapter, vpn_connected: bool) -> (bool, Option<String>) {
        capabilities(&mut a, vpn_connected);
        (a.can_change_dns, a.note)
    }

    #[test]
    fn capabilities_refuse_vpn_tunnel_not_present_unreadable_and_profile() {
        use DnsMode::{Automatic, Manual, Profile, Unknown};
        let refused = |a: Adapter, note: &str| {
            assert_eq!(capable(a, false), (false, Some(note.to_string())));
        };
        let mut gone = with_modes(AdapterKind::Ethernet, Automatic, Automatic);
        gone.status = LinkStatus::NotPresent;
        refused(gone, NOTE_NOT_PRESENT);
        let mut gone_vpn = with_modes(AdapterKind::Vpn, Unknown, Unknown);
        gone_vpn.status = LinkStatus::NotPresent;
        refused(gone_vpn, NOTE_NOT_PRESENT);
        refused(
            with_modes(AdapterKind::Vpn, Automatic, Automatic),
            NOTE_VPN_ADAPTER,
        );
        refused(
            with_modes(AdapterKind::Tunnel, Automatic, Automatic),
            NOTE_VPN_ADAPTER,
        );
        refused(
            with_modes(AdapterKind::Other, Manual, Automatic),
            NOTE_OTHER,
        );
        refused(
            with_modes(AdapterKind::Wifi, Automatic, Unknown),
            NOTE_UNREADABLE,
        );
        refused(
            with_modes(AdapterKind::Wifi, Unknown, Profile),
            NOTE_UNREADABLE,
        );
        refused(
            with_modes(AdapterKind::Wifi, Profile, Automatic),
            NOTE_PROFILE,
        );
        refused(with_modes(AdapterKind::Wifi, Manual, Profile), NOTE_PROFILE);

        assert_eq!(
            capable(with_modes(AdapterKind::Ethernet, Manual, Automatic), false),
            (true, None)
        );
        assert_eq!(
            capable(
                with_modes(AdapterKind::Cellular, Automatic, Automatic),
                false
            ),
            (true, None)
        );
    }

    #[test]
    fn renew_needs_a_connected_dhcp_adapter_of_a_renewable_kind() {
        let renewable = |a: &mut Adapter| {
            capabilities(a, false);
            a.can_renew
        };
        let mut wifi = adapter(1, "Wi-Fi", AdapterKind::Wifi);
        assert!(renewable(&mut wifi));
        let mut virt = adapter(2, "vEthernet", AdapterKind::Virtual);
        assert!(renewable(&mut virt));
        let mut bt = adapter(3, "Bluetooth", AdapterKind::Bluetooth);
        assert!(renewable(&mut bt));
        let mut static_ip = adapter(4, "Ethernet", AdapterKind::Ethernet);
        static_ip.dhcp_enabled = false;
        assert!(!renewable(&mut static_ip));
        let mut no_v4 = adapter(5, "Ethernet 2", AdapterKind::Ethernet);
        no_v4.ipv4_enabled = false;
        assert!(!renewable(&mut no_v4));
        let mut down = adapter(6, "Ethernet 3", AdapterKind::Ethernet);
        down.status = LinkStatus::Disconnected;
        assert!(!renewable(&mut down));
        let mut vpn = adapter(7, "VPN", AdapterKind::Vpn);
        assert!(!renewable(&mut vpn));
        let mut cell = adapter(8, "Cellular", AdapterKind::Cellular);
        assert!(!renewable(&mut cell));
    }

    #[test]
    fn vpn_connected_blocks_dns_changes_on_other_adapters() {
        let vpn = adapter(1, "ProtonVPN", AdapterKind::Vpn);
        let wifi = adapter(2, "Wi-Fi", AdapterKind::Wifi);
        let virt = adapter(3, "vEthernet", AdapterKind::Virtual);
        let stack = FakeStack::with(vec![vpn.clone(), wifi, virt]);
        let report = list_with(&stack, None).unwrap();
        assert!(report.vpn_connected);
        for a in &report.adapters {
            assert!(!a.can_change_dns, "{}", a.name);
        }
        let wifi = report.adapters.iter().find(|a| a.name == "Wi-Fi").unwrap();
        assert_eq!(wifi.note.as_deref(), Some(NOTE_VPN_CONNECTED));
        let virt = report
            .adapters
            .iter()
            .find(|a| a.name == "vEthernet")
            .unwrap();
        assert_eq!(virt.note.as_deref(), Some(NOTE_VPN_CONNECTED));

        let mut idle_vpn = vpn;
        idle_vpn.status = LinkStatus::Disconnected;
        let stack = FakeStack::with(vec![idle_vpn, adapter(2, "Wi-Fi", AdapterKind::Wifi)]);
        let report = list_with(&stack, None).unwrap();
        assert!(!report.vpn_connected, "a disconnected VPN pauses nothing");
        let wifi = report.adapters.iter().find(|a| a.name == "Wi-Fi").unwrap();
        assert!(wifi.can_change_dns);
        assert_eq!(wifi.note, None);
    }

    #[test]
    fn virtual_adapter_is_changeable_with_note() {
        assert_eq!(
            capable(
                with_modes(AdapterKind::Virtual, DnsMode::Automatic, DnsMode::Manual),
                false
            ),
            (true, Some(NOTE_VIRTUAL.to_string()))
        );
    }

    #[test]
    fn primary_is_lowest_metric_connected_adapter_with_gateway() {
        let mut wifi = adapter(12, "Wi-Fi", AdapterKind::Wifi);
        wifi.ipv4_metric = 35;
        let mut ethernet = adapter(7, "Ethernet", AdapterKind::Ethernet);
        ethernet.ipv4_metric = 25;
        let mut idle = adapter(3, "Ethernet 2", AdapterKind::Ethernet);
        idle.ipv4_metric = 5;
        idle.status = LinkStatus::Disconnected;
        let mut no_gateway = adapter(4, "vEthernet", AdapterKind::Virtual);
        no_gateway.ipv4_metric = 1;
        no_gateway.gateways.clear();
        let mut v6_gateway = adapter(5, "Ethernet 3", AdapterKind::Ethernet);
        v6_gateway.ipv4_metric = 2;
        v6_gateway.gateways = vec!["fe80::1".into()];
        let mut adapters = vec![wifi, ethernet, idle, no_gateway, v6_gateway];
        mark_primary(&mut adapters);
        let primaries: Vec<&str> = adapters
            .iter()
            .filter(|a| a.primary)
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(primaries, vec!["Ethernet"]);

        let mut a = adapter(9, "B", AdapterKind::Ethernet);
        let mut b = adapter(8, "A", AdapterKind::Ethernet);
        a.ipv4_metric = 10;
        b.ipv4_metric = 10;
        let mut tie = vec![a, b];
        mark_primary(&mut tie);
        assert!(
            tie[1].primary,
            "equal metrics: the lower interface index wins"
        );
        assert!(!tie[0].primary);

        let mut none = vec![idle_adapter()];
        mark_primary(&mut none);
        assert!(!none[0].primary);
    }

    fn idle_adapter() -> Adapter {
        let mut a = adapter(20, "Idle", AdapterKind::Ethernet);
        a.status = LinkStatus::Disconnected;
        a
    }

    #[test]
    fn minor_hides_idle_virtual_adapters() {
        let mut direct = adapter(1, "Local Area Connection* 1", AdapterKind::Virtual);
        direct.status = LinkStatus::Disconnected;
        direct.hardware = false;
        let mut switch = adapter(2, "vEthernet (Default Switch)", AdapterKind::Virtual);
        switch.hardware = false;
        let stack = FakeStack::with(vec![direct, switch, idle_adapter()]);
        let report = list_with(&stack, None).unwrap();
        let minor = |name: &str| {
            report
                .adapters
                .iter()
                .find(|a| a.name == name)
                .unwrap()
                .minor
        };
        assert!(minor("Local Area Connection* 1"));
        assert!(!minor("vEthernet (Default Switch)"), "connected");
        assert!(!minor("Idle"), "hardware");
    }

    #[test]
    fn limited_means_only_link_local_ipv4() {
        let mut limited = adapter(1, "Ethernet", AdapterKind::Ethernet);
        limited.ipv4[0].address = "169.254.33.2".into();
        let mut fine = adapter(2, "Wi-Fi", AdapterKind::Wifi);
        fine.ipv4.push(AddressInfo {
            address: "169.254.1.1".into(),
            prefix_length: 16,
            origin: AddressOrigin::LinkLocal,
            preferred: true,
        });
        let stack = FakeStack::with(vec![limited, fine]);
        let report = list_with(&stack, None).unwrap();
        let get = |name: &str| report.adapters.iter().find(|a| a.name == name).unwrap();
        assert!(get("Ethernet").limited);
        assert!(!get("Wi-Fi").limited);
    }

    #[test]
    fn sort_puts_connected_primary_first() {
        let mut zeta = adapter(1, "zeta", AdapterKind::Ethernet);
        zeta.status = LinkStatus::Disconnected;
        let mut beta = adapter(2, "Beta", AdapterKind::Virtual);
        beta.hardware = false;
        let mut primary = adapter(3, "Wi-Fi", AdapterKind::Wifi);
        primary.primary = true;
        let alpha = adapter(4, "alpha", AdapterKind::Ethernet);
        let mut adapters = vec![zeta, beta, alpha, primary];
        sort_adapters(&mut adapters);
        let names: Vec<&str> = adapters.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["Wi-Fi", "alpha", "Beta", "zeta"]);
    }

    #[test]
    fn list_marks_revertible_adapters_from_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        let session = journal.begin_session("dns", "test").unwrap();
        journal
            .record_dns(
                session,
                &NewDnsRecord {
                    interface_guid: guid(2).to_uppercase(),
                    family: IpFamily::Ipv6,
                    adapter_name: "Ethernet".into(),
                    previous_servers: String::new(),
                    target_servers: "2606:4700:4700::1111".into(),
                },
            )
            .unwrap();
        let stack = FakeStack::with(vec![
            adapter(1, "Wi-Fi", AdapterKind::Wifi),
            adapter(2, "Ethernet", AdapterKind::Ethernet),
        ]);
        let report = list_with(&stack, Some(&journal)).unwrap();
        let revertible = |name: &str| {
            report
                .adapters
                .iter()
                .find(|a| a.name == name)
                .unwrap()
                .dns_revertible
        };
        assert!(revertible("Ethernet"));
        assert!(!revertible("Wi-Fi"));

        let report = list_with(&stack, None).unwrap();
        assert!(report.adapters.iter().all(|a| !a.dns_revertible));
    }

    #[test]
    fn list_reports_unreadable_dns_policy_and_link_warnings() {
        let mut wifi = adapter(1, "Wi-Fi", AdapterKind::Wifi);
        wifi.read_warnings
            .push("cannot read link details of Wi-Fi: Element not found.".into());
        let stack = FakeStack::with(vec![wifi, adapter(2, "Ethernet", AdapterKind::Ethernet)]);
        stack.unreadable.borrow_mut().insert(guid(2));
        stack.set_static(&guid(1), IpFamily::Ipv4, "1.1.1.1,1.0.0.1");
        *stack.policy.borrow_mut() = vec!["10.0.0.53".into()];
        let report = list_with(&stack, None).unwrap();
        assert_eq!(
            report.warnings,
            vec![
                "cannot read link details of Wi-Fi: Element not found.".to_string(),
                "cannot read the DNS settings of Ethernet: Access is denied.".to_string(),
            ]
        );
        assert_eq!(report.dns_policy, vec!["10.0.0.53"]);
        let ethernet = report
            .adapters
            .iter()
            .find(|a| a.name == "Ethernet")
            .unwrap();
        assert_eq!(ethernet.dns_ipv4.mode, DnsMode::Unknown);
        assert_eq!(ethernet.note.as_deref(), Some(NOTE_UNREADABLE));
        let wifi = report.adapters.iter().find(|a| a.name == "Wi-Fi").unwrap();
        assert_eq!(wifi.dns_ipv4.mode, DnsMode::Manual);
        assert_eq!(wifi.dns_ipv4.preset.as_deref(), Some("cloudflare"));
        assert_eq!(wifi.dns_ipv6.mode, DnsMode::Automatic);
        assert!(wifi.read_warnings.is_empty());
    }

    fn raw(if_type: u32, name: &str) -> RawAdapter {
        RawAdapter {
            if_index: 12,
            guid: "{AAAAAAAA-0000-0000-0000-00000000000C}".into(),
            name: name.into(),
            description: format!("{name} adapter"),
            dns_suffix: "home".into(),
            mac: vec![0, 0, 0x5e, 0, 0x53, 0xab],
            flags: FLAG_DHCP_ENABLED | FLAG_IPV4_ENABLED | FLAG_IPV6_ENABLED,
            mtu: 1500,
            if_type,
            oper_status: 1,
            tunnel_type: 0,
            transmit_bps: 866_700_000,
            receive_bps: u64::MAX,
            unicast: vec![
                RawUnicast {
                    address: ip("fe80::1234"),
                    prefix_length: 64,
                    prefix_origin: 2,
                    suffix_origin: 4,
                    dad_state: DAD_PREFERRED,
                },
                RawUnicast {
                    address: ip("192.168.0.23"),
                    prefix_length: 24,
                    prefix_origin: 3,
                    suffix_origin: 3,
                    dad_state: DAD_PREFERRED,
                },
            ],
            dns_servers: vec![ip("192.168.0.1"), ip("fec0:0:0:ffff::1")],
            gateways: vec![ip("192.168.0.1")],
            ipv4_metric: 35,
            luid: 1,
            hardware: true,
            filter: false,
            medium: 9,
            link_error: None,
        }
    }

    #[test]
    fn raw_adapters_are_copied_and_filtered() {
        let a = adapter_from_raw(raw(71, "Wi-Fi")).unwrap();
        assert_eq!(a.id, "{aaaaaaaa-0000-0000-0000-00000000000c}");
        assert_eq!(a.kind, AdapterKind::Wifi);
        assert_eq!(a.status, LinkStatus::Connected);
        assert_eq!(a.mac, "00-00-5E-00-53-AB");
        assert_eq!(a.transmit_bps, Some(866_700_000));
        assert_eq!(a.receive_bps, None);
        assert!(a.dhcp_enabled && a.ipv4_enabled && a.ipv6_enabled);
        assert_eq!(a.ipv4.len(), 1);
        assert_eq!(a.ipv4[0].address, "192.168.0.23");
        assert_eq!(a.ipv4[0].origin, AddressOrigin::Dhcp);
        assert!(a.ipv4[0].preferred);
        assert_eq!(a.ipv6[0].origin, AddressOrigin::LinkLocal);
        assert_eq!(a.gateways, vec!["192.168.0.1"]);
        assert_eq!(a.dns_servers, vec!["192.168.0.1"]);
        assert_eq!(a.dns_suffix, "home");
        assert!(a.read_warnings.is_empty());

        let mut filter = raw(6, "Ethernet-WFP Native MAC Layer LightWeight Filter-0000");
        filter.filter = true;
        assert!(adapter_from_raw(filter).is_none());
        assert!(adapter_from_raw(raw(24, "Loopback Pseudo-Interface 1")).is_none());

        let mut unreadable = raw(6, "Ethernet");
        unreadable.hardware = false;
        unreadable.link_error = Some("Element not found.".into());
        let a = adapter_from_raw(unreadable).unwrap();
        assert_eq!(a.kind, AdapterKind::Virtual);
        assert_eq!(
            a.read_warnings,
            vec!["cannot read link details of Ethernet: Element not found."]
        );
    }

    #[test]
    fn a_connected_branded_tap_adapter_pauses_dns_changes() {
        let mut tap = raw(6, "Ethernet 5");
        tap.description = "TAP-NordVPN Windows Adapter V9".into();
        tap.hardware = false;
        tap.medium = 0;
        tap.gateways.clear();
        let tap = adapter_from_raw(tap).unwrap();
        assert_eq!(tap.kind, AdapterKind::Vpn);
        let stack = FakeStack::with(vec![tap, adapter(2, "Wi-Fi", AdapterKind::Wifi)]);
        let report = list_with(&stack, None).unwrap();
        assert!(report.vpn_connected);
        let wifi = report.adapters.iter().find(|a| a.name == "Wi-Fi").unwrap();
        assert!(!wifi.can_change_dns);
        assert_eq!(wifi.note.as_deref(), Some(NOTE_VPN_CONNECTED));
        let tap = report
            .adapters
            .iter()
            .find(|a| a.name == "Ethernet 5")
            .unwrap();
        assert_eq!(tap.note.as_deref(), Some(NOTE_VPN_ADAPTER));
    }

    #[test]
    fn disconnected_adapters_list_no_leftover_automatic_servers() {
        let mut ethernet = adapter(1, "Ethernet", AdapterKind::Ethernet);
        ethernet.status = LinkStatus::Disconnected;
        ethernet.dns_servers = vec![
            "192.168.0.1".into(),
            "198.51.100.53".into(),
            "2001:db8::53".into(),
        ];
        let mut dock = adapter(2, "Ethernet 2", AdapterKind::Ethernet);
        dock.status = LinkStatus::Disconnected;
        dock.dns_servers = vec!["10.0.0.1".into()];
        let mut gone = adapter(3, "Ethernet 3", AdapterKind::Ethernet);
        gone.status = LinkStatus::NotPresent;
        let wifi = adapter(4, "Wi-Fi", AdapterKind::Wifi);
        let stack = FakeStack::with(vec![ethernet, dock, gone, wifi]);
        stack.set_static(&guid(2), IpFamily::Ipv4, "1.1.1.1,1.0.0.1");
        stack
            .profile_dns
            .borrow_mut()
            .insert((guid(2), IpFamily::Ipv6), "2620:fe::fe".into());
        let report = list_with(&stack, None).unwrap();
        let get = |name: &str| report.adapters.iter().find(|a| a.name == name).unwrap();

        let ethernet = get("Ethernet");
        for family in IpFamily::ALL {
            let config = ethernet.dns(family);
            assert_eq!(config.mode, DnsMode::Automatic, "{family:?}");
            assert!(
                config.servers.is_empty(),
                "{family:?}: {:?}",
                config.servers
            );
        }
        assert!(ethernet.dns_servers.is_empty());
        assert!(
            get("Ethernet 3").dns_ipv4.servers.is_empty(),
            "not present counts as not connected"
        );

        // Configured servers are still named: they are what the adapter uses once connected.
        let dock = get("Ethernet 2");
        assert_eq!(dock.dns_ipv4.mode, DnsMode::Manual);
        assert_eq!(dock.dns_ipv4.servers, vec!["1.1.1.1", "1.0.0.1"]);
        assert_eq!(dock.dns_ipv4.preset.as_deref(), Some("cloudflare"));
        assert_eq!(dock.dns_ipv6.mode, DnsMode::Profile);
        assert_eq!(dock.dns_ipv6.servers, vec!["2620:fe::fe"]);

        let wifi = get("Wi-Fi");
        assert_eq!(wifi.dns_ipv4.servers, vec!["192.168.0.1"], "connected");
        assert_eq!(wifi.dns_servers, vec!["192.168.0.1"]);
    }

    #[test]
    fn report_serializes_with_the_documented_keys() {
        let stack = FakeStack::with(vec![adapter(1, "Wi-Fi", AdapterKind::Wifi)]);
        let report = list_with(&stack, None).unwrap();
        let json = serde_json::to_value(&report).unwrap();
        let adapter = &json["adapters"][0];
        for key in [
            "id",
            "name",
            "description",
            "kind",
            "status",
            "limited",
            "hardware",
            "minor",
            "primary",
            "if_index",
            "mac",
            "mtu",
            "receive_bps",
            "transmit_bps",
            "dhcp_enabled",
            "ipv4_enabled",
            "ipv6_enabled",
            "ipv4",
            "ipv6",
            "gateways",
            "dns_servers",
            "dns_ipv4",
            "dns_ipv6",
            "dns_suffix",
            "ipv4_metric",
            "can_change_dns",
            "can_renew",
            "dns_revertible",
            "note",
        ] {
            assert!(adapter.get(key).is_some(), "missing {key}");
        }
        assert!(adapter.get("read_warnings").is_none());
        assert_eq!(adapter["kind"], "wifi");
        assert_eq!(adapter["status"], "connected");
        assert_eq!(adapter["dns_ipv4"]["mode"], "automatic");
        assert_eq!(adapter["ipv4"][0]["origin"], "dhcp");
        for key in [
            "adapters",
            "dns_policy",
            "vpn_connected",
            "warnings",
            "duration_ms",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        let back: NetworkReport = serde_json::from_value(json).unwrap();
        assert_eq!(back.adapters[0].id, guid(1));
    }
}
