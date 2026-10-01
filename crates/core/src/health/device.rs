//! Device security: drive encryption through BitLocker's WMI class (administrators only),
//! and Secure Boot, the TPM and memory integrity from the system information readers;
//! checks 13 to 16.

use super::checkup::{unreadable, Check, CheckId, FixAction, Severity, BITLOCKER_URI};
use super::probe::{CheckupRaw, Context};
use super::protection::WMI_TIMEOUT_MS;
use crate::sysinfo::{FeatureState, SecureBoot, SecurityInfo, Virtualization};
use crate::win::wmi::{is_wbem, WmiConnection};
use crate::Result;

const ENCRYPTION_NAMESPACE: &str = r"ROOT\CIMV2\Security\MicrosoftVolumeEncryption";
const ENCRYPTION_QUERY: &str = "SELECT * FROM Win32_EncryptableVolume";
/// `VolumeType` of the Windows volume and of fixed data volumes.
const OS_VOLUME: u32 = 0;
const DATA_VOLUME: u32 = 1;

/// One volume BitLocker can encrypt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct EncryptedVolume {
    /// "C:"; empty for a volume without a letter.
    pub(crate) letter: String,
    /// 0 the Windows volume, 1 a fixed data volume, 2 a portable one.
    pub(crate) volume_type: Option<u32>,
    /// 1 when protection is on.
    pub(crate) protection: Option<u32>,
    /// 0 decrypted, 1 encrypted, 2 encrypting, 3 decrypting.
    pub(crate) conversion: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EncryptionRaw {
    /// Not read: the BitLocker class needs administrator rights.
    NotElevated,
    /// BitLocker's WMI namespace or class does not exist.
    NotAvailable,
    Volumes {
        /// Drive of the Windows directory, "C:".
        system_drive: String,
        volumes: Vec<EncryptedVolume>,
    },
}

/// The encryption state of every volume; reads nothing unless `elevated`.
pub(crate) fn read_encryption(elevated: bool) -> Result<EncryptionRaw> {
    use windows::Win32::System::Wmi::{WBEM_E_INVALID_CLASS, WBEM_E_INVALID_NAMESPACE};
    if !elevated {
        return Ok(EncryptionRaw::NotElevated);
    }
    let missing =
        |e: &crate::Error| is_wbem(e, WBEM_E_INVALID_NAMESPACE) || is_wbem(e, WBEM_E_INVALID_CLASS);
    let windows = crate::win::paths::windows_dir()?;
    let system_drive: String = windows.to_string_lossy().chars().take(2).collect();
    let conn = match WmiConnection::connect(ENCRYPTION_NAMESPACE) {
        Ok(conn) => conn,
        Err(e) if missing(&e) => return Ok(EncryptionRaw::NotAvailable),
        Err(e) => return Err(e),
    };
    let objects = match conn.query(ENCRYPTION_QUERY, WMI_TIMEOUT_MS) {
        Ok(objects) => objects,
        Err(e) if missing(&e) => return Ok(EncryptionRaw::NotAvailable),
        Err(e) => return Err(e),
    };
    let mut volumes = Vec::with_capacity(objects.len());
    for obj in &objects {
        volumes.push(EncryptedVolume {
            letter: obj
                .get("DriveLetter")?
                .as_text()
                .map(|s| s.trim().to_ascii_uppercase())
                .unwrap_or_default(),
            volume_type: obj.get("VolumeType")?.as_u32_bits(),
            protection: obj.get("ProtectionStatus")?.as_u32_bits(),
            conversion: obj.get("ConversionStatus")?.as_u32_bits(),
        });
    }
    Ok(EncryptionRaw::Volumes {
        system_drive: system_drive.to_ascii_uppercase(),
        volumes,
    })
}

fn volume_state(v: &EncryptedVolume) -> &'static str {
    match (v.conversion, v.protection) {
        (Some(2), _) => "encrypting",
        (_, Some(1)) => "encrypted",
        (Some(1), _) => "encrypted, protection off",
        (Some(3), _) => "decrypting",
        _ => "not encrypted",
    }
}

fn protected(v: &EncryptedVolume) -> bool {
    v.protection == Some(1) || v.conversion == Some(2)
}

const ENCRYPTION_DETAIL: &str = "If this PC is lost or stolen, anyone can read its files by \
    moving the drive to another computer.";
const SUSPENDED_DETAIL: &str = " Protection is suspended, or on Windows 11 Home device \
    encryption waits for you to sign in with a Microsoft account to finish.";

/// Check 13: the Windows drive (and fixed data drives) are encrypted.
pub(crate) fn encryption(raw: &CheckupRaw, ctx: &Context, home: Option<bool>) -> Check {
    let home = home == Some(true);
    let title = if home {
        "Device encryption"
    } else {
        "Drive encryption"
    };
    let with_platform_fix = |check: Check| {
        if home {
            check.uri("Open Device encryption", "ms-settings:deviceencryption")
        } else {
            check.uri("Open BitLocker", BITLOCKER_URI)
        }
    };
    let check = Check::new(CheckId::Encryption, ENCRYPTION_DETAIL).titled(title);
    let (system_drive, volumes) = match &raw.encryption {
        Err(e) => {
            return with_platform_fix(
                unreadable(
                    CheckId::Encryption,
                    ENCRYPTION_DETAIL,
                    "drive encryption",
                    e,
                )
                .titled(title),
            )
        }
        Ok(EncryptionRaw::NotElevated) => {
            return with_platform_fix(
                check
                    .needs_admin()
                    .fix("Restart as administrator", FixAction::Elevate),
            )
            .unknown("Needs administrator rights to check")
        }
        Ok(EncryptionRaw::NotAvailable) => {
            return check.not_applicable("Drive encryption is not available on this PC")
        }
        Ok(EncryptionRaw::Volumes {
            system_drive,
            volumes,
        }) => (system_drive, volumes),
    };
    let shown: Vec<&EncryptedVolume> = volumes
        .iter()
        .filter(|v| {
            !v.letter.is_empty() && matches!(v.volume_type, Some(OS_VOLUME | DATA_VOLUME) | None)
        })
        .collect();
    let mut check = check;
    if !shown.is_empty() {
        let facts: Vec<String> = shown
            .iter()
            .map(|v| format!("{} {}", v.letter, volume_state(v)))
            .collect();
        check = check.fact("Drives", facts.join("  ·  "));
    }
    let check = with_platform_fix(check);
    let os = volumes
        .iter()
        .find(|v| v.volume_type == Some(OS_VOLUME))
        .or_else(|| {
            volumes
                .iter()
                .find(|v| v.letter.eq_ignore_ascii_case(system_drive))
        });
    let Some(os) = os else {
        return check.unknown("Could not find the Windows drive");
    };
    let letter = if os.letter.is_empty() {
        system_drive.clone()
    } else {
        os.letter.clone()
    };
    if os.conversion == Some(2) {
        return check.good(format!("Encrypting drive {letter}…"));
    }
    if os.protection == Some(1) {
        let unprotected: Vec<&str> = shown
            .iter()
            .filter(|v| v.volume_type == Some(DATA_VOLUME) && !protected(v))
            .map(|v| v.letter.as_str())
            .collect();
        return match unprotected.as_slice() {
            [] => check.good(format!("Drive {letter} is encrypted")),
            [one] => check.attention(Severity::Low, format!("Drive {one} is not encrypted")),
            many => check.attention(
                Severity::Low,
                format!("Drives {} are not encrypted", many.join(" and ")),
            ),
        };
    }
    if os.conversion == Some(1) {
        return check
            .detail(format!("{ENCRYPTION_DETAIL}{SUSPENDED_DETAIL}"))
            .attention(
                Severity::Medium,
                format!("Drive {letter} is encrypted, but protection is off"),
            );
    }
    let severity = if ctx.has_battery {
        Severity::High
    } else {
        Severity::Medium
    };
    check.attention(severity, format!("Drive {letter} is not encrypted"))
}

/// Check 14: Secure Boot.
pub(crate) fn secure_boot(raw: &CheckupRaw) -> Check {
    const DETAIL: &str = "Secure Boot lets only trusted software start before Windows, which \
        blocks bootkits. It is turned on in the PC's firmware (UEFI) setup.";
    let check = Check::new(CheckId::SecureBoot, DETAIL)
        .uri("Open Device security", "windowsdefender://devicesecurity/");
    let info = match &raw.device {
        Ok(info) => info,
        Err(e) => return check.unknown(format!("Could not read device security: {e}")),
    };
    match info.secure_boot {
        SecureBoot::On => check.good("On"),
        SecureBoot::Off => check.attention(Severity::High, "Off"),
        SecureBoot::Unsupported => check.attention(
            Severity::Medium,
            "Not available: Windows starts in legacy BIOS mode",
        ),
        SecureBoot::Unknown => check.unknown("Unknown"),
    }
}

/// Check 15: a TPM 2.0.
pub(crate) fn tpm(raw: &CheckupRaw) -> Check {
    const DETAIL: &str =
        "BitLocker, device encryption and Windows Hello keep their keys in the TPM.";
    const MISSING: &str = "No TPM, or it is turned off in the firmware setup. BitLocker, device \
        encryption and Windows Hello use it.";
    let check = Check::new(CheckId::Tpm, DETAIL).uri(
        "Open Security processor",
        "windowsdefender://securityprocessor/",
    );
    let info = match &raw.device {
        Ok(info) => info,
        Err(e) => return check.unknown(format!("Could not read device security: {e}")),
    };
    match (info.tpm_found, info.tpm_version.as_deref()) {
        (Some(true), Some("2.0")) => check.good("TPM 2.0"),
        (Some(true), Some("1.2")) => check
            .detail(format!("{DETAIL} Windows 11 requires TPM 2.0."))
            .attention(Severity::Low, "TPM 1.2"),
        (Some(true), Some(version)) => check.good(format!("TPM {version}")),
        (Some(true), None) => check.good("TPM found"),
        (Some(false), _) => check
            .detail(MISSING)
            .attention(Severity::Medium, "Not found"),
        (None, _) => check.unknown("Unknown"),
    }
}

fn feature_word(state: FeatureState) -> &'static str {
    match state {
        FeatureState::Running => "running",
        FeatureState::NotRunning => "not running",
        FeatureState::Off => "off",
        FeatureState::Unknown => "unknown",
    }
}

fn virtualization_text(info: &SecurityInfo) -> String {
    match info.virtualization {
        Virtualization::Enabled => "Enabled in firmware".into(),
        Virtualization::Disabled => "Disabled in firmware".into(),
        Virtualization::InUse => format!(
            "In use by a hypervisor ({})",
            info.hypervisor.as_deref().unwrap_or("unknown")
        ),
        Virtualization::Unknown => "Unknown".into(),
    }
}

/// Check 16: memory integrity (hypervisor-protected code integrity).
pub(crate) fn memory_integrity(raw: &CheckupRaw) -> Check {
    const DETAIL: &str = "Memory integrity stops malicious drivers from changing the Windows \
        kernel. Windows Security lists any old driver that blocks it.";
    let check = Check::new(CheckId::MemoryIntegrity, DETAIL);
    let info = match &raw.device {
        Ok(info) => info,
        Err(e) => {
            return check
                .uri("Open Core isolation", "windowsdefender://coreisolation/")
                .unknown(format!("Could not read device security: {e}"))
        }
    };
    let check = check
        .fact("VBS", feature_word(info.vbs))
        .fact("Virtualization", virtualization_text(info))
        .uri("Open Core isolation", "windowsdefender://coreisolation/");
    match info.memory_integrity {
        FeatureState::Running => check.good("On"),
        FeatureState::Off => check.attention(Severity::Medium, "Off"),
        FeatureState::NotRunning if info.virtualization == Virtualization::Disabled => check
            .attention(
                Severity::Medium,
                "Turned on, not running: virtualization is disabled in the firmware setup",
            ),
        FeatureState::NotRunning => check.attention(
            Severity::Low,
            "Turned on, not running: it starts after the next restart",
        ),
        FeatureState::Unknown => check.unknown("Unknown"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::checkup::CheckState;
    use super::super::probe::fixtures::{self, volume};
    use super::*;

    fn volumes(list: Vec<EncryptedVolume>) -> CheckupRaw {
        let mut raw = fixtures::raw();
        raw.encryption = Ok(EncryptionRaw::Volumes {
            system_drive: "C:".into(),
            volumes: list,
        });
        raw
    }

    fn laptop() -> Context {
        Context {
            has_battery: true,
            ..fixtures::ctx()
        }
    }

    fn outcome(check: &Check) -> (CheckState, Severity, String) {
        (check.state, check.severity, check.summary.clone())
    }

    fn uris(check: &Check) -> Vec<String> {
        check
            .fixes
            .iter()
            .filter_map(|f| match &f.action {
                FixAction::Uri { uri } => Some(uri.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn without_administrator_rights_encryption_is_unknown_with_an_elevate_fix() {
        let mut raw = fixtures::raw();
        raw.encryption = Ok(EncryptionRaw::NotElevated);
        let check = encryption(&raw, &fixtures::ctx(), Some(false));
        assert_eq!(check.state, CheckState::Unknown);
        assert_eq!(check.summary, "Needs administrator rights to check");
        assert!(check.needs_admin);
        assert_eq!(check.fixes[0].action, FixAction::Elevate);
        assert_eq!(check.fixes[0].label, "Restart as administrator");
        assert_eq!(uris(&check), vec![BITLOCKER_URI.to_string()]);
        assert_eq!(check.title, "Drive encryption");
    }

    #[test]
    fn home_and_other_editions_get_their_own_title_and_page() {
        let home = encryption(&fixtures::raw(), &fixtures::ctx(), Some(true));
        assert_eq!(home.title, "Device encryption");
        assert_eq!(
            uris(&home),
            vec!["ms-settings:deviceencryption".to_string()]
        );
        let pro = encryption(&fixtures::raw(), &fixtures::ctx(), Some(false));
        assert_eq!(pro.title, "Drive encryption");
        assert_eq!(uris(&pro), vec![BITLOCKER_URI.to_string()]);
        assert_eq!(
            outcome(&pro),
            (
                CheckState::Good,
                Severity::High,
                "Drive C: is encrypted".into()
            )
        );
        assert_eq!(pro.facts[0].value, "C: encrypted");
    }

    #[test]
    fn an_unencrypted_windows_drive_is_high_on_laptops() {
        let raw = volumes(vec![volume("C:", 0, 0, 0)]);
        assert_eq!(
            outcome(&encryption(&raw, &laptop(), Some(false))),
            (
                CheckState::Attention,
                Severity::High,
                "Drive C: is not encrypted".into()
            )
        );
        assert_eq!(
            outcome(&encryption(&raw, &fixtures::ctx(), Some(false))),
            (
                CheckState::Attention,
                Severity::Medium,
                "Drive C: is not encrypted".into()
            )
        );
    }

    #[test]
    fn suspended_protection_and_encrypting() {
        let raw = volumes(vec![volume("C:", 0, 0, 1)]);
        let check = encryption(&raw, &fixtures::ctx(), Some(true));
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::Medium,
                "Drive C: is encrypted, but protection is off".into()
            )
        );
        assert!(check.detail.ends_with(SUSPENDED_DETAIL.trim_start()));
        let raw = volumes(vec![volume("C:", 0, 0, 2)]);
        let check = encryption(&raw, &fixtures::ctx(), Some(false));
        assert_eq!(check.state, CheckState::Good);
        assert_eq!(check.summary, "Encrypting drive C:…");
        assert_eq!(check.facts[0].value, "C: encrypting");
    }

    #[test]
    fn unencrypted_data_drives_are_low_when_the_windows_drive_is_fine() {
        let raw = volumes(vec![
            volume("C:", 0, 1, 1),
            volume("D:", 1, 0, 0),
            volume("E:", 2, 0, 0),
        ]);
        let check = encryption(&raw, &fixtures::ctx(), Some(false));
        assert_eq!(
            outcome(&check),
            (
                CheckState::Attention,
                Severity::Low,
                "Drive D: is not encrypted".into()
            )
        );
        // Portable drives are left out.
        assert_eq!(check.facts[0].value, "C: encrypted  ·  D: not encrypted");
        let raw = volumes(vec![
            volume("C:", 0, 1, 1),
            volume("D:", 1, 0, 0),
            volume("F:", 1, 0, 0),
        ]);
        assert_eq!(
            encryption(&raw, &fixtures::ctx(), Some(false)).summary,
            "Drives D: and F: are not encrypted"
        );
        let raw = volumes(vec![volume("C:", 0, 1, 1), volume("D:", 1, 1, 1)]);
        assert_eq!(
            encryption(&raw, &fixtures::ctx(), Some(false)).state,
            CheckState::Good
        );
    }

    #[test]
    fn missing_bitlocker_and_failed_reads() {
        let mut raw = fixtures::raw();
        raw.encryption = Ok(EncryptionRaw::NotAvailable);
        let check = encryption(&raw, &fixtures::ctx(), Some(true));
        assert_eq!(check.state, CheckState::NotApplicable);
        assert_eq!(
            check.summary,
            "Drive encryption is not available on this PC"
        );
        raw.encryption = Err("WMI did not answer".into());
        let check = encryption(&raw, &fixtures::ctx(), Some(true));
        assert_eq!(check.state, CheckState::Unknown);
        assert_eq!(
            check.summary,
            "Could not read drive encryption: WMI did not answer"
        );
        assert_eq!(check.title, "Device encryption");
        let raw = volumes(vec![volume("D:", 1, 1, 1)]);
        assert_eq!(
            encryption(&raw, &fixtures::ctx(), Some(false)).summary,
            "Could not find the Windows drive"
        );
    }

    #[test]
    fn the_windows_drive_is_found_by_letter_without_a_type() {
        let raw = volumes(vec![EncryptedVolume {
            letter: "C:".into(),
            volume_type: None,
            protection: Some(0),
            conversion: Some(0),
        }]);
        assert_eq!(
            encryption(&raw, &fixtures::ctx(), Some(false)).summary,
            "Drive C: is not encrypted"
        );
    }

    fn device(change: impl FnOnce(&mut SecurityInfo)) -> CheckupRaw {
        let mut raw = fixtures::raw();
        if let Ok(info) = &mut raw.device {
            change(info);
        }
        raw
    }

    #[test]
    fn secure_boot_states() {
        let at = |state: SecureBoot| outcome(&secure_boot(&device(|d| d.secure_boot = state)));
        assert_eq!(
            at(SecureBoot::On),
            (CheckState::Good, Severity::High, "On".into())
        );
        assert_eq!(
            at(SecureBoot::Off),
            (CheckState::Attention, Severity::High, "Off".into())
        );
        assert_eq!(
            at(SecureBoot::Unsupported),
            (
                CheckState::Attention,
                Severity::Medium,
                "Not available: Windows starts in legacy BIOS mode".into()
            )
        );
        assert_eq!(at(SecureBoot::Unknown).0, CheckState::Unknown);
    }

    #[test]
    fn tpm_states() {
        let at = |found: Option<bool>, version: Option<&str>| {
            outcome(&tpm(&device(|d| {
                d.tpm_found = found;
                d.tpm_version = version.map(str::to_string);
            })))
        };
        assert_eq!(
            at(Some(true), Some("2.0")),
            (CheckState::Good, Severity::Medium, "TPM 2.0".into())
        );
        assert_eq!(
            at(Some(true), Some("1.2")),
            (CheckState::Attention, Severity::Low, "TPM 1.2".into())
        );
        assert_eq!(
            at(Some(false), None),
            (CheckState::Attention, Severity::Medium, "Not found".into())
        );
        assert_eq!(at(None, None).0, CheckState::Unknown);
        let check = tpm(&device(|d| d.tpm_version = Some("1.2".into())));
        assert!(check.detail.ends_with("Windows 11 requires TPM 2.0."));
    }

    #[test]
    fn memory_integrity_states() {
        let at = |state: FeatureState, virtualization: Virtualization| {
            outcome(&memory_integrity(&device(|d| {
                d.memory_integrity = state;
                d.virtualization = virtualization;
            })))
        };
        assert_eq!(
            at(FeatureState::Running, Virtualization::InUse),
            (CheckState::Good, Severity::Medium, "On".into())
        );
        assert_eq!(
            at(FeatureState::Off, Virtualization::Enabled),
            (CheckState::Attention, Severity::Medium, "Off".into())
        );
        assert_eq!(
            at(FeatureState::NotRunning, Virtualization::Disabled),
            (
                CheckState::Attention,
                Severity::Medium,
                "Turned on, not running: virtualization is disabled in the firmware setup".into()
            )
        );
        assert_eq!(
            at(FeatureState::NotRunning, Virtualization::Enabled),
            (
                CheckState::Attention,
                Severity::Low,
                "Turned on, not running: it starts after the next restart".into()
            )
        );
        assert_eq!(
            at(FeatureState::Unknown, Virtualization::Unknown).0,
            CheckState::Unknown
        );
        let check = memory_integrity(&fixtures::raw());
        assert_eq!(check.facts[0].value, "running");
        assert_eq!(
            check.facts[1].value,
            "In use by a hypervisor (Microsoft Hv)"
        );
    }

    #[test]
    fn a_failed_device_read_makes_three_checks_unknown() {
        let mut raw = fixtures::raw();
        raw.device = Err("x".into());
        for check in [secure_boot(&raw), tpm(&raw), memory_integrity(&raw)] {
            assert_eq!(check.state, CheckState::Unknown);
            assert_eq!(check.summary, "Could not read device security: x");
            assert_eq!(check.fixes.len(), 1);
        }
    }

    #[test]
    fn encryption_is_not_read_without_administrator_rights() {
        assert_eq!(read_encryption(false).unwrap(), EncryptionRaw::NotElevated);
    }
}
