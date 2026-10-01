//! Network adapters and their DNS configuration. DNS server changes are journaled per
//! interface and address family and revert like every other record; flushing the resolver
//! cache, renewing a DHCP lease and resetting the stack are irreversible and only logged.
//! Every setting here is machine-wide, so no per-user check applies.
//!
//! - [`adapters`]     the adapter list, classification and what may be changed
//! - [`dns`]          DNS presets, requests, the journaled change and its restore
//! - [`maintenance`]  resolver cache flush, DHCP lease renewal and the stack reset
//! - `iphelper`       the IP Helper API behind `NetStack` (all unsafe code)
//! - `runner`         console programs run with a deadline (`netsh`)
//!
//! Everything that reaches the system goes through the `NetStack` and `CommandRunner`
//! seams, so every mutating path is tested against fakes.

pub mod adapters;
pub mod dns;
mod iphelper;
pub mod maintenance;
mod runner;
mod types;

pub use adapters::{
    Adapter, AdapterKind, AddressInfo, AddressOrigin, DnsConfig, DnsMode, LinkStatus, NetworkReport,
};
pub use dns::{
    parse_servers, same_servers, ChangeOutcome, DnsChange, DnsChoice, DnsPreset, DnsReport,
    DnsRequest, PRESETS,
};
pub use maintenance::{
    FlushReport, ManualSetting, RenewReport, ResetReport, ResetStep, StepStatus,
};
pub use types::{canonical_guid, dns_target, servers_text, split_servers, DnsRestore, IpFamily};

use crate::safety::state_log::{DnsRecord, Journal};
use crate::safety::Safety;
use crate::win::paths::system_dir;
use crate::{Error, Result};
use iphelper::IpHelper;
use runner::SystemRunner;

/// Audit log operation of a resolver cache flush, and its target.
pub(crate) const OP_FLUSH: &str = "flush_dns_cache";
pub(crate) const FLUSH_TARGET: &str = "DNS resolver cache";

/// Static DNS settings of one interface and address family, as Windows stores them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StaticDns {
    /// The adapter's own servers; `""` when they are obtained automatically.
    pub servers: String,
    /// Servers set for the connected Wi-Fi network in Windows Settings; `""` when none.
    pub profile_servers: String,
}

/// TCP/IP settings Windows keeps in the registry for one interface, whether or not its
/// adapter is listed now (an unplugged, disabled or removed adapter keeps them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StoredInterface {
    /// Canonical interface GUID.
    pub guid: String,
    /// Connection name, such as "Ethernet 2"; empty when Windows keeps none.
    pub name: String,
    /// Driver description and component id of the adapter; empty when unknown.
    pub description: String,
    pub component_id: String,
    /// DHCP is off (`EnableDHCP` 0), so `addresses` are set by hand.
    pub static_ipv4: bool,
    /// `IPAddress` and `SubnetMask`, paired by position.
    pub addresses: Vec<String>,
    pub masks: Vec<String>,
    /// `DefaultGateway`.
    pub gateways: Vec<String>,
    /// `NameServer` of IPv4 and IPv6; `""` when the servers are obtained automatically.
    pub ipv4_dns: String,
    pub ipv6_dns: String,
}

impl StoredInterface {
    /// The static DNS servers of `family`.
    pub(crate) fn dns(&self, family: IpFamily) -> &str {
        match family {
            IpFamily::Ipv4 => &self.ipv4_dns,
            IpFamily::Ipv6 => &self.ipv6_dns,
        }
    }
}

/// Everything the network features read from or write to the system.
pub(crate) trait NetStack {
    /// The adapters with addresses, flags and the DNS servers in use. DNS settings,
    /// capabilities and derived flags are filled in by [`adapters::list_with`].
    fn adapters(&self) -> Result<Vec<Adapter>>;
    /// Static DNS settings of `interface` (a GUID, braces optional).
    fn static_dns(&self, interface: &str, family: IpFamily) -> Result<StaticDns>;
    /// Sets the adapter's own servers (`NameServer`) only; `""` means automatic.
    fn set_static_dns(&self, interface: &str, family: IpFamily, servers: &str) -> Result<()>;
    /// Whether `Tcpip` (IPv4) or `Tcpip6` (IPv6) `Parameters\Interfaces\{guid}` exists.
    fn interface_key_exists(&self, interface: &str, family: IpFamily) -> Result<bool>;
    fn flush_resolver_cache(&self) -> Result<()>;
    fn release_lease(&self, if_index: u32) -> Result<()>;
    fn renew_lease(&self, if_index: u32) -> Result<()>;
    /// DNS servers set for every adapter by Group Policy.
    fn dns_policy(&self) -> Result<Vec<String>>;
    /// The TCP/IP settings stored for every interface, listed adapters or not. Read-only.
    fn stored_interfaces(&self) -> Result<Vec<StoredInterface>>;
}

/// The adapters of this PC with their DNS configuration and what may be changed on each.
/// With a journal, adapters with a recorded DNS change are marked revertible. Read-only.
pub fn list(journal: Option<&Journal>) -> Result<NetworkReport> {
    adapters::list_with(&IpHelper, journal)
}

/// The adapter `query` names: its id (braces and case ignored), its name (ASCII case
/// ignored) or its interface index. No match and several matches are errors.
pub fn find_adapter<'a>(adapters: &'a [Adapter], query: &str) -> Result<&'a Adapter> {
    let query = query.trim();
    if query.is_empty() {
        return Err(Error::Other("no network adapter was named".into()));
    }
    let id = canonical_guid(query);
    let index = query.parse::<u32>().ok();
    let matches: Vec<&Adapter> = adapters
        .iter()
        .filter(|a| a.id == id || a.name.eq_ignore_ascii_case(query) || Some(a.if_index) == index)
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(Error::Other(format!(
            "network adapter {query} is not on this PC; it may have been removed or disabled"
        ))),
        many => Err(Error::Other(format!(
            "{query} matches {} network adapters; name it by its id",
            many.len()
        ))),
    }
}

/// The static DNS servers of `interface` for `family`; `""` when they are obtained
/// automatically. Read-only.
pub fn static_dns(interface: &str, family: IpFamily) -> Result<String> {
    Ok(IpHelper.static_dns(interface, family)?.servers)
}

/// What [`set_dns`] would do. Read-only: nothing is recorded or written.
pub fn plan_dns(adapter_id: &str, request: &DnsRequest) -> Result<DnsReport> {
    dns::run(&IpHelper, dns::Mode::Plan, adapter_id, request)
}

/// Changes the DNS servers of `adapter_id`, recording each family's current servers in
/// the journal first. Refused while a VPN is connected and on adapters whose DNS servers
/// are managed elsewhere.
pub fn set_dns(safety: &Safety, adapter_id: &str, request: &DnsRequest) -> Result<DnsReport> {
    dns::run(&IpHelper, dns::Mode::Apply(safety), adapter_id, request)
}

/// [`plan_dns`] when `dry_run` is set, else [`set_dns`] under the session `begin` opens.
/// A dry run never calls `begin`: it opens no journal session and needs no elevation.
pub fn plan_or_set_dns(
    dry_run: bool,
    begin: impl FnOnce() -> Result<Safety>,
    adapter_id: &str,
    request: &DnsRequest,
) -> Result<DnsReport> {
    dns::plan_or_apply(&IpHelper, dry_run, begin, adapter_id, request)
}

/// Writes the recorded static DNS servers of `rec` back. Nothing is written when the
/// adapter already has them (AlreadyInState). NotFound when the interface no longer exists
/// on this PC. Fails, and the record stays active, when the interface exists but cannot be
/// written.
pub fn restore_dns(rec: &DnsRecord) -> Result<DnsRestore> {
    dns::restore_with(&IpHelper, rec)
}

/// Read-only. The servers currently set for the record's interface and family, as text,
/// when they differ from both the recorded baseline and the recorded target; None otherwise
/// or when the adapter is not listed.
pub fn dns_drift(rec: &DnsRecord) -> Result<Option<String>> {
    dns::drift_with(&IpHelper, rec)
}

/// Clears the DNS resolver cache. Nothing is logged; callers log.
pub fn flush_resolver_cache() -> Result<()> {
    IpHelper.flush_resolver_cache()
}

/// Clears the DNS resolver cache and logs it under `safety`'s session. Needs no elevation.
pub fn flush_dns_cache(safety: &Safety) -> Result<FlushReport> {
    maintenance::flush_with(safety, &IpHelper)
}

/// Renews the IPv4 DHCP lease of `adapter_id` (id, name or interface index), releasing it
/// first when `release_first` is set. Logged, not journaled.
pub fn renew_lease(safety: &Safety, adapter_id: &str, release_first: bool) -> Result<RenewReport> {
    maintenance::renew_with(safety, &IpHelper, adapter_id, release_first)
}

/// The reset steps and the manual settings a reset would remove. Runs nothing.
pub fn plan_reset() -> Result<ResetReport> {
    maintenance::reset_with(None, &IpHelper, &SystemRunner, &system_dir()?)
}

/// Resets the Winsock catalog and the TCP/IP stacks with `netsh`. Irreversible: the manual
/// settings it removes are logged first; the restore point, if any, comes from the caller's
/// [`Safety::begin`]. Windows must restart afterwards.
pub fn reset_stack(safety: &Safety) -> Result<ResetReport> {
    maintenance::reset_with(Some(safety), &IpHelper, &SystemRunner, &system_dir()?)
}

/// [`plan_reset`] when `dry_run` is set, else [`reset_stack`] under the session `begin`
/// opens (with its restore point). A dry run never calls `begin`.
pub fn plan_or_reset_stack(
    dry_run: bool,
    begin: impl FnOnce() -> Result<Safety>,
) -> Result<ResetReport> {
    maintenance::plan_or_reset(&IpHelper, &SystemRunner, &system_dir()?, dry_run, begin)
}

#[cfg(test)]
pub(crate) mod tests {
    //! The fake network stack the unit tests of this module share.

    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use super::*;

    /// Test interface GUID number `n`.
    pub(crate) fn guid(n: u32) -> String {
        format!("{{aaaaaaaa-0000-0000-0000-{n:012x}}}")
    }

    /// A temporary journal; keep the directory alive as long as the journal.
    pub(crate) fn journal() -> (tempfile::TempDir, Arc<Journal>) {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Journal::open(dir.path().join("journal.db")).unwrap());
        (dir, journal)
    }

    /// A connected adapter on DHCP with IPv4 and IPv6 on, address `192.168.0.{10 + n}/24`,
    /// gateway and DNS server 192.168.0.1 and interface index `n`. Only VPN, tunnel and
    /// virtual adapters are software adapters.
    pub(crate) fn adapter(n: u32, name: &str, kind: AdapterKind) -> Adapter {
        Adapter {
            id: guid(n),
            name: name.to_string(),
            description: format!("{name} adapter"),
            kind,
            status: LinkStatus::Connected,
            limited: false,
            hardware: !matches!(
                kind,
                AdapterKind::Virtual | AdapterKind::Vpn | AdapterKind::Tunnel
            ),
            minor: false,
            primary: false,
            if_index: n,
            mac: "00-00-5E-00-53-55".into(),
            mtu: 1500,
            receive_bps: Some(1_000_000_000),
            transmit_bps: Some(1_000_000_000),
            dhcp_enabled: true,
            ipv4_enabled: true,
            ipv6_enabled: true,
            ipv4: vec![AddressInfo {
                address: format!("192.168.0.{}", 10 + n),
                prefix_length: 24,
                origin: AddressOrigin::Dhcp,
                preferred: true,
            }],
            ipv6: Vec::new(),
            gateways: vec!["192.168.0.1".into()],
            dns_servers: vec!["192.168.0.1".into()],
            dns_ipv4: DnsConfig::unknown(),
            dns_ipv6: DnsConfig::unknown(),
            dns_suffix: String::new(),
            ipv4_metric: 25,
            can_change_dns: false,
            can_renew: false,
            dns_revertible: false,
            note: None,
            read_warnings: Vec::new(),
        }
    }

    type SetHook = Box<dyn Fn(&str, IpFamily, &str)>;

    /// In-memory network stack. Mutating calls are recorded in `calls` as `set <guid>
    /// <family> <servers>`, `flush`, `release <index>` and `renew <index>`.
    #[derive(Default)]
    pub(crate) struct FakeStack {
        pub adapters: RefCell<Vec<Adapter>>,
        /// (canonical GUID, family) -> static servers; missing means automatic.
        pub static_dns: RefCell<HashMap<(String, IpFamily), String>>,
        /// (canonical GUID, family) -> servers set for the Wi-Fi network.
        pub profile_dns: RefCell<HashMap<(String, IpFamily), String>>,
        /// Interfaces whose DNS settings cannot be read.
        pub unreadable: RefCell<HashSet<String>>,
        /// (canonical GUID, family) of TCP/IP interface keys that exist.
        pub keys: RefCell<HashSet<(String, IpFamily)>>,
        /// Family whose writes fail with "Access is denied.".
        pub fail_set: Option<IpFamily>,
        pub fail_flush: Option<String>,
        pub fail_renew: Option<String>,
        /// Error of `adapters()`.
        pub fail_adapters: RefCell<Option<String>>,
        pub policy: RefCell<Vec<String>>,
        /// Settings stored for interfaces, listed or not.
        pub stored: RefCell<Vec<StoredInterface>>,
        /// Error of `stored_interfaces()`.
        pub fail_stored: RefCell<Option<String>>,
        pub calls: RefCell<Vec<String>>,
        /// Runs before each write, after the call is recorded.
        pub on_set: Option<SetHook>,
    }

    impl FakeStack {
        pub(crate) fn with(adapters: Vec<Adapter>) -> FakeStack {
            let keys = adapters
                .iter()
                .flat_map(|a| IpFamily::ALL.map(|f| (a.id.clone(), f)))
                .collect();
            FakeStack {
                adapters: RefCell::new(adapters),
                keys: RefCell::new(keys),
                ..Default::default()
            }
        }

        pub(crate) fn set_static(&self, interface: &str, family: IpFamily, servers: &str) {
            let key = (canonical_guid(interface), family);
            if servers.is_empty() {
                self.static_dns.borrow_mut().remove(&key);
            } else {
                self.static_dns
                    .borrow_mut()
                    .insert(key, servers.to_string());
            }
        }

        pub(crate) fn static_of(&self, interface: &str, family: IpFamily) -> String {
            self.static_dns
                .borrow()
                .get(&(canonical_guid(interface), family))
                .cloned()
                .unwrap_or_default()
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl NetStack for FakeStack {
        fn adapters(&self) -> Result<Vec<Adapter>> {
            if let Some(e) = self.fail_adapters.borrow().as_ref() {
                return Err(Error::Other(e.clone()));
            }
            Ok(self.adapters.borrow().clone())
        }

        fn static_dns(&self, interface: &str, family: IpFamily) -> Result<StaticDns> {
            let id = canonical_guid(interface);
            if self.unreadable.borrow().contains(&id) {
                return Err(Error::Other("Access is denied.".into()));
            }
            let key = (id, family);
            Ok(StaticDns {
                servers: self
                    .static_dns
                    .borrow()
                    .get(&key)
                    .cloned()
                    .unwrap_or_default(),
                profile_servers: self
                    .profile_dns
                    .borrow()
                    .get(&key)
                    .cloned()
                    .unwrap_or_default(),
            })
        }

        fn set_static_dns(&self, interface: &str, family: IpFamily, servers: &str) -> Result<()> {
            let id = canonical_guid(interface);
            self.calls
                .borrow_mut()
                .push(format!("set {id} {} {servers}", family.as_str()));
            if let Some(hook) = &self.on_set {
                hook(&id, family, servers);
            }
            if self.fail_set == Some(family) {
                return Err(Error::Other("Access is denied.".into()));
            }
            self.set_static(&id, family, servers);
            Ok(())
        }

        fn interface_key_exists(&self, interface: &str, family: IpFamily) -> Result<bool> {
            Ok(self
                .keys
                .borrow()
                .contains(&(canonical_guid(interface), family)))
        }

        fn flush_resolver_cache(&self) -> Result<()> {
            self.calls.borrow_mut().push("flush".into());
            match &self.fail_flush {
                Some(e) => Err(Error::Other(e.clone())),
                None => Ok(()),
            }
        }

        fn release_lease(&self, if_index: u32) -> Result<()> {
            self.calls.borrow_mut().push(format!("release {if_index}"));
            Ok(())
        }

        fn renew_lease(&self, if_index: u32) -> Result<()> {
            self.calls.borrow_mut().push(format!("renew {if_index}"));
            match &self.fail_renew {
                Some(e) => Err(Error::Other(e.clone())),
                None => Ok(()),
            }
        }

        fn dns_policy(&self) -> Result<Vec<String>> {
            Ok(self.policy.borrow().clone())
        }

        fn stored_interfaces(&self) -> Result<Vec<StoredInterface>> {
            if let Some(e) = self.fail_stored.borrow().as_ref() {
                return Err(Error::Other(e.clone()));
            }
            Ok(self.stored.borrow().clone())
        }
    }

    #[test]
    fn find_adapter_matches_id_name_or_index() {
        let mut ethernet = adapter(2, "Ethernet", AdapterKind::Ethernet);
        ethernet.if_index = 7;
        let adapters = vec![adapter(1, "Wi-Fi", AdapterKind::Wifi), ethernet];
        let name = |q: &str| find_adapter(&adapters, q).map(|a| a.name.clone());
        assert_eq!(name(&guid(1)).unwrap(), "Wi-Fi");
        assert_eq!(name(&guid(2).to_uppercase()).unwrap(), "Ethernet");
        assert_eq!(
            name(guid(2).trim_matches(|c| c == '{' || c == '}')).unwrap(),
            "Ethernet"
        );
        assert_eq!(name("wi-fi").unwrap(), "Wi-Fi");
        assert_eq!(name(" ETHERNET ").unwrap(), "Ethernet");
        assert_eq!(name("7").unwrap(), "Ethernet");
        assert_eq!(name("1").unwrap(), "Wi-Fi");
        assert_eq!(
            name("Bluetooth").unwrap_err().to_string(),
            "network adapter Bluetooth is not on this PC; it may have been removed or disabled"
        );
        assert!(name("").is_err());

        let mut numbered = adapter(3, "7", AdapterKind::Ethernet);
        numbered.if_index = 30;
        let ambiguous = vec![adapters[1].clone(), numbered];
        let err = find_adapter(&ambiguous, "7").unwrap_err().to_string();
        assert_eq!(err, "7 matches 2 network adapters; name it by its id");
    }

    #[test]
    fn fake_stack_records_writes_and_reads_back() {
        let stack = FakeStack::with(vec![adapter(1, "Wi-Fi", AdapterKind::Wifi)]);
        stack
            .set_static_dns(&guid(1).to_uppercase(), IpFamily::Ipv4, "1.1.1.1")
            .unwrap();
        assert_eq!(stack.static_of(&guid(1), IpFamily::Ipv4), "1.1.1.1");
        assert_eq!(stack.calls(), vec![format!("set {} ipv4 1.1.1.1", guid(1))]);
        assert!(stack
            .interface_key_exists(&guid(1), IpFamily::Ipv6)
            .unwrap());
        assert!(!stack
            .interface_key_exists(&guid(2), IpFamily::Ipv4)
            .unwrap());
    }
}
