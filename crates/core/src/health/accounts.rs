//! Accounts and sign-in: User Account Control, whether the signed-in user is an
//! administrator, the built-in accounts and automatic sign-in; checks 17 to 20.
//!
//! Account names are never shown for the signed-in user. The Winlogon `DefaultPassword`
//! value is only checked for existence through `Key::value_info`; its data is never read.

use super::checkup::{unreadable, Check, CheckId, Severity};
use super::network::worst;
use super::probe::{hklm, key_dword, key_text, CheckupRaw, Context};
use super::text::plural;
use crate::win::accounts::{self as win_accounts, ElevationType, LocalAccount};
use crate::Result;

const UAC_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
const WINLOGON: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon";
/// Checked for existence only.
const DEFAULT_PASSWORD: &str = "DefaultPassword";
/// Relative ids of the built-in Administrator and Guest accounts.
const ADMINISTRATOR_RID: u32 = 500;
const GUEST_RID: u32 = 501;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountsRaw {
    pub(crate) enable_lua: Option<u32>,
    /// `ConsentPromptBehaviorAdmin`.
    pub(crate) consent_prompt: Option<u32>,
    /// `PromptOnSecureDesktop`.
    pub(crate) secure_desktop: Option<u32>,
    pub(crate) filter_admin_token: Option<u32>,
    /// `TypeOfAdminApprovalMode`: 2 is Administrator protection; `None` on builds without it.
    pub(crate) admin_approval_mode: Option<u32>,
    pub(crate) token: std::result::Result<ElevationType, String>,
    /// Whether the signed-in user is in the Administrators group; read only when Cairn may
    /// run as another account and a user is signed in.
    pub(crate) session_admin: Option<std::result::Result<bool, String>>,
    pub(crate) local_accounts: std::result::Result<Vec<LocalAccount>, String>,
    pub(crate) auto_logon: Option<String>,
    pub(crate) auto_logon_count: Option<u32>,
    /// A `DefaultPassword` value exists.
    pub(crate) default_password: bool,
}

pub(crate) fn read_accounts(ctx: &Context) -> Result<AccountsRaw> {
    let uac = hklm(UAC_KEY);
    let winlogon = hklm(WINLOGON);
    let session_admin = match (&ctx.session, ctx.other_user) {
        (_, Some(false)) => None,
        (Ok(Some(user)), _) => Some(
            win_accounts::user_is_local_admin(&user.domain, &user.name).map_err(|e| e.to_string()),
        ),
        _ => None,
    };
    let auto_logon = key_text(winlogon.as_ref(), "AutoAdminLogon")
        .or_else(|| key_dword(winlogon.as_ref(), "AutoAdminLogon").map(|v| v.to_string()));
    let default_password = match &winlogon {
        Some(key) => key.value_info(DEFAULT_PASSWORD)?.is_some(),
        None => false,
    };
    Ok(AccountsRaw {
        enable_lua: key_dword(uac.as_ref(), "EnableLUA"),
        consent_prompt: key_dword(uac.as_ref(), "ConsentPromptBehaviorAdmin"),
        secure_desktop: key_dword(uac.as_ref(), "PromptOnSecureDesktop"),
        filter_admin_token: key_dword(uac.as_ref(), "FilterAdministratorToken"),
        admin_approval_mode: key_dword(uac.as_ref(), "TypeOfAdminApprovalMode"),
        token: win_accounts::token_elevation_type().map_err(|e| e.to_string()),
        session_admin,
        local_accounts: win_accounts::local_accounts().map_err(|e| e.to_string()),
        auto_logon,
        auto_logon_count: key_dword(winlogon.as_ref(), "AutoLogonCount"),
        default_password,
    })
}

fn accounts_raw<'a>(
    raw: &'a CheckupRaw,
    id: CheckId,
    detail: &str,
) -> std::result::Result<&'a AccountsRaw, Box<Check>> {
    raw.accounts
        .as_ref()
        .map_err(|e| Box::new(unreadable(id, detail, "the account settings", e)))
}

const UAC_DETAIL: &str = "User Account Control asks before apps get administrator rights.";

/// Check 17: User Account Control.
pub(crate) fn uac(raw: &CheckupRaw) -> Check {
    let a = match accounts_raw(raw, CheckId::Uac, UAC_DETAIL) {
        Ok(a) => a,
        Err(check) => return (*check).tool("Open UAC settings", "uac_settings"),
    };
    let mut check = Check::new(CheckId::Uac, UAC_DETAIL);
    if let Some(mode) = a.admin_approval_mode {
        check = check.fact(
            "Administrator protection",
            if mode == 2 { "on" } else { "off" },
        );
    }
    let check = check.tool("Open UAC settings", "uac_settings");
    let lua = a.enable_lua.unwrap_or(1);
    let consent = a.consent_prompt.unwrap_or(5);
    let secure = a.secure_desktop.unwrap_or(1);
    if lua == 0 {
        return check
            .detail("Every app runs with full administrator rights, and Microsoft Store apps don't work.")
            .attention(Severity::Critical, "Off");
    }
    if consent == 0 {
        return check
            .detail("Apps get administrator rights without asking you.")
            .attention(Severity::High, "Never notify");
    }
    if matches!(consent, 2 | 4 | 5) && secure == 0 {
        return check
            .detail("Other apps can interfere with the prompt while the desktop isn't dimmed.")
            .attention(Severity::Low, "Notify without dimming the desktop");
    }
    match consent {
        5 => check.good("Notify when apps make changes (default)"),
        2 => check.good("Always notify"),
        1 | 3 => check.good("Asks for a password"),
        4 => check.good("Asks for consent"),
        other => check.good(format!("Custom ({other})")),
    }
}

/// Whether the signed-in user is an administrator.
enum Membership {
    Admin(bool),
    NoSession,
    Failed(String),
}

fn membership(a: &AccountsRaw, ctx: &Context) -> Membership {
    match &ctx.session {
        Ok(None) => return Membership::NoSession,
        Err(e) if ctx.other_user != Some(false) => return Membership::Failed(e.clone()),
        _ => {}
    }
    if ctx.other_user == Some(false) {
        if ctx.elevated {
            return Membership::Admin(true);
        }
        return match &a.token {
            Ok(ElevationType::Limited | ElevationType::Full) => Membership::Admin(true),
            Ok(ElevationType::Default) => Membership::Admin(false),
            Err(e) => Membership::Failed(e.clone()),
        };
    }
    match &a.session_admin {
        Some(Ok(admin)) => Membership::Admin(*admin),
        Some(Err(e)) => Membership::Failed(e.clone()),
        None => Membership::Failed("the signed-in account's groups were not read".into()),
    }
}

const ADMIN_DETAIL: &str = "Apps you run can ask for full control of the PC with one click. \
    Everyday use with a standard account, and a separate administrator account for changes, \
    stops that.";

/// Check 18: the signed-in user's account type.
pub(crate) fn admin_account(raw: &CheckupRaw, ctx: &Context) -> Check {
    let a = match accounts_raw(raw, CheckId::AdminAccount, ADMIN_DETAIL) {
        Ok(a) => a,
        Err(check) => return (*check).uri("Open Other users", "ms-settings:otherusers"),
    };
    let mut check = Check::new(CheckId::AdminAccount, ADMIN_DETAIL)
        .uri("Open Other users", "ms-settings:otherusers");
    if a.admin_approval_mode.is_some() {
        check = check.uri(
            "Open Administrator protection",
            "windowsdefender://administratorprotection/",
        );
    }
    match membership(a, ctx) {
        Membership::NoSession => check.unknown("No user is signed in to this session"),
        Membership::Failed(e) => check.unknown(format!("Could not check: {e}")),
        Membership::Admin(false) => check.good("Standard account"),
        Membership::Admin(true) if a.admin_approval_mode == Some(2) => {
            check.good("Administrator, with Administrator protection")
        }
        Membership::Admin(true) => check.attention(Severity::Low, "Administrator account"),
    }
}

const BUILTIN_DETAIL: &str =
    "Windows' built-in Administrator and Guest accounts are known to every attacker and \
     should stay disabled.";
/// Applies only while the built-in Administrator runs without Admin Approval Mode.
const UNPROMPTED_DETAIL: &str = "It runs everything with full rights and never shows UAC prompts.";

/// Check 19: the built-in Administrator and Guest accounts are disabled.
pub(crate) fn builtin_accounts(raw: &CheckupRaw) -> Check {
    let a = match accounts_raw(raw, CheckId::BuiltinAccounts, BUILTIN_DETAIL) {
        Ok(a) => a,
        Err(check) => return *check,
    };
    let check = Check::new(CheckId::BuiltinAccounts, BUILTIN_DETAIL);
    let accounts = match &a.local_accounts {
        Ok(accounts) => accounts,
        Err(e) => return check.unknown(format!("Could not read the local accounts: {e}")),
    };
    let enabled = |rid: u32| accounts.iter().find(|acc| acc.rid == rid && acc.enabled);
    let admin = enabled(ADMINISTRATOR_RID);
    let guest = enabled(GUEST_RID);
    let mut causes = Vec::new();
    let mut detail = Vec::new();
    if admin.is_some() {
        // With `FilterAdministratorToken` = 1 the account runs in Admin Approval Mode and
        // gets UAC prompts like any other administrator.
        let prompted = a.filter_admin_token == Some(1);
        let severity = if prompted {
            Severity::Medium
        } else {
            Severity::High
        };
        causes.push((
            severity,
            "The built-in Administrator account is enabled".to_string(),
        ));
        if !prompted {
            detail.push(UNPROMPTED_DETAIL.to_string());
        }
    }
    if guest.is_some() {
        causes.push((Severity::Medium, "The Guest account is enabled".to_string()));
    }
    let Some((severity, summary)) = worst(causes) else {
        return check.good("Administrator and Guest are disabled");
    };
    if detail.is_empty() {
        detail.push(BUILTIN_DETAIL.to_string());
    }
    for account in [admin, guest].into_iter().flatten() {
        detail.push(format!(
            "Turn it off in an administrator terminal with: net user \"{}\" /active:no",
            account.name
        ));
    }
    check.detail(detail.join(" ")).attention(severity, summary)
}

const AUTO_SIGN_IN_NOTE: &str = "Tick “Users must enter a user name and password to use this \
    computer”. If the box is missing, first turn off “For improved security, only allow \
    Windows Hello sign-in” in Sign-in options.";

/// Check 20: automatic sign-in.
pub(crate) fn auto_sign_in(raw: &CheckupRaw) -> Check {
    const DETAIL: &str = "Automatic sign-in signs a user in without a password when the PC starts.";
    let a = match accounts_raw(raw, CheckId::AutoSignIn, DETAIL) {
        Ok(a) => a,
        Err(check) => return *check,
    };
    let on = a.auto_logon.as_deref().map(str::trim) == Some("1");
    let mut check = Check::new(CheckId::AutoSignIn, DETAIL);
    if let Some(count) = a.auto_logon_count.filter(|_| on) {
        check = check.line(format!(
            "For the next {}",
            plural(u64::from(count), "sign-in")
        ));
    }
    let check = check
        .tool_with_note("Open User Accounts", "user_accounts", AUTO_SIGN_IN_NOTE)
        .uri("Open Sign-in options", "ms-settings:signinoptions");
    if !on {
        return check.good("Off");
    }
    if a.default_password {
        check
            .detail(
                "Anyone who can read the registry can read this password, and anyone who turns \
                 the PC on is signed in.",
            )
            .attention(
                Severity::High,
                "On, with the password stored in the registry",
            )
    } else {
        check
            .detail("Anyone who turns the PC on is signed in without a password.")
            .attention(Severity::Medium, "On")
    }
}

#[cfg(test)]
mod tests {
    use super::super::checkup::{CheckState, FixAction};
    use super::super::probe::fixtures::{self, account, user};
    use super::*;

    fn accounts(change: impl FnOnce(&mut AccountsRaw)) -> CheckupRaw {
        let mut raw = fixtures::raw();
        if let Ok(a) = &mut raw.accounts {
            change(a);
        }
        raw
    }

    fn outcome(check: &Check) -> (CheckState, Severity, String) {
        (check.state, check.severity, check.summary.clone())
    }

    #[test]
    fn uac_mapping() {
        let at = |lua: Option<u32>, consent: Option<u32>, secure: Option<u32>| {
            outcome(&uac(&accounts(|a| {
                a.enable_lua = lua;
                a.consent_prompt = consent;
                a.secure_desktop = secure;
            })))
        };
        let good = |s: &str| (CheckState::Good, Severity::Critical, s.to_string());
        let attention = |severity, s: &str| (CheckState::Attention, severity, s.to_string());
        assert_eq!(
            at(Some(0), Some(5), Some(1)),
            attention(Severity::Critical, "Off")
        );
        assert_eq!(
            at(Some(1), Some(0), Some(1)),
            attention(Severity::High, "Never notify")
        );
        for consent in [2, 4, 5] {
            assert_eq!(
                at(Some(1), Some(consent), Some(0)),
                attention(Severity::Low, "Notify without dimming the desktop")
            );
        }
        assert_eq!(
            at(Some(1), Some(5), Some(1)),
            good("Notify when apps make changes (default)")
        );
        assert_eq!(at(Some(1), Some(2), Some(1)), good("Always notify"));
        assert_eq!(at(Some(1), Some(1), Some(0)), good("Asks for a password"));
        assert_eq!(at(Some(1), Some(3), Some(1)), good("Asks for a password"));
        assert_eq!(at(Some(1), Some(4), Some(1)), good("Asks for consent"));
        assert_eq!(at(Some(1), Some(7), Some(1)), good("Custom (7)"));
        // Missing values mean the defaults 1, 5 and 1.
        assert_eq!(
            at(None, None, None),
            good("Notify when apps make changes (default)")
        );
        let off = uac(&accounts(|a| a.enable_lua = Some(0)));
        assert_eq!(
            off.detail,
            "Every app runs with full administrator rights, and Microsoft Store apps don't work."
        );
        assert_eq!(
            off.fixes[0].action,
            FixAction::WindowsTool {
                tool: "uac_settings".into(),
                requires_admin: false
            }
        );
    }

    #[test]
    fn administrator_protection_is_a_uac_fact() {
        assert!(uac(&fixtures::raw()).facts.is_empty());
        let check = uac(&accounts(|a| a.admin_approval_mode = Some(2)));
        assert_eq!(check.facts[0].label, "Administrator protection");
        assert_eq!(check.facts[0].value, "on");
        let check = uac(&accounts(|a| a.admin_approval_mode = Some(1)));
        assert_eq!(check.facts[0].value, "off");
    }

    fn ctx(elevated: bool, other_user: Option<bool>) -> Context {
        Context::new(elevated, other_user, false, Ok(Some(user())))
    }

    #[test]
    fn the_account_type_of_cairn_s_own_user_comes_from_the_token() {
        let at = |elevated: bool, token: std::result::Result<ElevationType, String>| {
            let raw = accounts(|a| a.token = token);
            outcome(&admin_account(&raw, &ctx(elevated, Some(false))))
        };
        let admin = (
            CheckState::Attention,
            Severity::Low,
            "Administrator account".to_string(),
        );
        assert_eq!(at(true, Ok(ElevationType::Default)), admin);
        assert_eq!(at(false, Ok(ElevationType::Limited)), admin);
        assert_eq!(at(false, Ok(ElevationType::Full)), admin);
        assert_eq!(
            at(false, Ok(ElevationType::Default)),
            (CheckState::Good, Severity::Low, "Standard account".into())
        );
        assert_eq!(
            at(false, Err("access denied".into())),
            (
                CheckState::Unknown,
                Severity::Low,
                "Could not check: access denied".into()
            )
        );
    }

    #[test]
    fn another_account_looks_up_the_signed_in_user() {
        let at = |other: Option<bool>, lookup: Option<std::result::Result<bool, String>>| {
            let raw = accounts(|a| a.session_admin = lookup);
            admin_account(&raw, &ctx(true, other)).summary
        };
        assert_eq!(at(Some(true), Some(Ok(true))), "Administrator account");
        assert_eq!(at(Some(true), Some(Ok(false))), "Standard account");
        assert_eq!(
            at(None, Some(Err("NetUserGetLocalGroups failed".into()))),
            "Could not check: NetUserGetLocalGroups failed"
        );
        assert!(at(Some(true), None).starts_with("Could not check"));
    }

    #[test]
    fn no_signed_in_user_or_administrator_protection() {
        let raw = fixtures::raw();
        let none = Context::new(true, Some(true), false, Ok(None));
        assert_eq!(
            admin_account(&raw, &none).summary,
            "No user is signed in to this session"
        );
        let failed = Context::new(true, None, false, Err("WTS failed".into()));
        assert_eq!(
            admin_account(&raw, &failed).summary,
            "Could not check: WTS failed"
        );
        let raw = accounts(|a| {
            a.admin_approval_mode = Some(2);
            a.token = Ok(ElevationType::Limited);
        });
        let check = admin_account(&raw, &ctx(false, Some(false)));
        assert_eq!(
            outcome(&check),
            (
                CheckState::Good,
                Severity::Low,
                "Administrator, with Administrator protection".into()
            )
        );
        assert!(check.fixes.iter().any(|f| f.action
            == FixAction::Uri {
                uri: "windowsdefender://administratorprotection/".into()
            }));
        // Account names are never shown.
        let text = serde_json::to_string(&check).unwrap();
        assert!(!text.contains(&user().name), "{text}");
    }

    #[test]
    fn built_in_accounts() {
        let at = |admin: bool, guest: bool, filter: Option<u32>| {
            let raw = accounts(|a| {
                a.filter_admin_token = filter;
                a.local_accounts = Ok(vec![
                    account(500, "Administrator", admin),
                    account(501, "Guest", guest),
                ]);
            });
            builtin_accounts(&raw)
        };
        assert_eq!(
            outcome(&at(false, false, None)),
            (
                CheckState::Good,
                Severity::High,
                "Administrator and Guest are disabled".into()
            )
        );
        let check = at(true, false, None);
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::High,
                "The built-in Administrator account is enabled".into()
            )
        );
        assert!(check.detail.contains("never shows UAC prompts"));
        assert!(check
            .detail
            .contains("net user \"Administrator\" /active:no"));
        // In Admin Approval Mode the account is prompted, so only the general advice applies.
        let check = at(true, false, Some(1));
        assert_eq!(check.severity, Severity::Medium);
        assert!(!check.detail.contains("never shows UAC prompts"));
        assert!(check.detail.starts_with(BUILTIN_DETAIL));
        assert!(check
            .detail
            .contains("net user \"Administrator\" /active:no"));
        assert!(at(true, false, Some(0))
            .detail
            .contains("never shows UAC prompts"));
        let check = at(false, true, None);
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::Medium,
                "The Guest account is enabled".into()
            )
        );
        assert!(check.detail.starts_with(BUILTIN_DETAIL));
        assert!(check.detail.contains("net user \"Guest\" /active:no"));
        assert!(check.fixes.is_empty());
        let both = at(true, true, Some(1));
        assert_eq!(
            both.summary,
            "The built-in Administrator account is enabled"
        );
        assert!(!both.detail.contains("never shows UAC prompts"));
        assert!(both
            .detail
            .contains("net user \"Administrator\" /active:no"));
        assert!(both.detail.contains("net user \"Guest\" /active:no"));
        let raw = accounts(|a| a.local_accounts = Err("NetUserEnum failed".into()));
        assert_eq!(
            builtin_accounts(&raw).summary,
            "Could not read the local accounts: NetUserEnum failed"
        );
    }

    #[test]
    fn automatic_sign_in() {
        let at = |on: Option<&str>, password: bool, count: Option<u32>| {
            auto_sign_in(&accounts(|a| {
                a.auto_logon = on.map(str::to_string);
                a.default_password = password;
                a.auto_logon_count = count;
            }))
        };
        assert_eq!(
            outcome(&at(None, false, None)),
            (CheckState::Good, Severity::High, "Off".into())
        );
        assert_eq!(at(Some("0"), true, Some(3)).summary, "Off");
        let check = at(Some("1"), true, None);
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::High,
                "On, with the password stored in the registry".into()
            )
        );
        let check = at(Some(" 1 "), false, Some(2));
        assert_eq!(
            outcome(&check),
            (CheckState::Attention, Severity::Medium, "On".into())
        );
        assert_eq!(check.facts[0].value, "For the next 2 sign-ins");
        assert_eq!(
            check.fixes[0].action,
            FixAction::WindowsTool {
                tool: "user_accounts".into(),
                requires_admin: true
            }
        );
        assert!(check.fixes[0]
            .note
            .as_deref()
            .unwrap()
            .starts_with("Tick “Users"));
        assert_eq!(
            check.fixes[1].action,
            FixAction::Uri {
                uri: "ms-settings:signinoptions".into()
            }
        );
    }

    #[test]
    fn the_stored_password_is_never_read() {
        let source = include_str!("accounts.rs");
        let body = source.split("\n#[cfg(test)]").next().unwrap();
        for read in [".query(DEFAULT_PASSWORD", ".query_raw(DEFAULT_PASSWORD"] {
            assert!(!body.contains(read), "{read}");
        }
        assert!(!body.contains("query(\"DefaultPassword\""));
        assert!(!body.contains("query_raw(\"DefaultPassword\""));
        assert!(body.contains("value_info(DEFAULT_PASSWORD)"));
    }

    #[test]
    fn account_settings_read_from_this_pc() {
        // Read-only: registry values, the token and the local account list.
        let raw = read_accounts(&fixtures::ctx()).unwrap();
        assert!(raw.token.is_ok());
        assert!(raw.session_admin.is_none());
        let accounts = raw.local_accounts.unwrap();
        assert!(accounts.iter().any(|a| a.rid == ADMINISTRATOR_RID));
    }
}
