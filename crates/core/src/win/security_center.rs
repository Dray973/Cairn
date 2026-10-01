//! Windows Security Center products and provider health.
//!
//! Reads the products registered with Windows Security Center (`IWSCProductList`) and the
//! overall health of a provider (`WscGetSecurityProviderHealth`). Both only read; the calling
//! thread must be in a COM apartment (see `win::com`). They fail while the Security Center
//! service is stopped or disabled.

use windows::core::HRESULT;
use windows::Win32::Foundation::S_OK;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::SecurityCenter::{
    IWSCProductList, WSCProductList, WSC_SECURITY_PRODUCT_STATE,
    WSC_SECURITY_PRODUCT_STATE_EXPIRED, WSC_SECURITY_PRODUCT_STATE_OFF,
    WSC_SECURITY_PRODUCT_STATE_ON, WSC_SECURITY_PRODUCT_STATE_SNOOZED,
    WSC_SECURITY_PRODUCT_UP_TO_DATE, WSC_SECURITY_PROVIDER, WSC_SECURITY_PROVIDER_HEALTH,
    WSC_SECURITY_PROVIDER_HEALTH_GOOD, WSC_SECURITY_PROVIDER_HEALTH_NOTMONITORED,
    WSC_SECURITY_PROVIDER_HEALTH_POOR, WSC_SECURITY_PROVIDER_HEALTH_SNOOZE,
};

pub(crate) use windows::Win32::System::SecurityCenter::{
    WSC_SECURITY_PROVIDER_ANTIVIRUS, WSC_SECURITY_PROVIDER_FIREWALL,
};

use crate::{Error, Result};

// wscapi.dll export (wscapi.h). Declared here for its return code: the windows-rs wrapper
// maps every success code to `Ok(())`, and S_FALSE means the Security Center service is not
// running, so the out value does not describe the provider.
#[link(name = "wscapi.dll", kind = "raw-dylib", modifiers = "+verbatim")]
extern "system" {
    fn WscGetSecurityProviderHealth(
        providers: u32,
        health: *mut WSC_SECURITY_PROVIDER_HEALTH,
    ) -> HRESULT;
}

const NOT_RUNNING: &str = "Windows Security Center is not running";

/// Overall health Security Center reports for one kind of protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderHealth {
    Good,
    NotMonitored,
    Poor,
    Snooze,
    Other(i32),
}

impl ProviderHealth {
    fn from_raw(raw: WSC_SECURITY_PROVIDER_HEALTH) -> ProviderHealth {
        match raw {
            WSC_SECURITY_PROVIDER_HEALTH_GOOD => ProviderHealth::Good,
            WSC_SECURITY_PROVIDER_HEALTH_NOTMONITORED => ProviderHealth::NotMonitored,
            WSC_SECURITY_PROVIDER_HEALTH_POOR => ProviderHealth::Poor,
            WSC_SECURITY_PROVIDER_HEALTH_SNOOZE => ProviderHealth::Snooze,
            other => ProviderHealth::Other(other.0),
        }
    }
}

/// State of one registered product.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProductState {
    On,
    Off,
    Snoozed,
    Expired,
    Other(i32),
}

impl ProductState {
    fn from_raw(raw: WSC_SECURITY_PRODUCT_STATE) -> ProductState {
        match raw {
            WSC_SECURITY_PRODUCT_STATE_ON => ProductState::On,
            WSC_SECURITY_PRODUCT_STATE_OFF => ProductState::Off,
            WSC_SECURITY_PRODUCT_STATE_SNOOZED => ProductState::Snoozed,
            WSC_SECURITY_PRODUCT_STATE_EXPIRED => ProductState::Expired,
            other => ProductState::Other(other.0),
        }
    }

    /// Lower-case word for lists: "on", "off", "snoozed", "expired".
    pub(crate) fn word(self) -> String {
        match self {
            ProductState::On => "on".into(),
            ProductState::Off => "off".into(),
            ProductState::Snoozed => "snoozed".into(),
            ProductState::Expired => "expired".into(),
            ProductState::Other(n) => format!("state {n}"),
        }
    }
}

/// A product registered with Security Center.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SecurityProduct {
    /// Display name as registered; only shown, never parsed.
    pub(crate) name: String,
    pub(crate) state: ProductState,
    /// Meaningful for antivirus products only.
    pub(crate) signatures_up_to_date: bool,
    /// What Windows Security opens to fix the product (`IWscProduct::RemediationPath`);
    /// empty when the product reports none.
    pub(crate) remediation: String,
}

impl SecurityProduct {
    /// Windows' own firewall, which Security Center lists among the firewall products. It is
    /// recognised by its remediation, the Windows Firewall Control Panel item in System32
    /// (`%windir%\system32\firewall.cpl`), not by its display name, which is only shown.
    pub(crate) fn is_windows_firewall(&self) -> bool {
        let path = self
            .remediation
            .trim()
            .trim_matches('"')
            .replace('/', r"\")
            .to_ascii_lowercase();
        path == "firewall.cpl" || path.ends_with(r"\system32\firewall.cpl")
    }
}

/// What `WscGetSecurityProviderHealth` answered. Only `S_OK` carries a health: with any other
/// success code (S_FALSE while the Security Center service is not running) the out value is
/// a fixed answer or still the caller's initial value, which reads as "good".
fn health_reading(code: HRESULT, health: WSC_SECURITY_PROVIDER_HEALTH) -> Result<ProviderHealth> {
    code.ok()?;
    if code != S_OK {
        return Err(Error::Other(NOT_RUNNING.into()));
    }
    Ok(ProviderHealth::from_raw(health))
}

/// Overall health of `provider` (for example `WSC_SECURITY_PROVIDER_ANTIVIRUS`).
pub(crate) fn provider_health(provider: WSC_SECURITY_PROVIDER) -> Result<ProviderHealth> {
    let mut health = WSC_SECURITY_PROVIDER_HEALTH::default();
    // SAFETY: `health` is a valid out pointer for the duration of the call.
    let code = unsafe { WscGetSecurityProviderHealth(provider.0 as u32, &mut health) };
    health_reading(code, health)
}

/// The products registered for `provider`, in Security Center's order.
pub(crate) fn products(provider: WSC_SECURITY_PROVIDER) -> Result<Vec<SecurityProduct>> {
    // SAFETY: the caller's thread is in a COM apartment; the list is released on return.
    let list: IWSCProductList =
        unsafe { CoCreateInstance(&WSCProductList, None, CLSCTX_INPROC_SERVER) }?;
    // SAFETY: `list` is a live interface; `provider` is passed by value.
    unsafe { list.Initialize(provider) }?;
    // SAFETY: as above; the call only returns a value.
    let count = unsafe { list.Count() }?;
    let mut out = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for index in 0..u32::try_from(count).unwrap_or(0) {
        // SAFETY: `index` is below the count the list reported.
        let product = unsafe { list.get_Item(index) }?;
        // SAFETY: `product` is a live interface; each getter only returns a value.
        let (name, state, signatures, remediation) = unsafe {
            (
                product.ProductName()?,
                product.ProductState()?,
                product.SignatureStatus(),
                product.RemediationPath(),
            )
        };
        out.push(SecurityProduct {
            name: name.to_string(),
            state: ProductState::from_raw(state),
            signatures_up_to_date: signatures
                .map(|s| s == WSC_SECURITY_PRODUCT_UP_TO_DATE)
                .unwrap_or(true),
            remediation: remediation.map(|p| p.to_string()).unwrap_or_default(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_states_map_to_their_names() {
        assert_eq!(
            ProductState::from_raw(WSC_SECURITY_PRODUCT_STATE_ON),
            ProductState::On
        );
        assert_eq!(
            ProductState::from_raw(WSC_SECURITY_PRODUCT_STATE_EXPIRED),
            ProductState::Expired
        );
        assert_eq!(
            ProductState::from_raw(WSC_SECURITY_PRODUCT_STATE(9)),
            ProductState::Other(9)
        );
        assert_eq!(ProductState::Snoozed.word(), "snoozed");
        assert_eq!(ProductState::Other(9).word(), "state 9");
        assert_eq!(
            ProviderHealth::from_raw(WSC_SECURITY_PROVIDER_HEALTH_POOR),
            ProviderHealth::Poor
        );
        assert_eq!(
            ProviderHealth::from_raw(WSC_SECURITY_PROVIDER_HEALTH(7)),
            ProviderHealth::Other(7)
        );
    }

    #[test]
    fn only_s_ok_carries_a_health() {
        use windows::Win32::Foundation::{E_INVALIDARG, S_FALSE};
        let untouched = WSC_SECURITY_PROVIDER_HEALTH::default();
        assert_eq!(untouched, WSC_SECURITY_PROVIDER_HEALTH_GOOD);
        // The service is not running: whatever the out value holds is not a reading.
        let stopped = health_reading(S_FALSE, untouched).unwrap_err();
        assert_eq!(stopped.to_string(), NOT_RUNNING);
        assert!(health_reading(S_FALSE, WSC_SECURITY_PROVIDER_HEALTH_POOR).is_err());
        assert!(health_reading(E_INVALIDARG, untouched).is_err());
        assert_eq!(
            health_reading(S_OK, untouched).unwrap(),
            ProviderHealth::Good
        );
        assert_eq!(
            health_reading(S_OK, WSC_SECURITY_PROVIDER_HEALTH_SNOOZE).unwrap(),
            ProviderHealth::Snooze
        );
    }

    fn with_remediation(remediation: &str) -> SecurityProduct {
        SecurityProduct {
            name: "Firewall".into(),
            state: ProductState::On,
            signatures_up_to_date: true,
            remediation: remediation.into(),
        }
    }

    #[test]
    fn windows_own_firewall_is_recognised_by_its_remediation() {
        for path in [
            r"%windir%\system32\firewall.cpl",
            r"C:\WINDOWS\System32\FireWall.cpl",
            "%SystemRoot%/System32/firewall.cpl",
            " \"%windir%\\system32\\firewall.cpl\" ",
            "Firewall.cpl",
        ] {
            assert!(with_remediation(path).is_windows_firewall(), "{path}");
        }
        for path in [
            "",
            "windowsdefender://",
            r"C:\Program Files\Fabrikam\Firewall\fabfw.exe",
            r"C:\Program Files\Fabrikam\firewall.cpl",
            "fabrikam-firewall.cpl",
        ] {
            assert!(!with_remediation(path).is_windows_firewall(), "{path}");
        }
    }

    #[test]
    fn security_center_reads_or_reports_an_error() {
        // Read-only. Security Center may be stopped on a test machine; then the calls fail
        // without side effects.
        let _com = crate::win::com::enter_mta();
        if let Ok(list) = products(WSC_SECURITY_PROVIDER_ANTIVIRUS) {
            assert!(list.iter().all(|p| !p.name.is_empty()));
        }
        // Security Center lists Windows' own firewall among the firewall products.
        if let Ok(list) = products(WSC_SECURITY_PROVIDER_FIREWALL) {
            assert!(list.iter().all(|p| !p.name.is_empty()));
            assert!(
                list.is_empty() || list.iter().any(SecurityProduct::is_windows_firewall),
                "{list:?}"
            );
        }
        for provider in [
            WSC_SECURITY_PROVIDER_ANTIVIRUS,
            WSC_SECURITY_PROVIDER_FIREWALL,
        ] {
            if let Err(e) = provider_health(provider) {
                assert!(!e.to_string().is_empty());
            }
        }
    }
}
