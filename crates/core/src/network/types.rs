//! Address families, DNS server lists and interface GUIDs as the journal and reports use
//! them.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// IP address family of a DNS server setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpFamily {
    Ipv4,
    Ipv6,
}

impl IpFamily {
    pub const ALL: [IpFamily; 2] = [IpFamily::Ipv4, IpFamily::Ipv6];

    /// `"ipv4"` or `"ipv6"`, as stored in the journal.
    pub fn as_str(self) -> &'static str {
        match self {
            IpFamily::Ipv4 => "ipv4",
            IpFamily::Ipv6 => "ipv6",
        }
    }

    /// `"IPv4"` or `"IPv6"`, for display.
    pub fn label(self) -> &'static str {
        match self {
            IpFamily::Ipv4 => "IPv4",
            IpFamily::Ipv6 => "IPv6",
        }
    }

    /// Parses [`IpFamily::as_str`] ignoring ASCII case.
    pub fn parse(s: &str) -> Option<IpFamily> {
        IpFamily::ALL
            .into_iter()
            .find(|f| f.as_str().eq_ignore_ascii_case(s.trim()))
    }

    /// Family of `addr`.
    pub fn of(addr: &IpAddr) -> IpFamily {
        match addr {
            IpAddr::V4(_) => IpFamily::Ipv4,
            IpAddr::V6(_) => IpFamily::Ipv6,
        }
    }
}

/// Outcome of writing a recorded DNS baseline back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DnsRestore {
    /// The recorded servers were written.
    Written,
    /// The adapter already had the recorded servers; nothing was written.
    AlreadyInState,
    /// The interface no longer exists on this PC.
    NotFound,
}

/// Splits a server list on commas, semicolons and whitespace; trims, drops empties, keeps order.
pub fn split_servers(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// "automatic" for an empty list, otherwise the servers joined with ", ".
pub fn servers_text(s: &str) -> String {
    let servers = split_servers(s);
    if servers.is_empty() {
        "automatic".to_string()
    } else {
        servers.join(", ")
    }
}

/// "IPv4 DNS servers of Wi-Fi": target of one family's DNS servers in reports and logs.
pub fn dns_target(family: IpFamily, adapter_name: &str) -> String {
    format!("{} DNS servers of {adapter_name}", family.label())
}

/// Canonical interface GUID: braces trimmed, ASCII-lowercased, wrapped as "{...}".
pub fn canonical_guid(s: &str) -> String {
    let inner = s.trim().trim_start_matches('{').trim_end_matches('}');
    format!("{{{}}}", inner.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn ip_family_round_trips() {
        for family in IpFamily::ALL {
            assert_eq!(IpFamily::parse(family.as_str()), Some(family));
            let json = serde_json::to_string(&family).unwrap();
            assert_eq!(json, format!("\"{}\"", family.as_str()));
            assert_eq!(serde_json::from_str::<IpFamily>(&json).unwrap(), family);
        }
        assert_eq!(IpFamily::parse("IPv6"), Some(IpFamily::Ipv6));
        assert_eq!(IpFamily::parse("IPV4"), Some(IpFamily::Ipv4));
        assert_eq!(IpFamily::parse("ipv5"), None);
        assert_eq!(IpFamily::Ipv4.label(), "IPv4");
        assert_eq!(IpFamily::Ipv6.label(), "IPv6");
        assert_eq!(
            IpFamily::of(&IpAddr::V4(Ipv4Addr::LOCALHOST)),
            IpFamily::Ipv4
        );
        assert_eq!(
            IpFamily::of(&IpAddr::V6(Ipv6Addr::LOCALHOST)),
            IpFamily::Ipv6
        );
    }

    #[test]
    fn server_lists_split_and_read_as_text() {
        assert_eq!(
            split_servers("1.1.1.1, 1.0.0.1;  8.8.8.8"),
            vec!["1.1.1.1", "1.0.0.1", "8.8.8.8"]
        );
        assert_eq!(split_servers(" ,; "), Vec::<String>::new());
        assert_eq!(
            split_servers("2606:4700:4700::1111\t::1"),
            vec!["2606:4700:4700::1111", "::1"]
        );
        assert_eq!(servers_text(""), "automatic");
        assert_eq!(servers_text("1.1.1.1,1.0.0.1"), "1.1.1.1, 1.0.0.1");
    }

    #[test]
    fn dns_target_names_family_and_adapter() {
        assert_eq!(
            dns_target(IpFamily::Ipv4, "Wi-Fi"),
            "IPv4 DNS servers of Wi-Fi"
        );
        assert_eq!(
            dns_target(IpFamily::Ipv6, "Ethernet"),
            "IPv6 DNS servers of Ethernet"
        );
    }

    #[test]
    fn guids_are_canonical_whatever_their_form() {
        let bare = canonical_guid("AAAAAAAA-0000-0000-0000-00000000000A");
        let braced_lower = canonical_guid("{aaaaaaaa-0000-0000-0000-00000000000a}");
        let braced_upper = canonical_guid("{AAAAAAAA-0000-0000-0000-00000000000A}");
        assert_eq!(bare, "{aaaaaaaa-0000-0000-0000-00000000000a}");
        assert_eq!(bare, braced_lower);
        assert_eq!(bare, braced_upper);
        assert_eq!(canonical_guid(" {x} "), "{x}");
    }

    #[test]
    fn dns_restore_serializes_in_snake_case() {
        assert_eq!(
            serde_json::to_string(&DnsRestore::AlreadyInState).unwrap(),
            "\"already_in_state\""
        );
    }
}
