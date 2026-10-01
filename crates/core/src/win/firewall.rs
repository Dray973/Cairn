//! Read-only Windows Firewall state.
//!
//! Reads each profile's switch and default inbound action, the network types in use, whether
//! Group Policy manages the settings and whether the Remote Desktop rule group is enabled,
//! through `INetFwPolicy2` getters only. The calling thread must be in a COM apartment (see
//! `win::com`).

use windows::core::BSTR;
use windows::Win32::NetworkManagement::WindowsFirewall::{
    INetFwPolicy2, NetFwPolicy2, NET_FW_ACTION_ALLOW, NET_FW_MODIFY_STATE_OK,
    NET_FW_PROFILE2_DOMAIN, NET_FW_PROFILE2_PRIVATE, NET_FW_PROFILE2_PUBLIC, NET_FW_PROFILE_TYPE2,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

use crate::Result;

/// Indirect string that names the Remote Desktop rule group; resolved by Windows in the
/// display language, so it works on every locale.
pub(crate) const REMOTE_DESKTOP_GROUP: &str = "@FirewallAPI.dll,-28752";

/// Profile bits of `CurrentProfileTypes`.
pub(crate) const PROFILE_DOMAIN: u32 = 1;
pub(crate) const PROFILE_PRIVATE: u32 = 2;
pub(crate) const PROFILE_PUBLIC: u32 = 4;

/// One firewall profile (domain, private or public networks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ProfileState {
    pub(crate) enabled: bool,
    pub(crate) block_all_inbound: bool,
    /// The default inbound action is Allow.
    pub(crate) inbound_allowed: bool,
}

/// The firewall configuration of this PC.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct FirewallState {
    /// `PROFILE_*` bits of the network types in use.
    pub(crate) current_profiles: u32,
    pub(crate) domain: ProfileState,
    pub(crate) private: ProfileState,
    pub(crate) public: ProfileState,
    /// Group Policy overrides the local settings.
    pub(crate) policy_managed: bool,
    /// Whether the Remote Desktop rule group is enabled for the domain, private and public
    /// profiles, in that order; `None` when it could not be read.
    pub(crate) remote_desktop_group: Option<[bool; 3]>,
}

fn profile(policy: &INetFwPolicy2, kind: NET_FW_PROFILE_TYPE2) -> Result<ProfileState> {
    // SAFETY: `policy` is a live interface; each getter only returns a value.
    unsafe {
        Ok(ProfileState {
            enabled: policy.get_FirewallEnabled(kind)?.as_bool(),
            block_all_inbound: policy.get_BlockAllInboundTraffic(kind)?.as_bool(),
            inbound_allowed: policy.get_DefaultInboundAction(kind)? == NET_FW_ACTION_ALLOW,
        })
    }
}

/// Reads the firewall configuration.
pub(crate) fn read() -> Result<FirewallState> {
    // SAFETY: the caller's thread is in a COM apartment; the policy is released on return.
    let policy: INetFwPolicy2 =
        unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER) }?;
    // SAFETY: `policy` is a live interface; the getters only return values.
    let current = unsafe { policy.CurrentProfileTypes() }?;
    // SAFETY: as above.
    let modify = unsafe { policy.LocalPolicyModifyState() };
    let group = BSTR::from(REMOTE_DESKTOP_GROUP);
    let rdp = |mask: u32| -> Result<bool> {
        // SAFETY: `group` outlives the call; the getter only returns a value.
        Ok(unsafe { policy.IsRuleGroupEnabled(mask as i32, &group) }?.as_bool())
    };
    let remote_desktop_group = (|| -> Result<[bool; 3]> {
        Ok([
            rdp(PROFILE_DOMAIN)?,
            rdp(PROFILE_PRIVATE)?,
            rdp(PROFILE_PUBLIC)?,
        ])
    })()
    .ok();
    Ok(FirewallState {
        current_profiles: current as u32,
        domain: profile(&policy, NET_FW_PROFILE2_DOMAIN)?,
        private: profile(&policy, NET_FW_PROFILE2_PRIVATE)?,
        public: profile(&policy, NET_FW_PROFILE2_PUBLIC)?,
        policy_managed: modify.map(|m| m != NET_FW_MODIFY_STATE_OK).unwrap_or(false),
        remote_desktop_group,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_bits_match_the_firewall_constants() {
        assert_eq!(PROFILE_DOMAIN, NET_FW_PROFILE2_DOMAIN.0 as u32);
        assert_eq!(PROFILE_PRIVATE, NET_FW_PROFILE2_PRIVATE.0 as u32);
        assert_eq!(PROFILE_PUBLIC, NET_FW_PROFILE2_PUBLIC.0 as u32);
    }

    #[test]
    fn the_firewall_configuration_is_readable() {
        // Read-only: getters of INetFwPolicy2 work for a standard user.
        let _com = crate::win::com::enter_mta();
        let state = read().unwrap();
        assert_eq!(state.current_profiles & !7, 0, "{state:?}");
    }

    #[test]
    fn this_module_only_reads() {
        let source = include_str!("firewall.rs");
        let body = source.split("\n#[cfg(test)]").next().unwrap();
        assert!(body.contains("fn read()"));
        for forbidden in [
            concat!("put", "_"),
            concat!("Enable", "RuleGroup"),
            concat!("Restore", "LocalFirewallDefaults"),
            concat!("Rules", "("),
        ] {
            assert!(!body.contains(forbidden), "{forbidden}");
        }
    }
}
