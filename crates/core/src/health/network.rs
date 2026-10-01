//! Firewall and network: Windows Firewall, Remote Desktop, Remote Assistance and SMB 1.0,
//! checks 6 to 9.

use super::checkup::{unreadable, Check, CheckId, Severity};
use super::probe::{hklm, hklm_dword, hklm_exists, key_dword, CheckupRaw};
use crate::win::firewall::{self, FirewallState, PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC};
use crate::win::scm::{self, Scm, StartType};
use crate::win::security_center::ProductState;
use crate::Result;

const TERMINAL_SERVER: &str = r"SYSTEM\CurrentControlSet\Control\Terminal Server";
const RDP_TCP: &str = r"SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations\RDP-Tcp";
const TERMINAL_SERVICES_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services";
const REMOTE_ASSISTANCE: &str = r"SYSTEM\CurrentControlSet\Control\Remote Assistance";
const SMB1_CLIENT: &str = r"SYSTEM\CurrentControlSet\Services\mrxsmb10";
const SMB1_SERVER: &str = r"SYSTEM\CurrentControlSet\Services\srv";
const SERVER_PARAMETERS: &str = r"SYSTEM\CurrentControlSet\Services\LanmanServer\Parameters";
/// Remote Desktop's default port.
const RDP_PORT: u32 = 3389;
/// Service start type value of a disabled service.
const DISABLED: u32 = 4;

/// Windows Firewall, its service and the PC's domain membership.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct FirewallRaw {
    pub(crate) state: FirewallState,
    /// Start type of the firewall service (mpssvc).
    pub(crate) service_start: Option<StartType>,
    pub(crate) domain_joined: Option<bool>,
}

/// Remote Desktop, Remote Assistance and SMB 1.0 settings; `None` for a missing value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RemoteRaw {
    pub(crate) deny_connections: Option<u32>,
    pub(crate) deny_connections_policy: Option<u32>,
    /// `UserAuthentication`: Network Level Authentication is required when 1.
    pub(crate) nla: Option<u32>,
    pub(crate) nla_policy: Option<u32>,
    pub(crate) port: Option<u32>,
    pub(crate) assistance: Option<u32>,
    pub(crate) assistance_policy: Option<u32>,
    /// The SMB 1.0 client driver's service key exists.
    pub(crate) smb1_client_key: bool,
    pub(crate) smb1_client_start: Option<u32>,
    /// The SMB 1.0 server driver's service key exists.
    pub(crate) smb1_server_key: bool,
    /// `LanmanServer\Parameters\SMB1`.
    pub(crate) smb1_server: Option<u32>,
}

pub(crate) fn read_firewall() -> Result<FirewallRaw> {
    let state = firewall::read()?;
    let service_start = Scm::connect()
        .ok()
        .and_then(|scm| scm.open("mpssvc", scm::READ_ACCESS).ok().flatten())
        .and_then(|service| service.config().ok())
        .map(|config| config.start_type);
    Ok(FirewallRaw {
        state,
        service_start,
        domain_joined: crate::win::accounts::domain_joined().ok(),
    })
}

pub(crate) fn read_remote() -> Result<RemoteRaw> {
    let policy = hklm(TERMINAL_SERVICES_POLICY);
    let rdp = hklm(RDP_TCP);
    Ok(RemoteRaw {
        deny_connections: hklm_dword(TERMINAL_SERVER, "fDenyTSConnections"),
        deny_connections_policy: key_dword(policy.as_ref(), "fDenyTSConnections"),
        nla: key_dword(rdp.as_ref(), "UserAuthentication"),
        nla_policy: key_dword(policy.as_ref(), "UserAuthentication"),
        port: key_dword(rdp.as_ref(), "PortNumber"),
        assistance: hklm_dword(REMOTE_ASSISTANCE, "fAllowToGetHelp"),
        assistance_policy: key_dword(policy.as_ref(), "fAllowToGetHelp"),
        smb1_client_key: hklm_exists(SMB1_CLIENT),
        smb1_client_start: hklm_dword(SMB1_CLIENT, "Start"),
        smb1_server_key: hklm_exists(SMB1_SERVER),
        smb1_server: hklm_dword(SERVER_PARAMETERS, "SMB1"),
    })
}

/// "Domain", "Private and Public": the profiles whose bits are set, in that order.
fn profile_names(bits: u32) -> String {
    let names: Vec<&str> = [
        (PROFILE_DOMAIN, "Domain"),
        (PROFILE_PRIVATE, "Private"),
        (PROFILE_PUBLIC, "Public"),
    ]
    .into_iter()
    .filter(|(bit, _)| bits & bit != 0)
    .map(|(_, name)| name)
    .collect();
    names.join(" and ")
}

const FIREWALL_DETAIL: &str = "The firewall blocks connections to this PC that you did not \
    allow. Public networks such as café Wi-Fi need it most.";
const FIREWALL_PAGE: &str = "windowsdefender://network/";

/// Check 6: Windows Firewall is on for every network type.
pub(crate) fn firewall(raw: &CheckupRaw) -> Check {
    let check = Check::new(CheckId::Firewall, FIREWALL_DETAIL);
    let fw = match &raw.firewall {
        Ok(fw) => fw,
        Err(e) => {
            return unreadable(CheckId::Firewall, FIREWALL_DETAIL, "Windows Firewall", e)
                .uri("Open Firewall & network protection", FIREWALL_PAGE)
        }
    };
    let state = &fw.state;
    let on_off = |p: &firewall::ProfileState| if p.enabled { "on" } else { "off" };
    let mut check = check;
    if state.current_profiles != 0 {
        check = check.fact("Current network", profile_names(state.current_profiles));
    }
    check = check.fact(
        "Profiles",
        format!(
            "Domain: {}  ·  Private: {}  ·  Public: {}",
            on_off(&state.domain),
            on_off(&state.private),
            on_off(&state.public)
        ),
    );
    if state.policy_managed {
        check = check.fact("Settings", "Managed by Group Policy");
    }
    check = check.uri("Open Firewall & network protection", FIREWALL_PAGE);

    // Security Center lists Windows Firewall itself among the firewall products; only another
    // product that is on replaces the checks below.
    if let Ok(wsc) = &raw.security_center {
        if let Some(product) = wsc
            .firewall
            .iter()
            .find(|p| p.state == ProductState::On && !p.is_windows_firewall())
        {
            return check.good(format!("Protected by {}", product.name.trim()));
        }
    }

    let profiles = [
        (PROFILE_DOMAIN, &state.domain),
        (PROFILE_PRIVATE, &state.private),
        (PROFILE_PUBLIC, &state.public),
    ];
    let active = |bit: u32| state.current_profiles & bit != 0;
    let mut causes: Vec<(Severity, String)> = Vec::new();
    if fw.service_start == Some(StartType::Disabled) {
        causes.push((
            Severity::Critical,
            "The firewall service is disabled".into(),
        ));
    }
    let off_active: u32 = profiles
        .iter()
        .filter(|(bit, p)| active(*bit) && !p.enabled)
        .map(|(bit, _)| bit)
        .sum();
    if off_active != 0 {
        causes.push((
            Severity::Critical,
            format!(
                "Off for the network you are on ({})",
                profile_names(off_active)
            ),
        ));
    }
    if profiles
        .iter()
        .any(|(bit, p)| active(*bit) && p.enabled && p.inbound_allowed)
    {
        causes.push((
            Severity::High,
            "Allows incoming connections by default".into(),
        ));
    }
    if !state.public.enabled && !active(PROFILE_PUBLIC) {
        causes.push((Severity::High, "Off for public networks".into()));
    }
    if !state.private.enabled && !active(PROFILE_PRIVATE) {
        causes.push((Severity::Medium, "Off for private networks".into()));
    }
    if !state.domain.enabled && !active(PROFILE_DOMAIN) && fw.domain_joined == Some(true) {
        causes.push((Severity::Low, "Off for domain networks".into()));
    }
    match worst(causes) {
        Some((severity, summary)) => check.attention(severity, summary),
        None if state.domain.enabled => check.good("On for every network type"),
        None => check.good("On for private and public networks"),
    }
}

/// The most severe cause; the first listed among equals.
pub(crate) fn worst(causes: Vec<(Severity, String)>) -> Option<(Severity, String)> {
    let mut best: Option<(Severity, String)> = None;
    for cause in causes {
        if best.as_ref().map_or(true, |b| cause.0 > b.0) {
            best = Some(cause);
        }
    }
    best
}

const RDP_DETAIL: &str = "Anyone who can reach this PC can try to sign in to it. Leave it on \
    only if you use it.";

/// Check 7: Remote Desktop is off, or at least protected.
pub(crate) fn remote_desktop(raw: &CheckupRaw, home: Option<bool>) -> Check {
    let check = Check::new(CheckId::RemoteDesktop, RDP_DETAIL);
    if home == Some(true) {
        return check.not_applicable("Not available on Windows 11 Home");
    }
    let remote = match &raw.remote {
        Ok(remote) => remote,
        Err(e) => {
            return unreadable(
                CheckId::RemoteDesktop,
                RDP_DETAIL,
                "the Remote Desktop settings",
                e,
            )
        }
    };
    let mut check = check;
    if let Some(port) = remote.port.filter(|p| *p != RDP_PORT) {
        check = check.fact("Port", format!("{port} (not the default)"));
    }
    check = check.uri("Open Remote Desktop settings", "ms-settings:remotedesktop");
    let deny = remote
        .deny_connections_policy
        .or(remote.deny_connections)
        .unwrap_or(1);
    if deny != 0 {
        return check.good("Off");
    }
    let nla = remote.nla_policy.or(remote.nla).unwrap_or(1);
    let public = raw
        .firewall
        .as_ref()
        .ok()
        .and_then(|fw| fw.state.remote_desktop_group)
        .is_some_and(|groups| groups[2]);
    if nla == 0 {
        check.attention(Severity::High, "On without Network Level Authentication")
    } else if public {
        check.attention(Severity::High, "On and reachable from public networks")
    } else {
        check.attention(Severity::Low, "On")
    }
}

const RA_DETAIL: &str = "Remote Assistance lets someone you invite view or control this PC. \
    Scammers often ask people to send an invitation.";

/// Check 8: Remote Assistance invitations.
pub(crate) fn remote_assistance(raw: &CheckupRaw) -> Check {
    let remote = match &raw.remote {
        Ok(remote) => remote,
        Err(e) => {
            return unreadable(
                CheckId::RemoteAssistance,
                RA_DETAIL,
                "the Remote Assistance settings",
                e,
            )
        }
    };
    let check = Check::new(CheckId::RemoteAssistance, RA_DETAIL)
        .tool("Open Remote settings", "remote_settings");
    let allowed = remote.assistance_policy.or(remote.assistance).unwrap_or(0);
    if allowed != 0 {
        check.attention(Severity::Low, "Invitations are allowed")
    } else {
        check.good("Off")
    }
}

const SMB1_DETAIL: &str = "SMB 1.0 is an outdated file-sharing protocol that worms such as \
    WannaCry used to spread. Current versions of Windows don't need it.";

/// Check 9: SMB 1.0 is not installed.
pub(crate) fn smb1(raw: &CheckupRaw) -> Check {
    let remote = match &raw.remote {
        Ok(remote) => remote,
        Err(e) => return unreadable(CheckId::Smb1, SMB1_DETAIL, "the SMB settings", e),
    };
    let check = Check::new(CheckId::Smb1, SMB1_DETAIL).tool_with_note(
        "Open Windows Features",
        "windows_features",
        "Clear “SMB 1.0/CIFS File Sharing Support”.",
    );
    let client = remote.smb1_client_key && remote.smb1_client_start != Some(DISABLED);
    let server = remote.smb1_server_key && remote.smb1_server.unwrap_or(1) != 0;
    if client || server {
        check.attention(Severity::High, "Installed")
    } else {
        check.good("Not installed")
    }
}

#[cfg(test)]
mod tests {
    use super::super::checkup::{CheckState, FixAction};
    use super::super::probe::fixtures::{self, product, windows_firewall};
    use super::*;
    use crate::win::security_center::SecurityProduct;

    fn fw(change: impl FnOnce(&mut FirewallRaw)) -> CheckupRaw {
        let mut raw = fixtures::raw();
        if let Ok(fw) = &mut raw.firewall {
            change(fw);
        }
        raw
    }

    fn remote(change: impl FnOnce(&mut RemoteRaw)) -> CheckupRaw {
        let mut raw = fixtures::raw();
        if let Ok(remote) = &mut raw.remote {
            change(remote);
        }
        raw
    }

    fn outcome(check: &Check) -> (CheckState, Severity, String) {
        (check.state, check.severity, check.summary.clone())
    }

    fn attention(severity: Severity, summary: &str) -> (CheckState, Severity, String) {
        (CheckState::Attention, severity, summary.to_string())
    }

    #[test]
    fn a_firewall_on_everywhere_is_good() {
        let check = firewall(&fixtures::raw());
        assert_eq!(check.state, CheckState::Good);
        assert_eq!(check.summary, "On for every network type");
        assert_eq!(check.facts[0].value, "Public");
        assert_eq!(
            check.facts[1].value,
            "Domain: on  ·  Private: on  ·  Public: on"
        );
        assert_eq!(
            check.fixes[0].action,
            FixAction::Uri {
                uri: FIREWALL_PAGE.into()
            }
        );
    }

    #[test]
    fn firewall_findings() {
        let cases: Vec<(CheckupRaw, (CheckState, Severity, String))> = vec![
            (
                fw(|f| f.service_start = Some(StartType::Disabled)),
                attention(Severity::Critical, "The firewall service is disabled"),
            ),
            (
                fw(|f| f.state.public.enabled = false),
                attention(
                    Severity::Critical,
                    "Off for the network you are on (Public)",
                ),
            ),
            (
                fw(|f| f.state.public.inbound_allowed = true),
                attention(Severity::High, "Allows incoming connections by default"),
            ),
            (
                fw(|f| {
                    f.state.current_profiles = PROFILE_PRIVATE;
                    f.state.public.enabled = false;
                }),
                attention(Severity::High, "Off for public networks"),
            ),
            (
                fw(|f| f.state.private.enabled = false),
                attention(Severity::Medium, "Off for private networks"),
            ),
            (
                fw(|f| {
                    f.state.domain.enabled = false;
                    f.domain_joined = Some(true);
                }),
                attention(Severity::Low, "Off for domain networks"),
            ),
            // Inactive profiles that allow inbound connections are not findings.
            (
                fw(|f| f.state.private.inbound_allowed = true),
                (
                    CheckState::Good,
                    Severity::Critical,
                    "On for every network type".into(),
                ),
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(outcome(&firewall(&raw)), expected);
        }
    }

    #[test]
    fn domain_off_without_a_domain_is_only_a_fact() {
        let raw = fw(|f| {
            f.state.domain.enabled = false;
            f.domain_joined = Some(false);
        });
        let check = firewall(&raw);
        assert_eq!(check.state, CheckState::Good);
        assert_eq!(check.summary, "On for private and public networks");
        assert_eq!(
            check.facts[1].value,
            "Domain: off  ·  Private: on  ·  Public: on"
        );
    }

    #[test]
    fn the_worst_cause_wins() {
        let raw = fw(|f| {
            f.state.private.enabled = false;
            f.state.public.enabled = false;
            f.service_start = Some(StartType::Disabled);
        });
        assert_eq!(firewall(&raw).summary, "The firewall service is disabled");
        let raw = fw(|f| {
            f.state.private.enabled = false;
            f.state.public.inbound_allowed = true;
        });
        assert_eq!(
            outcome(&firewall(&raw)),
            attention(Severity::High, "Allows incoming connections by default")
        );
    }

    #[test]
    fn a_vpn_with_two_active_profiles_checks_both() {
        let raw = fw(|f| {
            f.state.current_profiles = PROFILE_PRIVATE | PROFILE_PUBLIC;
            f.state.private.enabled = false;
            f.state.public.enabled = false;
        });
        let check = firewall(&raw);
        assert_eq!(
            outcome(&check),
            attention(
                Severity::Critical,
                "Off for the network you are on (Private and Public)"
            )
        );
        assert_eq!(check.facts[0].value, "Private and Public");
    }

    #[test]
    fn a_third_party_firewall_replaces_the_findings() {
        let mut raw = fw(|f| f.state.public.enabled = false);
        if let Ok(wsc) = &mut raw.security_center {
            wsc.firewall = vec![product("Fabrikam Firewall", ProductState::On, true)];
        }
        assert_eq!(
            outcome(&firewall(&raw)),
            (
                CheckState::Good,
                Severity::Critical,
                "Protected by Fabrikam Firewall".into()
            )
        );
        if let Ok(wsc) = &mut raw.security_center {
            wsc.firewall = vec![product("Fabrikam Firewall", ProductState::Off, true)];
        }
        assert_eq!(firewall(&raw).state, CheckState::Attention);
    }

    #[test]
    fn windows_firewall_listed_by_security_center_is_not_a_third_party_firewall() {
        let with_products = |raw: &mut CheckupRaw, products: Vec<SecurityProduct>| {
            if let Ok(wsc) = &mut raw.security_center {
                wsc.firewall = products;
            }
        };
        let mut raw = fixtures::raw();
        with_products(&mut raw, vec![windows_firewall(ProductState::On)]);
        assert_eq!(
            outcome(&firewall(&raw)),
            (
                CheckState::Good,
                Severity::Critical,
                "On for every network type".into()
            )
        );
        let mut raw = fw(|f| f.state.public.enabled = false);
        with_products(&mut raw, vec![windows_firewall(ProductState::On)]);
        assert_eq!(
            outcome(&firewall(&raw)),
            attention(
                Severity::Critical,
                "Off for the network you are on (Public)"
            )
        );
        let mut raw = fw(|f| f.state.public.inbound_allowed = true);
        with_products(&mut raw, vec![windows_firewall(ProductState::On)]);
        assert_eq!(
            outcome(&firewall(&raw)),
            attention(Severity::High, "Allows incoming connections by default")
        );
        // A third-party firewall still replaces the findings, before or after Windows' own.
        for products in [
            vec![
                windows_firewall(ProductState::Off),
                product("Fabrikam Firewall", ProductState::On, true),
            ],
            vec![
                product("Fabrikam Firewall", ProductState::On, true),
                windows_firewall(ProductState::On),
            ],
        ] {
            let mut raw = fw(|f| f.state.public.enabled = false);
            with_products(&mut raw, products);
            assert_eq!(firewall(&raw).summary, "Protected by Fabrikam Firewall");
        }
    }

    #[test]
    fn group_policy_is_a_fact_and_a_failed_read_is_unknown() {
        let raw = fw(|f| f.state.policy_managed = true);
        assert!(firewall(&raw)
            .facts
            .iter()
            .any(|f| f.value == "Managed by Group Policy"));
        let mut raw = fixtures::raw();
        raw.firewall = Err("access denied".into());
        let check = firewall(&raw);
        assert_eq!(check.state, CheckState::Unknown);
        assert_eq!(
            check.summary,
            "Could not read Windows Firewall: access denied"
        );
        assert_eq!(check.fixes.len(), 1);
    }

    #[test]
    fn remote_desktop_is_not_on_home() {
        let check = remote_desktop(&remote(|r| r.deny_connections = Some(0)), Some(true));
        assert_eq!(check.state, CheckState::NotApplicable);
        assert_eq!(check.summary, "Not available on Windows 11 Home");
    }

    #[test]
    fn remote_desktop_states() {
        let on = |change: fn(&mut RemoteRaw)| {
            remote(move |r| {
                r.deny_connections = Some(0);
                change(r);
            })
        };
        assert_eq!(
            outcome(&remote_desktop(&fixtures::raw(), Some(false))),
            (CheckState::Good, Severity::High, "Off".into())
        );
        assert_eq!(
            outcome(&remote_desktop(&on(|_| {}), Some(false))),
            attention(Severity::Low, "On")
        );
        assert_eq!(
            outcome(&remote_desktop(&on(|r| r.nla = Some(0)), Some(false))),
            attention(Severity::High, "On without Network Level Authentication")
        );
        let mut public = on(|_| {});
        if let Ok(f) = &mut public.firewall {
            f.state.remote_desktop_group = Some([false, false, true]);
        }
        assert_eq!(
            outcome(&remote_desktop(&public, None)),
            attention(Severity::High, "On and reachable from public networks")
        );
        let check = remote_desktop(&on(|r| r.port = Some(3390)), Some(false));
        assert_eq!(check.facts[0].value, "3390 (not the default)");
        assert!(remote_desktop(&fixtures::raw(), Some(false))
            .facts
            .is_empty());
    }

    #[test]
    fn remote_desktop_policies_win() {
        let raw = remote(|r| {
            r.deny_connections = Some(0);
            r.deny_connections_policy = Some(1);
        });
        assert_eq!(remote_desktop(&raw, Some(false)).summary, "Off");
        let raw = remote(|r| {
            r.deny_connections = Some(1);
            r.deny_connections_policy = Some(0);
            r.nla = Some(1);
            r.nla_policy = Some(0);
        });
        assert_eq!(
            remote_desktop(&raw, Some(false)).summary,
            "On without Network Level Authentication"
        );
        // Missing values mean off.
        let raw = remote(|r| r.deny_connections = None);
        assert_eq!(remote_desktop(&raw, Some(false)).summary, "Off");
    }

    #[test]
    fn remote_assistance_invitations() {
        let check = remote_assistance(&fixtures::raw());
        assert_eq!(check.summary, "Off");
        assert_eq!(
            check.fixes[0].action,
            FixAction::WindowsTool {
                tool: "remote_settings".into(),
                requires_admin: true
            }
        );
        let raw = remote(|r| r.assistance = Some(1));
        assert_eq!(
            outcome(&remote_assistance(&raw)),
            attention(Severity::Low, "Invitations are allowed")
        );
        let raw = remote(|r| {
            r.assistance = Some(1);
            r.assistance_policy = Some(0);
        });
        assert_eq!(remote_assistance(&raw).summary, "Off");
        let raw = remote(|r| r.assistance_policy = Some(1));
        assert_eq!(remote_assistance(&raw).summary, "Invitations are allowed");
    }

    #[test]
    fn smb1_key_combinations() {
        let at = |change: fn(&mut RemoteRaw)| outcome(&smb1(&remote(change)));
        let good = (
            CheckState::Good,
            Severity::High,
            "Not installed".to_string(),
        );
        let installed = attention(Severity::High, "Installed");
        assert_eq!(at(|_| {}), good);
        assert_eq!(at(|r| r.smb1_client_key = true), installed);
        assert_eq!(
            at(|r| {
                r.smb1_client_key = true;
                r.smb1_client_start = Some(4);
            }),
            good
        );
        assert_eq!(
            at(|r| {
                r.smb1_client_key = true;
                r.smb1_client_start = Some(3);
            }),
            installed
        );
        assert_eq!(at(|r| r.smb1_server_key = true), installed);
        assert_eq!(
            at(|r| {
                r.smb1_server_key = true;
                r.smb1_server = Some(0);
            }),
            good
        );
        assert_eq!(at(|r| r.smb1_server = Some(1)), good);
        let check = smb1(&fixtures::raw());
        assert_eq!(
            check.fixes[0].note.as_deref(),
            Some("Clear “SMB 1.0/CIFS File Sharing Support”.")
        );
    }

    #[test]
    fn remote_settings_read_from_this_pc() {
        // Read-only registry reads.
        let raw = read_remote().unwrap();
        assert!(raw.port.map_or(true, |p| p > 0));
    }
}
