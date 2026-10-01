//! Virus and threat protection: the antivirus products Security Center knows, Microsoft
//! Defender's status, and checks 1 to 5.

use chrono::{DateTime, Utc};

use super::checkup::{Check, CheckId, Severity};
use super::probe::CheckupRaw;
use super::text::{age_text, local_datetime, plural};
use crate::win::security_center::{
    self as wsc, ProductState, ProviderHealth, SecurityProduct, WSC_SECURITY_PROVIDER_ANTIVIRUS,
    WSC_SECURITY_PROVIDER_FIREWALL,
};
use crate::win::wmi::{cim_datetime, is_wbem, WmiConnection, WmiValue};
use crate::Result;

/// WMI namespace and class of Microsoft Defender's status.
const DEFENDER_NAMESPACE: &str = r"ROOT\Microsoft\Windows\Defender";
const DEFENDER_QUERY: &str = "SELECT * FROM MSFT_MpComputerStatus";
/// How long one WMI object may take to arrive.
pub(crate) const WMI_TIMEOUT_MS: i32 = 5000;

const THREAT_PAGE: &str = "windowsdefender://threat/";
const THREAT_SETTINGS_PAGE: &str = "windowsdefender://threatsettings/";

/// What Security Center reports.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct WscRaw {
    pub(crate) antivirus: Vec<SecurityProduct>,
    pub(crate) antivirus_health: Option<ProviderHealth>,
    /// Firewall products, Windows Firewall itself included (see
    /// `SecurityProduct::is_windows_firewall`).
    pub(crate) firewall: Vec<SecurityProduct>,
}

/// Microsoft Defender's running mode (`AMRunningMode`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DefenderMode {
    /// "Normal": Defender is the active antivirus.
    Active,
    /// Another antivirus protects the PC; Defender only scans on demand.
    Passive,
    EdrBlock,
    Other(String),
}

/// Maps `AMRunningMode` (compared ignoring case).
pub(crate) fn defender_mode(raw: &str) -> DefenderMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "normal" => DefenderMode::Active,
        "passive mode" | "sxs passive mode" => DefenderMode::Passive,
        "edr block mode" => DefenderMode::EdrBlock,
        _ => DefenderMode::Other(raw.trim().to_string()),
    }
}

/// `MSFT_MpComputerStatus`; each property is `None` when the platform does not report it.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct DefenderRaw {
    pub(crate) mode: Option<DefenderMode>,
    pub(crate) antivirus_enabled: Option<bool>,
    pub(crate) realtime: Option<bool>,
    pub(crate) tamper_protected: Option<bool>,
    pub(crate) tamper_source: Option<String>,
    pub(crate) signatures_out_of_date: Option<bool>,
    pub(crate) signature_age_days: Option<u32>,
    pub(crate) signature_version: Option<String>,
    pub(crate) signature_updated: Option<DateTime<Utc>>,
    /// `ComputerState` bits of pending threat actions.
    pub(crate) computer_state: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DefenderRead {
    Present(DefenderRaw),
    /// The Defender WMI namespace or class does not exist.
    NotInstalled,
}

/// Security Center's antivirus products and health and its firewall products.
pub(crate) fn read_security_center() -> Result<WscRaw> {
    let antivirus = wsc::products(WSC_SECURITY_PROVIDER_ANTIVIRUS)?;
    Ok(WscRaw {
        antivirus,
        antivirus_health: wsc::provider_health(WSC_SECURITY_PROVIDER_ANTIVIRUS).ok(),
        firewall: wsc::products(WSC_SECURITY_PROVIDER_FIREWALL).unwrap_or_default(),
    })
}

/// Microsoft Defender's status through WMI. `SELECT *` keeps older platforms working;
/// properties they lack are `None`.
pub(crate) fn read_defender() -> Result<DefenderRead> {
    use windows::Win32::System::Wmi::{WBEM_E_INVALID_CLASS, WBEM_E_INVALID_NAMESPACE};
    let missing =
        |e: &crate::Error| is_wbem(e, WBEM_E_INVALID_NAMESPACE) || is_wbem(e, WBEM_E_INVALID_CLASS);
    let conn = match WmiConnection::connect(DEFENDER_NAMESPACE) {
        Ok(conn) => conn,
        Err(e) if missing(&e) => return Ok(DefenderRead::NotInstalled),
        Err(e) => return Err(e),
    };
    let objects = match conn.query(DEFENDER_QUERY, WMI_TIMEOUT_MS) {
        Ok(objects) => objects,
        Err(e) if missing(&e) => return Ok(DefenderRead::NotInstalled),
        Err(e) => return Err(e),
    };
    let Some(obj) = objects.first() else {
        return Ok(DefenderRead::NotInstalled);
    };
    let get = |name: &str| obj.get(name).unwrap_or(WmiValue::Null);
    let text = |name: &str| {
        get(name)
            .as_text()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Ok(DefenderRead::Present(DefenderRaw {
        mode: text("AMRunningMode").map(|m| defender_mode(&m)),
        antivirus_enabled: get("AntivirusEnabled").as_bool(),
        realtime: get("RealTimeProtectionEnabled").as_bool(),
        tamper_protected: get("IsTamperProtected").as_bool(),
        tamper_source: text("TamperProtectionSource"),
        signatures_out_of_date: get("DefenderSignaturesOutOfDate").as_bool(),
        signature_age_days: get("AntivirusSignatureAge").as_u32_bits(),
        signature_version: text("AntivirusSignatureVersion"),
        signature_updated: text("AntivirusSignatureLastUpdated").and_then(|t| cim_datetime(&t)),
        computer_state: get("ComputerState").as_u32_bits(),
    }))
}

fn is_defender(name: &str) -> bool {
    name.to_ascii_lowercase().contains("defender")
}

/// The product name as shown: Security Center still registers Defender as "Windows
/// Defender".
fn display_name(name: &str) -> String {
    if name.trim().eq_ignore_ascii_case("Windows Defender") {
        "Microsoft Defender Antivirus".into()
    } else {
        name.trim().to_string()
    }
}

/// Whether Security Center lists an antivirus other than Microsoft Defender that is on;
/// `None` when Security Center cannot be read.
fn another_antivirus_on(raw: &CheckupRaw) -> Option<bool> {
    raw.security_center.as_ref().ok().map(|w| {
        w.antivirus
            .iter()
            .any(|p| p.state == ProductState::On && !is_defender(&p.name))
    })
}

/// Whether Microsoft Defender is the active antivirus: its running mode is "Normal", or,
/// when Defender's own status cannot be read, no antivirus other than Defender is on in
/// Security Center. `None` when neither can be read.
pub(crate) fn defender_active(raw: &CheckupRaw) -> Option<bool> {
    let by_security_center = || another_antivirus_on(raw).map(|other| !other);
    match &raw.defender {
        Ok(DefenderRead::Present(d)) => match &d.mode {
            Some(mode) => Some(*mode == DefenderMode::Active),
            None => by_security_center(),
        },
        Ok(DefenderRead::NotInstalled) => Some(false),
        Err(_) => by_security_center(),
    }
}

/// Whether the Defender-only checks apply.
enum Gate<'a> {
    Active(&'a DefenderRaw),
    NotApplicable(&'static str),
    Unknown(String),
}

fn defender_gate(raw: &CheckupRaw) -> Gate<'_> {
    let error = || match &raw.defender {
        Err(e) => format!("Could not read Microsoft Defender: {e}"),
        Ok(_) => "Could not read Microsoft Defender".to_string(),
    };
    match (defender_active(raw), &raw.defender) {
        (Some(true), Ok(DefenderRead::Present(d))) => Gate::Active(d),
        (Some(false), Ok(DefenderRead::NotInstalled)) => {
            Gate::NotApplicable("Microsoft Defender Antivirus is not installed")
        }
        // A running mode other than the passive ones does not say that another antivirus
        // protects the PC; only Security Center can.
        (
            Some(false),
            Ok(DefenderRead::Present(DefenderRaw {
                mode: Some(DefenderMode::Other(mode)),
                ..
            })),
        ) if another_antivirus_on(raw) != Some(true) => {
            Gate::Unknown(format!("Microsoft Defender reports its state as “{mode}”"))
        }
        (Some(false), _) => Gate::NotApplicable("Another antivirus is active"),
        _ => Gate::Unknown(error()),
    }
}

fn gated(check: Check, raw: &CheckupRaw, rule: impl FnOnce(Check, &DefenderRaw) -> Check) -> Check {
    match defender_gate(raw) {
        Gate::Active(d) => rule(check, d),
        Gate::NotApplicable(summary) => check.not_applicable(summary),
        Gate::Unknown(summary) => check.unknown(summary),
    }
}

const ANTIVIRUS_DETAIL: &str = "An antivirus checks files and programs for malware as you use \
    them. Without one, malware can run unnoticed.";

/// Check 1: an antivirus is on.
pub(crate) fn antivirus(raw: &CheckupRaw) -> Check {
    let mut check = Check::new(CheckId::Antivirus, ANTIVIRUS_DETAIL);
    let defender = match &raw.defender {
        Ok(DefenderRead::Present(d)) => Some(d),
        _ => None,
    };
    let mut third_party = false;
    if let Ok(wsc) = &raw.security_center {
        if !wsc.antivirus.is_empty() {
            let listed: Vec<String> = wsc
                .antivirus
                .iter()
                .map(|p| format!("{} ({})", display_name(&p.name), p.state.word()))
                .collect();
            check = check.fact("Registered", listed.join("  ·  "));
        }
        third_party = wsc.antivirus.iter().any(|p| !is_defender(&p.name));
    }
    if let Some(mode) = defender.and_then(|d| d.mode.as_ref()) {
        let value = match mode {
            DefenderMode::Active => "active".to_string(),
            DefenderMode::Passive => "passive (another antivirus protects this PC)".to_string(),
            DefenderMode::EdrBlock => "EDR block mode".to_string(),
            DefenderMode::Other(raw) => raw.clone(),
        };
        check = check.fact("Microsoft Defender", value);
    }
    check = check.uri("Open Virus & threat protection", THREAT_PAGE);
    if third_party {
        check = check.uri("Open security providers", "windowsdefender://providers/");
    }
    let defender_on = defender.is_some_and(|d| {
        d.mode == Some(DefenderMode::Active) && d.antivirus_enabled != Some(false)
    });
    match (&raw.security_center, &raw.defender) {
        (Ok(wsc), _) if !wsc.antivirus.is_empty() => from_products(check, &wsc.antivirus),
        (Ok(wsc), _) => {
            if defender_on {
                check.good("Microsoft Defender Antivirus is on")
            } else if wsc.antivirus_health == Some(ProviderHealth::Good) {
                check.good("An antivirus is on")
            } else {
                check.attention(Severity::Critical, "No antivirus is turned on")
            }
        }
        (Err(_), Ok(DefenderRead::Present(d))) => match &d.mode {
            _ if defender_on => check.good("Microsoft Defender Antivirus is on"),
            Some(DefenderMode::Active) => {
                check.attention(Severity::Critical, "No antivirus is turned on")
            }
            Some(DefenderMode::Passive) | Some(DefenderMode::EdrBlock) => {
                check.good("Another antivirus protects this PC")
            }
            _ => {
                let error = raw
                    .security_center
                    .as_ref()
                    .err()
                    .cloned()
                    .unwrap_or_default();
                check.unknown(format!("Could not read Windows Security Center: {error}"))
            }
        },
        (Err(e), _) => check.unknown(format!("Could not read Windows Security Center: {e}")),
    }
}

fn from_products(check: Check, products: &[SecurityProduct]) -> Check {
    let on: Vec<&SecurityProduct> = products
        .iter()
        .filter(|p| p.state == ProductState::On)
        .collect();
    if let Some(first) = on.first() {
        if let Some(stale) = on
            .iter()
            .find(|p| !is_defender(&p.name) && !p.signatures_up_to_date)
        {
            return check.attention(
                Severity::High,
                format!(
                    "{}'s virus definitions are out of date",
                    display_name(&stale.name)
                ),
            );
        }
        let active = on.iter().find(|p| !is_defender(&p.name)).unwrap_or(first);
        return check.good(format!("{} is on", display_name(&active.name)));
    }
    if let Some(snoozed) = products.iter().find(|p| p.state == ProductState::Snoozed) {
        return check.attention(
            Severity::High,
            format!("{} is snoozed", display_name(&snoozed.name)),
        );
    }
    if let [only] = products {
        if only.state == ProductState::Expired {
            return check.attention(
                Severity::Critical,
                format!("{}'s subscription has expired", display_name(&only.name)),
            );
        }
    }
    check.attention(Severity::Critical, "No antivirus is turned on")
}

/// Check 2: Defender's real-time protection.
pub(crate) fn realtime(raw: &CheckupRaw) -> Check {
    let check = Check::new(
        CheckId::RealtimeProtection,
        "Real-time protection checks files when they are downloaded, opened or run. While it \
         is off, malware is only found by scans.",
    )
    .uri("Open protection settings", THREAT_SETTINGS_PAGE);
    gated(check, raw, |check, d| {
        if d.antivirus_enabled == Some(false) {
            return check.attention(Severity::Critical, "Microsoft Defender Antivirus is off");
        }
        match d.realtime {
            Some(true) => check.good("On"),
            Some(false) => check.attention(Severity::Critical, "Off"),
            None => check.unknown("Microsoft Defender did not report this setting"),
        }
    })
}

/// Definitions this many days old or more read as never updated.
const NEVER_UPDATED_DAYS: u32 = 65_535;

/// Check 3: the age of Defender's virus definitions.
pub(crate) fn intelligence(raw: &CheckupRaw, now: DateTime<Utc>) -> Check {
    let check = Check::new(
        CheckId::SecurityIntelligence,
        "Microsoft Defender needs current definitions to recognise new threats. Windows \
         Update normally installs them several times a day.",
    );
    gated(check, raw, |mut check, d| {
        if let Some(version) = &d.signature_version {
            check = check.fact("Version", version.clone());
        }
        if let Some(updated) = d.signature_updated {
            check = check.fact("Last updated", local_datetime(updated));
        }
        check = check.uri("Open Virus & threat protection", THREAT_PAGE);
        let age = d.signature_age_days.or_else(|| {
            d.signature_updated
                .map(|t| super::text::days_between(t, now).clamp(0, i64::from(u32::MAX)) as u32)
        });
        if d.signatures_out_of_date == Some(true) {
            return check.attention(Severity::High, "Out of date");
        }
        match age {
            Some(days) if days >= NEVER_UPDATED_DAYS => {
                check.attention(Severity::High, "Out of date")
            }
            Some(days) if days > 7 => check.attention(
                Severity::High,
                format!("{} old", plural(u64::from(days), "day")),
            ),
            Some(days) if days >= 3 => check.attention(
                Severity::Medium,
                format!("{} old", plural(u64::from(days), "day")),
            ),
            Some(days) => match d.signature_updated {
                Some(updated) => check.good(format!("Updated {}", age_text(updated, now))),
                None if days == 0 => check.good("Updated today"),
                None => check.good(format!("Updated {} ago", plural(u64::from(days), "day"))),
            },
            None => check.unknown("Microsoft Defender did not report the age of its definitions"),
        }
    })
}

/// Check 4: Tamper Protection.
pub(crate) fn tamper(raw: &CheckupRaw) -> Check {
    let check = Check::new(
        CheckId::TamperProtection,
        "Tamper Protection stops malicious apps from turning off Microsoft Defender's \
         protection.",
    );
    gated(check, raw, |mut check, d| {
        if let Some(source) = d.tamper_source.as_deref().filter(|s| !s.is_empty()) {
            check = check.fact("Set by", source);
        }
        check = check.uri("Open protection settings", THREAT_SETTINGS_PAGE);
        match d.tamper_protected {
            Some(true) => check.good("On"),
            Some(false) => check.attention(Severity::Medium, "Off"),
            None => check.unknown("Microsoft Defender did not report this setting"),
        }
    })
}

/// `ComputerState` bits, worst first.
const THREAT_STATES: [(u32, Severity, &str); 5] = [
    (
        16,
        Severity::Critical,
        "Microsoft Defender reports a critical failure",
    ),
    (
        8,
        Severity::High,
        "A Microsoft Defender Offline scan is needed",
    ),
    (4, Severity::High, "A threat needs your action"),
    (2, Severity::Medium, "Restart to finish removing a threat"),
    (1, Severity::Medium, "A full scan is needed"),
];

/// Check 5: threat actions Defender is waiting for.
pub(crate) fn threat_actions(raw: &CheckupRaw) -> Check {
    let check = Check::new(
        CheckId::ThreatActions,
        "Microsoft Defender found something it could not finish handling on its own.",
    )
    .uri("Open Protection history", "windowsdefender://history");
    gated(check, raw, |check, d| {
        let Some(state) = d.computer_state else {
            return check.unknown("Microsoft Defender did not report its state");
        };
        match THREAT_STATES.iter().find(|(bit, _, _)| state & bit != 0) {
            Some((_, severity, summary)) => check.attention(*severity, *summary),
            None => check.good("Nothing waiting"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::checkup::{CheckState, FixAction};
    use super::super::probe::fixtures::{self, now, product, with_defender};
    use super::*;

    fn with_products(products: Vec<SecurityProduct>) -> CheckupRaw {
        let mut raw = fixtures::raw();
        if let Ok(wsc) = &mut raw.security_center {
            wsc.antivirus = products;
        }
        raw
    }

    fn summary(check: &Check) -> (CheckState, Severity, &str) {
        (check.state, check.severity, check.summary.as_str())
    }

    #[test]
    fn running_modes_are_mapped_ignoring_case() {
        assert_eq!(defender_mode("Normal"), DefenderMode::Active);
        assert_eq!(defender_mode(" normal "), DefenderMode::Active);
        assert_eq!(defender_mode("Passive Mode"), DefenderMode::Passive);
        assert_eq!(defender_mode("SxS Passive Mode"), DefenderMode::Passive);
        assert_eq!(defender_mode("EDR Block Mode"), DefenderMode::EdrBlock);
        assert_eq!(
            defender_mode("Not running"),
            DefenderMode::Other("Not running".into())
        );
    }

    #[test]
    fn defender_on_is_good_and_named_as_shown_today() {
        let check = antivirus(&fixtures::raw());
        assert_eq!(
            summary(&check),
            (
                CheckState::Good,
                Severity::Critical,
                "Microsoft Defender Antivirus is on"
            )
        );
        assert_eq!(check.facts[0].label, "Registered");
        assert_eq!(check.facts[0].value, "Microsoft Defender Antivirus (on)");
        assert_eq!(check.facts[1].value, "active");
        assert_eq!(check.fixes.len(), 1);
    }

    #[test]
    fn a_third_party_antivirus_is_named_and_makes_defender_checks_not_applicable() {
        let mut raw = with_products(vec![
            product("Windows Defender", ProductState::Off, true),
            product("Contoso Antivirus", ProductState::On, true),
        ]);
        with_defender(&mut raw, |d| d.mode = Some(DefenderMode::Passive));
        let check = antivirus(&raw);
        assert_eq!(check.summary, "Contoso Antivirus is on");
        assert_eq!(
            check.facts[0].value,
            "Microsoft Defender Antivirus (off)  ·  Contoso Antivirus (on)"
        );
        assert_eq!(
            check.facts[1].value,
            "passive (another antivirus protects this PC)"
        );
        assert!(check.fixes.iter().any(|f| f.action
            == FixAction::Uri {
                uri: "windowsdefender://providers/".into()
            }));
        for check in [
            realtime(&raw),
            intelligence(&raw, now()),
            tamper(&raw),
            threat_actions(&raw),
        ] {
            assert_eq!(check.state, CheckState::NotApplicable, "{:?}", check.id);
            assert_eq!(check.summary, "Another antivirus is active");
        }
        with_defender(&mut raw, |d| d.mode = Some(DefenderMode::EdrBlock));
        assert_eq!(realtime(&raw).state, CheckState::NotApplicable);
    }

    #[test]
    fn an_unrecognised_defender_state_is_unknown_unless_another_antivirus_is_on() {
        let defender_checks = |raw: &CheckupRaw| {
            [
                realtime(raw),
                intelligence(raw, now()),
                tamper(raw),
                threat_actions(raw),
            ]
        };
        let mut raw = with_products(vec![product("Windows Defender", ProductState::Off, true)]);
        with_defender(&mut raw, |d| d.mode = Some(defender_mode("Not running")));
        for check in defender_checks(&raw) {
            assert_eq!(check.state, CheckState::Unknown, "{:?}", check.id);
            assert_eq!(
                check.summary,
                "Microsoft Defender reports its state as “Not running”"
            );
        }
        assert_eq!(antivirus(&raw).summary, "No antivirus is turned on");
        // Security Center cannot be read: nothing says another antivirus protects the PC.
        let mut unread = raw.clone();
        unread.security_center = Err("Security Center did not answer within 8 s".into());
        assert_eq!(realtime(&unread).state, CheckState::Unknown);
        // Another antivirus that is on explains the state.
        if let Ok(wsc) = &mut raw.security_center {
            wsc.antivirus
                .push(product("Contoso Antivirus", ProductState::On, true));
        }
        for check in defender_checks(&raw) {
            assert_eq!(check.state, CheckState::NotApplicable, "{:?}", check.id);
            assert_eq!(check.summary, "Another antivirus is active");
        }
        // One that is off does not.
        if let Ok(wsc) = &mut raw.security_center {
            wsc.antivirus[1].state = ProductState::Off;
        }
        assert_eq!(realtime(&raw).state, CheckState::Unknown);
    }

    #[test]
    fn third_party_states() {
        let cases = [
            (
                vec![product("Contoso Antivirus", ProductState::Snoozed, true)],
                CheckState::Attention,
                Severity::High,
                "Contoso Antivirus is snoozed",
            ),
            (
                vec![product("Contoso Antivirus", ProductState::Expired, true)],
                CheckState::Attention,
                Severity::Critical,
                "Contoso Antivirus's subscription has expired",
            ),
            (
                vec![product("Contoso Antivirus", ProductState::On, false)],
                CheckState::Attention,
                Severity::High,
                "Contoso Antivirus's virus definitions are out of date",
            ),
            (
                vec![
                    product("Windows Defender", ProductState::Off, true),
                    product("Contoso Antivirus", ProductState::Off, true),
                ],
                CheckState::Attention,
                Severity::Critical,
                "No antivirus is turned on",
            ),
            (
                vec![
                    product("Windows Defender", ProductState::Off, true),
                    product("Contoso Antivirus", ProductState::Expired, true),
                ],
                CheckState::Attention,
                Severity::Critical,
                "No antivirus is turned on",
            ),
        ];
        for (products, state, severity, text) in cases {
            let check = antivirus(&with_products(products));
            assert_eq!(summary(&check), (state, severity, text));
        }
        // Defender's own definitions are covered by the definitions check.
        let check = antivirus(&with_products(vec![product(
            "Windows Defender",
            ProductState::On,
            false,
        )]));
        assert_eq!(check.state, CheckState::Good);
    }

    #[test]
    fn no_registered_product() {
        let mut raw = with_products(Vec::new());
        with_defender(&mut raw, |d| d.antivirus_enabled = Some(false));
        if let Ok(wsc) = &mut raw.security_center {
            wsc.antivirus_health = Some(ProviderHealth::Poor);
        }
        assert_eq!(
            summary(&antivirus(&raw)),
            (
                CheckState::Attention,
                Severity::Critical,
                "No antivirus is turned on"
            )
        );
        // Security Center is not running, so it reports no health: nothing says one is on.
        if let Ok(wsc) = &mut raw.security_center {
            wsc.antivirus_health = None;
        }
        assert_eq!(antivirus(&raw).summary, "No antivirus is turned on");
        if let Ok(wsc) = &mut raw.security_center {
            wsc.antivirus_health = Some(ProviderHealth::Good);
        }
        assert_eq!(antivirus(&raw).summary, "An antivirus is on");
        let raw = with_products(Vec::new());
        assert_eq!(
            antivirus(&raw).summary,
            "Microsoft Defender Antivirus is on"
        );
    }

    #[test]
    fn defender_wmi_missing_with_security_center_readable() {
        let mut raw = fixtures::raw();
        raw.defender = Ok(DefenderRead::NotInstalled);
        assert_eq!(
            antivirus(&raw).summary,
            "Microsoft Defender Antivirus is on"
        );
        assert_eq!(
            realtime(&raw).summary,
            "Microsoft Defender Antivirus is not installed"
        );
        raw.defender = Err("WMI did not answer".into());
        assert_eq!(antivirus(&raw).state, CheckState::Good);
        let check = realtime(&raw);
        assert_eq!(check.state, CheckState::Unknown);
        assert_eq!(
            check.summary,
            "Could not read Microsoft Defender: WMI did not answer"
        );
    }

    #[test]
    fn security_center_missing_falls_back_to_defender() {
        let mut raw = fixtures::raw();
        raw.security_center = Err("the Security Center service is stopped".into());
        assert_eq!(
            summary(&antivirus(&raw)),
            (
                CheckState::Good,
                Severity::Critical,
                "Microsoft Defender Antivirus is on"
            )
        );
        with_defender(&mut raw, |d| d.antivirus_enabled = Some(false));
        assert_eq!(antivirus(&raw).summary, "No antivirus is turned on");
        with_defender(&mut raw, |d| d.mode = Some(DefenderMode::Passive));
        assert_eq!(
            antivirus(&raw).summary,
            "Another antivirus protects this PC"
        );
    }

    #[test]
    fn both_sources_failing_is_unknown() {
        let mut raw = fixtures::raw();
        raw.security_center = Err("Security Center did not answer within 8 s".into());
        raw.defender = Err("x".into());
        let check = antivirus(&raw);
        assert_eq!(check.state, CheckState::Unknown);
        assert_eq!(
            check.summary,
            "Could not read Windows Security Center: Security Center did not answer within 8 s"
        );
        for check in [
            realtime(&raw),
            intelligence(&raw, now()),
            tamper(&raw),
            threat_actions(&raw),
        ] {
            assert_eq!(check.state, CheckState::Unknown, "{:?}", check.id);
        }
    }

    #[test]
    fn realtime_protection() {
        assert_eq!(realtime(&fixtures::raw()).summary, "On");
        let mut raw = fixtures::raw();
        with_defender(&mut raw, |d| d.realtime = Some(false));
        assert_eq!(
            summary(&realtime(&raw)),
            (CheckState::Attention, Severity::Critical, "Off")
        );
        with_defender(&mut raw, |d| d.antivirus_enabled = Some(false));
        assert_eq!(
            realtime(&raw).summary,
            "Microsoft Defender Antivirus is off"
        );
    }

    #[test]
    fn definition_ages() {
        let at = |age: u32, out_of_date: bool| {
            let mut raw = fixtures::raw();
            with_defender(&mut raw, |d| {
                d.signature_age_days = Some(age);
                d.signatures_out_of_date = Some(out_of_date);
            });
            let check = intelligence(&raw, now());
            (check.state, check.severity, check.summary)
        };
        assert_eq!(
            at(2, false),
            (
                CheckState::Good,
                Severity::High,
                "Updated 17 hours ago".to_string()
            )
        );
        assert_eq!(
            at(3, false),
            (CheckState::Attention, Severity::Medium, "3 days old".into())
        );
        assert_eq!(
            at(7, false),
            (CheckState::Attention, Severity::Medium, "7 days old".into())
        );
        assert_eq!(
            at(8, false),
            (CheckState::Attention, Severity::High, "8 days old".into())
        );
        assert_eq!(
            at(0, true),
            (CheckState::Attention, Severity::High, "Out of date".into())
        );
        assert_eq!(
            at(65_535, false),
            (CheckState::Attention, Severity::High, "Out of date".into())
        );
        let check = intelligence(&fixtures::raw(), now());
        assert_eq!(check.facts[0].label, "Version");
        assert_eq!(check.facts[0].value, "1.459.440.0");
        assert_eq!(check.facts[1].label, "Last updated");
    }

    #[test]
    fn tamper_protection() {
        assert_eq!(tamper(&fixtures::raw()).summary, "On");
        let mut raw = fixtures::raw();
        with_defender(&mut raw, |d| {
            d.tamper_protected = Some(false);
            d.tamper_source = Some("Intune".into());
        });
        let check = tamper(&raw);
        assert_eq!(
            summary(&check),
            (CheckState::Attention, Severity::Medium, "Off")
        );
        assert_eq!(check.facts[0].label, "Set by");
        assert_eq!(check.facts[0].value, "Intune");
    }

    #[test]
    fn threat_actions_take_the_worst_bit() {
        let at = |state: u32| {
            let mut raw = fixtures::raw();
            with_defender(&mut raw, |d| d.computer_state = Some(state));
            let check = threat_actions(&raw);
            (check.state, check.severity, check.summary)
        };
        assert_eq!(
            at(0),
            (
                CheckState::Good,
                Severity::Critical,
                "Nothing waiting".to_string()
            )
        );
        let expected = [
            (1, Severity::Medium, "A full scan is needed"),
            (2, Severity::Medium, "Restart to finish removing a threat"),
            (4, Severity::High, "A threat needs your action"),
            (
                8,
                Severity::High,
                "A Microsoft Defender Offline scan is needed",
            ),
            (
                16,
                Severity::Critical,
                "Microsoft Defender reports a critical failure",
            ),
        ];
        for (bit, severity, text) in expected {
            assert_eq!(at(bit), (CheckState::Attention, severity, text.to_string()));
        }
        assert_eq!(at(1 | 4).2, "A threat needs your action");
        assert_eq!(at(2 | 16).1, Severity::Critical);
        assert_eq!(at(1 | 2).2, "Restart to finish removing a threat");
        let mut raw = fixtures::raw();
        with_defender(&mut raw, |d| d.computer_state = None);
        assert_eq!(threat_actions(&raw).state, CheckState::Unknown);
    }
}
