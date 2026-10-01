//! Windows edition, version, activation, up time and Fast Startup.

use std::mem::size_of;

use chrono::{DateTime, Utc};
use windows::core::{GUID, PWSTR};
use windows::Win32::Security::Authentication::Identity::{
    SLIsGenuineLocal, SL_GENUINE_STATE, SL_GEN_STATE_INVALID_LICENSE, SL_GEN_STATE_IS_GENUINE,
    SL_GEN_STATE_TAMPERED,
};
use windows::Win32::System::Power::{GetPwrCapabilities, SYSTEM_POWER_CAPABILITIES};
use windows::Win32::System::SystemInformation::{
    ComputerNamePhysicalDnsHostname, GetComputerNameExW, GetTickCount64, IMAGE_FILE_MACHINE,
    IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_I386,
};
use windows::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};

use super::{open_hklm, reg_dword, reg_text, Activation, Architecture, OsInfo};
use crate::{Error, Result};

const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
const SESSION_POWER: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Power";
/// Application id of Windows for `SLIsGenuineLocal`.
const WINDOWS_APP_ID: GUID = GUID::from_u128(0x55c92734_d682_4d71_983e_d6ec3f16059f);
/// First build of Windows 11; its registry still names the product "Windows 10".
const WINDOWS_11_BUILD: u32 = 22000;
/// `SystemS4` and `HiberFilePresent` in `SYSTEM_POWER_CAPABILITIES`.
const S4_OFFSET: usize = 6;
const HIBER_FILE_OFFSET: usize = 8;

pub(super) fn read() -> Result<OsInfo> {
    let key = open_hklm(CURRENT_VERSION)
        .ok_or_else(|| Error::Other("the Windows version key cannot be read".into()))?;
    let edition = reg_text(&key, "EditionID");
    let build = reg_text(&key, "CurrentBuildNumber")
        .or_else(|| reg_text(&key, "CurrentBuild"))
        .and_then(|b| b.parse().ok())
        .unwrap_or(0);
    let hiberboot = open_hklm(SESSION_POWER).and_then(|k| reg_dword(&k, "HiberbootEnabled"));
    let architecture = native_architecture();
    Ok(OsInfo {
        product_name: product_name(
            reg_text(&key, "ProductName").as_deref(),
            edition.as_deref(),
            build,
        ),
        display_version: version_text(
            reg_text(&key, "DisplayVersion").as_deref(),
            reg_text(&key, "ReleaseId").as_deref(),
        ),
        edition_id: edition.unwrap_or_default(),
        build,
        revision: reg_dword(&key, "UBR"),
        architecture,
        emulated: emulated(architecture, cfg!(target_arch = "x86_64")),
        installed_at: install_time(reg_dword(&key, "InstallDate")),
        // SAFETY: GetTickCount64 takes no arguments.
        uptime_secs: unsafe { GetTickCount64() } / 1000,
        fast_startup: fast_startup(hiberboot, read_power_caps()),
        activation: activation(genuine_state()),
        computer_name: computer_name().unwrap_or_default(),
    })
}

/// Product name for display. Builds from 22000 on are Windows 11 even though their
/// `ProductName` still starts with "Windows 10"; a missing name is made from the edition.
pub(crate) fn product_name(raw: Option<&str>, edition_id: Option<&str>, build: u32) -> String {
    let windows_11 = build >= WINDOWS_11_BUILD;
    if let Some(name) = raw.map(str::trim).filter(|n| !n.is_empty()) {
        return match name.strip_prefix("Windows 10") {
            Some(rest) if windows_11 => format!("Windows 11{rest}"),
            _ => name.to_string(),
        };
    }
    let base = if windows_11 {
        "Windows 11"
    } else {
        "Windows 10"
    };
    match edition_id.map(str::trim).filter(|e| !e.is_empty()) {
        Some(edition) => format!("{base} {}", edition_name(edition)),
        None => base.to_string(),
    }
}

/// Marketing name of an `EditionID`; unknown ids are shown as they are.
fn edition_name(edition_id: &str) -> &str {
    match edition_id {
        "Core" => "Home",
        "CoreSingleLanguage" => "Home Single Language",
        "CoreN" => "Home N",
        "Professional" => "Pro",
        "ProfessionalWorkstation" => "Pro for Workstations",
        "EnterpriseS" => "Enterprise LTSC",
        "IoTEnterprise" => "IoT Enterprise",
        other => other,
    }
}

/// `DisplayVersion` ("25H2"), else the older `ReleaseId` ("2009").
pub(super) fn version_text(
    display_version: Option<&str>,
    release_id: Option<&str>,
) -> Option<String> {
    [display_version, release_id]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|v| !v.is_empty())
        .map(str::to_string)
}

/// `InstallDate` is in seconds since 1970 (UTC); 0 means not recorded.
pub(super) fn install_time(unix_seconds: Option<u32>) -> Option<DateTime<Utc>> {
    let secs = unix_seconds.filter(|&s| s > 0)?;
    DateTime::<Utc>::from_timestamp(i64::from(secs), 0)
}

pub(super) fn architecture(native_machine: u16) -> Architecture {
    match IMAGE_FILE_MACHINE(native_machine) {
        IMAGE_FILE_MACHINE_AMD64 => Architecture::X64,
        IMAGE_FILE_MACHINE_ARM64 => Architecture::Arm64,
        IMAGE_FILE_MACHINE_I386 => Architecture::X86,
        _ => Architecture::Other,
    }
}

/// An x64 process on an ARM64 machine runs under emulation.
pub(super) fn emulated(native: Architecture, process_is_x64: bool) -> bool {
    process_is_x64 && native == Architecture::Arm64
}

/// Architecture of the machine, whatever this process was built for.
pub(super) fn native_architecture() -> Architecture {
    let mut process = IMAGE_FILE_MACHINE(0);
    let mut native = IMAGE_FILE_MACHINE(0);
    // SAFETY: both out pointers are valid locals; the pseudo handle needs no closing.
    match unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, Some(&mut native)) } {
        Ok(()) => architecture(native.0),
        Err(_) if cfg!(target_arch = "x86_64") => Architecture::X64,
        Err(_) => Architecture::Other,
    }
}

/// Maps the `SL_GENUINE_STATE` of Windows; `None` when the query failed.
pub(super) fn activation(state: Option<i32>) -> Activation {
    match state.map(SL_GENUINE_STATE) {
        Some(SL_GEN_STATE_IS_GENUINE) => Activation::Activated,
        Some(SL_GEN_STATE_INVALID_LICENSE) | Some(SL_GEN_STATE_TAMPERED) => {
            Activation::NotActivated
        }
        _ => Activation::Unknown,
    }
}

fn genuine_state() -> Option<i32> {
    let mut state = SL_GENUINE_STATE(-1);
    // SAFETY: the application id and the state are valid for the call; no UI options.
    unsafe { SLIsGenuineLocal(&WINDOWS_APP_ID, &mut state, None) }.ok()?;
    Some(state.0)
}

fn computer_name() -> Option<String> {
    let mut size = 0u32;
    // SAFETY: a null buffer with size 0 asks for the required size, which is written to
    // `size`; the call reports ERROR_MORE_DATA.
    let _ = unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, None, &mut size) };
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u16; size as usize];
    // SAFETY: `buf` holds `size` UTF-16 units, the length passed in `size`.
    unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            Some(PWSTR(buf.as_mut_ptr())),
            &mut size,
        )
    }
    .ok()?;
    let len = (size as usize).min(buf.len());
    let name = String::from_utf16_lossy(&buf[..len]);
    (!name.is_empty()).then_some(name)
}

/// The two power capabilities Fast Startup depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PowerCaps {
    /// Hibernation (S4) is supported.
    pub s4: bool,
    pub hiber_file: bool,
}

/// Reads `SystemS4` and `HiberFilePresent` from the bytes of a
/// `SYSTEM_POWER_CAPABILITIES`.
pub(super) fn power_caps(bytes: &[u8]) -> Option<PowerCaps> {
    Some(PowerCaps {
        s4: *bytes.get(S4_OFFSET)? != 0,
        hiber_file: *bytes.get(HIBER_FILE_OFFSET)? != 0,
    })
}

/// Buffer for `GetPwrCapabilities`, aligned like the structure it receives; read as bytes
/// because the structure's `BOOLEAN` fields are typed as `bool`.
#[repr(C, align(8))]
struct PowerCapsBuffer([u8; size_of::<SYSTEM_POWER_CAPABILITIES>()]);

fn read_power_caps() -> Option<PowerCaps> {
    let mut buf = PowerCapsBuffer([0; size_of::<SYSTEM_POWER_CAPABILITIES>()]);
    // SAFETY: the buffer is as large and as aligned as SYSTEM_POWER_CAPABILITIES and is
    // only read back as bytes.
    let ok = unsafe { GetPwrCapabilities(buf.0.as_mut_ptr().cast::<SYSTEM_POWER_CAPABILITIES>()) };
    if ok {
        power_caps(&buf.0)
    } else {
        None
    }
}

/// Fast Startup is on only when `HiberbootEnabled` is 1 and hibernation is available with
/// a hibernation file; it is off when either is turned off, and unknown otherwise.
pub(super) fn fast_startup(hiberboot: Option<u32>, caps: Option<PowerCaps>) -> Option<bool> {
    match (hiberboot, caps) {
        (Some(0), _) => Some(false),
        (_, Some(caps)) if !caps.s4 || !caps.hiber_file => Some(false),
        (Some(1), Some(_)) => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_10_product_name_is_corrected_from_build_22000() {
        assert_eq!(
            product_name(Some("Windows 10 Home"), Some("Core"), 26200),
            "Windows 11 Home"
        );
        assert_eq!(
            product_name(Some("Windows 10 Pro"), None, 22000),
            "Windows 11 Pro"
        );
        assert_eq!(
            product_name(Some("Windows 10 Pro"), None, 21999),
            "Windows 10 Pro"
        );
        assert_eq!(
            product_name(Some("Windows 10 Home"), Some("Core"), 19045),
            "Windows 10 Home"
        );
        assert_eq!(
            product_name(
                Some("Windows Server 2025 Standard"),
                Some("ServerStandard"),
                26100
            ),
            "Windows Server 2025 Standard"
        );
        assert_eq!(product_name(None, Some("Core"), 26200), "Windows 11 Home");
        assert_eq!(
            product_name(Some("  "), Some("Professional"), 19045),
            "Windows 10 Pro"
        );
        assert_eq!(
            product_name(None, Some("CoreSingleLanguage"), 22631),
            "Windows 11 Home Single Language"
        );
        assert_eq!(
            product_name(None, Some("CoreN"), 22631),
            "Windows 11 Home N"
        );
        assert_eq!(
            product_name(None, Some("ProfessionalWorkstation"), 22631),
            "Windows 11 Pro for Workstations"
        );
        assert_eq!(
            product_name(None, Some("EnterpriseS"), 19044),
            "Windows 10 Enterprise LTSC"
        );
        assert_eq!(
            product_name(None, Some("IoTEnterprise"), 26100),
            "Windows 11 IoT Enterprise"
        );
        assert_eq!(
            product_name(None, Some("Education"), 26100),
            "Windows 11 Education"
        );
        assert_eq!(product_name(None, None, 26100), "Windows 11");
    }

    #[test]
    fn version_text_uses_release_id_when_display_version_is_missing() {
        assert_eq!(
            version_text(Some("25H2"), Some("2009")).as_deref(),
            Some("25H2")
        );
        assert_eq!(version_text(None, Some("2009")).as_deref(), Some("2009"));
        assert_eq!(
            version_text(Some(" "), Some("1909")).as_deref(),
            Some("1909")
        );
        assert_eq!(version_text(None, None), None);
    }

    #[test]
    fn install_date_converts_unix_seconds() {
        let at = install_time(Some(1_773_480_600)).unwrap();
        assert_eq!(at.to_rfc3339(), "2026-03-14T09:30:00+00:00");
        assert_eq!(install_time(Some(0)), None);
        assert_eq!(install_time(None), None);
    }

    #[test]
    fn architecture_and_emulation_mapping() {
        assert_eq!(architecture(0x8664), Architecture::X64);
        assert_eq!(architecture(0xAA64), Architecture::Arm64);
        assert_eq!(architecture(0x014C), Architecture::X86);
        assert_eq!(architecture(0x01C4), Architecture::Other);
        assert_eq!(architecture(0), Architecture::Other);
        assert!(emulated(Architecture::Arm64, true));
        assert!(!emulated(Architecture::Arm64, false));
        assert!(!emulated(Architecture::X64, true));
    }

    #[test]
    fn activation_state_mapping() {
        assert_eq!(activation(Some(0)), Activation::Activated);
        assert_eq!(activation(Some(1)), Activation::NotActivated);
        assert_eq!(activation(Some(2)), Activation::NotActivated);
        assert_eq!(activation(Some(3)), Activation::Unknown, "offline");
        assert_eq!(activation(Some(4)), Activation::Unknown);
        assert_eq!(activation(None), Activation::Unknown);
    }

    #[test]
    fn fast_startup_needs_hiberboot_s4_and_hiberfile() {
        let this_pc = PowerCaps {
            s4: true,
            hiber_file: true,
        };
        let no_hiber_file = PowerCaps {
            s4: true,
            hiber_file: false,
        };
        let no_s4 = PowerCaps {
            s4: false,
            hiber_file: false,
        };
        assert_eq!(fast_startup(Some(1), Some(this_pc)), Some(true));
        assert_eq!(
            fast_startup(Some(1), Some(no_hiber_file)),
            Some(false),
            "hibernation off"
        );
        assert_eq!(fast_startup(Some(1), Some(no_s4)), Some(false));
        assert_eq!(fast_startup(Some(0), Some(this_pc)), Some(false));
        assert_eq!(fast_startup(Some(0), None), Some(false));
        assert_eq!(fast_startup(None, Some(no_hiber_file)), Some(false));
        assert_eq!(fast_startup(Some(1), None), None);
        assert_eq!(fast_startup(None, Some(this_pc)), None);
        assert_eq!(fast_startup(None, None), None);

        let mut bytes = [0u8; 76];
        assert_eq!(power_caps(&bytes), Some(no_s4));
        bytes[S4_OFFSET] = 1;
        bytes[HIBER_FILE_OFFSET] = 1;
        assert_eq!(power_caps(&bytes), Some(this_pc));
        bytes[S4_OFFSET] = 0x80;
        assert_eq!(
            power_caps(&bytes),
            Some(this_pc),
            "any non-zero byte is true"
        );
        assert_eq!(power_caps(&bytes[..8]), None);
    }

    #[test]
    fn power_capabilities_buffer_matches_the_structure() {
        assert!(size_of::<SYSTEM_POWER_CAPABILITIES>() > HIBER_FILE_OFFSET);
        assert_eq!(
            std::mem::align_of::<PowerCapsBuffer>()
                % std::mem::align_of::<SYSTEM_POWER_CAPABILITIES>(),
            0
        );
    }
}
