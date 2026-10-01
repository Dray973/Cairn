//! The IP Helper API behind [`NetStack`]: adapter enumeration, per-interface DNS settings,
//! DHCP leases and the resolver cache. All unsafe code of the network module is here; the
//! rest of the module sees owned copies only.

use std::collections::HashMap;
use std::mem::{offset_of, size_of};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ptr;

use windows::core::{s, w, GUID, PWSTR};
use windows::Win32::Foundation::{
    FreeLibrary, GetLastError, SetLastError, ERROR_BUFFER_OVERFLOW, ERROR_FILE_NOT_FOUND,
    ERROR_INSUFFICIENT_BUFFER, ERROR_NO_DATA, HMODULE, NO_ERROR, WIN32_ERROR,
};
use windows::Win32::NetworkManagement::IpHelper::{
    FreeInterfaceDnsSettings, GetAdaptersAddresses, GetIfEntry2, GetInterfaceDnsSettings,
    GetInterfaceInfo, IpReleaseAddress, IpRenewAddress, SetInterfaceDnsSettings,
    DNS_INTERFACE_SETTINGS, DNS_INTERFACE_SETTINGS_VERSION1, DNS_SETTING_IPV6,
    DNS_SETTING_NAMESERVER, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST,
    GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_INDEX_MAP, IP_INTERFACE_INFO,
    MIB_IF_ROW2,
};
use windows::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6, SOCKET_ADDRESS,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

use super::adapters::{adapter_from_raw, Adapter, RawAdapter, RawUnicast};
use super::{canonical_guid, split_servers, IpFamily, NetStack, StaticDns, StoredInterface};
use crate::win::registry::{self, Hive, Key, RegValue};
use crate::win::{check, wide};
use crate::{Error, Result};

/// First buffer size for the adapter and interface lists.
const INITIAL_BUFFER_BYTES: usize = 16 * 1024;
/// Retries of a list call that reports a larger buffer each time.
const MAX_BUFFER_RETRIES: usize = 4;
/// Group Policy value that sets DNS servers for every adapter.
const DNS_POLICY_KEY: &str = r"SOFTWARE\Policies\Microsoft\Windows NT\DNSClient";
/// The network adapter device class: each numbered subkey names an adapter's interface
/// GUID (`NetCfgInstanceId`), its driver description and its component id.
const ADAPTER_CLASS_KEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4D36E972-E325-11CE-BFC1-08002BE10318}";
/// Connection names of the adapters: `{guid}\Connection`, value `Name`.
const CONNECTIONS_KEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Network\{4D36E972-E325-11CE-BFC1-08002BE10318}";

/// `DnsFlushResolverCache` from `dnsapi.dll`: takes nothing, returns a BOOL.
type FlushResolverCache = unsafe extern "system" fn() -> i32;

/// The live network stack of this PC.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IpHelper;

impl NetStack for IpHelper {
    fn adapters(&self) -> Result<Vec<Adapter>> {
        Ok(read_raw_adapters()?
            .into_iter()
            .filter_map(adapter_from_raw)
            .collect())
    }

    fn static_dns(&self, interface: &str, family: IpFamily) -> Result<StaticDns> {
        read_dns_settings(interface, family)
    }

    fn set_static_dns(&self, interface: &str, family: IpFamily, servers: &str) -> Result<()> {
        write_name_servers(interface, family, servers)
    }

    fn interface_key_exists(&self, interface: &str, family: IpFamily) -> Result<bool> {
        registry::exists(Hive::LocalMachine, &interface_key(interface, family))
    }

    fn flush_resolver_cache(&self) -> Result<()> {
        flush_resolver_cache()
    }

    fn release_lease(&self, if_index: u32) -> Result<()> {
        let map = index_map(if_index)?;
        // SAFETY: `map` is an entry GetInterfaceInfo returned, passed by reference.
        let rc = unsafe { IpReleaseAddress(&map) };
        check(WIN32_ERROR(rc))
    }

    fn renew_lease(&self, if_index: u32) -> Result<()> {
        let map = index_map(if_index)?;
        // SAFETY: as above.
        let rc = unsafe { IpRenewAddress(&map) };
        check(WIN32_ERROR(rc))
    }

    fn dns_policy(&self) -> Result<Vec<String>> {
        Ok(
            match registry::read_value(Hive::LocalMachine, DNS_POLICY_KEY, "NameServer")? {
                Some(RegValue::Sz(text)) | Some(RegValue::ExpandSz(text)) => split_servers(&text),
                _ => Vec::new(),
            },
        )
    }

    fn stored_interfaces(&self) -> Result<Vec<StoredInterface>> {
        read_stored_interfaces()
    }
}

/// `SYSTEM\CurrentControlSet\Services\Tcpip[6]\Parameters\Interfaces`.
fn interfaces_root(family: IpFamily) -> String {
    let service = match family {
        IpFamily::Ipv4 => "Tcpip",
        IpFamily::Ipv6 => "Tcpip6",
    };
    format!(r"SYSTEM\CurrentControlSet\Services\{service}\Parameters\Interfaces")
}

/// `SYSTEM\CurrentControlSet\Services\Tcpip[6]\Parameters\Interfaces\{guid}`.
pub(crate) fn interface_key(interface: &str, family: IpFamily) -> String {
    format!(r"{}\{}", interfaces_root(family), canonical_guid(interface))
}

// ───────────────────────────── stored settings ─────────────────────────────

/// Text of a string value; a multi-string is joined with commas. Empty when missing.
fn text_value(key: &Key, name: &str) -> Result<String> {
    Ok(match key.query(name)? {
        Some(RegValue::Sz(text)) | Some(RegValue::ExpandSz(text)) => text,
        Some(RegValue::MultiSz(items)) => items.join(","),
        _ => String::new(),
    })
}

/// Entries of a multi-string value, trimmed and in their positions (`IPAddress` and
/// `SubnetMask` pair up by position), or of a string value holding a list.
fn list_value(key: &Key, name: &str) -> Result<Vec<String>> {
    Ok(match key.query(name)? {
        Some(RegValue::MultiSz(items)) => items.into_iter().map(|s| s.trim().to_string()).collect(),
        Some(RegValue::Sz(text)) | Some(RegValue::ExpandSz(text)) => split_servers(&text),
        _ => Vec::new(),
    })
}

/// The TCP/IP settings of every interface key of both families, with the adapter's
/// connection name and driver where Windows keeps them. Read-only.
fn read_stored_interfaces() -> Result<Vec<StoredInterface>> {
    let mut stored: Vec<StoredInterface> = Vec::new();
    for family in IpFamily::ALL {
        let root = interfaces_root(family);
        for name in registry::subkey_names(Hive::LocalMachine, &root)? {
            let Some(key) = Key::open(Hive::LocalMachine, &format!(r"{root}\{name}"), false)?
            else {
                continue;
            };
            let guid = canonical_guid(&name);
            let index = match stored.iter().position(|s| s.guid == guid) {
                Some(index) => index,
                None => {
                    stored.push(StoredInterface {
                        guid,
                        ..StoredInterface::default()
                    });
                    stored.len() - 1
                }
            };
            let entry = &mut stored[index];
            let servers = text_value(&key, "NameServer")?;
            match family {
                IpFamily::Ipv4 => {
                    entry.static_ipv4 =
                        matches!(key.query("EnableDHCP")?, Some(RegValue::Dword(0)));
                    entry.addresses = list_value(&key, "IPAddress")?;
                    entry.masks = list_value(&key, "SubnetMask")?;
                    entry.gateways = list_value(&key, "DefaultGateway")?;
                    entry.ipv4_dns = servers;
                }
                IpFamily::Ipv6 => entry.ipv6_dns = servers,
            }
        }
    }
    let drivers = adapter_drivers();
    for entry in &mut stored {
        entry.name = connection_name(&entry.guid);
        if let Some((description, component_id)) = drivers.get(&entry.guid) {
            entry.description = description.clone();
            entry.component_id = component_id.clone();
        }
    }
    Ok(stored)
}

/// Driver description and component id of every network adapter Windows knows, by
/// canonical interface GUID. Best effort: an entry that cannot be read is left out.
fn adapter_drivers() -> HashMap<String, (String, String)> {
    let mut drivers = HashMap::new();
    let Ok(names) = registry::subkey_names(Hive::LocalMachine, ADAPTER_CLASS_KEY) else {
        return drivers;
    };
    for name in names {
        let path = format!(r"{ADAPTER_CLASS_KEY}\{name}");
        let Ok(Some(key)) = Key::open(Hive::LocalMachine, &path, false) else {
            continue;
        };
        let Ok(id) = text_value(&key, "NetCfgInstanceId") else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let text = |value: &str| text_value(&key, value).unwrap_or_default();
        drivers.insert(
            canonical_guid(&id),
            (text("DriverDesc"), text("ComponentId")),
        );
    }
    drivers
}

/// Connection name of the interface `guid`, such as "Ethernet 2"; empty when Windows keeps
/// none or it cannot be read.
fn connection_name(guid: &str) -> String {
    let path = format!(r"{CONNECTIONS_KEY}\{}\Connection", canonical_guid(guid));
    match registry::read_value(Hive::LocalMachine, &path, "Name") {
        Ok(Some(RegValue::Sz(name))) | Ok(Some(RegValue::ExpandSz(name))) => name,
        _ => String::new(),
    }
}

fn win32_error(code: WIN32_ERROR) -> Error {
    Error::Win32(windows::core::Error::from(code))
}

/// The interface GUID `interface` names (braces optional, any case).
fn interface_guid(interface: &str) -> Result<GUID> {
    let canonical = canonical_guid(interface);
    let inner = &canonical[1..canonical.len() - 1];
    GUID::try_from(inner)
        .map_err(|_| Error::Other(format!("{interface} is not a network interface GUID")))
}

fn family_flag(family: IpFamily) -> u64 {
    match family {
        IpFamily::Ipv4 => 0,
        IpFamily::Ipv6 => u64::from(DNS_SETTING_IPV6),
    }
}

// ───────────────────────────── adapters ─────────────────────────────

/// Every adapter `GetAdaptersAddresses` lists (no filter or hidden interfaces), with the
/// link details of its interface row.
fn read_raw_adapters() -> Result<Vec<RawAdapter>> {
    let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST;
    let mut bytes = INITIAL_BUFFER_BYTES;
    for _ in 0..=MAX_BUFFER_RETRIES {
        // u64 elements keep the buffer aligned for the structures the API writes into it.
        let mut buf = vec![0u64; bytes.div_ceil(size_of::<u64>())];
        let mut size = u32::try_from(buf.len() * size_of::<u64>()).unwrap_or(u32::MAX);
        let first = buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: `first` points to `size` writable bytes, 8-byte aligned; `size` is
        // updated by the call.
        let rc = unsafe {
            GetAdaptersAddresses(u32::from(AF_UNSPEC.0), flags, None, Some(first), &mut size)
        };
        match WIN32_ERROR(rc) {
            NO_ERROR => {
                // SAFETY: on success `buf` holds the adapter list; every pointer in it
                // points into `buf`, which lives until the copy returns.
                let mut raw = unsafe { copy_adapters(first) };
                drop(buf);
                for adapter in &mut raw {
                    match link_details(adapter.luid) {
                        Ok((hardware, filter, medium)) => {
                            adapter.hardware = hardware;
                            adapter.filter = filter;
                            adapter.medium = medium;
                        }
                        Err(e) => adapter.link_error = Some(e.to_string()),
                    }
                }
                return Ok(raw);
            }
            ERROR_BUFFER_OVERFLOW => bytes = (size as usize).max(bytes) + 1024,
            ERROR_NO_DATA => return Ok(Vec::new()),
            other => return Err(win32_error(other)),
        }
    }
    Err(Error::Other(
        "the network adapter list kept growing while it was read".into(),
    ))
}

/// Copies the adapter list that starts at `first`.
///
/// # Safety
/// `first` is null or the head of a list written by `GetAdaptersAddresses` whose buffer
/// stays alive for the call.
unsafe fn copy_adapters(first: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<RawAdapter> {
    let mut adapters = Vec::new();
    let mut current = first;
    while !current.is_null() {
        // SAFETY: a non-null entry of the list is a valid structure (caller contract).
        let adapter = unsafe { &*current };
        // SAFETY: as above; its nested lists point into the same buffer.
        adapters.push(unsafe { copy_adapter(adapter) });
        current = adapter.Next;
    }
    adapters
}

/// Copies one adapter entry.
///
/// # Safety
/// `a` is an entry of a list written by `GetAdaptersAddresses` whose buffer is alive.
unsafe fn copy_adapter(a: &IP_ADAPTER_ADDRESSES_LH) -> RawAdapter {
    // SAFETY: both views of the first union are plain integers the API wrote.
    let if_index = unsafe { a.Anonymous1.Anonymous.IfIndex };
    // SAFETY: as above, for the flags union.
    let flags = unsafe { a.Anonymous2.Flags };
    // SAFETY: as above, for the LUID union.
    let luid = unsafe { a.Luid.Value };
    let mac_len = (a.PhysicalAddressLength as usize).min(a.PhysicalAddress.len());

    let mut unicast = Vec::new();
    let mut entry = a.FirstUnicastAddress;
    while !entry.is_null() {
        // SAFETY: non-null entries of the list are valid (caller contract).
        let u = unsafe { &*entry };
        // SAFETY: the socket address points into the API's buffer with its length.
        if let Some(address) = unsafe { ip_of(&u.Address) } {
            unicast.push(RawUnicast {
                address,
                prefix_length: u.OnLinkPrefixLength,
                prefix_origin: u.PrefixOrigin.0,
                suffix_origin: u.SuffixOrigin.0,
                dad_state: u.DadState.0,
            });
        }
        entry = u.Next;
    }

    let mut dns_servers = Vec::new();
    let mut entry = a.FirstDnsServerAddress;
    while !entry.is_null() {
        // SAFETY: as above.
        let d = unsafe { &*entry };
        // SAFETY: as above.
        dns_servers.extend(unsafe { ip_of(&d.Address) });
        entry = d.Next;
    }

    let mut gateways = Vec::new();
    let mut entry = a.FirstGatewayAddress;
    while !entry.is_null() {
        // SAFETY: as above.
        let g = unsafe { &*entry };
        // SAFETY: as above.
        gateways.extend(unsafe { ip_of(&g.Address) });
        entry = g.Next;
    }

    RawAdapter {
        if_index,
        // SAFETY: the strings are null or NUL-terminated strings in the API's buffer.
        guid: unsafe { narrow_text(a.AdapterName.0) },
        // SAFETY: as above.
        name: unsafe { wide_text(a.FriendlyName) },
        // SAFETY: as above.
        description: unsafe { wide_text(a.Description) },
        // SAFETY: as above.
        dns_suffix: unsafe { wide_text(a.DnsSuffix) },
        mac: a.PhysicalAddress[..mac_len].to_vec(),
        flags,
        mtu: a.Mtu,
        if_type: a.IfType,
        oper_status: a.OperStatus.0,
        tunnel_type: u32::try_from(a.TunnelType.0).unwrap_or(0),
        transmit_bps: a.TransmitLinkSpeed,
        receive_bps: a.ReceiveLinkSpeed,
        unicast,
        dns_servers,
        gateways,
        ipv4_metric: a.Ipv4Metric,
        luid,
        ..RawAdapter::default()
    }
}

/// Text of a NUL-terminated ANSI string; empty for null.
///
/// # Safety
/// `p` is null or points to a NUL-terminated string.
unsafe fn narrow_text(p: *const u8) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: a non-null `p` is NUL-terminated (caller contract).
    let text = unsafe { std::ffi::CStr::from_ptr(p.cast()) };
    text.to_string_lossy().into_owned()
}

/// Text of a NUL-terminated UTF-16 string; empty for null.
///
/// # Safety
/// `p` is null or points to a NUL-terminated string.
unsafe fn wide_text(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: a non-null `p` is NUL-terminated (caller contract).
    String::from_utf16_lossy(unsafe { p.as_wide() })
}

/// The IP address a socket address holds; `None` for a null pointer, a length too short
/// for its family, or a family other than IPv4 and IPv6.
///
/// # Safety
/// `sa.lpSockaddr` is null or points to at least `sa.iSockaddrLength` readable bytes.
pub(crate) unsafe fn ip_of(sa: &SOCKET_ADDRESS) -> Option<IpAddr> {
    let base = sa.lpSockaddr.cast::<u8>().cast_const();
    if base.is_null() {
        return None;
    }
    let len = usize::try_from(sa.iSockaddrLength).ok()?;
    if len < size_of::<u16>() {
        return None;
    }
    // SAFETY: at least two bytes are readable (checked above); the read is unaligned.
    let family = unsafe { ptr::read_unaligned(base.cast::<u16>()) };
    if family == AF_INET.0 && len >= size_of::<SOCKADDR_IN>() {
        // SAFETY: `len` covers a whole SOCKADDR_IN; the read is unaligned.
        let sin = unsafe { ptr::read_unaligned(base.cast::<SOCKADDR_IN>()) };
        // SAFETY: every view of IN_ADDR is the same four bytes in network order.
        let raw = unsafe { sin.sin_addr.S_un.S_addr };
        Some(IpAddr::V4(Ipv4Addr::from(raw.to_ne_bytes())))
    } else if family == AF_INET6.0 && len >= size_of::<SOCKADDR_IN6>() {
        // SAFETY: `len` covers a whole SOCKADDR_IN6; the read is unaligned.
        let sin6 = unsafe { ptr::read_unaligned(base.cast::<SOCKADDR_IN6>()) };
        // SAFETY: every view of IN6_ADDR is the same sixteen bytes.
        let bytes = unsafe { sin6.sin6_addr.u.Byte };
        Some(IpAddr::V6(Ipv6Addr::from(bytes)))
    } else {
        None
    }
}

/// `HardwareInterface`, `FilterInterface` and the physical medium of the interface `luid`.
fn link_details(luid: u64) -> Result<(bool, bool, u32)> {
    let mut row = MIB_IF_ROW2::default();
    row.InterfaceLuid.Value = luid;
    // SAFETY: `row` is a valid, writable MIB_IF_ROW2 whose LUID selects the interface.
    check(unsafe { GetIfEntry2(&mut row) })?;
    let bits = row.InterfaceAndOperStatusFlags._bitfield;
    Ok((
        bits & 0x01 != 0,
        bits & 0x02 != 0,
        u32::try_from(row.PhysicalMediumType.0).unwrap_or(0),
    ))
}

// ───────────────────────────── DNS settings ─────────────────────────────

/// Settings returned by `GetInterfaceDnsSettings`; the strings it allocated are freed on
/// drop.
struct DnsSettings(DNS_INTERFACE_SETTINGS);

impl Drop for DnsSettings {
    fn drop(&mut self) {
        let s = &self.0;
        let allocated = [s.Domain, s.NameServer, s.SearchList, s.ProfileNameServer]
            .iter()
            .any(|p| !p.is_null());
        if allocated {
            // SAFETY: the strings were allocated by GetInterfaceDnsSettings for this
            // structure and are freed exactly once, here.
            unsafe { FreeInterfaceDnsSettings(&mut self.0) };
        }
    }
}

/// Static and per-network DNS servers of one interface and family. An interface without
/// stored settings has neither.
fn read_dns_settings(interface: &str, family: IpFamily) -> Result<StaticDns> {
    let guid = interface_guid(interface)?;
    let mut settings = DnsSettings(DNS_INTERFACE_SETTINGS {
        Version: DNS_INTERFACE_SETTINGS_VERSION1,
        Flags: family_flag(family),
        ..Default::default()
    });
    // SAFETY: `settings.0` is a valid, writable version 1 structure; the strings the call
    // allocates are freed when `settings` drops.
    let rc = unsafe { GetInterfaceDnsSettings(guid, &mut settings.0) };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(StaticDns::default());
    }
    check(rc)?;
    Ok(StaticDns {
        // SAFETY: the strings are null or NUL-terminated strings the call allocated.
        servers: unsafe { wide_text(settings.0.NameServer) },
        // SAFETY: as above.
        profile_servers: unsafe { wide_text(settings.0.ProfileNameServer) },
    })
}

/// Sets the static DNS servers of one interface and family; `""` makes them automatic.
/// Only the adapter's own `NameServer` setting is written, never a Wi-Fi network's.
fn write_name_servers(interface: &str, family: IpFamily, servers: &str) -> Result<()> {
    let guid = interface_guid(interface)?;
    let mut text = wide(servers);
    let settings = DNS_INTERFACE_SETTINGS {
        Version: DNS_INTERFACE_SETTINGS_VERSION1,
        Flags: u64::from(DNS_SETTING_NAMESERVER) | family_flag(family),
        NameServer: PWSTR(text.as_mut_ptr()),
        ..Default::default()
    };
    // SAFETY: `settings` is a valid version 1 structure; its NameServer points into
    // `text`, a NUL-terminated buffer that outlives the call.
    check(unsafe { SetInterfaceDnsSettings(guid, &settings) })
}

// ───────────────────────────── DHCP lease ─────────────────────────────

/// The `GetInterfaceInfo` entry of the IPv4 interface `if_index`.
fn index_map(if_index: u32) -> Result<IP_ADAPTER_INDEX_MAP> {
    let mut bytes = INITIAL_BUFFER_BYTES;
    for _ in 0..=MAX_BUFFER_RETRIES {
        let mut buf = vec![0u64; bytes.div_ceil(size_of::<u64>())];
        let capacity = buf.len() * size_of::<u64>();
        let mut size = u32::try_from(capacity).unwrap_or(u32::MAX);
        let table = buf.as_mut_ptr().cast::<IP_INTERFACE_INFO>();
        // SAFETY: `table` points to `size` writable bytes, 8-byte aligned.
        let rc = WIN32_ERROR(unsafe { GetInterfaceInfo(Some(table), &mut size) });
        match rc {
            NO_ERROR => return find_index_map(&buf, if_index),
            ERROR_INSUFFICIENT_BUFFER => bytes = (size as usize).max(capacity) + 1024,
            ERROR_NO_DATA => break,
            other => return Err(win32_error(other)),
        }
    }
    Err(Error::Other(format!(
        "interface {if_index} has no IPv4 DHCP client; it may have been disconnected"
    )))
}

/// Finds `if_index` in an `IP_INTERFACE_INFO` table held in `buf`.
fn find_index_map(buf: &[u64], if_index: u32) -> Result<IP_ADAPTER_INDEX_MAP> {
    let bytes = std::mem::size_of_val(buf);
    let base = buf.as_ptr().cast::<u8>();
    if bytes < size_of::<i32>() {
        return Err(Error::Other("the interface list is empty".into()));
    }
    // SAFETY: the buffer holds at least the leading count (checked above).
    let count = unsafe { ptr::read_unaligned(base.cast::<i32>()) };
    let first = offset_of!(IP_INTERFACE_INFO, Adapter);
    let entry = size_of::<IP_ADAPTER_INDEX_MAP>();
    for i in 0..usize::try_from(count).unwrap_or(0) {
        let offset = first + i * entry;
        if offset + entry > bytes {
            break;
        }
        // SAFETY: the entry lies inside the buffer (checked above); the read is unaligned.
        let map = unsafe { ptr::read_unaligned(base.add(offset).cast::<IP_ADAPTER_INDEX_MAP>()) };
        if map.Index == if_index {
            return Ok(map);
        }
    }
    Err(Error::Other(format!(
        "interface {if_index} has no IPv4 DHCP client; it may have been disconnected"
    )))
}

// ───────────────────────────── resolver cache ─────────────────────────────

/// `dnsapi.dll`, unloaded on drop.
struct Library(HMODULE);

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the module was loaded by LoadLibraryExW and is released once, here.
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}

/// Clears the DNS Client service's resolver cache through `DnsFlushResolverCache`.
fn flush_resolver_cache() -> Result<()> {
    // SAFETY: a constant, NUL-terminated library name; the search is limited to System32.
    let module = unsafe { LoadLibraryExW(w!("dnsapi.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }?;
    let library = Library(module);
    // SAFETY: a valid module handle and a constant, NUL-terminated export name.
    let export = unsafe { GetProcAddress(library.0, s!("DnsFlushResolverCache")) }
        .ok_or_else(|| Error::Other("dnsapi.dll does not export DnsFlushResolverCache".into()))?;
    // SAFETY: DnsFlushResolverCache takes no arguments and returns a BOOL, so both
    // function pointer types have the same calling convention and ABI.
    let flush = unsafe {
        std::mem::transmute::<unsafe extern "system" fn() -> isize, FlushResolverCache>(export)
    };
    // SAFETY: clears this thread's last-error value, so a failure that sets none is seen.
    unsafe { SetLastError(NO_ERROR) };
    // SAFETY: the export stays loaded while `library` is alive.
    let ok = unsafe { flush() };
    // SAFETY: reads this thread's last-error value, immediately after the call.
    let last_error = unsafe { GetLastError() }.0;
    drop(library);
    map_flush(ok, last_error)
}

/// Result of `DnsFlushResolverCache` from its return value and the last error read right
/// after it.
pub(crate) fn map_flush(ok: i32, last_error: u32) -> Result<()> {
    if ok != 0 {
        Ok(())
    } else if last_error == 0 {
        Err(Error::Other(
            "the DNS Client service did not flush its cache".into(),
        ))
    } else {
        Err(win32_error(WIN32_ERROR(last_error)))
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Networking::WinSock::{IN6_ADDR, IN6_ADDR_0, IN_ADDR, IN_ADDR_0, SOCKADDR};

    use super::*;

    fn socket_address<T>(value: &mut T, len: usize) -> SOCKET_ADDRESS {
        SOCKET_ADDRESS {
            lpSockaddr: (value as *mut T).cast::<SOCKADDR>(),
            iSockaddrLength: i32::try_from(len).unwrap(),
        }
    }

    #[test]
    fn ip_of_reads_ipv4_and_ipv6_sockaddrs() {
        let mut v4 = SOCKADDR_IN {
            sin_family: AF_INET,
            sin_port: 53,
            sin_addr: IN_ADDR {
                S_un: IN_ADDR_0 {
                    S_addr: u32::from_ne_bytes([192, 168, 0, 1]),
                },
            },
            sin_zero: [0; 8],
        };
        let full = socket_address(&mut v4, size_of::<SOCKADDR_IN>());
        // SAFETY: the socket address points to `v4` with its size.
        assert_eq!(
            unsafe { ip_of(&full) },
            Some("192.168.0.1".parse().unwrap())
        );
        let short = socket_address(&mut v4, size_of::<SOCKADDR_IN>() - 1);
        // SAFETY: the length is within `v4`.
        assert_eq!(unsafe { ip_of(&short) }, None);
        let tiny = socket_address(&mut v4, 1);
        // SAFETY: as above.
        assert_eq!(unsafe { ip_of(&tiny) }, None);

        let mut bytes = [0u8; 16];
        bytes[..2].copy_from_slice(&[0x26, 0x06]);
        bytes[14..].copy_from_slice(&[0x11, 0x11]);
        let mut v6 = SOCKADDR_IN6 {
            sin6_family: AF_INET6,
            sin6_addr: IN6_ADDR {
                u: IN6_ADDR_0 { Byte: bytes },
            },
            ..Default::default()
        };
        let full = socket_address(&mut v6, size_of::<SOCKADDR_IN6>());
        // SAFETY: the socket address points to `v6` with its size.
        assert_eq!(unsafe { ip_of(&full) }, Some("2606::1111".parse().unwrap()));
        let short = socket_address(&mut v6, size_of::<SOCKADDR_IN>());
        // SAFETY: the length is within `v6`.
        assert_eq!(
            unsafe { ip_of(&short) },
            None,
            "an IPv6 family needs a whole SOCKADDR_IN6"
        );

        let mut other = SOCKADDR_IN6 {
            sin6_family: windows::Win32::Networking::WinSock::ADDRESS_FAMILY(17),
            ..Default::default()
        };
        let unknown = socket_address(&mut other, size_of::<SOCKADDR_IN6>());
        // SAFETY: as above.
        assert_eq!(unsafe { ip_of(&unknown) }, None);

        let null = SOCKET_ADDRESS {
            lpSockaddr: ptr::null_mut(),
            iSockaddrLength: 16,
        };
        // SAFETY: a null pointer is never read.
        assert_eq!(unsafe { ip_of(&null) }, None);
    }

    #[test]
    fn flush_result_mapping() {
        assert!(map_flush(1, 0).is_ok());
        assert!(
            map_flush(1, 5).is_ok(),
            "success ignores a stale last error"
        );
        assert_eq!(
            map_flush(0, 0).unwrap_err().to_string(),
            "the DNS Client service did not flush its cache"
        );
        let denied = map_flush(0, 5).unwrap_err();
        assert!(matches!(denied, Error::Win32(_)), "{denied:?}");
        assert_eq!(denied.win32_code(), Some(5));
    }

    #[test]
    fn interface_guids_parse_in_any_form() {
        let expected = GUID::from_u128(0xaaaaaaaa_0000_0000_0000_00000000000a);
        for form in [
            "{aaaaaaaa-0000-0000-0000-00000000000a}",
            "AAAAAAAA-0000-0000-0000-00000000000A",
            " {AAAAAAAA-0000-0000-0000-00000000000A} ",
        ] {
            assert_eq!(interface_guid(form).unwrap(), expected, "{form}");
        }
        assert!(interface_guid("Wi-Fi").is_err());
        assert!(interface_guid("{}").is_err());
    }

    #[test]
    fn interface_keys_name_the_tcpip_service_of_the_family() {
        assert_eq!(
            interface_key("AAAAAAAA-0000-0000-0000-00000000000A", IpFamily::Ipv4),
            r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\{aaaaaaaa-0000-0000-0000-00000000000a}"
        );
        assert!(interface_key("{x}", IpFamily::Ipv6)
            .starts_with(r"SYSTEM\CurrentControlSet\Services\Tcpip6\Parameters\Interfaces\"));
    }

    #[test]
    fn index_map_table_is_bounds_checked() {
        let entry = size_of::<IP_ADAPTER_INDEX_MAP>();
        let first = offset_of!(IP_INTERFACE_INFO, Adapter);
        let bytes = first + 2 * entry;
        let mut buf = vec![0u64; bytes.div_ceil(8)];
        let base = buf.as_mut_ptr().cast::<u8>();
        // SAFETY: every write lies inside `buf`, which holds `bytes` or more bytes.
        unsafe {
            ptr::write_unaligned(base.cast::<i32>(), 3);
            ptr::write_unaligned(base.add(first).cast::<u32>(), 7);
            ptr::write_unaligned(base.add(first + entry).cast::<u32>(), 12);
        }
        assert_eq!(find_index_map(&buf, 12).unwrap().Index, 12);
        assert_eq!(find_index_map(&buf, 7).unwrap().Index, 7);
        assert!(
            find_index_map(&buf, 99).is_err(),
            "the third entry is past the buffer"
        );
        assert!(find_index_map(&[], 7).is_err());
    }
}
