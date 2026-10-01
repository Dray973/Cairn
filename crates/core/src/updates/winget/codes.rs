//! What winget's exit codes mean for one app and for the update check.
//!
//! winget exits with an HRESULT; its own codes are `0x8A15xxxx` (winget-cli
//! `doc/windows/package-manager/winget/returnCodes.md`). An installer's own exit code can
//! pass through as well, such as 3010 for "restart required".

use serde::{Deserialize, Serialize};

use super::UpdatesKind;
use crate::tools::exit_code_hex;

/// Where one app of an update or install batch stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    Queued,
    Running,
    Succeeded,
    AlreadyCurrent,
    AlreadyInstalled,
    RestartRequired,
    Failed,
    TimedOut,
    LeftRunning,
    NotStarted,
    Skipped,
}

impl ItemState {
    /// The item has ended, whatever the result.
    pub fn is_finished(self) -> bool {
        !matches!(self, ItemState::Queued | ItemState::Running)
    }

    /// The item ended as wanted: updated, installed or nothing to do.
    pub fn is_good(self) -> bool {
        matches!(
            self,
            ItemState::Succeeded | ItemState::AlreadyCurrent | ItemState::AlreadyInstalled
        )
    }

    /// Outcome of the item's final audit row, for items whose process ended.
    pub fn audit_outcome(self) -> &'static str {
        match self {
            ItemState::Succeeded => "succeeded",
            ItemState::RestartRequired => "restart_required",
            ItemState::AlreadyCurrent => "already_in_desired_state",
            ItemState::AlreadyInstalled => "already_installed",
            ItemState::TimedOut => "timeout",
            ItemState::LeftRunning => "left_running",
            _ => "failed",
        }
    }
}

/// winget codes named in winget-cli's return code list.
mod code {
    pub const UPDATE_NOT_APPLICABLE: u32 = 0x8A15_002B;
    pub const UPGRADE_VERSION_NOT_NEWER: u32 = 0x8A15_004F;
    pub const PACKAGE_ALREADY_INSTALLED: u32 = 0x8A15_0061;
    pub const INSTALL_REBOOT_REQUIRED_TO_FINISH: u32 = 0x8A15_0109;
    pub const INSTALL_REBOOT_INITIATED: u32 = 0x8A15_010B;
}

/// winget's own codes share this upper half.
const WINGET_FACILITY: u32 = 0x8A15_0000;
/// Installer codes for "restart to finish" that winget may pass through.
const RESTART_CODES: [i32; 2] = [3010, 1641];

pub const ALREADY_CURRENT_TEXT: &str = "Already up to date.";
pub const ALREADY_INSTALLED_TEXT: &str = "Already installed.";
pub const RESTART_TEXT: &str = "Restart Windows to finish.";

/// The message of a failure code, when it has a specific one.
fn failure_text(code: u32) -> Option<&'static str> {
    Some(match code {
        0x8A15_0101 | 0x8A15_0103 | 0x8A15_0111 => "The app is open. Close it and try again.",
        0x8A15_0049 | 0x8A15_0006 | 0x8A15_0115 => {
            "The app's own installer failed. If the app is open, close it and try again."
        }
        0x8A15_0108 => "The app's installer failed and asks you to contact its publisher.",
        0x8A15_001E => {
            "The Microsoft Store couldn't install it. Try it in the Microsoft Store app."
        }
        0x8A15_0069 => {
            "The installed copy is a placeholder; finish installing it in the Microsoft Store."
        }
        0x8A15_0050 => "winget can't tell which version is installed, so it doesn't update it.",
        0x8A15_0102 => "Another installation is running. Try again when it finishes.",
        0x8A15_0105 => "Not enough disk space.",
        0x8A15_0106 => "Not enough memory. Close other apps and try again.",
        0x8A15_0107 | 0x8A15_0008 | 0x8A15_004B | 0x8A15_0045 | 0x8A15_0086 | 0x8A15_002E => {
            "Couldn't download it. Check your internet connection."
        }
        0x8A15_005E => {
            "The connection was intercepted (a VPN, proxy or security app), so winget refused it."
        }
        0x8A15_0056 | 0x8A15_007D => {
            "This app can't be updated or installed by an administrator app. Update it from the \
             app itself or the Microsoft Store."
        }
        0x8A15_0011 | 0x8A15_002D => {
            "The download failed winget's security check; nothing was installed. Try again later."
        }
        0x8A15_0010 | 0x8A15_0113 => "There's no installer for this PC.",
        0x8A15_0014 | 0x8A15_0017 => "winget couldn't find this app. Check its id.",
        0x8A15_0016 => "More than one app matches this id.",
        0x8A15_010F | 0x8A15_003A | 0x8A15_001B | 0x8A15_001C => "A policy on this PC blocks it.",
        0x8A15_0068 => "It's pinned in winget; unpin it to update.",
        0x8A15_008E => {
            "winget can't update this copy because it was installed another way. It may update \
             itself; otherwise get the update from its publisher."
        }
        0x8A15_0114 => {
            "Its installer can't update an installed copy; get the update from the app itself or \
             its publisher."
        }
        0x8A15_010A => "Restart Windows, then try again.",
        0x8A15_010C => "The installer was cancelled.",
        0x8A15_010D => "Another version is already installed.",
        0x8A15_010E => "A newer version is already installed.",
        0x8A15_0104 | 0x8A15_0110 | 0x8A15_006B => "A component it needs couldn't be installed.",
        0x8A15_006D => "A Windows service it needs is busy. Try again later.",
        0x8A15_0005 | 0x8A15_006A => "winget was stopped.",
        0x8A15_0041 | 0x8A15_0046 => "The license terms weren't accepted.",
        _ => return None,
    })
}

/// The state and user message of an app whose winget process ended with `code`.
pub fn item_outcome(kind: UpdatesKind, code: i32) -> (ItemState, Option<String>) {
    let install = kind == UpdatesKind::Install;
    let nothing_to_do = if install {
        (ItemState::AlreadyInstalled, ALREADY_INSTALLED_TEXT)
    } else {
        (ItemState::AlreadyCurrent, ALREADY_CURRENT_TEXT)
    };
    if code == 0 {
        return (ItemState::Succeeded, None);
    }
    if RESTART_CODES.contains(&code) {
        return (ItemState::RestartRequired, Some(RESTART_TEXT.to_string()));
    }
    let raw = code as u32;
    match raw {
        code::INSTALL_REBOOT_REQUIRED_TO_FINISH => {
            return (ItemState::RestartRequired, Some(RESTART_TEXT.to_string()))
        }
        code::INSTALL_REBOOT_INITIATED => {
            return (
                ItemState::RestartRequired,
                Some("The installer is restarting Windows.".to_string()),
            )
        }
        code::UPDATE_NOT_APPLICABLE | code::UPGRADE_VERSION_NOT_NEWER => {
            return (nothing_to_do.0, Some(nothing_to_do.1.to_string()))
        }
        code::PACKAGE_ALREADY_INSTALLED => {
            return (nothing_to_do.0, Some(nothing_to_do.1.to_string()))
        }
        _ => {}
    }
    let message = match failure_text(raw) {
        Some(text) => text.to_string(),
        None if raw & 0xFFFF_0000 == WINGET_FACILITY => {
            format!("winget error {}.", exit_code_hex(code))
        }
        None => format!("The installer failed (exit code {code})."),
    };
    (ItemState::Failed, Some(message))
}

/// Failures that come from the installed copy, the app's installers or this PC's policies.
/// They stay the same until something outside Cairn changes, so trying again from Cairn
/// fails the same way.
const LASTING_FAILURES: [u32; 15] = [
    0x8A15_008E, // UPDATE_INSTALL_TECHNOLOGY_MISMATCH
    0x8A15_0114, // INSTALL_UPGRADE_NOT_SUPPORTED
    0x8A15_0056, // INSTALLER_PROHIBITS_ELEVATION
    0x8A15_007D, // ADMIN_CONTEXT_ACTION_PROHIBITED
    0x8A15_0010, // NO_APPLICABLE_INSTALLER
    0x8A15_0113, // INSTALL_SYSTEM_NOT_SUPPORTED
    0x8A15_0068, // PACKAGE_IS_PINNED
    0x8A15_0050, // UPGRADE_VERSION_UNKNOWN
    0x8A15_0069, // PACKAGE_IS_STUB
    0x8A15_010D, // INSTALL_ALREADY_INSTALLED
    0x8A15_010E, // INSTALL_DOWNGRADE
    0x8A15_010F, // INSTALL_BLOCKED_BY_POLICY
    0x8A15_003A, // BLOCKED_BY_POLICY
    0x8A15_001B, // MSSTORE_BLOCKED_BY_POLICY
    0x8A15_001C, // MSSTORE_APP_BLOCKED_BY_POLICY
];

/// Whether trying again can end differently for an app whose winget ended with `code`: false
/// for the codes of `LASTING_FAILURES`, true for every other code.
pub fn retry_may_help(code: i32) -> bool {
    !LASTING_FAILURES.contains(&(code as u32))
}

/// How the upgrade listing of an update check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanCode {
    Ok,
    /// Nothing to upgrade.
    Empty,
    /// The listing may be incomplete; the rows read still count, with this warning. Without
    /// rows it is an error with this message.
    Partial(&'static str),
    Error(ScanFailure),
}

/// Why an update check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanFailure {
    Known(&'static str),
    /// Any other code: "winget ended with {hex}."
    Other(i32),
}

impl ScanFailure {
    pub fn message(self) -> String {
        match self {
            ScanFailure::Known(text) => text.to_string(),
            ScanFailure::Other(code) => format!("winget ended with {}.", exit_code_hex(code)),
        }
    }
}

pub const UNREACHABLE_TEXT: &str =
    "winget couldn't reach its sources. Check your internet connection.";
pub const SOURCES_DAMAGED_TEXT: &str = "winget's source data is missing or damaged. Run 'winget \
     source reset --force' in an administrator terminal, then check again.";
pub const WINGET_BLOCKED_TEXT: &str = "A policy turns winget off on this PC.";

/// What the exit code of `winget upgrade` (the listing) means.
pub fn scan_outcome(code: i32) -> ScanCode {
    if code == 0 {
        return ScanCode::Ok;
    }
    match code as u32 {
        0x8A15_0014 | code::UPDATE_NOT_APPLICABLE => ScanCode::Empty,
        0x8A15_004B | 0x8A15_0045 => ScanCode::Partial(UNREACHABLE_TEXT),
        0x8A15_000F | 0x8A15_000A | 0x8A15_003F => {
            ScanCode::Error(ScanFailure::Known(SOURCES_DAMAGED_TEXT))
        }
        0x8A15_003A => ScanCode::Error(ScanFailure::Known(WINGET_BLOCKED_TEXT)),
        _ => ScanCode::Error(ScanFailure::Other(code)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hr(raw: u32) -> i32 {
        raw as i32
    }

    fn message(kind: UpdatesKind, raw: u32) -> String {
        item_outcome(kind, hr(raw)).1.unwrap_or_default()
    }

    #[test]
    fn success_restart_and_nothing_to_do() {
        for kind in [UpdatesKind::Upgrade, UpdatesKind::Install] {
            assert_eq!(item_outcome(kind, 0), (ItemState::Succeeded, None));
            for code in [3010, 1641] {
                assert_eq!(item_outcome(kind, code).0, ItemState::RestartRequired);
            }
            assert_eq!(
                item_outcome(kind, hr(0x8A15_0109)),
                (ItemState::RestartRequired, Some(RESTART_TEXT.into()))
            );
            assert_eq!(
                item_outcome(kind, hr(0x8A15_010B)).1.as_deref(),
                Some("The installer is restarting Windows.")
            );
        }
        for raw in [0x8A15_002B, 0x8A15_004F, 0x8A15_0061] {
            assert_eq!(
                item_outcome(UpdatesKind::Upgrade, hr(raw)),
                (ItemState::AlreadyCurrent, Some(ALREADY_CURRENT_TEXT.into()))
            );
            assert_eq!(
                item_outcome(UpdatesKind::Install, hr(raw)),
                (
                    ItemState::AlreadyInstalled,
                    Some(ALREADY_INSTALLED_TEXT.into())
                )
            );
        }
    }

    #[test]
    fn every_mapped_failure_has_its_message() {
        let table: &[(&[u32], &str)] = &[
            (
                &[0x8A15_0101, 0x8A15_0103, 0x8A15_0111],
                "The app is open. Close it and try again.",
            ),
            (
                &[0x8A15_0049, 0x8A15_0006, 0x8A15_0115],
                "The app's own installer failed. If the app is open, close it and try again.",
            ),
            (
                &[0x8A15_0108],
                "The app's installer failed and asks you to contact its publisher.",
            ),
            (
                &[0x8A15_001E],
                "The Microsoft Store couldn't install it. Try it in the Microsoft Store app.",
            ),
            (
                &[0x8A15_0069],
                "The installed copy is a placeholder; finish installing it in the Microsoft Store.",
            ),
            (
                &[0x8A15_0050],
                "winget can't tell which version is installed, so it doesn't update it.",
            ),
            (
                &[0x8A15_0102],
                "Another installation is running. Try again when it finishes.",
            ),
            (&[0x8A15_0105], "Not enough disk space."),
            (
                &[0x8A15_0106],
                "Not enough memory. Close other apps and try again.",
            ),
            (
                &[
                    0x8A15_0107,
                    0x8A15_0008,
                    0x8A15_004B,
                    0x8A15_0045,
                    0x8A15_0086,
                    0x8A15_002E,
                ],
                "Couldn't download it. Check your internet connection.",
            ),
            (
                &[0x8A15_005E],
                "The connection was intercepted (a VPN, proxy or security app), so winget refused it.",
            ),
            (
                &[0x8A15_0056, 0x8A15_007D],
                "This app can't be updated or installed by an administrator app. Update it from \
                 the app itself or the Microsoft Store.",
            ),
            (
                &[0x8A15_0011, 0x8A15_002D],
                "The download failed winget's security check; nothing was installed. Try again later.",
            ),
            (&[0x8A15_0010, 0x8A15_0113], "There's no installer for this PC."),
            (
                &[0x8A15_0014, 0x8A15_0017],
                "winget couldn't find this app. Check its id.",
            ),
            (&[0x8A15_0016], "More than one app matches this id."),
            (
                &[0x8A15_010F, 0x8A15_003A, 0x8A15_001B, 0x8A15_001C],
                "A policy on this PC blocks it.",
            ),
            (&[0x8A15_0068], "It's pinned in winget; unpin it to update."),
            (
                &[0x8A15_008E],
                "winget can't update this copy because it was installed another way. It may \
                 update itself; otherwise get the update from its publisher.",
            ),
            (
                &[0x8A15_0114],
                "Its installer can't update an installed copy; get the update from the app \
                 itself or its publisher.",
            ),
            (&[0x8A15_010A], "Restart Windows, then try again."),
            (&[0x8A15_010C], "The installer was cancelled."),
            (&[0x8A15_010D], "Another version is already installed."),
            (&[0x8A15_010E], "A newer version is already installed."),
            (
                &[0x8A15_0104, 0x8A15_0110, 0x8A15_006B],
                "A component it needs couldn't be installed.",
            ),
            (
                &[0x8A15_006D],
                "A Windows service it needs is busy. Try again later.",
            ),
            (&[0x8A15_0005, 0x8A15_006A], "winget was stopped."),
            (
                &[0x8A15_0041, 0x8A15_0046],
                "The license terms weren't accepted.",
            ),
        ];
        for (codes, text) in table {
            for &raw in *codes {
                for kind in [UpdatesKind::Upgrade, UpdatesKind::Install] {
                    assert_eq!(item_outcome(kind, hr(raw)).0, ItemState::Failed, "{raw:#X}");
                    assert_eq!(message(kind, raw), *text, "{raw:#X}");
                }
            }
        }
    }

    #[test]
    fn unknown_codes_name_themselves() {
        assert_eq!(
            item_outcome(UpdatesKind::Upgrade, hr(0x8A15_FFFF)),
            (ItemState::Failed, Some("winget error 0x8A15FFFF.".into()))
        );
        assert_eq!(
            item_outcome(UpdatesKind::Install, 1603),
            (
                ItemState::Failed,
                Some("The installer failed (exit code 1603).".into())
            )
        );
        assert_eq!(exit_code_hex(hr(0x8A15_0109)), "0x8A150109");
        assert_eq!(exit_code_hex(-1), "0xFFFFFFFF");
    }

    #[test]
    fn no_message_tells_the_user_to_uninstall() {
        for low in 0..=0xFFFF_u32 {
            if let Some(text) = failure_text(WINGET_FACILITY | low) {
                assert!(
                    !text.to_ascii_lowercase().contains("uninstall"),
                    "{:#X}: {text}",
                    WINGET_FACILITY | low
                );
            }
        }
    }

    #[test]
    fn retries_cannot_change_lasting_failures() {
        let lasting = [
            0x8A15_008E,
            0x8A15_0114,
            0x8A15_0056,
            0x8A15_007D,
            0x8A15_0010,
            0x8A15_0113,
            0x8A15_0068,
            0x8A15_0050,
            0x8A15_0069,
            0x8A15_010D,
            0x8A15_010E,
            0x8A15_010F,
            0x8A15_003A,
            0x8A15_001B,
            0x8A15_001C,
        ];
        for raw in lasting {
            assert!(!retry_may_help(hr(raw)), "{raw:#X}");
            for kind in [UpdatesKind::Upgrade, UpdatesKind::Install] {
                assert_eq!(item_outcome(kind, hr(raw)).0, ItemState::Failed, "{raw:#X}");
            }
        }
        // An open app, a busy installer, the network or an installer's own error can change.
        for raw in [
            0x8A15_0049,
            0x8A15_0006,
            0x8A15_0101,
            0x8A15_0102,
            0x8A15_0107,
            0x8A15_010A,
            0x8A15_FFFF,
        ] {
            assert!(retry_may_help(hr(raw)), "{raw:#X}");
        }
        for code in [0, 3010, 1641, 1603, -1] {
            assert!(retry_may_help(code), "{code}");
        }
    }

    #[test]
    fn audit_outcomes_per_state() {
        assert_eq!(ItemState::Succeeded.audit_outcome(), "succeeded");
        assert_eq!(
            ItemState::RestartRequired.audit_outcome(),
            "restart_required"
        );
        assert_eq!(
            ItemState::AlreadyCurrent.audit_outcome(),
            "already_in_desired_state"
        );
        assert_eq!(
            ItemState::AlreadyInstalled.audit_outcome(),
            "already_installed"
        );
        assert_eq!(ItemState::Failed.audit_outcome(), "failed");
        assert_eq!(ItemState::TimedOut.audit_outcome(), "timeout");
        assert_eq!(ItemState::LeftRunning.audit_outcome(), "left_running");
        assert!(ItemState::AlreadyInstalled.is_good());
        assert!(!ItemState::RestartRequired.is_good());
        assert!(!ItemState::Running.is_finished());
        assert!(ItemState::NotStarted.is_finished());
    }

    #[test]
    fn scan_codes() {
        assert_eq!(scan_outcome(0), ScanCode::Ok);
        assert_eq!(scan_outcome(hr(0x8A15_0014)), ScanCode::Empty);
        assert_eq!(scan_outcome(hr(0x8A15_002B)), ScanCode::Empty);
        assert_eq!(
            scan_outcome(hr(0x8A15_004B)),
            ScanCode::Partial(UNREACHABLE_TEXT)
        );
        assert_eq!(
            scan_outcome(hr(0x8A15_0045)),
            ScanCode::Partial(UNREACHABLE_TEXT)
        );
        for raw in [0x8A15_000F, 0x8A15_000A, 0x8A15_003F] {
            assert_eq!(
                scan_outcome(hr(raw)),
                ScanCode::Error(ScanFailure::Known(SOURCES_DAMAGED_TEXT))
            );
        }
        assert_eq!(
            scan_outcome(hr(0x8A15_003A)),
            ScanCode::Error(ScanFailure::Known(WINGET_BLOCKED_TEXT))
        );
        match scan_outcome(hr(0x8A15_0999)) {
            ScanCode::Error(failure) => {
                assert_eq!(failure.message(), "winget ended with 0x8A150999.")
            }
            other => panic!("{other:?}"),
        }
    }
}
