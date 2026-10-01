//! Apps and browser: SmartScreen for apps and in Microsoft Edge, Smart App Control and file
//! name extensions; checks 21 to 24. Edge's SmartScreen switch and the extension setting are
//! the signed-in user's own and are read from their hive.

use super::checkup::{unreadable, Check, CheckId, FixAction, Severity};
use super::probe::{hklm, hklm_dword, hklm_exists, key_dword, key_text, CheckupRaw, UserHive};
use crate::win::registry::{Hive, Key, RegValue};
use crate::Result;

const POLICY_SYSTEM: &str = r"SOFTWARE\Policies\Microsoft\Windows\System";
const EXPLORER: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer";
const EDGE_APP_PATH: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\msedge.exe";
/// Edge policy key, under HKLM and under the user's hive.
const EDGE_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Edge";
const EDGE_POLICY_USER: &str = r"Software\Policies\Microsoft\Edge";
/// Edge's own SmartScreen switch (the key's default value), which Windows Security reports.
const EDGE_SMARTSCREEN_USER: &str = r"Software\Microsoft\Edge\SmartScreenEnabled";
const SMART_APP_CONTROL: &str = r"SYSTEM\CurrentControlSet\Control\CI\Policy";
/// Where File Explorer keeps "Hide extensions for known file types".
pub(crate) const EXPLORER_ADVANCED: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced";
pub(crate) const HIDE_FILE_EXT: &str = "HideFileExt";
/// Catalog tweak that shows file extensions (journaled, undoable from History).
pub(crate) const FILE_EXTENSIONS_TWEAK: &str = "interface.file_extensions";

const APP_BROWSER_PAGE: &str = "windowsdefender://appbrowser/";

/// The signed-in user's own settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct UserApps {
    pub(crate) edge_policy: Option<u32>,
    pub(crate) edge: Option<u32>,
    pub(crate) hide_file_ext: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppsRaw {
    /// Group Policy `EnableSmartScreen`.
    pub(crate) smartscreen_policy: Option<u32>,
    /// Group Policy `ShellSmartScreenLevel` ("Warn" or "Block").
    pub(crate) smartscreen_policy_level: Option<String>,
    /// Explorer's `SmartScreenEnabled` ("Warn", "Prompt", "RequireAdmin", "Off").
    pub(crate) smartscreen: Option<String>,
    pub(crate) edge_installed: bool,
    pub(crate) edge_policy: Option<u32>,
    /// The user's settings, or why they could not be read.
    pub(crate) user: std::result::Result<UserApps, String>,
    /// `VerifiedAndReputablePolicyState`.
    pub(crate) smart_app_control: Option<u32>,
}

/// Why per-user checks were not made while the signed-in user's hive is not loaded.
pub(crate) const USER_HIVE_NOT_LOADED: &str = "The signed-in user's settings are not loaded.";

pub(crate) fn read_apps(hive: &UserHive) -> Result<AppsRaw> {
    let policy = hklm(POLICY_SYSTEM);
    let user = match hive {
        UserHive::Unavailable(note) => Err(note.clone()),
        // Under HKEY_USERS a hive that is not loaded reads like one without the keys.
        UserHive::Users(sid) => match Key::open(Hive::Users, sid, false) {
            Ok(Some(_)) => read_user_apps(&|path| hive.open(path)),
            Ok(None) => Err(USER_HIVE_NOT_LOADED.to_string()),
            Err(e) => Err(user_settings_error(&e)),
        },
        UserHive::Current => read_user_apps(&|path| hive.open(path)),
    };
    Ok(AppsRaw {
        smartscreen_policy: key_dword(policy.as_ref(), "EnableSmartScreen"),
        smartscreen_policy_level: key_text(policy.as_ref(), "ShellSmartScreenLevel"),
        smartscreen: key_text(hklm(EXPLORER).as_ref(), "SmartScreenEnabled"),
        edge_installed: hklm_exists(EDGE_APP_PATH),
        edge_policy: hklm_dword(EDGE_POLICY, "SmartScreenEnabled"),
        user,
        smart_app_control: hklm_dword(SMART_APP_CONTROL, "VerifiedAndReputablePolicyState"),
    })
}

/// The signed-in user's own settings through `open`, which opens a key of their hive. A
/// missing key or value is Windows' default; one that cannot be read makes the settings
/// unknown, so a per-user check never reports a default it did not read.
fn read_user_apps(
    open: &dyn Fn(&str) -> Result<Option<Key>>,
) -> std::result::Result<UserApps, String> {
    let read = || -> Result<UserApps> {
        let edge_policy = open(EDGE_POLICY_USER)?;
        let edge = open(EDGE_SMARTSCREEN_USER)?;
        let advanced = open(EXPLORER_ADVANCED)?;
        Ok(UserApps {
            edge_policy: user_dword(edge_policy.as_ref(), "SmartScreenEnabled")?,
            edge: user_dword(edge.as_ref(), "")?,
            hide_file_ext: user_dword(advanced.as_ref(), HIDE_FILE_EXT)?,
        })
    };
    read().map_err(|e| user_settings_error(&e))
}

/// A DWORD value of a per-user key: `None` when the key or value is missing or the value has
/// another type; an error when it cannot be read.
fn user_dword(key: Option<&Key>, name: &str) -> Result<Option<u32>> {
    let Some(key) = key else {
        return Ok(None);
    };
    Ok(match key.query(name)? {
        Some(RegValue::Dword(v)) => Some(v),
        _ => None,
    })
}

fn user_settings_error(e: &crate::Error) -> String {
    format!("Could not read the signed-in user's settings: {e}")
}

fn apps_raw<'a>(
    raw: &'a CheckupRaw,
    id: CheckId,
    detail: &str,
) -> std::result::Result<&'a AppsRaw, Box<Check>> {
    raw.apps
        .as_ref()
        .map_err(|e| Box::new(unreadable(id, detail, "the app settings", e)))
}

/// Check 21: SmartScreen for apps and files.
pub(crate) fn smartscreen(raw: &CheckupRaw) -> Check {
    const DETAIL: &str = "SmartScreen warns you before you run downloaded apps and files that \
        are unknown or known to be malicious.";
    let a = match apps_raw(raw, CheckId::Smartscreen, DETAIL) {
        Ok(a) => a,
        Err(check) => return (*check).uri("Open App & browser control", APP_BROWSER_PAGE),
    };
    let check = Check::new(CheckId::Smartscreen, DETAIL)
        .uri("Open App & browser control", APP_BROWSER_PAGE);
    const WARNS: &str = "Warns before unrecognised apps run";
    const BLOCKS: &str = "Blocks unrecognised apps";
    match a.smartscreen_policy {
        Some(0) => return check.attention(Severity::High, "Off (set by policy)"),
        Some(_) => {
            let block = a
                .smartscreen_policy_level
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case("block"));
            return check.good(if block { BLOCKS } else { WARNS });
        }
        None => {}
    }
    match a
        .smartscreen
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("off") => check.attention(Severity::High, "Off"),
        Some("requireadmin") | Some("block") => check.good(BLOCKS),
        _ => check.good(WARNS),
    }
}

/// Check 22: SmartScreen in Microsoft Edge.
pub(crate) fn edge_smartscreen(raw: &CheckupRaw) -> Check {
    const DETAIL: &str = "Edge blocks known phishing and malware sites with SmartScreen.";
    let a = match apps_raw(raw, CheckId::EdgeSmartscreen, DETAIL) {
        Ok(a) => a,
        Err(check) => return (*check).uri("Open App & browser control", APP_BROWSER_PAGE),
    };
    let check = Check::new(CheckId::EdgeSmartscreen, DETAIL)
        .uri("Open App & browser control", APP_BROWSER_PAGE);
    if !a.edge_installed {
        return check.not_applicable("Microsoft Edge is not installed");
    }
    match a.edge_policy {
        Some(0) => return check.attention(Severity::Medium, "Off (set by policy)"),
        Some(_) => return check.good("On"),
        None => {}
    }
    let user = match &a.user {
        Ok(user) => user,
        Err(note) => return check.unknown(note.clone()),
    };
    match (user.edge_policy, user.edge) {
        (Some(0), _) => check.attention(Severity::Medium, "Off (set by policy)"),
        (Some(_), _) => check.good("On"),
        (None, Some(0)) => check.attention(Severity::Medium, "Off"),
        (None, _) => check.good("On"),
    }
}

/// Check 23: Smart App Control (information only, never scored).
pub(crate) fn smart_app_control(raw: &CheckupRaw) -> Check {
    const DETAIL: &str = "Smart App Control blocks apps that are unknown or known to be malicious.";
    let a = match apps_raw(raw, CheckId::SmartAppControl, DETAIL) {
        Ok(a) => a,
        Err(check) => return (*check).uri("Open Smart App Control", "windowsdefender://smartapp/"),
    };
    let check = Check::new(CheckId::SmartAppControl, DETAIL)
        .uri("Open Smart App Control", "windowsdefender://smartapp/");
    match a.smart_app_control {
        Some(1) => check.good("On"),
        Some(2) => check.good("Evaluation: Windows is checking whether it suits this PC"),
        Some(0) => check
            .detail(format!(
                "Once off, it can only be turned on again by reinstalling Windows, so {} does \
                 not recommend a change.",
                crate::APP_NAME
            ))
            .attention(Severity::Info, "Off"),
        Some(other) => check.unknown(format!("Unknown state ({other})")),
        None => check.not_applicable("Not available on this PC"),
    }
}

/// Check 24: File Explorer shows file name extensions.
pub(crate) fn file_extensions(raw: &CheckupRaw) -> Check {
    const DETAIL: &str = "With extensions hidden, a program named “invoice.pdf.exe” looks like \
        “invoice.pdf”.";
    let fix = FixAction::Tweak {
        id: FILE_EXTENSIONS_TWEAK.into(),
    };
    let a = match apps_raw(raw, CheckId::FileExtensions, DETAIL) {
        Ok(a) => a,
        Err(check) => return (*check).fix("Show file extensions", fix),
    };
    let check = Check::new(CheckId::FileExtensions, DETAIL).fix("Show file extensions", fix);
    match &a.user {
        Err(note) => check.unknown(note.clone()),
        Ok(user) if user.hide_file_ext == Some(0) => check.good("Shown"),
        Ok(_) => check.attention(Severity::Low, "Hidden"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::checkup::CheckState;
    use super::super::probe::fixtures;
    use super::super::probe::NO_SESSION_NOTE;
    use super::*;
    use crate::debloat::catalog::{self, Action, RegData};
    use crate::win::registry::Hive;

    fn apps(change: impl FnOnce(&mut AppsRaw)) -> CheckupRaw {
        let mut raw = fixtures::raw();
        if let Ok(a) = &mut raw.apps {
            change(a);
        }
        raw
    }

    fn user(change: impl FnOnce(&mut UserApps)) -> CheckupRaw {
        apps(|a| {
            if let Ok(u) = &mut a.user {
                change(u);
            }
        })
    }

    fn outcome(check: &Check) -> (CheckState, Severity, String) {
        (check.state, check.severity, check.summary.clone())
    }

    #[test]
    fn smartscreen_values_in_any_case() {
        let at = |value: Option<&str>| {
            smartscreen(&apps(|a| a.smartscreen = value.map(str::to_string))).summary
        };
        assert_eq!(at(None), "Warns before unrecognised apps run");
        assert_eq!(at(Some("Warn")), "Warns before unrecognised apps run");
        assert_eq!(at(Some("prompt")), "Warns before unrecognised apps run");
        assert_eq!(at(Some("RequireAdmin")), "Blocks unrecognised apps");
        assert_eq!(at(Some("BLOCK")), "Blocks unrecognised apps");
        assert_eq!(at(Some("off")), "Off");
        assert_eq!(
            outcome(&smartscreen(&apps(|a| a.smartscreen = Some("Off".into())))),
            (CheckState::Attention, Severity::High, "Off".into())
        );
    }

    #[test]
    fn the_smartscreen_policy_wins() {
        let raw = apps(|a| {
            a.smartscreen = Some("Warn".into());
            a.smartscreen_policy = Some(0);
        });
        assert_eq!(
            outcome(&smartscreen(&raw)),
            (
                CheckState::Attention,
                Severity::High,
                "Off (set by policy)".into()
            )
        );
        let raw = apps(|a| {
            a.smartscreen = Some("Off".into());
            a.smartscreen_policy = Some(1);
            a.smartscreen_policy_level = Some("Block".into());
        });
        assert_eq!(smartscreen(&raw).summary, "Blocks unrecognised apps");
        let raw = apps(|a| {
            a.smartscreen = Some("Off".into());
            a.smartscreen_policy = Some(1);
        });
        assert_eq!(
            smartscreen(&raw).summary,
            "Warns before unrecognised apps run"
        );
    }

    #[test]
    fn edge_smartscreen_states() {
        assert_eq!(edge_smartscreen(&fixtures::raw()).summary, "On");
        let raw = apps(|a| a.edge_installed = false);
        let check = edge_smartscreen(&raw);
        assert_eq!(check.state, CheckState::NotApplicable);
        assert_eq!(check.summary, "Microsoft Edge is not installed");
        assert_eq!(
            outcome(&edge_smartscreen(&user(|u| u.edge = Some(0)))),
            (CheckState::Attention, Severity::Medium, "Off".into())
        );
        assert_eq!(edge_smartscreen(&user(|u| u.edge = None)).summary, "On");
        let raw = user(|u| {
            u.edge = Some(1);
            u.edge_policy = Some(0);
        });
        assert_eq!(edge_smartscreen(&raw).summary, "Off (set by policy)");
        let mut raw = user(|u| u.edge = Some(0));
        if let Ok(a) = &mut raw.apps {
            a.edge_policy = Some(1);
        }
        assert_eq!(edge_smartscreen(&raw).summary, "On");
        if let Ok(a) = &mut raw.apps {
            a.edge_policy = Some(0);
        }
        assert_eq!(edge_smartscreen(&raw).summary, "Off (set by policy)");
        assert!(edge_smartscreen(&raw).per_user);
    }

    #[test]
    fn an_unavailable_user_hive_makes_per_user_checks_unknown() {
        let raw = apps(|a| a.user = Err(NO_SESSION_NOTE.into()));
        for check in [edge_smartscreen(&raw), file_extensions(&raw)] {
            assert_eq!(check.state, CheckState::Unknown);
            assert_eq!(check.summary, NO_SESSION_NOTE);
        }
        assert_eq!(smartscreen(&raw).state, CheckState::Good);
    }

    #[test]
    fn smart_app_control_values() {
        let at = |value: Option<u32>| {
            outcome(&smart_app_control(&apps(|a| a.smart_app_control = value)))
        };
        assert_eq!(at(Some(1)), (CheckState::Good, Severity::Info, "On".into()));
        assert_eq!(
            at(Some(2)),
            (
                CheckState::Good,
                Severity::Info,
                "Evaluation: Windows is checking whether it suits this PC".into()
            )
        );
        assert_eq!(
            at(Some(0)),
            (CheckState::Attention, Severity::Info, "Off".into())
        );
        assert_eq!(
            at(None),
            (
                CheckState::NotApplicable,
                Severity::Info,
                "Not available on this PC".into()
            )
        );
        let off = smart_app_control(&apps(|a| a.smart_app_control = Some(0)));
        assert!(off.detail.contains("so Cairn does not recommend a change"));
    }

    #[test]
    fn file_extension_states() {
        assert_eq!(file_extensions(&fixtures::raw()).summary, "Shown");
        let check = file_extensions(&user(|u| u.hide_file_ext = Some(1)));
        assert_eq!(
            outcome(&check),
            (CheckState::Attention, Severity::Low, "Hidden".into())
        );
        assert!(check.per_user);
        assert_eq!(
            check.fixes[0].action,
            FixAction::Tweak {
                id: "interface.file_extensions".into()
            }
        );
        // Hidden is Windows' default when the value is missing.
        assert_eq!(
            file_extensions(&user(|u| u.hide_file_ext = None)).summary,
            "Hidden"
        );
    }

    #[test]
    fn the_fix_tweak_writes_exactly_the_value_the_check_reads() {
        let tweak = catalog::tweak(FILE_EXTENSIONS_TWEAK).expect("catalog tweak");
        assert_eq!(tweak.actions.len(), 1);
        match &tweak.actions[0] {
            Action::Registry(action) => {
                assert_eq!(action.hive, Hive::CurrentUser);
                assert_eq!(action.path, EXPLORER_ADVANCED);
                assert_eq!(action.name, HIDE_FILE_EXT);
                assert_eq!(action.data, RegData::Dword(0));
            }
            other => panic!("unexpected action {other:?}"),
        }
    }

    #[test]
    fn a_signed_in_user_hive_that_is_not_loaded_is_unknown() {
        // Read-only: HKEY_USERS holds no hive for this generic SID.
        let hive = UserHive::Users("S-1-5-21-1111111111-2222222222-3333333333-1001".into());
        let mut read = read_apps(&hive).unwrap();
        assert_eq!(read.user, Err(USER_HIVE_NOT_LOADED.to_string()));
        // Whatever this PC's machine-wide Edge settings are, the user's own decide.
        read.edge_installed = true;
        read.edge_policy = None;
        let mut raw = fixtures::raw();
        raw.apps = Ok(read);
        for check in [edge_smartscreen(&raw), file_extensions(&raw)] {
            assert_eq!(check.state, CheckState::Unknown, "{:?}", check.id);
            assert_eq!(check.summary, USER_HIVE_NOT_LOADED);
        }
    }

    #[test]
    fn unreadable_user_settings_make_per_user_checks_unknown() {
        let denied = |_: &str| -> Result<Option<Key>> {
            Err(crate::Error::Win32(windows::core::Error::from_hresult(
                windows::core::HRESULT(0x8007_0005u32 as i32),
            )))
        };
        let user = read_user_apps(&denied);
        let note = user.clone().unwrap_err();
        assert!(
            note.starts_with("Could not read the signed-in user's settings: "),
            "{note}"
        );
        let raw = apps(|a| a.user = user);
        for check in [edge_smartscreen(&raw), file_extensions(&raw)] {
            assert_eq!(check.state, CheckState::Unknown, "{:?}", check.id);
            assert_eq!(check.summary, note);
        }
        // Keys that do not exist hold Windows' defaults.
        let missing = |_: &str| -> Result<Option<Key>> { Ok(None) };
        assert_eq!(read_user_apps(&missing), Ok(UserApps::default()));
    }

    #[test]
    fn app_settings_read_from_this_pc() {
        // Read-only registry reads through the current user's hive.
        let raw = read_apps(&UserHive::Current).unwrap();
        assert!(raw.user.is_ok());
        let raw = read_apps(&UserHive::Unavailable(NO_SESSION_NOTE.into())).unwrap();
        assert_eq!(raw.user, Err(NO_SESSION_NOTE.to_string()));
    }
}
