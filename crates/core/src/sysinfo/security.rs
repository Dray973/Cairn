//! Secure Boot, TPM, hardware virtualization, memory integrity and virtualization-based
//! security.
//!
//! The raw inputs are registry values, `Tbsi_GetDeviceInfo`, the code integrity options
//! from `NtQuerySystemInformation`, `IsProcessorFeaturePresent` and CPUID; a pure
//! [`interpret`] turns them into states. Nothing here needs elevation.

use std::mem::size_of;

use windows::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemCodeIntegrityInformation,
};
use windows::Win32::System::Threading::{IsProcessorFeaturePresent, PF_VIRT_FIRMWARE_ENABLED};
use windows::Win32::System::TpmBaseServices::{Tbsi_GetDeviceInfo, TPM_DEVICE_INFO};
use windows::Win32::System::WindowsProgramming::SYSTEM_CODEINTEGRITY_INFORMATION;

use super::{
    open_hklm, reg_dword, FeatureState, FirmwareKind, SecureBoot, SecurityInfo, Virtualization,
};
use crate::Result;

const SECURE_BOOT_KEY: &str = r"SYSTEM\CurrentControlSet\Control\SecureBoot\State";
const HVCI_KEY: &str =
    r"SYSTEM\CurrentControlSet\Control\DeviceGuard\Scenarios\HypervisorEnforcedCodeIntegrity";
const DEVICE_GUARD_KEY: &str = r"SYSTEM\CurrentControlSet\Control\DeviceGuard";
const DEVICE_GUARD_POLICY_KEY: &str = r"SOFTWARE\Policies\Microsoft\Windows\DeviceGuard";
const VBS_VALUE: &str = "EnableVirtualizationBasedSecurity";

/// `TBS_SUCCESS`, and `TBS_E_TPM_NOT_FOUND` (no TPM, or turned off in the firmware).
const TBS_SUCCESS: u32 = 0;
const TBS_E_TPM_NOT_FOUND: u32 = 0x8028_400F;
/// `CODEINTEGRITY_OPTION_HVCI_KMCI_ENABLED`: kernel-mode code integrity is enforced by the
/// hypervisor (memory integrity is running).
const HVCI_RUNNING: u32 = 0x400;

/// Everything [`interpret`] needs, as read from the system.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct SecurityRaw {
    pub firmware: Option<FirmwareKind>,
    /// `UEFISecureBootEnabled`.
    pub secure_boot: Option<u32>,
    /// `Tbsi_GetDeviceInfo` result and the reported TPM version.
    pub tpm_result: Option<u32>,
    pub tpm_version: u32,
    /// Code integrity options; `None` when the query failed.
    pub code_integrity: Option<u32>,
    /// HVCI `Enabled` value.
    pub hvci_configured: Option<u32>,
    /// `EnableVirtualizationBasedSecurity`, the policy value winning over the local one.
    pub vbs_configured: Option<u32>,
    /// `PF_VIRT_FIRMWARE_ENABLED`: VT-x / AMD-V is turned on and free to use.
    pub firmware_virtualization: bool,
    /// CPUID hypervisor bit; `None` when CPUID could not be used.
    pub hypervisor_present: Option<bool>,
    /// Vendor signature of CPUID leaf 0x40000000.
    pub hypervisor_vendor: Option<String>,
}

pub(super) fn read(firmware: FirmwareKind) -> Result<SecurityInfo> {
    let (hypervisor_present, hypervisor_vendor) = cpuid_hypervisor();
    let (tpm_result, tpm_version) = tpm_device();
    let raw = SecurityRaw {
        firmware: Some(firmware),
        secure_boot: open_hklm(SECURE_BOOT_KEY)
            .and_then(|k| reg_dword(&k, "UEFISecureBootEnabled")),
        tpm_result,
        tpm_version,
        code_integrity: code_integrity_options(),
        hvci_configured: open_hklm(HVCI_KEY).and_then(|k| reg_dword(&k, "Enabled")),
        vbs_configured: vbs_configured(
            open_hklm(DEVICE_GUARD_POLICY_KEY).and_then(|k| reg_dword(&k, VBS_VALUE)),
            open_hklm(DEVICE_GUARD_KEY).and_then(|k| reg_dword(&k, VBS_VALUE)),
        ),
        // SAFETY: IsProcessorFeaturePresent takes a plain value and touches no memory.
        firmware_virtualization: unsafe { IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED) }
            .as_bool(),
        hypervisor_present,
        hypervisor_vendor,
    };
    Ok(interpret(&raw))
}

/// The Group Policy setting wins over the local Device Guard setting.
pub(super) fn vbs_configured(policy: Option<u32>, local: Option<u32>) -> Option<u32> {
    policy.or(local)
}

/// Turns the raw readings into the reported states.
pub(super) fn interpret(raw: &SecurityRaw) -> SecurityInfo {
    let firmware = raw.firmware.unwrap_or(FirmwareKind::Unknown);
    let secure_boot = match raw.secure_boot {
        Some(1) => SecureBoot::On,
        Some(0) => SecureBoot::Off,
        None if firmware == FirmwareKind::Bios => SecureBoot::Unsupported,
        _ => SecureBoot::Unknown,
    };
    let (tpm_found, tpm_version) = match raw.tpm_result {
        Some(TBS_SUCCESS) => (
            Some(true),
            match raw.tpm_version {
                2 => Some("2.0".to_string()),
                1 => Some("1.2".to_string()),
                _ => None,
            },
        ),
        Some(TBS_E_TPM_NOT_FOUND) => (Some(false), None),
        _ => (None, None),
    };
    let hypervisor = raw.hypervisor_present == Some(true);
    let virtualization = if hypervisor {
        Virtualization::InUse
    } else if raw.firmware_virtualization {
        Virtualization::Enabled
    } else if raw.hypervisor_present.is_none() {
        Virtualization::Unknown
    } else {
        Virtualization::Disabled
    };
    let hvci_running = raw
        .code_integrity
        .map(|options| options & HVCI_RUNNING != 0);
    let memory_integrity = match hvci_running {
        None => FeatureState::Unknown,
        Some(true) => FeatureState::Running,
        Some(false) if raw.hvci_configured == Some(1) => FeatureState::NotRunning,
        Some(false) => FeatureState::Off,
    };
    // Turning on memory integrity (Windows Security, the HVCI scenario key, App Control)
    // starts VBS for it without writing `EnableVirtualizationBasedSecurity`, so without that
    // value the HVCI scenario counts as the VBS configuration.
    let vbs_turned_on = match raw.vbs_configured {
        Some(0) => false,
        Some(_) => true,
        None => raw.hvci_configured == Some(1),
    };
    let vbs = if hvci_running == Some(true) {
        // Memory integrity runs inside VBS, so it running proves VBS runs, whatever the
        // configuration says.
        FeatureState::Running
    } else if !vbs_turned_on {
        FeatureState::Off
    } else if raw.hypervisor_present == Some(false) {
        FeatureState::NotRunning
    } else {
        FeatureState::Unknown
    };
    SecurityInfo {
        firmware,
        secure_boot,
        tpm_found,
        tpm_version,
        virtualization,
        hypervisor: hypervisor
            .then(|| raw.hypervisor_vendor.as_deref().and_then(hypervisor_name))
            .flatten(),
        memory_integrity,
        vbs,
    }
}

/// Product name of a CPUID hypervisor vendor signature; an unknown signature is shown as
/// it is, an empty one is `None`.
pub(super) fn hypervisor_name(signature: &str) -> Option<String> {
    let signature = signature.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    let name = match signature {
        "" => return None,
        "Microsoft Hv" => "Microsoft Hyper-V",
        "VMwareVMware" => "VMware",
        "KVMKVMKVM" => "KVM",
        "VBoxVBoxVBox" => "VirtualBox",
        "XenVMMXenVMM" => "Xen",
        "TCGTCGTCGTCG" => "QEMU",
        "prl hyperv" => "Parallels",
        other => other,
    };
    Some(name.to_string())
}

/// Text of the 12-byte vendor signature in EBX, ECX, EDX.
pub(super) fn cpuid_signature(ebx: u32, ecx: u32, edx: u32) -> String {
    let bytes: Vec<u8> = [ebx, ecx, edx]
        .iter()
        .flat_map(|r| r.to_le_bytes())
        .collect();
    String::from_utf8_lossy(&bytes).to_string()
}

/// The CPUID hypervisor bit and vendor; `(None, None)` where CPUID cannot be trusted (an
/// x64 process emulated on ARM64) or does not exist.
#[cfg(target_arch = "x86_64")]
fn cpuid_hypervisor() -> (Option<bool>, Option<String>) {
    use std::arch::x86_64::__cpuid;

    use super::os::{emulated, native_architecture};

    /// CPUID leaf 1, ECX bit 31: a hypervisor is present.
    const HYPERVISOR_PRESENT_BIT: u32 = 1 << 31;

    if emulated(native_architecture(), true) {
        return (None, None);
    }
    #[allow(unused_unsafe)]
    // SAFETY: CPUID is available on every x86_64 processor and only reads registers.
    let features = unsafe { __cpuid(1) };
    if features.ecx & HYPERVISOR_PRESENT_BIT == 0 {
        return (Some(false), None);
    }
    #[allow(unused_unsafe)]
    // SAFETY: as above; leaf 0x40000000 is defined whenever the hypervisor bit is set.
    let vendor = unsafe { __cpuid(0x4000_0000) };
    (
        Some(true),
        Some(cpuid_signature(vendor.ebx, vendor.ecx, vendor.edx)),
    )
}

#[cfg(not(target_arch = "x86_64"))]
fn cpuid_hypervisor() -> (Option<bool>, Option<String>) {
    (None, None)
}

/// `Tbsi_GetDeviceInfo` result code and TPM version (0 when not reported).
fn tpm_device() -> (Option<u32>, u32) {
    let mut info = TPM_DEVICE_INFO::default();
    // SAFETY: `info` is a writable TPM_DEVICE_INFO of the size passed; it holds only
    // integer fields.
    let result = unsafe {
        Tbsi_GetDeviceInfo(
            size_of::<TPM_DEVICE_INFO>() as u32,
            (&mut info as *mut TPM_DEVICE_INFO).cast(),
        )
    };
    (
        Some(result),
        if result == TBS_SUCCESS {
            info.tpmVersion
        } else {
            0
        },
    )
}

/// Code integrity options; `None` when the query fails.
fn code_integrity_options() -> Option<u32> {
    let mut info = SYSTEM_CODEINTEGRITY_INFORMATION {
        Length: size_of::<SYSTEM_CODEINTEGRITY_INFORMATION>() as u32,
        CodeIntegrityOptions: 0,
    };
    let mut returned = 0u32;
    // SAFETY: `info` is a writable SYSTEM_CODEINTEGRITY_INFORMATION whose size is passed
    // and whose `Length` is set as the query requires; `returned` is a valid out pointer.
    let status = unsafe {
        NtQuerySystemInformation(
            SystemCodeIntegrityInformation,
            (&mut info as *mut SYSTEM_CODEINTEGRITY_INFORMATION).cast(),
            size_of::<SYSTEM_CODEINTEGRITY_INFORMATION>() as u32,
            &mut returned,
        )
    };
    status.is_ok().then_some(info.CodeIntegrityOptions)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This PC: UEFI with Secure Boot on, TPM 2.0, memory integrity and VBS turned on but
    /// not running because virtualization is disabled in the firmware.
    fn this_pc() -> SecurityRaw {
        SecurityRaw {
            firmware: Some(FirmwareKind::Uefi),
            secure_boot: Some(1),
            tpm_result: Some(TBS_SUCCESS),
            tpm_version: 2,
            code_integrity: Some(0xC005),
            hvci_configured: Some(1),
            vbs_configured: Some(1),
            firmware_virtualization: false,
            hypervisor_present: Some(false),
            hypervisor_vendor: None,
        }
    }

    #[test]
    fn this_pc_reports_memory_integrity_not_running() {
        let info = interpret(&this_pc());
        assert_eq!(info.firmware, FirmwareKind::Uefi);
        assert_eq!(info.secure_boot, SecureBoot::On);
        assert_eq!(info.tpm_found, Some(true));
        assert_eq!(info.tpm_version.as_deref(), Some("2.0"));
        assert_eq!(info.virtualization, Virtualization::Disabled);
        assert_eq!(info.hypervisor, None);
        assert_eq!(info.memory_integrity, FeatureState::NotRunning);
        assert_eq!(info.vbs, FeatureState::NotRunning);
    }

    #[test]
    fn hypervisor_present_means_in_use() {
        let raw = SecurityRaw {
            firmware_virtualization: true,
            hypervisor_present: Some(true),
            hypervisor_vendor: Some("Microsoft Hv".into()),
            ..this_pc()
        };
        let info = interpret(&raw);
        assert_eq!(info.virtualization, Virtualization::InUse);
        assert_eq!(info.hypervisor.as_deref(), Some("Microsoft Hyper-V"));
        assert_eq!(
            info.vbs,
            FeatureState::Unknown,
            "VBS configured, HVCI not running"
        );

        let enabled = SecurityRaw {
            firmware_virtualization: true,
            ..this_pc()
        };
        assert_eq!(interpret(&enabled).virtualization, Virtualization::Enabled);
        assert_eq!(interpret(&enabled).hypervisor, None);

        let unknown = SecurityRaw {
            hypervisor_present: None,
            ..this_pc()
        };
        assert_eq!(interpret(&unknown).virtualization, Virtualization::Unknown);
    }

    #[test]
    fn hvci_flag_means_running() {
        let raw = SecurityRaw {
            code_integrity: Some(0xC005 | HVCI_RUNNING),
            hypervisor_present: Some(true),
            hypervisor_vendor: Some("Microsoft Hv".into()),
            ..this_pc()
        };
        let info = interpret(&raw);
        assert_eq!(info.memory_integrity, FeatureState::Running);
        assert_eq!(info.vbs, FeatureState::Running);

        let off = SecurityRaw {
            hvci_configured: Some(0),
            vbs_configured: None,
            ..this_pc()
        };
        let info = interpret(&off);
        assert_eq!(info.memory_integrity, FeatureState::Off);
        assert_eq!(info.vbs, FeatureState::Off);

        let failed = SecurityRaw {
            code_integrity: None,
            ..this_pc()
        };
        assert_eq!(interpret(&failed).memory_integrity, FeatureState::Unknown);
    }

    #[test]
    fn unknown_hypervisor_leaves_vbs_to_memory_integrity() {
        // An x64 process emulated on ARM64 cannot use CPUID.
        let running = SecurityRaw {
            code_integrity: Some(0xC005 | HVCI_RUNNING),
            hypervisor_present: None,
            ..this_pc()
        };
        let info = interpret(&running);
        assert_eq!(info.memory_integrity, FeatureState::Running);
        assert_eq!(info.vbs, FeatureState::Running);

        let not_running = SecurityRaw {
            code_integrity: Some(0xC005),
            hypervisor_present: None,
            ..this_pc()
        };
        let info = interpret(&not_running);
        assert_eq!(info.vbs, FeatureState::Unknown);
        assert_eq!(info.virtualization, Virtualization::Unknown);
    }

    #[test]
    fn memory_integrity_running_proves_vbs_without_the_vbs_value() {
        // Turned on in Windows Security: only the HVCI scenario key is written.
        let toggled = SecurityRaw {
            code_integrity: Some(0xC005 | HVCI_RUNNING),
            hvci_configured: Some(1),
            vbs_configured: None,
            firmware_virtualization: true,
            hypervisor_present: Some(true),
            hypervisor_vendor: Some("Microsoft Hv".into()),
            ..this_pc()
        };
        let info = interpret(&toggled);
        assert_eq!(info.memory_integrity, FeatureState::Running);
        assert_eq!(info.vbs, FeatureState::Running);

        // App Control HVCIOptions: no registry value at all.
        let app_control = SecurityRaw {
            hvci_configured: None,
            ..toggled.clone()
        };
        assert_eq!(interpret(&app_control).vbs, FeatureState::Running);

        // Running wins over a VBS value of 0.
        let vbs_zero = SecurityRaw {
            vbs_configured: Some(0),
            ..toggled
        };
        assert_eq!(interpret(&vbs_zero).vbs, FeatureState::Running);
    }

    #[test]
    fn hvci_scenario_counts_as_vbs_configuration_without_the_vbs_value() {
        // Memory integrity turned on, but virtualization is off in the firmware.
        let raw = SecurityRaw {
            vbs_configured: None,
            ..this_pc()
        };
        let info = interpret(&raw);
        assert_eq!(info.memory_integrity, FeatureState::NotRunning);
        assert_eq!(info.vbs, FeatureState::NotRunning);

        // With a hypervisor that does not run memory integrity, VBS cannot be told.
        let hypervisor = SecurityRaw {
            hypervisor_present: Some(true),
            hypervisor_vendor: Some("Microsoft Hv".into()),
            ..raw.clone()
        };
        assert_eq!(interpret(&hypervisor).vbs, FeatureState::Unknown);

        // Neither the VBS value nor the HVCI scenario: off.
        let off = SecurityRaw {
            hvci_configured: Some(0),
            ..raw
        };
        assert_eq!(interpret(&off).vbs, FeatureState::Off);
    }

    #[test]
    fn secure_boot_missing_on_legacy_bios_is_unsupported() {
        let bios = SecurityRaw {
            firmware: Some(FirmwareKind::Bios),
            secure_boot: None,
            ..this_pc()
        };
        assert_eq!(interpret(&bios).secure_boot, SecureBoot::Unsupported);
        let uefi_missing = SecurityRaw {
            secure_boot: None,
            ..this_pc()
        };
        assert_eq!(interpret(&uefi_missing).secure_boot, SecureBoot::Unknown);
        let off = SecurityRaw {
            secure_boot: Some(0),
            ..this_pc()
        };
        assert_eq!(interpret(&off).secure_boot, SecureBoot::Off);
        let odd = SecurityRaw {
            secure_boot: Some(7),
            ..this_pc()
        };
        assert_eq!(interpret(&odd).secure_boot, SecureBoot::Unknown);
    }

    #[test]
    fn tpm_codes() {
        let tpm = |result: Option<u32>, version: u32| {
            let info = interpret(&SecurityRaw {
                tpm_result: result,
                tpm_version: version,
                ..this_pc()
            });
            (info.tpm_found, info.tpm_version)
        };
        assert_eq!(tpm(Some(0), 2), (Some(true), Some("2.0".to_string())));
        assert_eq!(tpm(Some(0), 1), (Some(true), Some("1.2".to_string())));
        assert_eq!(tpm(Some(0), 0), (Some(true), None));
        assert_eq!(tpm(Some(0x8028_400F), 0), (Some(false), None));
        assert_eq!(
            tpm(Some(0x8028_4001), 0),
            (None, None),
            "other TBS errors are unknown"
        );
        assert_eq!(tpm(None, 2), (None, None));
    }

    #[test]
    fn policy_vbs_setting_wins() {
        assert_eq!(vbs_configured(Some(0), Some(1)), Some(0));
        assert_eq!(vbs_configured(Some(1), Some(0)), Some(1));
        assert_eq!(vbs_configured(None, Some(1)), Some(1));
        assert_eq!(vbs_configured(None, None), None);
        let raw = SecurityRaw {
            vbs_configured: vbs_configured(Some(0), Some(1)),
            ..this_pc()
        };
        assert_eq!(interpret(&raw).vbs, FeatureState::Off);
    }

    #[test]
    fn hypervisor_vendor_names() {
        let cases = [
            ("Microsoft Hv", "Microsoft Hyper-V"),
            ("VMwareVMware", "VMware"),
            ("KVMKVMKVM\0\0\0", "KVM"),
            ("VBoxVBoxVBox", "VirtualBox"),
            ("XenVMMXenVMM", "Xen"),
            ("TCGTCGTCGTCG", "QEMU"),
            (" prl hyperv  ", "Parallels"),
            ("ACRNACRNACRN", "ACRNACRNACRN"),
        ];
        for (signature, name) in cases {
            assert_eq!(
                hypervisor_name(signature).as_deref(),
                Some(name),
                "{signature:?}"
            );
        }
        assert_eq!(hypervisor_name("\0\0\0\0"), None);
        // "Microsoft Hv" as CPUID returns it in EBX, ECX, EDX.
        let signature = cpuid_signature(
            u32::from_le_bytes(*b"Micr"),
            u32::from_le_bytes(*b"osof"),
            u32::from_le_bytes(*b"t Hv"),
        );
        assert_eq!(signature, "Microsoft Hv");
    }

    #[test]
    fn live_read_is_consistent() {
        let info = read(FirmwareKind::Uefi).unwrap();
        if info.virtualization == Virtualization::InUse {
            assert!(info.hypervisor.is_some());
        } else {
            assert_eq!(info.hypervisor, None);
        }
        if info.tpm_found != Some(true) {
            assert_eq!(info.tpm_version, None);
        }
    }
}
